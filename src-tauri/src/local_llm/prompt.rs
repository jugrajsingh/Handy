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
pub fn system_prompt(style: PromptStyle, settings: &AppSettings) -> Result<String, LocalLlmError> {
    match style {
        PromptStyle::S1ControlLine => Ok(S1_SYSTEM_PROMPT.to_string()),
        PromptStyle::PlainSystemPrompt => {
            let prompt = settings
                .post_process_selected_prompt_id
                .as_deref()
                .and_then(|id| {
                    settings
                        .post_process_prompts
                        .iter()
                        .find(|prompt| prompt.id == id)
                })
                .map(|prompt| prompt.prompt.replace("${output}", ""))
                .filter(|prompt| !prompt.trim().is_empty());
            prompt.ok_or_else(|| LocalLlmError::Failed("post_process_prompt_missing".into()))
        }
    }
}

/// Chat messages for one chunk in the model's prompt style.
pub fn build_messages(
    style: PromptStyle,
    system: &str,
    control: &str,
    chunk: &str,
) -> Vec<ChatMessage> {
    let user = match style {
        PromptStyle::S1ControlLine => format!("{control}\n{chunk}"),
        PromptStyle::PlainSystemPrompt => chunk.to_string(),
    };
    vec![
        ChatMessage {
            role: "system",
            content: system.to_string(),
        },
        ChatMessage {
            role: "user",
            content: user,
        },
    ]
}

/// Thirtieths of a token one character costs, per script.
///
/// Latin-script text (ASCII with digits and spaces, Latin-extended, general punctuation,
/// currency symbols, arrows)
/// keeps one token per three characters. Fitted to llama-server `/tokenize` counts of
/// S1-mini and Muesli 2B over the 180 eval samples (the worse model; spaces inside Indic
/// text count as Latin): Devanagari needs up to 1.18 tokens per char and gets 1.5;
/// Gurmukhi needs up to 1.91 and gets 2.5. Other Indic scripts (probed on short text:
/// Bengali 1.06, Tamil 1.14, Telugu 1.49, Gujarati 1.45) share the Gurmukhi weight. Every
/// other BMP script costs one token per char (probed worst case: Greek 0.84) and
/// characters beyond the BMP three (emoji probed at 2.18).
fn char_weight(c: char) -> u32 {
    match c {
        '\u{0900}'..='\u{097F}' => 45,
        '\u{0980}'..='\u{0DFF}' => 75,
        '\u{0000}'..='\u{024F}'
        | '\u{0300}'..='\u{036F}'
        | '\u{1E00}'..='\u{1EFF}'
        | '\u{2000}'..='\u{206F}'
        | '\u{20A0}'..='\u{20CF}'
        | '\u{2190}'..='\u{21FF}' => 10,
        c if u32::from(c) > 0xFFFF => 90,
        _ => 30,
    }
}

