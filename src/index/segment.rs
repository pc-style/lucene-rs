//! A segment: an immutable mini-index of `max_doc` documents. `SegmentWriter` writes one from
//! sorted postings (a flush or a merge); `SegmentReader` memory-maps one for searching.

use crate::codec::postings_reader::{IndexedFeatures, PostingsEnum};
use crate::codec::postings_writer::TermMeta;
use crate::codec::store::{In, Out};
use crate::document::{DocValuesType, Document, Field, FieldType, IndexOptions};
use crate::error::{Result, corrupt};
use crate::index::codec_util::{
    check_header, read_string, verify_checksum, write_footer, write_header, write_string,
};
use crate::index::doc_values::{self, Columns, NumericIndex, SortValue};
use crate::index::field_infos::{FieldInfo, FieldInfos};
use crate::index::live_docs::LiveDocs;
use crate::index::stored::StoredFieldsReader;
use crate::index::terms::{FieldTerms, TermsIter};
use crate::num::{u32_from, usize_from};
use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::Path;

const SI_CODEC: &str = "LuceneRsSegmentInfo";
pub const DOC_CODEC: &str = "LuceneRsPostingsDoc";
pub const POS_CODEC: &str = "LuceneRsPostingsPos";
pub const TIM_CODEC: &str = "LuceneRsTerms";
pub const NRM_CODEC: &str = "LuceneRsNorms";
pub const VERSION: u32 = 1;
pub(crate) const EXTENSIONS: &[&str] = &["si", "doc", "pos", "tim", "nrm", "fdt", "fdx", "dvm"];

/// Index-wide statistics of one field in one segment (inputs to BM25).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldStats {
    /// Documents with at least one term in the field.
    pub doc_count: u64,
    /// Total number of tokens (equals `sum_doc_freq` without frequencies).
    pub sum_total_term_freq: u64,
    pub sum_doc_freq: u64,
    pub num_terms: u64,
    pub(crate) terms_index_fp: u64,
    pub(crate) norms_offset: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct SegmentInfo {
    pub(crate) format_version: u32,
    pub name: String,
    pub max_doc: u32,
    pub fields: FieldInfos,
    /// Indexed by field number; `None` for fields without terms in this segment.
    pub stats: Vec<Option<FieldStats>>,
}

impl SegmentInfo {
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        EXTENSIONS
            .iter()
            .filter(|&&ext| ext != "dvm" || self.format_version >= 2)
            .map(|e| format!("{}.{e}", self.name))
            .collect()
    }

    #[must_use]
    pub fn field_stats(&self, number: u32) -> Option<&FieldStats> {
        self.stats.get(usize_from(number)).and_then(Option::as_ref)
    }

    pub(crate) fn set_field_stats(&mut self, number: u32, stats: FieldStats) {
        let n = usize_from(number);
        if self.stats.len() <= n {
            self.stats.resize(n.saturating_add(1), None);
        }
        if let Some(slot) = self.stats.get_mut(n) {
            *slot = Some(stats);
        }
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Out::new();
        write_header(&mut out, SI_CODEC, self.format_version);
        write_string(&mut out, &self.name);
        out.write_vint(self.max_doc);
        out.write_vint(u32_from(self.fields.len(), "fields")?);
        for f in self.fields.iter() {
            write_string(&mut out, &f.name);
            out.write_vint(f.number);
            out.write_byte(f.index_options.to_byte());
            out.write_byte(u8::from(f.has_norms));
            if self.format_version >= 2 {
                out.write_byte(match f.doc_values {
                    None => 0,
                    Some(DocValuesType::I64) => 1,
                    Some(DocValuesType::F64) => 2,
                    Some(DocValuesType::Keyword) => 3,
                });
            }
            if let Some(s) = self.field_stats(f.number) {
                out.write_byte(1);
                out.write_vlong(s.doc_count);
                out.write_vlong(s.sum_total_term_freq);
                out.write_vlong(s.sum_doc_freq);
                out.write_vlong(s.num_terms);
                out.write_vlong(s.terms_index_fp);
                out.write_vlong(s.norms_offset.map_or(0, |o| o.saturating_add(1)));
            } else {
                out.write_byte(0);
            }
        }
        write_footer(&mut out);
        Ok(out.buf)
    }

    fn decode(data: &[u8], file: &str) -> Result<Self> {
        verify_checksum(data, file)?;
        let (start, format_version) = match check_header(data, SI_CODEC, 2, file) {
            Ok(start) => (start, 2),
            Err(_) => (check_header(data, SI_CODEC, 1, file)?, 1),
        };
        let mut i = In::new(data, start);
        let name = read_string(&mut i, file)?;
        let max_doc = i.read_vint();
        let n = i.read_vint();
        let mut info = Self {
            format_version,
            name,
            max_doc,
            fields: FieldInfos::default(),
            stats: Vec::new(),
        };
        for _ in 0..n {
            let name = read_string(&mut i, file)?;
            let number = i.read_vint();
            let index_options = IndexOptions::from_byte(i.read_byte())
                .ok_or_else(|| corrupt(format!("{file}: bad index options")))?;
            let has_norms = i.read_byte() != 0;
            let doc_values = if format_version >= 2 {
                match i.read_byte() {
                    0 => None,
                    1 => Some(DocValuesType::I64),
                    2 => Some(DocValuesType::F64),
                    3 => Some(DocValuesType::Keyword),
                    _ => return Err(corrupt("invalid doc values metadata")),
                }
            } else {
                None
            };
            info.fields.insert(FieldInfo {
                doc_values,
                name,
                number,
                index_options,
                has_norms,
            })?;
            if i.read_byte() == 1 {
                let stats = FieldStats {
                    doc_count: i.read_vlong(),
                    sum_total_term_freq: i.read_vlong(),
                    sum_doc_freq: i.read_vlong(),
                    num_terms: i.read_vlong(),
                    terms_index_fp: i.read_vlong(),
                    norms_offset: i.read_vlong().checked_sub(1),
                };
                info.set_field_stats(number, stats);
            }
        }
        Ok(info)
    }
}

