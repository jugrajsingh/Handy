//! Technical reading: turns one paths, file name, flag or identifier token into the words an
//! engineer would say, driven by a vocabulary (built-in JSON plus an optional user file).
//!
//! The module is pure: it never touches the filesystem. `speak_technical` takes one
//! whitespace-free word, possibly wrapped in punctuation, and returns the spoken form with the
//! trailing sentence punctuation kept. A token that no rule applies to is returned unchanged,
//! so callers may pass every word through it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use thiserror::Error;

const BUILTIN_JSON: &str = include_str!("../../../src-tauri/resources/tts/vocabulary.json");
const SCHEMA_VERSION: u32 = 1;
const SENTENCE_PUNCTUATION: [char; 6] = ['.', ',', ';', ':', '!', '?'];
const WRAPPERS: [char; 9] = ['(', ')', '[', ']', '{', '}', '\'', '"', '`'];
const CLOSERS: [char; 5] = [')', ']', '}', '\'', '"'];

static LINE_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?):([0-9]+)(?::([0-9]+))?$").expect("the line suffix pattern is valid")
});
static VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bv([0-9]+\.[0-9]+\.[0-9]+)").expect("the version pattern is valid")
});
static BUILTIN: LazyLock<Vocabulary> = LazyLock::new(|| {
    let file = Vocabulary::parse(BUILTIN_JSON)
        .expect("the embedded vocabulary is valid; every_builtin_example_passes guards it");
    Vocabulary::from_file(&file)
});

/// An error in a vocabulary file; the app shows one notice per distinct value.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VocabularyError {
    #[error("vocabulary file is not valid at line {line} column {column}: {message}")]
    Json {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("vocabulary version {0} is not supported (expected 1)")]
    Version(u32),
    #[error("vocabulary term {key:?} has an invalid pattern: {message}")]
    Pattern { key: String, message: String },
}

/// How a `terms` entry is matched against one path segment or word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TermMatch {
    #[default]
    Whole,
    Anywhere,
    Prefix,
    Suffix,
    Regex,
}

/// One `terms` entry. Build it with `Term::new` so the pattern is compiled once, up front.
#[derive(Debug, Clone)]
pub struct Term {
    pub say: String,
    pub matching: TermMatch,
    pub case: bool,
    pattern: Regex,
}

impl PartialEq for Term {
    fn eq(&self, other: &Self) -> bool {
        self.say == other.say
            && self.matching == other.matching
            && self.case == other.case
            && self.pattern.as_str() == other.pattern.as_str()
    }
}

impl Term {
    /// Compiles the matcher for `key`; for `TermMatch::Regex` the key is the pattern itself.
    pub fn new(
        key: &str,
        say: impl Into<String>,
        matching: TermMatch,
        case: bool,
    ) -> Result<Term, VocabularyError> {
        let pattern_error = |message: &str| VocabularyError::Pattern {
            key: key.to_string(),
            message: message.to_string(),
        };
        if key.is_empty() {
            return Err(pattern_error("the key is empty"));
        }
        let escaped = regex::escape(key);
        let source = match matching {
            TermMatch::Whole => format!("^(?:{escaped})$"),
            TermMatch::Prefix => format!("^(?:{escaped})"),
            TermMatch::Suffix => format!("(?:{escaped})$"),
            TermMatch::Anywhere => escaped,
            TermMatch::Regex => key.to_string(),
        };
        let pattern = RegexBuilder::new(&source)
            .case_insensitive(!case)
            .build()
            .map_err(|error| pattern_error(&error.to_string()))?;
        Ok(Term {
            say: say.into(),
            matching,
            case,
            pattern,
        })
    }
}

/// One checked example: the exact input and the spoken text it must produce.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub text: String,
    pub spoken: String,
}

/// A parsed and validated vocabulary file (version 1); every section is optional.
#[derive(Debug, Clone, PartialEq)]
pub struct VocabularyFile {
    symbols: BTreeMap<String, String>,
    extensions: BTreeMap<String, String>,
    terms: Vec<(String, Term)>,
    acronyms: Vec<String>,
    examples: Vec<Example>,
}

/// The vocabulary the rules run against: built-in entries plus the user's, user winning.
#[derive(Debug, Clone, PartialEq)]
pub struct Vocabulary {
    symbols: BTreeMap<String, String>,
    extensions: BTreeMap<String, String>,
    terms: Vec<(String, Term)>,
    acronyms: BTreeSet<String>,
    examples: Vec<Example>,
}

#[derive(Deserialize)]
struct Probe {
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[expect(
        dead_code,
        reason = "the version was already checked by Probe; deny_unknown_fields needs the field"
    )]
    version: u32,
    #[serde(default)]
    symbols: BTreeMap<String, String>,
    #[serde(default)]
    extensions: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "ordered_terms")]
    terms: Vec<(String, RawTerm)>,
    #[serde(default)]
    acronyms: Vec<String>,
    #[serde(default)]
    examples: Vec<Example>,
}

enum RawTerm {
    Plain(String),
    Full(RawFullTerm),
}

fn default_case() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFullTerm {
    say: String,
    #[serde(rename = "match", default)]
    matching: TermMatch,
    #[serde(default = "default_case")]
    case: bool,
}

