//! Stateless sorted top-k collection. Heap memory is bounded by the requested size; unlike `search`, this
//! collector visits every match and does not use score-based block pruning.
use crate::codec::postings_reader::NO_MORE_DOCS;
use crate::document::DocValuesType;
use crate::error::{Error, Result};
use crate::index::doc_values::SortValue;
use crate::search::searcher::{is_live, scorer};
use crate::{IndexSearcher, Query};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ops::Bound;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingValue {
    First,
    Last,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortField {
    pub field: String,
    pub order: SortOrder,
    pub missing: MissingValue,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sort {
    Score,
    Fields(Vec<SortField>),
}
#[derive(Clone, Debug, PartialEq)]
pub struct FieldDoc {
    pub doc: u32,
    pub score: f32,
    pub sort_values: Vec<Option<SortValue>>,
}

fn invalid(message: &str) -> Error {
    Error::IllegalArgument(message.into())
}

fn compare(a: &FieldDoc, b: &FieldDoc, sort: &Sort) -> Ordering {
    let order = match sort {
        Sort::Score => b.score.total_cmp(&a.score),
        Sort::Fields(fields) => fields
            .iter()
            .zip(a.sort_values.iter().zip(&b.sort_values))
            .map(|(f, (a, b))| match (a, b) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => {
                    if f.missing == MissingValue::First {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (Some(_), None) => {
                    if f.missing == MissingValue::First {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (Some(a), Some(b)) => {
                    if f.order == SortOrder::Asc {
                        a.compare(b)
                    } else {
                        b.compare(a)
                    }
                }
            })
            .find(|o| !o.is_eq())
            .unwrap_or(Ordering::Equal),
    };
    order.then(a.doc.cmp(&b.doc))
}

struct Hit<'a> {
    hit: FieldDoc,
    sort: &'a Sort,
}
impl PartialEq for Hit<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Hit<'_> {}
impl PartialOrd for Hit<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Hit<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        compare(&self.hit, &other.hit, self.sort)
    }
}

fn validate_bounds<T: PartialOrd>(lower: &Bound<T>, upper: &Bound<T>) -> Result<()> {
    if let (Bound::Included(a) | Bound::Excluded(a), Bound::Included(b) | Bound::Excluded(b)) =
        (lower, upper)
        && a > b
    {
        return Err(invalid("range lower bound exceeds upper bound"));
    }
    Ok(())
}

impl IndexSearcher {
    pub(super) fn validate_query(&self, query: &Query) -> Result<()> {
        match query {
            Query::I64Range {
                field,
                lower,
                upper,
            } => {
                validate_bounds(lower, upper)?;
                self.validate_column(field, Some(DocValuesType::I64))?;
            }
            Query::F64Range {
                field,
                lower,
                upper,
            } => {
                for b in [lower, upper] {
                    if let Bound::Included(v) | Bound::Excluded(v) = b
                        && !v.is_finite()
                    {
                        return Err(invalid("range bounds must be finite"));
                    }
                }
                validate_bounds(lower, upper)?;
                self.validate_column(field, Some(DocValuesType::F64))?;
            }
            Query::Boolean(b) => {
                for (q, _) in &b.clauses {
                    self.validate_query(q)?;
                }
            }
            Query::Boost(q, boost) => {
                if !boost.is_finite() || *boost < 0.0 {
                    return Err(invalid("boost must be finite and nonnegative"));
                }
                self.validate_query(q)?;
            }
            Query::ConstantScore(q) => self.validate_query(q)?,
            _ => (),
        }
        Ok(())
    }

    fn validate_column(&self, field: &str, expected: Option<DocValuesType>) -> Result<()> {
        let mut kind = expected;
        for info in self
            .reader()
            .segments()
            .iter()
            .filter_map(|s| s.field_infos().get(field))
        {
            if let Some(actual) = info.doc_values {
                if kind.is_some_and(|k| k != actual) {
                    return Err(invalid("incompatible doc values type"));
                }
                kind = Some(actual);
            } else if info.index_options != crate::IndexOptions::None {
                return Err(invalid("field has no doc values"));
            }
        }
        Ok(())
    }

    /// Collects up to `size` hits strictly after a score/field-sort boundary.
    /// Pass `None` for the first batch, then the last returned hit for the next batch.
    /// The caller must keep the reader snapshot, query and sort unchanged between calls.
    /// This method manages no cursor state and computes no total; use [`Self::count`]
    /// separately when an exact count is needed. This collector still visits all matches.
    /// Missing values are ordered independently of sort direction. Ties use ascending doc ID.
    ///
    /// # Errors
    /// Invalid ranges, column types, zero size, or malformed boundary values.
    pub fn search_after(
        &self,
        query: &Query,
        sort: &Sort,
        size: usize,
        after: Option<&FieldDoc>,
    ) -> Result<Vec<FieldDoc>> {
        if size == 0 {
            return Err(invalid("size must be positive"));
        }
        self.validate_query(query)?;
        if let Sort::Fields(fields) = sort {
            if fields.is_empty() {
                return Err(invalid("sort needs at least one field"));
            }
            for f in fields {
                self.validate_column(&f.field, None)?;
            }
        }
        if let Some(after) = after {
            if !after.score.is_finite() {
                return Err(invalid("boundary score must be finite"));
            }
            match sort {
                Sort::Score if !after.sort_values.is_empty() => {
                    return Err(invalid("score boundary must not contain field values"));
                }
                Sort::Fields(fields) => {
                    if fields.len() != after.sort_values.len() {
                        return Err(invalid("boundary must contain one value per sort field"));
                    }
                    for (field, value) in fields.iter().zip(&after.sort_values) {
                        let kind = match value {
                            None => continue,
                            Some(SortValue::I64(_)) => DocValuesType::I64,
                            Some(SortValue::F64(v)) if v.is_finite() => DocValuesType::F64,
                            Some(SortValue::Keyword(_)) => DocValuesType::Keyword,
                            Some(SortValue::F64(_)) => {
                                return Err(invalid("boundary value must be finite"));
                            }
                        };
                        self.validate_column(&field.field, Some(kind))?;
                    }
                }
                Sort::Score => (),
            }
        }
        let weight = self.create_weight(&query.rewrite(), 1.0);
        let mut heap = BinaryHeap::new();
        for (seg, &base) in self
            .reader()
            .segments()
            .iter()
            .zip(self.reader().doc_bases())
        {
            let Some(mut scorer) = scorer(&weight, seg)? else {
                continue;
            };
            loop {
                let doc = scorer.next_doc();
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !is_live(seg.live_docs(), doc) {
                    continue;
                }
                let local = u32::try_from(doc).map_err(|_| invalid("negative doc ID"))?;
                let hit = FieldDoc {
                    doc: base.saturating_add(local),
                    score: scorer.score(),
                    sort_values: match sort {
                        Sort::Score => Vec::new(),
                        Sort::Fields(fields) => fields
                            .iter()
                            .map(|f| seg.doc_value(&f.field, local).cloned())
                            .collect(),
                    },
                };
                if !hit.score.is_finite() {
                    return Err(invalid("nonfinite score"));
                }
                if after.is_some_and(|last| !compare(&hit, last, sort).is_gt()) {
                    continue;
                }
                let entry = Hit { hit, sort };
                if heap.len() < size {
                    heap.push(entry);
                } else if heap.peek().is_some_and(|worst| entry < *worst) {
                    heap.pop();
                    heap.push(entry);
                }
            }
        }
        Ok(heap.into_sorted_vec().into_iter().map(|h| h.hit).collect())
    }
}
