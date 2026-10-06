//! Port of Lucene104PostingsReader.BlockPostingsEnum for the configuration used by top-k
//! scoring queries: needsFreq = true, needsImpacts = true, no positions.

use crate::codec::forutil::{self, BLOCK_SIZE};
use crate::codec::postings_writer::{LEVEL1_NUM_DOCS, TermMeta};
use crate::codec::store::In;
use crate::pool::{Pooled, recyclable};

pub const NO_MORE_DOCS: i32 = i32::MAX;
const LEVEL1: i32 = LEVEL1_NUM_DOCS as i32;
const BLOCK: i32 = BLOCK_SIZE as i32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Packed,
    Unary,
}

/// DocAndFloatFeatureBuffer
pub struct DocAndFreqBuffer {
    pub docs: Vec<i32>,
    pub features: Vec<f32>,
    pub size: usize,
}

impl Default for DocAndFreqBuffer {
    fn default() -> Self {
        Self { docs: vec![0; BLOCK_SIZE + 1], features: vec![0.0; BLOCK_SIZE + 1], size: 0 }
    }
}

/// Lucene's Impacts.getImpacts(level) result.
pub struct FreqNormBuffer {
    pub freqs: Vec<u32>,
    pub norms: Vec<u8>,
}

/// Per-enum block buffers. Every element is written before it is read, so recycled buffers
/// need no clearing.
struct Buffers {
    doc_buffer: [i32; BLOCK_SIZE + 1],
    freq_buffer: [u32; BLOCK_SIZE],
    scratch: [u32; BLOCK_SIZE],
    doc_bitset: [u64; BLOCK_SIZE / 2],
    doc_cumulative_word_pop_counts: [i32; BLOCK_SIZE / 2],
}

impl Default for Buffers {
    fn default() -> Self {
        Buffers {
            doc_buffer: [0; BLOCK_SIZE + 1],
            freq_buffer: [0; BLOCK_SIZE],
            scratch: [0; BLOCK_SIZE],
            doc_bitset: [0; BLOCK_SIZE / 2],
            doc_cumulative_word_pop_counts: [0; BLOCK_SIZE / 2],
        }
    }
}
recyclable!(Buffers);
recyclable!(DocAndFreqBuffer);

pub struct PostingsEnum<'a> {
    data: &'a [u8],
    pos: usize,
    b: Pooled<Buffers>,
    doc_bitset_base: i32,
    encoding: Encoding,

    doc: i32,
    prev_doc_id: i32,
    doc_count_left: i32,
    freq_fp: usize,
    level0_last_doc_id: i32,
    level0_doc_end_fp: usize,
    level1_last_doc_id: i32,
    level1_doc_end_fp: usize,
    level1_doc_count_upto: i32,
    doc_buffer_size: usize,
    doc_buffer_upto: usize,
    needs_refilling: bool,

    doc_freq: i32,
    total_term_freq: u64,
    singleton_doc_id: i32,

    level0_impacts: (usize, usize),
    level1_impacts: (usize, usize),
}

const NO_FREQ_FP: usize = usize::MAX;

impl<'a> PostingsEnum<'a> {
    /// BlockPostingsEnum#reset
    pub fn new(data: &'a [u8], meta: &TermMeta) -> Box<Self> {
        let mut e = Box::new(PostingsEnum {
            data,
            pos: 0,
            b: Pooled::take(),
            doc_bitset_base: 0,
            encoding: Encoding::Packed,
            doc: -1,
            prev_doc_id: -1,
            doc_count_left: meta.doc_freq as i32,
            freq_fp: NO_FREQ_FP,
            level0_last_doc_id: -1,
            level0_doc_end_fp: 0,
            level1_last_doc_id: -1,
            level1_doc_end_fp: 0,
            level1_doc_count_upto: 0,
            doc_buffer_size: BLOCK_SIZE,
            doc_buffer_upto: BLOCK_SIZE,
            needs_refilling: false,
            doc_freq: meta.doc_freq as i32,
            total_term_freq: meta.total_term_freq,
            singleton_doc_id: meta.singleton_doc_id,
            level0_impacts: (0, 0),
            level1_impacts: (0, 0),
        });
        if e.doc_freq < LEVEL1 {
            e.level1_last_doc_id = NO_MORE_DOCS;
            if e.doc_freq > 1 {
                e.pos = meta.doc_start_fp as usize;
            }
        } else {
            e.level1_last_doc_id = -1;
            e.level1_doc_end_fp = meta.doc_start_fp as usize;
        }
        e
    }

