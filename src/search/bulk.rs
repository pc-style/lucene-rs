//! Bulk scorers for the common top-k shapes, ported from Lucene 10.5: `BatchScoreBulkScorer`
//! (one term), `MaxScoreBulkScorer` (disjunction of terms) and `BlockMaxConjunctionBulkScorer`
//! (conjunction of terms). All honor deleted documents.
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

use crate::codec::postings_reader::{DocAndFreqBuffer, NO_MORE_DOCS};
use crate::index::LiveDocs;
use crate::pool::{Pooled, recyclable};
use crate::search::collector::TopScoreDocCollector;
use crate::search::term_scorer::TermScorer;
use crate::search::util::{DocAndScoreAccBuffer, sum_upper_bound, unsigned_min};

// ------------------------------------------------------------------------------------------
// BatchScoreBulkScorer (single term query)

#[cfg_attr(feature = "profile", inline(never))]
pub fn score_term(scorer: &mut TermScorer, c: &mut TopScoreDocCollector, live: Option<&LiveDocs>) {
    let mut buffer: Pooled<DocAndFreqBuffer> = Pooled::take();
    scorer.set_min_competitive_score(c.min_competitive_score);
    if scorer.doc_id() < 0 {
        scorer.advance(0);
    }
    loop {
        scorer.next_docs_and_scores(NO_MORE_DOCS, live, &mut buffer);
        if buffer.size == 0 {
            break;
        }
        for i in 0..buffer.size {
            let score = buffer.features[i];
            if score >= c.min_competitive_score {
                c.collect(buffer.docs[i], score);
            }
        }
        scorer.set_min_competitive_score(c.min_competitive_score);
    }
}

// ------------------------------------------------------------------------------------------
// MaxScoreBulkScorer (pure disjunction)

const INNER_WINDOW_SIZE: i32 = 1 << 12;

struct Wrapper {
    doc: i32,
    cost: i64,
    max_window_score: f32,
}

/// `MaxScoreBulkScorer`'s window state. Invariant between uses: `window_matches` and
/// `window_scores` are all zero (the flush step restores this).
struct WindowScratch {
    window_matches: Vec<u64>,
    window_scores: Vec<f64>,
    acc: DocAndScoreAccBuffer,
}

impl Default for WindowScratch {
    fn default() -> Self {
        Self {
            window_matches: vec![0; INNER_WINDOW_SIZE as usize / 64],
            window_scores: vec![0.0; INNER_WINDOW_SIZE as usize],
            acc: DocAndScoreAccBuffer {
                docs: vec![0; INNER_WINDOW_SIZE as usize],
                scores: vec![0.0; INNER_WINDOW_SIZE as usize],
                size: 0,
            },
        }
    }
}
recyclable!(WindowScratch);
recyclable!(DocAndScoreAccBuffer);

pub struct MaxScoreBulkScorer<'a> {
    scorers: Vec<TermScorer<'a>>,
    w: Vec<Wrapper>,
    all: Vec<usize>,
    scratch: Vec<usize>,
    heap: Vec<usize>,
    first_essential: usize,
    first_required: usize,
    next_min_competitive_score: f32,
    max_score_sums: Vec<f64>,
    w2: Pooled<WindowScratch>,
    buf: Pooled<DocAndFreqBuffer>,
    live: Option<&'a LiveDocs>,
    num_outer_windows: i32,
    num_candidates: i32,
    min_window_size: i32,
}

impl<'a> MaxScoreBulkScorer<'a> {
    pub fn new(scorers: Vec<TermScorer<'a>>, live: Option<&'a LiveDocs>) -> Self {
        let n = scorers.len();
        let w = scorers
            .iter()
            .map(|s| Wrapper {
                doc: -1,
                cost: s.cost(),
                max_window_score: 0.0,
            })
            .collect();
        MaxScoreBulkScorer {
            scorers,
            w,
            all: (0..n).collect(),
            scratch: vec![0; n],
            heap: Vec::with_capacity(n),
            first_essential: 0,
            first_required: n,
            next_min_competitive_score: 0.0,
            max_score_sums: vec![0.0; n],
            w2: Pooled::take(),
            buf: Pooled::take(),
            live,
            num_outer_windows: 0,
            num_candidates: 0,
            min_window_size: 1,
        }
    }

