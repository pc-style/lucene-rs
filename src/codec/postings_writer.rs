//! Port of `Lucene104PostingsWriter` (no payloads or offsets).
//!
//! 256-doc FOR-or-bitset doc blocks, PFOR freq and position blocks, level-0 skip data (with
//! impacts) before every packed block and level-1 skip data (with impacts) every 32 blocks.
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

use crate::codec::forutil::{self, BLOCK_SIZE, bits_required};
use crate::codec::store::Out;

pub const LEVEL1_FACTOR: usize = 32;
pub const LEVEL1_NUM_DOCS: usize = LEVEL1_FACTOR * BLOCK_SIZE;
const LEVEL1_MASK: usize = LEVEL1_NUM_DOCS - 1;

/// Per-term metadata stored in the term dictionary (Lucene's `IntBlockTermState`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermMeta {
    pub doc_freq: u32,
    /// Equals `doc_freq` when frequencies are not indexed.
    pub total_term_freq: u64,
    pub doc_start_fp: u64,
    pub pos_start_fp: u64,
    /// Offset of the last (vInt-encoded) position block from `pos_start_fp`, or -1.
    pub last_pos_block_offset: i64,
    /// -1 unless `doc_freq == 1`, in which case the doc ID is inlined here.
    pub singleton_doc_id: i32,
}

/// `CompetitiveImpactAccumulator`, specialized to single-byte norms (the only kind BM25 uses).
pub struct ImpactAcc {
    max_freqs: [u32; 256],
}

impl ImpactAcc {
    const fn new() -> Self {
        Self {
            max_freqs: [0; 256],
        }
    }
    const fn clear(&mut self) {
        self.max_freqs = [0; 256];
    }
    #[inline]
    fn add(&mut self, freq: u32, norm: u8) {
        let m = &mut self.max_freqs[norm as usize];
        *m = (*m).max(freq);
    }
    fn add_all(&mut self, other: &Self) {
        for (a, b) in self.max_freqs.iter_mut().zip(other.max_freqs.iter()) {
            *a = (*a).max(*b);
        }
    }
    /// Pareto-optimal (freq, norm) pairs, in increasing freq and norm order.
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
    pub pos_out: Out,
    write_freqs: bool,
    write_positions: bool,

    doc_delta_buffer: [u32; BLOCK_SIZE],
    freq_buffer: [u32; BLOCK_SIZE],
    doc_buffer_upto: usize,
    pos_delta_buffer: [u32; BLOCK_SIZE],
    pos_buffer_upto: usize,
    last_position: u32,

    level0_last_doc_id: i32,
    level1_last_doc_id: i32,
    level0_last_pos_fp: u64,
    level1_last_pos_fp: u64,
    doc_id: i32,
    last_doc_id: i32,
    doc_count: usize,
    doc_start_fp: u64,
    pos_start_fp: u64,

    level0_acc: ImpactAcc,
    level1_acc: ImpactAcc,
    impacts_scratch: Vec<(u32, u8)>,
    scratch: Out,
    level0_output: Out,
    level1_output: Out,
    spare_bitset: [u64; BLOCK_SIZE / 2],
}

