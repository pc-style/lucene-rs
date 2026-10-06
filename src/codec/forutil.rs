//! Port of Lucene104 ForUtil / PForUtil: 256 integers per block, bit-packed with the same
//! SWAR "collapse into 8/16/32-bit lanes" layout Lucene uses so decoding auto-vectorizes.

use crate::codec::store::{In, Out};

pub const BLOCK_SIZE: usize = 256;
const MAX_EXCEPTIONS: usize = 7;

#[inline(always)]
const fn expand_mask16(m: u32) -> u32 {
    m | (m << 16)
}
#[inline(always)]
const fn expand_mask8(m: u32) -> u32 {
    expand_mask16(m | (m << 8))
}
#[inline(always)]
const fn low_bits(b: u32) -> u32 {
    if b >= 32 { u32::MAX } else { (1u32 << b) - 1 }
}
/// Mask of the `b` low bits of every `p`-bit lane.
#[inline(always)]
const fn mask(p: u32, b: u32) -> u32 {
    match p {
        8 => expand_mask8(low_bits(b)),
        16 => expand_mask16(low_bits(b)),
        _ => low_bits(b),
    }
}

#[inline]
pub fn bits_required(v: u32) -> u32 {
    (32 - v.leading_zeros()).max(1)
}

fn collapse8(a: &mut [u32; BLOCK_SIZE]) {
    for i in 0..64 {
        a[i] = (a[i] << 24) | (a[64 + i] << 16) | (a[128 + i] << 8) | a[192 + i];
    }
}
fn collapse16(a: &mut [u32; BLOCK_SIZE]) {
    for i in 0..128 {
        a[i] = (a[i] << 16) | a[128 + i];
    }
}
#[cfg(test)]
#[inline(always)]
fn expand8(a: &mut [u32; BLOCK_SIZE]) {
    for i in 0..64 {
        let l = a[i];
        a[i] = (l >> 24) & 0xFF;
        a[64 + i] = (l >> 16) & 0xFF;
        a[128 + i] = (l >> 8) & 0xFF;
        a[192 + i] = l & 0xFF;
    }
}
#[cfg(test)]
#[inline(always)]
fn expand16(a: &mut [u32; BLOCK_SIZE]) {
    for i in 0..128 {
        let l = a[i];
        a[i] = (l >> 16) & 0xFFFF;
        a[128 + i] = l & 0xFFFF;
    }
}

pub fn num_bytes(bpv: u32) -> usize {
    (bpv as usize) << 5
}

/// ForUtil#encode. Mutates `ints`.
pub fn encode(ints: &mut [u32; BLOCK_SIZE], bpv: u32, out: &mut Out) {
    let p: u32 = if bpv <= 8 {
        collapse8(ints);
        8
    } else if bpv <= 16 {
        collapse16(ints);
        16
    } else {
        32
    };
    let num_ints = BLOCK_SIZE * p as usize / 32;
    let nips = bpv as usize * 8;
    let mut tmp = [0u32; BLOCK_SIZE];
    let mut idx = 0;
    let mut shift = p as i32 - bpv as i32;
    for t in tmp.iter_mut().take(nips) {
        *t = ints[idx] << shift;
        idx += 1;
    }
    shift -= bpv as i32;
    while shift >= 0 {
        for t in tmp.iter_mut().take(nips) {
            *t |= ints[idx] << shift;
            idx += 1;
        }
        shift -= bpv as i32;
    }
    let rbpi = (shift + bpv as i32) as u32;
    let mask_rbpi = mask(p, rbpi);
    let mut tmp_idx = 0;
    let mut rbpv = bpv;
    while idx < num_ints {
        if rbpv >= rbpi {
            rbpv -= rbpi;
            tmp[tmp_idx] |= (ints[idx] >> rbpv) & mask_rbpi;
            tmp_idx += 1;
            if rbpv == 0 {
                idx += 1;
                rbpv = bpv;
            }
        } else {
            let mask1 = mask(p, rbpv);
            let mask2 = mask(p, rbpi - rbpv);
            tmp[tmp_idx] |= (ints[idx] & mask1) << (rbpi - rbpv);
            idx += 1;
            rbpv = bpv - rbpi + rbpv;
            tmp[tmp_idx] |= (ints[idx] >> rbpv) & mask2;
            tmp_idx += 1;
        }
    }
    for t in tmp.iter().take(nips) {
        out.write_int(*t);
    }
}

