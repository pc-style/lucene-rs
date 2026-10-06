//! Lossless integer conversions that the standard library does not offer as `From` impls.

use crate::error::{Error, Result};

const _: () = assert!(
    usize::BITS >= 32,
    "lucene-rs requires a usize of at least 32 bits"
);

/// `u32` to `usize` (lossless: `usize` is at least 32 bits, checked above).
#[allow(clippy::as_conversions)]
#[inline]
pub const fn usize_from(v: u32) -> usize {
    v as usize
}

/// `usize` to `u64` (lossless on every supported target).
#[allow(clippy::as_conversions)]
#[inline]
pub const fn u64_from(v: usize) -> u64 {
    v as u64
}

/// `usize` to `u32`, failing with `IllegalArgument` past `u32::MAX`.
pub fn u32_from(v: usize, what: &str) -> Result<u32> {
    u32::try_from(v).map_err(|_| Error::IllegalArgument(format!("too many {what}: {v}")))
}

/// `(float) x` for a double, as Lucene does when narrowing summed boosts.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
#[inline]
pub const fn f32_from_f64(v: f64) -> f32 {
    v as f32
}