/// Byte range of a field's norms in the `.nrm` file.
fn norms_range(stats: &FieldStats, max_doc: u32) -> Option<std::ops::Range<usize>> {
    let start = usize::try_from(stats.norms_offset?).ok()?;
    Some(start..start.checked_add(usize_from(max_doc))?)
}

#[allow(unsafe_code)]
fn map(dir: &Path, file: &str) -> Result<Mmap> {
    let f = File::open(dir.join(file))?;
    // SAFETY: index files are written once and never modified in place.
    Ok(unsafe { Mmap::map(&f)? })
}

/// An open, immutable segment plus its current deletions.
pub struct SegmentReader {
    dvm: Option<Mmap>,
    pub(crate) columns: Columns,
    numeric: HashMap<u32, NumericIndex>,
    pub(crate) info: SegmentInfo,
    pub(crate) del_gen: u64,
    doc: Mmap,
    pos: Mmap,
    tim: Mmap,
    nrm: Mmap,
    fdt: Mmap,
    fdx: Mmap,
    terms: HashMap<u32, FieldTerms>,
    live_docs: Option<LiveDocs>,
}

pub(crate) fn live_docs_file(name: &str, del_gen: u64) -> String {
    format!("{name}_{del_gen}.liv")
}

impl SegmentReader {
    pub(crate) fn open(dir: &Path, name: &str, del_gen: u64) -> Result<Self> {
        let si_file = format!("{name}.si");
        let info = SegmentInfo::decode(&fs::read(dir.join(&si_file))?, &si_file)?;
        let doc = map(dir, &format!("{name}.doc"))?;
        let pos = map(dir, &format!("{name}.pos"))?;
        let tim = map(dir, &format!("{name}.tim"))?;
        let nrm = map(dir, &format!("{name}.nrm"))?;
        let fdt = map(dir, &format!("{name}.fdt"))?;
        let fdx = map(dir, &format!("{name}.fdx"))?;
        let dvm = if info.format_version >= 2 {
            Some(map(dir, &format!("{name}.dvm"))?)
        } else {
            None
        };
        let columns = dvm
            .as_ref()
            .map(|data| doc_values::decode(data, info.max_doc, &info.fields))
            .transpose()?
            .unwrap_or_default();
        let numeric = columns
            .iter()
            .filter(|(n, _)| {
                info.fields.by_number(**n).is_some_and(|f| {
                    matches!(f.doc_values, Some(DocValuesType::I64 | DocValuesType::F64))
                })
            })
            .map(|(&n, values)| (n, NumericIndex::new(values)))
            .collect();
        check_header(&doc, DOC_CODEC, VERSION, ".doc")?;
        check_header(&pos, POS_CODEC, VERSION, ".pos")?;
        check_header(&tim, TIM_CODEC, VERSION, ".tim")?;
        check_header(&nrm, NRM_CODEC, VERSION, ".nrm")?;
        StoredFieldsReader::open(&fdt, &fdx, info.max_doc)?;
        let mut terms = HashMap::new();
        for f in info.fields.iter() {
            let Some(s) = info.field_stats(f.number) else {
                continue;
            };
            let opts = f.index_options;
            let index_fp = usize::try_from(s.terms_index_fp)
                .map_err(|_| corrupt(format!("{name}.si: bad offset")))?;
            terms.insert(
                f.number,
                FieldTerms::open(&tim, index_fp, opts.has_freqs(), opts.has_positions())?,
            );
            if s.norms_offset.is_some()
                && norms_range(s, info.max_doc)
                    .and_then(|r| nrm.get(r))
                    .is_none()
            {
                return Err(corrupt(format!("{name}.nrm: truncated")));
            }
        }
        let live_docs = if del_gen > 0 {
            let file = live_docs_file(name, del_gen);
            Some(LiveDocs::decode(
                &fs::read(dir.join(&file))?,
                info.max_doc,
                &file,
            )?)
        } else {
            None
        };
        Ok(Self {
            dvm,
            columns,
            numeric,
            info,
            del_gen,
            doc,
            pos,
            tim,
            nrm,
            fdt,
            fdx,
            terms,
            live_docs,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.info.name
    }
    #[must_use]
    pub const fn max_doc(&self) -> u32 {
        self.info.max_doc
    }
    pub fn num_docs(&self) -> u32 {
        self.live_docs
            .as_ref()
            .map_or(self.info.max_doc, LiveDocs::num_live)
    }
    #[must_use]
    pub const fn live_docs(&self) -> Option<&LiveDocs> {
        self.live_docs.as_ref()
    }
    #[must_use]
    pub const fn field_infos(&self) -> &FieldInfos {
        &self.info.fields
    }
    #[must_use]
    pub fn field_stats(&self, field: &str) -> Option<&FieldStats> {
        self.info
            .fields
            .get(field)
            .and_then(|f| self.info.field_stats(f.number))
    }

    /// Single-valued column value for a segment-local document.
    #[must_use]
    pub fn doc_value(&self, field: &str, doc: u32) -> Option<&SortValue> {
        self.columns
            .get(&self.info.fields.get(field)?.number)?
            .get(usize_from(doc))?
            .as_ref()
    }

    pub(crate) fn range_docs(
        &self,
        field: &str,
        lower: &std::ops::Bound<SortValue>,
        upper: &std::ops::Bound<SortValue>,
    ) -> Vec<u32> {
        self.info
            .fields
            .get(field)
            .and_then(|f| self.numeric.get(&f.number))
            .map_or_else(Vec::new, |index| index.range(lower, upper))
    }

    pub(crate) fn field_terms(&self, number: u32) -> Option<&FieldTerms> {
        self.terms.get(&number)
    }

    /// Postings metadata of `term` in `field`, if present.
    #[must_use]
    pub fn term_meta(&self, field: &str, term: &[u8]) -> Option<(FieldInfo, TermMeta)> {
        let f = self.info.fields.get(field)?;
        let meta = self.field_terms(f.number)?.lookup(&self.tim, term)?;
        Some((f.clone(), meta))
    }

    pub(crate) fn terms_iter(&self, number: u32) -> Option<TermsIter<'_>> {
        Some(self.field_terms(number)?.cursor(&self.tim))
    }

    #[must_use]
    pub fn postings(
        &self,
        field: &FieldInfo,
        meta: &TermMeta,
        needs_positions: bool,
    ) -> Box<PostingsEnum<'_>> {
        let features = IndexedFeatures {
            freqs: field.index_options.has_freqs(),
            positions: field.index_options.has_positions(),
        };
        PostingsEnum::new(&self.doc, &self.pos, meta, features, needs_positions)
    }

