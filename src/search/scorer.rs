//! Doc-at-a-time scorers for the general query shapes (Lucene's `ConjunctionScorer`,
//! `DisjunctionSumScorer`, `ReqExclScorer`, `ReqOptSumScorer`, `PhraseScorer` with an
//! `ExactPhraseMatcher`). These score every match; the specialized bulk scorers in `bulk.rs`
//! cover the common pure-term shapes with dynamic pruning.
// Numeric kernel ported 1:1 from Lucene: indexes, offsets and integer casts mirror the Java
// source and sit on hot paths, so the numeric lints are relaxed here (and only here). Slice
// indexing stays bounds-checked: a corrupt index panics, it never reads out of bounds.
#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::cast_precision_loss
)]

use crate::codec::postings_reader::{NO_MORE_DOCS, PostingsEnum};
use crate::index::LiveDocs;
use crate::search::collector::TopScoreDocCollector;
use crate::search::term_scorer::TermScorer;
use crate::sim::Bm25;

pub trait Scorer {
    fn doc_id(&self) -> i32;
    fn next_doc(&mut self) -> i32;
    /// First match at or after `target` (`target` > current doc).
    fn advance(&mut self, target: i32) -> i32;
    fn score(&mut self) -> f32;
    fn cost(&self) -> i64;
}

impl Scorer for TermScorer<'_> {
    fn doc_id(&self) -> i32 {
        TermScorer::doc_id(self)
    }
    fn next_doc(&mut self) -> i32 {
        self.pe.next_doc()
    }
    fn advance(&mut self, target: i32) -> i32 {
        self.pe.advance(target)
    }
    fn score(&mut self) -> f32 {
        TermScorer::score(self)
    }
    fn cost(&self) -> i64 {
        TermScorer::cost(self)
    }
}

pub type BoxScorer<'a> = Box<dyn Scorer + 'a>;

pub struct RangeScorer {
    docs: std::vec::IntoIter<u32>,
    doc: i32,
    score: f32,
    cost: i64,
}

impl RangeScorer {
    pub fn new(docs: Vec<u32>, score: f32) -> Self {
        let cost = i64::try_from(docs.len()).unwrap_or(i64::MAX);
        Self {
            docs: docs.into_iter(),
            doc: -1,
            score,
            cost,
        }
    }
}

impl Scorer for RangeScorer {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> i32 {
        self.doc = self
            .docs
            .next()
            .and_then(|d| i32::try_from(d).ok())
            .unwrap_or(NO_MORE_DOCS);
        self.doc
    }
    fn advance(&mut self, target: i32) -> i32 {
        while self.doc < target {
            self.next_doc();
        }
        self.doc
    }
    fn score(&mut self) -> f32 {
        self.score
    }
    fn cost(&self) -> i64 {
        self.cost
    }
}

/// All sub-scorers match. Scores are summed in a double over the scoring clauses.
pub struct ConjunctionScorer<'a> {
    subs: Vec<(BoxScorer<'a>, bool)>, // (scorer, contributes to score), cheapest first
    doc: i32,
}

impl<'a> ConjunctionScorer<'a> {
    pub fn new(mut subs: Vec<(BoxScorer<'a>, bool)>) -> Self {
        subs.sort_by_key(|(s, _)| s.cost());
        ConjunctionScorer { subs, doc: -1 }
    }
    fn do_next(&mut self, mut doc: i32) -> i32 {
        'outer: loop {
            if doc == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return doc;
            }
            for i in 1..self.subs.len() {
                let s = &mut self.subs[i].0;
                let mut d = s.doc_id();
                if d < doc {
                    d = s.advance(doc);
                }
                if d > doc {
                    doc = self.subs[0].0.advance(d);
                    continue 'outer;
                }
            }
            self.doc = doc;
            return doc;
        }
    }
}

