//! Per-index field metadata (Lucene's `FieldInfos`). Field numbers are global to an index so
//! segments can be merged without renumbering.

use crate::document::{DocValuesType, FieldType, IndexOptions};
use crate::error::{Error, Result};
use crate::num::{u32_from, usize_from};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldInfo {
    pub doc_values: Option<DocValuesType>,
    pub name: String,
    pub number: u32,
    pub index_options: IndexOptions,
    pub has_norms: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FieldInfos {
    by_number: Vec<Option<FieldInfo>>,
    by_name: HashMap<String, u32>,
}

impl FieldInfos {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&FieldInfo> {
        self.by_name.get(name).and_then(|&n| self.by_number(n))
    }
    #[must_use]
    pub fn by_number(&self, number: u32) -> Option<&FieldInfo> {
        self.by_number
            .get(usize_from(number))
            .and_then(Option::as_ref)
    }
    pub fn iter(&self) -> impl Iterator<Item = &FieldInfo> {
        self.by_number.iter().flatten()
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Adds `info` with its existing number (used when loading segments).
    pub(crate) fn insert(&mut self, info: FieldInfo) -> Result<()> {
        let merged = match self.get(&info.name) {
            Some(existing) if existing.number != info.number => {
                return Err(Error::Corrupt(format!(
                    "field {} has two numbers",
                    info.name
                )));
            }
            Some(existing) => merge_info(existing, &info)?,
            None => {
                self.by_name.insert(info.name.clone(), info.number);
                info
            }
        };
        let n = usize_from(merged.number);
        if self.by_number.len() <= n {
            self.by_number.resize(n.saturating_add(1), None);
        }
        if let Some(slot) = self.by_number.get_mut(n) {
            *slot = Some(merged);
        }
        Ok(())
    }

    /// Returns the number for a field as indexed with `ft`, registering it if new. Errors if the
    /// field was previously indexed with different options (Lucene enforces the same).
    pub(crate) fn get_or_add(&mut self, name: &str, ft: FieldType) -> Result<u32> {
        let number = match self.get(name) {
            Some(existing) => existing.number,
            None => u32_from(self.by_number.len(), "fields")?,
        };
        self.insert(FieldInfo {
            doc_values: ft.doc_values,
            name: name.to_string(),
            number,
            index_options: ft.index_options,
            has_norms: ft.is_indexed() && !ft.omit_norms,
        })?;
        Ok(number)
    }
}

/// A stored-only use of a field (options None) is compatible with any indexed use; otherwise
/// index options and norms must agree.
fn merge_info(a: &FieldInfo, b: &FieldInfo) -> Result<FieldInfo> {
    if a.doc_values.is_some() && b.doc_values.is_some() && a.doc_values != b.doc_values {
        return Err(Error::IllegalArgument(format!(
            "cannot change doc values type of {}",
            a.name
        )));
    }
    if a.index_options != IndexOptions::None
        && b.index_options != IndexOptions::None
        && (a.index_options != b.index_options || a.has_norms != b.has_norms)
    {
        return Err(Error::IllegalArgument(format!(
            "cannot change field \"{}\" from index options={:?}, norms={} to {:?}, norms={}",
            a.name, a.index_options, a.has_norms, b.index_options, b.has_norms
        )));
    }
    let mut merged = if a.index_options == IndexOptions::None {
        b.clone()
    } else {
        a.clone()
    };
    merged.doc_values = a.doc_values.or(b.doc_values);
    Ok(merged)
}
