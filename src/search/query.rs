//! Queries and their rewrite rules (after Lucene's `Query#rewrite`).

use crate::index::Term;
use crate::num::f32_from_f64;

/// How a clause participates in a [`BooleanQuery`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Occur {
    /// Must match; contributes to the score.
    Must,
    /// May match; contributes to the score. With no `Must`/`Filter` clauses, at least
    /// `max(1, minimum_should_match)` should-clauses must match.
    Should,
    /// Must not match.
    MustNot,
    /// Must match; does not contribute to the score.
    Filter,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    /// Documents containing a term, scored with BM25.
    Term(Term),
    Boolean(BooleanQuery),
    /// Documents containing the terms at the given relative positions (exact phrase).
    Phrase(PhraseQuery),
    /// Multiplies the inner query's scores.
    Boost(Box<Self>, f32),
    /// Matches like the inner query; every match scores 1 (times any boost).
    ConstantScore(Box<Self>),
    MatchAll,
    MatchNone,
}

impl Query {
    pub fn term(field: impl Into<String>, text: impl AsRef<str>) -> Self {
        Self::Term(Term::new(field, text))
    }
    #[must_use]
    pub fn boost(self, boost: f32) -> Self {
        Self::Boost(Box::new(self), boost)
    }
    #[must_use]
    pub fn constant_score(self) -> Self {
        Self::ConstantScore(Box::new(self))
    }

    /// Simplifies to an equivalent query, repeating until nothing changes.
    #[must_use]
    pub fn rewrite(&self) -> Self {
        let mut q = self.clone();
        loop {
            let r = rewrite_once(&q);
            if r == q {
                return r;
            }
            q = r;
        }
    }
}

fn unboost(q: &Query) -> (&Query, f32) {
    match q {
        Query::Boost(inner, b) => (inner, *b),
        q => (q, 1.0),
    }
}

fn rewrite_once(q: &Query) -> Query {
    match q {
        Query::Boost(inner, b) => {
            let inner = rewrite_once(inner);
            #[allow(clippy::float_cmp)] // exact, like Lucene's `boost == 1f`
            let is_one = *b == 1.0;
            match inner {
                _ if is_one => inner,
                Query::MatchNone => Query::MatchNone,
                Query::Boost(i, b2) => Query::Boost(i, b2 * b),
                i => Query::Boost(Box::new(i), *b),
            }
        }
        Query::ConstantScore(inner) => match rewrite_once(inner) {
            Query::MatchNone => Query::MatchNone,
            Query::ConstantScore(i) | Query::Boost(i, _) => Query::ConstantScore(i),
            i => Query::ConstantScore(Box::new(i)),
        },
        Query::Phrase(p) => match p.terms.as_slice() {
            [] => Query::MatchNone,
            [(t, _)] => Query::Term(Term::from_bytes(p.field.clone(), t.clone())),
            _ => Query::Phrase(p.clone()),
        },
        Query::Boolean(b) => rewrite_boolean(b),
        q => q.clone(),
    }
}

fn rewrite_boolean(b: &BooleanQuery) -> Query {
    let msm = b.minimum_should_match;
    let mut clauses: Vec<(Query, Occur)> = Vec::new();
    for (q, occur) in &b.clauses {
        let q = rewrite_once(q);
        if q == Query::MatchNone {
            match occur {
                Occur::Must | Occur::Filter => return Query::MatchNone,
                Occur::Should | Occur::MustNot => continue,
            }
        }
        clauses.push((q, *occur));
    }
    let count = |o: Occur, c: &[(Query, Occur)]| c.iter().filter(|(_, x)| *x == o).count();
    if clauses.is_empty() || clauses.iter().all(|(_, o)| *o == Occur::MustNot) {
        return Query::MatchNone; // pure negative queries match nothing
    }
    if count(Occur::Should, &clauses) < msm {
        return Query::MatchNone;
    }
    if let ([(q, occur)], true) = (clauses.as_slice(), msm <= 1) {
        return match occur {
            Occur::Must | Occur::Should => q.clone(),
            Occur::Filter => Query::ConstantScore(Box::new(q.clone())).boost(0.0),
            Occur::MustNot => Query::MatchNone,
        };
    }
    // Dedupe: identical should (when msm <= 1) or must clauses become one clause with summed
    // boosts; duplicate filter/must-not clauses and filters that repeat a must clause are dropped.
    let mut out: Vec<(Query, Occur, f64)> = Vec::new();
    for (q, occur) in clauses {
        let summable = occur == Occur::Must || (occur == Occur::Should && msm <= 1);
        let (base, b) = unboost(&q);
        if summable {
            if let Some(e) = out
                .iter_mut()
                .find(|(o, oc, _)| *oc == occur && *o == *base)
            {
                e.2 += f64::from(b);
            } else {
                out.push((base.clone(), occur, f64::from(b)));
            }
        } else {
            let dup = out.iter().any(|(o, oc, _)| {
                (*oc == occur || (occur == Occur::Filter && *oc == Occur::Must)) && *o == q
            });
            if !dup {
                out.push((q, occur, 1.0));
            }
        }
    }
    #[allow(clippy::float_cmp)] // exact, like Lucene
    let clauses = out
        .into_iter()
        .map(|(q, o, b)| {
            if b != 1.0 && matches!(o, Occur::Must | Occur::Should) {
                (q.boost(f32_from_f64(b)), o)
            } else {
                (q, o)
            }
        })
        .collect();
    Query::Boolean(BooleanQuery {
        clauses,
        minimum_should_match: msm,
    })
}

