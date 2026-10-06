//! Port of Lucene104PostingsReader.BlockPostingsEnum for the configuration used by top-k
//! scoring queries: needsFreq = true, needsImpacts = true, no positions.

use crate::forutil::{self, BLOCK_SIZE};
use crate::postings_writer::{LEVEL1_NUM_DOCS, TermMeta};
use crate::store::In;

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

pub struct PostingsEnum<'a> {
    data: &'a [u8],
    pos: usize,
    doc_buffer: [i32; BLOCK_SIZE + 1],
    freq_buffer: [u32; BLOCK_SIZE],
    scratch: [u32; BLOCK_SIZE],
    doc_bitset: [u64; BLOCK_SIZE / 2],
    doc_cumulative_word_pop_counts: [i32; BLOCK_SIZE / 2],
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
            doc_buffer: [0; BLOCK_SIZE + 1],
            freq_buffer: [0; BLOCK_SIZE],
            scratch: [0; BLOCK_SIZE],
            doc_bitset: [0; BLOCK_SIZE / 2],
            doc_cumulative_word_pop_counts: [0; BLOCK_SIZE / 2],
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

    #[inline]
    pub fn freq(&mut self) -> u32 {
        if self.freq_fp != NO_FREQ_FP {
            let mut input = In::new(self.data, self.freq_fp);
            forutil::pfor_decode(&mut input, &mut self.freq_buffer);
            self.pos = input.pos;
            self.freq_fp = NO_FREQ_FP;
        }
        self.freq_buffer[self.doc_buffer_upto - 1]
    }

    fn refill_full_block(&mut self) {
        let mut input = self.input();
        let bpv = input.read_byte() as i8;
        if bpv > 0 {
            forutil::decode(bpv as u32, &mut input, &mut self.scratch);
            // prefixSum(docBuffer, BLOCK_SIZE, prevDocID)
            let mut acc = self.prev_doc_id;
            for i in 0..BLOCK_SIZE {
                acc = acc.wrapping_add(self.scratch[i] as i32);
                self.doc_buffer[i] = acc;
            }
            self.encoding = Encoding::Packed;
        } else {
            self.doc_bitset_base = self.prev_doc_id + 1;
            let num_longs = if bpv == 0 {
                self.doc_bitset[..BLOCK_SIZE / 64].fill(u64::MAX);
                BLOCK_SIZE / 64
            } else {
                let n = (-(bpv as i32)) as usize;
                for i in 0..n {
                    self.doc_bitset[i] = input.read_long();
                }
                n
            };
            let mut acc = 0;
            for i in 0..num_longs - 1 {
                acc += self.doc_bitset[i].count_ones() as i32;
                self.doc_cumulative_word_pop_counts[i] = acc;
            }
            self.doc_cumulative_word_pop_counts[num_longs - 1] = BLOCK;
            self.encoding = Encoding::Unary;
        }
        self.freq_fp = input.pos;
        forutil::pfor_skip(&mut input);
        self.pos = input.pos;
        self.doc_count_left -= BLOCK;
        self.prev_doc_id = self.doc_buffer[BLOCK_SIZE - 1];
        self.doc_buffer_upto = 0;
    }

    fn refill_remainder(&mut self) {
        debug_assert!(self.doc_count_left >= 0 && self.doc_count_left < BLOCK);
        if self.doc_freq == 1 {
            self.doc_buffer[0] = self.singleton_doc_id;
            self.freq_buffer[0] = self.total_term_freq as u32;
            self.doc_buffer[1] = NO_MORE_DOCS;
            self.doc_count_left = 0;
            self.doc_buffer_size = 1;
        } else {
            let n = self.doc_count_left as usize;
            let mut input = self.input();
            // PostingsUtil#readVIntBlock
            input.read_group_vints(&mut self.doc_buffer, n);
            for i in 0..n {
                let v = self.doc_buffer[i] as u32;
                self.doc_buffer[i] = (v >> 1) as i32;
                self.freq_buffer[i] = if v & 1 != 0 { 1 } else { input.read_vint() };
            }
            self.pos = input.pos;
            let mut acc = self.prev_doc_id;
            for d in self.doc_buffer[..n].iter_mut() {
                acc += *d;
                *d = acc;
            }
            self.doc_buffer[n] = NO_MORE_DOCS;
            self.freq_fp = NO_FREQ_FP;
            self.doc_buffer_size = n;
            self.doc_count_left = 0;
        }
        self.prev_doc_id = self.doc_buffer[BLOCK_SIZE - 1];
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
        let word = self.doc_bitset[i] >> (index & 63);
        if word != 0 {
            return (index + word.trailing_zeros() as usize) as i32;
        }
        loop {
            i += 1;
            if i >= self.doc_bitset.len() {
                return NO_MORE_DOCS;
            }
            let w = self.doc_bitset[i];
            if w != 0 {
                return ((i << 6) + w.trailing_zeros() as usize) as i32;
            }
        }
    }

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
            Encoding::Packed => self.doc = self.doc_buffer[self.doc_buffer_upto],
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
                    find_next_geq(&self.doc_buffer, target, self.doc_buffer_upto, self.doc_buffer_size);
                self.doc = self.doc_buffer[next];
                self.doc_buffer_upto = next + 1;
            }
            Encoding::Unary => {
                let next = self.next_set_bit((target - self.doc_bitset_base) as usize);
                self.doc = self.doc_bitset_base + next;
                let word_index = (next >> 6) as usize;
                self.doc_buffer_upto = (1 + self.doc_cumulative_word_pop_counts[word_index]
                    - (self.doc_bitset[word_index] >> (next & 63)).count_ones() as i32)
                    as usize;
            }
        }
        self.doc
    }

    #[inline]
    fn compute_buffer_end_boundary(&self, up_to: i32) -> usize {
        if self.doc_buffer_size != 0 && self.doc_buffer[self.doc_buffer_size - 1] < up_to {
            self.doc_buffer_size
        } else {
            find_next_geq(&self.doc_buffer, up_to, self.doc_buffer_upto, self.doc_buffer_size)
        }
    }

    /// Returns docs (and freqs as floats) of the current block that are < up_to, then advances
    /// to up_to.
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
                buffer.docs[..buffer.size].copy_from_slice(&self.doc_buffer[start..end]);
            }
            Encoding::Unary => {
                buffer.size = bitset_into_array(
                    &self.doc_bitset,
                    (self.doc - self.doc_bitset_base) as usize,
                    (up_to - self.doc_bitset_base) as usize,
                    self.doc_bitset_base,
                    &mut buffer.docs,
                );
            }
        }
        for i in 0..buffer.size {
            buffer.features[i] = self.freq_buffer[start + i] as f32;
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

/// VectorUtil#findNextGEQ: first index in [from, to) with buffer[i] >= target, else `to`.
/// Written in 8-wide chunks so LLVM emits a SIMD compare + movemask, like Lucene's Panama path.
#[inline]
fn find_next_geq(buffer: &[i32], target: i32, from: usize, to: usize) -> usize {
    let mut i = from;
    while i + 8 <= to {
        let chunk: &[i32; 8] = buffer[i..i + 8].try_into().unwrap();
        let mut m = 0u32;
        for (j, &v) in chunk.iter().enumerate() {
            m |= ((v >= target) as u32) << j;
        }
        if m != 0 {
            return i + m.trailing_zeros() as usize;
        }
        i += 8;
    }
    while i < to {
        if buffer[i] >= target {
            return i;
        }
        i += 1;
    }
    to
}

/// FixedBitSet#intoArray
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
