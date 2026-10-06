//! `TermScorer` with `MaxScoreCache` and `ImpactsDISI` (block-max skipping for a single term).
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

use crate::codec::forutil::BLOCK_SIZE;
use crate::codec::postings_reader::{DocAndFreqBuffer, NO_MORE_DOCS, PostingsEnum};
use crate::index::LiveDocs;
use crate::sim::Bm25;

struct MaxScoreCache {
    global_max_score: f32,
    cache: [f32; 2],
    cache_up_to: [i32; 2],
}

struct ImpactsDisi {
    min_competitive_score: f32,
    up_to: i32,
    max_score: f32,
}

pub struct TermScorer<'a> {
    pub(crate) pe: Box<PostingsEnum<'a>>,
    sim: Bm25,
    /// `None` when the field omits norms (every doc then scores with norm 1).
    norms: Option<&'a [u8]>,
    msc: MaxScoreCache,
    disi: Option<ImpactsDisi>,
}

impl<'a> TermScorer<'a> {
    pub fn new(
        pe: Box<PostingsEnum<'a>>,
        sim: Bm25,
        norms: Option<&'a [u8]>,
        top_level: bool,
    ) -> Self {
        let global_max_score = sim.score(f32::MAX, 1);
        TermScorer {
            pe,
            sim,
            norms,
            msc: MaxScoreCache {
                global_max_score,
                cache: [0.0; 2],
                cache_up_to: [-1; 2],
            },
            disi: top_level.then_some(ImpactsDisi {
                min_competitive_score: 0.0,
                up_to: NO_MORE_DOCS,
                max_score: f32::MAX,
            }),
        }
    }

    /// Turns on block-max skipping (`ImpactsDISI`), for a scorer that is the whole query.
    pub const fn set_top_level(&mut self) {
        if self.disi.is_none() {
            self.disi = Some(ImpactsDisi {
                min_competitive_score: 0.0,
                up_to: NO_MORE_DOCS,
                max_score: f32::MAX,
            });
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

    #[cfg_attr(feature = "profile", inline(never))]
    #[cfg_attr(not(feature = "profile"), inline)]
    pub fn score(&mut self) -> f32 {
        let doc = self.pe.doc_id();
        let freq = self.pe.freq();
        let norm = self.norms.map_or(1, |n| n[doc as usize]);
        self.sim.score(freq as f32, norm)
    }

    // MaxScoreCache
    pub fn advance_shallow(&mut self, target: i32) -> i32 {
        self.pe.advance_shallow(target);
        self.pe.doc_id_up_to(0)
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn max_score_for_level(&mut self, level: usize) -> f32 {
        let up_to = self.pe.doc_id_up_to(level);
        if self.msc.cache_up_to[level] < up_to {
            let x = self.pe.max_impact_x(level, self.sim.norm_inverses());
            self.msc.cache[level] = self.sim.score_x(x);
            self.msc.cache_up_to[level] = up_to;
        }
        self.msc.cache[level]
    }

    #[cfg_attr(feature = "profile", inline(never))]
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
        if level == -1 {
            -1
        } else {
            self.pe.doc_id_up_to(level as usize)
        }
    }

    // ImpactsDISI
    pub fn set_min_competitive_score(&mut self, min_score: f32) {
        if let Some(d) = &mut self.disi
            && min_score > d.min_competitive_score
        {
            d.min_competitive_score = min_score;
            d.up_to = -1;
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]
    fn advance_target(&mut self, mut target: i32) -> i32 {
        let Some(d) = self.disi.as_ref() else {
            return target;
        };
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
        if let Some(d) = self.disi.as_mut() {
            d.up_to = up_to;
            d.max_score = max_score;
        }
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

    #[cfg_attr(feature = "profile", inline(never))]
    /// Next batch of (doc, score) below `up_to` from the current block, skipping deleted docs
    /// (and, for a top-level scorer, non-competitive blocks).
    pub fn next_docs_and_scores(
        &mut self,
        up_to: i32,
        live: Option<&LiveDocs>,
        buffer: &mut DocAndFreqBuffer,
    ) {
        loop {
            if self.disi.is_some() {
                // ImpactsDISI#ensureCompetitive
                let doc = self.pe.doc_id();
                let t = self.advance_target(doc);
                if t != doc {
                    self.pe.advance(t);
                }
            }
            self.pe.next_postings(up_to, buffer);
            if let (Some(live), true) = (live, buffer.size > 0) {
                let mut k = 0;
                for i in 0..buffer.size {
                    let d = buffer.docs[i];
                    buffer.docs[k] = d;
                    buffer.features[k] = buffer.features[i];
                    k += live.get(d as u32) as usize;
                }
                buffer.size = k;
                if k == 0 {
                    continue; // whole batch deleted, but there may be more docs before up_to
                }
            }
            break;
        }
        // Two passes like BM25Similarity's BulkSimScorer: gather norm inverses, then a pure
        // arithmetic loop that vectorizes. Clamping (a no-op on valid indexes) avoids a
        // bounds-check branch while staying safe on corrupt input.
        let n = buffer.size;
        let mut ni = [0f32; BLOCK_SIZE + 1];
        let cache = self.sim.norm_inverses();
        match self.norms {
            Some(norms) => {
                let last = norms.len() - 1;
                for (x, &d) in ni[..n].iter_mut().zip(&buffer.docs[..n]) {
                    *x = cache[norms[(d as u32 as usize).min(last)] as usize];
                }
            }
            None => ni[..n].fill(cache[1]),
        }
        for (f, &x) in buffer.features[..n].iter_mut().zip(&ni[..n]) {
            *f = self.sim.score_x(*f * x);
        }
    }
}
