//! `DirectoryReader`: a point-in-time view of the newest commit.

use crate::document::Document;
use crate::error::{Error, Result};
use crate::index::commit::SegmentInfos;
use crate::index::segment::SegmentReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Index-wide statistics of a field (Lucene's `CollectionStatistics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectionStats {
    pub max_doc: u64,
    pub doc_count: u64,
    pub sum_total_term_freq: u64,
    pub sum_doc_freq: u64,
}

/// Index-wide statistics of a term (Lucene's `TermStatistics`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermStats {
    pub doc_freq: u64,
    pub total_term_freq: u64,
}

/// An immutable view of one commit. Cheap to clone (segments are shared).
#[derive(Clone)]
pub struct DirectoryReader {
    dir: PathBuf,
    generation: u64,
    segments: Vec<Arc<SegmentReader>>,
    doc_bases: Vec<u32>,
    max_doc: u32,
}

impl DirectoryReader {
    /// Opens the newest commit in `dir`.
    ///
    /// # Errors
    /// [`Error::IndexNotFound`] if `dir` has no commit, I/O errors, or [`Error::Corrupt`].
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let infos = SegmentInfos::read_latest(&dir)?;
        Self::from_infos(dir, &infos, &[])
    }

    fn from_infos(
        dir: PathBuf,
        infos: &SegmentInfos,
        reuse: &[Arc<SegmentReader>],
    ) -> Result<Self> {
        let mut segments = Vec::with_capacity(infos.segments.len());
        let mut doc_bases = Vec::with_capacity(infos.segments.len());
        let mut base = 0u32;
        let too_big =
            || Error::IllegalArgument(format!("index has more than {} documents", i32::MAX));
        for s in &infos.segments {
            let r = match reuse
                .iter()
                .find(|r| r.name() == s.name && r.del_gen == s.del_gen)
            {
                Some(r) => r.clone(),
                None => Arc::new(SegmentReader::open(&dir, &s.name, s.del_gen)?),
            };
            doc_bases.push(base);
            base = base
                .checked_add(r.max_doc())
                .filter(|&b| i32::try_from(b).is_ok())
                .ok_or_else(too_big)?;
            segments.push(r);
        }
        Ok(Self {
            dir,
            generation: infos.generation,
            segments,
            doc_bases,
            max_doc: base,
        })
    }

    /// A reader on the newest commit if it changed, reusing unchanged segments; else `None`.
    ///
    /// # Errors
    /// As for [`DirectoryReader::open`].
    pub fn reopen(&self) -> Result<Option<Self>> {
        match SegmentInfos::latest_generation(&self.dir)? {
            Some(g) if g != self.generation => {
                let infos = SegmentInfos::read(&self.dir, g)?;
                Ok(Some(Self::from_infos(
                    self.dir.clone(),
                    &infos,
                    &self.segments,
                )?))
            }
            _ => Ok(None),
        }
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub fn segments(&self) -> &[Arc<SegmentReader>] {
        &self.segments
    }
    /// The global doc ID of each segment's first document.
    #[must_use]
    pub fn doc_bases(&self) -> &[u32] {
        &self.doc_bases
    }
    #[must_use]
    pub const fn max_doc(&self) -> u32 {
        self.max_doc
    }
    #[must_use]
    pub fn num_docs(&self) -> u32 {
        self.segments.iter().map(|s| s.num_docs()).sum()
    }
    #[must_use]
    pub fn num_deleted_docs(&self) -> u32 {
        self.max_doc.saturating_sub(self.num_docs())
    }

    /// Segment and segment-local doc ID of a global doc ID.
    #[must_use]
    pub fn locate(&self, doc: u32) -> Option<(&SegmentReader, u32)> {
        if doc >= self.max_doc {
            return None;
        }
        let i = self
            .doc_bases
            .partition_point(|&b| b <= doc)
            .checked_sub(1)?;
        Some((
            self.segments.get(i)?,
            doc.checked_sub(*self.doc_bases.get(i)?)?,
        ))
    }

    /// Stored fields of a document.
    ///
    /// # Errors
    /// [`Error::IllegalArgument`] if `doc` is out of range, [`Error::Corrupt`] for damaged data.
    pub fn document(&self, doc: u32) -> Result<Document> {
        let (seg, local) = self
            .locate(doc)
            .ok_or_else(|| Error::IllegalArgument(format!("doc {doc} out of range")))?;
        seg.document(local)
    }

    #[must_use]
    pub fn collection_stats(&self, field: &str) -> Option<CollectionStats> {
        let mut s = CollectionStats {
            max_doc: u64::from(self.max_doc),
            doc_count: 0,
            sum_total_term_freq: 0,
            sum_doc_freq: 0,
        };
        for f in self
            .segments
            .iter()
            .filter_map(|seg| seg.field_stats(field))
        {
            s.doc_count = s.doc_count.saturating_add(f.doc_count);
            s.sum_total_term_freq = s.sum_total_term_freq.saturating_add(f.sum_total_term_freq);
            s.sum_doc_freq = s.sum_doc_freq.saturating_add(f.sum_doc_freq);
        }
        (s.doc_count > 0).then_some(s)
    }

    #[must_use]
    pub fn term_stats(&self, field: &str, term: &[u8]) -> Option<TermStats> {
        let mut s = TermStats {
            doc_freq: 0,
            total_term_freq: 0,
        };
        for (_, m) in self
            .segments
            .iter()
            .filter_map(|seg| seg.term_meta(field, term))
        {
            s.doc_freq = s.doc_freq.saturating_add(u64::from(m.doc_freq));
            s.total_term_freq = s.total_term_freq.saturating_add(m.total_term_freq);
        }
        (s.doc_freq > 0).then_some(s)
    }

    /// Verifies every file's checksum.
    ///
    /// # Errors
    /// [`Error::Corrupt`] naming the first file that fails.
    pub fn check_integrity(&self) -> Result<()> {
        self.segments.iter().try_for_each(|s| s.check_integrity())
    }
}
