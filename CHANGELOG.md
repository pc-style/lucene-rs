# Changelog

## 0.2.0 (2026-10-07)

- Typed i64/f64 range queries, single-value numeric/keyword doc values and field sorting.
- Stateless `search_after` collection with bounded top-k heaps and score/field hit boundaries.
  Exact counting is a separate operation; applications own pagination sessions and tokens.
- Doc values survive updates, deletes, merges and reopen; checksummed new segment files.
- New segment metadata reads 0.1 indexes; newly written segments require 0.2. Back up before
  upgrading. Public field metadata and query variants expanded (source-breaking for exhaustive users).
- Numeric indexes are sorted in RAM, not BKD; paged search scans all matches. Existing published
  benchmark scores measure the 0.1 text-search path, not these additions.

## 0.1.0 (2026-10-06)

First release.

- Lucene104 postings format port: 256-doc FOR/bitset doc blocks, PFOR freqs and positions,
  two-level skip data with impacts, generated per-bit-width decoders.
- BM25 (`k1 = 1.2`, `b = 0.75`) with Lucene's one-byte SmallFloat norms; scores are
  bit-identical to Lucene 10.5.2 on the benchmark corpus.
- Top-k search with Lucene's dynamic pruning: `BatchScoreBulkScorer` + `ImpactsDISI` for terms,
  `MaxScoreBulkScorer` for disjunctions, `BlockMaxConjunctionBulkScorer` for conjunctions.
- Queries: term, boolean (must/should/must-not/filter, minimum should match), exact phrase,
  boost, constant score, match-all; Lucene-style rewrites.
- Indexing: documents with text, keyword and stored fields; segments flushed by RAM or doc
  count; deletes and updates by term; log merge policy; force merge; atomic commits;
  `DirectoryReader::reopen`.
- Analysis: standard (UAX#29), whitespace, simple, keyword and per-field analyzers; English
  stop words.
- Classic query parser.
- `lucene-rs` CLI (feature `cli`): JSON Lines indexing, search, delete, merge, stats, check.
- Cross-platform durable writes, including Windows-compatible file syncing.
