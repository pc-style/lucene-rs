//! `IndexWriter`: adds, updates and deletes documents, flushes segments, merges, commits.

use crate::analysis::{Analyzer, StandardAnalyzer};
use crate::codec::postings_reader::NO_MORE_DOCS;
use crate::document::Document;
use crate::error::{Error, Result};
use crate::index::Term;
use crate::index::buffer::DocumentsBuffer;
use crate::index::commit::{PREFIX, SegmentCommitInfo, SegmentInfos, sync_dir};
use crate::index::field_infos::FieldInfos;
use crate::index::live_docs::LiveDocs;
use crate::index::merge::{LogMergePolicy, merge_segments};
use crate::index::segment::{EXTENSIONS, SegmentReader, live_docs_file};
use crate::num::usize_from;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const WRITE_LOCK: &str = "write.lock";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Start a new, empty index (an existing one is replaced on the next commit).
    Create,
    /// Append to the existing index, or create one.
    CreateOrAppend,
}

#[derive(Clone)]
pub struct IndexWriterConfig {
    pub analyzer: Arc<dyn Analyzer>,
    /// Flush a segment once buffered documents use about this many megabytes.
    pub ram_buffer_size_mb: u32,
    /// Also flush after this many buffered documents.
    pub max_buffered_docs: Option<u32>,
    pub merge_policy: LogMergePolicy,
    pub open_mode: OpenMode,
}

impl Default for IndexWriterConfig {
    fn default() -> Self {
        Self {
            analyzer: Arc::new(StandardAnalyzer::new()),
            ram_buffer_size_mb: 64,
            max_buffered_docs: None,
            merge_policy: LogMergePolicy::default(),
            open_mode: OpenMode::CreateOrAppend,
        }
    }
}

impl IndexWriterConfig {
    pub fn new(analyzer: impl Analyzer + 'static) -> Self {
        Self {
            analyzer: Arc::new(analyzer),
            ..Self::default()
        }
    }
    #[must_use]
    pub const fn ram_buffer_size_mb(mut self, mb: u32) -> Self {
        self.ram_buffer_size_mb = mb;
        self
    }
    #[must_use]
    pub fn max_buffered_docs(mut self, n: u32) -> Self {
        self.max_buffered_docs = Some(n.max(1));
        self
    }
    #[must_use]
    pub const fn merge_policy(mut self, p: LogMergePolicy) -> Self {
        self.merge_policy = p;
        self
    }
    #[must_use]
    pub const fn open_mode(mut self, m: OpenMode) -> Self {
        self.open_mode = m;
        self
    }
}

/// Writes an index in a directory.
///
/// One writer per directory at a time (enforced with an OS lock on `write.lock`). Changes
/// become visible to readers after [`IndexWriter::commit`]; dropping a writer without
/// committing discards uncommitted changes.
pub struct IndexWriter {
    dir: PathBuf,
    config: IndexWriterConfig,
    infos: SegmentInfos,
    field_infos: FieldInfos,
    buffer: DocumentsBuffer,
    /// (term, number of buffered docs when the delete was issued)
    pending_deletes: Vec<(Term, u32)>,
    readers: HashMap<String, Arc<SegmentReader>>,
    live: HashMap<String, LiveDocs>,
    dirty_live: HashSet<String>,
    changed: bool,
    _lock: File,
}

