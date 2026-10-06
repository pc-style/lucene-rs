//! A port of Apache Lucene 10.5's core search path to Rust: Lucene104 postings format
//! (256-doc FOR/bitset blocks, PFOR freqs, 2-level skip data with impacts), BM25 with
//! SmallFloat norms, and the block-max top-k scorers (MaxScore, BlockMaxConjunction).
pub mod forutil;
pub mod index;
pub mod postings_reader;
pub mod postings_writer;
pub mod search;
pub mod sim;
pub mod store;