impl Scorer for ConjunctionScorer<'_> {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> i32 {
        let d = self.subs[0].0.next_doc();
        self.do_next(d)
    }
    fn advance(&mut self, target: i32) -> i32 {
        let d = self.subs[0].0.advance(target);
        self.do_next(d)
    }
    fn score(&mut self) -> f32 {
        let mut sum = 0f64;
        for (s, scoring) in &mut self.subs {
            if *scoring {
                sum += s.score() as f64;
            }
        }
        sum as f32
    }
    fn cost(&self) -> i64 {
        self.subs[0].0.cost()
    }
}

/// At least `min_match` sub-scorers match; scores of matching subs are summed in a double.
pub struct DisjunctionScorer<'a> {
    subs: Vec<BoxScorer<'a>>,
    min_match: usize,
    doc: i32,
}

impl<'a> DisjunctionScorer<'a> {
    pub fn new(subs: Vec<BoxScorer<'a>>, min_match: usize) -> Self {
        DisjunctionScorer {
            subs,
            min_match: min_match.max(1),
            doc: -1,
        }
    }
}

impl Scorer for DisjunctionScorer<'_> {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> i32 {
        self.advance(self.doc + 1)
    }
    fn advance(&mut self, mut target: i32) -> i32 {
        loop {
            let mut min = NO_MORE_DOCS;
            for s in &mut self.subs {
                let mut d = s.doc_id();
                if d < target {
                    d = s.advance(target);
                }
                min = min.min(d);
            }
            if min == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return min;
            }
            let n = self.subs.iter().filter(|s| s.doc_id() == min).count();
            if n >= self.min_match {
                self.doc = min;
                return min;
            }
            target = min + 1;
        }
    }
    fn score(&mut self) -> f32 {
        let doc = self.doc;
        let mut sum = 0f64;
        for s in &mut self.subs {
            if s.doc_id() == doc {
                sum += s.score() as f64;
            }
        }
        sum as f32
    }
    fn cost(&self) -> i64 {
        self.subs.iter().map(|s| s.cost()).sum()
    }
}

/// Matches of `req` that `excl` does not match.
pub struct ReqExclScorer<'a> {
    req: BoxScorer<'a>,
    excl: BoxScorer<'a>,
}

impl<'a> ReqExclScorer<'a> {
    pub fn new(req: BoxScorer<'a>, excl: BoxScorer<'a>) -> Self {
        ReqExclScorer { req, excl }
    }
    fn skip_excluded(&mut self, mut doc: i32) -> i32 {
        while doc != NO_MORE_DOCS {
            let mut e = self.excl.doc_id();
            if e < doc {
                e = self.excl.advance(doc);
            }
            if e != doc {
                return doc;
            }
            doc = self.req.next_doc();
        }
        doc
    }
}

impl Scorer for ReqExclScorer<'_> {
    fn doc_id(&self) -> i32 {
        self.req.doc_id()
    }
    fn next_doc(&mut self) -> i32 {
        let d = self.req.next_doc();
        self.skip_excluded(d)
    }
    fn advance(&mut self, target: i32) -> i32 {
        let d = self.req.advance(target);
        self.skip_excluded(d)
    }
    fn score(&mut self) -> f32 {
        self.req.score()
    }
    fn cost(&self) -> i64 {
        self.req.cost()
    }
}

/// Matches of `req`; adds the score of `opt` where it also matches.
pub struct ReqOptScorer<'a> {
    req: BoxScorer<'a>,
    opt: BoxScorer<'a>,
}

impl<'a> ReqOptScorer<'a> {
    pub fn new(req: BoxScorer<'a>, opt: BoxScorer<'a>) -> Self {
        ReqOptScorer { req, opt }
    }
}

impl Scorer for ReqOptScorer<'_> {
    fn doc_id(&self) -> i32 {
        self.req.doc_id()
    }
    fn next_doc(&mut self) -> i32 {
        self.req.next_doc()
    }
    fn advance(&mut self, target: i32) -> i32 {
        self.req.advance(target)
    }
    fn score(&mut self) -> f32 {
        let doc = self.req.doc_id();
        let mut score = self.req.score() as f64;
        let mut o = self.opt.doc_id();
        if o < doc {
            o = self.opt.advance(doc);
        }
        if o == doc {
            score += self.opt.score() as f64;
        }
        score as f32
    }
    fn cost(&self) -> i64 {
        self.req.cost()
    }
}

