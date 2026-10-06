//! In-memory inversion of buffered documents (Lucene's `DocumentsWriterPerThread` +
//! `IndexingChain`), flushed as one segment.
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

use crate::analysis::Analyzer;
use crate::codec::store::In;
use crate::document::{Document, FieldValue};
use crate::error::{Error, Result};
use crate::index::field_infos::FieldInfos;
use crate::index::segment::SegmentInfo;
use crate::index::segment_writer::SegmentWriter;
use crate::index::stored::StoredFieldsWriter;
use crate::sim::int_to_byte4;
use rustc_hash::FxHashMap;
use std::path::Path;

struct TermBuf {
    last_doc: i32,
    /// Per doc: vInt(docDelta) [vInt(freq)] [freq x vInt(positionDelta)]
    buf: Vec<u8>,
}

struct FieldBuffer {
    has_freqs: bool,
    has_positions: bool,
    has_norms: bool,
    ids: FxHashMap<Box<[u8]>, u32>,
    terms: Vec<TermBuf>,
    norms: Vec<u8>,
}

/// One field of the document being inverted.
#[derive(Default)]
struct FieldAcc {
    number: u32,
    pairs: Vec<(u32, u32)>, // (term id, position)
    length: u32,
    position: i64,
}

pub struct DocumentsBuffer {
    fields: Vec<Option<FieldBuffer>>,
    stored: StoredFieldsWriter,
    pub max_doc: u32,
    bytes_used: usize,
    accs: Vec<FieldAcc>,
}

