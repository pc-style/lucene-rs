//! Lucene "classic" query syntax: `title:rust +"exact phrase" -draft (a OR b)^2`.
//!
//! Supported: terms, `field:` prefixes, quoted phrases, `+`/`-`/`!` and `AND`/`OR`/`NOT`/`&&`/
//! `||` with the classic parser's precedence rules, grouping, `^boost`, and `\` escapes.
//! Not supported (returns an error): wildcards, fuzzy terms, ranges, sloppy phrases.

use crate::analysis::{Analyzer, tokenize};
use crate::error::{Error, Result};
use crate::search::{BooleanQuery, Occur, PhraseQuery, Query};
use std::sync::Arc;

/// Operator applied between clauses that have no explicit `AND`/`OR`/`+`/`-`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Or,
    And,
}

pub struct QueryParser {
    default_field: String,
    analyzer: Arc<dyn Analyzer>,
    default_operator: Operator,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Term(String),
    Quoted(String),
    Colon,
    Plus,
    Minus,
    Not,
    And,
    Or,
    LParen,
    RParen,
    Caret,
    Tilde,
}

#[derive(Clone, Copy, PartialEq)]
enum Conj {
    None,
    And,
    Or,
}

#[derive(Clone, Copy, PartialEq)]
enum Mods {
    None,
    Req,
    Not,
}

fn err(msg: impl Into<String>) -> Error {
    Error::QueryParse(msg.into())
}

/// Characters that end a term (`+` and `-` only when they start one, so "wi-fi" is one term).
const fn ends_term(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '!' | '(' | ')' | ':' | '^' | '"' | '~' | '[' | ']' | '{' | '}' | '*' | '?'
        )
}

fn lex(s: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let tok = match c {
            c if c.is_whitespace() => continue,
            '+' => Tok::Plus,
            '-' => Tok::Minus,
            '!' => Tok::Not,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ':' => Tok::Colon,
            '^' => Tok::Caret,
            '~' => Tok::Tilde,
            '&' if chars.next_if_eq(&'&').is_some() => Tok::And,
            '|' if chars.next_if_eq(&'|').is_some() => Tok::Or,
            '"' => {
                let mut t = String::new();
                loop {
                    match chars.next() {
                        None => return Err(err("unterminated quoted phrase")),
                        Some('"') => break,
                        Some('\\') => t.push(chars.next().ok_or_else(|| err("dangling escape"))?),
                        Some(ch) => t.push(ch),
                    }
                }
                Tok::Quoted(t)
            }
            '[' | ']' | '{' | '}' => return Err(err("range queries are not supported")),
            '*' | '?' => return Err(err("wildcard queries are not supported")),
            first => {
                let mut t = String::new();
                let mut next = Some(first);
                while let Some(ch) = next {
                    if ch == '\\' {
                        t.push(chars.next().ok_or_else(|| err("dangling escape"))?);
                    } else {
                        t.push(ch);
                    }
                    next = chars.next_if(|&ch| !ends_term(ch));
                }
                if chars.peek().is_some_and(|&ch| ch == '*' || ch == '?') {
                    return Err(err("wildcard queries are not supported"));
                }
                match t.as_str() {
                    "AND" => Tok::And,
                    "OR" => Tok::Or,
                    "NOT" => Tok::Not,
                    _ => Tok::Term(t),
                }
            }
        };
        out.push(tok);
    }
    Ok(out)
}

/// Token cursor for the recursive-descent parser.
struct Cursor {
    toks: std::iter::Peekable<std::vec::IntoIter<Tok>>,
}

impl Cursor {
    fn peek(&mut self) -> Option<&Tok> {
        self.toks.peek()
    }
    fn eat(&mut self, t: &Tok) -> bool {
        self.toks.next_if_eq(t).is_some()
    }
}

impl QueryParser {
    pub fn new(default_field: impl Into<String>, analyzer: impl Analyzer + 'static) -> Self {
        Self::with_shared_analyzer(default_field, Arc::new(analyzer))
    }

    pub fn with_shared_analyzer(
        default_field: impl Into<String>,
        analyzer: Arc<dyn Analyzer>,
    ) -> Self {
        Self {
            default_field: default_field.into(),
            analyzer,
            default_operator: Operator::Or,
        }
    }

    #[must_use]
    pub const fn default_operator(mut self, op: Operator) -> Self {
        self.default_operator = op;
        self
    }

    /// Parses `text` into a query (`MatchNone` if it analyzes to nothing).
    ///
    /// # Errors
    /// [`Error::QueryParse`] for malformed or unsupported syntax.
    pub fn parse(&self, text: &str) -> Result<Query> {
        let mut cur = Cursor {
            toks: lex(text)?.into_iter().peekable(),
        };
        let q = self.parse_query(&mut cur, &self.default_field)?;
        if let Some(t) = cur.peek() {
            return Err(err(format!("unexpected {t:?}")));
        }
        Ok(q.unwrap_or(Query::MatchNone))
    }