    // --- DisiPriorityQueue over wrapper indices, ordered by doc ---
    #[inline]
    fn top(&self) -> usize {
        self.heap[0]
    }
    fn top2(&self) -> Option<usize> {
        match self.heap.len() {
            0 | 1 => None,
            2 => Some(self.heap[1]),
            _ => {
                let (a, b) = (self.heap[1], self.heap[2]);
                Some(if self.w[a].doc <= self.w[b].doc { a } else { b })
            }
        }
    }
    fn update_top(&mut self) -> usize {
        let len = self.heap.len();
        let top = self.heap[0];
        let top_doc = self.w[top].doc;
        let mut slot = 0;
        loop {
            let left = 2 * slot + 1;
            if left >= len {
                break;
            }
            let right = left + 1;
            let child = if right < len && self.w[self.heap[right]].doc < self.w[self.heap[left]].doc
            {
                right
            } else {
                left
            };
            if self.w[self.heap[child]].doc < top_doc {
                self.heap[slot] = self.heap[child];
                slot = child;
            } else {
                break;
            }
        }
        self.heap[slot] = top;
        self.heap[0]
    }
    fn heap_add(&mut self, x: usize) {
        self.heap.push(x);
        let mut i = self.heap.len() - 1;
        while i > 0 {
            let p = (i - 1) / 2;
            if self.w[self.heap[p]].doc > self.w[x].doc {
                self.heap[i] = self.heap[p];
                i = p;
            } else {
                break;
            }
        }
        self.heap[i] = x;
    }