impl<'de> Deserialize<'de> for RawTerm {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TermVisitor;
        impl<'de> Visitor<'de> for TermVisitor {
            type Value = RawTerm;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or an object with say, match and case")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<RawTerm, E> {
                Ok(RawTerm::Plain(value.to_string()))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<RawTerm, A::Error> {
                RawFullTerm::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(RawTerm::Full)
            }
        }
        deserializer.deserialize_any(TermVisitor)
    }
}

/// Reads a JSON object as an ordered list so the file order of `terms` survives.
fn ordered_terms<'de, D: de::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(String, RawTerm)>, D::Error> {
    struct OrderedVisitor;
    impl<'de> Visitor<'de> for OrderedVisitor {
        type Value = Vec<(String, RawTerm)>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object of terms")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries: Vec<(String, RawTerm)> = Vec::new();
            while let Some((key, value)) = map.next_entry::<String, RawTerm>()? {
                match entries.iter_mut().find(|entry| entry.0 == key) {
                    Some(existing) => existing.1 = value,
                    None => entries.push((key, value)),
                }
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_map(OrderedVisitor)
}

fn json_error(error: &serde_json::Error) -> VocabularyError {
    let text = error.to_string();
    let message = text
        .rsplit_once(" at line ")
        .map_or(text.as_str(), |(head, _)| head)
        .to_string();
    VocabularyError::Json {
        line: error.line(),
        column: error.column(),
        message,
    }
}

impl Vocabulary {
    /// The vocabulary compiled into the crate.
    pub fn builtin() -> Vocabulary {
        BUILTIN.clone()
    }

    /// Parses and validates a vocabulary file; it is not merged with anything.
    pub fn parse(json: &str) -> Result<VocabularyFile, VocabularyError> {
        let probe: Probe = serde_json::from_str(json).map_err(|error| json_error(&error))?;
        if probe.version != SCHEMA_VERSION {
            return Err(VocabularyError::Version(probe.version));
        }
        let raw: RawFile = serde_json::from_str(json).map_err(|error| json_error(&error))?;
        let mut terms = Vec::with_capacity(raw.terms.len());
        for (key, value) in raw.terms {
            let term = match value {
                RawTerm::Plain(say) => Term::new(&key, say, TermMatch::Whole, true)?,
                RawTerm::Full(full) => Term::new(&key, full.say, full.matching, full.case)?,
            };
            terms.push((key, term));
        }
        Ok(VocabularyFile {
            symbols: raw.symbols,
            extensions: raw
                .extensions
                .into_iter()
                .map(|(key, spoken)| (key.to_lowercase(), spoken))
                .collect(),
            terms,
            acronyms: raw.acronyms,
            examples: raw.examples,
        })
    }

    /// The built-in vocabulary with the user's entries on top: a user entry replaces a
    /// built-in one with the same key, user terms apply first, acronyms and examples are unioned.
    pub fn merged(user: &VocabularyFile) -> Vocabulary {
        let mut merged = BUILTIN.clone();
        merged.symbols.extend(user.symbols.clone());
        merged.extensions.extend(user.extensions.clone());
        let mut terms = user.terms.clone();
        terms.extend(
            merged
                .terms
                .drain(..)
                .filter(|(key, _)| !user.terms.iter().any(|(user_key, _)| user_key == key)),
        );
        merged.terms = terms;
        merged.acronyms.extend(user.acronyms.iter().cloned());
        for example in &user.examples {
            if !merged.examples.contains(example) {
                merged.examples.push(example.clone());
            }
        }
        merged
    }

    /// The examples every check runs, built-in first.
    pub fn examples(&self) -> &[Example] {
        &self.examples
    }

    fn from_file(file: &VocabularyFile) -> Vocabulary {
        Vocabulary {
            symbols: file.symbols.clone(),
            extensions: file.extensions.clone(),
            terms: file.terms.clone(),
            acronyms: file.acronyms.iter().cloned().collect(),
            examples: file.examples.clone(),
        }
    }

    fn symbol(&self, symbol: &str) -> Option<&str> {
        self.symbols.get(symbol).map(String::as_str)
    }

    fn whole_term(&self, text: &str) -> Option<&str> {
        self.terms
            .iter()
            .find(|(_, term)| term.matching == TermMatch::Whole && term.pattern.is_match(text))
            .map(|(_, term)| term.say.as_str())
    }

    /// Applies `terms` to one segment: whole, then prefix and suffix, then anywhere, then regex,
    /// each in list order. Matched text becomes spoken and is never touched by later rules.
    fn apply_terms(&self, segment: &str) -> Vec<Piece> {
        if let Some(say) = self.whole_term(segment) {
            return vec![Piece::Spoken(say.to_string())];
        }
        let mut pieces = vec![Piece::Raw(segment.to_string())];
        for (_, term) in &self.terms {
            if matches!(term.matching, TermMatch::Prefix | TermMatch::Suffix) {
                apply_edge(&mut pieces, term);
            }
        }
        for (_, term) in &self.terms {
            if term.matching == TermMatch::Anywhere {
                pieces = rewrite_raw(pieces, |raw| split_matches(raw, term, false));
            }
        }
        for (_, term) in &self.terms {
            if term.matching == TermMatch::Regex {
                pieces = rewrite_raw(pieces, |raw| split_matches(raw, term, true));
            }
        }
        pieces
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Raw(String),
    Spoken(String),
}

fn apply_edge(pieces: &mut Vec<Piece>, term: &Term) {
    if term.matching == TermMatch::Prefix {
        let rest = match pieces.first() {
            Some(Piece::Raw(raw)) => term
                .pattern
                .find(raw)
                .map(|found| raw[found.end()..].to_string()),
            _ => None,
        };
        if let Some(rest) = rest {
            pieces.remove(0);
            if !rest.is_empty() {
                pieces.insert(0, Piece::Raw(rest));
            }
            pieces.insert(0, Piece::Spoken(term.say.clone()));
        }
    } else {
        let rest = match pieces.last() {
            Some(Piece::Raw(raw)) => term
                .pattern
                .find(raw)
                .map(|found| raw[..found.start()].to_string()),
            _ => None,
        };
        if let Some(rest) = rest {
            pieces.pop();
            if !rest.is_empty() {
                pieces.push(Piece::Raw(rest));
            }
            pieces.push(Piece::Spoken(term.say.clone()));
        }
    }
}

fn rewrite_raw(pieces: Vec<Piece>, rewrite: impl Fn(&str) -> Vec<Piece>) -> Vec<Piece> {
    pieces
        .into_iter()
        .flat_map(|piece| match piece {
            Piece::Raw(raw) => rewrite(&raw),
            spoken @ Piece::Spoken(_) => vec![spoken],
        })
        .collect()
}

fn split_matches(raw: &str, term: &Term, expand: bool) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut last = 0;
    for captures in term.pattern.captures_iter(raw) {
        let Some(found) = captures.get(0).filter(|found| !found.is_empty()) else {
            continue;
        };
        if found.start() > last {
            pieces.push(Piece::Raw(raw[last..found.start()].to_string()));
        }
        let mut say = String::new();
        if expand {
            captures.expand(&term.say, &mut say);
        } else {
            say.clone_from(&term.say);
        }
        pieces.push(Piece::Spoken(say));
        last = found.end();
    }
    if last < raw.len() {
        pieces.push(Piece::Raw(raw[last..].to_string()));
    }
    pieces
}

struct Parts<'a> {
    core: &'a str,
    ending: Option<char>,
}