    fn parse_query(&self, cur: &mut Cursor, field: &str) -> Result<Option<Query>> {
        let mut clauses: Vec<(Query, Occur)> = Vec::new();
        while cur.peek().is_some_and(|t| *t != Tok::RParen) {
            let conj = if clauses.is_empty() {
                Conj::None
            } else if cur.eat(&Tok::And) {
                Conj::And
            } else if cur.eat(&Tok::Or) {
                Conj::Or
            } else {
                Conj::None
            };
            let mods = if cur.eat(&Tok::Plus) {
                Mods::Req
            } else if cur.eat(&Tok::Minus) || cur.eat(&Tok::Not) {
                Mods::Not
            } else {
                Mods::None
            };
            let q = self.parse_clause(cur, field)?;
            self.add_clause(&mut clauses, conj, mods, q);
        }
        Ok(match clauses.as_slice() {
            [] => None,
            [(q, Occur::Should | Occur::Must)] => Some(q.clone()),
            _ => Some(Query::Boolean(BooleanQuery {
                clauses,
                minimum_should_match: 0,
            })),
        })
    }

    /// `QueryParserBase#addClause`.
    fn add_clause(
        &self,
        clauses: &mut Vec<(Query, Occur)>,
        conj: Conj,
        mods: Mods,
        q: Option<Query>,
    ) {
        if let Some(last) = clauses.last_mut() {
            if conj == Conj::And && last.1 != Occur::MustNot {
                last.1 = Occur::Must;
            }
            if self.default_operator == Operator::And
                && conj == Conj::Or
                && last.1 != Occur::MustNot
            {
                last.1 = Occur::Should;
            }
        }
        let Some(q) = q else { return };
        let prohibited = mods == Mods::Not;
        let required = match self.default_operator {
            Operator::Or => mods == Mods::Req || (conj == Conj::And && !prohibited),
            Operator::And => !prohibited && conj != Conj::Or,
        };
        let occur = if prohibited {
            Occur::MustNot
        } else if required {
            Occur::Must
        } else {
            Occur::Should
        };
        clauses.push((q, occur));
    }

    fn parse_clause(&self, cur: &mut Cursor, default_field: &str) -> Result<Option<Query>> {
        let field = default_field;
        let q = match cur.toks.next() {
            Some(Tok::Term(text)) if cur.eat(&Tok::Colon) => {
                return self.parse_clause_body(cur, &text);
            }
            Some(Tok::Term(text)) => {
                if cur.peek() == Some(&Tok::Tilde) {
                    return Err(err("fuzzy queries are not supported"));
                }
                self.terms(field, &text)
            }
            Some(t) => return self.parse_clause_body_from(cur, field, t),
            None => return Err(err("unexpected end of query")),
        };
        Self::parse_boost(cur, q)
    }

    fn parse_clause_body(&self, cur: &mut Cursor, field: &str) -> Result<Option<Query>> {
        match cur.toks.next() {
            Some(Tok::Term(text)) => {
                if cur.peek() == Some(&Tok::Tilde) {
                    return Err(err("fuzzy queries are not supported"));
                }
                let q = self.terms(field, &text);
                Self::parse_boost(cur, q)
            }
            Some(t) => self.parse_clause_body_from(cur, field, t),
            None => Err(err("unexpected end of query")),
        }
    }

    fn parse_clause_body_from(
        &self,
        cur: &mut Cursor,
        field: &str,
        first: Tok,
    ) -> Result<Option<Query>> {
        let q = match first {
            Tok::LParen => {
                let q = self.parse_query(cur, field)?;
                if !cur.eat(&Tok::RParen) {
                    return Err(err("missing closing parenthesis"));
                }
                q
            }
            Tok::Quoted(text) => {
                if cur.peek() == Some(&Tok::Tilde) {
                    return Err(err("sloppy phrase queries are not supported"));
                }
                self.phrase(field, &text)
            }
            t => return Err(err(format!("unexpected {t:?}"))),
        };
        Self::parse_boost(cur, q)
    }

    fn parse_boost(cur: &mut Cursor, q: Option<Query>) -> Result<Option<Query>> {
        if !cur.eat(&Tok::Caret) {
            return Ok(q);
        }
        let b = match cur.toks.next() {
            Some(Tok::Term(n)) => n
                .parse::<f32>()
                .map_err(|_| err(format!("bad boost {n}")))?,
            _ => return Err(err("expected a number after ^")),
        };
        Ok(q.map(|q| q.boost(b)))
    }