    pub fn score(&mut self, c: &mut TopScoreDocCollector, min: i32, max: i32) {
        let mut outer_min = min;
        'outer: while outer_min < max {
            let mut outer_max = self.compute_outer_window_max(outer_min).min(max);
            loop {
                self.update_max_window_scores(outer_min, outer_max);
                if !self.partition_scorers(c.min_competitive_score) {
                    outer_min = outer_max;
                    continue 'outer;
                }
                let new_outer_max = self.compute_outer_window_max(outer_min);
                if new_outer_max >= outer_max {
                    break;
                }
                outer_max = new_outer_max;
            }
            let mut top = self.top();
            while self.w[top].doc < outer_min {
                self.w[top].doc = self.scorers[top].pe.advance(outer_min);
                top = self.update_top();
            }
            while self.w[top].doc < outer_max {
                self.score_inner_window(c, outer_max);
                top = self.top();
                if c.min_competitive_score >= self.next_min_competitive_score {
                    break;
                }
            }
            outer_min = self.w[top].doc.min(outer_max);
            self.num_outer_windows += 1;
        }
    }

    fn score_inner_window(&mut self, c: &mut TopScoreDocCollector, max: i32) {
        let top = self.top();
        match self.top2() {
            None => self.score_inner_window_single_essential_clause(c, max),
            Some(top2) if self.w[top2].doc - INNER_WINDOW_SIZE / 2 >= self.w[top].doc => {
                let up_to = max.min(self.w[top2].doc);
                self.score_inner_window_single_essential_clause(c, up_to);
            }
            Some(_) => self.score_inner_window_multiple_essential_clauses(c, max),
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn score_inner_window_single_essential_clause(
        &mut self,
        c: &mut TopScoreDocCollector,
        up_to: i32,
    ) {
        let top = self.top();
        loop {
            self.scorers[top].next_docs_and_scores(up_to, self.live, &mut self.buf);
            if self.buf.size == 0 {
                break;
            }
            self.w2.acc.copy_from(&self.buf);
            self.score_non_essential_clauses(c);
        }
        self.w[top].doc = self.scorers[top].doc_id();
        self.update_top();
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn score_inner_window_multiple_essential_clauses(
        &mut self,
        c: &mut TopScoreDocCollector,
        max: i32,
    ) {
        let mut top = self.top();
        let inner_min = self.w[top].doc;
        let inner_max = unsigned_min(max, inner_min.wrapping_add(INNER_WINDOW_SIZE));
        let inner_size = (inner_max - inner_min) as usize;
        // collectEssentialScoresIntoWindow
        loop {
            loop {
                self.scorers[top].next_docs_and_scores(inner_max, self.live, &mut self.buf);
                if self.buf.size == 0 {
                    break;
                }
                for idx in 0..self.buf.size {
                    let i = (self.buf.docs[idx] - inner_min) as usize;
                    self.w2.window_matches[i >> 6] |= 1u64 << (i & 63);
                    self.w2.window_scores[i] += self.buf.features[idx] as f64;
                }
            }
            self.w[top].doc = self.scorers[top].doc_id();
            top = self.update_top();
            if self.w[top].doc >= inner_max {
                break;
            }
        }
        // flushWindowToDocAndScoreAccBuffer
        let mut k = 0;
        for wi in 0..inner_size.div_ceil(64) {
            let mut word = self.w2.window_matches[wi];
            while word != 0 {
                let i = (wi << 6) + word.trailing_zeros() as usize;
                self.w2.acc.docs[k] = inner_min + i as i32;
                self.w2.acc.scores[k] = self.w2.window_scores[i];
                self.w2.window_scores[i] = 0.0;
                k += 1;
                word &= word - 1;
            }
            self.w2.window_matches[wi] = 0;
        }
        self.w2.acc.size = k;
        self.score_non_essential_clauses(c);
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn compute_outer_window_max(&mut self, window_min: i32) -> i32 {
        let n = self.all.len();
        let first_window_lead = self.first_essential.min(n - 1);
        let mut window_max = NO_MORE_DOCS;
        for i in first_window_lead..n {
            let s = self.all[i];
            let target = self.w[s].doc.max(window_min);
            let up_to = self.scorers[s].advance_shallow(target);
            window_max = unsigned_min(window_max, up_to.wrapping_add(1));
        }
        if n - first_window_lead > 1 {
            let threshold = self.num_outer_windows as i64 * 32 * n as i64;
            if (self.num_candidates as i64) < threshold {
                self.min_window_size = (self.min_window_size << 1).min(INNER_WINDOW_SIZE);
            } else {
                self.min_window_size = 1;
            }
            let min_window_max =
                unsigned_min(i32::MAX, window_min.wrapping_add(self.min_window_size));
            window_max = window_max.max(min_window_max);
        }
        window_max
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn update_max_window_scores(&mut self, window_min: i32, window_max: i32) {
        for s in 0..self.scorers.len() {
            if self.w[s].doc < window_max {
                if self.w[s].doc < window_min {
                    self.scorers[s].advance_shallow(window_min);
                }
                self.w[s].max_window_score = self.scorers[s].get_max_score(window_max - 1);
            } else {
                self.w[s].max_window_score = 0.0;
            }
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn score_non_essential_clauses(&mut self, c: &mut TopScoreDocCollector) {
        self.num_candidates = self.num_candidates.wrapping_add(self.w2.acc.size as i32);
        let n = self.all.len();
        for i in (0..self.first_essential).rev() {
            let s = self.all[i];
            self.w2
                .acc
                .filter_competitive_hits(self.max_score_sums[i], c.min_competitive_score, n);
            if i >= self.first_required {
                self.w2.acc.apply_required_clause(&mut self.scorers[s]);
            } else {
                self.w2.acc.apply_optional_clause(&mut self.scorers[s]);
            }
            self.w[s].doc = self.scorers[s].doc_id();
        }
        for i in 0..self.w2.acc.size {
            c.collect(self.w2.acc.docs[i], self.w2.acc.scores[i] as f32);
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn partition_scorers(&mut self, min_competitive: f32) -> bool {
        let n = self.all.len();
        self.scratch.copy_from_slice(&self.all);
        let w = &self.w;
        self.scratch.sort_by(|&a, &b| {
            let ka = w[a].max_window_score as f64 / w[a].cost.max(1) as f64;
            let kb = w[b].max_window_score as f64 / w[b].cost.max(1) as f64;
            ka.total_cmp(&kb)
        });
        let mut max_score_sum = 0f64;
        self.first_essential = 0;
        self.next_min_competitive_score = f32::INFINITY;
        for i in 0..n {
            let s = self.scratch[i];
            let new_sum = max_score_sum + self.w[s].max_window_score as f64;
            let sum_f = sum_upper_bound(new_sum, self.first_essential + 1) as f32;
            if sum_f < min_competitive {
                max_score_sum = new_sum;
                self.all[self.first_essential] = s;
                self.max_score_sums[self.first_essential] = max_score_sum;
                self.first_essential += 1;
            } else {
                self.all[n - 1 - (i - self.first_essential)] = s;
                self.next_min_competitive_score = sum_f.min(self.next_min_competitive_score);
            }
        }
        self.first_required = n;
        if self.first_essential == n {
            return false;
        }
        self.heap.clear();
        for i in self.first_essential..n {
            self.heap_add(self.all[i]);
        }
        if self.first_essential == n - 1 {
            self.first_required = n - 1;
            let mut max_required = self.w[self.all[self.first_essential]].max_window_score as f64;
            while self.first_required > 0 {
                let mut m = max_required;
                if self.first_required > 1 {
                    m += self.max_score_sums[self.first_required - 2];
                }
                if (m as f32) >= min_competitive {
                    break;
                }
                self.first_required -= 1;
                max_required += self.w[self.all[self.first_required]].max_window_score as f64;
            }
        }
        true
    }
}

// ------------------------------------------------------------------------------------------
// BlockMaxConjunctionBulkScorer (pure conjunction)

const MAX_WINDOW_SIZE: i32 = 65536;

pub struct BlockMaxConjunctionBulkScorer<'a> {
    scorers: Vec<TermScorer<'a>>,
    sum_of_other_clauses: Vec<f64>,
    live: Option<&'a LiveDocs>,
    buf: Pooled<DocAndFreqBuffer>,
    acc: Pooled<DocAndScoreAccBuffer>,
}

impl<'a> BlockMaxConjunctionBulkScorer<'a> {
    pub fn new(mut scorers: Vec<TermScorer<'a>>, live: Option<&'a LiveDocs>) -> Self {
        assert!(scorers.len() > 1);
        scorers.sort_by_key(super::term_scorer::TermScorer::cost); // stable, like Arrays.sort on objects
        let n = scorers.len();
        Self {
            scorers,
            sum_of_other_clauses: vec![f64::INFINITY; n],
            live,
            buf: Pooled::take(),
            acc: Pooled::take(),
        }
    }

    fn compute_max_score(&mut self, window_min: i32, window_max: i32) -> f32 {
        for s in &mut self.scorers {
            s.advance_shallow(window_min);
        }
        let mut max_window_score = 0f64;
        for i in 0..self.scorers.len() {
            let m = self.scorers[i].get_max_score(window_max);
            self.sum_of_other_clauses[i] = m as f64;
            max_window_score += m as f64;
        }
        for i in (0..self.sum_of_other_clauses.len() - 1).rev() {
            self.sum_of_other_clauses[i] += self.sum_of_other_clauses[i + 1];
        }
        max_window_score as f32
    }

    pub fn score(&mut self, c: &mut TopScoreDocCollector, min: i32, max: i32) {
        let mut window_min = if c.min_competitive_score == 0.0 {
            self.score_doc_first_until_dynamic_pruning(c, min, max)
        } else {
            self.scorers[0].doc_id().max(min)
        };
        while window_min < max {
            let mut window_max = self.scorers[0].advance_shallow(window_min).min(max - 1);
            window_max = unsigned_min(window_max, window_min.wrapping_add(MAX_WINDOW_SIZE));
            let max_window_score = self.compute_max_score(window_min, window_max);
            self.score_window_score_first(c, window_min, window_max + 1, max_window_score);
            window_min = self.scorers[0].doc_id().max(window_max + 1);
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn score_doc_first_until_dynamic_pruning(
        &mut self,
        c: &mut TopScoreDocCollector,
        min: i32,
        max: i32,
    ) -> i32 {
        let mut doc = self.scorers[0].doc_id();
        if doc < min {
            doc = self.scorers[0].pe.advance(min);
        }
        'outer: while doc < max {
            if self.live.is_some_and(|l| !l.get(doc as u32)) {
                doc = self.scorers[0].pe.next_doc();
                continue;
            }
            for i in 1..self.scorers.len() {
                let mut other = self.scorers[i].doc_id();
                if other < doc {
                    other = self.scorers[i].pe.advance(doc);
                }
                if doc != other {
                    doc = self.scorers[0].pe.advance(other);
                    continue 'outer;
                }
            }
            let mut score = 0f64;
            for s in &mut self.scorers {
                score += s.score() as f64;
            }
            c.collect(doc, score as f32);
            if c.min_competitive_score > 0.0 {
                return self.scorers[0].pe.next_doc();
            }
            doc = self.scorers[0].pe.next_doc();
        }
        doc
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn score_window_score_first(
        &mut self,
        c: &mut TopScoreDocCollector,
        min: i32,
        max: i32,
        max_window_score: f32,
    ) {
        if max_window_score < c.min_competitive_score {
            return;
        }
        if self.scorers[0].doc_id() < min {
            self.scorers[0].pe.advance(min);
        }
        if self.scorers[0].doc_id() >= max {
            return;
        }
        let n = self.scorers.len();
        loop {
            self.scorers[0].next_docs_and_scores(max, self.live, &mut self.buf);
            if self.buf.size == 0 {
                break;
            }
            self.acc.copy_from(&self.buf);
            for i in 1..n {
                let s = self.sum_of_other_clauses[i];
                #[allow(clippy::float_cmp)] // exact comparison, as in Lucene
                if s != self.sum_of_other_clauses[i - 1] {
                    self.acc
                        .filter_competitive_hits(s, c.min_competitive_score, n);
                }
                self.acc.apply_required_clause(&mut self.scorers[i]);
            }
            for i in 0..self.acc.size {
                c.collect(self.acc.docs[i], self.acc.scores[i] as f32);
            }
        }
        let mut max_other_doc = -1;
        for i in 1..n {
            max_other_doc = max_other_doc.max(self.scorers[i].doc_id());
        }
        if self.scorers[0].doc_id() < max_other_doc {
            self.scorers[0].pe.advance(max_other_doc);
        }
    }
}
