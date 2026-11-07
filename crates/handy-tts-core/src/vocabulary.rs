//! Technical reading: turns one paths, file name, flag or identifier token into the words an
//! engineer would say, driven by a vocabulary (built-in JSON plus an optional user file).
//!
//! The module is pure: it never touches the filesystem. `speak_technical` takes one
//! whitespace-free word, possibly wrapped in punctuation, and returns the spoken form with the
//! trailing sentence punctuation kept. A token that no rule applies to is returned unchanged,
//! so callers may pass every word through it.

use std::borrow::Cow;
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

/// Words for symbols the vocabulary has no entry for; any other symbol is silent.
const FALLBACK_SYMBOLS: [(char, &str); 7] = [
    ('@', "at"),
    ('+', "plus"),
    ('#', "hash"),
    ('&', "and"),
    ('=', "equals"),
    (':', "colon"),
    ('-', "dash"),
];

static BUILTIN: LazyLock<Vocabulary> = LazyLock::new(|| {
    // The embedded file is validated by `builtin_vocabulary_parses` and
    // `every_builtin_example_passes`; an unparsable one degrades to an empty vocabulary.
    Vocabulary::parse(BUILTIN_JSON)
        .map(|file| Vocabulary::from_file(&file))
        .unwrap_or_default()
});

/// Abbreviations that are prose even without a trailing period.
const ABBREVIATIONS: [&str; 10] = [
    "e.g", "i.e", "a.m", "p.m", "c.f", "n.b", "v.s", "a.k.a", "p.s", "ph.d",
];

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
    #[error("vocabulary entry {key:?} is invalid: {message}")]
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
    /// `say` may only hold letters, digits and spaces (plus `$1` style references for a regex).
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
        let say = say.into();
        validate_spoken(key, "term", &say, matching == TermMatch::Regex)?;
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
            say,
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
    /// The catalog model id this example is checked against; without one it is checked against
    /// the shared vocabulary and every model that has overrides.
    #[serde(default)]
    pub model: Option<String>,
}

/// The sections a per-model override can hold: the top-level sections except `examples`.
#[derive(Debug, Clone, PartialEq, Default)]
struct Sections {
    symbols: BTreeMap<String, String>,
    extensions: BTreeMap<String, String>,
    terms: Vec<(String, Term)>,
    acronyms: BTreeSet<String>,
}

/// Puts `over` in front of `base`: an entry of `over` replaces the `base` entry with the same
/// key and every `over` term applies before the `base` terms.
fn overlay_terms(base: &mut Vec<(String, Term)>, over: &[(String, Term)]) {
    let mut terms = over.to_vec();
    terms.extend(
        base.drain(..)
            .filter(|(key, _)| !over.iter().any(|(over_key, _)| over_key == key)),
    );
    *base = terms;
}

impl Sections {
    fn overlay(&mut self, over: &Sections) {
        self.symbols.extend(over.symbols.clone());
        self.extensions.extend(over.extensions.clone());
        overlay_terms(&mut self.terms, &over.terms);
        self.acronyms.extend(over.acronyms.iter().cloned());
    }
}

/// A parsed and validated vocabulary file (version 1); every section is optional.
#[derive(Debug, Clone, PartialEq)]
pub struct VocabularyFile {
    symbols: BTreeMap<String, String>,
    extensions: BTreeMap<String, String>,
    terms: Vec<(String, Term)>,
    acronyms: Vec<String>,
    examples: Vec<Example>,
    models: BTreeMap<String, Sections>,
}

/// The vocabulary the rules run against: built-in entries plus the user's, user winning.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Vocabulary {
    symbols: BTreeMap<String, String>,
    extensions: BTreeMap<String, String>,
    terms: Vec<(String, Term)>,
    acronyms: BTreeSet<String>,
    examples: Vec<Example>,
    /// Per-model overrides, already merged user over built-in; `for_model` applies one.
    models: BTreeMap<String, Sections>,
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
    #[serde(default)]
    models: BTreeMap<String, RawModel>,
}

/// One `models` entry: the top-level sections except `examples`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    #[serde(default)]
    symbols: BTreeMap<String, String>,
    #[serde(default)]
    extensions: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "ordered_terms")]
    terms: Vec<(String, RawTerm)>,
    #[serde(default)]
    acronyms: Vec<String>,
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

/// Removes `$1`, `$name` and `${name}` references from a regex replacement, leaving the literal
/// text; a `$` that starts no reference (such as `$$`) stays and is rejected as a literal.
fn without_references(text: &str) -> String {
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            literal.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('{') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == '}' {
                        closed = true;
                        break;
                    }
                    name.push(inner);
                }
                if !closed {
                    literal.push_str("${");
                    literal.push_str(&name);
                }
            }
            Some(next) if next.is_ascii_alphanumeric() || next == '_' => {
                while chars
                    .peek()
                    .is_some_and(|n| n.is_ascii_alphanumeric() || *n == '_')
                {
                    chars.next();
                }
            }
            _ => literal.push('$'),
        }
    }
    literal
}

/// Enforces the output contract on text a vocabulary entry says: letters, digits and spaces.
fn validate_spoken(
    key: &str,
    section: &str,
    spoken: &str,
    references: bool,
) -> Result<(), VocabularyError> {
    let literal = if references {
        without_references(spoken)
    } else {
        spoken.to_string()
    };
    match literal
        .chars()
        .find(|c| !(c.is_alphanumeric() || *c == ' '))
    {
        Some(bad) => Err(VocabularyError::Pattern {
            key: key.to_string(),
            message: format!(
                "the {section} says {spoken:?}, which contains {bad:?}; only letters, digits and spaces are allowed"
            ),
        }),
        None => Ok(()),
    }
}

/// The output contract, applied to every token whose spoken form differs from its source: each
/// remaining symbol becomes its word (vocabulary `symbols`, then `FALLBACK_SYMBOLS`) or is
/// dropped, and runs of spaces collapse. Only letters, digits, single spaces and a dot between
/// two digits (`1.5`, `version 1.2.3`) come out.
fn speakable(raw: &str, vocabulary: &Vocabulary) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut spaced = String::with_capacity(raw.len());
    for (index, &c) in chars.iter().enumerate() {
        let digit_dot = c == '.'
            && index > 0
            && chars[index - 1].is_ascii_digit()
            && chars.get(index + 1).is_some_and(char::is_ascii_digit);
        if c.is_alphanumeric() || digit_dot {
            spaced.push(c);
            continue;
        }
        spaced.push(' ');
        if let Some(word) = vocabulary.symbol_word(c) {
            spaced.push_str(word);
            spaced.push(' ');
        }
    }
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// Validates one set of sections (the top level or one model) and compiles its terms.
fn build_sections(
    symbols: BTreeMap<String, String>,
    extensions: BTreeMap<String, String>,
    raw_terms: Vec<(String, RawTerm)>,
    acronyms: Vec<String>,
) -> Result<Sections, VocabularyError> {
    for (key, spoken) in &symbols {
        validate_spoken(key, "symbol", spoken, false)?;
    }
    for (key, spoken) in &extensions {
        validate_spoken(key, "extension", spoken, false)?;
    }
    for acronym in &acronyms {
        if acronym.is_empty() || !acronym.chars().all(char::is_alphanumeric) {
            return Err(VocabularyError::Pattern {
                key: acronym.clone(),
                message: format!(
                    "the acronym {acronym:?} is spelled letter by letter, so it may only hold letters and digits"
                ),
            });
        }
    }
    let mut terms = Vec::with_capacity(raw_terms.len());
    for (key, value) in raw_terms {
        let term = match value {
            RawTerm::Plain(say) => Term::new(&key, say, TermMatch::Whole, true)?,
            RawTerm::Full(full) => Term::new(&key, full.say, full.matching, full.case)?,
        };
        terms.push((key, term));
    }
    Ok(Sections {
        symbols,
        extensions: extensions
            .into_iter()
            .map(|(key, spoken)| (key.to_lowercase(), spoken))
            .collect(),
        terms,
        acronyms: acronyms.into_iter().collect(),
    })
}