/// Every document (Lucene's `MatchAllDocsQuery`), constant score.
pub struct MatchAllScorer {
    doc: i32,
    max_doc: i32,
    score: f32,
}

impl MatchAllScorer {
    pub const fn new(max_doc: u32, score: f32) -> Self {
        Self {
            doc: -1,
            max_doc: max_doc as i32,
            score,
        }
    }
}

impl Scorer for MatchAllScorer {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> i32 {
        self.advance(self.doc + 1)
    }
    fn advance(&mut self, target: i32) -> i32 {
        self.doc = if target >= self.max_doc {
            NO_MORE_DOCS
        } else {
            target
        };
        self.doc
    }
    fn score(&mut self) -> f32 {
        self.score
    }
    fn cost(&self) -> i64 {
        self.max_doc as i64
    }
}

/// The inner scorer's matches with a constant score.
pub struct ConstantScorer<'a> {
    inner: BoxScorer<'a>,
    score: f32,
}

impl<'a> ConstantScorer<'a> {
    pub fn new(inner: BoxScorer<'a>, score: f32) -> Self {
        ConstantScorer { inner, score }
    }
}

impl Scorer for ConstantScorer<'_> {
    fn doc_id(&self) -> i32 {
        self.inner.doc_id()
    }
    fn next_doc(&mut self) -> i32 {
        self.inner.next_doc()
    }
    fn advance(&mut self, target: i32) -> i32 {
        self.inner.advance(target)
    }
    fn score(&mut self) -> f32 {
        self.score
    }
    fn cost(&self) -> i64 {
        self.inner.cost()
    }
}

/// Exact phrase: a conjunction over the terms' postings, confirmed by position matching.
/// The phrase frequency (number of match start positions) is scored with BM25 using the sum
/// of the terms' idf, like Lucene.
pub struct PhraseScorer<'a> {
    /// (postings, query position), cheapest first
    postings: Vec<(Box<PostingsEnum<'a>>, i32)>,
    sim: Bm25,
    norms: Option<&'a [u8]>,
    doc: i32,
    freq: u32,
    order: Vec<(u32, usize)>,
    cands: Vec<i32>,
    next: Vec<i32>,
}

impl<'a> PhraseScorer<'a> {
    pub fn new(
        mut postings: Vec<(Box<PostingsEnum<'a>>, i32)>,
        sim: Bm25,
        norms: Option<&'a [u8]>,
    ) -> Self {
        postings.sort_by_key(|(p, _)| p.cost());
        let n = postings.len();
        PhraseScorer {
            postings,
            sim,
            norms,
            doc: -1,
            freq: 0,
            order: Vec::with_capacity(n),
            cands: Vec::new(),
            next: Vec::new(),
        }
    }

    fn conjunction(&mut self, mut doc: i32) -> i32 {
        'outer: loop {
            if doc == NO_MORE_DOCS {
                return doc;
            }
            for i in 1..self.postings.len() {
                let p = &mut self.postings[i].0;
                let mut d = p.doc_id();
                if d < doc {
                    d = p.advance(doc);
                }
                if d > doc {
                    doc = self.postings[0].0.advance(d);
                    continue 'outer;
                }
            }
            return doc;
        }
    }

