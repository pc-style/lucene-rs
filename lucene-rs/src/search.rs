//! Ports of the Lucene 10.5 top-k execution path for term, conjunction and disjunction queries:
//! TermScorer + MaxScoreCache + ImpactsDISI, BatchScoreBulkScorer, MaxScoreBulkScorer,
//! BlockMaxConjunctionBulkScorer and TopScoreDocCollector.

use crate::index::Index;
use crate::postings_reader::{DocAndFreqBuffer, FreqNormBuffer, NO_MORE_DOCS, PostingsEnum};
use crate::sim::Bm25;

// ------------------------------------------------------------------------------------------
// Math helpers (MathUtil, ScorerUtil)

#[inline]
fn sum_upper_bound(sum: f64, num_values: usize) -> f64 {
    if num_values <= 2 {
        return sum;
    }
    let b = (num_values - 1) as f64 * f64::powi(2.0, -52);
    (1.0 + 2.0 * b) * sum
}

#[inline]
fn unsigned_min(a: i32, b: i32) -> i32 {
    if (a as u32) < (b as u32) { a } else { b }
}

/// Math.ulp(float)
fn ulp(f: f32) -> f32 {
    let e = (f.to_bits() >> 23) & 0xFF;
    if e > 23 {
        f32::from_bits((e - 23) << 23)
    } else if e >= 1 {
        f32::from_bits(1 << (e - 1))
    } else {
        f32::from_bits(1)
    }
}

#[inline]
fn next_up(f: f32) -> f32 {
    if f.is_nan() || f == f32::INFINITY {
        f
    } else if f == 0.0 {
        f32::from_bits(1)
    } else if f > 0.0 {
        f32::from_bits(f.to_bits() + 1)
    } else {
        f32::from_bits(f.to_bits() - 1)
    }
}

fn min_required_score(max_remaining: f64, min_competitive: f32, num_scorers: usize) -> f64 {
    let mut m = min_competitive as f64 - max_remaining;
    let sub = ulp(min_competitive) as f64;
    while m > 0.0 && (sum_upper_bound(m + max_remaining, num_scorers) as f32) >= min_competitive {
        m -= sub;
    }
    m
}

/// DocAndScoreAccBuffer
#[derive(Default)]
pub struct DocAndScoreAccBuffer {
    pub docs: Vec<i32>,
    pub scores: Vec<f64>,
    pub size: usize,
}

impl DocAndScoreAccBuffer {
    fn copy_from(&mut self, b: &DocAndFreqBuffer) {
        if self.docs.len() < b.size {
            self.docs.resize(b.size, 0);
            self.scores.resize(b.size, 0.0);
        }
        self.docs[..b.size].copy_from_slice(&b.docs[..b.size]);
        for i in 0..b.size {
            self.scores[i] = b.features[i] as f64;
        }
        self.size = b.size;
    }

    fn filter_competitive_hits(&mut self, max_remaining: f64, min_competitive: f32, n: usize) {
        let min_req = min_required_score(max_remaining, min_competitive, n);
        if min_req <= 0.0 {
            return;
        }
        // VectorUtil#filterByScore
        let mut k = 0;
        for i in 0..self.size {
            let (d, s) = (self.docs[i], self.scores[i]);
            self.docs[k] = d;
            self.scores[k] = s;
            k += (s >= min_req) as usize;
        }
        self.size = k;
    }

    fn apply_required_clause(&mut self, s: &mut TermScorer) {
        let mut k = 0;
        let mut cur = s.doc_id();
        for i in 0..self.size {
            let target = self.docs[i];
            if cur < target {
                cur = s.pe.advance(target);
            }
            if cur == target {
                self.docs[k] = target;
                self.scores[k] = self.scores[i] + s.score() as f64;
                k += 1;
            }
        }
        self.size = k;
    }