    /// `QueryBuilder#createFieldQuery` for unquoted text.
    fn terms(&self, field: &str, text: &str) -> Option<Query> {
        let toks = tokenize(&*self.analyzer, field, text);
        match toks.as_slice() {
            [] => None,
            [(t, _)] => Some(Query::term(field, t)),
            _ => {
                let occur = if self.default_operator == Operator::And {
                    Occur::Must
                } else {
                    Occur::Should
                };
                // tokens at the same position are synonyms: OR them together
                let b = toks
                    .chunk_by(|a, b| a.1 == b.1)
                    .fold(BooleanQuery::new(), |b, group| {
                        let q = match group {
                            [(t, _)] => Query::term(field, t),
                            _ => group
                                .iter()
                                .fold(BooleanQuery::new(), |s, (t, _)| {
                                    s.should(Query::term(field, t))
                                })
                                .build(),
                        };
                        b.clause(q, occur)
                    });
                Some(b.build())
            }
        }
    }

    fn phrase(&self, field: &str, text: &str) -> Option<Query> {
        let toks = tokenize(&*self.analyzer, field, text);
        match toks.as_slice() {
            [] => None,
            [(t, _)] => Some(Query::term(field, t)),
            [(_, base), ..] => {
                let base = *base;
                Some(
                    toks.iter()
                        .fold(PhraseQuery::new(field), |p, (t, pos)| {
                            p.term_at(t, pos.saturating_sub(base))
                        })
                        .build(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{StandardAnalyzer, WhitespaceAnalyzer};

    fn p(s: &str) -> Query {
        QueryParser::new("body", StandardAnalyzer::new())
            .parse(s)
            .unwrap()
    }
    fn t(f: &str, s: &str) -> Query {
        Query::term(f, s)
    }

    #[test]
    fn single_term_and_fields() {
        assert_eq!(p("Rust"), t("body", "rust"));
        assert_eq!(p("title:Rust"), t("title", "rust"));
    }

    #[test]
    fn modifiers_and_operators() {
        assert_eq!(
            p("a b"),
            BooleanQuery::new()
                .should(t("body", "a"))
                .should(t("body", "b"))
                .build()
        );
        assert_eq!(
            p("+a -b c"),
            BooleanQuery::new()
                .must(t("body", "a"))
                .must_not(t("body", "b"))
                .should(t("body", "c"))
                .build()
        );
        assert_eq!(
            p("a AND b"),
            BooleanQuery::new()
                .must(t("body", "a"))
                .must(t("body", "b"))
                .build()
        );
        assert_eq!(
            p("a && b || c"),
            BooleanQuery::new()
                .must(t("body", "a"))
                .must(t("body", "b"))
                .should(t("body", "c"))
                .build()
        );
        assert_eq!(
            p("a NOT b"),
            BooleanQuery::new()
                .should(t("body", "a"))
                .must_not(t("body", "b"))
                .build()
        );
        let and = QueryParser::new("body", StandardAnalyzer::new()).default_operator(Operator::And);
        assert_eq!(
            and.parse("a b OR c").unwrap(),
            BooleanQuery::new()
                .must(t("body", "a"))
                .should(t("body", "b"))
                .should(t("body", "c"))
                .build()
        );
    }

    #[test]
    fn phrases_groups_boosts() {
        assert_eq!(
            p("\"New York\""),
            PhraseQuery::new("body").term("new").term("york").build()
        );
        assert_eq!(
            p("title:(a b)^2"),
            BooleanQuery::new()
                .should(t("title", "a"))
                .should(t("title", "b"))
                .build()
                .boost(2.0)
        );
        assert_eq!(
            p("wi-fi"),
            BooleanQuery::new()
                .should(t("body", "wi"))
                .should(t("body", "fi"))
                .build()
        );
        let ws = QueryParser::new("body", WhitespaceAnalyzer);
        assert_eq!(ws.parse("a\\:b").unwrap(), t("body", "a:b"));
    }

    #[test]
    fn stop_words_keep_phrase_gaps() {
        let q = QueryParser::new("body", StandardAnalyzer::english())
            .parse("\"lord of the rings\"")
            .unwrap();
        assert_eq!(
            q,
            PhraseQuery::new("body")
                .term_at("lord", 0)
                .term_at("rings", 3)
                .build()
        );
    }

    #[test]
    fn unsupported_syntax_errors() {
        let qp = QueryParser::new("body", StandardAnalyzer::new());
        for s in [
            "foo*",
            "fo?",
            "foo~",
            "\"a b\"~2",
            "[a TO b]",
            "(a",
            "\"open",
        ] {
            assert!(qp.parse(s).is_err(), "{s}");
        }
    }
}