/// Separates the wrapping punctuation: `core` is the token without wrappers and sentence
/// punctuation, `ending` is the sentence punctuation the token ended with (kept when speaking).
fn split_token(token: &str) -> Parts<'_> {
    let inner = token.trim_end_matches(|c| CLOSERS.contains(&c) || c == '`');
    let ending = inner
        .chars()
        .last()
        .filter(|c| SENTENCE_PUNCTUATION.contains(c));
    let unpunctuated = token.trim_end_matches(|c| {
        SENTENCE_PUNCTUATION.contains(&c) || CLOSERS.contains(&c) || c == '`'
    });
    let emphasis =
        unpunctuated.len() > 1 && unpunctuated.starts_with('*') && unpunctuated.ends_with('*');
    let wrapper = |c: char| WRAPPERS.contains(&c) || (emphasis && c == '*');
    let core = token.trim_matches(wrapper);
    let core = core.trim_end_matches(|c| SENTENCE_PUNCTUATION.contains(&c));
    let core = core.trim_matches(wrapper);
    Parts { core, ending }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Path,
    Flag,
    Hidden,
    FileName,
    Snake,
    Camel,
    Dotted,
    Call,
}

fn allowed(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~')
}

fn is_capitalized_word(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(char::is_uppercase)
        && text.chars().count() >= 2
        && chars.all(char::is_lowercase)
}

/// `end.Next`: a lowercase word glued to a capitalised one is a missing space, not a name.
fn is_run_on(first: &str, second: &str) -> bool {
    !first.is_empty() && first.chars().all(char::is_lowercase) && is_capitalized_word(second)
}

/// Splits `name.ext` when the final dot part is a file extension: in the vocabulary, or 2-5
/// alphanumerics starting with a letter. The name needs two characters unless the extension is
/// in the vocabulary, so `e.g`, `U.S`, `3.14` and `1.5x` never qualify.
fn file_extension<'a>(
    text: &'a str,
    vocabulary: Option<&Vocabulary>,
) -> Option<(&'a str, &'a str)> {
    let (name, ext) = text.rsplit_once('.')?;
    if ext.is_empty()
        || !ext.chars().all(char::is_alphanumeric)
        || name.ends_with('.')
        || name.contains("..")
    {
        return None;
    }
    let known = vocabulary.is_some_and(|v| v.extensions.contains_key(&ext.to_lowercase()));
    let plausible = (2..=5).contains(&ext.chars().count())
        && ext.chars().next().is_some_and(char::is_alphabetic);
    if !(known || plausible) || (name.chars().count() < 2 && !known) || is_run_on(name, ext) {
        return None;
    }
    Some((name, ext))
}