    /// One norm byte per document, if the field has norms.
    #[must_use]
    pub fn norms(&self, number: u32) -> Option<&[u8]> {
        self.nrm.get(norms_range(
            self.info.field_stats(number)?,
            self.info.max_doc,
        )?)
    }

    pub(crate) fn stored_raw(&self, doc: u32) -> Result<&[u8]> {
        StoredFieldsReader::open(&self.fdt, &self.fdx, self.info.max_doc)?.raw(doc)
    }

    /// The stored fields of `doc` (segment-local ID).
    ///
    /// # Errors
    /// [`Error::Corrupt`](crate::Error::Corrupt) if the stored fields are damaged, or
    /// `IllegalArgument`-style corruption errors for out-of-range doc IDs.
    pub fn document(&self, doc: u32) -> Result<Document> {
        let values =
            StoredFieldsReader::open(&self.fdt, &self.fdx, self.info.max_doc)?.document(doc)?;
        let mut d = Document::new();
        for (number, value) in values {
            let name = self
                .info
                .fields
                .by_number(number)
                .ok_or_else(|| corrupt("stored field with unknown number"))?;
            d.add(Field::new(name.name.clone(), value, FieldType::STORED_ONLY));
        }
        Ok(d)
    }

    /// Verifies the checksum of every file of this segment.
    ///
    /// # Errors
    /// [`Error::Corrupt`](crate::Error::Corrupt) naming the first file that fails.
    pub fn check_integrity(&self) -> Result<()> {
        if let Some(data) = &self.dvm {
            verify_checksum(data, &format!("{}.dvm", self.info.name))?;
        }
        for (data, ext) in [
            (&self.doc, "doc"),
            (&self.pos, "pos"),
            (&self.tim, "tim"),
            (&self.nrm, "nrm"),
            (&self.fdt, "fdt"),
            (&self.fdx, "fdx"),
        ] {
            verify_checksum(data, &format!("{}.{ext}", self.info.name))?;
        }
        Ok(())
    }
}
