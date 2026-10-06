//! Commit points (Lucene's `SegmentInfos`): `segments_N` lists the segments of generation N and
//! their deletion generations. Written to a temporary file, fsynced, then renamed.

use crate::codec::store::{In, Out};
use crate::error::{Error, Result, corrupt};
use crate::index::codec_util::{
    check_header, read_string, verify_checksum, write_footer, write_header, write_string,
};
use crate::num::{u32_from, usize_from};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

const CODEC: &str = "LuceneRsSegments";
const VERSION: u32 = 1;
pub const PREFIX: &str = "segments_";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentCommitInfo {
    pub name: String,
    pub max_doc: u32,
    /// 0 = no deletions; otherwise the `.liv` generation.
    pub del_gen: u64,
    pub del_count: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SegmentInfos {
    pub generation: u64,
    /// Source of new segment names (`_0`, `_1`, ... in base 36, like Lucene).
    pub counter: u64,
    /// Source of new `.liv` generations.
    pub next_del_gen: u64,
    pub segments: Vec<SegmentCommitInfo>,
}

pub fn segments_file(generation: u64) -> String {
    format!("{PREFIX}{generation}")
}

fn to_base36(mut n: u64) -> String {
    let mut digits = Vec::new();
    loop {
        let d = u32::try_from(n.rem_euclid(36)).unwrap_or(0);
        digits.push(char::from_digit(d, 36).unwrap_or('0'));
        n = n.div_euclid(36);
        if n == 0 {
            break;
        }
    }
    digits.iter().rev().collect()
}

impl SegmentInfos {
    pub fn new_segment_name(&mut self) -> String {
        let n = self.counter;
        self.counter = self.counter.saturating_add(1);
        format!("_{}", to_base36(n))
    }

    pub const fn new_del_gen(&mut self) -> u64 {
        self.next_del_gen = self.next_del_gen.saturating_add(1);
        self.next_del_gen
    }

    /// Generation of the newest `segments_N` in `dir`, if any.
    pub fn latest_generation(dir: &Path) -> Result<Option<u64>> {
        let mut best = None;
        for e in fs::read_dir(dir)? {
            let name = e?.file_name();
            let name = name.to_string_lossy();
            if let Some(g) = name
                .strip_prefix(PREFIX)
                .and_then(|g| g.parse::<u64>().ok())
            {
                best = best.max(Some(g));
            }
        }
        Ok(best)
    }

    pub fn read_latest(dir: &Path) -> Result<Self> {
        let generation = Self::latest_generation(dir)?.ok_or_else(|| {
            Error::IndexNotFound(format!("no {PREFIX}N file in {}", dir.display()))
        })?;
        Self::read(dir, generation)
    }

    pub fn read(dir: &Path, generation: u64) -> Result<Self> {
        let file = segments_file(generation);
        let data = fs::read(dir.join(&file))?;
        verify_checksum(&data, &file)?;
        let start = check_header(&data, CODEC, VERSION, &file)?;
        let mut i = In::new(&data, start);
        let g = i.read_vlong();
        if g != generation {
            return Err(corrupt(format!("{file}: generation {g} != {generation}")));
        }
        let counter = i.read_vlong();
        let next_del_gen = i.read_vlong();
        let n = i.read_vint();
        let mut segments = Vec::with_capacity(usize_from(n.min(4096)));
        for _ in 0..n {
            segments.push(SegmentCommitInfo {
                name: read_string(&mut i, &file)?,
                max_doc: i.read_vint(),
                del_gen: i.read_vlong(),
                del_count: i.read_vint(),
            });
        }
        Ok(Self {
            generation,
            counter,
            next_del_gen,
            segments,
        })
    }

    /// Writes `segments_{generation}` atomically.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let mut out = Out::new();
        write_header(&mut out, CODEC, VERSION);
        out.write_vlong(self.generation);
        out.write_vlong(self.counter);
        out.write_vlong(self.next_del_gen);
        out.write_vint(u32_from(self.segments.len(), "segments")?);
        for s in &self.segments {
            write_string(&mut out, &s.name);
            out.write_vint(s.max_doc);
            out.write_vlong(s.del_gen);
            out.write_vint(s.del_count);
        }
        write_footer(&mut out);
        let file = segments_file(self.generation);
        let tmp = dir.join(format!("pending_{file}"));
        write_synced(&tmp, &out.buf)?;
        fs::rename(&tmp, dir.join(&file))?;
        sync_dir(dir);
        Ok(())
    }

    pub fn num_docs(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| u64::from(s.max_doc.saturating_sub(s.del_count)))
            .sum()
    }
}

/// Writes `data` to `path` and fsyncs it through the same handle. Reopening the file read-only
/// to sync it fails on Windows, where `FlushFileBuffers` needs write access.
pub fn write_synced(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = File::create(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

/// Best-effort directory fsync so renames survive a crash (no-op where unsupported).
pub fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}