/// A boolean combination of queries. Build with the chained methods, then [`BooleanQuery::build`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BooleanQuery {
    pub clauses: Vec<(Query, Occur)>,
    pub minimum_should_match: usize,
}

impl BooleanQuery {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn clause(mut self, query: Query, occur: Occur) -> Self {
        self.clauses.push((query, occur));
        self
    }
    #[must_use]
    pub fn must(self, query: Query) -> Self {
        self.clause(query, Occur::Must)
    }
    #[must_use]
    pub fn should(self, query: Query) -> Self {
        self.clause(query, Occur::Should)
    }
    #[must_use]
    pub fn must_not(self, query: Query) -> Self {
        self.clause(query, Occur::MustNot)
    }
    #[must_use]
    pub fn filter(self, query: Query) -> Self {
        self.clause(query, Occur::Filter)
    }
    #[must_use]
    pub const fn minimum_should_match(mut self, n: usize) -> Self {
        self.minimum_should_match = n;
        self
    }
    #[must_use]
    pub const fn build(self) -> Query {
        Query::Boolean(self)
    }
}

/// An exact phrase (slop 0). Terms are already-analyzed tokens with relative positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhraseQuery {
    pub field: String,
    pub terms: Vec<(Vec<u8>, u32)>,
}

impl PhraseQuery {
    #[must_use]
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            terms: Vec::new(),
        }
    }
    /// Appends a term at the next position.
    #[must_use]
    pub fn term(mut self, text: impl AsRef<str>) -> Self {
        let pos = self.terms.last().map_or(0, |t| t.1.saturating_add(1));
        self.terms.push((text.as_ref().as_bytes().to_vec(), pos));
        self
    }
    /// Adds a term at an explicit position (e.g. to leave a gap where a stop word was).
    #[must_use]
    pub fn term_at(mut self, text: impl AsRef<str>, position: u32) -> Self {
        self.terms
            .push((text.as_ref().as_bytes().to_vec(), position));
        self
    }
    #[must_use]
    pub const fn build(self) -> Query {
        Query::Phrase(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Query {
        Query::term("f", s)
    }

    #[test]
    fn single_clause_unwraps() {
        assert_eq!(BooleanQuery::new().should(t("a")).build().rewrite(), t("a"));
        assert_eq!(
            BooleanQuery::new().must_not(t("a")).build().rewrite(),
            Query::MatchNone
        );
        assert_eq!(BooleanQuery::new().build().rewrite(), Query::MatchNone);
    }

    #[test]
    fn duplicate_shoulds_sum_boosts() {
        let q = BooleanQuery::new()
            .should(t("a"))
            .should(t("a").boost(2.0))
            .should(t("b"))
            .build()
            .rewrite();
        assert_eq!(
            q,
            BooleanQuery::new()
                .should(t("a").boost(3.0))
                .should(t("b"))
                .build()
        );
    }

    #[test]
    fn nested_boosts_multiply_and_phrases_shrink() {
        assert_eq!(t("a").boost(2.0).boost(3.0).rewrite(), t("a").boost(6.0));
        assert_eq!(PhraseQuery::new("f").term("a").build().rewrite(), t("a"));
        assert_eq!(
            BooleanQuery::new()
                .must(t("a"))
                .must(Query::MatchNone)
                .build()
                .rewrite(),
            Query::MatchNone
        );
    }
}
