//! File headers and checksummed footers, after Lucene's `CodecUtil`.

use crate::codec::store::{In, Out};
use crate::error::{Result, corrupt};
use crate::num::u32_from;

const CODEC_MAGIC: u32 = 0x3fd7_6c17;
const FOOTER_MAGIC: u32 = !CODEC_MAGIC;
pub const FOOTER_LEN: usize = 16;

/// Magic, codec name, version. File offsets stored elsewhere are absolute, header included.
pub fn write_header(out: &mut Out, codec: &str, version: u32) {
    out.write_bytes(&CODEC_MAGIC.to_be_bytes());
    write_string(out, codec);
    out.write_int(version);
}

/// Validates the header (and the footer's presence) and returns the offset just past it.
pub fn check_header(data: &[u8], codec: &str, version: u32, file: &str) -> Result<usize> {
    if data.len() < FOOTER_LEN.saturating_add(4)
        || data.get(..4) != Some(&CODEC_MAGIC.to_be_bytes()[..])
    {
        return Err(corrupt(format!("{file}: bad header magic")));
    }
    let mut i = In::new(data, 4);
    if read_string(&mut i, file)? != codec {
        return Err(corrupt(format!("{file}: expected codec {codec}")));
    }
    let v = u32::from_le_bytes(take::<4>(&mut i, file)?);
    if v != version {
        return Err(corrupt(format!(
            "{file}: unsupported version {v} (expected {version})"
        )));
    }
    check_footer_magic(data, file)?;
    Ok(i.pos)
}

/// Footer magic, algorithm id (0 = CRC32), CRC32 of everything before the checksum.
pub fn write_footer(out: &mut Out) {
    out.write_bytes(&FOOTER_MAGIC.to_be_bytes());
    out.write_int(0);
    let crc = crc32fast::hash(&out.buf);
    out.write_long(u64::from(crc));
}

fn check_footer_magic(data: &[u8], file: &str) -> Result<()> {
    let start = data
        .len()
        .checked_sub(FOOTER_LEN)
        .ok_or_else(|| corrupt(format!("{file}: too short")))?;
    if data.get(start..start.saturating_add(4)) != Some(&FOOTER_MAGIC.to_be_bytes()[..]) {
        return Err(corrupt(format!(
            "{file}: bad footer magic (truncated file?)"
        )));
    }
    Ok(())
}

/// Recomputes the CRC32 of the whole file (Lucene's `checksumEntireFile`).
pub fn verify_checksum(data: &[u8], file: &str) -> Result<()> {
    check_footer_magic(data, file)?;
    let split = data.len().saturating_sub(8);
    let (body, stored) = data.split_at(split);
    let expected = u64::from_le_bytes(
        stored
            .try_into()
            .map_err(|_| corrupt(format!("{file}: too short")))?,
    );
    let actual = u64::from(crc32fast::hash(body));
    if actual != expected {
        return Err(corrupt(format!(
            "{file}: checksum mismatch ({actual:#x} != {expected:#x})"
        )));
    }
    Ok(())
}

/// The next `N` bytes, or a corruption error if the input is too short.
fn take<const N: usize>(i: &mut In, file: &str) -> Result<[u8; N]> {
    let end = i.pos.saturating_add(N);
    let bytes = i
        .data
        .get(i.pos..end)
        .and_then(|b| <[u8; N]>::try_from(b).ok());
    let bytes = bytes.ok_or_else(|| corrupt(format!("{file}: truncated")))?;
    i.pos = end;
    Ok(bytes)
}

pub fn write_string(out: &mut Out, s: &str) {
    // strings here are field/segment/codec names; anything near 4GB is a caller bug
    out.write_vint(u32_from(s.len(), "bytes in a name").unwrap_or(u32::MAX));
    out.write_bytes(s.as_bytes());
}

pub fn read_string(i: &mut In, file: &str) -> Result<String> {
    let n = crate::num::usize_from(i.read_vint());
    let end = i.pos.saturating_add(n);
    let b = i
        .data
        .get(i.pos..end)
        .ok_or_else(|| corrupt(format!("{file}: truncated string")))?;
    i.pos = end;
    String::from_utf8(b.to_vec()).map_err(|_| corrupt(format!("{file}: invalid UTF-8")))
}