/// Names the model an entry error came from.
fn in_model(id: &str, error: VocabularyError) -> VocabularyError {
    match error {
        VocabularyError::Pattern { key, message } => VocabularyError::Pattern {
            key,
            message: format!("in models.{id}: {message}"),
        },
        other => other,
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
        let top = build_sections(raw.symbols, raw.extensions, raw.terms, raw.acronyms)?;
        let mut models = BTreeMap::new();
        for (id, model) in raw.models {
            let sections =
                build_sections(model.symbols, model.extensions, model.terms, model.acronyms)
                    .map_err(|error| in_model(&id, error))?;
            models.insert(id, sections);
        }
        Ok(VocabularyFile {
            symbols: top.symbols,
            extensions: top.extensions,
            terms: top.terms,
            acronyms: top.acronyms.into_iter().collect(),
            examples: raw.examples,
            models,
        })
    }

    /// The built-in vocabulary with the user's entries on top: a user entry replaces a
    /// built-in one with the same key, user terms apply first, acronyms and examples are unioned.
    pub fn merged(user: &VocabularyFile) -> Vocabulary {
        Self::merge_onto(BUILTIN.clone(), user)
    }

    /// `merged` over any base vocabulary: the user's top-level sections go over the base's, the
    /// user's `models.M` over the base's `models.M`.
    fn merge_onto(mut base: Vocabulary, user: &VocabularyFile) -> Vocabulary {
        base.apply(&Sections {
            symbols: user.symbols.clone(),
            extensions: user.extensions.clone(),
            terms: user.terms.clone(),
            acronyms: user.acronyms.iter().cloned().collect(),
        });
        for example in &user.examples {
            if !base.examples.contains(example) {
                base.examples.push(example.clone());
            }
        }
        for (id, sections) in &user.models {
            base.models.entry(id.clone()).or_default().overlay(sections);
        }
        base
    }

    /// Puts `over` on top of the top-level sections.
    fn apply(&mut self, over: &Sections) {
        self.symbols.extend(over.symbols.clone());
        self.extensions.extend(over.extensions.clone());
        overlay_terms(&mut self.terms, &over.terms);
        self.acronyms.extend(over.acronyms.iter().cloned());
    }

    /// The vocabulary for a read on `model_id`. The sections resolve in this order: the user's
    /// `models.<id>`, the built-in `models.<id>`, the user's top level, the built-in top level.
    /// A model without overrides resolves to the shared vocabulary. The result holds no model
    /// sections of its own, so resolving it again changes nothing.
    pub fn for_model(&self, model_id: &str) -> Vocabulary {
        let mut resolved = self.clone();
        if let Some(over) = self.models.get(model_id) {
            resolved.apply(over);
        }
        resolved.models.clear();
        resolved
    }

    /// True when the vocabulary has a `models` section for `model_id`.
    pub fn has_model(&self, model_id: &str) -> bool {
        self.models.contains_key(model_id)
    }

    /// The model ids that have overrides, in order.
    pub fn model_ids(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(String::as_str)
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
            models: file.models.clone(),
        }
    }

    /// The word for one symbol: the vocabulary's entry, else the built-in fallback.
    fn symbol_word(&self, symbol: char) -> Option<&str> {
        let mut key = [0_u8; 4];
        self.symbols
            .get(&*symbol.encode_utf8(&mut key))
            .map(String::as_str)
            .or_else(|| {
                FALLBACK_SYMBOLS
                    .iter()
                    .find(|(fallback, _)| *fallback == symbol)
                    .map(|(_, word)| *word)
            })
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

/// Splits `raw` around the term's matches. A regex expansion copies captured source text, which
/// may hold symbols; `speakable` turns them into words before the token is returned.
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
        let say = if expand {
            let mut expanded = String::new();
            captures.expand(&term.say, &mut expanded);
            expanded
        } else {
            term.say.clone()
        };
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
    /// The whole trailing run of sentence punctuation, including any inside quotes or emphasis.
    ending: String,
}

fn is_trailing(c: char, emphasis: bool) -> bool {
    CLOSERS.contains(&c) || c == '`' || SENTENCE_PUNCTUATION.contains(&c) || (emphasis && c == '*')
}

/// Separates the wrapping punctuation: `core` is the token without wrappers and sentence
/// punctuation, `ending` is every sentence-punctuation character the token ended with, in order
/// (kept when speaking). Emphasis stars are wrappers only when they open and close the token.
fn split_token(token: &str) -> Parts<'_> {
    let lead_len = token.len()
        - token
            .trim_start_matches(|c| WRAPPERS.contains(&c) || c == '*')
            .len();
    let lead_has_star = token[..lead_len].contains('*');
    let starred_end = token.trim_end_matches(|c| is_trailing(c, true)).len();
    let emphasis = lead_has_star && token[starred_end..].contains('*');
    let body_end = token.trim_end_matches(|c| is_trailing(c, emphasis)).len();
    let lead_len = if emphasis {
        lead_len
    } else {
        token.len() - token.trim_start_matches(WRAPPERS).len()
    };
    let core = token.get(lead_len..body_end).unwrap_or("");
    let ending = token[body_end..]
        .chars()
        .filter(|c| SENTENCE_PUNCTUATION.contains(c))
        .collect();
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

/// `e.g`, `U.S`, `p.m.`, `Ph.D.`: abbreviations stay prose. Single-letter segments that are all
/// capitals, a listed abbreviation, or a trailing period after a one-letter last segment.
fn is_abbreviation(core: &str, period_follows: bool) -> bool {
    if core.contains('/') || !core.contains('.') {
        return false;
    }
    if ABBREVIATIONS.contains(&core.to_lowercase().as_str()) {
        return true;
    }
    let single = |segment: &&str| {
        let mut chars = segment.chars();
        chars.next().is_some_and(char::is_alphabetic) && chars.next().is_none()
    };
    let segments: Vec<&str> = core.split('.').collect();
    let last_single = segments.last().is_some_and(single);
    let all_single = segments.iter().all(single);
    (all_single
        && core
            .chars()
            .filter(|c| c.is_alphabetic())
            .all(char::is_uppercase))
        || (period_follows && last_single)
}