impl Default for PostingsWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl PostingsWriter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            doc_out: Out::new(),
            pos_out: Out::new(),
            write_freqs: true,
            write_positions: false,
            doc_delta_buffer: [0; BLOCK_SIZE],
            freq_buffer: [0; BLOCK_SIZE],
            doc_buffer_upto: 0,
            pos_delta_buffer: [0; BLOCK_SIZE],
            pos_buffer_upto: 0,
            last_position: 0,
            level0_last_doc_id: -1,
            level1_last_doc_id: -1,
            level0_last_pos_fp: 0,
            level1_last_pos_fp: 0,
            doc_id: -1,
            last_doc_id: -1,
            doc_count: 0,
            doc_start_fp: 0,
            pos_start_fp: 0,
            level0_acc: ImpactAcc::new(),
            level1_acc: ImpactAcc::new(),
            impacts_scratch: Vec::new(),
            scratch: Out::new(),
            level0_output: Out::new(),
            level1_output: Out::new(),
            spare_bitset: [0; BLOCK_SIZE / 2],
        }
    }

    /// `setField`: what the following terms index.
    pub fn set_field(&mut self, write_freqs: bool, write_positions: bool) {
        assert!(
            write_freqs || !write_positions,
            "positions require frequencies"
        );
        self.write_freqs = write_freqs;
        self.write_positions = write_positions;
    }

    pub const fn start_term(&mut self) {
        self.doc_start_fp = self.doc_out.len() as u64;
        if self.write_positions {
            self.pos_start_fp = self.pos_out.len() as u64;
            self.level0_last_pos_fp = self.pos_start_fp;
            self.level1_last_pos_fp = self.pos_start_fp;
        }
        self.last_doc_id = -1;
        self.level0_last_doc_id = -1;
        self.level1_last_doc_id = -1;
        if self.write_freqs {
            self.level0_acc.clear();
        }
    }

    /// `startDoc`. `norm` is the encoded norm (1 when the field omits norms).
    pub fn start_doc(&mut self, doc_id: i32, freq: u32, norm: u8) {
        if self.doc_buffer_upto == BLOCK_SIZE {
            self.flush_doc_block(false);
            self.doc_buffer_upto = 0;
        }
        let delta = doc_id - self.last_doc_id;
        assert!(
            doc_id >= 0 && delta > 0,
            "docs out of order ({doc_id} <= {})",
            self.last_doc_id
        );
        self.doc_delta_buffer[self.doc_buffer_upto] = delta as u32;
        if self.write_freqs {
            self.freq_buffer[self.doc_buffer_upto] = freq;
            self.level0_acc.add(freq, norm);
        }
        self.doc_id = doc_id;
        self.last_position = 0;
    }

    pub fn add_position(&mut self, position: u32) {
        debug_assert!(self.write_positions);
        self.pos_delta_buffer[self.pos_buffer_upto] = position - self.last_position;
        self.pos_buffer_upto += 1;
        self.last_position = position;
        if self.pos_buffer_upto == BLOCK_SIZE {
            forutil::pfor_encode(&mut self.pos_delta_buffer, &mut self.pos_out);
            self.pos_buffer_upto = 0;
        }
    }

    pub const fn finish_doc(&mut self) {
        self.doc_buffer_upto += 1;
        self.doc_count += 1;
        self.last_doc_id = self.doc_id;
    }

    fn flush_doc_block(&mut self, finish_term: bool) {
        debug_assert!(self.doc_buffer_upto != 0);
        if self.doc_buffer_upto < BLOCK_SIZE {
            assert!(finish_term);
            // PostingsUtil#writeVIntBlock
            let n = self.doc_buffer_upto;
            if self.write_freqs {
                for i in 0..n {
                    self.doc_delta_buffer[i] =
                        (self.doc_delta_buffer[i] << 1) | (self.freq_buffer[i] == 1) as u32;
                }
            }
            self.level0_output
                .write_group_vints(&self.doc_delta_buffer[..n]);
            if self.write_freqs {
                for i in 0..n {
                    if self.freq_buffer[i] != 1 {
                        self.level0_output.write_vint(self.freq_buffer[i]);
                    }
                }
            }
        } else {
            if self.write_freqs {
                self.level0_acc.competitive(&mut self.impacts_scratch);
                write_impacts(&self.impacts_scratch, &mut self.scratch);
                self.level0_output.write_vlong(self.scratch.len() as u64);
                self.level0_output.append(&self.scratch);
                self.scratch.clear();
                if self.write_positions {
                    let fp = self.pos_out.len() as u64;
                    self.level0_output.write_vlong(fp - self.level0_last_pos_fp);
                    self.level0_output.write_byte(self.pos_buffer_upto as u8);
                    self.level0_last_pos_fp = fp;
                }
            }
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
                // unary coding: the block's doc IDs as a bit set
                self.spare_bitset[..num_bitset_longs].fill(0);
                let mut s: i64 = -1;
                for &d in &self.doc_delta_buffer {
                    s += d as i64;
                    self.spare_bitset[(s >> 6) as usize] |= 1u64 << (s & 63);
                }
                assert!(num_bitset_longs <= BLOCK_SIZE / 2);
                self.level0_output
                    .write_byte((-(num_bitset_longs as i32)) as u8);
                for i in 0..num_bitset_longs {
                    self.level0_output.write_long(self.spare_bitset[i]);
                }
            }
            if self.write_freqs {
                forutil::pfor_encode(&mut self.freq_buffer, &mut self.level0_output);
            }

            self.scratch
                .write_vint15((self.doc_id - self.level0_last_doc_id) as u32);
            self.scratch.write_vlong15(self.level0_output.len() as u64);
            num_skip_bytes += self.scratch.len();
            self.level1_output.write_vlong(num_skip_bytes as u64);
            self.level1_output.append(&self.scratch);
            self.scratch.clear();
        }

        self.level1_output.append(&self.level0_output);
        self.level0_output.clear();
        self.level0_last_doc_id = self.doc_id;
        if self.write_freqs {
            self.level1_acc.add_all(&self.level0_acc);
            self.level0_acc.clear();
        }

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
        self.doc_out
            .write_vint((self.doc_id - self.level1_last_doc_id) as u32);
        if self.write_freqs {
            self.level1_acc.competitive(&mut self.impacts_scratch);
            write_impacts(&self.impacts_scratch, &mut self.scratch);
            let num_impact_bytes = self.scratch.len();
            if self.write_positions {
                let fp = self.pos_out.len() as u64;
                self.scratch.write_vlong(fp - self.level1_last_pos_fp);
                self.scratch.write_byte(self.pos_buffer_upto as u8);
                self.level1_last_pos_fp = fp;
            }
            let level1_len = 2 * 2 + self.scratch.len() + self.level1_output.len();
            self.doc_out.write_vlong(level1_len as u64);
            assert!(i16::try_from(self.scratch.len() + 2).is_ok());
            self.doc_out.write_short((self.scratch.len() + 2) as i16);
            self.doc_out.write_short(num_impact_bytes as i16);
            self.doc_out.append(&self.scratch);
            self.scratch.clear();
        } else {
            self.doc_out.write_vlong(self.level1_output.len() as u64);
        }
        self.doc_out.append(&self.level1_output);
        self.level1_output.clear();
    }

    /// `finishTerm`. For fields without frequencies pass `total_term_freq = doc_freq`.
    pub fn finish_term(&mut self, doc_freq: u32, total_term_freq: u64) -> TermMeta {
        assert_eq!(doc_freq as usize, self.doc_count);
        let singleton_doc_id = if doc_freq == 1 {
            self.doc_delta_buffer[0] as i32 - 1
        } else {
            self.flush_doc_block(true);
            -1
        };
        let mut last_pos_block_offset = -1;
        if self.write_positions {
            if total_term_freq > BLOCK_SIZE as u64 {
                last_pos_block_offset = (self.pos_out.len() as u64 - self.pos_start_fp) as i64;
            }
            for i in 0..self.pos_buffer_upto {
                self.pos_out.write_vint(self.pos_delta_buffer[i]);
            }
        }
        let meta = TermMeta {
            doc_freq,
            total_term_freq,
            doc_start_fp: self.doc_start_fp,
            pos_start_fp: if self.write_positions {
                self.pos_start_fp
            } else {
                0
            },
            last_pos_block_offset,
            singleton_doc_id,
        };
        self.doc_buffer_upto = 0;
        self.pos_buffer_upto = 0;
        self.last_doc_id = -1;
        self.doc_count = 0;
        meta
    }
}
