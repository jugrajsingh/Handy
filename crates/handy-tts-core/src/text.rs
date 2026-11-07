use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use regex::Regex;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;

use crate::vocabulary::{TechnicalOptions, Vocabulary, speak_technical};

/// Request size used by the streaming session to keep synthesis lanes supplied.
pub const STREAMING_CHUNK_CHARS: usize = 100;

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
    html_exhausted: bool,
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
    chunk_limit: usize,
    byte_cursor: usize,
    utf16_cursor: u32,
    emoji: Regex,
    version: Regex,
    vocabulary: Option<Arc<Vocabulary>>,
    technical: TechnicalOptions,
    resume_byte: usize,
    #[cfg(test)]
    work_units: usize,
    #[cfg(test)]
    html_scan_bytes: usize,
    #[cfg(test)]
    event_reads: std::rc::Rc<std::cell::Cell<usize>>,
}
impl<'a> TextCursor<'a> {
    /// Creates a cursor with the streaming request budget.
    pub fn streaming(source: &'a str, read_code: bool) -> Result<Self, TextError> {
        Self::streaming_with(source, read_code, None, TechnicalOptions::default())
    }
    /// Creates a streaming cursor that reads technical tokens with `vocabulary` when one is
    /// given (technical reading on); `None` prepares text exactly as `streaming` always has.
    /// `technical` chooses full or short paths and links.
    pub fn streaming_with(
        source: &'a str,
        read_code: bool,
        vocabulary: Option<Arc<Vocabulary>>,
        technical: TechnicalOptions,
    ) -> Result<Self, TextError> {
        let mut cursor = Self::new(source, read_code)?;
        cursor.set_chunk_limit(STREAMING_CHUNK_CHARS);
        cursor.vocabulary = vocabulary;
        cursor.technical = technical;
        Ok(cursor)
    }
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
            chunk_limit: 300,
            byte_cursor: 0,
            utf16_cursor: 0,
            emoji: Regex::new(
                r"[0-9#*]\x{FE0F}?\x{20E3}|[\p{Extended_Pictographic}\p{Regional_Indicator}\x{1F3FB}-\x{1F3FF}\x{FE0E}\x{FE0F}\x{200D}\x{20E3}]",
            )?,
            version: Regex::new(r"\bv([0-9]+\.[0-9]+\.[0-9]+)")?,
            vocabulary: None,
            technical: TechnicalOptions::default(),
            resume_byte: 0,
            #[cfg(test)]
            work_units: 0,
            #[cfg(test)]
            html_scan_bytes: 0,
            #[cfg(test)]
            event_reads,
        })
    }
    /// Resumes preparation at `source_start`, a UTF-16 source offset such as a chunk's
    /// `source_start`, numbering the first chunk `index`: words ending at or before the offset
    /// are dropped, so that chunk starts there. Markdown is still parsed from the beginning, so
    /// its context and every source offset stay those of the whole source. Call before the
    /// first `next_chunk`.
    pub fn resume_at(&mut self, source_start: u32, index: usize) {
        self.index = index;
        let mut units = 0_u32;
        self.resume_byte = self.source.len();
        for (byte, c) in self.source.char_indices() {
            if units >= source_start {
                self.resume_byte = byte;
                break;
            }
            units += if c.len_utf16() == 2 { 2 } else { 1 };
        }
    }
    /// Lowers the later-chunk ceiling without changing the first chunk or word boundaries.
    pub fn set_chunk_limit(&mut self, limit: usize) {
        self.chunk_limit = limit.clamp(70, 300);
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
    /// The technical reading of one word, when a vocabulary is set and the reading differs from
    /// the word; a word it leaves unchanged takes the ordinary path, so prose reads as before.
    fn technical_word(&self, word: &str) -> Option<String> {
        let vocabulary = self.vocabulary.as_deref()?;
        let word = self.emoji.replace_all(word, "");
        let spoken = speak_technical(&word, vocabulary, &self.technical);
        (spoken != word).then_some(spoken)
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
            html_exhausted: false,
        });
    }
    fn finish_word(&mut self) -> Option<Piece> {
        let raw = std::mem::take(&mut self.word);
        let range = self.word_range.take()?;
        let code = std::mem::take(&mut self.word_code);
        let text = match self.technical_word(&raw) {
            Some(spoken) => spoken,
            None => self.normalize(&raw, &code),
        };
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
                    && !f.html_exhausted
                    && let Some(end) = {
                        #[cfg(test)]
                        {
                            self.html_scan_bytes += f.value[f.offset..]
                                .find('>')
                                .map_or(f.value.len() - f.offset, |end| end + 1);
                        }
                        let end = f.value[f.offset..].find('>');
                        f.html_exhausted = end.is_none();
                        end
                    }
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
                Event::End(TagEnd::Heading(_)) => return self.boundary(range, 600),
                Event::End(TagEnd::HtmlBlock) => return self.boundary(range, 450),
                Event::End(TagEnd::Item) => return self.boundary(range, 300),
                Event::End(TagEnd::Paragraph) => return self.boundary(range, 450),
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
    fn structural_pause(&mut self, mut pause: u32) -> (u32, bool) {
        let mut synthetic_period = pause >= 250;
        let mut immediate_marker = true;
        while let Some(next) = self.piece() {
            if !next.text.is_empty() {
                self.pending.push_front(next);
                break;
            }
            if immediate_marker && pause == 100 && next.pause >= 250 {
                synthetic_period = true;
            }
            immediate_marker = false;
            pause = pause.max(next.pause);
        }
        (pause, synthetic_period)
    }
    /// Where an over-long first piece is cut: at `cut`, the character limit, or with technical
    /// reading on, at the last space before it so no spoken word is split. Returns the end of
    /// the kept text and the start of the remainder.
    fn spoken_word_cut(&self, text: &str, cut: usize) -> (usize, usize) {
        if self.vocabulary.is_none() {
            return (cut, cut);
        }
        let window = cut + text[cut..].chars().next().map_or(0, char::len_utf8);
        match text[..window].rfind(' ') {
            Some(space) if space > 0 => (space, space + 1),
            _ => (cut, cut),
        }
    }
    /// With technical reading on, moves the proportional source cut of a split piece to the
    /// nearest point just after a separator such as `/`, `.` or `_`, so each part's highlight
    /// ends between source segments instead of inside one.
    fn source_cut_near(&self, source: &str, proportional: usize) -> usize {
        if self.vocabulary.is_none() {
            return proportional;
        }
        source
            .char_indices()
            .filter(|(_, c)| !c.is_alphanumeric())
            .map(|(index, c)| index + c.len_utf8())
            .filter(|end| *end < source.len())
            .min_by_key(|end| end.abs_diff(proportional))
            .unwrap_or(proportional)
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
        let limit = if self.index == 0 {
            70
        } else {
            self.chunk_limit
        };
        let mut words: Vec<Piece> = Vec::new();
        let mut size = 0;
        let mut pause = 0;
        let mut synthetic_period = false;
        let mut boundary = None;
        while let Some(mut p) = self.piece() {
            if self.resume_byte > 0 && p.range.end <= self.resume_byte {
                continue;
            }
            if p.text.is_empty() {
                if words.is_empty() {
                    continue;
                }
                (pause, synthetic_period) = self.structural_pause(p.pause);
                break;
            }
            if words.is_empty() && p.text.chars().count() > limit {
                let cut = p
                    .text
                    .char_indices()
                    .nth(limit - 1)
                    .map(|(i, _)| i)
                    .ok_or(TextError::Range)?;
                let source = &self.source[p.range.clone()];
                let source_count = source.chars().count();
                let spoken_count = p.text.chars().count();
                let (cut, resume) = self.spoken_word_cut(&p.text, cut);
                let source_cut = if source == p.text {
                    cut
                } else {
                    let count =
                        (p.text[..cut].chars().count() * source_count / spoken_count).max(1);
                    let proportional = source
                        .char_indices()
                        .nth(count)
                        .map_or(source.len(), |(i, _)| i);
                    self.source_cut_near(source, proportional)
                };
                let remainder = Piece {
                    text: p.text[resume..].to_owned(),
                    range: p.range.start + source_cut..p.range.end,
                    pause: p.pause,
                };
                p.text.truncate(cut);
                p.range.end = remainder.range.start;
                p.pause = 0;
                self.pending.push_front(remainder);
                words.push(p);
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
                (pause, synthetic_period) = self.structural_pause(0);
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
        let ending = text.trim_end_matches([')', ']', '}', '\'', '"']);
        let punctuation_pause = if ending.ends_with(['.', '!', '?']) {
            280
        } else if ending.ends_with([',', ';', ':']) {
            120
        } else {
            0
        };
        pause = pause.max(punctuation_pause);
        if synthetic_period
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
    fn wbfix_unbroken_words_are_bounded_with_exact_source_ranges_and_following_text() {
        for count in [400, 20_000] {
            for letter in ["x", "é"] {
                let word = letter.repeat(count);
                let source = format!("{word}\n\nFollowing text must be reached.");
                let mut cursor = TextCursor::streaming(&source, false).unwrap();
                let mut joined = String::new();
                let mut end = 0;
                while let Some(chunk) = cursor.next_chunk().unwrap() {
                    assert!(
                        chunk.text.chars().count() <= 100,
                        "synthesis text budget exceeded"
                    );
                    if chunk.source_start < word.encode_utf16().count() as u32 {
                        assert_eq!(chunk.source_start, end);
                        let original: String = source
                            .encode_utf16()
                            .skip(chunk.source_start as usize)
                            .take((chunk.source_end - chunk.source_start) as usize)
                            .map(|unit| char::from_u32(u32::from(unit)).unwrap())
                            .collect();
                        assert_eq!(chunk.text.trim_end_matches('.'), original);
                        joined.push_str(&original);
                    } else {
                        assert_eq!(chunk.text, "Following text must be reached.");
                    }
                    end = chunk.source_end;
                }
                assert_eq!(joined, word);
                assert_eq!(end as usize, source.encode_utf16().count());
            }
        }
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
    fn bounded_words_and_long_word_parts_keep_utf16_ranges_for_highlighting() {
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
        assert_eq!(prepared(&word, false).replace(' ', ""), format!("{word}."));
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
    #[test]
    fn smaller_chunks_preserve_prepared_words_ranges_and_structural_pauses() {
        let source = format!(
            "# A heading\n\n{}\n\nA final paragraph.",
            "These ordinary words stay in their original order. ".repeat(12)
        );
        let mut cursor = TextCursor::new(&source, false).unwrap();
        cursor.set_chunk_limit(100);
        let mut output = Vec::new();
        while let Some(chunk) = cursor.next_chunk().unwrap() {
            assert!(chunk.text.chars().count() <= 101);
            assert!(chunk.source_start < chunk.source_end);
            if let Some(previous) = output.last() {
                let previous: &TextChunk = previous;
                assert!(previous.source_end <= chunk.source_start);
            }
            output.push(chunk);
        }
        assert_eq!(output[0].pause_ms, 600);
        assert_eq!(output.last().unwrap().pause_ms, 450);
        assert_eq!(
            output
                .iter()
                .flat_map(|c| c.text.split_whitespace())
                .collect::<Vec<_>>(),
            chunks(&source)
                .iter()
                .flat_map(|c| c.text.split_whitespace())
                .collect::<Vec<_>>()
        );
    }
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
        assert_eq!(chunks("# Heading")[0].pause_ms, 600);
        assert_eq!(chunks("Paragraph text.")[0].pause_ms, 450);
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
    fn empty_trailing_table_cells_preserve_chunk_text() {
        let source = "| Name | |\n| --- | --- |\n| A | |";
        let out = chunks(source);
        assert_eq!(
            out.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["Name", "A"]
        );
        for (chunk, expected) in out.iter().zip(["Name", "A"]) {
            let start = source.find(expected).unwrap();
            assert_eq!(
                chunk.source_start,
                source[..start].encode_utf16().count() as u32
            );
            assert_eq!(
                chunk.source_end,
                source[..start + expected.len()].encode_utf16().count() as u32
            );
        }
        assert_eq!(
            out.iter().map(|c| c.pause_ms).collect::<Vec<_>>(),
            vec![250, 250]
        );
    }
    #[test]
    fn paragraph_pauses_inside_lists_keep_the_larger_boundary() {
        for source in [
            "- First paragraph.\n\n  Second paragraph.",
            "- First paragraph.\n\n- Second paragraph.",
        ] {
            let out = chunks(source);
            assert_eq!(
                out.iter().map(|c| c.pause_ms).collect::<Vec<_>>(),
                vec![450, 450]
            );
            for (chunk, expected) in out.iter().zip(["First paragraph.", "Second paragraph."]) {
                assert_eq!(chunk.text, expected);
                let start = source.find(expected).unwrap();
                assert_eq!(chunk.source_start, start as u32);
                assert_eq!(chunk.source_end, (start + expected.len()) as u32);
            }
        }
        assert_eq!(chunks("- Tight item.")[0].pause_ms, 300);
    }
    #[test]
    fn sentence_and_clause_pauses() {
        let out = chunks(
            "A first sentence with enough words to pass forty characters. Another sentence.",
        );
        assert_eq!(
            out.iter().map(|c| c.pause_ms).collect::<Vec<_>>(),
            vec![280, 450]
        );
        let source = format!(
            "# Start\n\n{}end, {}ends.",
            "word ".repeat(39),
            "word ".repeat(39)
        );
        assert_eq!(
            source.strip_prefix("# Start\n\n").unwrap().chars().count(),
            400
        );
        let out = chunks(&source);
        assert_eq!(out.len(), 3);
        assert!(out[1].text.ends_with(','));
        assert_eq!(out[1].pause_ms, 120);
        assert_eq!(out[2].pause_ms, 450);
        assert_eq!(chunks("- List item.")[0].pause_ms, 300);
        assert_eq!(chunks(&"word ".repeat(30))[0].pause_ms, 0);
        let quoted =
            chunks(r#""A quoted sentence with enough characters ends here." More follows."#);
        assert_eq!(quoted[0].pause_ms, 280);
    }
    #[test]
    fn hostile_html_scans_each_suffix_at_most_once() {
        let source = format!("<div>\n{}\n", "<".repeat(200_000));
        let mut cursor = TextCursor::streaming(&source, false).unwrap();
        assert!(cursor.next_chunk().unwrap().is_some());
        assert!(
            cursor.html_scan_bytes <= source.len(),
            "HTML scan inspected {} bytes for {} input bytes",
            cursor.html_scan_bytes,
            source.len()
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
#[cfg(test)]
mod technical {
    use super::*;
    use crate::vocabulary::speak_technical;

    const OWNER_LINE: &str = "Cached files go here: tests/lib.sh has a cache_dir helper that returns notes/.cache/demo/.tests.";

    fn chunks_with(
        source: &str,
        vocabulary: Option<Arc<Vocabulary>>,
        options: TechnicalOptions,
    ) -> Vec<TextChunk> {
        let mut cursor = TextCursor::streaming_with(source, false, vocabulary, options).unwrap();
        std::iter::from_fn(|| cursor.next_chunk().unwrap()).collect()
    }
    fn technical_chunks(source: &str, options: TechnicalOptions) -> Vec<TextChunk> {
        chunks_with(source, Some(Arc::new(Vocabulary::builtin())), options)
    }
    fn joined(chunks: &[TextChunk]) -> String {
        chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn technical_text(source: &str) -> String {
        joined(&technical_chunks(source, TechnicalOptions::default()))
    }
    fn today(source: &str) -> Vec<TextChunk> {
        let mut cursor = TextCursor::streaming(source, false).unwrap();
        std::iter::from_fn(|| cursor.next_chunk().unwrap()).collect()
    }
    fn utf16(source: &str, byte: usize) -> u32 {
        source[..byte].encode_utf16().count() as u32
    }
    fn short_paths() -> TechnicalOptions {
        TechnicalOptions {
            full_paths: false,
            full_links: false,
        }
    }

    #[test]
    fn technical_paragraph_reads_owner_examples() {
        assert_eq!(
            technical_text(OWNER_LINE),
            "Cached files go here: tests slash lib dot S H has a cache underscore dir helper that returns notes slash dot cache slash demo slash dot tests."
        );
        assert_eq!(
            technical_text("Run /review with --dry-run on every .sh file."),
            "Run slash review with dash dash dry dash run on every dot S H file."
        );
    }

    #[test]
    fn technical_off_is_unchanged() {
        let off = chunks_with(OWNER_LINE, None, TechnicalOptions::default());
        assert_eq!(off, today(OWNER_LINE));
        assert_eq!(
            joined(&off),
            "Cached files go here: lib.sh has a cache_dir helper that returns .tests."
        );
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text");
        let cases: std::collections::BTreeMap<String, Vec<String>> =
            serde_json::from_str(include_str!("../tests/fixtures/text/goldens.json")).unwrap();
        for name in cases.keys() {
            let source = std::fs::read_to_string(fixtures.join(name)).unwrap();
            let expected = today(&source);
            for full_paths in [false, true] {
                for full_links in [false, true] {
                    let options = TechnicalOptions {
                        full_paths,
                        full_links,
                    };
                    assert_eq!(chunks_with(&source, None, options), expected, "{name}");
                }
            }
        }
    }

    #[test]
    fn technical_on_keeps_prose_as_today() {
        for source in [
            "Read me & you, then AT&amp;T and foo*bar.",
            "<div>Read me &amp; you</div>\n\nEnd.",
            "Then the deadline is October 12, 2026, at 18:45 today. Read it; remember it.",
            "For example e.g. the U.S. rate is 3.14 at 10:59, done.",
            "Flags 🇺🇸 keycap 1️⃣ skin 👍🏽 family 👨‍👩‍👧‍👦 done.",
            "Naïve first sentence is long enough to pass forty chars. 😀😀 Ünïcödé second sentence follows here.",
        ] {
            assert_eq!(
                technical_chunks(source, TechnicalOptions::default()),
                today(source),
                "{source}"
            );
        }
    }

    #[test]
    fn technical_highlight_covers_source_token() {
        let source =
            "Café notes 😀 here.\n\n**tests/lib.sh**\n\n(/review),\n\nNaïve `fixture_base` text.";
        let out = technical_chunks(source, TechnicalOptions::default());
        let chunk = |text: &str| out.iter().find(|chunk| chunk.text == text).unwrap();
        let path = chunk("tests slash lib dot S H.");
        let start = source.find("tests/lib.sh").unwrap();
        assert_eq!(
            (path.source_start, path.source_end),
            (
                utf16(source, start),
                utf16(source, start + "tests/lib.sh".len())
            )
        );
        let command = chunk("slash review,");
        let start = source.find("(/review),").unwrap();
        assert_eq!(
            (command.source_start, command.source_end),
            (
                utf16(source, start),
                utf16(source, start + "(/review),".len())
            )
        );
        let code = chunk("Naïve fixture underscore base text.");
        let start = source.find("Naïve").unwrap();
        assert_eq!(
            (code.source_start, code.source_end),
            (utf16(source, start), utf16(source, source.len()))
        );
    }

    #[test]
    fn long_technical_paragraph_chunks_within_limit() {
        let paths: Vec<String> = (0..10)
            .map(|n| format!("src/mod_{n}/lib.rs:{}", 100 + n))
            .collect();
        let source = format!("Paths: {}.", paths.join(" "));
        let out = technical_chunks(&source, TechnicalOptions::default());
        let vocabulary = Vocabulary::builtin();
        let mut tokens: Vec<String> = vec!["Paths:".into()];
        for (index, path) in paths.iter().enumerate() {
            let token = if index == paths.len() - 1 {
                format!("{path}.")
            } else {
                path.clone()
            };
            let spoken = speak_technical(&token, &vocabulary, &TechnicalOptions::default());
            assert!(
                spoken.chars().count() < 70 && spoken.chars().count() > 40,
                "{spoken}"
            );
            tokens.push(spoken);
        }
        assert_eq!(joined(&out), tokens.join(" "));
        assert!(out.len() >= 5, "{} chunks", out.len());
        let source_tokens: Vec<&str> = source.split(' ').collect();
        let mut next_token = 0;
        let mut previous_end = 0;
        for chunk in &out {
            assert!(
                chunk.text.chars().count() <= STREAMING_CHUNK_CHARS,
                "{:?} exceeds the limit",
                chunk.text
            );
            let mut rest = chunk.text.as_str();
            let first = next_token;
            for spoken in &tokens[next_token..] {
                match rest.strip_prefix(spoken.as_str()) {
                    Some(after) => {
                        rest = after.trim_start();
                        next_token += 1;
                    }
                    None => break,
                }
            }
            assert!(rest.is_empty(), "{:?} splits a spoken token", chunk.text);
            assert!(chunk.source_start >= previous_end);
            previous_end = chunk.source_end;
            let covered = &source[chunk.source_start as usize..chunk.source_end as usize];
            assert_eq!(covered, source_tokens[first..next_token].join(" "));
        }
        assert_eq!(next_token, tokens.len());
    }

    #[test]
    fn oversized_technical_token_splits_between_spoken_words() {
        let token = "/home/user/projects/app/src/modules/library/crates/handy-tts-core/src/deeply/nested/module_folder/text.rs:118";
        let source = format!("See {token} now.");
        let spoken = speak_technical(token, &Vocabulary::builtin(), &TechnicalOptions::default());
        assert!(spoken.chars().count() > 2 * STREAMING_CHUNK_CHARS);
        let out = technical_chunks(&source, TechnicalOptions::default());
        assert_eq!(joined(&out), format!("See {spoken} now."));
        let token_start = source.find(token).unwrap() as u32;
        let token_end = token_start + token.len() as u32;
        let parts: Vec<&TextChunk> = out
            .iter()
            .filter(|chunk| chunk.source_start >= token_start && chunk.source_start < token_end)
            .collect();
        assert!(parts.len() >= 3, "{} parts", parts.len());
        assert_eq!(parts[0].source_start, token_start);
        for pair in parts.windows(2) {
            assert_eq!(pair[0].source_end, pair[1].source_start);
            let before = source[..pair[0].source_end as usize].chars().next_back();
            assert!(
                before.is_some_and(|c| !c.is_alphanumeric()),
                "a part ends inside a source segment at {}",
                pair[0].source_end
            );
        }
        for chunk in &out {
            assert!(chunk.text.chars().count() <= STREAMING_CHUNK_CHARS);
            for word in chunk.text.trim_end_matches('.').split(' ') {
                assert!(
                    spoken.split(' ').any(|spoken_word| spoken_word == word)
                        || ["See", "now"].contains(&word),
                    "{word:?} is not a whole spoken word"
                );
            }
        }
    }

    #[test]
    fn inline_code_span_is_technical() {
        assert_eq!(
            technical_text("Call `fixture_base` and `lib.sh` now."),
            "Call fixture underscore base and lib dot S H now."
        );
        assert_eq!(
            technical_text("```\nfixture_base\n```"),
            "code block skipped."
        );
        let mut cursor = TextCursor::streaming_with(
            "```\nrun fixture_base\n```",
            true,
            Some(Arc::new(Vocabulary::builtin())),
            TechnicalOptions::default(),
        )
        .unwrap();
        assert_eq!(
            cursor.next_chunk().unwrap().unwrap().text,
            "run fixture underscore base."
        );
    }

    #[test]
    fn short_paths_read_the_last_segment() {
        let source = "Edit tests/lib.sh and src/tts/manager.rs:684 today.";
        assert_eq!(
            joined(&technical_chunks(source, short_paths())),
            "Edit lib dot S H and manager dot R S at line 684 today."
        );
        assert_eq!(
            technical_text(source),
            "Edit tests slash lib dot S H and src slash tts slash manager dot R S at line 684 today."
        );
        let token = "src/tts/manager.rs:684";
        let alone = technical_chunks(token, short_paths());
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].text, "manager dot R S at line 684.");
        assert_eq!(
            (alone[0].source_start, alone[0].source_end),
            (0, token.len() as u32)
        );
    }

    #[test]
    fn full_links_read_host_and_path() {
        let source = "See https://github.com/cjpais/Handy?tab=readme#top today.";
        let full_links = TechnicalOptions {
            full_paths: true,
            full_links: true,
        };
        assert_eq!(
            joined(&technical_chunks(source, full_links)),
            "See github dot com slash cjpais slash Handy today."
        );
        assert_eq!(technical_text(source), "See link to github.com today.");
    }
}
