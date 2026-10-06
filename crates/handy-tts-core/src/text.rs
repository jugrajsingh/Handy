use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
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
struct Fragment<'a> {
    value: pulldown_cmark::CowStr<'a>,
    range: Range<usize>,
    offset: usize,
    base: Option<usize>,
    inline: bool,
    html: bool,
}
/// An incremental Markdown preparation and chunking cursor.
pub struct TextCursor<'a> {
    source: &'a str,
    events: Box<dyn Iterator<Item = (Event<'a>, Range<usize>)> + 'a>,
    pending: VecDeque<Piece>,
    fragment: Option<Fragment<'a>>,
    word: String,
    word_range: Option<Range<usize>>,
    word_code: Vec<Range<usize>>,
    read_code: bool,
    in_code: bool,
    skip_depth: usize,
    index: usize,
    byte_cursor: usize,
    utf16_cursor: u32,
    emoji: Regex,
    version: Regex,
    #[cfg(test)]
    work_units: usize,
    #[cfg(test)]
    event_reads: std::rc::Rc<std::cell::Cell<usize>>,
}
impl<'a> TextCursor<'a> {
    /// Creates a cursor that reads or skips fenced code blocks.
    pub fn new(source: &'a str, read_code: bool) -> Result<Self, TextError> {
        let events = Parser::new_ext(
            source,
            Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES | Options::ENABLE_STRIKETHROUGH,
        )
        .into_offset_iter();
        #[cfg(test)]
        let event_reads = std::rc::Rc::new(std::cell::Cell::new(0));
        #[cfg(test)]
        let events = events.inspect({
            let reads = event_reads.clone();
            move |_| reads.set(reads.get() + 1)
        });
        Ok(Self {
            source,
            events: Box::new(events),
            pending: VecDeque::new(),
            fragment: None,
            word: String::new(),
            word_range: None,
            word_code: Vec::new(),
            read_code,
            in_code: false,
            skip_depth: 0,
            index: 0,
            byte_cursor: 0,
            utf16_cursor: 0,
            emoji: Regex::new(
                r"[0-9#*]\x{FE0F}?\x{20E3}|[\p{Extended_Pictographic}\p{Regional_Indicator}\x{1F3FB}-\x{1F3FF}\x{FE0E}\x{FE0F}\x{200D}\x{20E3}]",
            )?,
            version: Regex::new(r"\bv([0-9]+\.[0-9]+\.[0-9]+)")?,
            #[cfg(test)]
            work_units: 0,
            #[cfg(test)]
            event_reads,
        })
    }
    fn normalize(&self, word: &str, code: &[Range<usize>]) -> String {
        let raw_word = word;
        let stripped = self.emoji.replace_all(word, "");
        let word = stripped.as_ref();
        let bare = word.trim_matches(['(', ')', '[', ']', '{', '}', '\'', '"']);
        let bare = bare.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        let bare = bare.trim_matches(['(', ')', '[', ']', '{', '}', '\'', '"']);
        let ending = word
            .trim_end_matches([')', ']', '}', '\'', '"'])
            .chars()
            .last()
            .filter(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
        if bare.starts_with("https://") || bare.starts_with("http://") {
            let host = bare
                .split_once("://")
                .map(|(_, s)| s.split(['/', '?', '#']).next().unwrap_or(""))
                .unwrap_or("");
            return format!(
                "link to {host}{}",
                ending.map(String::from).unwrap_or_default()
            );
        }
        let version = self.version.replace_all(word, "version $1");
        if version != word {
            return version.into_owned();
        }
        if word == "&" {
            return "and".into();
        }
        let tail = bare.rsplit('/').next().unwrap_or(bare);
        let extension = tail.rsplit_once('.').is_some_and(|(_, ext)| {
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        });
        if ["/", "./", "../", "~/"].iter().any(|p| bare.starts_with(p))
            || (bare.contains('/') && extension)
        {
            return format!("{tail}{}", ending.map(String::from).unwrap_or_default());
        }
        let mut spoken = raw_word.to_string();
        for range in code.iter().rev() {
            if let Some(fragment) = spoken.get(range.clone()) {
                let normalized = fragment.replace(['_', '.'], " ");
                spoken.replace_range(range.clone(), &normalized);
            }
        }
        if spoken.contains('/') && !bare.chars().all(|c| c.is_ascii_digit() || c == '/') {
            spoken = spoken.replace('/', " ");
        }
        self.emoji.replace_all(&spoken, "").trim().to_string()
    }
    fn set_fragment(
        &mut self,
        value: pulldown_cmark::CowStr<'a>,
        range: Range<usize>,
        inline: bool,
    ) {
        let ptr = value.as_ptr() as usize;
        let source_ptr = self.source.as_ptr() as usize;
        let base = ptr.checked_sub(source_ptr).filter(|offset| {
            *offset <= self.source.len() && value.len() <= self.source.len() - *offset
        });
        self.fragment = Some(Fragment {
            value,
            range,
            offset: 0,
            base,
            inline,
            html: false,
        });
    }
    fn finish_word(&mut self) -> Option<Piece> {
        let raw = std::mem::take(&mut self.word);
        let range = self.word_range.take()?;
        let code = std::mem::take(&mut self.word_code);
        let text = self.normalize(&raw, &code);
        if text.trim().is_empty() {
            None
        } else {
            Some(Piece {
                text,
                range,
                pause: 0,
            })
        }
    }
    fn boundary(&mut self, range: Range<usize>, pause: u32) -> Option<Piece> {
        let marker = Piece {
            text: String::new(),
            range,
            pause,
        };
        if let Some(word) = self.finish_word() {
            self.pending.push_back(marker);
            Some(word)
        } else {
            Some(marker)
        }
    }
    fn tag_separates_words(&self, tag: &str, source_end: usize) -> bool {
        let name = tag
            .trim_start_matches('<')
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(name.as_str(), "br" | "p" | "div" | "li") {
            return true;
        }
        if matches!(
            name.as_str(),
            "b" | "i"
                | "em"
                | "span"
                | "strong"
                | "u"
                | "s"
                | "small"
                | "sup"
                | "sub"
                | "code"
                | "mark"
                | "del"
                | "ins"
        ) {
            return false;
        }
        let word_character = |c: char| c.is_alphanumeric() || c == '_';
        self.word.chars().last().is_some_and(word_character)
            && self
                .source
                .get(source_end..)
                .and_then(|text| text.chars().next())
                .is_some_and(word_character)
    }
    fn piece(&mut self) -> Option<Piece> {
        loop {
            if let Some(p) = self.pending.pop_front() {
                return Some(p);
            }
            if let Some(mut f) = self.fragment.take()
                && let Some(ch) = f.value[f.offset..].chars().next()
            {
                #[cfg(test)]
                {
                    self.work_units += 1;
                }
                let start = f.offset;
                f.offset += ch.len_utf8();
                if f.html
                    && ch == '<'
                    && let Some(end) = f.value[f.offset..].find('>')
                {
                    let tag = &f.value[f.offset..f.offset + end];
                    let source_end = f
                        .base
                        .map(|base| base + f.offset + end + 1)
                        .unwrap_or(f.range.end);
                    let separates = self.tag_separates_words(tag, source_end);
                    f.offset += end + 1;
                    self.fragment = Some(f);
                    if separates && let Some(word) = self.finish_word() {
                        return Some(word);
                    }
                    continue;
                }
                let mut atom = ch.to_string();
                if f.html
                    && ch == '&'
                    && let Some((end, _)) = f.value[start..]
                        .char_indices()
                        .take(32)
                        .find(|(_, c)| *c == ';')
                {
                    let raw = &f.value[start..start + end + 1];
                    atom = Parser::new(raw)
                        .filter_map(|event| match event {
                            Event::Text(t) => Some(t.into_string()),
                            _ => None,
                        })
                        .collect::<String>();
                    f.offset = start + end + 1;
                    #[cfg(test)]
                    {
                        self.work_units += raw.chars().count() - 1;
                    }
                }
                if atom.chars().all(char::is_whitespace) {
                    self.fragment = Some(f);
                    if let Some(p) = self.finish_word() {
                        return Some(p);
                    }
                    continue;
                }
                let mapped = f
                    .base
                    .map(|b| b + start..b + f.offset)
                    .unwrap_or_else(|| f.range.clone());
                self.word_range = Some(match self.word_range.take() {
                    Some(r) => r.start..mapped.end,
                    None => mapped,
                });
                if f.inline {
                    let offset = self.word.len();
                    if let Some(last) = self.word_code.last_mut().filter(|r| r.end == offset) {
                        last.end += atom.len();
                    } else {
                        self.word_code.push(offset..offset + atom.len());
                    }
                }
                self.word.push_str(&atom);
                self.fragment = Some(f);
                continue;
            }
            let Some((event, range)) = self.events.next() else {
                return self.finish_word();
            };
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
                Event::Start(Tag::Item) => {
                    if !self.word.is_empty() {
                        return self.boundary(range, 250);
                    }
                }
                Event::Start(
                    Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::TableCell
                    | Tag::CodeBlock(CodeBlockKind::Indented),
                ) => {
                    if let Some(word) = self.finish_word() {
                        return Some(word);
                    }
                }
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => {
                    self.in_code = true;
                    if !self.read_code {
                        self.skip_depth = 1;
                        self.pending.push_back(Piece {
                            text: "code block skipped.".into(),
                            range: range.clone(),
                            pause: 0,
                        });
                        self.pending.push_back(Piece {
                            text: String::new(),
                            range,
                            pause: 250,
                        });
                        if let Some(word) = self.finish_word() {
                            return Some(word);
                        }
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.in_code = false;
                    return self.boundary(range, 250);
                }
                Event::Text(t) => self.set_fragment(t, range, self.in_code),
                Event::Code(t) => self.set_fragment(t, range, true),
                Event::End(TagEnd::Heading(_)) => return self.boundary(range, 450),
                Event::End(TagEnd::HtmlBlock) => return self.boundary(range, 250),
                Event::End(TagEnd::Item | TagEnd::Paragraph) => return self.boundary(range, 250),
                Event::End(TagEnd::TableCell) => return self.boundary(range, 100),
                Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                    return self.boundary(range, 250);
                }
                Event::HardBreak | Event::SoftBreak => {
                    if let Some(p) = self.finish_word() {
                        return Some(p);
                    }
                }
                Event::Html(t) => {
                    self.set_fragment(t, range, false);
                    if let Some(fragment) = self.fragment.as_mut() {
                        fragment.html = true;
                    }
                }
                Event::InlineHtml(tag) => {
                    if self.tag_separates_words(&tag, range.end)
                        && let Some(word) = self.finish_word()
                    {
                        return Some(word);
                    }
                }
                Event::FootnoteReference(_) => {}
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
    /// Prepares the next spoken chunk, returning None at the end of the source.
    pub fn next_chunk(&mut self) -> Result<Option<TextChunk>, TextError> {
        let limit = if self.index == 0 { 70 } else { 300 };
        let mut words: Vec<Piece> = Vec::new();
        let mut size = 0;
        let mut pause = 0;
        let mut boundary = None;
        while let Some(p) = self.piece() {
            if p.text.is_empty() {
                if words.is_empty() {
                    continue;
                }
                pause = p.pause;
                if pause == 100
                    && let Some(next) = self.piece()
                {
                    if next.text.is_empty() && next.pause >= 250 {
                        pause = next.pause;
                    } else {
                        self.pending.push_front(next);
                    }
                }
                break;
            }
            let added = p.text.chars().count() + usize::from(!words.is_empty());
            if !words.is_empty() && size + added > limit {
                self.pending.push_front(p);
                if let Some(cut) = boundary {
                    for extra in words.drain(cut..).rev() {
                        self.pending.push_front(extra);
                    }
                } else if self.index > 0
                    && let Some(cut) = words.iter().rposition(|w| w.text.ends_with([',', ';']))
                {
                    for extra in words.drain(cut + 1..).rev() {
                        self.pending.push_front(extra);
                    }
                }
                break;
            }
            size += added;
            let ending = p.text.trim_end_matches([')', ']', '}', '\'', '"']);
            let end_sentence = ending.ends_with(['.', '!', '?']);
            let natural = ending.ends_with(['.', '!', '?', ',', ';', ':']);
            words.push(p);
            if end_sentence {
                boundary = Some(words.len());
            }
            if self.index == 0 && natural && size >= 40 {
                if let Some(next) = self.piece() {
                    if next.text.is_empty() {
                        pause = next.pause;
                    } else {
                        self.pending.push_front(next);
                    }
                }
                break;
            }
        }
        if words.is_empty() {
            return Ok(None);
        }
        let range = words[0].range.start..words.last().ok_or(TextError::Range)?.range.end;
        let mut text = words
            .into_iter()
            .map(|w| w.text)
            .collect::<Vec<_>>()
            .join(" ");
        if pause >= 250
            && !text
                .trim_end_matches([')', ']', '}', '\'', '"'])
                .ends_with(['.', '!', '?', ':', ',', ';'])
        {
            text.push('.');
        }
        let (source_start, source_end) = self.source_range(range)?;
        let chunk = TextChunk {
            index: self.index,
            text,
            source_start,
            source_end,
            pause_ms: pause,
        };
        self.index += 1;
        Ok(Some(chunk))
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
        assert_eq!(chunk.source_start, 3);
        assert_eq!(chunk.source_end, unicode.encode_utf16().count() as u32);
        assert_eq!(chunk.text, "naïve readers enjoy clear sentences.");
        let word = "x".repeat(400);
        assert_eq!(prepared(&word, false), format!("{word}."));
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
#[cfg(test)]
mod fixes {
    use super::*;
    fn chunks(s: &str) -> Vec<TextChunk> {
        let mut c = TextCursor::new(s, false).unwrap();
        std::iter::from_fn(|| c.next_chunk().unwrap()).collect()
    }
    fn spoken(s: &str) -> String {
        chunks(s)
            .into_iter()
            .map(|c| c.text)
            .collect::<Vec<_>>()
            .join(" ")
    }
    #[test]
    fn structural_pauses() {
        assert_eq!(chunks("# Heading")[0].pause_ms, 450);
        assert_eq!(chunks("Paragraph text.")[0].pause_ms, 250);
        let table = chunks("| Name | Value |\n| --- | --- |\n| A | 42 |\n");
        assert_eq!(
            table.iter().map(|c| c.pause_ms).collect::<Vec<_>>(),
            vec![100, 250, 100, 250]
        );
        assert_eq!(
            table.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["Name", "Value.", "A", "42."]
        );
    }
    #[test]
    fn html_and_footnote_markers_are_dropped() {
        assert_eq!(
            spoken("<b>hello</b> note[^secret]\n\n[^secret]: hidden"),
            "hello note."
        );
        assert_eq!(
            spoken("<div>Read me &amp; you</div>\n\nEnd."),
            "Read me and you. End."
        );
    }
    #[test]
    fn removed_html_tags_separate_text_without_splitting_formatted_words() {
        for source in [
            "Line one<br>line two",
            "Line one<br/>line two",
            "Line one<BR />line two",
            "Line one</p><p>line two",
            "Line one<div>line two</div>",
            "Line one<li>line two</li>",
            "Line one<custom>line two",
            "Line one<br><br/>  line two",
            "<div>Line one<br/>line two</div>",
        ] {
            assert_eq!(spoken(source), "Line one line two.", "{source}");
        }
        for source in [
            "a<b>bold</b>word",
            "a<i>bold</i>word",
            "a<em>bold</em>word",
            "a<span>bold</span>word",
            "a<strong>bold</strong>word",
            "<div>a<b>bold</b>word</div>",
        ] {
            assert_eq!(spoken(source), "aboldword.", "{source}");
        }
        assert_eq!(spoken("two<span> </span>words"), "two words.");
        let table = chunks("| Words |\n| --- |\n| first<br>second |\n");
        assert_eq!(table[1].text, "first second.");
    }
    #[test]
    fn later_limit_and_sentence_boundaries() {
        let left = format!("{}ends", "word ".repeat(59));
        let right = format!("{}end", "word ".repeat(59));
        let sentence = format!("{left}, {right}.");
        assert_eq!(sentence.chars().count(), 600);
        let s = format!("# Start\n\n{sentence}");
        let out = chunks(&s);
        assert_eq!(out.len(), 3);
        assert!(out[1].text.ends_with(','));
        assert!(out.iter().all(|c| c.text.chars().count() <= 300));
        let normal = chunks(
            "# Start\n\nThen the deadline is October 12, 2026, at 18:45 UTC. Read it; remember it.",
        );
        assert_eq!(normal.len(), 2);
    }
    #[test]
    fn exact_utf16_ranges() {
        let s = "Naïve first sentence is long enough to pass forty chars. 😀😀 Ünïcödé second sentence follows here.";
        let out = chunks(s);
        let second = s.find("Ünïcödé").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].source_start, out[0].source_end),
            (
                0,
                s[..s.find(". ").unwrap() + 1].encode_utf16().count() as u32
            )
        );
        assert_eq!(
            out[1].source_start,
            s[..second].encode_utf16().count() as u32
        );
        assert_eq!(out[1].source_end, s.encode_utf16().count() as u32);
        let s = "<div>Naïve first sentence is long enough to pass forty chars. 😀😀 Ünïcödé &amp; second.</div>";
        let out = chunks(s);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].source_start, 5);
        assert_eq!(
            out[0].source_end,
            s[..s.find(". ").unwrap() + 1].encode_utf16().count() as u32
        );
        assert_eq!(
            out[1].source_start,
            s[..s.find("Ünïcödé").unwrap()].encode_utf16().count() as u32
        );
        assert_eq!(
            out[1].source_end,
            s[..s.find("</div>").unwrap()].encode_utf16().count() as u32
        );
        assert_eq!(out[1].text, "Ünïcödé and second.");
    }
    #[test]
    fn first_chunk_work_is_bounded_on_large_single_line() {
        let s = "A readable sentence of at least forty characters ends here. ".repeat(20000);
        let mut c = TextCursor::new(&s, false).unwrap();
        assert!(c.next_chunk().unwrap().is_some());
        assert!(c.work_units < 400, "{}", c.work_units);
        for _ in 0..200 {
            c.next_chunk().unwrap();
        }
        assert!(c.work_units < 65000, "{}", c.work_units);
    }
    #[test]
    fn first_chunk_prefers_punctuation_then_word_boundary() {
        let s =
            "First natural boundary has enough readable words here, and then we continue speaking.";
        let out = chunks(s);
        assert!(out[0].text.ends_with(','));
        assert!((40..=70).contains(&out[0].text.chars().count()));
        let s = r#""A quoted sentence with enough characters ends here." More follows."#;
        let out = chunks(s);
        assert!(out[0].text.ends_with(".\""));
        assert!(!out[1].text.ends_with(".\"."));
        let s = "words ".repeat(30);
        assert!(chunks(&s)[0].text.chars().count() <= 70);
        assert_eq!(chunks(&s)[0].text.split_whitespace().last(), Some("words"));
        assert_eq!(
            chunks("# Short\n\nA following paragraph.")[0].text,
            "Short."
        );
    }
    #[test]
    fn skipped_fence_does_not_change_following_prose() {
        assert_eq!(
            spoken("```\nsecret();\n```\n\n## 3.6 Footer\n\nGPU3/Vulkan and repo/model2."),
            "code block skipped. 3.6 Footer. GPU3 Vulkan and repo model2."
        );
        assert_eq!(
            spoken("<div>AT&amp;T &#65; &#x42; &euro;</div>"),
            "AT&T A B €."
        );
    }
    #[test]
    fn first_chunk_does_not_consume_document_events() {
        let s = "A readable sentence of at least forty characters ends here.\n\n".repeat(20000);
        let mut c = TextCursor::new(&s, false).unwrap();
        assert!(c.next_chunk().unwrap().is_some());
        assert!(c.event_reads.get() < 10, "{}", c.event_reads.get());
    }
    #[test]
    fn normalization_preserves_content() {
        let s = "Use and/or TCP/IP 24/7 10/01/2026 1/2 Diff/Side provider/model GPU/Vulkan /a/file.rs ./b.txt ../c.md ~/d.png a/b.rs.";
        assert_eq!(
            spoken(s),
            "Use and or TCP IP 24/7 10/01/2026 1/2 Diff Side provider model GPU Vulkan file.rs b.txt c.md d.png b.rs."
        );
        assert_eq!(
            spoken("Visit (https://example.com/a). Then \"https://example.org/b\"."),
            "Visit link to example.com. Then link to example.org."
        );
        assert_eq!(
            spoken("AT&amp;T foo*bar (v0.9.7) `v1.2.3`"),
            "AT&T foo*bar (version 0.9.7) version 1.2.3."
        );
        assert_eq!(
            spoken("Flags 🇺🇸 keycap 1️⃣ skin 👍🏽 family 👨‍👩‍👧‍👦 done."),
            "Flags keycap skin family done."
        );
        assert_eq!(
            spoken("Intro.\n\n    Indented prose here.\n"),
            "Intro. Indented prose here."
        );
        assert_eq!(
            spoken("words continue\nonto next line."),
            "words continue onto next line."
        );
    }
}
