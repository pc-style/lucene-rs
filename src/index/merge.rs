//! Merge selection (port of Lucene's `LogMergePolicy` level logic, counting documents) and
//! segment merging. Merges combine adjacent segments, so document order is preserved.

use crate::codec::postings_reader::NO_MORE_DOCS;
use crate::codec::postings_writer::TermMeta;
use crate::document::IndexOptions;
use crate::error::{Error, Result};
use crate::index::field_infos::{FieldInfo, FieldInfos};
use crate::index::segment::{SegmentInfo, SegmentReader};
use crate::index::segment_writer::SegmentWriter;
use crate::index::stored::StoredFieldsWriter;
use crate::index::terms::TermsIter;
use crate::num::usize_from;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::path::Path;

const LEVEL_LOG_SPAN: f64 = 0.75;

/// Merges segments of roughly equal size in groups of `merge_factor` (Lucene's
/// `LogDocMergePolicy`).
#[derive(Clone, Debug)]
pub struct LogMergePolicy {
    /// How many segments of one size level are merged together.
    pub merge_factor: u32,
    /// Segments smaller than this (in live docs) all count as the lowest level.
    pub min_merge_docs: u32,
    /// Segments this large are never merged by the policy (`force_merge` still merges them).
    pub max_merge_docs: u32,
}

impl Default for LogMergePolicy {
    fn default() -> Self {
        Self {
            merge_factor: 10,
            min_merge_docs: 1000,
            max_merge_docs: u32::MAX,
        }
    }
}

impl LogMergePolicy {
    /// `LogMergePolicy#findMerges` over segment sizes (live docs), oldest first.
    #[must_use]
    pub fn find_merges(&self, sizes: &[u32]) -> Vec<Range<usize>> {
        let mf = self.merge_factor.max(2);
        let norm = f64::from(mf).ln();
        let level = |size: u32| f64::from(size.max(1)).ln() / norm;
        let level_floor = if self.min_merge_docs == 0 {
            0.0
        } else {
            level(self.min_merge_docs)
        };
        let levels: Vec<f64> = sizes.iter().map(|&s| level(s)).collect();
        let step = usize_from(mf);
        let mut merges = Vec::new();
        let mut start = 0;
        while let Some(tail) = levels.get(start..).filter(|t| !t.is_empty()) {
            let max_level = tail.iter().copied().fold(f64::MIN, f64::max);
            let bottom = if max_level <= level_floor {
                -1.0
            } else {
                let b = max_level - LEVEL_LOG_SPAN;
                if b < level_floor { level_floor } else { b }
            };
            // segments start..=upto form this level (the max-level segment always qualifies)
            let upto = start.saturating_add(tail.iter().rposition(|&l| l >= bottom).unwrap_or(0));
            let mut from = start;
            while let Some(end) = from
                .checked_add(step)
                .filter(|&e| e <= upto.saturating_add(1))
            {
                if sizes
                    .get(from..end)
                    .is_some_and(|g| g.iter().all(|&s| s < self.max_merge_docs))
                {
                    merges.push(from..end);
                }
                from = end;
            }
            start = upto.saturating_add(1);
        }
        merges
    }
}

type TermHeap = BinaryHeap<Reverse<(Vec<u8>, usize)>>;

/// One segment's position in a field's term dictionary during a merge.
struct TermCursor<'a> {
    iter: TermsIter<'a>,
    meta: TermMeta,
}

impl TermCursor<'_> {
    /// Moves to the next term, pushing it onto the heap; returns false at the end.
    fn advance(&mut self, seg: usize, heap: &mut TermHeap) -> bool {
        match self.iter.next() {
            Some((t, m)) => {
                heap.push(Reverse((t.to_vec(), seg)));
                self.meta = m;
                true
            }
            None => false,
        }
    }
}

/// For each segment, old doc ID -> new doc ID (`None` if deleted); plus the merged doc count.
fn doc_maps(readers: &[&SegmentReader]) -> Result<(Vec<Vec<Option<u32>>>, u32)> {
    let mut next = 0u32;
    let mut maps = Vec::with_capacity(readers.len());
    for r in readers {
        let mut m = Vec::with_capacity(usize_from(r.max_doc()));
        for d in 0..r.max_doc() {
            if r.live_docs().is_none_or(|l| l.get(d)) {
                m.push(Some(next));
                next = next
                    .checked_add(1)
                    .ok_or_else(|| Error::IllegalArgument("merged segment too large".into()))?;
            } else {
                m.push(None);
            }
        }
        maps.push(m);
    }
    Ok((maps, next))
}

