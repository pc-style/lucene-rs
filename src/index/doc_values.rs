//! Single-valued columns and a one-dimensional numeric index. The sorted numeric lookup
//! is built at open time; this is not Lucene's BKD format.

use crate::codec::store::Out;
use crate::document::{DocValuesType, FieldValue};
use crate::error::{Error, Result, corrupt};
use crate::index::codec_util::{
    FOOTER_LEN, check_header, verify_checksum, write_footer, write_header,
};
use crate::index::field_infos::FieldInfos;
use crate::num::{u32_from, usize_from};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::ops::Bound;

const CODEC: &str = "LuceneRsDocValues";

#[derive(Clone, Debug, PartialEq)]
pub enum SortValue {
    I64(i64),
    F64(f64),
    Keyword(String),
}

impl SortValue {
    pub(crate) fn from_field(value: &FieldValue, kind: DocValuesType) -> Result<Self> {
        match (value, kind) {
            (FieldValue::I64(v), DocValuesType::I64) => Ok(Self::I64(*v)),
            (FieldValue::F64(v), DocValuesType::F64) if v.is_finite() => Ok(Self::F64(*v)),
            (FieldValue::Text(v), DocValuesType::Keyword) => Ok(Self::Keyword(v.clone())),
            _ => Err(Error::IllegalArgument(
                "doc value has wrong type or is not finite".into(),
            )),
        }
    }

    pub(crate) fn compare(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::I64(a), Self::I64(b)) => a.cmp(b),
            (Self::F64(a), Self::F64(b)) => a.total_cmp(b),
            (Self::Keyword(a), Self::Keyword(b)) => a.cmp(b),
            _ => self.kind().cmp(&other.kind()),
        }
    }

    const fn kind(&self) -> u8 {
        match self {
            Self::I64(_) => 1,
            Self::F64(_) => 2,
            Self::Keyword(_) => 3,
        }
    }
}

pub(crate) type Columns = BTreeMap<u32, Vec<Option<SortValue>>>;

pub(crate) fn encode(columns: &Columns, max_doc: u32) -> Result<Vec<u8>> {
    let mut out = Out::new();
    write_header(&mut out, CODEC, 1);
    out.write_int(u32_from(columns.len(), "doc value columns")?);
    out.write_int(max_doc);
    for (&number, values) in columns {
        out.write_int(number);
        for doc in 0..max_doc {
            match values.get(usize_from(doc)).and_then(Option::as_ref) {
                None => out.write_byte(0),
                Some(value) => {
                    out.write_byte(value.kind());
                    match value {
                        SortValue::I64(v) => out.write_bytes(&v.to_le_bytes()),
                        SortValue::F64(v) => out.write_bytes(&v.to_le_bytes()),
                        SortValue::Keyword(v) => {
                            out.write_int(u32_from(v.len(), "keyword bytes")?);
                            out.write_bytes(v.as_bytes());
                        }
                    }
                }
            }
        }
    }
    write_footer(&mut out);
    Ok(out.buf)
}

fn take<const N: usize>(input: &mut Cursor<&[u8]>) -> Result<[u8; N]> {
    let mut b = [0; N];
    input
        .read_exact(&mut b)
        .map_err(|_| corrupt("truncated doc values"))?;
    Ok(b)
}

pub(crate) fn decode(data: &[u8], max_doc: u32, fields: &FieldInfos) -> Result<Columns> {
    verify_checksum(data, ".dvm")?;
    let start = check_header(data, CODEC, 1, ".dvm")?;
    let body = data
        .get(start..data.len().saturating_sub(FOOTER_LEN))
        .ok_or_else(|| corrupt("truncated doc values"))?;
    let mut input = Cursor::new(body);
    let count = u32::from_le_bytes(take(&mut input)?);
    if usize_from(count) > fields.len() || u32::from_le_bytes(take(&mut input)?) != max_doc {
        return Err(corrupt("invalid doc values counts"));
    }
    let mut columns = Columns::new();
    for _ in 0..count {
        let number = u32::from_le_bytes(take(&mut input)?);
        let kind = fields
            .by_number(number)
            .and_then(|f| f.doc_values)
            .ok_or_else(|| corrupt("unknown doc values field"))?;
        let mut values = Vec::new();
        for _ in 0..max_doc {
            let [tag] = take(&mut input)?;
            let value = match (tag, kind) {
                (0, _) => None,
                (1, DocValuesType::I64) => {
                    Some(SortValue::I64(i64::from_le_bytes(take(&mut input)?)))
                }
                (2, DocValuesType::F64) => {
                    let value = f64::from_le_bytes(take(&mut input)?);
                    if !value.is_finite() {
                        return Err(corrupt("nonfinite doc value"));
                    }
                    Some(SortValue::F64(value))
                }
                (3, DocValuesType::Keyword) => {
                    let len = usize_from(u32::from_le_bytes(take(&mut input)?));
                    let pos =
                        usize::try_from(input.position()).map_err(|_| corrupt("invalid offset"))?;
                    let bytes = body
                        .get(pos..pos.saturating_add(len))
                        .ok_or_else(|| corrupt("truncated keyword"))?;
                    input.set_position(
                        u64::try_from(pos.saturating_add(len))
                            .map_err(|_| corrupt("invalid offset"))?,
                    );
                    Some(SortValue::Keyword(
                        String::from_utf8(bytes.to_vec())
                            .map_err(|_| corrupt("invalid keyword"))?,
                    ))
                }
                _ => return Err(corrupt("invalid doc value type")),
            };
            values.push(value);
        }
        if columns.insert(number, values).is_some() {
            return Err(corrupt("duplicate doc value column"));
        }
    }
    if usize::try_from(input.position()).ok() != Some(body.len()) {
        return Err(corrupt("trailing doc values"));
    }
    Ok(columns)
}

pub(crate) struct NumericIndex(Vec<(SortValue, u32)>);

impl NumericIndex {
    pub fn new(values: &[Option<SortValue>]) -> Self {
        let mut points: Vec<_> = values
            .iter()
            .enumerate()
            .filter_map(|(doc, v)| Some((v.as_ref()?.clone(), u32::try_from(doc).ok()?)))
            .collect();
        points.sort_unstable_by(|a, b| a.0.compare(&b.0));
        Self(points)
    }

    pub fn range(&self, lower: &Bound<SortValue>, upper: &Bound<SortValue>) -> Vec<u32> {
        let start = self.0.partition_point(|(v, _)| match lower {
            Bound::Unbounded => false,
            Bound::Included(x) => v.compare(x).is_lt(),
            Bound::Excluded(x) => !v.compare(x).is_gt(),
        });
        let end = self.0.partition_point(|(v, _)| match upper {
            Bound::Unbounded => true,
            Bound::Included(x) => !v.compare(x).is_gt(),
            Bound::Excluded(x) => v.compare(x).is_lt(),
        });
        let mut docs: Vec<_> = self
            .0
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .map(|(_, d)| *d)
            .collect();
        docs.sort_unstable();
        docs
    }
}
