use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use std::collections::VecDeque;
use std::ops::Range;
use thiserror::Error;

/// A spoken chunk with its original UTF-16 source range and trailing pause.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextChunk {
    pub index: usize,
    pub text: String,
    pub source_start: u32,
    pub source_end: u32,
    pub pause_ms: u32,
}
/// Errors from normalizer initialization or source range conversion.
#[derive(Debug, Error)]
pub enum TextError {
    #[error("text normalizer initialization failed")]
    Regex(#[from] regex::Error),
    #[error("source character range overflow")]
    Range,
}
struct Piece {
    text: String,
    range: Range<usize>,
    pause: u32,
}
/// An incremental Markdown preparation and chunking cursor.
pub struct TextCursor<'a> {
    source: &'a str,
    events: Box<dyn Iterator<Item = (Event<'a>, Range<usize>)> + 'a>,
    pending: VecDeque<Piece>,
    text_event: Option<(pulldown_cmark::CowStr<'a>, Range<usize>, usize, bool)>,
    read_code: bool,
    in_code: bool,
    skip_depth: usize,
    index: usize,
    byte_cursor: usize,
    utf16_cursor: u32,
    emoji: Regex,
    version: Regex,
}
impl<'a> TextCursor<'a> {
    /// Creates a cursor that reads or skips fenced code blocks.
    pub fn new(source: &'a str, read_code: bool) -> Result<Self, TextError> {
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES | Options::ENABLE_STRIKETHROUGH;
        Ok(Self {
            source,
            events: Box::new(Parser::new_ext(source, options).into_offset_iter()),
            pending: VecDeque::new(),
            text_event: None,
            read_code,
            in_code: false,
            skip_depth: 0,
            index: 0,
            byte_cursor: 0,
            utf16_cursor: 0,
            emoji: Regex::new(r"[\p{Extended_Pictographic}\x{FE0F}\x{200D}]")?,
            version: Regex::new(r"^v([0-9]+\.[0-9]+\.[0-9]+)(.*)$")?,
        })
    }
    fn normalize(&self, word: &str, inline: bool) -> String {
        let stripped = self.emoji.replace_all(word, "");
        let word = stripped.as_ref();
        if word.starts_with("https://") || word.starts_with("http://") {
            let host = word
                .split_once("://")
                .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
                .unwrap_or("");
            return format!("link to {}", host.trim_end_matches(['.', ',', ';', ')']));
        }
        if inline {
            return word.replace(['_', '.'], " ");
        }
        if word.contains('/') || word.contains('\\') {
            let tail = word.rsplit(['/', '\\']).next().unwrap_or(word);
            if !tail.is_empty() {
                return tail.to_string();
            }
        }
        self.version.replace(word, "version $1$2").into_owned()
    }
    fn piece(&mut self) -> Option<Piece> {
        loop {
            if let Some(piece) = self.pending.pop_front() {
                return Some(piece);
            }
            if let Some((value, range, offset, inline)) = self.text_event.take() {
                let suffix = &value[offset..];
                let whitespace = suffix.len() - suffix.trim_start().len();
                let start = offset + whitespace;
                if start < value.len() {
                    let end = value[start..]
                        .find(char::is_whitespace)
                        .map(|i| start + i)
                        .unwrap_or(value.len());
                    let spoken = self.normalize(&value[start..end], inline);
                    let exact = self
                        .source
                        .get(range.clone())
                        .and_then(|raw| raw.find(value.as_ref()));
                    let mapped = if let Some(prefix) = exact {
                        range.start + prefix + start..range.start + prefix + end
                    } else {
                        range.clone()
                    };
                    self.text_event = Some((value, range, end, inline));
                    if !spoken.is_empty() {
                        return Some(Piece {
                            text: spoken,
                            range: mapped,
                            pause: 0,
                        });
                    }
                    continue;
                }
            }
            let (event, range) = self.events.next()?;
            if self.skip_depth > 0 {
                match event {
                    Event::Start(_) => self.skip_depth += 1,
                    Event::End(end) => {
                        self.skip_depth -= 1;
                        if self.skip_depth == 0 && end == TagEnd::CodeBlock {
                            self.in_code = false;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match event {
                Event::Start(Tag::FootnoteDefinition(_)) => self.skip_depth = 1,
                Event::Start(Tag::CodeBlock(_)) => {
                    self.in_code = true;
                    if !self.read_code {
                        self.skip_depth = 1;
                        return Some(Piece {
                            text: "code block skipped.".into(),
                            range,
                            pause: 250,
                        });
                    }
                }
                Event::End(TagEnd::CodeBlock) => self.in_code = false,
                Event::Text(text) => self.text_event = Some((text, range, 0, self.in_code)),
                Event::Code(text) => self.text_event = Some((text, range, 0, true)),
                Event::End(TagEnd::Heading(_)) => {
                    return Some(Piece {
                        text: ".".into(),
                        range,
                        pause: 450,
                    });
                }
                Event::End(
                    TagEnd::Item | TagEnd::Paragraph | TagEnd::TableHead | TagEnd::TableRow,
                ) => {
                    return Some(Piece {
                        text: ".".into(),
                        range,
                        pause: 250,
                    });
                }
                Event::End(TagEnd::TableCell) => {
                    return Some(Piece {
                        text: ",".into(),
                        range,
                        pause: 100,
                    });
                }
                Event::HardBreak | Event::SoftBreak => {
                    return Some(Piece {
                        text: String::new(),
                        range,
                        pause: 0,
                    });
                }
                Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
                _ => {}
            }
        }
    }
    fn source_range(&mut self, range: Range<usize>) -> Result<(u32, u32), TextError> {
        let start = range.start.max(self.byte_cursor);
        self.utf16_cursor = self
            .utf16_cursor
            .checked_add(
                u32::try_from(self.source[self.byte_cursor..start].encode_utf16().count())
                    .map_err(|_| TextError::Range)?,
            )
            .ok_or(TextError::Range)?;
        let start_char = self.utf16_cursor;
        let end = range.end.max(start);
        self.utf16_cursor = self
            .utf16_cursor
            .checked_add(
                u32::try_from(self.source[start..end].encode_utf16().count())
                    .map_err(|_| TextError::Range)?,
            )
            .ok_or(TextError::Range)?;
        self.byte_cursor = end;
        Ok((start_char, self.utf16_cursor))
    }
    /// Prepares the next spoken chunk, returning None when the source is exhausted.
    pub fn next_chunk(&mut self) -> Result<Option<TextChunk>, TextError> {
        let limit = if self.index == 0 { 70 } else { 300 };
        let mut text = String::new();
        let mut range: Option<Range<usize>> = None;
        let mut pause = 0;
        while let Some(piece) = self.piece() {
            if text.is_empty() && matches!(piece.text.as_str(), "." | ",") {
                continue;
            }
            let addition = piece.text.chars().count()
                + usize::from(!text.is_empty() && !matches!(piece.text.as_str(), "." | ","));
            if !text.is_empty() && text.chars().count() + addition > limit {
                self.pending.push_front(piece);
                break;
            }
            if !matches!(piece.text.as_str(), "." | ",") && !text.is_empty() {
                text.push(' ');
            }
            if !(piece.text == "." && text.ends_with(['.', '!', '?'])) {
                text.push_str(&piece.text);
            }
            range = Some(match range {
                Some(previous) => {
                    previous.start.min(piece.range.start)..previous.end.max(piece.range.end)
                }
                None => piece.range,
            });
            pause = pause.max(piece.pause);
            if piece.pause >= 250 && !text.is_empty() {
                break;
            }
            if text.ends_with(['.', '!', '?', ',', ';'])
                && (self.index != 0 || text.chars().count() >= 40)
            {
                if let Some(next) = self.piece() {
                    if matches!(next.text.as_str(), "." | ",") && next.pause > 0 {
                        pause = pause.max(next.pause);
                    } else {
                        self.pending.push_front(next);
                    }
                }
                break;
            }
        }
        if text.trim().is_empty() {
            return Ok(None);
        }
        let range = range.ok_or(TextError::Range)?;
        let (source_start, source_end) = self.source_range(range)?;
        let output = TextChunk {
            index: self.index,
            text: text.trim().into(),
            source_start,
            source_end,
            pause_ms: pause,
        };
        self.index += 1;
        Ok(Some(output))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn prepared(source: &str, code: bool) -> String {
        let mut cursor = TextCursor::new(source, code).unwrap();
        let mut all = Vec::new();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            all.push(chunk.text);
        }
        all.join(" ")
    }
    #[test]
    fn section_four_markdown_rules_have_golden_outputs() {
        let source = "# Heading\n\n- **first** item\n- second [label](https://example.com/path)\n\n`foo_bar.rs` and v0.9.7 at /a/b/file.rs.\n\nhttps://example.org/path ![Picture](img.png) <b>hello</b> 😀 note[^1]\n\n```rust\nsecret();\n```\n\n| Name | Value |\n| --- | --- |\n| A | 42 |\n\n[^1]: omit footnote\n";
        let result = prepared(source, false);
        assert!(result.contains("first item."));
        assert!(result.contains("second label."));
        assert!(result.contains("foo bar rs"));
        assert!(result.contains("version 0.9.7"));
        assert!(result.contains("file.rs."));
        assert!(result.contains("link to example.org"));
        assert!(result.contains("Picture"));
        assert!(!result.contains("😀"));
        assert!(result.contains("code block skipped."));
        assert!(!result.contains("secret"));
        assert!(!result.contains("omit footnote"));
        assert!(prepared("```\nfoo_bar.rs\n```", true).contains("foo bar rs"));
    }
    #[test]
    fn never_splits_a_word_and_returns_utf16_ranges_for_highlighting() {
        let source = "Reading clearly, one phrase at a time. This second sentence contains naïve names and remains bounded.";
        let mut cursor = TextCursor::new(source, false).unwrap();
        let mut last = 0;
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            assert!(chunk.source_start >= last);
            assert!(chunk.source_end > chunk.source_start);
            assert!(chunk.source_end <= source.encode_utf16().count() as u32);
            last = chunk.source_end;
        }
        let unicode = "😀 naïve readers enjoy clear sentences.";
        let mut cursor = TextCursor::new(unicode, false).unwrap();
        let chunk = cursor.next_chunk().unwrap().unwrap();
        assert_eq!(chunk.source_start, 0);
        assert_eq!(chunk.source_end, unicode.encode_utf16().count() as u32);
        assert_eq!(chunk.text, "naïve readers enjoy clear sentences.");
        let word = "x".repeat(400);
        assert_eq!(prepared(&word, false), word);
    }
}

#[cfg(test)]
mod bakeoff_goldens {
    use super::*;
    #[test]
    fn all_seven_bakeoff_inputs_match_reviewed_preparation_and_chunks() {
        let cases: std::collections::BTreeMap<String, Vec<String>> =
            serde_json::from_str(include_str!("../tests/fixtures/text/goldens.json")).unwrap();
        assert_eq!(cases.len(), 7);
        for (name, expected) in cases {
            let source = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/text")
                    .join(&name),
            )
            .unwrap();
            let mut cursor = TextCursor::new(&source, false).unwrap();
            let mut output = Vec::new();
            while let Some(chunk) = cursor.next_chunk().unwrap() {
                output.push(chunk.text);
            }
            assert_eq!(output, expected, "{name}");
        }
    }
}