fn merged_norms(
    readers: &[&SegmentReader],
    maps: &[Vec<Option<u32>>],
    field: u32,
    max_doc: u32,
) -> Vec<u8> {
    let mut norms = vec![0u8; usize_from(max_doc)];
    for (r, map) in readers.iter().zip(maps) {
        let Some(src) = r.norms(field) else { continue };
        for (&norm, new) in src.iter().zip(map) {
            if let Some(slot) = new.and_then(|n| norms.get_mut(usize_from(n))) {
                *slot = norm;
            }
        }
    }
    norms
}

/// Copies one term's postings from one segment, remapping docs and skipping deleted ones.
fn copy_postings(
    w: &mut SegmentWriter,
    reader: &SegmentReader,
    info: &FieldInfo,
    meta: &TermMeta,
    map: &[Option<u32>],
) {
    let has_pos = info.index_options.has_positions();
    let mut pe = reader.postings(info, meta, has_pos);
    loop {
        let d = pe.next_doc();
        if d == NO_MORE_DOCS {
            break;
        }
        let new = u32::try_from(d)
            .ok()
            .and_then(|d| map.get(usize_from(d)).copied().flatten());
        let Some(new) = new else { continue };
        let freq = pe.freq();
        w.start_doc(new, freq);
        if has_pos {
            for _ in 0..freq {
                w.add_position(pe.next_position());
            }
        }
        w.finish_doc();
    }
}

/// Merges `readers` (in order, skipping deleted docs) into segment `name`. Returns `None` if
/// every document was deleted.
pub(crate) fn merge_segments(
    dir: &Path,
    readers: &[&SegmentReader],
    name: String,
    infos: &FieldInfos,
) -> Result<Option<SegmentInfo>> {
    let (maps, max_doc) = doc_maps(readers)?;
    if max_doc == 0 {
        return Ok(None);
    }
    let mut w = SegmentWriter::new(name, max_doc, infos.clone());
    for info in infos.iter() {
        if info.index_options == IndexOptions::None {
            continue;
        }
        let mut cursors: Vec<Option<TermCursor>> = readers
            .iter()
            .map(|r| {
                r.terms_iter(info.number).map(|iter| TermCursor {
                    iter,
                    meta: TermMeta::default(),
                })
            })
            .collect();
        if cursors.iter().all(Option::is_none) {
            continue;
        }
        let norms = info
            .has_norms
            .then(|| merged_norms(readers, &maps, info.number, max_doc));
        w.start_field(info.number, norms.as_deref())?;
        let mut heap = TermHeap::new();
        for (seg, c) in cursors.iter_mut().enumerate() {
            if c.as_mut().is_some_and(|cur| !cur.advance(seg, &mut heap)) {
                *c = None;
            }
        }
        let mut same: Vec<usize> = Vec::new();
        while let Some(Reverse((term, first))) = heap.pop() {
            same.clear();
            same.push(first);
            while let Some(Reverse((_, seg))) = heap.peek().filter(|Reverse((t, _))| *t == term) {
                same.push(*seg);
                heap.pop();
            }
            same.sort_unstable();
            w.start_term();
            for &seg in &same {
                if let (Some(Some(cur)), Some(r), Some(map)) =
                    (cursors.get(seg), readers.get(seg), maps.get(seg))
                {
                    copy_postings(&mut w, r, info, &cur.meta, map);
                }
            }
            w.finish_term(&term);
            for &seg in &same {
                if let Some(slot) = cursors.get_mut(seg)
                    && slot
                        .as_mut()
                        .is_some_and(|cur| !cur.advance(seg, &mut heap))
                {
                    *slot = None;
                }
            }
        }
        w.finish_field();
    }
    let mut stored = StoredFieldsWriter::new();
    for (r, map) in readers.iter().zip(&maps) {
        for (d, new) in (0..r.max_doc()).zip(map) {
            if new.is_some() {
                stored.add_raw(r.stored_raw(d)?);
            }
        }
    }
    w.set_stored(stored);
    Ok(Some(w.finish(dir)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_ten_small_segments() {
        let p = LogMergePolicy::default();
        assert_eq!(p.find_merges(&[10; 9]), Vec::<Range<usize>>::new());
        assert_eq!(p.find_merges(&[10; 10]), vec![0..10]);
        assert_eq!(p.find_merges(&[10; 25]), vec![0..10, 10..20]);
    }

    #[test]
    fn big_segment_is_its_own_level() {
        let p = LogMergePolicy::default();
        let mut sizes = vec![1_000_000];
        sizes.extend([500; 10]);
        assert_eq!(p.find_merges(&sizes), vec![1..11]);
    }
}
