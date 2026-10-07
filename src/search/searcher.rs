//! `IndexSearcher`: builds index-wide weights for a query, then scores each segment with the
//! fastest applicable scorer and collects the top hits.

use crate::codec::postings_reader::NO_MORE_DOCS;
use crate::document::Document;
use crate::error::{Error, Result};
use crate::index::doc_values::SortValue;
use crate::index::{DirectoryReader, LiveDocs, SegmentReader, Term};
use crate::search::bulk::{BlockMaxConjunctionBulkScorer, MaxScoreBulkScorer, score_term};
use crate::search::collector::TopScoreDocCollector;
use crate::search::query::{Occur, Query};
use crate::search::scorer::{
    BoxScorer, ConjunctionScorer, ConstantScorer, DisjunctionScorer, MatchAllScorer, PhraseScorer,
    ReqExclScorer, ReqOptScorer,
};
use crate::search::term_scorer::TermScorer;
use crate::sim::{Bm25, avg_field_length, idf, idf_sum};
use std::ops::Bound;

/// Exact hit counting stops after this many hits (Lucene's default for `search(query, n)`).
pub const TOTAL_HITS_THRESHOLD: u64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotalHitsRelation {
    EqualTo,
    /// Counting stopped early because the remaining documents could not reach the top `n`.
    GreaterThanOrEqualTo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TotalHits {
    pub value: u64,
    pub relation: TotalHitsRelation,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreDoc {
    /// Global document ID (valid for the reader that produced it).
    pub doc: u32,
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopDocs {
    pub total_hits: TotalHits,
    /// Best hits first; ties broken by lower doc ID.
    pub score_docs: Vec<ScoreDoc>,
}

/// A query prepared against index-wide statistics (Lucene's `Weight`).
pub(super) enum Weight {
    Range {
        field: String,
        lower: Bound<SortValue>,
        upper: Bound<SortValue>,
        score: f32,
    },
    /// `sim` is `None` when the term occurs nowhere in the index.
    Term {
        term: Term,
        sim: Option<Bm25>,
    },
    Boolean {
        clauses: Vec<(Self, Occur)>,
        min_should_match: usize,
    },
    Phrase {
        field: String,
        terms: Vec<(Vec<u8>, u32)>,
        sim: Option<Bm25>,
    },
    MatchAll {
        score: f32,
    },
    ConstantScore {
        inner: Box<Self>,
        score: f32,
    },
    MatchNone,
}

pub struct IndexSearcher {
    reader: DirectoryReader,
}

impl IndexSearcher {
    #[must_use]
    pub const fn new(reader: DirectoryReader) -> Self {
        Self { reader }
    }

    #[must_use]
    pub const fn reader(&self) -> &DirectoryReader {
        &self.reader
    }

    /// Stored fields of a hit.
    ///
    /// # Errors
    /// As for [`DirectoryReader::document`].
    pub fn doc(&self, doc: u32) -> Result<Document> {
        self.reader.document(doc)
    }

    pub(super) fn create_weight(&self, q: &Query, boost: f32) -> Weight {
        match q {
            Query::I64Range {
                field,
                lower,
                upper,
            } => Weight::Range {
                field: field.clone(),
                lower: lower.map(SortValue::I64),
                upper: upper.map(SortValue::I64),
                score: boost,
            },
            Query::F64Range {
                field,
                lower,
                upper,
            } => Weight::Range {
                field: field.clone(),
                lower: lower.map(SortValue::F64),
                upper: upper.map(SortValue::F64),
                score: boost,
            },
            Query::Term(term) => {
                let field = term.field.as_str();
                let sim = match (
                    self.reader.collection_stats(field),
                    self.reader.term_stats(field, &term.bytes),
                ) {
                    (Some(c), Some(t)) => Some(Bm25::new(
                        boost,
                        idf(t.doc_freq, c.doc_count),
                        avg_field_length(c.sum_total_term_freq, c.doc_count),
                    )),
                    _ => None,
                };
                Weight::Term {
                    term: term.clone(),
                    sim,
                }
            }
            Query::Boolean(b) => Weight::Boolean {
                clauses: b
                    .clauses
                    .iter()
                    .map(|(q, o)| {
                        let scoring = matches!(o, Occur::Must | Occur::Should);
                        (self.create_weight(q, if scoring { boost } else { 0.0 }), *o)
                    })
                    .collect(),
                min_should_match: b.minimum_should_match,
            },
            Query::Phrase(p) => {
                let c = self.reader.collection_stats(&p.field);
                let stats: Option<Vec<_>> = p
                    .terms
                    .iter()
                    .map(|(t, _)| self.reader.term_stats(&p.field, t))
                    .collect();
                let sim = match (c, stats) {
                    (Some(c), Some(stats)) => Some(Bm25::new(
                        boost,
                        idf_sum(stats.iter().map(|t| idf(t.doc_freq, c.doc_count))),
                        avg_field_length(c.sum_total_term_freq, c.doc_count),
                    )),
                    _ => None,
                };
                Weight::Phrase {
                    field: p.field.clone(),
                    terms: p.terms.clone(),
                    sim,
                }
            }
            Query::Boost(inner, b) => self.create_weight(inner, boost * b),
            Query::ConstantScore(inner) => Weight::ConstantScore {
                inner: Box::new(self.create_weight(inner, 0.0)),
                score: boost,
            },
            Query::MatchAll => Weight::MatchAll { score: boost },
            Query::MatchNone => Weight::MatchNone,
        }
    }

    /// The top `n` hits by score, plus the hit count (exact up to 1000 hits).
    ///
    /// # Errors
    /// [`Error::IllegalArgument`] for a phrase query on a field without positions.
    pub fn search(&self, query: &Query, n: usize) -> Result<TopDocs> {
        self.validate_query(query)?;
        let n = n.max(1);
        let weight = self.create_weight(&query.rewrite(), 1.0);
        let mut c = TopScoreDocCollector::new(n, TOTAL_HITS_THRESHOLD);
        for (seg, &base) in self.reader.segments().iter().zip(self.reader.doc_bases()) {
            c.begin_leaf(base);
            score_segment(&weight, seg, &mut c)?;
        }
        Ok(TopDocs {
            total_hits: TotalHits {
                value: c.total_hits,
                relation: if c.hits_lower_bound {
                    TotalHitsRelation::GreaterThanOrEqualTo
                } else {
                    TotalHitsRelation::EqualTo
                },
            },
            score_docs: c
                .top_docs()
                .into_iter()
                .filter_map(|(doc, score)| {
                    Some(ScoreDoc {
                        doc: u32::try_from(doc).ok()?,
                        score,
                    })
                })
                .collect(),
        })
    }

    /// Exact number of live documents matching `query`.
    ///
    /// # Errors
    /// As for [`IndexSearcher::search`].
    pub fn count(&self, query: &Query) -> Result<u64> {
        self.validate_query(query)?;
        let weight = self.create_weight(&query.rewrite(), 0.0);
        let mut total = 0u64;
        for seg in self.reader.segments() {
            if let Some(mut s) = scorer(&weight, seg)? {
                let live = seg.live_docs();
                loop {
                    let d = s.next_doc();
                    if d == NO_MORE_DOCS {
                        break;
                    }
                    total = total.saturating_add(u64::from(is_live(live, d)));
                }
            }
        }
        Ok(total)
    }
}

fn term_scorer<'a>(
    seg: &'a SegmentReader,
    term: &Term,
    sim: &Bm25,
    top_level: bool,
) -> Option<TermScorer<'a>> {
    let (field, meta) = seg.term_meta(&term.field, &term.bytes)?;
    let pe = seg.postings(&field, &meta, false);
    Some(TermScorer::new(
        pe,
        sim.clone(),
        seg.norms(field.number),
        top_level,
    ))
}

/// A clause that is a (possibly boosted, already folded into `sim`) term.
const fn as_term(w: &Weight) -> Option<(&Term, Option<&Bm25>)> {
    match w {
        Weight::Term { term, sim } => Some((term, sim.as_ref())),
        _ => None,
    }
}

/// Scores one segment, using Lucene's specialized bulk scorers where they apply.
fn score_segment(w: &Weight, seg: &SegmentReader, c: &mut TopScoreDocCollector) -> Result<()> {
    let live = seg.live_docs();
    match w {
        Weight::Term {
            term,
            sim: Some(sim),
        } => {
            if let Some(mut s) = term_scorer(seg, term, sim, true) {
                score_term(&mut s, c, live);
            }
            return Ok(());
        }
        Weight::Term { sim: None, .. } | Weight::MatchNone => return Ok(()),
        Weight::Phrase { field, terms, sim } => {
            if let Some(mut s) = phrase_scorer(seg, field, terms, sim.as_ref())? {
                s.score_top_k(c, live);
            }
            return Ok(());
        }
        Weight::Boolean {
            clauses,
            min_should_match,
        } if clauses.iter().all(|(w, _)| as_term(w).is_some()) => {
            let only = |o: &[Occur]| clauses.iter().all(|(_, x)| o.contains(x));
            let musts = clauses.iter().filter(|(_, o)| *o == Occur::Must).count();
            if only(&[Occur::Should]) && *min_should_match <= 1 {
                // MaxScoreBulkScorer over the terms present in this segment
                let scorers: Vec<TermScorer> = clauses
                    .iter()
                    .filter_map(|(w, _)| {
                        as_term(w).and_then(|(t, sim)| term_scorer(seg, t, sim?, false))
                    })
                    .collect();
                let mut scorers = scorers;
                if scorers.len() > 1 {
                    MaxScoreBulkScorer::new(scorers, live).score(c, 0, NO_MORE_DOCS);
                } else if let Some(mut s) = scorers.pop() {
                    // a single clause left in this segment is scored as a top-level term
                    s.set_top_level();
                    score_term(&mut s, c, live);
                }
                return Ok(());
            }
            if only(&[Occur::Must, Occur::Filter]) && musts >= 2 {
                let mut scorers = Vec::with_capacity(clauses.len());
                for (w, _) in clauses {
                    let Some((t, sim)) = as_term(w) else {
                        return Ok(());
                    };
                    match sim.and_then(|sim| term_scorer(seg, t, sim, false)) {
                        Some(s) => scorers.push(s),
                        None => return Ok(()), // a required term is missing: no matches here
                    }
                }
                BlockMaxConjunctionBulkScorer::new(scorers, live).score(c, 0, NO_MORE_DOCS);
                return Ok(());
            }
        }
        _ => {}
    }
    if let Some(mut s) = scorer(w, seg)? {
        loop {
            let d = s.next_doc();
            if d == NO_MORE_DOCS {
                break;
            }
            if is_live(live, d) {
                c.collect(d, s.score());
            }
        }
    }
    Ok(())
}

pub(super) fn is_live(live: Option<&LiveDocs>, doc: i32) -> bool {
    u32::try_from(doc).is_ok_and(|d| live.is_none_or(|l| l.get(d)))
}

/// At least `min_match` (and at least one) of `subs`; `None` if that cannot happen.
fn disjunction(mut subs: Vec<BoxScorer<'_>>, min_match: usize) -> Option<BoxScorer<'_>> {
    if subs.len() < min_match.max(1) {
        return None;
    }
    if subs.len() == 1 {
        return subs.pop();
    }
    Some(Box::new(DisjunctionScorer::new(subs, min_match)))
}

/// All of `subs` (flag: whether the clause scores); a lone scoring clause is used directly.
fn conjunction(mut subs: Vec<(BoxScorer<'_>, bool)>) -> Option<BoxScorer<'_>> {
    match subs.as_slice() {
        [] => None,
        [(_, true)] => subs.pop().map(|(s, _)| s),
        _ => Some(Box::new(ConjunctionScorer::new(subs))),
    }
}

/// Exact-phrase scorer for one segment, or `None` if some term is absent there.
fn phrase_scorer<'a>(
    seg: &'a SegmentReader,
    field: &str,
    terms: &[(Vec<u8>, u32)],
    sim: Option<&Bm25>,
) -> Result<Option<PhraseScorer<'a>>> {
    let Some(info) = seg.field_infos().get(field) else {
        return Ok(None);
    };
    if info.index_options.is_indexed_without_positions() {
        return Err(Error::IllegalArgument(format!(
            "field \"{field}\" was indexed without position data; cannot run a phrase query"
        )));
    }
    let Some(sim) = sim else { return Ok(None) };
    let mut postings = Vec::with_capacity(terms.len());
    for (t, pos) in terms {
        let Some((fi, meta)) = seg.term_meta(field, t) else {
            return Ok(None);
        };
        let pos = i32::try_from(*pos)
            .map_err(|_| Error::IllegalArgument("phrase position too large".into()))?;
        postings.push((seg.postings(&fi, &meta, true), pos));
    }
    Ok(Some(PhraseScorer::new(
        postings,
        sim.clone(),
        seg.norms(info.number),
    )))
}

/// A doc-at-a-time scorer for any weight, or `None` if nothing in the segment can match.
pub(super) fn scorer<'a>(w: &Weight, seg: &'a SegmentReader) -> Result<Option<BoxScorer<'a>>> {
    Ok(match w {
        Weight::Range {
            field,
            lower,
            upper,
            score,
        } => Some(Box::new(crate::search::scorer::RangeScorer::new(
            seg.range_docs(field, lower, upper),
            *score,
        ))),
        Weight::MatchNone => None,
        Weight::Term { term, sim } => sim
            .as_ref()
            .and_then(|sim| term_scorer(seg, term, sim, false))
            .map(|s| -> BoxScorer { Box::new(s) }),
        Weight::MatchAll { score } => Some(Box::new(MatchAllScorer::new(seg.max_doc(), *score))),
        Weight::ConstantScore { inner, score } => {
            scorer(inner, seg)?.map(|s| -> BoxScorer { Box::new(ConstantScorer::new(s, *score)) })
        }
        Weight::Phrase { field, terms, sim } => {
            phrase_scorer(seg, field, terms, sim.as_ref())?.map(|s| -> BoxScorer { Box::new(s) })
        }
        Weight::Boolean {
            clauses,
            min_should_match,
        } => {
            let msm = *min_should_match;
            let mut required: Vec<(BoxScorer, bool)> = Vec::new();
            let mut optional: Vec<BoxScorer> = Vec::new();
            let mut prohibited: Vec<BoxScorer> = Vec::new();
            for (cw, occur) in clauses {
                let s = scorer(cw, seg)?;
                match (occur, s) {
                    (Occur::Must | Occur::Filter, None) => return Ok(None),
                    (Occur::Must, Some(s)) => required.push((s, true)),
                    (Occur::Filter, Some(s)) => required.push((s, false)),
                    (Occur::Should, Some(s)) => optional.push(s),
                    (Occur::MustNot, Some(s)) => prohibited.push(s),
                    (_, None) => {}
                }
            }
            let main = if required.is_empty() {
                let Some(any) = disjunction(optional, msm) else {
                    return Ok(None);
                };
                any
            } else {
                if msm > 0 {
                    let Some(any) = disjunction(std::mem::take(&mut optional), msm) else {
                        return Ok(None);
                    };
                    required.push((any, true));
                }
                let Some(req) = conjunction(required) else {
                    return Ok(None);
                };
                match disjunction(optional, 1) {
                    Some(opt) => Box::new(ReqOptScorer::new(req, opt)),
                    None => req,
                }
            };
            Some(match disjunction(prohibited, 1) {
                Some(excl) => Box::new(ReqExclScorer::new(main, excl)),
                None => main,
            })
        }
    })
}