    fn apply_optional_clause(&mut self, s: &mut TermScorer) {
        let mut cur = s.doc_id();
        for i in 0..self.size {
            let target = self.docs[i];
            if cur < target {
                cur = s.pe.advance(target);
            }
            if cur == target {
                self.scores[i] += s.score() as f64;
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// TopScoreDocCollector

#[inline]
fn float_to_sortable_int(f: f32) -> i32 {
    let bits = f.to_bits() as i32;
    bits ^ ((bits >> 31) & 0x7fffffff)
}
#[inline]
fn sortable_int_to_float(i: i32) -> f32 {
    f32::from_bits((i ^ ((i >> 31) & 0x7fffffff)) as u32)
}
#[inline]
fn encode(doc: i32, score: f32) -> i64 {
    ((float_to_sortable_int(score) as i64) << 32) | ((i32::MAX - doc) as i64)
}
#[inline]
fn to_score(code: i64) -> f32 {
    sortable_int_to_float((code as u64 >> 32) as i32)
}
#[inline]
fn to_doc(code: i64) -> i32 {
    i32::MAX - (code as i32)
}

pub struct TopScoreDocCollector {
    heap: Vec<i64>, // binary min-heap
    top_score: f32,
    pub min_competitive_score: f32,
    pub total_hits: u64,
    total_hits_threshold: u64,
}

impl TopScoreDocCollector {
    pub fn new(k: usize, total_hits_threshold: u64) -> Self {
        let least = encode(i32::MAX, f32::NEG_INFINITY);
        Self {
            heap: vec![least; k],
            top_score: f32::NEG_INFINITY,
            min_competitive_score: 0.0,
            total_hits: 0,
            total_hits_threshold,
        }
    }

    #[inline]
    pub fn collect(&mut self, doc: i32, score: f32) {
        self.total_hits += 1;
        if score <= self.top_score {
            if self.total_hits == self.total_hits_threshold + 1 {
                self.update_min_competitive_score();
            }
        } else {
            self.heap[0] = encode(doc, score);
            self.sift_down();
            self.top_score = to_score(self.heap[0]);
            self.update_min_competitive_score();
        }
    }

    fn sift_down(&mut self) {
        let h = &mut self.heap;
        let n = h.len();
        let mut i = 0;
        let v = h[0];
        loop {
            let l = 2 * i + 1;
            if l >= n {
                break;
            }
            let r = l + 1;
            let c = if r < n && h[r] < h[l] { r } else { l };
            if h[c] < v {
                h[i] = h[c];
                i = c;
            } else {
                break;
            }
        }
        h[i] = v;
    }

    #[inline]
    fn update_min_competitive_score(&mut self) {
        if self.total_hits > self.total_hits_threshold {
            let local = next_up(self.top_score);
            if local > self.min_competitive_score {
                self.min_competitive_score = local;
            }
        }
    }

    /// (doc, score) sorted by descending score, ascending doc.
    pub fn top_docs(&self) -> Vec<(i32, f32)> {
        let least = encode(i32::MAX, f32::NEG_INFINITY);
        let mut codes: Vec<i64> = self.heap.iter().copied().filter(|&c| c != least).collect();
        codes.sort_unstable_by(|a, b| b.cmp(a));
        codes.into_iter().map(|c| (to_doc(c), to_score(c))).collect()
    }
}

// ------------------------------------------------------------------------------------------
// TermScorer, MaxScoreCache, ImpactsDISI

struct MaxScoreCache {
    global_max_score: f32,
    cache: [f32; 2],
    cache_up_to: [i32; 2],
    impacts: FreqNormBuffer,
}

struct ImpactsDisi {
    min_competitive_score: f32,
    up_to: i32,
    max_score: f32,
}

pub struct TermScorer<'a> {
    pub pe: Box<PostingsEnum<'a>>,
    sim: Bm25,
    norms: &'a [u8],
    msc: MaxScoreCache,
    disi: Option<ImpactsDisi>,
}

impl<'a> TermScorer<'a> {
    pub fn new(pe: Box<PostingsEnum<'a>>, sim: Bm25, norms: &'a [u8], top_level: bool) -> Self {
        let global_max_score = sim.score(f32::MAX, 1);
        TermScorer {
            pe,
            sim,
            norms,
            msc: MaxScoreCache {
                global_max_score,
                cache: [0.0; 2],
                cache_up_to: [-1; 2],
                impacts: FreqNormBuffer { freqs: Vec::new(), norms: Vec::new() },
            },
            disi: top_level.then_some(ImpactsDisi {
                min_competitive_score: 0.0,
                up_to: NO_MORE_DOCS,
                max_score: f32::MAX,
            }),
        }
    }

    #[inline]
    pub fn doc_id(&self) -> i32 {
        self.pe.doc_id()
    }

    #[inline]
    pub fn cost(&self) -> i64 {
        self.pe.cost()
    }

    #[inline]
    pub fn score(&mut self) -> f32 {
        let doc = self.pe.doc_id();
        let freq = self.pe.freq();
        self.sim.score(freq as f32, self.norms[doc as usize])
    }

    // MaxScoreCache
    pub fn advance_shallow(&mut self, target: i32) -> i32 {
        self.pe.advance_shallow(target);
        self.pe.doc_id_up_to(0)
    }

    fn max_score_for_level(&mut self, level: usize) -> f32 {
        let up_to = self.pe.doc_id_up_to(level);
        if self.msc.cache_up_to[level] < up_to {
            self.pe.impacts(level, &mut self.msc.impacts);
            let mut max = 0f32;
            for (&f, &n) in self.msc.impacts.freqs.iter().zip(self.msc.impacts.norms.iter()) {
                max = max.max(self.sim.score(f as f32, n));
            }
            self.msc.cache[level] = max;
            self.msc.cache_up_to[level] = up_to;
        }
        self.msc.cache[level]
    }

    pub fn get_max_score(&mut self, up_to: i32) -> f32 {
        for level in 0..self.pe.num_levels() {
            if up_to <= self.pe.doc_id_up_to(level) {
                return self.max_score_for_level(level);
            }
        }
        self.msc.global_max_score
    }

    fn get_skip_up_to(&mut self, min_score: f32) -> i32 {
        let num_levels = self.pe.num_levels();
        let mut level = num_levels as i32 - 1;
        for l in 0..num_levels {
            if self.max_score_for_level(l) >= min_score {
                level = l as i32 - 1;
                break;
            }
        }
        if level == -1 { -1 } else { self.pe.doc_id_up_to(level as usize) }
    }

    // ImpactsDISI
    pub fn set_min_competitive_score(&mut self, min_score: f32) {
        if let Some(d) = &mut self.disi {
            if min_score > d.min_competitive_score {
                d.min_competitive_score = min_score;
                d.up_to = -1;
            }
        }
    }

    fn advance_target(&mut self, mut target: i32) -> i32 {
        let d = self.disi.as_ref().unwrap();
        if target <= d.up_to {
            return target;
        }
        let min_comp = d.min_competitive_score;
        let mut up_to = self.advance_shallow(target);
        let mut max_score = self.max_score_for_level(0);
        let result = loop {
            if max_score >= min_comp {
                break target;
            }
            if up_to == NO_MORE_DOCS {
                break NO_MORE_DOCS;
            }
            let skip_up_to = self.get_skip_up_to(min_comp);
            if skip_up_to == -1 {
                target = up_to + 1;
            } else if skip_up_to == NO_MORE_DOCS {
                break NO_MORE_DOCS;
            } else {
                target = skip_up_to + 1;
            }
            up_to = self.advance_shallow(target);
            max_score = self.max_score_for_level(0);
        };
        let d = self.disi.as_mut().unwrap();
        d.up_to = up_to;
        d.max_score = max_score;
        result
    }

    /// scorer.iterator().advance(target)
    pub fn advance(&mut self, target: i32) -> i32 {
        if self.disi.is_some() {
            let t = self.advance_target(target);
            self.pe.advance(t)
        } else {
            self.pe.advance(target)
        }
    }

    pub fn next_docs_and_scores(&mut self, up_to: i32, buffer: &mut DocAndFreqBuffer) {
        if self.disi.is_some() {
            // ImpactsDISI#ensureCompetitive
            let doc = self.pe.doc_id();
            let t = self.advance_target(doc);
            if t != doc {
                self.pe.advance(t);
            }
        }
        self.pe.next_postings(up_to, buffer);
        for i in 0..buffer.size {
            let norm = self.norms[buffer.docs[i] as usize];
            buffer.features[i] = self.sim.score(buffer.features[i], norm);
        }
    }
}

// ------------------------------------------------------------------------------------------
// BatchScoreBulkScorer (single term query)

pub fn score_term(scorer: &mut TermScorer, c: &mut TopScoreDocCollector) {
    let mut buffer = DocAndFreqBuffer::default();
    scorer.set_min_competitive_score(c.min_competitive_score);
    if scorer.doc_id() < 0 {
        scorer.advance(0);
    }
    loop {
        scorer.next_docs_and_scores(NO_MORE_DOCS, &mut buffer);
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
    window_matches: Vec<u64>,
    window_scores: Vec<f64>,
    buf: DocAndFreqBuffer,
    acc: DocAndScoreAccBuffer,
    num_outer_windows: i32,
    num_candidates: i32,
    min_window_size: i32,
}

impl<'a> MaxScoreBulkScorer<'a> {
    pub fn new(scorers: Vec<TermScorer<'a>>) -> Self {
        let n = scorers.len();
        let w = scorers.iter().map(|s| Wrapper { doc: -1, cost: s.cost(), max_window_score: 0.0 }).collect();
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
            window_matches: vec![0; INNER_WINDOW_SIZE as usize / 64],
            window_scores: vec![0.0; INNER_WINDOW_SIZE as usize],
            buf: DocAndFreqBuffer::default(),
            acc: DocAndScoreAccBuffer {
                docs: vec![0; INNER_WINDOW_SIZE as usize],
                scores: vec![0.0; INNER_WINDOW_SIZE as usize],
                size: 0,
            },
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
        let n = self.heap.len();
        let v = self.heap[0];
        let vd = self.w[v].doc;
        let mut i = 0;
        loop {
            let l = 2 * i + 1;
            if l >= n {
                break;
            }
            let r = l + 1;
            let c = if r < n && self.w[self.heap[r]].doc < self.w[self.heap[l]].doc { r } else { l };
            if self.w[self.heap[c]].doc < vd {
                self.heap[i] = self.heap[c];
                i = c;
            } else {
                break;
            }
        }
        self.heap[i] = v;
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
                self.score_inner_window_single_essential_clause(c, up_to)
            }
            Some(_) => self.score_inner_window_multiple_essential_clauses(c, max),
        }
    }

    fn score_inner_window_single_essential_clause(&mut self, c: &mut TopScoreDocCollector, up_to: i32) {
        let top = self.top();
        loop {
            self.scorers[top].next_docs_and_scores(up_to, &mut self.buf);
            if self.buf.size == 0 {
                break;
            }
            self.acc.copy_from(&self.buf);
            self.score_non_essential_clauses(c);
        }
        self.w[top].doc = self.scorers[top].doc_id();
        self.update_top();
    }

    fn score_inner_window_multiple_essential_clauses(&mut self, c: &mut TopScoreDocCollector, max: i32) {
        let mut top = self.top();
        let inner_min = self.w[top].doc;
        let inner_max = unsigned_min(max, inner_min.wrapping_add(INNER_WINDOW_SIZE));
        let inner_size = (inner_max - inner_min) as usize;
        // collectEssentialScoresIntoWindow
        loop {
            loop {
                self.scorers[top].next_docs_and_scores(inner_max, &mut self.buf);
                if self.buf.size == 0 {
                    break;
                }
                for idx in 0..self.buf.size {
                    let i = (self.buf.docs[idx] - inner_min) as usize;
                    self.window_matches[i >> 6] |= 1u64 << (i & 63);
                    self.window_scores[i] += self.buf.features[idx] as f64;
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
            let mut word = self.window_matches[wi];
            while word != 0 {
                let i = (wi << 6) + word.trailing_zeros() as usize;
                self.acc.docs[k] = inner_min + i as i32;
                self.acc.scores[k] = self.window_scores[i];
                self.window_scores[i] = 0.0;
                k += 1;
                word &= word - 1;
            }
            self.window_matches[wi] = 0;
        }
        self.acc.size = k;
        self.score_non_essential_clauses(c);
    }

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
            let min_window_max = unsigned_min(i32::MAX, window_min.wrapping_add(self.min_window_size));
            window_max = window_max.max(min_window_max);
        }
        window_max
    }

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

    fn score_non_essential_clauses(&mut self, c: &mut TopScoreDocCollector) {
        self.num_candidates = self.num_candidates.wrapping_add(self.acc.size as i32);
        let n = self.all.len();
        for i in (0..self.first_essential).rev() {
            let s = self.all[i];
            self.acc.filter_competitive_hits(self.max_score_sums[i], c.min_competitive_score, n);
            if i >= self.first_required {
                self.acc.apply_required_clause(&mut self.scorers[s]);
            } else {
                self.acc.apply_optional_clause(&mut self.scorers[s]);
            }
            self.w[s].doc = self.scorers[s].doc_id();
        }
        for i in 0..self.acc.size {
            c.collect(self.acc.docs[i], self.acc.scores[i] as f32);
        }
    }

    fn partition_scorers(&mut self, min_competitive: f32) -> bool {
        let n = self.all.len();
        self.scratch.copy_from_slice(&self.all);
        let w = &self.w;
        self.scratch.sort_by(|&a, &b| {
            let ka = w[a].max_window_score as f64 / w[a].cost.max(1) as f64;
            let kb = w[b].max_window_score as f64 / w[b].cost.max(1) as f64;
            ka.partial_cmp(&kb).unwrap()
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
    buf: DocAndFreqBuffer,
    acc: DocAndScoreAccBuffer,
}

impl<'a> BlockMaxConjunctionBulkScorer<'a> {
    pub fn new(mut scorers: Vec<TermScorer<'a>>) -> Self {
        assert!(scorers.len() > 1);
        scorers.sort_by_key(|s| s.cost()); // stable, like Arrays.sort on objects
        let n = scorers.len();
        Self {
            scorers,
            sum_of_other_clauses: vec![f64::INFINITY; n],
            buf: DocAndFreqBuffer::default(),
            acc: DocAndScoreAccBuffer::default(),
        }
    }

    fn compute_max_score(&mut self, window_min: i32, window_max: i32) -> f32 {
        for s in self.scorers.iter_mut() {
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
        let mut window_min = self.scorers[0].doc_id().max(min);
        if c.min_competitive_score == 0.0 {
            window_min = self.score_doc_first_until_dynamic_pruning(c, min, max);
        }
        while window_min < max {
            let mut window_max = self.scorers[0].advance_shallow(window_min).min(max - 1);
            window_max = unsigned_min(window_max, window_min.wrapping_add(MAX_WINDOW_SIZE));
            let max_window_score = self.compute_max_score(window_min, window_max);
            self.score_window_score_first(c, window_min, window_max + 1, max_window_score);
            window_min = self.scorers[0].doc_id().max(window_max + 1);
        }
    }

    fn score_doc_first_until_dynamic_pruning(&mut self, c: &mut TopScoreDocCollector, min: i32, max: i32) -> i32 {
        let mut doc = self.scorers[0].doc_id();
        if doc < min {
            doc = self.scorers[0].pe.advance(min);
        }
        'outer: while doc < max {
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
            for s in self.scorers.iter_mut() {
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

    fn score_window_score_first(&mut self, c: &mut TopScoreDocCollector, min: i32, max: i32, max_window_score: f32) {
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
            self.scorers[0].next_docs_and_scores(max, &mut self.buf);
            if self.buf.size == 0 {
                break;
            }
            self.acc.copy_from(&self.buf);
            for i in 1..n {
                let s = self.sum_of_other_clauses[i];
                if s != self.sum_of_other_clauses[i - 1] {
                    self.acc.filter_competitive_hits(s, c.min_competitive_score, n);
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

// ------------------------------------------------------------------------------------------
// Query entry point (what IndexSearcher#search(query, 10) does for our three query shapes)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Term,
    And,
    Or,
}

pub struct TopDocs {
    pub total_hits: u64,
    pub hits: Vec<(i32, f32)>,
}

pub fn search(index: &Index, kind: Kind, terms: &[&str], k: usize) -> TopDocs {
    let mut c = TopScoreDocCollector::new(k, 1000);
    let mut scorers = Vec::with_capacity(terms.len());
    for t in terms {
        match index.lookup(t.as_bytes()) {
            Some(meta) => {
                let sim = Bm25::new(1.0, meta.doc_freq as u64, index.doc_count, index.sum_total_term_freq);
                let pe = PostingsEnum::new(&index.doc, &meta);
                let top_level = kind == Kind::Term;
                scorers.push(TermScorer::new(pe, sim, &index.norms, top_level));
            }
            None if kind == Kind::And => return TopDocs { total_hits: 0, hits: vec![] },
            None => {}
        }
    }
    match (kind, scorers.len()) {
        (_, 0) => {}
        (Kind::Term, _) | (_, 1) => score_term(&mut scorers[0], &mut c),
        (Kind::And, _) => BlockMaxConjunctionBulkScorer::new(scorers).score(&mut c, 0, NO_MORE_DOCS),
        (Kind::Or, _) => MaxScoreBulkScorer::new(scorers).score(&mut c, 0, NO_MORE_DOCS),
    }
    TopDocs { total_hits: c.total_hits, hits: c.top_docs() }
}
