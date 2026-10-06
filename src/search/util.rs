//! Helpers shared by the bulk scorers (Lucene's `MathUtil` / `ScorerUtil` / `DocAndScoreAccBuffer`).
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

use crate::codec::postings_reader::DocAndFreqBuffer;
use crate::search::term_scorer::TermScorer;

#[inline]
pub fn sum_upper_bound(sum: f64, num_values: usize) -> f64 {
    if num_values <= 2 {
        return sum;
    }
    let b = (num_values - 1) as f64 * f64::powi(2.0, -52);
    2.0f64.mul_add(b, 1.0) * sum
}

#[inline]
pub const fn unsigned_min(a: i32, b: i32) -> i32 {
    if (a as u32) < (b as u32) { a } else { b }
}

/// Math.ulp(float)
pub const fn ulp(f: f32) -> f32 {
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
pub fn next_up(f: f32) -> f32 {
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

#[cfg_attr(feature = "profile", inline(never))]
pub fn min_required_score(max_remaining: f64, min_competitive: f32, num_scorers: usize) -> f64 {
    let mut m = min_competitive as f64 - max_remaining;
    let sub = ulp(min_competitive) as f64;
    while m > 0.0 && (sum_upper_bound(m + max_remaining, num_scorers) as f32) >= min_competitive {
        m -= sub;
    }
    m
}

/// `DocAndScoreAccBuffer`
#[derive(Default)]
pub struct DocAndScoreAccBuffer {
    pub docs: Vec<i32>,
    pub scores: Vec<f64>,
    pub size: usize,
}

impl DocAndScoreAccBuffer {
    pub(crate) fn copy_from(&mut self, b: &DocAndFreqBuffer) {
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

    #[cfg_attr(feature = "profile", inline(never))]
    pub(crate) fn filter_competitive_hits(
        &mut self,
        max_remaining: f64,
        min_competitive: f32,
        n: usize,
    ) {
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

    #[cfg_attr(feature = "profile", inline(never))]
    pub(crate) fn apply_required_clause(&mut self, s: &mut TermScorer) {
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

    #[cfg_attr(feature = "profile", inline(never))]
    pub(crate) fn apply_optional_clause(&mut self, s: &mut TermScorer) {
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