fn is_dotted(text: &str) -> bool {
    let segments: Vec<&str> = text.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            segment.chars().count() >= 2
                && segment
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_')
                && segment
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        })
        && !(segments.len() == 2 && is_run_on(segments[0], segments[1]))
}

fn is_flag(text: &str) -> bool {
    text.strip_prefix("--")
        .or_else(|| text.strip_prefix('-'))
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_alphabetic)
}

fn is_call(core: &str, open: usize) -> bool {
    let head = &core[..open];
    let args = &core[open + 1..];
    head.chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && head
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && args.chars().all(|c| allowed(c) || c == ',')
        && !matches!(args, "s" | "es")
}

/// Splits an identifier run on lower-to-upper boundaries and where an acronym run ends and a
/// word of at least two lowercase letters starts (`TTSManager`, but not the plural `GPUs`).
fn split_camel(run: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = run.char_indices().collect();
    let mut pieces = Vec::new();
    let mut start = 0;
    for index in 1..chars.len() {
        let (offset, current) = chars[index];
        let previous = chars[index - 1].1;
        let lower_to_upper = previous.is_lowercase() && current.is_uppercase();
        let acronym_end = previous.is_uppercase()
            && current.is_uppercase()
            && chars
                .get(index + 1)
                .is_some_and(|next| next.1.is_lowercase())
            && chars
                .get(index + 2)
                .is_some_and(|next| next.1.is_lowercase());
        if lower_to_upper || acronym_end {
            pieces.push(&run[start..offset]);
            start = offset;
        }
    }
    pieces.push(&run[start..]);
    pieces
}

fn has_camel_boundary(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|run| split_camel(run).len() > 1)
}

/// Decides which technical kind a token core is, if any. `core` has no wrapping punctuation
/// and no `:line` suffix. Kinds follow the spec table order.
fn classify(core: &str, vocabulary: Option<&Vocabulary>) -> Option<Kind> {
    if let Some(open) = core.find('(') {
        return is_call(core, open).then_some(Kind::Call);
    }
    let flag_value = if core.starts_with('-') {
        core.split_once('=')
    } else {
        None
    };
    let body = flag_value.map_or(core, |(flag, _)| flag);
    if flag_value.is_some_and(|(_, value)| !value.chars().all(allowed)) {
        return None;
    }
    if body.is_empty() || !body.chars().all(allowed) || !body.chars().any(char::is_alphanumeric) {
        return None;
    }
    if body.contains('~') && !body.starts_with("~/") {
        return None;
    }
    if body.contains('/') {
        let numeric = body.chars().all(|c| c.is_ascii_digit() || c == '/');
        return (!numeric).then_some(Kind::Path);
    }
    if body.contains("..") {
        return None;
    }
    if is_flag(body) {
        return Some(Kind::Flag);
    }
    if body
        .strip_prefix('.')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        return Some(Kind::Hidden);
    }
    if file_extension(body, vocabulary).is_some() {
        return Some(Kind::FileName);
    }
    if body.contains('_') {
        return Some(Kind::Snake);
    }
    if has_camel_boundary(body) {
        return Some(Kind::Camel);
    }
    is_dotted(body).then_some(Kind::Dotted)
}

/// Splits a `:684` or `:684:7` suffix off a path or file name. Anything else keeps its colon.
fn split_line<'a>(
    core: &'a str,
    vocabulary: Option<&Vocabulary>,
) -> (&'a str, Option<(&'a str, Option<&'a str>)>) {
    let Some(captures) = LINE_SUFFIX.captures(core) else {
        return (core, None);
    };
    let head = captures.get(1).map_or("", |m| m.as_str());
    if matches!(
        classify(head, vocabulary),
        Some(Kind::Path | Kind::FileName)
    ) {
        let line = captures.get(2).map_or("", |m| m.as_str());
        let column = captures.get(3).map(|m| m.as_str());
        return (head, Some((line, column)));
    }
    (core, None)
}

/// True when `token` is a path, file name, flag, slash command, call or identifier that the
/// rules read in full. Prose, numbers, links and versions are not technical.
pub fn is_technical(token: &str) -> bool {
    let parts = split_token(token);
    if parts.core.is_empty() {
        return false;
    }
    let (head, _) = split_line(parts.core, None);
    classify(head, None).is_some()
}

