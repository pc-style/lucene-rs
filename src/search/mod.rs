//! Searching: queries, scoring (BM25) and top-k collection.

mod bulk;
mod collector;
mod page;
pub use page::{FieldDoc, MissingValue, SearchCursor, SearchPage, Sort, SortField, SortOrder};
pub mod query;
mod scorer;
mod searcher;
mod term_scorer;
mod util;

pub use query::{BooleanQuery, Occur, PhraseQuery, Query};
pub use searcher::{
    IndexSearcher, ScoreDoc, TOTAL_HITS_THRESHOLD, TopDocs, TotalHits, TotalHitsRelation,
};