/// Digits and dots starting with a digit (`3.14`, `192.168.1.1`), optionally a decimal
/// multiplier (`1.5x`) or a version (`v0.10.0`, `v1.2`): numbers, never file names.
fn is_number_or_version(text: &str) -> bool {
    let body = text
        .strip_prefix(['v', 'V'])
        .or_else(|| text.strip_suffix(['x', 'X']))
        .unwrap_or(text);
    body.starts_with(|c: char| c.is_ascii_digit())
        && body.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Splits `name.ext` when the final dot part is a file extension. A known extension (vocabulary
/// `extensions`, any case) always does, even where `end.Next` style prose would otherwise win;
/// an unknown one needs 1-5 ASCII alphanumerics, and a letter first when the name is only digits
/// (`2024.pdf`, `001.wav`, but not `1.5k`). Numbers, decimal multipliers, versions and IP
/// addresses are never file names.
fn file_extension<'a>(text: &'a str, vocabulary: &Vocabulary) -> Option<(&'a str, &'a str)> {
    let (name, ext) = text.rsplit_once('.')?;
    if ext.is_empty()
        || !ext.chars().all(char::is_alphanumeric)
        || name.ends_with('.')
        || name.contains("..")
        || is_number_or_version(text)
    {
        return None;
    }
    if vocabulary.extensions.contains_key(&ext.to_lowercase()) {
        return Some((name, ext));
    }
    let digit_name = name.chars().all(|c| c.is_ascii_digit() || c == '.');
    let unknown_ok = name.chars().any(char::is_alphanumeric)
        && (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && (!digit_name || ext.starts_with(|c: char| c.is_ascii_alphabetic()))
        && !is_run_on(name, ext);
    unknown_ok.then_some((name, ext))
}

fn is_dotted(text: &str) -> bool {
    let segments: Vec<&str> = text.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            segment
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
fn classify(core: &str, vocabulary: &Vocabulary, period_follows: bool) -> Option<Kind> {
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
    if is_abbreviation(body, period_follows) {
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

/// Splits `head:line[:column]` into its parts; the digits-only trailing parts are the suffix.
fn parse_line_suffix(core: &str) -> Option<(&str, &str, Option<&str>)> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let (rest, last) = core.rsplit_once(':')?;
    if !digits(last) {
        return None;
    }
    if let Some((head, line)) = rest.rsplit_once(':')
        && digits(line)
        && !head.is_empty()
    {
        return Some((head, line, Some(last)));
    }
    (!rest.is_empty()).then_some((rest, last, None))
}

/// Splits a `:684` or `:684:7` suffix off a path or file name. Anything else keeps its colon.
fn split_line<'a>(
    core: &'a str,
    vocabulary: &Vocabulary,
) -> (&'a str, Option<(&'a str, Option<&'a str>)>) {
    let Some((head, line, column)) = parse_line_suffix(core) else {
        return (core, None);
    };
    if matches!(
        classify(head, vocabulary, false),
        Some(Kind::Path | Kind::FileName)
    ) {
        return (head, Some((line, column)));
    }
    (core, None)
}

/// True when `token` is a path, file name, flag, slash command, call or identifier that the
/// rules read in full. Prose, numbers, links and versions are not technical. File extensions
/// are judged against the built-in vocabulary.
pub fn is_technical(token: &str) -> bool {
    let parts = split_token(token);
    if parts.core.is_empty() {
        return false;
    }
    let (head, line) = split_line(parts.core, &BUILTIN);
    classify(
        head,
        &BUILTIN,
        line.is_none() && parts.ending.starts_with('.'),
    )
    .is_some()
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
    if ext.chars().count() <= 3 && ext.chars().all(char::is_alphanumeric) {
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

/// Speaks text without slashes: alphanumeric runs become words and every other character is
/// left as its own word for `speakable` to name. A dot between digits stays inside its number.
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
            words.push(chars[index].to_string());
            index += 1;
        }
    }
}

fn speak_raw(raw: &str, final_piece: bool, vocabulary: &Vocabulary, words: &mut Vec<String>) {
    let split = if final_piece {
        file_extension(raw, vocabulary)
    } else {
        None
    };
    match split {
        Some((name, ext)) => {
            speak_atoms(name, vocabulary, words);
            words.push(".".to_string());
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
        if index > 0 {
            words.push("/".to_string());
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
        words.push("=".to_string());
        speak_path(value, vocabulary, &mut words);
    } else {
        speak_path(core, vocabulary, &mut words);
    }
    words.join(" ")
}

/// Byte length of `1.2.3` style digits at the start of `rest` (three dot-joined digit runs).
fn version_len(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut index = 0;
    for part in 0..3 {
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return None;
        }
        if part < 2 {
            if bytes.get(index) != Some(&b'.') {
                return None;
            }
            index += 1;
        }
    }
    Some(index)
}

/// Today's version rule: `v1.2.3` at a word boundary becomes `version 1.2.3`; None when the
/// token holds no version.
fn replace_versions(token: &str) -> Option<String> {
    let mut spoken = String::new();
    let mut copied = 0;
    let mut found = false;
    for (offset, c) in token.char_indices() {
        if c != 'v' || offset < copied {
            continue;
        }
        let boundary = token[..offset]
            .chars()
            .next_back()
            .is_none_or(|previous| !(previous.is_alphanumeric() || previous == '_'));
        if !boundary {
            continue;
        }
        if let Some(length) = version_len(&token[offset + 1..]) {
            spoken.push_str(&token[copied..offset]);
            spoken.push_str("version ");
            spoken.push_str(&token[offset + 1..offset + 1 + length]);
            copied = offset + 1 + length;
            found = true;
        }
    }
    if !found {
        return None;
    }
    spoken.push_str(&token[copied..]);
    Some(spoken)
}

/// How much of a path or link technical reading speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TechnicalOptions {
    /// Every path segment is read; off reads the last one (`tests/lib.sh` is "lib dot S H").
    pub full_paths: bool,
    /// A link's host and path are read; off keeps "link to <host>".
    pub full_links: bool,
}

impl Default for TechnicalOptions {
    /// Full paths, short links.
    fn default() -> Self {
        TechnicalOptions {
            full_paths: true,
            full_links: false,
        }
    }
}

/// The part of a path short paths read: the last segment, when the path has a directory part
/// and looks like a file system path (it starts with `/`, `./`, `../` or `~/`, has three or more
/// segments, or ends in a file name or hidden segment). A slash command (`/review`) and a word
/// pair such as `and/or` or `TCP/IP` keep their full reading.
fn short_path<'a>(path: &'a str, vocabulary: &Vocabulary) -> &'a str {
    let trimmed = path.trim_end_matches('/');
    let Some((directory, last)) = trimmed.rsplit_once('/') else {
        return path;
    };
    if directory.is_empty() || last.is_empty() {
        return path;
    }
    let rooted = ["/", "./", "../", "~/"]
        .iter()
        .any(|prefix| path.starts_with(prefix));
    let deep = trimmed.split('/').filter(|part| !part.is_empty()).count() >= 3;
    let file = last.starts_with('.') || file_extension(last, vocabulary).is_some();
    if rooted || deep || file { last } else { path }
}

/// Speaks a link without its scheme, query or fragment: the host through the terms and the
/// symbol words, then each path segment through the path rules.
fn speak_link(address: &str, vocabulary: &Vocabulary) -> String {
    let address = address.split(['?', '#']).next().unwrap_or("");
    let (host, path) = address.split_once('/').unwrap_or((address, ""));
    let mut words = Vec::new();
    for piece in vocabulary.apply_terms(host) {
        match piece {
            Piece::Spoken(spoken) => push_word(&mut words, &spoken),
            Piece::Raw(raw) => speak_atoms(&raw, vocabulary, &mut words),
        }
    }
    let path = path.trim_end_matches('/');
    if !path.is_empty() {
        words.push("/".to_string());
        speak_path(path, vocabulary, &mut words);
    }
    words.join(" ")
}

/// What one token becomes before the output contract is applied.
enum Spoken {
    /// No rule applies: the token reads exactly as it was written.
    Unchanged,
    /// A link keeps today's "link to <host>" wording, host as written.
    Link(String),
    /// Words that may still hold symbols; `speakable` finishes them.
    Words(String),
}

