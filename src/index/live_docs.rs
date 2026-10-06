//! Deleted-documents bitset (Lucene's live docs), one generation per file.

use crate::codec::store::{In, Out};
use crate::error::{Result, corrupt};
use crate::index::codec_util::{check_header, write_footer, write_header};
use crate::num::usize_from;

const CODEC: &str = "LuceneRsLiveDocs";
const VERSION: u32 = 1;

/// Bit set where a set bit means the document is live (not deleted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveDocs {
    words: Vec<u64>,
    max_doc: u32,
}

impl LiveDocs {
    #[must_use]
    pub fn all_live(max_doc: u32) -> Self {
        let mut words = vec![u64::MAX; usize_from(max_doc).div_ceil(64)];
        if let (Some(last), true) = (words.last_mut(), !max_doc.is_multiple_of(64)) {
            *last = !(u64::MAX << (max_doc & 63));
        }
        Self { words, max_doc }
    }
    #[inline]
    #[must_use]
    pub fn get(&self, doc: u32) -> bool {
        self.words
            .get(usize_from(doc >> 6))
            .is_some_and(|w| w >> (doc & 63) & 1 != 0)
    }
    /// Marks `doc` deleted; returns true if it was live (false also for out-of-range docs).
    pub fn delete(&mut self, doc: u32) -> bool {
        let Some(w) = self.words.get_mut(usize_from(doc >> 6)) else {
            return false;
        };
        let was = *w >> (doc & 63) & 1 != 0;
        *w &= !(1u64 << (doc & 63));
        was
    }
    #[must_use]
    pub fn num_live(&self) -> u32 {
        self.words.iter().map(|w| w.count_ones()).sum()
    }
    #[must_use]
    pub const fn max_doc(&self) -> u32 {
        self.max_doc
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Out::new();
        write_header(&mut out, CODEC, VERSION);
        out.write_vint(self.max_doc);
        for &w in &self.words {
            out.write_long(w);
        }
        write_footer(&mut out);
        out.buf
    }

    pub(crate) fn decode(data: &[u8], expected_max_doc: u32, file: &str) -> Result<Self> {
        let start = check_header(data, CODEC, VERSION, file)?;
        let mut i = In::new(data, start);
        let max_doc = i.read_vint();
        if max_doc != expected_max_doc {
            return Err(corrupt(format!(
                "{file}: maxDoc {max_doc} != {expected_max_doc}"
            )));
        }
        let n = usize_from(max_doc).div_ceil(64);
        if data.len().saturating_sub(i.pos) < n.saturating_mul(8) {
            return Err(corrupt(format!("{file}: truncated")));
        }
        let words = (0..n).map(|_| i.read_long()).collect();
        Ok(Self { words, max_doc })
    }
}