fn spell(text: &str) -> String {
    text.chars()
        .map(|c| c.to_uppercase().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

fn speak_extension(ext: &str, vocabulary: &Vocabulary) -> String {
    if let Some(spoken) = vocabulary.extensions.get(&ext.to_lowercase()) {
        return spoken.clone();
    }
    if ext.chars().count() <= 3 && ext.chars().all(char::is_alphabetic) {
        return spell(ext);
    }
    ext.to_string()
}

fn push_word(words: &mut Vec<String>, word: &str) {
    if !word.is_empty() {
        words.push(word.to_string());
    }
}

/// Speaks one alphanumeric run: acronyms are spelled, and a run that was split on camel
/// boundaries has its capitalised pieces lowercased (`fooBar` is "foo bar").
fn speak_run(run: &str, vocabulary: &Vocabulary, words: &mut Vec<String>) {
    let pieces = split_camel(run);
    let split = pieces.len() > 1;
    for piece in pieces {
        if vocabulary.acronyms.contains(piece) {
            words.push(spell(piece));
        } else if split && !piece.chars().skip(1).any(char::is_uppercase) {
            words.push(piece.to_lowercase());
        } else {
            words.push(piece.to_string());
        }
    }
}

/// Speaks text without slashes: alphanumeric runs become words, every other character is its
/// symbol word (or silent when the vocabulary has none). A dot between digits stays a dot.
fn speak_atoms(text: &str, vocabulary: &Vocabulary, words: &mut Vec<String>) {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_alphanumeric() {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_alphanumeric()
                    || (chars[index] == '.'
                        && chars[index - 1].is_ascii_digit()
                        && chars.get(index + 1).is_some_and(char::is_ascii_digit)))
            {
                index += 1;
            }
            let run: String = chars[start..index].iter().collect();
            speak_run(&run, vocabulary, words);
        } else {
            if let Some(word) = vocabulary.symbol(&chars[index].to_string()) {
                push_word(words, word);
            }
            index += 1;
        }
    }
}

fn speak_raw(raw: &str, final_piece: bool, vocabulary: &Vocabulary, words: &mut Vec<String>) {
    let split = if final_piece {
        file_extension(raw, Some(vocabulary))
    } else {
        None
    };
    match split {
        Some((name, ext)) => {
            speak_atoms(name, vocabulary, words);
            if let Some(dot) = vocabulary.symbol(".") {
                push_word(words, dot);
            }
            push_word(words, &speak_extension(ext, vocabulary));
        }
        None => speak_atoms(raw, vocabulary, words),
    }
}

/// Speaks a path: segments split on `/`, each through the terms, then the identifier rules.
fn speak_path(text: &str, vocabulary: &Vocabulary, words: &mut Vec<String>) {
    if text.is_empty() {
        return;
    }
    if let Some(say) = vocabulary.whole_term(text) {
        push_word(words, say);
        return;
    }
    let segments: Vec<&str> = text.split('/').collect();
    let last = segments.len() - 1;
    for (index, segment) in segments.iter().enumerate() {
        if index > 0
            && let Some(slash) = vocabulary.symbol("/")
        {
            push_word(words, slash);
        }
        if segment.is_empty() {
            continue;
        }
        let pieces = vocabulary.apply_terms(segment);
        let count = pieces.len();
        for (position, piece) in pieces.into_iter().enumerate() {
            match piece {
                Piece::Spoken(spoken) => push_word(words, &spoken),
                Piece::Raw(raw) => {
                    speak_raw(
                        &raw,
                        index == last && position == count - 1,
                        vocabulary,
                        words,
                    );
                }
            }
        }
    }
}

/// Speaks a classified core: a call speaks its name then its arguments, a flag with `=value`
/// speaks the value after "equals", everything else is a path.
fn speak_core(core: &str, vocabulary: &Vocabulary) -> String {
    let mut words = Vec::new();
    if let Some(open) = core.find('(') {
        speak_path(&core[..open], vocabulary, &mut words);
        for argument in core[open + 1..].split(',') {
            speak_path(argument.trim(), vocabulary, &mut words);
        }
    } else if let Some((flag, value)) = core.split_once('=').filter(|_| core.starts_with('-')) {
        speak_path(flag, vocabulary, &mut words);
        if let Some(equals) = vocabulary.symbol("=") {
            push_word(&mut words, equals);
        }
        speak_path(value, vocabulary, &mut words);
    } else {
        speak_path(core, vocabulary, &mut words);
    }
    words.join(" ")
}

/// Speaks one token the way an engineer reads it aloud. Exact `terms` apply first, then links
/// and versions keep today's wording, then the kind rules; a token nothing applies to comes back
/// unchanged. Trailing sentence punctuation is kept, wrapping punctuation is dropped.
pub fn speak_technical(token: &str, vocabulary: &Vocabulary) -> String {
    let parts = split_token(token);
    if parts.core.is_empty() {
        return token.to_string();
    }
    let core = parts.core;
    let ending: String = parts.ending.map(String::from).unwrap_or_default();
    if let Some(say) = vocabulary.whole_term(core) {
        return format!("{say}{ending}");
    }
    if core.starts_with("https://") || core.starts_with("http://") {
        let host = core.split_once("://").map_or("", |(_, rest)| {
            rest.split(['/', '?', '#']).next().unwrap_or("")
        });
        return format!("link to {host}{ending}");
    }
    if !core.contains('/') && VERSION.is_match(token) {
        return VERSION.replace_all(token, "version $1").into_owned();
    }
    let (head, line) = split_line(core, Some(vocabulary));
    if classify(head, Some(vocabulary)).is_some() {
        let mut spoken = speak_core(head, vocabulary);
        if let Some((line, column)) = line {
            let _ = write!(spoken, " at line {line}");
            if let Some(column) = column {
                let _ = write!(spoken, " column {column}");
            }
        }
        spoken.push_str(&ending);
        return spoken;
    }
    let pieces = vocabulary.apply_terms(core);
    match pieces.as_slice() {
        [Piece::Raw(raw)] => {
            if vocabulary.acronyms.contains(raw.as_str()) {
                format!("{}{ending}", spell(raw))
            } else {
                token.to_string()
            }
        }
        _ => {
            let joined: String = pieces
                .into_iter()
                .map(|piece| match piece {
                    Piece::Raw(text) | Piece::Spoken(text) => text,
                })
                .collect();
            format!("{joined}{ending}")
        }
    }
}