    #[inline]
    fn input(&self) -> In<'a> {
        In::new(self.data, self.pos)
    }

    #[inline]
    pub fn doc_id(&self) -> i32 {
        self.doc
    }

    #[inline]
    pub fn cost(&self) -> i64 {
        self.doc_freq as i64
    }

    #[cfg_attr(feature = "profile", inline(never))]

    #[cfg_attr(not(feature = "profile"), inline)]
    pub fn freq(&mut self) -> u32 {
        if self.freq_fp != NO_FREQ_FP {
            let mut input = In::new(self.data, self.freq_fp);
            forutil::pfor_decode(&mut input, &mut self.b.freq_buffer);
            self.pos = input.pos;
            self.freq_fp = NO_FREQ_FP;
        }
        self.b.freq_buffer[self.doc_buffer_upto - 1]
    }

    fn refill_full_block(&mut self) {
        let mut input = self.input();
        let bpv = input.read_byte() as i8;
        if bpv > 0 {
            forutil::decode(bpv as u32, &mut input, &mut self.b.scratch);
            let b = &mut *self.b;
            prefix_sum(&b.scratch, &mut b.doc_buffer, self.prev_doc_id);
            self.encoding = Encoding::Packed;
        } else {
            self.doc_bitset_base = self.prev_doc_id + 1;
            let num_longs = if bpv == 0 {
                self.b.doc_bitset[..BLOCK_SIZE / 64].fill(u64::MAX);
                BLOCK_SIZE / 64
            } else {
                let n = (-(bpv as i32)) as usize;
                for i in 0..n {
                    self.b.doc_bitset[i] = input.read_long();
                }
                n
            };
            let mut acc = 0;
            for i in 0..num_longs - 1 {
                acc += self.b.doc_bitset[i].count_ones() as i32;
                self.b.doc_cumulative_word_pop_counts[i] = acc;
            }
            self.b.doc_cumulative_word_pop_counts[num_longs - 1] = BLOCK;
            self.encoding = Encoding::Unary;
        }
        self.freq_fp = input.pos;
        forutil::pfor_skip(&mut input);
        self.pos = input.pos;
        self.doc_count_left -= BLOCK;
        self.prev_doc_id = self.b.doc_buffer[BLOCK_SIZE - 1];
        self.doc_buffer_upto = 0;
    }

    fn refill_remainder(&mut self) {
        debug_assert!(self.doc_count_left >= 0 && self.doc_count_left < BLOCK);
        if self.doc_freq == 1 {
            self.b.doc_buffer[0] = self.singleton_doc_id;
            self.b.freq_buffer[0] = self.total_term_freq as u32;
            self.b.doc_buffer[1] = NO_MORE_DOCS;
            self.doc_count_left = 0;
            self.doc_buffer_size = 1;
        } else {
            let n = self.doc_count_left as usize;
            let mut input = self.input();
            // PostingsUtil#readVIntBlock
            input.read_group_vints(&mut self.b.doc_buffer, n);
            for i in 0..n {
                let v = self.b.doc_buffer[i] as u32;
                self.b.doc_buffer[i] = (v >> 1) as i32;
                self.b.freq_buffer[i] = if v & 1 != 0 { 1 } else { input.read_vint() };
            }
            self.pos = input.pos;
            let mut acc = self.prev_doc_id;
            for d in self.b.doc_buffer[..n].iter_mut() {
                acc += *d;
                *d = acc;
            }
            self.b.doc_buffer[n] = NO_MORE_DOCS;
            self.freq_fp = NO_FREQ_FP;
            self.doc_buffer_size = n;
            self.doc_count_left = 0;
        }
        self.prev_doc_id = self.b.doc_buffer[BLOCK_SIZE - 1];
        self.doc_buffer_upto = 0;
        self.encoding = Encoding::Packed;
    }

    #[inline]
    fn refill_docs(&mut self) {
        if self.doc_count_left >= BLOCK {
            self.refill_full_block();
        } else {
            self.refill_remainder();
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]

    fn skip_level1_to(&mut self, target: i32) {
        loop {
            self.prev_doc_id = self.level1_last_doc_id;
            self.level0_last_doc_id = self.level1_last_doc_id;
            self.pos = self.level1_doc_end_fp;
            self.doc_count_left = self.doc_freq - self.level1_doc_count_upto;
            self.level1_doc_count_upto += LEVEL1;
            if self.doc_count_left < LEVEL1 {
                self.level1_last_doc_id = NO_MORE_DOCS;
                break;
            }
            let mut input = self.input();
            self.level1_last_doc_id += input.read_vint() as i32;
            let delta = input.read_vlong() as usize;
            self.level1_doc_end_fp = delta + input.pos;
            let _skip1_len = input.read_short();
            let num_impact_bytes = input.read_short() as usize;
            if self.level1_last_doc_id >= target {
                self.level1_impacts = (input.pos, num_impact_bytes);
            }
            input.pos += num_impact_bytes;
            self.pos = input.pos;
            if self.level1_last_doc_id >= target {
                break;
            }
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]

    fn do_move_to_next_level0_block(&mut self) {
        if self.doc_count_left >= BLOCK {
            let mut input = self.input();
            input.read_vlong(); // level0NumBytes
            self.level0_last_doc_id += input.read_vint15() as i32;
            let block_length = input.read_vlong15() as usize;
            self.level0_doc_end_fp = input.pos + block_length;
            let num_impact_bytes = input.read_vint() as usize;
            self.level0_impacts = (input.pos, num_impact_bytes);
            input.pos += num_impact_bytes;
            self.pos = input.pos;
            self.refill_full_block();
        } else {
            self.level0_last_doc_id = NO_MORE_DOCS;
            self.refill_remainder();
        }
    }

    fn move_to_next_level0_block(&mut self) {
        if self.doc == self.level1_last_doc_id {
            self.skip_level1_to(self.doc + 1);
        }
        self.prev_doc_id = self.level0_last_doc_id;
        self.do_move_to_next_level0_block();
    }

    #[cfg_attr(feature = "profile", inline(never))]

    fn skip_level0_to(&mut self, target: i32) {
        loop {
            self.prev_doc_id = self.level0_last_doc_id;
            if self.doc_count_left >= BLOCK {
                let mut input = self.input();
                let num_skip_bytes = input.read_vlong() as usize;
                let skip0_end = input.pos + num_skip_bytes;
                self.level0_last_doc_id += input.read_vint15() as i32;
                let found = target <= self.level0_last_doc_id;
                let block_length = input.read_vlong15() as usize;
                self.level0_doc_end_fp = input.pos + block_length;
                if found {
                    let num_impact_bytes = input.read_vint() as usize;
                    self.level0_impacts = (input.pos, num_impact_bytes);
                }
                self.pos = skip0_end;
                if found {
                    break;
                }
                self.pos = self.level0_doc_end_fp;
                self.doc_count_left -= BLOCK;
            } else {
                self.level0_last_doc_id = NO_MORE_DOCS;
                break;
            }
        }
    }

    pub fn advance_shallow(&mut self, target: i32) {
        if target > self.level0_last_doc_id {
            self.do_advance_shallow(target);
            self.needs_refilling = true;
        }
    }

    fn do_advance_shallow(&mut self, target: i32) {
        if target > self.level1_last_doc_id {
            self.skip_level1_to(target);
        } else if self.needs_refilling {
            self.pos = self.level0_doc_end_fp;
            self.doc_count_left -= BLOCK;
        }
        self.skip_level0_to(target);
    }

    #[inline]
    fn next_set_bit(&self, index: usize) -> i32 {
        let mut i = index >> 6;
        let word = self.b.doc_bitset[i] >> (index & 63);
        if word != 0 {
            return (index + word.trailing_zeros() as usize) as i32;
        }
        loop {
            i += 1;
            if i >= self.b.doc_bitset.len() {
                return NO_MORE_DOCS;
            }
            let w = self.b.doc_bitset[i];
            if w != 0 {
                return ((i << 6) + w.trailing_zeros() as usize) as i32;
            }
        }
    }

    #[cfg_attr(feature = "profile", inline(never))]

    pub fn next_doc(&mut self) -> i32 {
        if self.doc == self.level0_last_doc_id || self.needs_refilling {
            if self.needs_refilling {
                self.refill_docs();
                self.needs_refilling = false;
            } else {
                self.move_to_next_level0_block();
            }
        }
        match self.encoding {
            Encoding::Packed => self.doc = self.b.doc_buffer[self.doc_buffer_upto],
            Encoding::Unary => {
                let next = self.next_set_bit((self.doc - self.doc_bitset_base + 1) as usize);
                self.doc = self.doc_bitset_base + next;
            }
        }
        self.doc_buffer_upto += 1;
        self.doc
    }

    pub fn advance(&mut self, target: i32) -> i32 {
        if target > self.level0_last_doc_id || self.needs_refilling {
            if target > self.level0_last_doc_id {
                self.do_advance_shallow(target);
            }
            self.refill_docs();
            self.needs_refilling = false;
        }
        match self.encoding {
            Encoding::Packed => {
                let next =
                    find_next_geq(&self.b.doc_buffer, target, self.doc_buffer_upto, self.doc_buffer_size);
                self.doc = self.b.doc_buffer[next];
                self.doc_buffer_upto = next + 1;
            }
            Encoding::Unary => {
                let next = self.next_set_bit((target - self.doc_bitset_base) as usize);
                self.doc = self.doc_bitset_base + next;
                let word_index = (next >> 6) as usize;
                self.doc_buffer_upto = (1 + self.b.doc_cumulative_word_pop_counts[word_index]
                    - (self.b.doc_bitset[word_index] >> (next & 63)).count_ones() as i32)
                    as usize;
            }
        }
        self.doc
    }

    #[inline]
    fn compute_buffer_end_boundary(&self, up_to: i32) -> usize {
        if self.doc_buffer_size != 0 && self.b.doc_buffer[self.doc_buffer_size - 1] < up_to {
            self.doc_buffer_size
        } else {
            find_next_geq(&self.b.doc_buffer, up_to, self.doc_buffer_upto, self.doc_buffer_size)
        }
    }

    /// Returns docs (and freqs as floats) of the current block that are < up_to, then advances
    /// to up_to.
    #[cfg_attr(feature = "profile", inline(never))]
    pub fn next_postings(&mut self, up_to: i32, buffer: &mut DocAndFreqBuffer) {
        debug_assert!(!self.needs_refilling);
        buffer.size = 0;
        if self.doc >= up_to {
            return;
        }
        let up_to = (up_to as i64).min(self.level0_last_doc_id as i64 + 1) as i32;
        self.freq();
        let start = self.doc_buffer_upto - 1;
        match self.encoding {
            Encoding::Packed => {
                let end = self.compute_buffer_end_boundary(up_to);
                buffer.size = end - start;
                buffer.docs[..buffer.size].copy_from_slice(&self.b.doc_buffer[start..end]);
            }
            Encoding::Unary => {
                buffer.size = bitset_into_array(
                    &self.b.doc_bitset,
                    (self.doc - self.doc_bitset_base) as usize,
                    (up_to - self.doc_bitset_base) as usize,
                    self.doc_bitset_base,
                    &mut buffer.docs,
                );
            }
        }
        for i in 0..buffer.size {
            buffer.features[i] = self.b.freq_buffer[start + i] as f32;
        }
        self.advance(up_to);
    }

    // ---- Impacts ----

    #[inline]
    pub fn num_levels(&self) -> usize {
        if self.level1_last_doc_id == NO_MORE_DOCS { 1 } else { 2 }
    }

    #[inline]
    pub fn doc_id_up_to(&self, level: usize) -> i32 {
        match level {
            0 => self.level0_last_doc_id,
            1 => self.level1_last_doc_id,
            _ => NO_MORE_DOCS,
        }
    }

    /// Max of `freq * norm_inverse[norm]` over the impacts of `level` (see `Bm25::score_x`).
    #[cfg_attr(feature = "profile", inline(never))]
    pub fn max_impact_x(&self, level: usize, norm_inverse: &[f32; 256]) -> f32 {
        let (start, len) = if level == 0 && self.level0_last_doc_id != NO_MORE_DOCS {
            self.level0_impacts
        } else if level == 1 {
            self.level1_impacts
        } else {
            return i32::MAX as f32 * norm_inverse[1];
        };
        let mut input = In::new(self.data, start);
        let end = start + len;
        let (mut freq, mut norm) = (0u32, 0i64);
        let mut max = 0f32;
        while input.pos < end {
            let freq_delta = input.read_vint();
            freq += 1 + (freq_delta >> 1);
            if freq_delta & 1 != 0 {
                norm += 1 + input.read_zlong();
            } else {
                norm += 1;
            }
            max = max.max(freq as f32 * norm_inverse[norm as u8 as usize]);
        }
        max
    }

    #[cfg_attr(feature = "profile", inline(never))]
    pub fn impacts(&self, level: usize, out: &mut FreqNormBuffer) {
        out.freqs.clear();
        out.norms.clear();
        let (start, len) = if level == 0 && self.level0_last_doc_id != NO_MORE_DOCS {
            self.level0_impacts
        } else if level == 1 {
            self.level1_impacts
        } else {
            out.freqs.push(i32::MAX as u32);
            out.norms.push(1);
            return;
        };
        let mut input = In::new(self.data, start);
        let end = start + len;
        let (mut freq, mut norm) = (0u32, 0i64);
        while input.pos < end {
            let freq_delta = input.read_vint();
            freq += 1 + (freq_delta >> 1);
            if freq_delta & 1 != 0 {
                norm += 1 + input.read_zlong();
            } else {
                norm += 1;
            }
            out.freqs.push(freq);
            out.norms.push(norm as u8);
        }
    }
}

/// `dst[i] = base + src[0] + ... + src[i]` (Lucene104PostingsReader#prefixSum).
#[inline]
fn prefix_sum(src: &[u32; BLOCK_SIZE], dst: &mut [i32; BLOCK_SIZE + 1], base: i32) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    // SAFETY: AVX2 is enabled at compile time; loads/stores stay within the 256-element arrays.
    unsafe {
        use std::arch::x86_64::*;
        let mut carry = _mm256_set1_epi32(base);
        let last = _mm256_set1_epi32(7);
        for c in 0..BLOCK_SIZE / 8 {
            let mut x = _mm256_loadu_si256(src.as_ptr().add(c * 8) as *const __m256i);
            x = _mm256_add_epi32(x, _mm256_slli_si256::<4>(x));
            x = _mm256_add_epi32(x, _mm256_slli_si256::<8>(x));
            // carry the low 128-bit lane's total into the high lane
            let t = _mm256_shuffle_epi32::<0xFF>(x);
            x = _mm256_add_epi32(x, _mm256_permute2x128_si256::<0x08>(t, t));
            x = _mm256_add_epi32(x, carry);
            _mm256_storeu_si256(dst.as_mut_ptr().add(c * 8) as *mut __m256i, x);
            carry = _mm256_permutevar8x32_epi32(x, last);
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    {
        let mut acc = base;
        for i in 0..BLOCK_SIZE {
            acc = acc.wrapping_add(src[i] as i32);
            dst[i] = acc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_sum_matches_scalar() {
        let mut src = [0u32; BLOCK_SIZE];
        for (i, v) in src.iter_mut().enumerate() {
            *v = (i as u32 * 2654435761) % 1000;
        }
        let mut dst = [0i32; BLOCK_SIZE + 1];
        prefix_sum(&src, &mut dst, 12345);
        let mut acc = 12345i32;
        for i in 0..BLOCK_SIZE {
            acc += src[i] as i32;
            assert_eq!(dst[i], acc, "at {i}");
        }
    }
}

/// VectorUtil#findNextGEQ: first index in [from, to) with buffer[i] >= target, else `to`.
/// Scans 16-wide chunks (a SIMD compare + movemask) for the next 64 entries, the common case,
/// then falls back to a branchless binary search for far targets.
#[cfg_attr(feature = "profile", inline(never))]
#[cfg_attr(not(feature = "profile"), inline)]
fn find_next_geq(buffer: &[i32], target: i32, from: usize, to: usize) -> usize {
    const W: usize = 16;
    let mut i = from;
    let scan_end = (from + 4 * W).min(to);
    while i + W <= scan_end {
        let chunk: &[i32; W] = buffer[i..i + W].try_into().unwrap();
        let mut m = 0u32;
        for (j, &v) in chunk.iter().enumerate() {
            m |= ((v >= target) as u32) << j;
        }
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += W;
    }
    if to - i <= W {
        while i < to {
            if buffer[i] >= target {
                return i;
            }
            i += 1;
        }
        return to;
    }
    // lower bound in [i, to): everything before i is < target
    let mut base = i;
    let mut len = to - i;
    while len > 1 {
        let half = len / 2;
        if buffer[base + half - 1] < target {
            base += half;
        }
        len -= half;
    }
    if buffer[base] < target { base + 1 } else { base }
}

/// FixedBitSet#intoArray
#[cfg_attr(feature = "profile", inline(never))]
fn bitset_into_array(bits: &[u64], mut from: usize, to: usize, base: i32, out: &mut [i32]) -> usize {
    let mut n = 0;
    let mut emit = |mut word: u64, base: i32, n: &mut usize| {
        while word != 0 {
            out[*n] = base + word.trailing_zeros() as i32;
            *n += 1;
            word &= word - 1;
        }
    };
    if from & 63 != 0 {
        let mut word = bits[from >> 6] >> (from & 63);
        let til_next = (64 - (from & 63)) & 63;
        if to - from < til_next {
            word &= (1u64 << (to - from)) - 1;
            emit(word, from as i32 + base, &mut n);
            return n;
        }
        emit(word, from as i32 + base, &mut n);
        from += til_next;
    }
    for i in (from >> 6)..(to >> 6) {
        emit(bits[i], base + (i << 6) as i32, &mut n);
    }
    if to & 63 != 0 {
        let word = bits[to >> 6] & ((1u64 << (to & 63)) - 1);
        emit(word, base + (to & !63) as i32, &mut n);
    }
    n
}
