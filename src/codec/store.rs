//! Port of the parts of Lucene's DataOutput/DataInput used by the postings format.
//! Multi-byte fixed-width values are little-endian, like Lucene 9+.
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

#[derive(Default)]
pub struct Out {
    pub buf: Vec<u8>,
}

impl Out {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.buf.len()
    }
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
    }
    #[inline]
    pub fn write_byte(&mut self, b: u8) {
        self.buf.push(b);
    }
    #[inline]
    pub fn write_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    /// `other.copyTo(this)`
    #[inline]
    pub fn append(&mut self, other: &Self) {
        self.buf.extend_from_slice(&other.buf);
    }
    #[inline]
    pub fn write_short(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn write_int(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn write_long(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn write_vint(&mut self, mut v: u32) {
        while v & !0x7F != 0 {
            self.buf.push(((v & 0x7F) | 0x80) as u8);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    #[inline]
    pub fn write_vlong(&mut self, mut v: u64) {
        while v & !0x7F != 0 {
            self.buf.push(((v & 0x7F) | 0x80) as u8);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    #[inline]
    pub fn write_zlong(&mut self, v: i64) {
        self.write_vlong(((v << 1) ^ (v >> 63)) as u64);
    }
    /// Lucene104PostingsWriter#writeVInt15
    pub fn write_vint15(&mut self, v: u32) {
        self.write_vlong15(v as u64);
    }
    /// Lucene104PostingsWriter#writeVLong15
    pub fn write_vlong15(&mut self, v: u64) {
        if v & !0x7FFF == 0 {
            self.write_short(v as i16);
        } else {
            self.write_short((0x8000 | (v & 0x7FFF)) as u16 as i16);
            self.write_vlong(v >> 15);
        }
    }
    /// DataOutput#writeGroupVInts
    pub fn write_group_vints(&mut self, values: &[u32]) {
        let mut off = 0;
        while values.len() - off >= 4 {
            let flag_pos = self.buf.len();
            self.buf.push(0);
            let mut flag = 0u8;
            for shift in [6, 4, 2, 0] {
                let v = values[off];
                off += 1;
                let n = (32 - (v | 1).leading_zeros()).div_ceil(8);
                self.buf.extend_from_slice(&v.to_le_bytes()[..n as usize]);
                flag |= ((n - 1) as u8) << shift;
            }
            self.buf[flag_pos] = flag;
        }
        for &v in &values[off..] {
            self.write_vint(v);
        }
    }
}

pub struct In<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> In<'a> {
    #[inline]
    #[must_use]
    pub const fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }
    #[inline]
    pub const fn read_byte(&mut self) -> u8 {
        let b = self.data[self.pos];
        self.pos += 1;
        b
    }
    #[inline]
    pub fn read_short(&mut self) -> i16 {
        let d = &self.data[self.pos..self.pos + 2];
        let v = i16::from_le_bytes([d[0], d[1]]);
        self.pos += 2;
        v
    }
    #[inline]
    pub fn read_long(&mut self) -> u64 {
        let d = &self.data[self.pos..self.pos + 8];
        let v = u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
        self.pos += 8;
        v
    }
    #[inline]
    pub const fn read_vint(&mut self) -> u32 {
        let mut b = self.read_byte();
        if b < 0x80 {
            return b as u32;
        }
        let mut v = (b & 0x7F) as u32;
        let mut shift = 7;
        loop {
            b = self.read_byte();
            v |= ((b & 0x7F) as u32) << shift;
            if b < 0x80 {
                return v;
            }
            shift += 7;
        }
    }
    #[inline]
    pub const fn read_vlong(&mut self) -> u64 {
        let mut b = self.read_byte();
        if b < 0x80 {
            return b as u64;
        }
        let mut v = (b & 0x7F) as u64;
        let mut shift = 7;
        loop {
            b = self.read_byte();
            v |= ((b & 0x7F) as u64) << shift;
            if b < 0x80 {
                return v;
            }
            shift += 7;
        }
    }
    #[inline]
    pub const fn read_zlong(&mut self) -> i64 {
        let v = self.read_vlong();
        ((v >> 1) as i64) ^ -((v & 1) as i64)
    }
    #[inline]
    pub fn read_vint15(&mut self) -> u32 {
        let s = self.read_short();
        if s >= 0 {
            s as u32
        } else {
            (s as u32 & 0x7FFF) | (self.read_vint() << 15)
        }
    }
    #[inline]
    pub fn read_vlong15(&mut self) -> u64 {
        let s = self.read_short();
        if s >= 0 {
            s as u64
        } else {
            (s as u64 & 0x7FFF) | (self.read_vlong() << 15)
        }
    }
    /// GroupVIntUtil#readGroupVInts
    pub fn read_group_vints(&mut self, dst: &mut [i32], limit: usize) {
        let mut i = 0;
        while i + 4 <= limit {
            let flag = self.read_byte();
            for shift in [6, 4, 2, 0] {
                let n = (((flag >> shift) & 3) + 1) as usize;
                let mut v = [0u8; 4];
                v[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                dst[i] = u32::from_le_bytes(v) as i32;
                i += 1;
            }
        }
        while i < limit {
            dst[i] = self.read_vint() as i32;
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut o = Out::new();
        let vals = [
            0u32,
            1,
            127,
            128,
            255,
            256,
            65535,
            65536,
            1 << 24,
            u32::MAX,
            7,
            300,
        ];
        o.write_group_vints(&vals);
        o.write_vint15(32767);
        o.write_vint15(32768);
        o.write_vlong15(1 << 40);
        o.write_zlong(-57);
        o.write_zlong(1_234_567);
        let mut i = In::new(&o.buf, 0);
        let mut got = [0i32; 12];
        i.read_group_vints(&mut got, 12);
        assert_eq!(got.map(|x| x as u32), vals);
        assert_eq!(i.read_vint15(), 32767);
        assert_eq!(i.read_vint15(), 32768);
        assert_eq!(i.read_vlong15(), 1 << 40);
        assert_eq!(i.read_zlong(), -57);
        assert_eq!(i.read_zlong(), 1_234_567);
        assert_eq!(i.pos, o.len());
    }
}
