#![doc = include_str!("../README.md")]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod analysis;
mod codec;
pub mod document;
mod error;
pub mod index;
mod num;
mod pool;
pub mod queryparser;
pub mod search;
pub mod sim;

pub use analysis::{Analyzer, StandardAnalyzer};
pub use document::{Document, Field, FieldType, FieldValue, IndexOptions, Store};
pub use error::{Error, Result};
pub use index::{DirectoryReader, IndexWriter, IndexWriterConfig, OpenMode, Term};
pub use queryparser::QueryParser;
pub use search::{BooleanQuery, IndexSearcher, Occur, PhraseQuery, Query, ScoreDoc, TopDocs};