impl IndexWriter {
    /// Opens (or creates) the index in `dir` for writing.
    ///
    /// # Errors
    /// [`Error::LockObtainFailed`] if another writer holds the lock, I/O errors, or
    /// [`Error::Corrupt`] if the existing commit cannot be read.
    pub fn open(dir: impl AsRef<Path>, config: IndexWriterConfig) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(WRITE_LOCK))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::LockObtainFailed(format!(
                    "{} is locked by another IndexWriter",
                    dir.display()
                )));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let (infos, changed) = match (SegmentInfos::latest_generation(&dir)?, config.open_mode) {
            (Some(g), OpenMode::CreateOrAppend) => (SegmentInfos::read(&dir, g)?, false),
            (Some(g), OpenMode::Create) => {
                let old = SegmentInfos::read(&dir, g)?;
                (
                    SegmentInfos {
                        generation: g,
                        counter: old.counter,
                        next_del_gen: old.next_del_gen,
                        segments: vec![],
                    },
                    true,
                )
            }
            (None, _) => (SegmentInfos::default(), true),
        };
        let mut w = Self {
            dir,
            config,
            infos,
            field_infos: FieldInfos::default(),
            buffer: DocumentsBuffer::new(),
            pending_deletes: Vec::new(),
            readers: HashMap::new(),
            live: HashMap::new(),
            dirty_live: HashSet::new(),
            changed,
            _lock: lock,
        };
        let names: Vec<String> = w.infos.segments.iter().map(|s| s.name.clone()).collect();
        for name in names {
            let r = w.reader(&name)?;
            for f in r.field_infos().iter() {
                w.field_infos.insert(f.clone())?;
            }
        }
        w.delete_unreferenced_files()?;
        Ok(w)
    }

    #[must_use]
    pub const fn config(&self) -> &IndexWriterConfig {
        &self.config
    }

    /// Live documents, including buffered ones (buffered deletes are not yet applied).
    #[must_use]
    pub fn num_docs(&self) -> u64 {
        self.infos
            .num_docs()
            .saturating_add(u64::from(self.buffer.max_doc))
    }

    /// Buffers a document; may flush a segment (and merge) when the buffer is full.
    ///
    /// # Errors
    /// [`Error::IllegalArgument`] for an invalid document (e.g. a field indexed inconsistently),
    /// or I/O errors from an automatic flush.
    pub fn add_document(&mut self, doc: &Document) -> Result<()> {
        let analyzer = self.config.analyzer.clone();
        self.buffer
            .add_document(doc, &*analyzer, &mut self.field_infos)?;
        self.changed = true;
        let ram_limit = usize_from(self.config.ram_buffer_size_mb).saturating_mul(1 << 20);
        let over_ram = self.buffer.ram_bytes_used() > ram_limit;
        let over_docs = self
            .config
            .max_buffered_docs
            .is_some_and(|n| self.buffer.max_doc >= n);
        if over_ram || over_docs {
            self.flush()?;
            self.maybe_merge()?;
        }
        Ok(())
    }

    /// Deletes all documents containing `term` (including buffered ones added before this
    /// call). Applied on the next flush or commit.
    pub fn delete_documents(&mut self, term: Term) {
        self.pending_deletes.push((term, self.buffer.max_doc));
        self.changed = true;
    }

    /// Atomically deletes documents containing `term` and adds `doc`.
    ///
    /// # Errors
    /// As for [`IndexWriter::add_document`].
    pub fn update_document(&mut self, term: Term, doc: &Document) -> Result<()> {
        self.delete_documents(term);
        self.add_document(doc)
    }

    /// Deletes every document (on the next commit).
    pub fn delete_all(&mut self) {
        self.buffer = DocumentsBuffer::new();
        self.pending_deletes.clear();
        self.infos.segments.clear();
        self.readers.clear();
        self.live.clear();
        self.dirty_live.clear();
        self.changed = true;
    }

    fn reader(&mut self, name: &str) -> Result<Arc<SegmentReader>> {
        if let Some(r) = self.readers.get(name) {
            return Ok(r.clone());
        }
        let r = Arc::new(SegmentReader::open(&self.dir, name, 0)?);
        self.readers.insert(name.to_string(), r.clone());
        Ok(r)
    }

    fn live_docs_mut(&mut self, seg: &SegmentCommitInfo) -> Result<&mut LiveDocs> {
        let live = match self.live.entry(seg.name.clone()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(if seg.del_gen > 0 {
                let file = live_docs_file(&seg.name, seg.del_gen);
                LiveDocs::decode(&fs::read(self.dir.join(&file))?, seg.max_doc, &file)?
            } else {
                LiveDocs::all_live(seg.max_doc)
            }),
        };
        Ok(live)
    }

    /// Writes buffered documents as a new segment and applies buffered deletes. The result is
    /// not visible to readers until `commit`.
    ///
    /// # Errors
    /// I/O errors while writing segment files.
    pub fn flush(&mut self) -> Result<()> {
        let new_segment = if self.buffer.is_empty() {
            None
        } else {
            let name = self.infos.new_segment_name();
            let buffer = std::mem::replace(&mut self.buffer, DocumentsBuffer::new());
            let info = buffer.flush(&self.dir, name.clone(), &self.field_infos)?;
            self.infos.segments.push(SegmentCommitInfo {
                name: name.clone(),
                max_doc: info.max_doc,
                del_gen: 0,
                del_count: 0,
            });
            Some(name)
        };
        let deletes = std::mem::take(&mut self.pending_deletes);
        let segments = self.infos.segments.clone();
        for (term, upto) in &deletes {
            for seg in &segments {
                // Deletes apply to docs added before them: all docs of older segments, the
                // first `upto` docs of the segment just flushed.
                let limit = if new_segment.as_deref() == Some(seg.name.as_str()) {
                    *upto
                } else {
                    u32::MAX
                };
                let reader = self.reader(&seg.name)?;
                let Some((field, meta)) = reader.term_meta(&term.field, &term.bytes) else {
                    continue;
                };
                let mut docs = Vec::new();
                let mut pe = reader.postings(&field, &meta, false);
                while let Ok(d) = u32::try_from(pe.next_doc()) {
                    if d >= limit || pe.doc_id() == NO_MORE_DOCS {
                        break;
                    }
                    docs.push(d);
                }
                let live = self.live_docs_mut(seg)?;
                let changed = docs.into_iter().fold(false, |acc, d| live.delete(d) | acc);
                if changed {
                    self.dirty_live.insert(seg.name.clone());
                }
            }
        }
        self.write_live_docs()
    }

    fn write_live_docs(&mut self) -> Result<()> {
        let dirty: Vec<String> = self.dirty_live.drain().collect();
        for name in dirty {
            let Some(live) = self.live.get(&name).cloned() else {
                continue;
            };
            let del_count = live.max_doc().saturating_sub(live.num_live());
            if del_count == live.max_doc() {
                self.infos.segments.retain(|s| s.name != name); // fully deleted segment
                self.live.remove(&name);
                self.readers.remove(&name);
                continue;
            }
            let del_gen = self.infos.new_del_gen();
            let path = self.dir.join(live_docs_file(&name, del_gen));
            fs::write(&path, live.encode())?;
            File::open(&path)?.sync_all()?;
            if let Some(s) = self.infos.segments.iter_mut().find(|s| s.name == name) {
                s.del_gen = del_gen;
                s.del_count = del_count;
            }
        }
        Ok(())
    }

    fn merge_range(&mut self, range: Range<usize>) -> Result<()> {
        let segs: Vec<SegmentCommitInfo> = self
            .infos
            .segments
            .get(range.clone())
            .unwrap_or_default()
            .to_vec();
        let readers: Vec<SegmentReader> = segs
            .iter()
            .map(|s| SegmentReader::open(&self.dir, &s.name, s.del_gen))
            .collect::<Result<_>>()?;
        let name = self.infos.new_segment_name();
        let refs: Vec<&SegmentReader> = readers.iter().collect();
        let merged = merge_segments(&self.dir, &refs, name.clone(), &self.field_infos)?;
        let replacement = merged.map(|info| SegmentCommitInfo {
            name,
            max_doc: info.max_doc,
            del_gen: 0,
            del_count: 0,
        });
        self.infos.segments.splice(range, replacement);
        for s in &segs {
            self.readers.remove(&s.name);
            self.live.remove(&s.name);
        }
        self.changed = true;
        Ok(())
    }

    fn live_sizes(&self) -> Vec<u32> {
        self.infos
            .segments
            .iter()
            .map(|s| s.max_doc.saturating_sub(s.del_count))
            .collect()
    }

    fn maybe_merge(&mut self) -> Result<()> {
        loop {
            let merges = self.config.merge_policy.find_merges(&self.live_sizes());
            if merges.is_empty() {
                return Ok(());
            }
            for r in merges.into_iter().rev() {
                self.merge_range(r)?;
            }
        }
    }

    /// Merges down to at most `max_segments` segments (expunging deletions); `commit`
    /// afterwards to publish. With `max_segments == 1` the index ends up as one segment.
    ///
    /// # Errors
    /// [`Error::IllegalArgument`] if `max_segments` is 0, or I/O errors.
    pub fn force_merge(&mut self, max_segments: usize) -> Result<()> {
        if max_segments == 0 {
            return Err(Error::IllegalArgument(
                "max_segments must be at least 1".into(),
            ));
        }
        self.flush()?;
        let n = self.infos.segments.len();
        if max_segments == 1 {
            if n > 1 || self.infos.segments.first().is_some_and(|s| s.del_count > 0) {
                self.merge_range(0..n)?;
            }
            return Ok(());
        }
        while self.infos.segments.len() > max_segments {
            let sizes = self.live_sizes();
            // merge the adjacent pair with the fewest live docs
            let best = sizes
                .windows(2)
                .enumerate()
                .min_by_key(|(_, w)| w.iter().map(|&s| u64::from(s)).sum::<u64>())
                .map(|(i, _)| i);
            let Some(i) = best else { break };
            self.merge_range(i..i.saturating_add(2))?;
        }
        Ok(())
    }

    /// Flushes, runs merges, and atomically publishes a new commit point.
    ///
    /// # Errors
    /// I/O errors; on error the previous commit stays intact.
    pub fn commit(&mut self) -> Result<()> {
        self.flush()?;
        self.maybe_merge()?;
        if !self.changed {
            return Ok(());
        }
        self.infos.generation = self.infos.generation.saturating_add(1);
        self.infos.write(&self.dir)?;
        self.changed = false;
        self.delete_unreferenced_files()
    }

    /// Commits and releases the write lock.
    ///
    /// # Errors
    /// As for [`IndexWriter::commit`].
    pub fn close(mut self) -> Result<()> {
        self.commit()
    }

    /// Deletes index files that neither the newest commit nor pending state reference.
    fn delete_unreferenced_files(&self) -> Result<()> {
        let mut keep: HashSet<String> = HashSet::new();
        keep.insert(WRITE_LOCK.to_string());
        if let Some(g) = SegmentInfos::latest_generation(&self.dir)? {
            keep.insert(crate::index::commit::segments_file(g));
            if g != self.infos.generation {
                // keep what the newest on-disk commit references
                for s in SegmentInfos::read(&self.dir, g)?.segments {
                    keep_segment(&mut keep, &s);
                }
            }
        }
        for s in &self.infos.segments {
            keep_segment(&mut keep, s);
        }
        for e in fs::read_dir(&self.dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            let ours =
                name.starts_with('_') || name.starts_with(PREFIX) || name.starts_with("pending_");
            if ours && !keep.contains(&name) {
                let _ = fs::remove_file(self.dir.join(&name)); // may fail on Windows if open
            }
        }
        sync_dir(&self.dir);
        Ok(())
    }
}

fn keep_segment(keep: &mut HashSet<String>, s: &SegmentCommitInfo) {
    for ext in EXTENSIONS {
        keep.insert(format!("{}.{ext}", s.name));
    }
    if s.del_gen > 0 {
        keep.insert(live_docs_file(&s.name, s.del_gen));
    }
}