/// Conservative token estimate: characters weighted by script, rounded up in total.
pub fn est_tokens(text: &str) -> u32 {
    let thirtieths: u64 = text.chars().map(|c| u64::from(char_weight(c))).sum();
    u32::try_from(thirtieths.div_ceil(30)).unwrap_or(u32::MAX)
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

/// Splits one word longer than the budget at char boundaries, each piece within the budget.
fn split_long_word(word: &str, budget: u32) -> Vec<String> {
    let limit = u64::from(budget) * 30;
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut used = 0_u64;
    for c in word.chars() {
        let weight = u64::from(char_weight(c));
        if !current.is_empty() && used + weight > limit {
            pieces.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(c);
        used += weight;
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
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
/// with fewer than half the words or characters (above 24 input characters) is rejected.
pub fn check_output(input: &str, output: &str) -> Result<(), LocalLlmError> {
    let in_chars = input.chars().filter(|c| !c.is_whitespace()).count();
    let out_chars = output.chars().filter(|c| !c.is_whitespace()).count();
    if in_chars > 24 && out_chars.saturating_mul(2) < in_chars {
        return Err(LocalLlmError::BadOutput(
            "output has under 50% of input characters".into(),
        ));
    }
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

    #[test]
    fn wbfix3_unspaced_output_loss_is_rejected() {
        let input = "語".repeat(1200);
        for output in [String::new(), "語".repeat(599)] {
            assert!(
                matches!(
                    check_output(&input, &output),
                    Err(LocalLlmError::BadOutput(_))
                ),
                "unspaced transcript loss must be rejected"
            );
        }
        assert!(check_output(&input, &"語".repeat(600)).is_ok());
        assert!(check_output(&input, &input).is_ok());
    }

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
        let msgs = build_messages(
            PromptStyle::S1ControlLine,
            S1_SYSTEM_PROMPT,
            &control,
            transcript,
        );
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
    fn token_estimate_weights_characters_by_script() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abc"), 1);
        assert_eq!(est_tokens("abcd"), 2);
        assert_eq!(est_tokens("abcdef"), 2);
        assert_eq!(est_tokens("\u{8a9e}\u{8a9e}\u{8a9e}"), 3);
        assert_eq!(est_tokens("\u{1f600}\u{1f600}\u{1f600}"), 9);
    }

    #[test]
    fn latin_script_keeps_one_token_per_three_characters() {
        let samples = [
            "so um i need to send the report by friday",
            "El vuelo IB 3172 sale a las 18:40, no a las 8:40. Ojo con eso.",
            "1234567890 \t\n x",
            "caf\u{e9} na\u{ef}ve \u{201c}quoted\u{201d} \u{2014} \u{20ac}5 \u{2192} done",
            "\u{1ea1}\u{1ebf}n \u{1eef}ng v\u{1ec7}t",
        ];
        for text in samples {
            let chars = u32::try_from(text.chars().count()).unwrap();
            assert_eq!(
                est_tokens(text),
                chars.div_ceil(3),
                "Latin-script text must stay at one token per three characters: {text}"
            );
        }
    }

    /// Real counts are from llama-server `/tokenize` on S1-mini and Muesli 2B over the
    /// eval samples. At one token per three characters each of these was under-budgeted.
    #[test]
    fn token_estimate_covers_measured_indic_samples() {
        let cases = [
            // Gurmukhi (v2-017): 46 chars, 72 S1-mini tokens, 35 Muesli tokens.
            ("ਉਮ ਬਿੱਲੇ ਤੂੰ ਸ਼ਨੀਵਾਰ ਨੂੰ ਮੈਚ ਖੇਡਣ ਆਉਣਾ ਕਿ ਨਹੀਂ", 96, 72, 35),
            // Devanagari (v2-009): 51 chars, 51 S1-mini tokens, 23 Muesli tokens.
            ("उम मेरी शिफ्ट रात नौ बजे नहीं नहीं दस बजे शुरू होगी", 64, 51, 23),
        ];
        for (text, expected, s1_mini, muesli) in cases {
            let estimate = est_tokens(text);
            assert_eq!(
                estimate, expected,
                "script-weighted estimate must match for {text}"
            );
            assert!(
                estimate >= s1_mini.max(muesli),
                "estimate must cover the measured tokens of {text}"
            );
            let one_per_three = u32::try_from(text.chars().count()).unwrap().div_ceil(3);
            assert!(
                one_per_three < s1_mini.max(muesli),
                "one token per three characters was enough for {text}"
            );
        }
    }

    #[test]
    fn long_indic_words_split_within_the_budget() {
        let gurmukhi = "ਸੱਚ".repeat(400);
        let devanagari = "सत्य".repeat(400);
        for transcript in [gurmukhi, devanagari] {
            let chunks = chunk_transcript(&transcript, 64);
            assert!(
                chunks.len() > 1,
                "indic word must be split into several chunks"
            );
            for chunk in &chunks {
                assert!(
                    est_tokens(chunk) <= 64,
                    "indic chunk must stay within the budget"
                );
            }
            assert_eq!(chunks.join("").replace(' ', ""), transcript);
        }
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
        assert!(check_output(input, "six seven eight nine ten").is_ok());
        assert!(matches!(
            check_output(input, "one two three four"),
            Err(LocalLlmError::BadOutput(_))
        ));
    }

    #[test]
    fn plain_prompt_uses_selected_handy_prompt_and_verbatim_transcript() {
        let mut settings = crate::settings::get_default_settings();
        settings.post_process_prompts = vec![crate::settings::LLMPrompt {
            id: "selected".into(),
            name: "Selected".into(),
            prompt: "Format as prose. ${output} Preserve names. ${output}".into(),
        }];
        settings.post_process_selected_prompt_id = Some("selected".into());
        let system = system_prompt(PromptStyle::PlainSystemPrompt, &settings).unwrap();
        assert_eq!(system, "Format as prose.  Preserve names. ");
        let transcript = "[Styling: casual]\nhello ${output}";
        let messages = build_messages(
            PromptStyle::PlainSystemPrompt,
            &system,
            "unused",
            transcript,
        );
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, system);
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, transcript);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn plain_prompt_rejects_missing_or_effectively_empty_selection() {
        let mut settings = crate::settings::get_default_settings();
        for id in [None, Some("missing".to_string())] {
            settings.post_process_selected_prompt_id = id;
            assert!(system_prompt(PromptStyle::PlainSystemPrompt, &settings).is_err());
            assert_eq!(
                system_prompt(PromptStyle::S1ControlLine, &settings).unwrap(),
                S1_SYSTEM_PROMPT
            );
        }
        settings.post_process_selected_prompt_id = Some("selected".into());
        for text in ["", " \n", "${output}", " ${output} \n ${output}"] {
            settings.post_process_prompts = vec![crate::settings::LLMPrompt {
                id: "selected".into(),
                name: "Selected".into(),
                prompt: text.into(),
            }];
            assert!(system_prompt(PromptStyle::PlainSystemPrompt, &settings).is_err());
        }
    }
}