/// Applies the rules to a token core: exact `terms` first, then links and versions with today's
/// wording, then the kind rules, then partial terms and acronyms on any other word. `options`
/// chooses full or short paths and links.
fn speak_parts(
    core: &str,
    period_follows: bool,
    vocabulary: &Vocabulary,
    options: &TechnicalOptions,
) -> Spoken {
    if let Some(say) = vocabulary.whole_term(core) {
        return Spoken::Words(say.to_string());
    }
    if let Some(address) = core
        .strip_prefix("https://")
        .or_else(|| core.strip_prefix("http://"))
    {
        let host = address.split(['/', '?', '#']).next().unwrap_or("");
        if options.full_links && !host.is_empty() {
            return Spoken::Words(speak_link(address, vocabulary));
        }
        return Spoken::Link(host.to_string());
    }
    if !core.contains('/')
        && let Some(versioned) = replace_versions(core)
    {
        return Spoken::Words(versioned);
    }
    let (head, line) = split_line(core, vocabulary);
    if let Some(kind) = classify(head, vocabulary, line.is_none() && period_follows) {
        let head = if kind == Kind::Path && !options.full_paths {
            short_path(head, vocabulary)
        } else {
            head
        };
        let mut spoken = speak_core(head, vocabulary);
        if let Some((line, column)) = line {
            let _ = write!(spoken, " at line {line}");
            if let Some(column) = column {
                let _ = write!(spoken, " column {column}");
            }
        }
        return Spoken::Words(spoken);
    }
    let pieces = vocabulary.apply_terms(core);
    if let [Piece::Raw(raw)] = pieces.as_slice() {
        return if vocabulary.acronyms.contains(raw.as_str()) {
            Spoken::Words(spell(raw))
        } else {
            Spoken::Unchanged
        };
    }
    let mut words = Vec::new();
    for piece in pieces {
        match piece {
            Piece::Spoken(spoken) => push_word(&mut words, &spoken),
            Piece::Raw(raw) => speak_atoms(&raw, vocabulary, &mut words),
        }
    }
    Spoken::Words(words.join(" "))
}

/// Speaks one token the way an engineer reads it aloud; a token nothing applies to comes back
/// unchanged. Every other result passes through `speakable`, so it holds only letters, digits,
/// single spaces and digit dots, followed by the token's trailing sentence punctuation; the one
/// exemption is the host in "link to <host>". Wrapping punctuation is dropped. `options` chooses
/// full or short paths and links.
pub fn speak_technical(token: &str, vocabulary: &Vocabulary, options: &TechnicalOptions) -> String {
    let parts = split_token(token);
    if parts.core.is_empty() {
        return token.to_string();
    }
    let ending = parts.ending.as_str();
    match speak_parts(parts.core, ending.starts_with('.'), vocabulary, options) {
        Spoken::Unchanged => token.to_string(),
        Spoken::Link(host) => format!("link to {host}{ending}"),
        Spoken::Words(words) => format!("{}{ending}", speakable(&words, vocabulary)),
    }
}

/// One example run against a vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleResult {
    pub text: String,
    /// The model the example ran against; `None` is the shared vocabulary.
    pub model: Option<String>,
    pub expected: String,
    pub actual: String,
}

