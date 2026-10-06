//! SmallFloat norm encoding and BM25Similarity, ported from Lucene 10.5.

const fn long_to_int4(i: u64) -> u32 {
    let num_bits = 64 - i.leading_zeros();
    if num_bits < 4 {
        i as u32
    } else {
        let shift = num_bits - 4;
        let mut encoded = (i >> shift) as u32;
        encoded &= 0x07;
        encoded |= (shift + 1) << 3;
        encoded
    }
}

const fn int4_to_long(i: u32) -> u64 {
    let bits = (i & 0x07) as u64;
    let shift = (i >> 3) as i32 - 1;
    if shift == -1 { bits } else { (bits | 0x08) << shift }
}

const MAX_INT4: u32 = long_to_int4(i32::MAX as u64);
const NUM_FREE_VALUES: u32 = 255 - MAX_INT4;

/// SmallFloat#intToByte4
pub const fn int_to_byte4(i: u32) -> u8 {
    if i < NUM_FREE_VALUES {
        i as u8
    } else {
        (NUM_FREE_VALUES + long_to_int4((i - NUM_FREE_VALUES) as u64)) as u8
    }
}

/// SmallFloat#byte4ToInt
pub const fn byte4_to_int(b: u8) -> u32 {
    let i = b as u32;
    if i < NUM_FREE_VALUES {
        i
    } else {
        (NUM_FREE_VALUES as u64 + int4_to_long(i - NUM_FREE_VALUES)) as u32
    }
}

const K1: f32 = 1.2;
const B: f32 = 0.75;

/// BM25Similarity.BM25Scorer
#[derive(Clone)]
pub struct Bm25 {
    weight: f32,
    cache: [f32; 256],
}

impl Bm25 {
    pub fn new(boost: f32, doc_freq: u64, doc_count: u64, sum_total_term_freq: u64) -> Self {
        // (float) Math.log(1 + (docCount - docFreq + 0.5D) / (docFreq + 0.5D))
        let idf = (1.0f64 + ((doc_count as i64 - doc_freq as i64) as f64 + 0.5) / (doc_freq as f64 + 0.5))
            .ln() as f32;
        let avgdl = (sum_total_term_freq as f64 / doc_count as f64) as f32;
        let mut cache = [0f32; 256];
        for (i, c) in cache.iter_mut().enumerate() {
            let len = byte4_to_int(i as u8) as f32;
            *c = 1f32 / (K1 * ((1f32 - B) + B * len / avgdl));
        }
        Bm25 { weight: boost * idf, cache }
    }

    #[inline(always)]
    pub fn score(&self, freq: f32, norm: u8) -> f32 {
        self.score_x(freq * self.cache[norm as usize])
    }

    pub fn norm_inverses(&self) -> &[f32; 256] {
        &self.cache
    }

    /// `1 / (k1 * (1 - b + b * dl / avgdl))` for an encoded norm.
    #[inline(always)]
    pub fn norm_inverse(&self, norm: u8) -> f32 {
        self.cache[norm as usize]
    }

    /// Score for `x = freq * norm_inverse(norm)`. Each IEEE op here is monotonic, so the max
    /// score over a set of (freq, norm) pairs equals `score_x` of their max `x`, bit for bit.
    #[inline(always)]
    pub fn score_x(&self, x: f32) -> f32 {
        self.weight - self.weight / (1f32 + x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smallfloat_matches_lucene_contract() {
        assert_eq!(NUM_FREE_VALUES, 24);
        for i in 0..24 {
            assert_eq!(byte4_to_int(int_to_byte4(i)), i);
        }
        // monotonic and lossy-but-lower-bounding, as documented in SmallFloat
        let mut prev = 0;
        for i in 0..2_000_000u32 {
            let b = int_to_byte4(i);
            assert!(b >= prev);
            prev = b;
            assert!(byte4_to_int(b) <= i);
        }
        assert_eq!(int_to_byte4(i32::MAX as u32), 255);
    }
}
