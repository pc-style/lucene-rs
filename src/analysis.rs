//! Text analysis: turning field values into terms with positions.
//!
//! `StandardAnalyzer` follows Lucene's: UAX#29 word segmentation (via `unicode-segmentation`),
//! Java-style per-code-point lowercasing, optional stop words (none by default), tokens split at
//! 255 chars. Lucene's `StandardTokenizer` is a `JFlex` grammar for the same Unicode rules; the two
//! agree on ordinary text but can differ on emoji (Lucene emits them as tokens, this does not)
//! and on characters added in newer Unicode versions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

/// Lucene's default maximum token length; longer tokens are split into chunks of this size.
pub const DEFAULT_MAX_TOKEN_LENGTH: usize = 255;

/// Lucene's `EnglishAnalyzer.ENGLISH_STOP_WORDS_SET`.
pub const ENGLISH_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

/// Produces the terms of a field value.
pub trait Analyzer: Send + Sync {
    /// Calls `sink(term, position_increment)` for every token, in order. The first token of a
    /// value normally has increment 1; removed tokens (stop words) add to the next increment.
    fn analyze(&self, field: &str, text: &str, sink: &mut dyn FnMut(&str, u32));
}

impl<T: Analyzer + ?Sized> Analyzer for Arc<T> {
    fn analyze(&self, field: &str, text: &str, sink: &mut dyn FnMut(&str, u32)) {
        (**self).analyze(field, text, sink);
    }
}

impl<T: Analyzer + ?Sized> Analyzer for Box<T> {
    fn analyze(&self, field: &str, text: &str, sink: &mut dyn FnMut(&str, u32)) {
        (**self).analyze(field, text, sink);
    }
}

/// `Character.toLowerCase(int)`: the simple (1:1) Unicode lowercase mapping.
fn push_lowercase(out: &mut String, c: char) {
    if c.is_ascii() {
        out.push(c.to_ascii_lowercase());
    } else if c == '\u{0130}' {
        out.push('i'); // full mapping would be "i\u{307}"; Java's simple mapping is 'i'
    } else {
        out.extend(c.to_lowercase());
    }
}

/// `Character.isWhitespace`: Unicode space separators except no-break spaces, plus controls.
#[must_use]
pub const fn java_is_whitespace(c: char) -> bool {
    match c {
        '\u{00A0}' | '\u{2007}' | '\u{202F}' => false,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{001C}'..='\u{001F}' => true,
        _ => c.is_whitespace(),
    }
}

/// Emits `token` (already lowercased if needed), split into `max_len`-char chunks, applying
/// stop words and accumulating position increments.
struct Emitter<'s, 'f> {
    sink: &'s mut dyn FnMut(&str, u32),
    stop_words: Option<&'f HashSet<String>>,
    max_len: usize,
    pending_inc: u32,
}

impl Emitter<'_, '_> {
    fn emit(&mut self, token: &str) {
        let mut rest = token;
        while !rest.is_empty() {
            let split = rest
                .char_indices()
                .nth(self.max_len)
                .map_or(rest.len(), |(i, _)| i);
            let (t, tail) = rest.split_at(split);
            rest = tail;
            self.pending_inc = self.pending_inc.saturating_add(1);
            if self.stop_words.is_some_and(|s| s.contains(t)) {
                continue;
            }
            (self.sink)(t, self.pending_inc);
            self.pending_inc = 0;
        }
    }
}

/// Lucene's `StandardAnalyzer`: UAX#29 words, lowercased, optional stop words.
#[derive(Clone, Debug)]
pub struct StandardAnalyzer {
    stop_words: HashSet<String>,
    max_token_length: usize,
}

impl Default for StandardAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl StandardAnalyzer {
    /// No stop words, like Lucene's default `StandardAnalyzer`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stop_words: HashSet::new(),
            max_token_length: DEFAULT_MAX_TOKEN_LENGTH,
        }
    }
    pub fn with_stop_words<I, S>(words: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            stop_words: words.into_iter().map(Into::into).collect(),
            ..Self::new()
        }
    }
    /// `StandardAnalyzer` with `ENGLISH_STOP_WORDS`.
    #[must_use]
    pub fn english() -> Self {
        Self::with_stop_words(ENGLISH_STOP_WORDS.iter().copied())
    }
    /// Tokens longer than `n` chars are split into chunks of `n` (values below 1 count as 1).
    #[must_use]
    pub fn max_token_length(mut self, n: usize) -> Self {
        self.max_token_length = n.max(1);
        self
    }
}

impl Analyzer for StandardAnalyzer {
    fn analyze(&self, _field: &str, text: &str, sink: &mut dyn FnMut(&str, u32)) {
        let mut e = Emitter {
            sink,
            stop_words: (!self.stop_words.is_empty()).then_some(&self.stop_words),
            max_len: self.max_token_length,
            pending_inc: 0,
        };
        let mut buf = String::new();
        for word in text.split_word_bounds() {
            if !word.chars().any(char::is_alphanumeric) {
                continue;
            }
            buf.clear();
            for c in word.chars() {
                push_lowercase(&mut buf, c);
            }
            e.emit(&buf);
        }
    }
}