    /// Number of positions p such that every term i occurs at p + `offset_i`.
    fn phrase_freq(&mut self) -> u32 {
        // Intersect relative positions (position - query offset), rarest term in this doc
        // first, reading each other term's positions only as far as the candidates go and
        // stopping as soon as no candidate is left. Unread positions are skipped lazily by
        // the postings enum.
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend(
            self.postings
                .iter_mut()
                .enumerate()
                .map(|(i, (p, _))| (p.freq(), i)),
        );
        order.sort_unstable();
        let mut cands = std::mem::take(&mut self.cands);
        let mut next = std::mem::take(&mut self.next);
        cands.clear();
        let mut terms = order.iter();
        if let Some(&(freq, i)) = terms.next() {
            let (p, off) = &mut self.postings[i];
            for _ in 0..freq {
                cands.push(p.next_position() as i32 - *off);
            }
        }
        for &(freq, i) in terms {
            if cands.is_empty() {
                break;
            }
            let (p, off) = &mut self.postings[i];
            next.clear();
            let mut c = 0;
            for _ in 0..freq {
                let pos = p.next_position() as i32 - *off;
                while c < cands.len() && cands[c] < pos {
                    c += 1;
                }
                if c == cands.len() {
                    break;
                }
                if cands[c] == pos {
                    next.push(pos);
                    c += 1;
                }
            }
            std::mem::swap(&mut cands, &mut next);
        }
        let freq = cands.len() as u32;
        self.order = order;
        self.cands = cands;
        self.next = next;
        freq
    }

    #[inline]
    fn norm(&self, doc: i32) -> u8 {
        self.norms.map_or(1, |n| n[doc as usize])
    }

    /// Top-k scoring with dynamic pruning (the role of Lucene's phrase impacts): once the
    /// collector has a competitive threshold, skips blocks whose best possible phrase score,
    /// bounded by each term's block impacts (phrase freq <= term freq), cannot compete, and
    /// skips reading positions for docs whose smallest term freq cannot compete.
    pub fn score_top_k(&mut self, c: &mut TopScoreDocCollector, live: Option<&LiveDocs>) {
        let mut block_up_to = -1;
        let mut block_bound = f32::INFINITY;
        let mut doc = self.postings[0].0.next_doc();
        loop {
            doc = self.conjunction(doc);
            if doc == NO_MORE_DOCS {
                return;
            }
            let min_comp = c.min_competitive_score;
            if min_comp > 0.0 {
                if doc > block_up_to {
                    // all enums sit on `doc`, so their current blocks bound (doc, up_to]
                    block_up_to = NO_MORE_DOCS;
                    block_bound = f32::INFINITY;
                    for (pe, _) in &self.postings {
                        block_up_to = block_up_to.min(pe.doc_id_up_to(0));
                        let x = pe.max_impact_x(0, self.sim.norm_inverses());
                        block_bound = block_bound.min(self.sim.score_x(x));
                    }
                }
                if block_bound < min_comp {
                    if block_up_to == NO_MORE_DOCS {
                        return;
                    }
                    doc = self.postings[0].0.advance(block_up_to + 1);
                    continue;
                }
                let min_freq = self
                    .postings
                    .iter_mut()
                    .map(|(pe, _)| pe.freq())
                    .min()
                    .unwrap_or(0);
                if self.sim.score(min_freq as f32, self.norm(doc)) < min_comp {
                    doc = self.postings[0].0.next_doc();
                    continue;
                }
            }
            if live.is_none_or(|l| l.get(doc as u32)) {
                let f = self.phrase_freq();
                if f > 0 {
                    c.collect(doc, self.sim.score(f as f32, self.norm(doc)));
                }
            }
            doc = self.postings[0].0.next_doc();
        }
    }

    fn confirm(&mut self, mut doc: i32) -> i32 {
        loop {
            doc = self.conjunction(doc);
            if doc == NO_MORE_DOCS {
                self.doc = doc;
                return doc;
            }
            let f = self.phrase_freq();
            if f > 0 {
                self.freq = f;
                self.doc = doc;
                return doc;
            }
            doc = self.postings[0].0.next_doc();
        }
    }
}

impl Scorer for PhraseScorer<'_> {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> i32 {
        let d = self.postings[0].0.next_doc();
        self.confirm(d)
    }
    fn advance(&mut self, target: i32) -> i32 {
        let d = self.postings[0].0.advance(target);
        self.confirm(d)
    }
    fn score(&mut self) -> f32 {
        self.sim.score(self.freq as f32, self.norm(self.doc))
    }
    fn cost(&self) -> i64 {
        self.postings[0].0.cost()
    }
}