/// Inverse of [`encode`], specialized per bit width like Lucene's generated decodeN methods.
#[cfg(test)]
#[inline(always)]
fn decode_impl<const B: u32>(input: &mut In, ints: &mut [u32; BLOCK_SIZE]) {
    let p: u32 = if B <= 8 {
        8
    } else if B <= 16 {
        16
    } else {
        32
    };
    let num_ints = BLOCK_SIZE * p as usize / 32;
    let nips = B as usize * 8;
    let bytes = &input.data[input.pos..input.pos + nips * 4];
    input.pos += nips * 4;
    let mut tmp = [0u32; BLOCK_SIZE];
    for (t, c) in tmp[..nips].iter_mut().zip(bytes.chunks_exact(4)) {
        *t = u32::from_le_bytes(c.try_into().unwrap());
    }
    let mask_b = mask(p, B);
    let mut idx = 0;
    let mut shift = p as i32 - B as i32;
    while shift >= 0 {
        let (dst, src) = (&mut ints[idx..idx + nips], &tmp[..nips]);
        for i in 0..nips {
            dst[i] = (src[i] >> shift) & mask_b;
        }
        idx += nips;
        shift -= B as i32;
    }
    let r = (shift + B as i32) as u32;
    if idx < num_ints {
        // Remaining values are a MSB-first bit stream spread over the low `r` bits of each
        // lane of tmp[0..], exactly as written by the second phase of `encode`.
        let mask_r = mask(p, r);
        let mut t = 0;
        let mut avail = 0u32;
        let mut cur = 0u32;
        while idx < num_ints {
            let mut val = 0u32;
            let mut need = B;
            while need > 0 {
                if avail == 0 {
                    cur = tmp[t] & mask_r;
                    t += 1;
                    avail = r;
                }
                let take = need.min(avail);
                val = (val << take) | ((cur >> (avail - take)) & mask(p, take));
                need -= take;
                avail -= take;
            }
            ints[idx] = val;
            idx += 1;
        }
    }
    match p {
        8 => expand8(ints),
        16 => expand16(ints),
        _ => {}
    }
}

#[cfg(test)]
macro_rules! dispatch {
    ($bpv:expr, $input:expr, $ints:expr, [$($n:literal)*]) => {
        match $bpv {
            $($n => decode_impl::<$n>($input, $ints),)*
            _ => unreachable!("bpv {}", $bpv),
        }
    };
}

/// Decode 256 values of `bpv` bits (generated straight-line decoders, see scripts/gen_forutil.py).
#[inline]
pub fn decode(bpv: u32, input: &mut In, ints: &mut [u32; BLOCK_SIZE]) {
    let n = num_bytes(bpv);
    super::forutil_gen::decode(bpv, &input.data[input.pos..input.pos + n], ints);
    input.pos += n;
}

/// Reference decoder that inverts `encode` with a generic bit-stream loop. Used to test the
/// generated decoders.
#[cfg(test)]
fn decode_reference(bpv: u32, input: &mut In, ints: &mut [u32; BLOCK_SIZE]) {
    dispatch!(bpv, input, ints, [1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32]);
}

/// PForUtil#encode. Mutates `ints`.
pub fn pfor_encode(ints: &mut [u32; BLOCK_SIZE], out: &mut Out) {
    let mut histogram = [0usize; 33];
    let mut max_bits = 0;
    for &v in ints.iter() {
        let b = bits_required(v);
        histogram[b as usize] += 1;
        max_bits = max_bits.max(b);
    }
    let min_bits = max_bits.saturating_sub(8);
    let mut cumulative = 0;
    let mut patched = max_bits;
    let mut num_exceptions = 0;
    let mut b = max_bits as i32;
    while b >= min_bits as i32 {
        if cumulative > MAX_EXCEPTIONS {
            break;
        }
        patched = b as u32;
        num_exceptions = cumulative;
        cumulative += histogram[b as usize];
        b -= 1;
    }
    let max_unpatched = low_bits(patched);
    let mut exceptions = Vec::with_capacity(num_exceptions * 2);
    if num_exceptions > 0 {
        for (i, v) in ints.iter_mut().enumerate() {
            if *v > max_unpatched {
                exceptions.push(i as u8);
                exceptions.push((*v >> patched) as u8);
                *v &= max_unpatched;
            }
        }
        debug_assert_eq!(exceptions.len(), num_exceptions * 2);
    }
    if ints.iter().all(|&v| v == ints[0]) && max_bits <= 8 {
        for i in 0..num_exceptions {
            exceptions[2 * i + 1] = ((exceptions[2 * i + 1] as u32) << patched) as u8;
        }
        out.write_byte((num_exceptions << 5) as u8);
        out.write_vint(ints[0]);
    } else {
        out.write_byte(((num_exceptions << 5) as u32 | patched) as u8);
        encode(ints, patched, out);
    }
    out.write_bytes(&exceptions);
}

