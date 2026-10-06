//! Writes a segment from sorted postings (used by flushes and merges). Runs once per posting,
//! so it is treated as codec code.
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

use crate::codec::postings_writer::{PostingsWriter, TermMeta};
use crate::codec::store::Out;
use crate::document::IndexOptions;
use crate::error::{Error, Result};
use crate::index::codec_util::{write_footer, write_header};
use crate::index::commit::write_synced;
use crate::index::field_infos::FieldInfos;
use crate::index::segment::{
    DOC_CODEC, FieldStats, NRM_CODEC, POS_CODEC, SegmentInfo, TIM_CODEC, VERSION,
};
use crate::index::stored::StoredFieldsWriter;
use crate::index::terms::FieldTermsWriter;
use std::path::Path;

/// Writes a segment field by field, term by term, doc by doc (all in sorted order).
pub struct SegmentWriter {
    name: String,
    max_doc: u32,
    fields: FieldInfos,
    stats: Vec<Option<FieldStats>>,
    pw: PostingsWriter,
    tim: Out,
    nrm: Out,
    stored: Option<StoredFieldsWriter>,
    // current field
    field: Option<(u32, FieldTermsWriter, IndexOptions)>,
    norms: Vec<u8>,
    has_norms: bool,
    docs_seen: Vec<u64>,
    sum_ttf: u64,
    sum_df: u64,
    // current term
    term_started: bool,
    df: u32,
    ttf: u64,
}

impl SegmentWriter {
    pub fn new(name: String, max_doc: u32, fields: FieldInfos) -> Self {
        let mut pw = PostingsWriter::new();
        write_header(&mut pw.doc_out, DOC_CODEC, VERSION);
        write_header(&mut pw.pos_out, POS_CODEC, VERSION);
        let mut tim = Out::new();
        write_header(&mut tim, TIM_CODEC, VERSION);
        let mut nrm = Out::new();
        write_header(&mut nrm, NRM_CODEC, VERSION);
        Self {
            name,
            max_doc,
            fields,
            stats: Vec::new(),
            pw,
            tim,
            nrm,
            stored: None,
            field: None,
            norms: Vec::new(),
            has_norms: false,
            docs_seen: Vec::new(),
            sum_ttf: 0,
            sum_df: 0,
            term_started: false,
            df: 0,
            ttf: 0,
        }
    }

    /// Starts writing an indexed field. `norms` (one byte per doc) is required iff the field
    /// has norms.
    pub fn start_field(&mut self, number: u32, norms: Option<&[u8]>) -> Result<()> {
        let info = self
            .fields
            .by_number(number)
            .ok_or_else(|| Error::IllegalArgument(format!("unknown field {number}")))?;
        let has_norms = info.has_norms;
        let opts = info.index_options;
        if opts == IndexOptions::None
            || norms.is_some() != has_norms
            || norms.is_some_and(|n| n.len() != self.max_doc as usize)
        {
            return Err(Error::IllegalArgument(format!(
                "bad field setup for field {number}"
            )));
        }
        self.pw.set_field(opts.has_freqs(), opts.has_positions());
        self.field = Some((
            number,
            FieldTermsWriter::new(opts.has_freqs(), opts.has_positions()),
            opts,
        ));
        self.has_norms = has_norms;
        self.norms.clear();
        if let Some(n) = norms {
            self.norms.extend_from_slice(n);
        }
        self.docs_seen = vec![0; (self.max_doc as usize).div_ceil(64)];
        self.sum_ttf = 0;
        self.sum_df = 0;
        Ok(())
    }

    pub const fn start_term(&mut self) {
        self.term_started = false;
        self.df = 0;
        self.ttf = 0;
    }

    #[inline]
    pub fn start_doc(&mut self, doc: u32, freq: u32) {
        if !self.term_started {
            self.pw.start_term();
            self.term_started = true;
        }
        let norm = if self.has_norms {
            self.norms[doc as usize]
        } else {
            1
        };
        self.pw.start_doc(doc as i32, freq, norm);
        self.docs_seen[(doc >> 6) as usize] |= 1 << (doc & 63);
        self.df += 1;
        self.ttf += u64::from(freq);
    }

    #[inline]
    pub fn add_position(&mut self, position: u32) {
        self.pw.add_position(position);
    }

    #[inline]
    pub const fn finish_doc(&mut self) {
        self.pw.finish_doc();
    }

    /// Ends the current term; terms that ended up with no documents are dropped.
    pub fn finish_term(&mut self, term: &[u8]) {
        if self.df == 0 {
            return;
        }
        let Some((_, terms, opts)) = self.field.as_mut() else {
            return;
        };
        let ttf = if opts.has_freqs() {
            self.ttf
        } else {
            u64::from(self.df)
        };
        let meta: TermMeta = self.pw.finish_term(self.df, ttf);
        terms.add(term, meta, &mut self.tim);
        self.sum_df += u64::from(self.df);
        self.sum_ttf += ttf;
    }

    pub fn finish_field(&mut self) {
        let Some((number, terms, _)) = self.field.take() else {
            return;
        };
        let num_terms = terms.num_terms;
        let terms_index_fp = terms.finish(&mut self.tim);
        let norms_offset = if self.has_norms {
            let off = self.nrm.len() as u64;
            self.nrm.write_bytes(&self.norms);
            Some(off)
        } else {
            None
        };
        if self.stats.len() <= number as usize {
            self.stats.resize(number as usize + 1, None);
        }
        if num_terms > 0 {
            self.stats[number as usize] = Some(FieldStats {
                doc_count: self
                    .docs_seen
                    .iter()
                    .map(|w| u64::from(w.count_ones()))
                    .sum(),
                sum_total_term_freq: self.sum_ttf,
                sum_doc_freq: self.sum_df,
                num_terms,
                terms_index_fp,
                norms_offset,
            });
        }
    }

    pub fn set_stored(&mut self, stored: StoredFieldsWriter) {
        self.stored = Some(stored);
    }

    pub fn finish(self, dir: &Path) -> Result<SegmentInfo> {
        let Self {
            name,
            max_doc,
            fields,
            stats,
            mut pw,
            mut tim,
            mut nrm,
            stored,
            ..
        } = self;
        pw.doc_out.write_bytes(&[0u8; 64]); // slack so fixed-width reads never run off the end
        write_footer(&mut pw.doc_out);
        write_footer(&mut pw.pos_out);
        write_footer(&mut tim);
        write_footer(&mut nrm);
        let stored = if let Some(s) = stored {
            s
        } else {
            let mut w = StoredFieldsWriter::new();
            for _ in 0..max_doc {
                w.add_document(std::iter::empty())?;
            }
            w
        };
        let (fdt, fdx) = stored.finish()?;
        let info = SegmentInfo {
            name,
            max_doc,
            fields,
            stats,
        };
        let write = |ext: &str, data: &[u8]| -> Result<()> {
            write_synced(&dir.join(format!("{}.{ext}", info.name)), data)
        };
        write("doc", &pw.doc_out.buf)?;
        write("pos", &pw.pos_out.buf)?;
        write("tim", &tim.buf)?;
        write("nrm", &nrm.buf)?;
        write("fdt", &fdt)?;
        write("fdx", &fdx)?;
        write("si", &info.encode()?)?; // last: a segment is complete once its .si exists
        Ok(info)
    }
}