#[inline]
fn push_vint(buf: &mut Vec<u8>, mut v: u32) {
    while v & !0x7F != 0 {
        buf.push(((v & 0x7F) | 0x80) as u8);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// Checks every field and returns its number, so a bad document leaves no partial state.
fn validate(doc: &Document, infos: &mut FieldInfos) -> Result<Vec<u32>> {
    doc.fields()
        .iter()
        .map(|f| {
            if f.field_type.is_indexed() && f.value.as_str().is_none() {
                return Err(Error::IllegalArgument(format!(
                    "field \"{}\": only text values can be indexed",
                    f.name
                )));
            }
            if !f.field_type.is_indexed() && !f.field_type.stored {
                return Err(Error::IllegalArgument(format!(
                    "field \"{}\" is neither indexed nor stored",
                    f.name
                )));
            }
            infos.get_or_add(&f.name, f.field_type)
        })
        .collect()
}

impl DocumentsBuffer {
    pub fn new() -> Self {
        Self {
            fields: Vec::new(),
            stored: StoredFieldsWriter::new(),
            max_doc: 0,
            bytes_used: 0,
            accs: Vec::new(),
        }
    }

    pub const fn ram_bytes_used(&self) -> usize {
        self.bytes_used + self.stored.ram_bytes()
    }

    pub const fn is_empty(&self) -> bool {
        self.max_doc == 0
    }

    pub fn add_document(
        &mut self,
        doc: &Document,
        analyzer: &dyn Analyzer,
        infos: &mut FieldInfos,
    ) -> Result<()> {
        let numbers = validate(doc, infos)?;
        let doc_id = self.max_doc;
        let mut accs = std::mem::take(&mut self.accs);
        let used = self.invert(doc, &numbers, analyzer, infos, &mut accs);
        for acc in &mut accs[..used] {
            self.append_postings(doc_id, acc);
        }
        self.accs = accs;
        self.stored.add_document(
            doc.fields()
                .iter()
                .zip(&numbers)
                .filter(|(f, _)| f.field_type.stored)
                .map(|(f, &n)| (n, &f.value as &FieldValue)),
        )?;
        self.max_doc += 1;
        Ok(())
    }

    /// Tokenizes every indexed field of the document into `accs` (one per distinct field);
    /// returns how many entries of `accs` are in use.
    fn invert(
        &mut self,
        doc: &Document,
        numbers: &[u32],
        analyzer: &dyn Analyzer,
        infos: &FieldInfos,
        accs: &mut Vec<FieldAcc>,
    ) -> usize {
        let mut used = 0;
        for (f, &number) in doc.fields().iter().zip(numbers) {
            let (Some(text), Some(info)) = (f.value.as_str(), infos.by_number(number)) else {
                continue;
            };
            if !f.field_type.is_indexed() {
                continue;
            }
            if self.fields.len() <= number as usize {
                self.fields.resize_with(number as usize + 1, || None);
            }
            let fb = self.fields[number as usize].get_or_insert_with(|| FieldBuffer {
                has_freqs: info.index_options.has_freqs(),
                has_positions: info.index_options.has_positions(),
                has_norms: info.has_norms,
                ids: FxHashMap::default(),
                terms: Vec::new(),
                norms: Vec::new(),
            });
            let acc_idx = accs[..used]
                .iter()
                .position(|a| a.number == number)
                .unwrap_or_else(|| {
                    if accs.len() == used {
                        accs.push(FieldAcc::default());
                    }
                    let a = &mut accs[used];
                    a.number = number;
                    a.pairs.clear();
                    a.length = 0;
                    a.position = -1;
                    used += 1;
                    used - 1
                });
            let acc = &mut accs[acc_idx];
            let bytes_used = &mut self.bytes_used;
            let mut add = |term: &str, inc: u32| {
                acc.position += inc as i64;
                if inc > 0 {
                    acc.length += 1; // discountOverlaps: stacked tokens don't add length
                }
                let id = fb.ids.get(term.as_bytes()).copied().unwrap_or_else(|| {
                    let id = fb.terms.len() as u32;
                    fb.ids.insert(term.as_bytes().into(), id);
                    fb.terms.push(TermBuf {
                        last_doc: -1,
                        buf: Vec::new(),
                    });
                    *bytes_used += term.len() + 64;
                    id
                });
                acc.pairs.push((id, acc.position.max(0) as u32));
            };
            if f.field_type.tokenized {
                analyzer.analyze(&f.name, text, &mut add);
            } else {
                add(text, 1);
            }
        }
        used
    }

    /// Appends one document's postings (and norm) for one field.
    fn append_postings(&mut self, doc_id: u32, acc: &mut FieldAcc) {
        let Some(fb) = self
            .fields
            .get_mut(acc.number as usize)
            .and_then(Option::as_mut)
        else {
            return;
        };
        acc.pairs.sort_by_key(|p| p.0); // stable: positions stay in order
        for group in acc.pairs.chunk_by(|a, b| a.0 == b.0) {
            let tb = &mut fb.terms[group[0].0 as usize];
            let before = tb.buf.capacity();
            push_vint(&mut tb.buf, (doc_id as i32 - tb.last_doc) as u32);
            if fb.has_freqs {
                push_vint(&mut tb.buf, group.len() as u32);
            }
            if fb.has_positions {
                let mut last = 0;
                for &(_, p) in group {
                    push_vint(&mut tb.buf, p - last);
                    last = p;
                }
            }
            tb.last_doc = doc_id as i32;
            self.bytes_used += tb.buf.capacity() - before;
        }
        if fb.has_norms {
            fb.norms.resize(doc_id as usize, 0);
            fb.norms.push(int_to_byte4(acc.length));
            self.bytes_used += 1;
        }
    }

    /// Writes the buffered documents as segment `name`.
    pub fn flush(self, dir: &Path, name: String, infos: &FieldInfos) -> Result<SegmentInfo> {
        let max_doc = self.max_doc;
        let mut w = SegmentWriter::new(name, max_doc, infos.clone());
        for (number, fb) in self.fields.into_iter().enumerate() {
            let Some(mut fb) = fb else { continue };
            if fb.terms.is_empty() {
                continue;
            }
            let norms = fb.has_norms.then(|| {
                fb.norms.resize(max_doc as usize, 0);
                fb.norms
            });
            w.start_field(number as u32, norms.as_deref())?;
            let mut order: Vec<(Box<[u8]>, u32)> = fb.ids.into_iter().collect();
            order.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (term, id) in order {
                let tb = &fb.terms[id as usize];
                w.start_term();
                let mut input = In::new(&tb.buf, 0);
                let mut doc: i32 = -1;
                while input.pos < tb.buf.len() {
                    doc += input.read_vint() as i32;
                    let freq = if fb.has_freqs { input.read_vint() } else { 1 };
                    w.start_doc(doc as u32, freq);
                    if fb.has_positions {
                        let mut p = 0;
                        for _ in 0..freq {
                            p += input.read_vint();
                            w.add_position(p);
                        }
                    }
                    w.finish_doc();
                }
                w.finish_term(&term);
            }
            w.finish_field();
        }
        w.set_stored(self.stored);
        w.finish(dir)
    }
}
