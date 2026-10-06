# lucene-rs

[![CI](https://github.com/pc-style/lucene-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/pc-style/lucene-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/lucene-rs.svg)](https://crates.io/crates/lucene-rs)
[![docs.rs](https://img.shields.io/docsrs/lucene-rs)](https://docs.rs/lucene-rs)

A Rust port of the core of [Apache Lucene](https://lucene.apache.org/) 10.5: the Lucene104
postings format, BM25 scoring, Lucene's block-max top-k algorithms, segment-based indexing
with deletes and merges, analyzers and the classic query parser.

It started as an answer to "is the JVM Lucene's bottleneck?". Port the code a Lucene query
actually runs, check the answers are identical, then time both.

- **Same answers.** Across 1,201 benchmark queries on 469k Wikipedia articles, lucene-rs
  returns the same top-10 documents as Lucene 10.5.2 in the same order, with bit-identical
  float scores and identical hit counts.
- **Faster.** 1.6x faster per query on term, AND and OR queries, 2x on phrase queries, and
  over 150x faster to open an index (no JVM warm-up).

## Quick start

```toml
[dependencies]
lucene-rs = "0.1"
```

```rust
use lucene_rs::{
    DirectoryReader, Document, Field, IndexSearcher, IndexWriter, IndexWriterConfig, QueryParser,
    StandardAnalyzer, Store, Term,
};

fn main() -> lucene_rs::Result<()> {
    let mut writer = IndexWriter::open("my-index", IndexWriterConfig::default())?;
    writer.add_document(
        &Document::new()
            .with(Field::string("id", "1", Store::Yes))
            .with(Field::text("title", "Rust in Action", Store::Yes))
            .with(Field::text("body", "systems programming in rust", Store::No)),
    )?;
    writer.update_document(Term::new("id", "1"), &Document::new() /* ... */)?;
    writer.commit()?;

    let searcher = IndexSearcher::new(DirectoryReader::open("my-index")?);
    let query = QueryParser::new("body", StandardAnalyzer::new())
        .parse("\"systems programming\" +title:rust -draft")?;
    for hit in searcher.search(&query, 10)?.score_docs {
        println!("{:.3} {:?}", hit.score, searcher.doc(hit.doc)?.get_str("title"));
    }
    Ok(())
}
```

Queries can also be built directly:

```rust
use lucene_rs::{BooleanQuery, PhraseQuery, Query};

let q = BooleanQuery::new()
    .must(PhraseQuery::new("body").term("new").term("york").build())
    .should(Query::term("title", "guide").boost(2.0))
    .must_not(Query::term("tags", "draft"))
    .build();
```

See [`examples/basic.rs`](examples/basic.rs) for a complete program.

## Command-line tool

```sh
cargo install lucene-rs --features cli

lucene-rs index  ./idx docs.jsonl --keyword id --keyword tags   # one JSON object per line
lucene-rs search ./idx '"full text" +title:search -draft' -n 5  # JSON Lines results
lucene-rs delete ./idx id 42
lucene-rs merge  ./idx --max-segments 1
lucene-rs stats  ./idx
lucene-rs check  ./idx                                          # verify every checksum
```

## What is ported

| lucene-rs | Lucene 10.5 |
|---|---|
| `codec::forutil` (+ generated `forutil_gen`) | `ForUtil`, `PForUtil`: 256-value FOR/PFOR blocks in Lucene's 8/16/32-bit lane layout |
| `codec::postings_writer`, `codec::postings_reader` | `Lucene104PostingsWriter`/`Reader`: FOR or bitset doc blocks, PFOR freqs and positions, two-level skip data with impacts, lazy freq decoding |
| `sim` | `SmallFloat` norms, `BM25Similarity` (same float operations) |
| `search::term_scorer` | `TermScorer`, `MaxScoreCache`, `ImpactsDISI` |
| `search::bulk` | `BatchScoreBulkScorer`, `MaxScoreBulkScorer`, `BlockMaxConjunctionBulkScorer`, `ScorerUtil` |
| `search::collector` | `TopScoreDocCollector` (exact hit counts up to 1,000, then dynamic pruning) |
| `search::scorer` | `ConjunctionScorer`, `DisjunctionSumScorer`, `ReqExclScorer`, `ReqOptSumScorer`, exact `PhraseScorer` |
| `search::query` | `TermQuery`, `BooleanQuery`, `PhraseQuery`, `BoostQuery`, `ConstantScoreQuery`, `MatchAllDocsQuery`, rewrite rules |
| `index` | `IndexWriter`, `DirectoryReader`, `SegmentInfos`, `LogDocMergePolicy`, live docs, `CodecUtil` checksums |
| `analysis` | `StandardAnalyzer` (UAX#29), `WhitespaceAnalyzer`, `SimpleAnalyzer`, `KeywordAnalyzer`, `PerFieldAnalyzerWrapper` |
| `queryparser` | classic `QueryParser` |

### Not yet

Numeric points and range queries, doc values and sorting by field, vectors, highlighting,
wildcard/fuzzy/regex queries, sloppy phrases, stored-field compression, concurrent indexing
and search threads, and other similarities than BM25. The file format follows Lucene104's
postings layout but is not file-compatible with Lucene; the term dictionary is a simpler
block index instead of BlockTree/FST.

## Correctness

- **Against Lucene.** `bench/` indexes the same corpus with both engines and diffs the top-10
  of every query. Result: 1,201/1,201 queries identical (docs, order, bit-identical scores,
  hit counts). See [`bench/README.md`](bench/README.md).
- **Against a brute-force model.** `tests/random_vs_reference.rs` indexes random documents
  across many segments (flushes and merges) and checks 1,500 random queries of every shape
  against BM25 computed directly from the raw tokens. Planting bugs in the pruning or matching
  logic makes it fail.
- **Lifecycle.** `tests/index_lifecycle.rs` covers updates and deletes across flushes and
  merges, commit visibility, reopen, locking, rollback, corruption detection and the parser.

## Performance

Single-threaded mean latency per query (median of 30 timed runs per query after warm-up),
469k English Wikipedia articles, 8-vCPU Xeon @ 2.6 GHz. Lucene 10.5.2 on JDK 21 with the
Panama vector module enabled.

| queries | lucene-rs | Lucene | speedup |
|---|---:|---:|---:|
| single term (300) | 27.7 µs | 54.9 µs | 1.98x |
| AND of 2–4 terms (300) | 109.8 µs | 165.0 µs | 1.50x |
| OR of 2–4 terms (301) | 122.7 µs | 203.5 µs | 1.66x |
| exact phrase (300) | 369.9 µs | 728.3 µs | 1.97x |
| open index | 2.2 ms | 378 ms | |
| first pass over 901 queries (cold) | 106 ms | 672 ms | 6.3x |

Methodology, percentiles and the JVM experiments (GC choice, JDK 25, vector API on and off)
are in [`bench/README.md`](bench/README.md).

For best performance build with `RUSTFLAGS="-C target-cpu=native"`: the block decoders and
doc-ID scans then use AVX2/AVX-512 (a portable build measured 1–6% slower).

## Safety and lints

The crate builds with the strict clippy configuration from
[xearch](https://github.com/pc-style/xearch) (`pedantic` and `nursery` denied, plus
`unwrap_used`, `panic`, `indexing_slicing`, `arithmetic_side_effects`, `as_conversions`, ...).
Numeric lints are relaxed only inside the bit-level kernels ported from Lucene, and there are
two audited `unsafe` blocks: memory-mapping index files and an AVX2 prefix sum. Slice indexing
is bounds-checked everywhere, so a corrupt index can make a search panic but cannot cause an
out-of-bounds read; `check_integrity` verifies the CRC32 footer of every file. Details:
[`docs/lints.md`](docs/lints.md).

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
bench/scripts/run-all.sh   # full Lucene comparison (downloads ~1 GB, needs JDK 21+)
```

`src/codec/forutil_gen.rs` is generated: `python3 scripts/gen_forutil.py > src/codec/forutil_gen.rs`.

## License

Apache License 2.0, like Lucene. lucene-rs contains code ported from Apache Lucene; see
[`NOTICE`](NOTICE). "Apache Lucene" and "Lucene" are trademarks of The Apache Software
Foundation. This project is independent and not affiliated with or endorsed by the ASF.
