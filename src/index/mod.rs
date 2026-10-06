//! Writing and reading indexes: documents are buffered and inverted in memory, flushed as
//! immutable segments, merged in the background of commits, and published atomically.

mod buffer;
mod codec_util;
mod commit;
pub mod doc_values;
pub mod field_infos;
pub mod live_docs;
pub mod merge;
pub mod reader;
pub mod segment;
mod segment_writer;
mod stored;
pub(crate) mod terms;
pub mod writer;

pub use field_infos::{FieldInfo, FieldInfos};
pub use live_docs::LiveDocs;
pub use merge::LogMergePolicy;
pub use reader::{CollectionStats, DirectoryReader, TermStats};
pub use segment::{FieldStats, SegmentInfo, SegmentReader};
pub use writer::{IndexWriter, IndexWriterConfig, OpenMode};

/// A term in a field: the unit of indexing and of `TermQuery`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Term {
    pub field: String,
    pub bytes: Vec<u8>,
}

impl Term {
    pub fn new(field: impl Into<String>, text: impl AsRef<str>) -> Self {
        Self {
            field: field.into(),
            bytes: text.as_ref().as_bytes().to_vec(),
        }
    }
    pub fn from_bytes(field: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            field: field.into(),
            bytes: bytes.into(),
        }
    }
    /// The term text, if it is valid UTF-8.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }
}