/// Lucene's `WhitespaceAnalyzer`: splits on Java whitespace, no lowercasing.
#[derive(Clone, Debug, Default)]
pub struct WhitespaceAnalyzer;

impl Analyzer for WhitespaceAnalyzer {
    fn analyze(&self, _field: &str, text: &str, sink: &mut dyn FnMut(&str, u32)) {
        let mut e = Emitter {
            sink,
            stop_words: None,
            max_len: DEFAULT_MAX_TOKEN_LENGTH,
            pending_inc: 0,
        };
        for t in text.split(java_is_whitespace).filter(|t| !t.is_empty()) {
            e.emit(t);
        }
    }
}

/// Lucene's `SimpleAnalyzer`: maximal runs of letters, lowercased.
#[derive(Clone, Debug, Default)]
pub struct SimpleAnalyzer;

impl Analyzer for SimpleAnalyzer {
    fn analyze(&self, _field: &str, text: &str, sink: &mut dyn FnMut(&str, u32)) {
        let mut e = Emitter {
            sink,
            stop_words: None,
            max_len: DEFAULT_MAX_TOKEN_LENGTH,
            pending_inc: 0,
        };
        let mut buf = String::new();
        for c in text.chars() {
            if c.is_alphabetic() {
                push_lowercase(&mut buf, c);
            } else if !buf.is_empty() {
                e.emit(&buf);
                buf.clear();
            }
        }
        if !buf.is_empty() {
            e.emit(&buf);
        }
    }
}

/// Lucene's `KeywordAnalyzer`: the whole value is one token.
#[derive(Clone, Debug, Default)]
pub struct KeywordAnalyzer;

impl Analyzer for KeywordAnalyzer {
    fn analyze(&self, _field: &str, text: &str, sink: &mut dyn FnMut(&str, u32)) {
        if !text.is_empty() {
            sink(text, 1);
        }
    }
}

/// Lucene's `PerFieldAnalyzerWrapper`.
pub struct PerFieldAnalyzer {
    default: Arc<dyn Analyzer>,
    fields: HashMap<String, Arc<dyn Analyzer>>,
}

impl PerFieldAnalyzer {
    pub fn new(default: impl Analyzer + 'static) -> Self {
        Self {
            default: Arc::new(default),
            fields: HashMap::new(),
        }
    }
    #[must_use]
    pub fn field(mut self, name: impl Into<String>, analyzer: impl Analyzer + 'static) -> Self {
        self.fields.insert(name.into(), Arc::new(analyzer));
        self
    }
}

impl Analyzer for PerFieldAnalyzer {
    fn analyze(&self, field: &str, text: &str, sink: &mut dyn FnMut(&str, u32)) {
        self.fields
            .get(field)
            .unwrap_or(&self.default)
            .analyze(field, text, sink);
    }
}

/// Collects `(term, position)` pairs; positions start at 0.
pub fn tokenize(analyzer: &dyn Analyzer, field: &str, text: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut next: u32 = 0; // position of the next token with increment 1
    analyzer.analyze(field, text, &mut |t, inc| {
        let pos = next.saturating_add(inc).saturating_sub(1);
        out.push((t.to_string(), pos));
        next = pos.saturating_add(1);
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(a: &dyn Analyzer, s: &str) -> Vec<(String, u32)> {
        tokenize(a, "f", s)
    }

    #[test]
    fn standard_uax29() {
        let t: Vec<String> = terms(
            &StandardAnalyzer::new(),
            "The U.S.A. can't wi-fi 3.14 Ünïcode İstanbul, 東京",
        )
        .into_iter()
        .map(|x| x.0)
        .collect();
        assert_eq!(
            t,
            [
                "the",
                "u.s.a",
                "can't",
                "wi",
                "fi",
                "3.14",
                "ünïcode",
                "istanbul",
                "東",
                "京"
            ]
        );
    }

    #[test]
    fn stop_words_leave_position_gaps() {
        let t = terms(&StandardAnalyzer::english(), "the quick and the dead");
        assert_eq!(t, [("quick".to_string(), 1), ("dead".to_string(), 4)]);
    }

    #[test]
    fn long_tokens_are_split() {
        let long = "x".repeat(600);
        let t = terms(&WhitespaceAnalyzer, &long);
        assert_eq!(
            t.iter().map(|x| x.0.len()).collect::<Vec<_>>(),
            [255, 255, 90]
        );
        assert_eq!(t.iter().map(|x| x.1).collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn whitespace_is_java_whitespace() {
        let t: Vec<String> = terms(&WhitespaceAnalyzer, "a\u{00A0}b c\td")
            .into_iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(t, ["a\u{00A0}b", "c", "d"]);
    }

    #[test]
    fn simple_and_keyword() {
        let t: Vec<String> = terms(&SimpleAnalyzer, "Hello, World42x")
            .into_iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(t, ["hello", "world", "x"]);
        assert_eq!(
            terms(&KeywordAnalyzer, "New York"),
            [("New York".to_string(), 0)]
        );
    }
}
