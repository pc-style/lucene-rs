//! A Rust port of the core of [Apache Lucene](https://lucene.apache.org/) 10.5: the
//! Lucene104 postings format, BM25 scoring, Lucene's block-max top-k algorithms, segment-based
//! indexing with deletes and merges, analyzers and the classic query parser.
//!
//! On the benchmark corpus (469k Wikipedia articles) it returns the same top-10 documents with
//! bit-identical scores as Lucene 10.5.2, about 1.6–2x faster per query.
//!
//! # Quick start
//!
//! ```
//! use lucene_rs::{
//!     DirectoryReader, Document, Field, IndexSearcher, IndexWriter, IndexWriterConfig, QueryParser,
//!     StandardAnalyzer, Store,
//! };
//!
//! # fn main() -> lucene_rs::Result<()> {
//! # let tmp = tempfile::tempdir()?;
//! # let dir = tmp.path();
//! let mut writer = IndexWriter::open(dir, IndexWriterConfig::default())?;
//! writer.add_document(
//!     &Document::new()
//!         .with(Field::string("id", "1", Store::Yes))
//!         .with(Field::text("title", "Rust in Action", Store::Yes))
//!         .with(Field::text("body", "systems programming in rust", Store::No)),
//! )?;
//! writer.commit()?;
//!
//! let searcher = IndexSearcher::new(DirectoryReader::open(dir)?);
//! let query = QueryParser::new("body", StandardAnalyzer::new()).parse("\"systems programming\" title:rust")?;
//! let top = searcher.search(&query, 10)?;
//! assert_eq!(top.total_hits.value, 1);
//! let doc = searcher.doc(top.score_docs[0].doc)?;
//! assert_eq!(doc.get_str("title"), Some("Rust in Action"));
//! # Ok(())
//! # }
//! ```
//!
//! # Map
//!
//! - [`document`]: [`Document`], [`Field`], [`FieldType`] (text, keyword, stored fields).
//! - [`analysis`]: [`Analyzer`] and the standard, whitespace, simple and keyword analyzers.
//! - [`index`]: [`IndexWriter`] (add, update, delete, flush, merge, commit) and
//!   [`DirectoryReader`] (a point-in-time view of a commit).
//! - [`search`]: [`Query`], [`BooleanQuery`], [`PhraseQuery`] and [`IndexSearcher`].
//! - [`queryparser`]: Lucene's classic query syntax.
//! - [`sim`]: BM25 and Lucene's one-byte norm encoding.
//!
//! The on-disk format follows Lucene104's postings layout but is not file-compatible with
//! Lucene: indexes written by one cannot be read by the other.
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
pub use index::doc_values::SortValue;
pub use index::{DirectoryReader, IndexWriter, IndexWriterConfig, OpenMode, Term};
pub use queryparser::QueryParser;
pub use search::{BooleanQuery, IndexSearcher, Occur, PhraseQuery, Query, ScoreDoc, TopDocs};
pub use search::{FieldDoc, MissingValue, Sort, SortField, SortOrder};
