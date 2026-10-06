//! Port of Lucene104PostingsWriter for IndexOptions.DOCS_AND_FREQS with norms:
//! 256-doc FOR/bitset doc blocks + PFOR freq blocks, level-0 skip data (with impacts) before
//! every packed block and level-1 skip data (with impacts) before every 32 blocks.

use crate::forutil::{self, BLOCK_SIZE, bits_required};
use crate::store::Out;

pub const LEVEL1_FACTOR: usize = 32;
pub const LEVEL1_NUM_DOCS: usize = LEVEL1_FACTOR * BLOCK_SIZE;
const LEVEL1_MASK: usize = LEVEL1_NUM_DOCS - 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct TermMeta {
    pub doc_freq: u32,
    pub total_term_freq: u64,
    pub doc_start_fp: u64,
    /// -1 unless doc_freq == 1, in which case the postings are inlined here.
    pub singleton_doc_id: i32,
}

/// CompetitiveImpactAccumulator, specialised to single-byte norms (the only kind BM25 writes).
pub struct ImpactAcc {
    max_freqs: [u32; 256],
}

impl ImpactAcc {
    fn new() -> Self {
        Self { max_freqs: [0; 256] }
    }
    fn clear(&mut self) {
        self.max_freqs = [0; 256];
    }
    #[inline]
    fn add(&mut self, freq: u32, norm: u8) {
        let m = &mut self.max_freqs[norm as usize];
        *m = (*m).max(freq);
    }
    fn add_all(&mut self, other: &ImpactAcc) {
        for (a, b) in self.max_freqs.iter_mut().zip(other.max_freqs.iter()) {
            *a = (*a).max(*b);
        }
    }
    /// Pareto-optimal (freq, norm) pairs in increasing freq and norm order.
    fn competitive(&self, out: &mut Vec<(u32, u8)>) {
        out.clear();
        let mut max_freq_lower_norms = 0;
        for (i, &f) in self.max_freqs.iter().enumerate() {
            if f > max_freq_lower_norms {
                out.push((f, i as u8));
                max_freq_lower_norms = f;
            }
        }
    }
}

fn write_impacts(impacts: &[(u32, u8)], out: &mut Out) {
    let (mut pf, mut pn) = (0u32, 0i64);
    for &(f, n) in impacts {
        let freq_delta = f - pf - 1;
        let norm_delta = n as i64 - pn - 1;
        if norm_delta == 0 {
            out.write_vint(freq_delta << 1);
        } else {
            out.write_vint((freq_delta << 1) | 1);
            out.write_zlong(norm_delta);
        }
        pf = f;
        pn = n as i64;
    }
}

pub struct PostingsWriter {
    pub doc_out: Out,
    doc_delta_buffer: [u32; BLOCK_SIZE],
    freq_buffer: [u32; BLOCK_SIZE],
    doc_buffer_upto: usize,
    level0_last_doc_id: i32,
    level1_last_doc_id: i32,
    doc_id: i32,
    last_doc_id: i32,
    doc_count: usize,
    doc_start_fp: u64,
    level0_acc: ImpactAcc,
    level1_acc: ImpactAcc,
    impacts_scratch: Vec<(u32, u8)>,
    scratch: Out,
    level0_output: Out,
    level1_output: Out,
    spare_bitset: [u64; BLOCK_SIZE / 2],
    pub max_impact_bytes: usize,
}

impl Default for PostingsWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl PostingsWriter {
    pub fn new() -> Self {
        Self {
            doc_out: Out::new(),
            doc_delta_buffer: [0; BLOCK_SIZE],
            freq_buffer: [0; BLOCK_SIZE],
            doc_buffer_upto: 0,
            level0_last_doc_id: -1,
            level1_last_doc_id: -1,
            doc_id: -1,
            last_doc_id: -1,
            doc_count: 0,
            doc_start_fp: 0,
            level0_acc: ImpactAcc::new(),
            level1_acc: ImpactAcc::new(),
            impacts_scratch: Vec::new(),
            scratch: Out::new(),
            level0_output: Out::new(),
            level1_output: Out::new(),
            spare_bitset: [0; BLOCK_SIZE / 2],
            max_impact_bytes: 0,
        }
    }

    pub fn start_term(&mut self) {
        self.doc_start_fp = self.doc_out.len() as u64;
        self.last_doc_id = -1;
        self.level0_last_doc_id = -1;
        self.level1_last_doc_id = -1;
        self.level0_acc.clear();
    }

    pub fn start_doc(&mut self, doc_id: i32, freq: u32, norm: u8) {
        if self.doc_buffer_upto == BLOCK_SIZE {
            self.flush_doc_block(false);
            self.doc_buffer_upto = 0;
        }
        let delta = doc_id - self.last_doc_id;
        assert!(doc_id >= 0 && delta > 0, "docs out of order");
        self.doc_delta_buffer[self.doc_buffer_upto] = delta as u32;
        self.freq_buffer[self.doc_buffer_upto] = freq;
        self.doc_id = doc_id;
        self.level0_acc.add(freq, norm);
        // finishDoc
        self.doc_buffer_upto += 1;
        self.doc_count += 1;
        self.last_doc_id = doc_id;
    }

