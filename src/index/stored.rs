//! Stored fields: per document, the stored values as (field number, typed value) records in
//! `.fdt`, with a per-document offset table in `.fdx`. Values are not compressed (Lucene
//! compresses 80KB chunks with LZ4; see the README's scope notes).

use crate::codec::store::{In, Out};
use crate::document::FieldValue;
use crate::error::{Result, corrupt};
use crate::index::codec_util::{FOOTER_LEN, check_header, write_footer, write_header};
use crate::num::{u32_from, u64_from, usize_from};

const FDT_CODEC: &str = "LuceneRsStoredFieldsData";
const FDX_CODEC: &str = "LuceneRsStoredFieldsIndex";
const VERSION: u32 = 1;

const T_TEXT: u8 = 0;
const T_BYTES: u8 = 1;
const T_I64: u8 = 2;
const T_F64: u8 = 3;

pub struct StoredFieldsWriter {
    data: Out,
    offsets: Vec<u64>,
    record: Out,
}

impl StoredFieldsWriter {
    pub fn new() -> Self {
        let mut data = Out::new();
        write_header(&mut data, FDT_CODEC, VERSION);
        Self {
            data,
            offsets: Vec::new(),
            record: Out::new(),
        }
    }

    pub const fn ram_bytes(&self) -> usize {
        self.data
            .buf
            .capacity()
            .saturating_add(self.offsets.capacity().saturating_mul(8))
    }

    pub fn add_document<'a>(
        &mut self,
        fields: impl IntoIterator<Item = (u32, &'a FieldValue)>,
    ) -> Result<()> {
        let mut record = std::mem::take(&mut self.record);
        record.clear();
        let mut count = 0u32;
        for (number, value) in fields {
            count = count.saturating_add(1);
            record.write_vint(number);
            match value {
                FieldValue::Text(s) => {
                    record.write_byte(T_TEXT);
                    record.write_vint(u32_from(s.len(), "bytes in a stored value")?);
                    record.write_bytes(s.as_bytes());
                }
                FieldValue::Bytes(b) => {
                    record.write_byte(T_BYTES);
                    record.write_vint(u32_from(b.len(), "bytes in a stored value")?);
                    record.write_bytes(b);
                }
                FieldValue::I64(v) => {
                    record.write_byte(T_I64);
                    record.write_zlong(*v);
                }
                FieldValue::F64(v) => {
                    record.write_byte(T_F64);
                    record.write_long(v.to_bits());
                }
            }
        }
        self.offsets.push(u64_from(self.data.len()));
        self.data.write_vint(count);
        self.data.append(&record);
        self.record = record;
        Ok(())
    }

    /// Appends an already-encoded record (used by merges).
    pub fn add_raw(&mut self, record: &[u8]) {
        self.offsets.push(u64_from(self.data.len()));
        self.data.write_bytes(record);
    }

    /// Returns the `.fdt` and `.fdx` file contents.
    pub fn finish(mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        let docs = u32_from(self.offsets.len(), "documents")?;
        self.offsets.push(u64_from(self.data.len()));
        write_footer(&mut self.data);
        let mut fdx = Out::new();
        write_header(&mut fdx, FDX_CODEC, VERSION);
        fdx.write_vint(docs);
        for &o in &self.offsets {
            fdx.write_long(o);
        }
        write_footer(&mut fdx);
        Ok((self.data.buf, fdx.buf))
    }
}

pub struct StoredFieldsReader<'a> {
    fdt: &'a [u8],
    offsets: &'a [u8],
}

impl<'a> StoredFieldsReader<'a> {
    pub fn open(fdt: &'a [u8], fdx: &'a [u8], max_doc: u32) -> Result<Self> {
        check_header(fdt, FDT_CODEC, VERSION, ".fdt")?;
        let start = check_header(fdx, FDX_CODEC, VERSION, ".fdx")?;
        let mut i = In::new(fdx, start);
        if i.read_vint() != max_doc {
            return Err(corrupt(".fdx: wrong document count"));
        }
        let len = usize_from(max_doc).saturating_add(1).saturating_mul(8);
        let offsets = fdx
            .get(i.pos..i.pos.saturating_add(len))
            .ok_or_else(|| corrupt(".fdx: truncated"))?;
        if fdx.len().saturating_sub(i.pos.saturating_add(len)) < FOOTER_LEN {
            return Err(corrupt(".fdx: truncated"));
        }
        Ok(Self { fdt, offsets })
    }

    fn offset(&self, doc: u32) -> Result<usize> {
        let at = usize_from(doc).saturating_mul(8);
        let b = self
            .offsets
            .get(at..at.saturating_add(8))
            .and_then(|b| <[u8; 8]>::try_from(b).ok());
        let off = u64::from_le_bytes(
            b.ok_or_else(|| corrupt(format!("stored fields: doc {doc} out of range")))?,
        );
        usize::try_from(off).map_err(|_| corrupt("stored fields: offset out of range"))
    }

    /// The encoded record of `doc`.
    pub fn raw(&self, doc: u32) -> Result<&'a [u8]> {
        let (start, end) = (self.offset(doc)?, self.offset(doc.saturating_add(1))?);
        self.fdt
            .get(start..end)
            .ok_or_else(|| corrupt("stored fields: offset out of range"))
    }

    pub fn document(&self, doc: u32) -> Result<Vec<(u32, FieldValue)>> {
        let rec = self.raw(doc)?;
        let mut i = In::new(rec, 0);
        let take = |i: &mut In<'a>| -> Result<&'a [u8]> {
            let len = usize_from(i.read_vint());
            let end = i.pos.saturating_add(len);
            let b = rec
                .get(i.pos..end)
                .ok_or_else(|| corrupt("stored fields: truncated value"))?;
            i.pos = end;
            Ok(b)
        };
        let n = i.read_vint();
        let mut out = Vec::new();
        for _ in 0..n {
            let number = i.read_vint();
            let v = match i.read_byte() {
                T_TEXT => FieldValue::Text(
                    String::from_utf8(take(&mut i)?.to_vec())
                        .map_err(|_| corrupt("stored fields: bad UTF-8"))?,
                ),
                T_BYTES => FieldValue::Bytes(take(&mut i)?.to_vec()),
                T_I64 => FieldValue::I64(i.read_zlong()),
                T_F64 => FieldValue::F64(f64::from_bits(i.read_long())),
                t => return Err(corrupt(format!("stored fields: unknown type {t}"))),
            };
            out.push((number, v));
        }
        Ok(out)
    }
}
