//! `TopScoreDocCollector`: keeps the best `k` (score, doc) pairs across segments, counts hits
//! exactly up to a threshold, then publishes a minimum competitive score that lets scorers skip.
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

#[inline]
const fn float_to_sortable_int(f: f32) -> i32 {
    let bits = f.to_bits() as i32;
    bits ^ ((bits >> 31) & 0x7fff_ffff)
}
#[inline]
const fn sortable_int_to_float(i: i32) -> f32 {
    f32::from_bits((i ^ ((i >> 31) & 0x7fff_ffff)) as u32)
}
#[inline]
const fn encode(doc: i32, score: f32) -> i64 {
    ((float_to_sortable_int(score) as i64) << 32) | ((i32::MAX - doc) as i64)
}
#[inline]
const fn to_score(code: i64) -> f32 {
    sortable_int_to_float((code as u64 >> 32) as i32)
}
#[inline]
const fn to_doc(code: i64) -> i32 {
    i32::MAX - (code as i32)
}

use crate::search::util::next_up;

pub struct TopScoreDocCollector {
    heap: Vec<i64>, // binary min-heap of encoded (score, global doc)
    top_score: f32,
    /// Docs scoring below this cannot enter the top k (scorers may skip them).
    pub min_competitive_score: f32,
    pub total_hits: u64,
    total_hits_threshold: u64,
    /// Whether `total_hits` became a lower bound (scorers were allowed to skip).
    pub(crate) hits_lower_bound: bool,
    doc_base: i32,
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
            hits_lower_bound: false,
            doc_base: 0,
        }
    }

    /// `getLeafCollector` + `setScorer`: switches to a segment starting at `doc_base`.
    pub fn begin_leaf(&mut self, doc_base: u32) {
        self.doc_base = doc_base as i32;
        self.min_competitive_score = 0.0;
        self.update_min_competitive_score();
    }

    #[cfg_attr(feature = "profile", inline(never))]
    #[cfg_attr(not(feature = "profile"), inline)]
    pub fn collect(&mut self, doc: i32, score: f32) {
        self.total_hits += 1;
        if score <= self.top_score {
            if self.total_hits == self.total_hits_threshold + 1 {
                self.update_min_competitive_score();
            }
        } else {
            self.heap[0] = encode(doc + self.doc_base, score);
            self.sift_down();
            self.top_score = to_score(self.heap[0]);
            self.update_min_competitive_score();
        }
    }

    fn sift_down(&mut self) {
        let heap = &mut self.heap;
        let len = heap.len();
        let top = heap[0];
        let mut slot = 0;
        loop {
            let left = 2 * slot + 1;
            if left >= len {
                break;
            }
            let right = left + 1;
            let child = if right < len && heap[right] < heap[left] {
                right
            } else {
                left
            };
            if heap[child] < top {
                heap[slot] = heap[child];
                slot = child;
            } else {
                break;
            }
        }
        heap[slot] = top;
    }

    #[inline]
    fn update_min_competitive_score(&mut self) {
        if self.total_hits > self.total_hits_threshold {
            let local = next_up(self.top_score);
            if local > self.min_competitive_score {
                self.min_competitive_score = local;
                self.hits_lower_bound = true;
            }
        }
    }

    /// (doc, score) sorted by descending score, ascending doc.
    pub fn top_docs(&self) -> Vec<(i32, f32)> {
        let least = encode(i32::MAX, f32::NEG_INFINITY);
        let mut codes: Vec<i64> = self.heap.iter().copied().filter(|&c| c != least).collect();
        codes.sort_unstable_by(|a, b| b.cmp(a));
        codes
            .into_iter()
            .map(|c| (to_doc(c), to_score(c)))
            .collect()
    }
}