    fn flush_doc_block(&mut self, finish_term: bool) {
        debug_assert!(self.doc_buffer_upto != 0);
        if self.doc_buffer_upto < BLOCK_SIZE {
            assert!(finish_term);
            // PostingsUtil#writeVIntBlock
            let n = self.doc_buffer_upto;
            for i in 0..n {
                self.doc_delta_buffer[i] =
                    (self.doc_delta_buffer[i] << 1) | (self.freq_buffer[i] == 1) as u32;
            }
            self.level0_output.write_group_vints(&self.doc_delta_buffer[..n]);
            for i in 0..n {
                if self.freq_buffer[i] != 1 {
                    self.level0_output.write_vint(self.freq_buffer[i]);
                }
            }
        } else {
            self.level0_acc.competitive(&mut self.impacts_scratch);
            write_impacts(&self.impacts_scratch, &mut self.scratch);
            self.max_impact_bytes = self.max_impact_bytes.max(self.scratch.len());
            self.level0_output.write_vlong(self.scratch.len() as u64);
            self.level0_output.append(&self.scratch);
            self.scratch.clear();
            let mut num_skip_bytes = self.level0_output.len();

            let or = self.doc_delta_buffer.iter().fold(0, |a, &b| a | b);
            let bpv = bits_required(or);
            let doc_range = (self.last_doc_id - self.level0_last_doc_id) as usize;
            let num_bitset_longs = doc_range.div_ceil(64);
            let num_bits_next_bpv = (32.min(bpv + 1) as usize) * BLOCK_SIZE;
            if doc_range == BLOCK_SIZE {
                self.level0_output.write_byte(0);
            } else if num_bits_next_bpv <= doc_range {
                self.level0_output.write_byte(bpv as u8);
                forutil::encode(&mut self.doc_delta_buffer, bpv, &mut self.level0_output);
            } else {
                // unary coding: store doc IDs of the block as a bit set
                self.spare_bitset[..num_bitset_longs].fill(0);
                let mut s: i64 = -1;
                for &d in self.doc_delta_buffer.iter() {
                    s += d as i64;
                    self.spare_bitset[(s >> 6) as usize] |= 1u64 << (s & 63);
                }
                assert!(num_bitset_longs <= BLOCK_SIZE / 2);
                self.level0_output.write_byte((-(num_bitset_longs as i32)) as u8);
                for i in 0..num_bitset_longs {
                    self.level0_output.write_long(self.spare_bitset[i]);
                }
            }
            forutil::pfor_encode(&mut self.freq_buffer, &mut self.level0_output);

            self.scratch.write_vint15((self.doc_id - self.level0_last_doc_id) as u32);
            self.scratch.write_vlong15(self.level0_output.len() as u64);
            num_skip_bytes += self.scratch.len();
            self.level1_output.write_vlong(num_skip_bytes as u64);
            self.level1_output.append(&self.scratch);
            self.scratch.clear();
        }

        self.level1_output.append(&self.level0_output);
        self.level0_output.clear();
        self.level0_last_doc_id = self.doc_id;
        self.level1_acc.add_all(&self.level0_acc);
        self.level0_acc.clear();

        if self.doc_count & LEVEL1_MASK == 0 {
            self.write_level1_skip_data();
            self.level1_last_doc_id = self.doc_id;
            self.level1_acc.clear();
        } else if finish_term {
            self.doc_out.append(&self.level1_output);
            self.level1_output.clear();
            self.level1_acc.clear();
        }
    }

    fn write_level1_skip_data(&mut self) {
        self.doc_out.write_vint((self.doc_id - self.level1_last_doc_id) as u32);
        self.level1_acc.competitive(&mut self.impacts_scratch);
        write_impacts(&self.impacts_scratch, &mut self.scratch);
        let num_impact_bytes = self.scratch.len();
        self.max_impact_bytes = self.max_impact_bytes.max(num_impact_bytes);
        let level1_len = 2 * 2 + self.scratch.len() + self.level1_output.len();
        self.doc_out.write_vlong(level1_len as u64);
        assert!(num_impact_bytes + 2 <= i16::MAX as usize);
        self.doc_out.write_short((self.scratch.len() + 2) as i16);
        self.doc_out.write_short(num_impact_bytes as i16);
        self.doc_out.append(&self.scratch);
        self.scratch.clear();
        self.doc_out.append(&self.level1_output);
        self.level1_output.clear();
    }

    pub fn finish_term(&mut self, doc_freq: u32, total_term_freq: u64) -> TermMeta {
        assert_eq!(doc_freq as usize, self.doc_count);
        let singleton_doc_id = if doc_freq == 1 {
            self.doc_delta_buffer[0] as i32 - 1
        } else {
            self.flush_doc_block(true);
            -1
        };
        let meta = TermMeta {
            doc_freq,
            total_term_freq,
            doc_start_fp: self.doc_start_fp,
            singleton_doc_id,
        };
        self.doc_buffer_upto = 0;
        self.last_doc_id = -1;
        self.doc_count = 0;
        meta
    }
}