/// Runs every example and returns all outcomes in order; an outcome passes when
/// `expected == actual`. An example with a `model` runs against that model's resolved
/// vocabulary only; one without runs against the shared vocabulary and then each model that has
/// overrides, so a model entry that changes a shared example's reading fails it.
pub fn run_examples(vocabulary: &Vocabulary) -> Vec<ExampleResult> {
    let options = TechnicalOptions::default();
    let run = |example: &Example, model: Option<&str>| {
        let resolved = match model {
            Some(id) => Cow::Owned(vocabulary.for_model(id)),
            None => Cow::Borrowed(vocabulary),
        };
        ExampleResult {
            text: example.text.clone(),
            model: model.map(str::to_owned),
            expected: example.spoken.clone(),
            actual: speak_technical(&example.text, &resolved, &options),
        }
    };
    let mut results = Vec::new();
    for example in &vocabulary.examples {
        match example.model.as_deref() {
            Some(id) => results.push(run(example, Some(id))),
            None => {
                results.push(run(example, None));
                results.extend(vocabulary.model_ids().map(|id| run(example, Some(id))));
            }
        }
    }
    results
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
        speak_technical(token, &Vocabulary::builtin(), &TechnicalOptions::default())
    }

    fn with_user(json: &str) -> Vocabulary {
        Vocabulary::merged(&Vocabulary::parse(json).expect("test vocabulary parses"))
    }

    /// A vocabulary of only `json`: no built-in entries, so every entry is the test's own.
    fn bare(json: &str) -> Vocabulary {
        Vocabulary::from_file(&Vocabulary::parse(json).expect("test vocabulary parses"))
    }

    fn say_for(vocabulary: &Vocabulary, model: &str, token: &str) -> String {
        speak_technical(
            token,
            &vocabulary.for_model(model),
            &TechnicalOptions::default(),
        )
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
            assert_eq!(say("file.q"), "file dot Q");
            assert_eq!(say("backup.7z"), "backup dot 7 Z");
            assert_eq!(say("track.mp3"), "track dot M P 3");
            assert_eq!(say("2024.csv"), "2024 dot C S V");
            assert_eq!(say("2024.pdf"), "2024 dot P D F");
            assert_eq!(say("001.wav"), "001 dot W A V");
            assert_eq!(say("docs/2024.pdf"), "docs slash 2024 dot P D F");
            assert_eq!(say("file.q:12"), "file dot Q at line 12");
            assert_eq!(say("backup.7z:3:4"), "backup dot 7 Z at line 3 column 4");
            assert_eq!(say("lib.Sh"), "lib dot S H");
            assert_eq!(say("notes.Md"), "notes dot markdown");
            assert_eq!(say("dir/lib.Sh"), "dir slash lib dot S H");
            assert_eq!(say("archive.001"), "archive dot 0 0 1");
            assert_eq!(say("file.q:12."), "file dot Q at line 12.");
            assert!(is_technical("file.q"));
            assert!(is_technical("backup.7z"));
            assert!(is_technical("file.q:12"));
        }

        #[test]
        fn known_extension_in_any_case_is_a_file_name() {
            // Review r2 M1: a known extension wins over the `end.Next` run-on heuristic.
            assert_eq!(say("lib.Sh"), "lib dot S H");
            assert_eq!(say("notes.Md"), "notes dot markdown");
            assert_eq!(say("dir/lib.Sh"), "dir slash lib dot S H");
            assert_eq!(say("data.JSON"), "data dot jason");
            assert_eq!(say("lib.Sh:12,"), "lib dot S H at line 12,");
            assert_eq!(say("**notes.Md**."), "notes dot markdown.");
            assert!(is_technical("lib.Sh"));
            assert!(is_technical("notes.Md"));
            assert!(!is_technical("end.Next"));
            assert_eq!(say("end.Next"), "end.Next");
        }

        #[test]
        fn digit_only_name_with_a_qualifying_extension_is_a_file_name() {
            // Review r3 M1: a digit-only name keeps a letter-first extension.
            assert_eq!(say("2024.pdf"), "2024 dot P D F");
            assert_eq!(say("001.wav"), "001 dot W A V");
            assert_eq!(say("2024.pdf:12"), "2024 dot P D F at line 12");
            assert_eq!(say("docs/2024.pdf"), "docs slash 2024 dot P D F");
            assert_eq!(say("(2024.pdf),"), "2024 dot P D F,");
            assert_eq!(say("2024.xyz"), "2024 dot X Y Z");
            assert_eq!(say("10.c"), "10 dot C");
            assert!(is_technical("2024.pdf"));
            assert!(is_technical("001.wav:3:4"));
            for prose in [
                "3.14", "1.5x", "1.x", "v1.2", "10.0.0.1", "1.5k", "3.5mm", "2024.",
            ] {
                assert!(!is_technical(prose), "{prose} must not be technical");
                assert_eq!(say(prose), prose, "{prose} must read as today");
            }
            assert!(!is_technical("v0.10.0"));
        }

        #[test]
        fn numeric_extension_spells_its_digits() {
            // Review r2 M2: an alphanumeric extension needs no letter when the name has one.
            assert_eq!(say("archive.001"), "archive dot 0 0 1");
            assert_eq!(say("archive.001:12"), "archive dot 0 0 1 at line 12");
            assert_eq!(say("logs/archive.001,"), "logs slash archive dot 0 0 1,");
            assert_eq!(say("part.12345"), "part dot 12345");
            assert!(is_technical("archive.001"));
            for prose in [
                "3.14",
                "1.5x",
                "2.5X",
                "v0.10.0",
                "v1.2",
                "192.168.1.1",
                "0.10",
            ] {
                assert!(!is_technical(prose), "{prose} must not be technical");
            }
            assert_eq!(say("part.123456"), "part.123456");
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
            assert_eq!(say("a.b"), "a dot B");
            assert_eq!(say("x.y.z"), "x dot y dot Z");
            assert_eq!(say("a.b:5"), "a dot B at line 5");
            assert_eq!(say("a.b:5:2,"), "a dot B at line 5 column 2,");
            assert!(is_technical("a.b"));
            assert!(is_technical("a.b:5"));
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

    mod per_model {
        use super::*;

        const POCKET_SH: &str =
            r#"{"version":1,"models":{"pocket-en":{"extensions":{"sh":"ess aitch"}}}}"#;

        #[test]
        fn model_override_wins_for_that_model_only() {
            let vocabulary = with_user(POCKET_SH);
            assert_eq!(
                say_for(&vocabulary, "pocket-en", "tests/lib.sh"),
                "tests slash lib dot ess aitch"
            );
            assert_eq!(
                say_for(&vocabulary, "supertonic-3", "tests/lib.sh"),
                "tests slash lib dot S H"
            );
            assert_eq!(
                say_for(&vocabulary, "kokoro-82m-int8", ".sh"),
                "dot S H",
                "a model without overrides reads the shared vocabulary"
            );
            assert_eq!(
                speak_technical("lib.sh", &vocabulary, &TechnicalOptions::default()),
                "lib dot S H",
                "an unresolved vocabulary reads the shared sections"
            );
        }

        #[test]
        fn model_sections_cover_symbols_terms_and_acronyms() {
            let vocabulary = with_user(
                r#"{"version":1,"models":{"pocket-en":{
                    "symbols":{"/":"stroke"},
                    "terms":{"fooBar":"foo bahr"},
                    "acronyms":["XYZ"]}}}"#,
            );
            assert_eq!(say_for(&vocabulary, "pocket-en", "a/b"), "a stroke b");
            assert_eq!(say_for(&vocabulary, "pocket-en", "fooBar"), "foo bahr");
            assert_eq!(say_for(&vocabulary, "pocket-en", "XYZ"), "X Y Z");
            assert_eq!(say_for(&vocabulary, "supertonic-3", "a/b"), "a slash b");
            assert_eq!(say_for(&vocabulary, "supertonic-3", "fooBar"), "foo bar");
            assert_eq!(say_for(&vocabulary, "supertonic-3", "XYZ"), "XYZ");
        }

        #[test]
        fn resolution_order_user_model_then_builtin_model_then_user_then_builtin() {
            let builtin = |models: bool| {
                bare(&format!(
                    r#"{{"version":1,"symbols":{{".":"dot"}},"extensions":{{"sh":"top builtin"}}{}}}"#,
                    if models {
                        r#","models":{"m":{"extensions":{"sh":"model builtin"}}}"#
                    } else {
                        ""
                    }
                ))
            };
            let user = |top: bool, model: bool| {
                let top = if top {
                    r#","extensions":{"sh":"top user"}"#
                } else {
                    ""
                };
                let model = if model {
                    r#","models":{"m":{"extensions":{"sh":"model user"}}}"#
                } else {
                    ""
                };
                Vocabulary::parse(&format!(r#"{{"version":1{top}{model}}}"#))
                    .expect("test user file parses")
            };
            let read = |base: Vocabulary, file: &VocabularyFile| {
                say_for(&Vocabulary::merge_onto(base, file), "m", "a.sh")
            };
            assert_eq!(read(builtin(true), &user(true, true)), "a dot model user");
            assert_eq!(
                read(builtin(true), &user(true, false)),
                "a dot model builtin"
            );
            assert_eq!(read(builtin(false), &user(true, false)), "a dot top user");
            assert_eq!(
                read(builtin(false), &user(false, false)),
                "a dot top builtin"
            );
        }

        #[test]
        fn terms_resolve_in_the_same_order() {
            let base = bare(
                r#"{"version":1,"terms":{"foo":"top builtin","bar":"bar builtin"},
                    "models":{"m":{"terms":{"foo":"model builtin"}}}}"#,
            );
            let user = Vocabulary::parse(
                r#"{"version":1,"terms":{"foo":"top user"},"models":{"m":{"terms":{"bar":"bar user"}}}}"#,
            )
            .expect("test user file parses");
            let merged = Vocabulary::merge_onto(base, &user);
            assert_eq!(say_for(&merged, "m", "foo"), "model builtin");
            assert_eq!(say_for(&merged, "m", "bar"), "bar user");
            assert_eq!(say_for(&merged, "other", "foo"), "top user");
            assert_eq!(say_for(&merged, "other", "bar"), "bar builtin");
        }

        #[test]
        fn resolved_vocabulary_holds_no_model_sections() {
            let vocabulary = with_user(POCKET_SH);
            let resolved = vocabulary.for_model("pocket-en");
            assert_eq!(resolved.for_model("pocket-en"), resolved);
            assert!(!resolved.has_model("pocket-en"));
            assert!(vocabulary.has_model("pocket-en"));
            assert_eq!(vocabulary.model_ids().collect::<Vec<_>>(), ["pocket-en"]);
        }

        #[test]
        fn model_sections_are_validated_like_the_top_level() {
            let error = Vocabulary::parse(
                r#"{"version":1,"models":{"pocket-en":{"extensions":{"sh":"ess/aitch"}}}}"#,
            )
            .unwrap_err();
            match error {
                VocabularyError::Pattern { key, message } => {
                    assert_eq!(key, "sh");
                    assert!(message.contains("models.pocket-en"), "{message}");
                }
                other => panic!("expected a pattern error, got {other:?}"),
            }
            let invalid_regex =
                r#"{"version":1,"models":{"m":{"terms":{"(":{"say":"x","match":"regex"}}}}}"#;
            assert!(matches!(
                Vocabulary::parse(invalid_regex),
                Err(VocabularyError::Pattern { .. })
            ));
            for unknown in [
                r#"{"version":1,"models":{"m":{"examples":[]}}}"#,
                r#"{"version":1,"models":{"m":{"models":{}}}}"#,
                r#"{"version":1,"models":{"m":{"colour":{}}}}"#,
            ] {
                assert!(
                    matches!(
                        Vocabulary::parse(unknown),
                        Err(VocabularyError::Json { .. })
                    ),
                    "{unknown} must be a file error"
                );
            }
        }

        #[test]
        fn model_tagged_example_checks_only_its_model() {
            let vocabulary = bare(
                r#"{"version":1,"symbols":{".":"dot"},"extensions":{"sh":"S H"},
                    "models":{"pocket-en":{"extensions":{"sh":"ess aitch"}}},
                    "examples":[
                        {"text":"a.sh","spoken":"a dot ess aitch","model":"pocket-en"}]}"#,
            );
            let results = run_examples(&vocabulary);
            assert_eq!(results.len(), 1, "a tagged example runs for its model only");
            assert_eq!(results[0].model.as_deref(), Some("pocket-en"));
            assert!(check_examples(&vocabulary).is_empty());
            let wrong = bare(
                r#"{"version":1,"symbols":{".":"dot"},"extensions":{"sh":"S H"},
                    "models":{"pocket-en":{"extensions":{"sh":"ess aitch"}}},
                    "examples":[
                        {"text":"a.sh","spoken":"a dot ess aitch","model":"supertonic-3"}]}"#,
            );
            let failed = check_examples(&wrong);
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert_eq!(failed[0].model.as_deref(), Some("supertonic-3"));
            assert_eq!(failed[0].actual, "a dot S H");
        }

        #[test]
        fn untagged_example_is_checked_for_every_model_with_overrides() {
            let vocabulary = bare(
                r#"{"version":1,"symbols":{".":"dot"},"extensions":{"sh":"S H"},
                    "models":{"pocket-en":{"extensions":{"sh":"ess aitch"}}},
                    "examples":[{"text":"a.sh","spoken":"a dot S H"}]}"#,
            );
            let failed = check_examples(&vocabulary);
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert_eq!(failed[0].model.as_deref(), Some("pocket-en"));
            assert_eq!(run_examples(&vocabulary).len(), 2);
        }
    }

    mod review_focus {
        use super::*;

        #[test]
        fn prose_tokens_are_not_technical() {
            for token in [
                "e.g.",
                "e.g",
                "e.g.,",
                "p.m",
                "p.m.",
                "a.k.a",
                "Ph.D.",
                "U.S",
                "U.S.",
                "U.S.A.",
                "etc.)",
                "3.14",
                "0.10",
                "v1.2",
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
            assert_eq!(say("lib.sh..."), "lib dot S H...");
            assert_eq!(say("lib.sh?!"), "lib dot S H?!");
            assert_eq!(say("**lib.sh.**"), "lib dot S H.");
        }

        #[test]
        fn the_whole_trailing_punctuation_run_is_kept() {
            assert_eq!(say("lib.sh?!"), "lib dot S H?!");
            assert_eq!(say("**lib.sh.**"), "lib dot S H.");
            assert_eq!(say("\"lib.sh?\","), "lib dot S H?,");
            assert_eq!(say("(lib.sh)..."), "lib dot S H...");
            assert_eq!(say("`fixture_base`?!"), "fixture underscore base?!");
            assert_eq!(say("https://github.com/x/y..."), "link to github.com...");
            assert_eq!(say("GPU?!"), "G P U?!");
            assert_eq!(say("kubectl;;"), "cube control;;");
            assert!(is_technical("lib.sh..."));
            assert!(is_technical("**lib.sh.**"));
        }

        #[test]
        fn prose_with_punctuation_and_quotes_is_unchanged() {
            for token in ["don't", "well-known", "1,000", "\"done.\"", "*.rs", "foo*"] {
                assert_eq!(say(token), token);
            }
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
            assert_eq!(
                speak_technical("fooBar", &builtin, &TechnicalOptions::default()),
                "foo bar"
            );
            let user = with_user(
                r#"{"version":1,"terms":{"fooBar":"FOO BAR TERM","lib.sh":"the library"}}"#,
            );
            assert_eq!(
                speak_technical("fooBar", &user, &TechnicalOptions::default()),
                "FOO BAR TERM"
            );
            assert_eq!(
                speak_technical("tests/lib.sh", &user, &TechnicalOptions::default()),
                "tests slash the library"
            );
            assert_eq!(
                speak_technical("lib.sh", &user, &TechnicalOptions::default()),
                "the library"
            );
            assert_eq!(
                speak_technical("notes/fooBar/x", &user, &TechnicalOptions::default()),
                "notes slash FOO BAR TERM slash x"
            );
            assert_eq!(
                speak_technical("kubectl", &user, &TechnicalOptions::default()),
                "cube control"
            );
        }

        #[test]
        fn term_match_types() {
            let whole = with_user(r#"{"version":1,"terms":{"fooBar":{"say":"ZED"}}}"#);
            assert_eq!(
                speak_technical("fooBar", &whole, &TechnicalOptions::default()),
                "ZED"
            );
            assert_ne!(
                speak_technical("fooBarput", &whole, &TechnicalOptions::default()),
                "ZED"
            );
            assert!(
                !speak_technical("fooBarput", &whole, &TechnicalOptions::default()).contains("ZED")
            );

            let anywhere =
                with_user(r#"{"version":1,"terms":{"Bar":{"say":"ZED","match":"anywhere"}}}"#);
            assert_eq!(
                speak_technical("fooBarput", &anywhere, &TechnicalOptions::default()),
                "foo ZED put"
            );

            let prefix =
                with_user(r#"{"version":1,"terms":{"pre":{"say":"PRE","match":"prefix"}}}"#);
            assert_eq!(
                speak_technical("prefix_x", &prefix, &TechnicalOptions::default()),
                "PRE fix underscore x"
            );
            assert_eq!(
                speak_technical("xpre_y", &prefix, &TechnicalOptions::default()),
                "xpre underscore y"
            );

            let suffix =
                with_user(r#"{"version":1,"terms":{"ing":{"say":"ING","match":"suffix"}}}"#);
            assert_eq!(
                speak_technical("run_ing", &suffix, &TechnicalOptions::default()),
                "run underscore ING"
            );
            assert_eq!(
                speak_technical("ingot_x", &suffix, &TechnicalOptions::default()),
                "ingot underscore x"
            );

            let insensitive =
                with_user(r#"{"version":1,"terms":{"foobar":{"say":"ZED","case":false}}}"#);
            assert_eq!(
                speak_technical("FOOBAR", &insensitive, &TechnicalOptions::default()),
                "ZED"
            );
            let sensitive = with_user(r#"{"version":1,"terms":{"foobar":{"say":"ZED"}}}"#);
            assert_eq!(
                speak_technical("FOOBAR", &sensitive, &TechnicalOptions::default()),
                "FOOBAR"
            );

            let regex = with_user(
                r#"{"version":1,"terms":{"^v(\\d+)$":{"say":"version $1","match":"regex"}}}"#,
            );
            assert_eq!(
                speak_technical("v12", &regex, &TechnicalOptions::default()),
                "version 12"
            );
            assert_eq!(
                speak_technical("v12x", &regex, &TechnicalOptions::default()),
                "v12x"
            );
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
                speak_technical("zabz_xq", &user, &TechnicalOptions::default()),
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
            assert_eq!(say("archive.001:12"), "archive dot 0 0 1 at line 12");
            assert_eq!(say("2024.pdf:12"), "2024 dot P D F at line 12");
            assert_eq!(
                say("archive.001:12:3"),
                "archive dot 0 0 1 at line 12 column 3"
            );
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
            assert_eq!(
                speak_technical("file.zz", &user, &TechnicalOptions::default()),
                "file dot zed"
            );
            assert_eq!(
                speak_technical("tests/lib.sh", &user, &TechnicalOptions::default()),
                "tests slash lib dot S H"
            );
            assert_eq!(
                speak_technical("ZZZ", &user, &TechnicalOptions::default()),
                "Z Z Z"
            );
            assert_eq!(
                speak_technical("GPU", &user, &TechnicalOptions::default()),
                "G P U"
            );
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
            assert_eq!(
                speak_technical("a-b.sh", &user, &TechnicalOptions::default()),
                "a minus b dot S H"
            );
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
                    model: None,
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

        #[test]
        fn user_replacements_keep_the_output_contract() {
            let user = with_user(
                r#"{"version":1,
                    "symbols":{"/":"over","_":"under score"},
                    "extensions":{"zz":"zed zed"},
                    "terms":{"customtool":"custom tool",
                             "^id(.+)$":{"say":"id number $1","match":"regex"}}}"#,
            );
            for (token, expected) in [
                ("customtool", "custom tool"),
                ("a/b_c.zz", "a over b under score c dot zed zed"),
                ("id#5/x", "id number hash 5 over x"),
                ("id@7", "id number at 7"),
            ] {
                let spoken = speak_technical(token, &user, &TechnicalOptions::default());
                assert_eq!(spoken, expected);
                assert!(
                    spoken.chars().all(|c| c.is_alphanumeric() || c == ' '),
                    "{token} -> {spoken}"
                );
            }
            assert_eq!(
                speak_technical("customtool?!", &user, &TechnicalOptions::default()),
                "custom tool?!"
            );
        }

        #[test]
        fn partial_terms_leave_no_symbols_behind() {
            // Review r2 M3: the text a partial term leaves unmatched is spoken, not copied.
            let user = with_user(
                r#"{"version":1,"terms":{
                    "tool":{"say":"utility","match":"prefix"},
                    "name":{"say":"label","match":"suffix"},
                    "mid":{"say":"middle","match":"anywhere"},
                    "^id(.+)$":{"say":"id number $1","match":"regex"}}}"#,
            );
            for (token, expected) in [
                ("tool-name", "utility dash label"),
                ("tool@name", "utility at label"),
                ("tool@x", "utility at x"),
                ("tool+x", "utility plus x"),
                ("tool&x", "utility and x"),
                ("tool=x", "utility equals x"),
                ("tool:x", "utility colon x"),
                ("tool#x", "utility hash x"),
                ("tool!x", "utility x"),
                ("x$name", "x label"),
                ("a@mid@b", "a at middle at b"),
                ("id@7", "id number at 7"),
                ("id<7>", "id number 7"),
                ("tool-name,", "utility dash label,"),
                ("(tool@x)?!", "utility at x?!"),
            ] {
                let spoken = speak_technical(token, &user, &TechnicalOptions::default());
                assert_eq!(spoken, expected, "{token}");
            }
            let tool =
                with_user(r#"{"version":1,"terms":{"tool":{"say":"utility","match":"prefix"}}}"#);
            assert_eq!(
                speak_technical("tool-name", &tool, &TechnicalOptions::default()),
                "utility dash name"
            );
            assert_eq!(
                speak_technical("tool@name", &tool, &TechnicalOptions::default()),
                "utility at name"
            );
            let renamed = with_user(
                r#"{"version":1,"symbols":{"@":"around"},
                    "terms":{"tool":{"say":"utility","match":"prefix"}}}"#,
            );
            assert_eq!(
                speak_technical("tool@name", &renamed, &TechnicalOptions::default()),
                "utility around name"
            );
        }

        #[test]
        fn unsafe_user_replacements_are_file_errors_naming_the_key() {
            for (json, key) in [
                (
                    r#"{"version":1,"terms":{"customtool":"custom/tool"}}"#,
                    "customtool",
                ),
                (
                    r#"{"version":1,"terms":{"x":{"say":"a.b","match":"anywhere"}}}"#,
                    "x",
                ),
                (r#"{"version":1,"symbols":{"/":"a/b"}}"#, "/"),
                (r#"{"version":1,"extensions":{"sh":"S-H"}}"#, "sh"),
                (
                    r#"{"version":1,"terms":{"^v(.+)$":{"say":"v/$1","match":"regex"}}}"#,
                    "^v(.+)$",
                ),
                (
                    r#"{"version":1,"terms":{"^v(.+)$":{"say":"cost $$","match":"regex"}}}"#,
                    "^v(.+)$",
                ),
            ] {
                match Vocabulary::parse(json) {
                    Err(VocabularyError::Pattern {
                        key: found,
                        message,
                    }) => {
                        assert_eq!(found, key, "{json}");
                        assert!(message.contains("letters, digits and spaces"), "{message}");
                    }
                    other => panic!("{json} must be rejected, got {other:?}"),
                }
            }
            for acronym in ["C++", "A.I", "", "TT S"] {
                let json = format!(r#"{{"version":1,"acronyms":["GPU","{acronym}"]}}"#);
                match Vocabulary::parse(&json) {
                    Err(VocabularyError::Pattern { key, message }) => {
                        assert_eq!(key, acronym, "{json}");
                        assert!(
                            message.contains("only hold letters and digits"),
                            "{message}"
                        );
                    }
                    other => panic!("{json} must be rejected, got {other:?}"),
                }
            }
            assert!(
                Vocabulary::parse(
                    r#"{"version":1,"terms":{"^v(\\d+)$":{"say":"version ${1} x $1","match":"regex"}}}"#
                )
                .is_ok()
            );
        }

        #[test]
        fn builtin_vocabulary_parses() {
            assert!(Vocabulary::parse(BUILTIN_JSON).is_ok());
            assert_ne!(Vocabulary::builtin(), Vocabulary::default());
        }
    }

    mod options {
        use super::*;

        fn say_with(token: &str, full_paths: bool, full_links: bool) -> String {
            speak_technical(
                token,
                &Vocabulary::builtin(),
                &TechnicalOptions {
                    full_paths,
                    full_links,
                },
            )
        }

        #[test]
        fn defaults_are_full_paths_and_short_links() {
            assert_eq!(
                TechnicalOptions::default(),
                TechnicalOptions {
                    full_paths: true,
                    full_links: false,
                }
            );
        }

        #[test]
        fn short_paths_keep_the_last_segment_and_line() {
            assert_eq!(say_with("tests/lib.sh", false, false), "lib dot S H");
            assert_eq!(
                say_with("notes/.cache/demo/.tests", false, false),
                "dot tests"
            );
            assert_eq!(
                say_with("src/tts/manager.rs:684", false, false),
                "manager dot R S at line 684"
            );
            assert_eq!(
                say_with("src/tts/manager.rs:684:7,", false, false),
                "manager dot R S at line 684 column 7,"
            );
            assert_eq!(
                say_with("~/notes/todo.md.", false, false),
                "todo dot markdown."
            );
            assert_eq!(say_with("./b.txt", false, false), "b dot text");
            assert_eq!(
                say_with("tests/lib.sh", true, false),
                "tests slash lib dot S H"
            );
        }

        #[test]
        fn short_paths_keep_slash_commands_and_word_pairs() {
            assert_eq!(say_with("/review", false, false), "slash review");
            for pair in ["and/or", "TCP/IP", "provider/model", "GPU3/Vulkan"] {
                assert_eq!(
                    say_with(pair, false, false),
                    say_with(pair, true, false),
                    "{pair}"
                );
            }
            assert_eq!(
                say_with("provider/model", false, false),
                "provider slash model"
            );
        }

        #[test]
        fn full_links_drop_scheme_query_and_fragment() {
            let link = "https://github.com/cjpais/Handy?tab=readme#top";
            assert_eq!(
                say_with(link, true, true),
                "github dot com slash cjpais slash Handy"
            );
            assert_eq!(say_with(link, true, false), "link to github.com");
            assert_eq!(
                say_with("(https://example.com/docs/readme.md).", true, true),
                "example dot com slash docs slash readme dot markdown."
            );
            assert_eq!(say_with("http://github.com/", true, true), "github dot com");
            assert_eq!(say_with("https://", true, true), "link to ");
        }
    }

    mod output_contract {
        use super::*;

        const BASES: [&str; 13] = [
            "2024",
            "lib",
            "Lib",
            "fooBar",
            "fixture_base",
            "x2",
            "42",
            "a",
            "HTTP",
            "tool",
            "toolName",
            "myname",
            "id7",
        ];
        const SEPARATORS: [char; 10] = ['/', '.', '_', '-', '@', '+', '#', ':', '~', '='];
        const EXTENSIONS: [&str; 15] = [
            "sh", "Sh", "MD", "Json", "csv", "pdf", "Wav", "q", "7z", "001", "Abc", "zz", "ZZ",
            "abcde", "abcdef",
        ];
        const LINES: [&str; 3] = ["", ":12", ":3:4"];
        const WRAPS: [(&str, &str); 5] =
            [("", ""), ("**", "**"), ("`", "`"), ("(", ")"), ("\"", "\"")];
        const ENDINGS: [&str; 6] = ["", ".", ",", "?!", "...", ";"];
        /// Characters a token may end with around its sentence punctuation.
        const TRAILING: &str = ")]}'\"`*.,;:!?";
        const SENTENCE: &str = ".,;:!?";

        /// Deterministic token shapes: identifier pairs joined by every separator, file names
        /// with known and unknown extensions in mixed case and line suffixes, paths, each also
        /// wrapped in Markdown or quotes and followed by a punctuation run.
        fn generated_tokens() -> Vec<String> {
            let mut cores = Vec::new();
            for first in BASES {
                for separator in SEPARATORS {
                    for second in BASES {
                        cores.push(format!("{first}{separator}{second}"));
                    }
                }
            }
            for base in BASES {
                for extension in EXTENSIONS {
                    for line in LINES {
                        cores.push(format!("{base}.{extension}{line}"));
                    }
                    cores.push(format!("dir/{base}.{extension}"));
                }
            }
            for base in BASES {
                cores.push(format!("https://example.com/{base}/{base}.md?x=1#top"));
                cores.push(format!("http://{base}.org"));
                cores.push(format!("~/src/{base}/.cache"));
                cores.push(format!("{base}/{base}:7"));
            }
            let mut tokens = Vec::with_capacity(cores.len() * 2);
            for (index, core) in cores.iter().enumerate() {
                let (open, close) = WRAPS[index % WRAPS.len()];
                let ending = ENDINGS[(index / WRAPS.len()) % ENDINGS.len()];
                tokens.push(core.clone());
                tokens.push(format!("{open}{core}{close}{ending}"));
            }
            tokens
        }

        /// The sentence punctuation in the run of closing characters the token ends with.
        fn own_ending(token: &str) -> String {
            let body = token.trim_end_matches(|c| TRAILING.contains(c));
            token[body.len()..]
                .chars()
                .filter(|c| SENTENCE.contains(*c))
                .collect()
        }

        /// Letters, digits, single inner spaces and digit dots, then the token's own ending; a
        /// link keeps its host as written.
        fn contract_violation(token: &str, spoken: &str) -> Option<String> {
            let ending = own_ending(token);
            let Some(body) = spoken.strip_suffix(ending.as_str()) else {
                return Some(format!("does not end with {ending:?}"));
            };
            if token.contains("://") && body.starts_with("link to ") {
                return None;
            }
            if body.starts_with(' ') || body.ends_with(' ') || body.contains("  ") {
                return Some("has a stray space".to_string());
            }
            let chars: Vec<char> = body.chars().collect();
            chars.iter().enumerate().find_map(|(index, &c)| {
                let digit_dot = c == '.'
                    && index > 0
                    && chars[index - 1].is_ascii_digit()
                    && chars.get(index + 1).is_some_and(char::is_ascii_digit);
                (!(c.is_alphanumeric() || c == ' ' || digit_dot)).then(|| format!("holds {c:?}"))
            })
        }

        #[test]
        fn every_transformed_token_is_speakable() {
            let user = with_user(
                r##"{"version":1,
                    "symbols":{"/":"over","@":"around","#":"number sign"},
                    "extensions":{"zz":"zed zed","Sh":"shell","abcde":"alphabet"},
                    "terms":{
                        "tool":{"say":"utility","match":"prefix"},
                        "name":{"say":"label","match":"suffix"},
                        "Out":{"say":"out word","match":"anywhere"},
                        "ab":{"say":"A B","match":"anywhere","case":false},
                        "^id(.+)$":{"say":"id number $1","match":"regex"},
                        "x(.)":{"say":"ex $1","match":"regex"}}}"##,
            );
            let tokens = generated_tokens();
            assert!(tokens.len() >= 500, "only {} tokens", tokens.len());
            let mut transformed = 0;
            let mut violations = Vec::new();
            let all_options = [false, true].into_iter().flat_map(|full_paths| {
                [false, true].map(|full_links| TechnicalOptions {
                    full_paths,
                    full_links,
                })
            });
            for options in all_options {
                for vocabulary in [&Vocabulary::builtin(), &user] {
                    for token in &tokens {
                        let spoken = speak_technical(token, vocabulary, &options);
                        if spoken == *token {
                            continue;
                        }
                        transformed += 1;
                        if let Some(problem) = contract_violation(token, &spoken) {
                            violations
                                .push(format!("{token} {options:?} -> {spoken:?}: {problem}"));
                        }
                    }
                }
            }
            assert!(
                transformed >= 500,
                "only {transformed} tokens were transformed"
            );
            assert!(
                violations.is_empty(),
                "{} of {transformed} transformed tokens break the contract:\n{}",
                violations.len(),
                violations[..violations.len().min(40)].join("\n")
            );
        }

        #[test]
        fn the_contract_checker_rejects_symbols() {
            assert!(contract_violation("tool-name", "utility-name").is_some());
            assert!(contract_violation("tool@name", "utility@name").is_some());
            assert!(contract_violation("lib.sh,", "lib dot S H").is_some());
            assert!(contract_violation("lib.sh", "lib  dot").is_some());
            assert!(contract_violation("C++", "C + +").is_some());
            assert_eq!(contract_violation("**lib.sh.**", "lib dot S H."), None);
            assert_eq!(
                contract_violation("--speed=1.5", "dash dash speed equals 1.5"),
                None
            );
            assert_eq!(
                contract_violation("(https://x.org/a).", "link to x.org."),
                None
            );
        }
    }
}