/// PForUtil#decode
#[cfg_attr(feature = "profile", inline(never))]
pub fn pfor_decode(input: &mut In, ints: &mut [u32; BLOCK_SIZE]) {
    let token = input.read_byte() as u32;
    let bpv = token & 0x1F;
    if bpv == 0 {
        ints.fill(input.read_vint());
    } else {
        decode(bpv, input, ints);
    }
    for _ in 0..(token >> 5) {
        let i = input.read_byte() as usize;
        ints[i] |= (input.read_byte() as u32) << bpv;
    }
}

/// PForUtil#skip
#[cfg_attr(feature = "profile", inline(never))]
pub fn pfor_skip(input: &mut In) {
    let token = input.read_byte() as u32;
    let bpv = token & 0x1F;
    let num_exceptions = (token >> 5) as usize;
    if bpv == 0 {
        input.read_vlong();
        input.pos += num_exceptions << 1;
    } else {
        input.pos += num_bytes(bpv) + (num_exceptions << 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    #[test]
    fn for_roundtrip_all_widths() {
        let mut seed = 0x9E3779B97F4A7C15u64;
        for bpv in 1..=32u32 {
            for _ in 0..20 {
                let mut vals = [0u32; BLOCK_SIZE];
                for v in vals.iter_mut() {
                    *v = (rng(&mut seed) as u32) & low_bits(bpv);
                }
                vals[(rng(&mut seed) % 256) as usize] = low_bits(bpv); // force the max width
                let mut enc = vals;
                let mut out = Out::new();
                encode(&mut enc, bpv, &mut out);
                assert_eq!(out.len(), num_bytes(bpv));
                for f in [decode, decode_reference] {
                    let mut dec = [0u32; BLOCK_SIZE];
                    let mut input = In::new(&out.buf, 0);
                    f(bpv, &mut input, &mut dec);
                    assert_eq!(dec, vals, "bpv {bpv}");
                    assert_eq!(input.pos, out.len());
                }
            }
        }
    }

    #[test]
    fn pfor_roundtrip_with_exceptions() {
        let mut seed = 42u64;
        let cases: Vec<[u32; BLOCK_SIZE]> = vec![
            [1; BLOCK_SIZE],
            {
                let mut a = [1u32; BLOCK_SIZE];
                a[3] = 1000;
                a[200] = 70000;
                a
            },
            {
                let mut a = [0u32; BLOCK_SIZE];
                for v in a.iter_mut() {
                    *v = 1 + (rng(&mut seed) % 4) as u32;
                }
                a[17] = 300;
                a
            },
            {
                let mut a = [0u32; BLOCK_SIZE];
                for v in a.iter_mut() {
                    *v = 1 + (rng(&mut seed) % 100000) as u32;
                }
                a
            },
            {
                // all equal after patching, with exceptions (exercises the shifted-exception path)
                let mut a = [5u32; BLOCK_SIZE];
                a[0] = 200;
                a[9] = 133;
                a
            },
        ];
        for vals in cases {
            let mut enc = vals;
            let mut out = Out::new();
            pfor_encode(&mut enc, &mut out);
            out.write_byte(0xAB);
            let mut dec = [0u32; BLOCK_SIZE];
            let mut input = In::new(&out.buf, 0);
            pfor_decode(&mut input, &mut dec);
            assert_eq!(dec, vals);
            assert_eq!(input.read_byte(), 0xAB);
            let mut input = In::new(&out.buf, 0);
            pfor_skip(&mut input);
            assert_eq!(input.read_byte(), 0xAB);
        }
    }
}
