//! Term dictionary: per field, sorted terms in blocks of up to 32, prefix-compressed, each entry
//! carrying the term's postings metadata. An in-memory index of every block's first term (the
//! role Lucene's `BlockTree` FST plays) makes a lookup one binary search plus one block scan.
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
use crate::codec::postings_writer::TermMeta;
use crate::codec::store::{In, Out};
use crate::error::{Result, corrupt};

const TERMS_PER_BLOCK: usize = 32;

pub struct FieldTermsWriter {
    has_freq: bool,
    has_pos: bool,
    block: Vec<(Vec<u8>, TermMeta)>,
    index: Vec<(Vec<u8>, u64)>,
    pub num_terms: u64,
}

impl FieldTermsWriter {
    pub const fn new(has_freq: bool, has_pos: bool) -> Self {
        Self {
            has_freq,
            has_pos,
            block: Vec::new(),
            index: Vec::new(),
            num_terms: 0,
        }
    }

    pub fn add(&mut self, term: &[u8], meta: TermMeta, tim: &mut Out) {
        debug_assert!(self.block.last().is_none_or(|(t, _)| t.as_slice() < term));
        self.block.push((term.to_vec(), meta));
        self.num_terms += 1;
        if self.block.len() == TERMS_PER_BLOCK {
            self.flush_block(tim);
        }
    }

    fn flush_block(&mut self, tim: &mut Out) {
        if self.block.is_empty() {
            return;
        }
        self.index.push((self.block[0].0.clone(), tim.len() as u64));
        tim.write_vint(self.block.len() as u32);
        let mut prev: &[u8] = &[];
        for (term, m) in &self.block {
            let shared = prev
                .iter()
                .zip(term.iter())
                .take_while(|(a, b)| a == b)
                .count();
            tim.write_vint(shared as u32);
            tim.write_vint((term.len() - shared) as u32);
            tim.write_bytes(&term[shared..]);
            tim.write_vint(m.doc_freq);
            if self.has_freq {
                tim.write_vlong(m.total_term_freq - m.doc_freq as u64);
            }
            if m.doc_freq == 1 {
                tim.write_vint(m.singleton_doc_id as u32);
            } else {
                tim.write_vlong(m.doc_start_fp);
            }
            if self.has_pos {
                tim.write_vlong(m.pos_start_fp);
                if m.total_term_freq > BLOCK_SIZE as u64 {
                    tim.write_vlong(m.last_pos_block_offset as u64);
                }
            }
            prev = term;
        }
        self.block.clear();
    }

    /// Flushes the last block and writes this field's block index; returns the index offset.
    pub fn finish(mut self, tim: &mut Out) -> u64 {
        self.flush_block(tim);
        let fp = tim.len() as u64;
        tim.write_vint(self.index.len() as u32);
        for (t, block_fp) in &self.index {
            tim.write_vint(t.len() as u32);
            tim.write_bytes(t);
            tim.write_vlong(*block_fp);
        }
        fp
    }
}

/// A field's term dictionary, opened from the segment's `.tim` file.
pub struct FieldTerms {
    has_freq: bool,
    has_pos: bool,
    index_bytes: Vec<u8>,
    /// (offset into `index_bytes`, len, block file pointer)
    index_entries: Vec<(u32, u32, u64)>,
}

impl FieldTerms {
    pub(crate) fn open(tim: &[u8], index_fp: usize, has_freq: bool, has_pos: bool) -> Result<Self> {
        let mut input = In::new(tim, index_fp);
        if input.pos >= tim.len() {
            return Err(corrupt("terms index offset out of range"));
        }
        let n = input.read_vint() as usize;
        let mut index_bytes = Vec::new();
        let mut index_entries = Vec::with_capacity(n);
        for _ in 0..n {
            let len = input.read_vint() as usize;
            let t = tim
                .get(input.pos..input.pos + len)
                .ok_or_else(|| corrupt("truncated terms index"))?;
            let off = index_bytes.len() as u32;
            index_bytes.extend_from_slice(t);
            input.pos += len;
            index_entries.push((off, len as u32, input.read_vlong()));
        }
        Ok(Self {
            has_freq,
            has_pos,
            index_bytes,
            index_entries,
        })
    }

    #[inline]
    fn index_term(&self, i: usize) -> &[u8] {
        let (off, len, _) = self.index_entries[i];
        &self.index_bytes[off as usize..(off + len) as usize]
    }

    fn read_entry(&self, input: &mut In, term: &mut Vec<u8>) -> TermMeta {
        let shared = input.read_vint() as usize;
        let suffix = input.read_vint() as usize;
        term.truncate(shared);
        term.extend_from_slice(&input.data[input.pos..input.pos + suffix]);
        input.pos += suffix;
        let doc_freq = input.read_vint();
        let total_term_freq = if self.has_freq {
            input.read_vlong() + doc_freq as u64
        } else {
            doc_freq as u64
        };
        let (doc_start_fp, singleton_doc_id) = if doc_freq == 1 {
            (0, input.read_vint() as i32)
        } else {
            (input.read_vlong(), -1)
        };
        let (mut pos_start_fp, mut last_pos_block_offset) = (0, -1);
        if self.has_pos {
            pos_start_fp = input.read_vlong();
            if total_term_freq > BLOCK_SIZE as u64 {
                last_pos_block_offset = input.read_vlong() as i64;
            }
        }
        TermMeta {
            doc_freq,
            total_term_freq,
            doc_start_fp,
            pos_start_fp,
            last_pos_block_offset,
            singleton_doc_id,
        }
    }

    pub(crate) fn lookup(&self, tim: &[u8], term: &[u8]) -> Option<TermMeta> {
        // last block whose first term is <= term
        let mut lo = 0;
        let mut hi = self.index_entries.len();
        while lo < hi {
            let mid = usize::midpoint(lo, hi);
            if self.index_term(mid) <= term {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return None;
        }
        let mut input = In::new(tim, self.index_entries[lo - 1].2 as usize);
        let n = input.read_vint();
        let mut buf = Vec::with_capacity(term.len() + 8);
        for _ in 0..n {
            let meta = self.read_entry(&mut input, &mut buf);
            match buf.as_slice().cmp(term) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return Some(meta),
                std::cmp::Ordering::Greater => return None,
            }
        }
        None
    }

    /// Iterates all terms in order.
    pub(crate) const fn cursor<'a>(&'a self, tim: &'a [u8]) -> TermsIter<'a> {
        TermsIter {
            terms: self,
            tim,
            block: 0,
            left_in_block: 0,
            input: In::new(tim, 0),
            term: Vec::new(),
        }
    }
}

pub struct TermsIter<'a> {
    terms: &'a FieldTerms,
    tim: &'a [u8],
    block: usize,
    left_in_block: u32,
    input: In<'a>,
    term: Vec<u8>,
}

impl TermsIter<'_> {
    /// The next term and its metadata (a lending iterator: the slice lives until the next call).
    pub fn next(&mut self) -> Option<(&[u8], TermMeta)> {
        if self.left_in_block == 0 {
            if self.block == self.terms.index_entries.len() {
                return None;
            }
            self.input = In::new(self.tim, self.terms.index_entries[self.block].2 as usize);
            self.left_in_block = self.input.read_vint();
            self.block += 1;
            self.term.clear();
        }
        self.left_in_block -= 1;
        let meta = self.terms.read_entry(&mut self.input, &mut self.term);
        Some((&self.term, meta))
    }
}