/// One example run against a vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleResult {
    pub text: String,
    pub expected: String,
    pub actual: String,
}

/// Runs every example and returns all outcomes in order; an outcome passes when
/// `expected == actual`.
pub fn run_examples(vocabulary: &Vocabulary) -> Vec<ExampleResult> {
    vocabulary
        .examples
        .iter()
        .map(|example| ExampleResult {
            text: example.text.clone(),
            expected: example.spoken.clone(),
            actual: speak_technical(&example.text, vocabulary),
        })
        .collect()
}

/// The examples that do not produce their expected spoken text; empty means the check passes.
pub fn check_examples(vocabulary: &Vocabulary) -> Vec<ExampleResult> {
    run_examples(vocabulary)
        .into_iter()
        .filter(|result| result.expected != result.actual)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say(token: &str) -> String {
        speak_technical(token, &Vocabulary::builtin())
    }

    fn with_user(json: &str) -> Vocabulary {
        Vocabulary::merged(&Vocabulary::parse(json).expect("test vocabulary parses"))
    }

    mod table_rows {
        use super::*;

        #[test]
        fn path_is_spoken_in_full() {
            assert_eq!(say("tests/lib.sh"), "tests slash lib dot S H");
            assert_eq!(
                say("notes/.cache/demo/.tests"),
                "notes slash dot cache slash demo slash dot tests"
            );
            assert_eq!(say("/usr/bin/env"), "slash usr slash bin slash env");
            assert_eq!(say("../x/y"), "dot dot slash x slash y");
            assert!(is_technical("crates/handy-tts-core/src/text.rs"));
        }

        #[test]
        fn hidden_segment_is_dot_name() {
            assert_eq!(say(".cache"), "dot cache");
            assert_eq!(say(".gitignore"), "dot gitignore");
            assert_eq!(say("a/.b2"), "a slash dot b2");
        }

        #[test]
        fn home_is_spoken() {
            assert_eq!(say("~/x"), "home slash x");
            assert_eq!(say("~/notes.md"), "home slash notes dot markdown");
            assert!(!is_technical("~5"));
            assert!(!is_technical("~"));
        }

        #[test]
        fn file_name_spells_short_extension() {
            assert_eq!(say("lib.sh"), "lib dot S H");
            assert_eq!(say(".sh"), "dot S H");
            assert_eq!(say("README.md"), "README dot markdown");
            assert_eq!(say("data.json"), "data dot jason");
            assert_eq!(say("notes.txt"), "notes dot text");
            assert_eq!(say("page.PDF"), "page dot P D F");
            assert_eq!(say("main.rs"), "main dot R S");
            assert_eq!(say("config.lock"), "config dot lock");
            assert_eq!(say("fixture_base.sh"), "fixture underscore base dot S H");
            assert_eq!(say("archive.tar.gz"), "archive dot tar dot G Z");
        }

        #[test]
        fn flag_is_dash_dash() {
            assert_eq!(say("--dry-run"), "dash dash dry dash run");
            assert_eq!(say("-v"), "dash v");
            assert_eq!(say("--speed=1.5"), "dash dash speed equals 1.5");
            assert!(!is_technical("-1"));
            assert!(!is_technical("--"));
            assert!(!is_technical("---"));
        }

        #[test]
        fn slash_command_is_slash_word() {
            assert_eq!(say("/review"), "slash review");
            assert_eq!(say("/init"), "slash init");
            assert!(is_technical("/review"));
        }

        #[test]
        fn function_call_drops_parentheses() {
            assert_eq!(say("fixture_base()"), "fixture underscore base");
            assert_eq!(say("foo(barBaz)"), "foo bar baz");
            assert_eq!(say("fixture_base()."), "fixture underscore base.");
            assert!(!is_technical("item(s)"));
        }

        #[test]
        fn snake_case_says_underscore() {
            assert_eq!(say("fixture_base"), "fixture underscore base");
            assert_eq!(say("_private"), "underscore private");
            assert_eq!(
                say("__init__"),
                "underscore underscore init underscore underscore"
            );
        }

        #[test]
        fn camel_case_is_split() {
            assert_eq!(say("fooBar"), "foo bar");
            assert_eq!(say("PascalCase"), "pascal case");
            assert_eq!(say("getHTTPStatus"), "get H T T P status");
            assert!(!is_technical("Handy"));
            assert!(!is_technical("GPUs"));
        }

        #[test]
        fn dotted_name_says_dot() {
            assert_eq!(say("os.path"), "os dot path");
            assert_eq!(say("self.assertEqual"), "self dot assert equal");
            assert_eq!(say("os.environment"), "os dot environment");
            assert_eq!(say("a.long.chain"), "a dot long dot chain");
        }

        #[test]
        fn env_var_spells_acronyms() {
            assert_eq!(
                say("HANDY_TTS_WORKER"),
                "HANDY underscore T T S underscore WORKER"
            );
        }

        #[test]
        fn links_and_versions_are_unchanged() {
            assert_eq!(say("https://github.com/x/y"), "link to github.com");
            assert_eq!(say("https://github.com/x/y."), "link to github.com.");
            assert_eq!(say("(http://example.org?q=1)"), "link to example.org");
            assert_eq!(say("v1.2.3"), "version 1.2.3");
            assert_eq!(say("v0.10.0,"), "version 0.10.0,");
            assert!(!is_technical("https://github.com/x/y"));
            assert!(!is_technical("v1.2.3"));
        }
    }

    mod review_focus {
        use super::*;

        #[test]
        fn prose_tokens_are_not_technical() {
            for token in [
                "e.g.",
                "U.S.",
                "U.S.A.",
                "3.14",
                "10:59",
                "done.",
                "i.e.,",
                "1.5x",
                "192.168.1.1",
                "24/7",
                "10/11/2026",
                "wait...",
                "Dr.",
                "end.Next",
                "word(s)",
            ] {
                assert!(!is_technical(token), "{token} must not be technical");
                assert_eq!(say(token), token, "{token} must read as today");
            }
        }

        #[test]
        fn punctuation_around_tokens_is_kept() {
            assert_eq!(say("lib.sh,"), "lib dot S H,");
            assert_eq!(say("**tests/lib.sh**"), "tests slash lib dot S H");
            assert_eq!(say("**tests/lib.sh**,"), "tests slash lib dot S H,");
            assert_eq!(say("(/review)"), "slash review");
            assert_eq!(say("(/review)."), "slash review.");
            assert_eq!(say("\"lib.sh\"?"), "lib dot S H?");
            assert_eq!(say("`fixture_base`;"), "fixture underscore base;");
            assert_eq!(say("(manager.rs:684)!"), "manager dot R S at line 684!");
        }

        #[test]
        fn acronyms_inside_identifiers_are_spelled() {
            assert_eq!(say("parseHTTPRequest"), "parse H T T P request");
            assert_eq!(say("TTSManager"), "T T S manager");
            assert_eq!(say("GPU"), "G P U");
            assert_eq!(say("GPU,"), "G P U,");
            assert_eq!(say("Handy,"), "Handy,");
        }
    }

    mod terms {
        use super::*;

        #[test]
        fn terms_override_rules() {
            let builtin = Vocabulary::builtin();
            assert_eq!(speak_technical("fooBar", &builtin), "foo bar");
            let user = with_user(
                r#"{"version":1,"terms":{"fooBar":"FOO BAR TERM","lib.sh":"the library"}}"#,
            );
            assert_eq!(speak_technical("fooBar", &user), "FOO BAR TERM");
            assert_eq!(
                speak_technical("tests/lib.sh", &user),
                "tests slash the library"
            );
            assert_eq!(speak_technical("lib.sh", &user), "the library");
            assert_eq!(
                speak_technical("notes/fooBar/x", &user),
                "notes slash FOO BAR TERM slash x"
            );
            assert_eq!(speak_technical("kubectl", &user), "cube control");
        }

        #[test]
        fn term_match_types() {
            let whole = with_user(r#"{"version":1,"terms":{"fooBar":{"say":"ZED"}}}"#);
            assert_eq!(speak_technical("fooBar", &whole), "ZED");
            assert_ne!(speak_technical("fooBarput", &whole), "ZED");
            assert!(!speak_technical("fooBarput", &whole).contains("ZED"));

            let anywhere =
                with_user(r#"{"version":1,"terms":{"Bar":{"say":"ZED","match":"anywhere"}}}"#);
            assert_eq!(speak_technical("fooBarput", &anywhere), "foo ZED put");

            let prefix =
                with_user(r#"{"version":1,"terms":{"pre":{"say":"PRE","match":"prefix"}}}"#);
            assert_eq!(speak_technical("prefix_x", &prefix), "PRE fix underscore x");
            assert_eq!(speak_technical("xpre_y", &prefix), "xpre underscore y");

            let suffix =
                with_user(r#"{"version":1,"terms":{"ing":{"say":"ING","match":"suffix"}}}"#);
            assert_eq!(speak_technical("run_ing", &suffix), "run underscore ING");
            assert_eq!(speak_technical("ingot_x", &suffix), "ingot underscore x");

            let insensitive =
                with_user(r#"{"version":1,"terms":{"foobar":{"say":"ZED","case":false}}}"#);
            assert_eq!(speak_technical("FOOBAR", &insensitive), "ZED");
            let sensitive = with_user(r#"{"version":1,"terms":{"foobar":{"say":"ZED"}}}"#);
            assert_eq!(speak_technical("FOOBAR", &sensitive), "FOOBAR");

            let regex = with_user(
                r#"{"version":1,"terms":{"^v(\\d+)$":{"say":"version $1","match":"regex"}}}"#,
            );
            assert_eq!(speak_technical("v12", &regex), "version 12");
            assert_eq!(speak_technical("v12x", &regex), "v12x");
        }

        #[test]
        fn terms_apply_in_type_then_file_order() {
            let user = with_user(
                r#"{"version":1,"terms":{
                    "ab":{"say":"FIRST","match":"anywhere"},
                    "b":{"say":"SECOND","match":"anywhere"},
                    "x(.)":{"say":"R $1","match":"regex"}}}"#,
            );
            assert_eq!(
                speak_technical("zabz_xq", &user),
                "z FIRST z underscore R q"
            );
        }

        #[test]
        fn invalid_regex_term_is_a_file_error() {
            let error = Vocabulary::parse(
                r#"{"version":1,"terms":{"(unclosed":{"say":"x","match":"regex"}}}"#,
            )
            .expect_err("an invalid pattern is a file error");
            match error {
                VocabularyError::Pattern { key, .. } => assert_eq!(key, "(unclosed"),
                other => panic!("expected a pattern error, got {other:?}"),
            }
        }
    }

    mod files {
        use super::*;

        #[test]
        fn file_and_line_are_spoken() {
            assert_eq!(say("manager.rs:684"), "manager dot R S at line 684");
            assert_eq!(
                say("manager.rs:684:7"),
                "manager dot R S at line 684 column 7"
            );
            assert_eq!(
                say("crates/x/manager.rs:12,"),
                "crates slash x slash manager dot R S at line 12,"
            );
            assert!(is_technical("manager.rs:684"));
            assert_eq!(say("10:59"), "10:59");
            assert!(!is_technical("10:59"));
            assert!(!is_technical("localhost:8080"));
        }

        #[test]
        fn merge_unions_and_user_wins() {
            let user = with_user(
                r#"{"version":1,"extensions":{"ZZ":"zed"},"acronyms":["ZZZ"],
                    "examples":[{"text":"file.zz","spoken":"file dot zed"}]}"#,
            );
            assert_eq!(speak_technical("file.zz", &user), "file dot zed");
            assert_eq!(
                speak_technical("tests/lib.sh", &user),
                "tests slash lib dot S H"
            );
            assert_eq!(speak_technical("ZZZ", &user), "Z Z Z");
            assert_eq!(speak_technical("GPU", &user), "G P U");
            assert_eq!(
                user.examples().len(),
                Vocabulary::builtin().examples().len() + 1
            );
            assert!(check_examples(&user).is_empty());
        }

        #[test]
        fn a_colliding_entry_fails_the_examples_it_changes() {
            let user = with_user(
                r#"{"version":1,"symbols":{"-":"minus"},
                    "examples":[{"text":"a-b.sh","spoken":"a minus b dot S H"}]}"#,
            );
            assert_eq!(speak_technical("a-b.sh", &user), "a minus b dot S H");
            let failed: Vec<String> = check_examples(&user).into_iter().map(|r| r.text).collect();
            assert_eq!(
                failed,
                vec!["--dry-run".to_string(), "--speed=1.5".to_string()]
            );
        }

        #[test]
        fn parse_reports_json_version_and_unknown_keys() {
            match Vocabulary::parse("{\n  \"version\": 1,\n  \"terms\": [\n}") {
                Err(VocabularyError::Json { line, .. }) => assert_eq!(line, 4),
                other => panic!("expected a json error, got {other:?}"),
            }
            assert_eq!(
                Vocabulary::parse(r#"{"version":2}"#),
                Err(VocabularyError::Version(2))
            );
            assert!(matches!(
                Vocabulary::parse(r#"{"version":1,"surprise":true}"#),
                Err(VocabularyError::Json { .. })
            ));
            assert!(matches!(
                Vocabulary::parse(r#"{"version":1,"symbols":{"/":3}}"#),
                Err(VocabularyError::Json { .. })
            ));
            assert!(matches!(
                Vocabulary::parse(r#"{"version":1,"terms":{"a":{"say":"b","extra":1}}}"#),
                Err(VocabularyError::Json { .. })
            ));
            assert!(Vocabulary::parse(r#"{"version":1}"#).is_ok());
        }
    }

    mod checks {
        use super::*;

        #[test]
        fn every_builtin_example_passes() {
            let builtin = Vocabulary::builtin();
            assert!(builtin.examples().len() >= 19);
            assert_eq!(check_examples(&builtin), Vec::<ExampleResult>::new());
        }

        #[test]
        fn check_reports_a_wrong_example() {
            let user =
                with_user(r#"{"version":1,"examples":[{"text":"lib.sh","spoken":"lib shush"}]}"#);
            let failures = check_examples(&user);
            assert_eq!(
                failures,
                vec![ExampleResult {
                    text: "lib.sh".into(),
                    expected: "lib shush".into(),
                    actual: "lib dot S H".into(),
                }]
            );
        }

        #[test]
        fn technical_output_has_only_speakable_characters() {
            for token in [
                "tests/lib.sh",
                "~/a_b/.c-d",
                "--dry-run",
                "HANDY_TTS_WORKER",
                "parseHTTPRequest",
                "manager.rs:684:7",
                "foo(bar)",
            ] {
                let spoken = say(token);
                assert!(
                    spoken.chars().all(|c| c.is_alphanumeric() || c == ' '),
                    "{token} -> {spoken}"
                );
            }
        }
    }
}
