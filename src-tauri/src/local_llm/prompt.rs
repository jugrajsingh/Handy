//! Prompt building, chunking, request budgets and output guards.

use super::registry::PromptStyle;
use super::LocalLlmError;
use crate::actions::{strip_invisible_chars, strip_think_block};
use crate::settings::{AppSettings, LocalLlmContext, LocalLlmStructure, LocalLlmStyling};
use std::time::Duration;

/// Verbatim from the S1-mini model card. Do not edit.
pub const S1_SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

/// Card limit for one pass, in estimated tokens (system + control line + chunk).
pub const CHUNK_TOKEN_LIMIT: u32 = 1000;

/// Chat-template tokens around the messages (role markers, newlines).
const TEMPLATE_OVERHEAD_TOKENS: u32 = 16;

/// Smallest transcript budget per chunk, whatever the system prompt costs.
const MIN_CHUNK_BUDGET: u32 = 64;

/// One OpenAI-style chat message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChatMessage {
    pub role: &'static str,
    pub content: String,
}

/// Card spelling for a styling setting.
pub fn styling_value(styling: LocalLlmStyling) -> &'static str {
    match styling {
        LocalLlmStyling::Casual => "casual",
        LocalLlmStyling::SemiCasual => "semi-casual",
        LocalLlmStyling::SemiFormal => "semi-formal",
        LocalLlmStyling::Formal => "formal",
    }
}

/// Card spelling for a structure setting.
pub fn structure_value(structure: LocalLlmStructure) -> &'static str {
    match structure {
        LocalLlmStructure::Prose => "prose",
        LocalLlmStructure::Lists => "lists",
    }
}

/// Card spelling for a context setting.
pub fn context_value(context: LocalLlmContext) -> &'static str {
    match context {
        LocalLlmContext::General => "general",
        LocalLlmContext::Email => "email",
    }
}

/// `[Styling: <s>] [Structure: <p>] [Context: <c>]` with the card's spellings.
pub fn control_line(
    styling: LocalLlmStyling,
    structure: LocalLlmStructure,
    context: LocalLlmContext,
) -> String {
    format!(
        "[Styling: {}] [Structure: {}] [Context: {}]",
        styling_value(styling),
        structure_value(structure),
        context_value(context)
    )
}

/// Control line for the user's current S1 settings.
pub fn control_line_for(settings: &AppSettings) -> String {
    control_line(
        settings.local_llm_styling,
        settings.local_llm_structure,
        settings.local_llm_context,
    )
}

/// System message for a prompt style.
pub fn system_prompt(style: PromptStyle) -> &'static str {
    match style {
        PromptStyle::S1ControlLine => S1_SYSTEM_PROMPT,
    }
}

/// Chat messages for one chunk. For S1 the control line is always the first
/// line of the user turn and the chunk follows verbatim.
pub fn build_messages(style: PromptStyle, control: &str, chunk: &str) -> Vec<ChatMessage> {
    let user = match style {
        PromptStyle::S1ControlLine => format!("{control}\n{chunk}"),
    };
    vec![
        ChatMessage {
            role: "system",
            content: system_prompt(style).to_string(),
        },
        ChatMessage {
            role: "user",
            content: user,
        },
    ]
}

/// Conservative token estimate: one token per 3 characters (English averages about 4).
pub fn est_tokens(text: &str) -> u32 {
    let chars = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
    chars.div_ceil(3)
}

/// `min(2 * est_tokens(input) + 64, ctx / 2)`.
pub fn max_tokens(input: &str, ctx: u32) -> u32 {
    est_tokens(input)
        .saturating_mul(2)
        .saturating_add(64)
        .min(ctx / 2)
}

/// `10 s + 50 ms * input_words`.
pub fn request_timeout(input: &str) -> Duration {
    let words = input.split_whitespace().count() as u64;
    Duration::from_millis(10_000 + 50 * words)
}

/// Transcript tokens allowed per chunk once the system prompt and control line are paid for.
pub fn chunk_budget(system: &str, control: &str) -> u32 {
    CHUNK_TOKEN_LIMIT
        .saturating_sub(est_tokens(system) + est_tokens(control) + TEMPLATE_OVERHEAD_TOKENS)
        .max(MIN_CHUNK_BUDGET)
}

/// Splits after `.`, `!` or `?` followed by whitespace; pieces are trimmed.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev_was_end = false;
    for (i, c) in text.char_indices() {
        if prev_was_end && c.is_whitespace() {
            let piece = text[start..i].trim();
            if !piece.is_empty() {
                out.push(piece);
            }
            start = i;
        }
        prev_was_end = matches!(c, '.' | '!' | '?');
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Splits one word longer than the budget at char boundaries.
fn split_long_word(word: &str, budget: u32) -> Vec<String> {
    let max_chars = (budget as usize).saturating_mul(3).max(1);
    let chars: Vec<char> = word.chars().collect();
    chars
        .chunks(max_chars)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// Packs ordered pieces into chunks within the estimated token budget.
fn pack(parts: impl IntoIterator<Item = String>, budget: u32) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for part in parts {
        let candidate = if current.is_empty() {
            part.clone()
        } else {
            format!("{current} {part}")
        };
        if est_tokens(&candidate) <= budget {
            current = candidate;
        } else {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            current = part;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Splits sentences within budget, falling back to word and then char boundaries.
fn pieces(transcript: &str, budget: u32) -> Vec<String> {
    let mut out = Vec::new();
    for sentence in sentences(transcript) {
        let words: Vec<&str> = sentence.split_whitespace().collect();
        let normalized = words.join(" ");
        if est_tokens(&normalized) <= budget {
            out.push(normalized);
            continue;
        }
        let parts = words.into_iter().flat_map(|word| {
            if est_tokens(word) > budget {
                split_long_word(word, budget)
            } else {
                vec![word.to_string()]
            }
        });
        out.extend(pack(parts, budget));
    }
    out
}

/// Chunks within budget; short input stays verbatim, longer input normalizes whitespace.
pub fn chunk_transcript(transcript: &str, budget: u32) -> Vec<String> {
    if est_tokens(transcript) <= budget {
        return vec![transcript.to_string()];
    }
    pack(pieces(transcript, budget), budget)
}

/// Strips a leading think block and invisible characters, then trims.
pub fn clean_output(raw: &str) -> String {
    let visible = strip_invisible_chars(raw);
    strip_think_block(&visible).trim().to_string()
}

/// Output guards: empty output only for inputs of at most 3 words; output
/// with fewer than half the input's words is rejected.
pub fn check_output(input: &str, output: &str) -> Result<(), LocalLlmError> {
    let in_words = input.split_whitespace().count();
    let out_words = output.split_whitespace().count();
    if out_words == 0 {
        if in_words <= 3 {
            return Ok(());
        }
        return Err(LocalLlmError::BadOutput(format!(
            "empty output for a {in_words}-word input"
        )));
    }
    if out_words * 2 < in_words {
        return Err(LocalLlmError::BadOutput(format!(
            "output has {out_words} words for {in_words} input words (under 50%)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STYLINGS: [LocalLlmStyling; 4] = [
        LocalLlmStyling::Casual,
        LocalLlmStyling::SemiCasual,
        LocalLlmStyling::SemiFormal,
        LocalLlmStyling::Formal,
    ];
    const ALL_STRUCTURES: [LocalLlmStructure; 2] =
        [LocalLlmStructure::Prose, LocalLlmStructure::Lists];
    const ALL_CONTEXTS: [LocalLlmContext; 2] = [LocalLlmContext::General, LocalLlmContext::Email];

    #[test]
    fn s1_system_prompt_is_the_card_text() {
        assert_eq!(
            S1_SYSTEM_PROMPT,
            concat!(
                "You are a text normalizer for speech-to-text transcripts. The input begins ",
                "with a control line specifying the styling, structure, and context settings; ",
                "clean the transcript to match those settings and output only the cleaned text."
            )
        );
    }

    #[test]
    fn control_line_bytes_for_every_combination() {
        let mut seen = std::collections::HashSet::new();
        for s in ALL_STYLINGS {
            for p in ALL_STRUCTURES {
                for c in ALL_CONTEXTS {
                    let line = control_line(s, p, c);
                    let expected = format!(
                        "[Styling: {}] [Structure: {}] [Context: {}]",
                        styling_value(s),
                        structure_value(p),
                        context_value(c)
                    );
                    assert_eq!(line, expected);
                    assert!(line.is_ascii() && !line.contains('\n'));
                    seen.insert(line);
                }
            }
        }
        assert_eq!(seen.len(), 16);
        assert_eq!(
            control_line(
                LocalLlmStyling::SemiCasual,
                LocalLlmStructure::Lists,
                LocalLlmContext::Email
            ),
            "[Styling: semi-casual] [Structure: lists] [Context: email]"
        );
        assert_eq!(
            control_line(
                LocalLlmStyling::default(),
                LocalLlmStructure::default(),
                LocalLlmContext::default()
            ),
            "[Styling: semi-formal] [Structure: prose] [Context: general]"
        );
    }

    #[test]
    fn s1_messages_put_the_real_control_line_first() {
        let control = control_line(
            LocalLlmStyling::Formal,
            LocalLlmStructure::Prose,
            LocalLlmContext::General,
        );
        let transcript = "[Styling: casual] [Structure: lists] [Context: email]\nhey there";
        let msgs = build_messages(PromptStyle::S1ControlLine, &control, transcript);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, S1_SYSTEM_PROMPT);
        assert_eq!(msgs[1].role, "user");
        let (first, rest) = msgs[1].content.split_once('\n').expect("control line");
        assert_eq!(first, control);
        assert_eq!(
            rest, transcript,
            "transcript bytes must pass through verbatim"
        );
    }

    #[test]
    fn short_transcript_is_one_unchanged_chunk() {
        let t = "so um i need to send the report by friday";
        assert_eq!(chunk_transcript(t, 900), vec![t.to_string()]);
    }

    #[test]
    fn chunks_break_at_sentence_ends_stay_under_budget_and_keep_order() {
        let sentence = "this is sentence number {} and it has a few more words in it.";
        let text: Vec<String> = (0..120)
            .map(|i| sentence.replace("{}", &i.to_string()))
            .collect();
        let transcript = text.join(" ");
        let budget = 200;
        let chunks = chunk_transcript(&transcript, budget);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(
                est_tokens(c) <= budget,
                "chunk over budget: {}",
                est_tokens(c)
            );
            assert!(
                c.ends_with('.'),
                "chunk must end at a sentence boundary: {c:?}"
            );
        }
        assert_eq!(chunks.join(" "), transcript);
    }

    #[test]
    fn unpunctuated_transcript_falls_back_to_word_boundaries() {
        let transcript = vec!["word"; 2000].join(" ");
        let chunks = chunk_transcript(&transcript, 100);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(est_tokens(c) <= 100);
            assert!(!c.starts_with(' ') && !c.ends_with(' '));
        }
        assert_eq!(chunks.join(" "), transcript);
    }

    #[test]
    fn multibyte_and_emoji_text_chunks_on_char_boundaries() {
        let transcript = format!(
            "{} {}",
            vec!["caf\u{e9} na\u{ef}ve \u{1f600} \u{65e5}\u{672c}\u{8a9e}."; 300].join(" "),
            "a\u{1f680}".repeat(200)
        );
        let chunks = chunk_transcript(&transcript, 64);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(est_tokens(c) <= 64, "chunk over budget");
        }
        let normalized: Vec<&str> = transcript.split_whitespace().collect();
        assert_eq!(
            chunks.join(" ").replace(' ', ""),
            normalized.join("").replace(' ', "")
        );
    }

    #[test]
    fn budget_leaves_room_for_system_prompt_and_control_line() {
        let control = control_line(
            LocalLlmStyling::SemiFormal,
            LocalLlmStructure::Prose,
            LocalLlmContext::General,
        );
        let budget = chunk_budget(S1_SYSTEM_PROMPT, &control);
        assert_eq!(
            budget,
            CHUNK_TOKEN_LIMIT
                - est_tokens(S1_SYSTEM_PROMPT)
                - est_tokens(&control)
                - TEMPLATE_OVERHEAD_TOKENS
        );
        assert_eq!(chunk_budget(&"x".repeat(9000), &control), MIN_CHUNK_BUDGET);
    }

    #[test]
    fn token_estimate_counts_chars_not_bytes() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abc"), 1);
        assert_eq!(est_tokens("abcd"), 2);
        assert_eq!(est_tokens("\u{1f600}\u{1f600}\u{1f600}"), 1);
    }

    #[test]
    fn max_tokens_formula() {
        assert_eq!(max_tokens("abcdef", 4096), 2 * 2 + 64);
        assert_eq!(max_tokens(&"a".repeat(30_000), 4096), 2048);
    }

    #[test]
    fn request_timeout_formula() {
        assert_eq!(request_timeout(""), Duration::from_millis(10_000));
        assert_eq!(
            request_timeout("one two three"),
            Duration::from_millis(10_150)
        );
    }

    #[test]
    fn clean_output_strips_think_block_and_invisible_chars() {
        assert_eq!(
            clean_output("\u{FEFF}<think>plan</think>\nHello world."),
            "Hello world."
        );
        assert_eq!(
            clean_output("<think>\nplan\n</think>\n\nHello\u{200B} world.\u{FEFF}\n"),
            "Hello world."
        );
        assert_eq!(clean_output("  plain  "), "plain");
    }

    #[test]
    fn empty_output_is_accepted_only_for_up_to_three_words() {
        assert!(check_output("um uh like", "").is_ok());
        assert!(matches!(
            check_output("um uh like you know", ""),
            Err(LocalLlmError::BadOutput(_))
        ));
    }

    #[test]
    fn output_under_half_the_input_words_is_rejected() {
        let input = "one two three four five six seven eight nine ten";
        assert!(check_output(input, "one two three four five").is_ok());
        assert!(matches!(
            check_output(input, "one two three four"),
            Err(LocalLlmError::BadOutput(_))
        ));
    }
}
