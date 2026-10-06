# Is the JVM Lucene's bottleneck? A Rust port of Lucene's search core

Prompted by [@notpronsh](https://x.com/notpronsh/status/2107411015335887120): "Can someone port Lucene (ElasticSearch's core algorithm) to Rust... I just wanna know if the JVM is a bottleneck".

`lucene-rs/` is a port of the code a Lucene 10.5.2 top-10 BM25 query actually runs, written against the Lucene sources. It covers the index format, scoring, and the dynamic-pruning query algorithms. Both engines then answer the same queries over the same Wikipedia index, and their results are checked to be identical before any timing.

## Answer

The JVM is not *the* bottleneck, but it is a real tax:

- **Steady state (fully JIT-warmed):** the Rust port is **1.41× faster overall** (1.28× on AND, 1.47× on OR, 1.61× on single-term queries). The gap scales with query work: the median ratio is 1.3–1.55× whether a query takes 10 µs or 1 ms. Lucene's fixed per-query overhead is only about 3.5 µs larger.
- **Cold start:** the JVM pays for class loading and JIT. Opening the index takes 360 ms vs 1.4 ms. The first pass over all 901 queries takes 620 ms vs 98 ms (**6.3×**). One process that opens the index and answers 3 queries takes 0.55 s vs under 10 ms.
- Most of the steady-state cost is the algorithms and data layout, which both engines share. In an async-profiler run of Lucene on OR queries, the clearly JVM-specific item is MemorySegment access with its bounds and session checks on mmap'd reads (`ScopedMemoryAccess`, `MemorySessionImpl.checkValidStateRaw`): about 8% of samples. The Rust side was not profiled because `perf` isn't installed in this orb, so the rest of the gap is not attributed.

## Results

Machine: Amp orb, 8 vCPU Intel Xeon @ 2.60GHz (AVX-512), 15 GB RAM. Single-threaded search. Each query is the median of 30 timed passes after 50 warm-up passes (~45k queries). The table is the median of 3 alternating runs.

| query type (n) | Rust port mean | Lucene 10.5.2 / JDK 21 mean | Lucene / Rust |
|---|---:|---:|---:|
| TERM (300) | 33.7 µs | 54.5 µs | 1.61× |
| AND, 2–4 terms (300) | 128.0 µs | 164.0 µs | 1.28× |
| OR, 2–4 terms (301) | 137.1 µs | 202.1 µs | 1.47× |
| all (901) | 99.7 µs | 140.2 µs | 1.41× |
| first pass over all 901 queries | 98 ms | 620 ms | 6.3× |
| index open | 1.4 ms | 360 ms | |

Lucene variants tried, as mean µs for TERM / AND / OR:

| JVM config | TERM | AND | OR |
|---|---:|---:|---:|
| JDK 21, G1, Panama vectors (baseline above) | 54.5 | 164.0 | 202.1 |
| JDK 21, ParallelGC | 53.3 | 163.1 | 194.5 |
| JDK 21, 200 warm-up passes instead of 50 | 55.1 | 166.4 | 202.2 |
| JDK 21, no `jdk.incubator.vector` (scalar fallback) | 56.0 | 200.5 | 233.1 |
| JDK 25, G1, Panama vectors | 61.0 | 182.4 | 239.6 |

Rust variants: a portable x86-64 build (no `target-cpu=native`) measured 34.0 / 136.3 / 143.4 µs, so the result does not depend on AVX-512 codegen.

## Correctness: same answers, bit for bit

- `scripts/compare.py results-lucene.tsv results-rust.tsv`: for all 901 queries, top-10 doc IDs are identical and in the same order, all 8,986 scores are bit-identical float32 values, and total-hit counts are identical. Matching total-hit counts means the collector's 1000-hit threshold and the block skipping fire at the same points.
- `bench idx-rust queries check`: the pruned (block-max) top-10 equals exhaustive scoring for all 901 queries.
- The postings file is 191,864,423 bytes vs Lucene's `.doc` at 191,867,686 bytes: same encoding, give or take headers and the sign of impact norm deltas.

## What was ported

| Rust | Lucene 10.5.2 |
|---|---|
| `forutil.rs` | `ForUtil` (256-int blocks, 8/16/32-bit lane layout), `PForUtil` (patched FOR for freqs) |
| `postings_writer.rs` | `Lucene104PostingsWriter` (FOR or bitset doc blocks, vInt15 skip headers, level-0 skip data every 256 docs, level-1 every 8192, impacts), `CompetitiveImpactAccumulator` |
| `postings_reader.rs` | `Lucene104PostingsReader.BlockPostingsEnum` (lazy freq decode, `advanceShallow`, `nextPostings`, impacts), `FixedBitSet.intoArray`, `VectorUtil.findNextGEQ` |
| `sim.rs` | `SmallFloat.intToByte4` norms, `BM25Similarity` (k1=1.2, b=0.75, same float ops) |
| `search.rs` | `TermScorer`, `MaxScoreCache`, `ImpactsDISI`, `BatchScoreBulkScorer`, `MaxScoreBulkScorer`, `BlockMaxConjunctionBulkScorer`, `ScorerUtil`, `TopScoreDocCollector` (1000-hit threshold) |
| `index.rs` | not a port: in-memory inverter and a simple block term dictionary in place of `IndexWriter` and BlockTree/FST |

About 2,600 lines of Rust. Dependencies: `memmap2` and `rustc-hash`.

## Scope and caveats

- One text field, one segment (Lucene force-merged to 1), no deletions, `DOCS_AND_FREQS` (no positions, so no phrase queries). Lucene is configured the same way.
- Both engines get identical tokens: the corpus is pre-normalized (lowercase, `[a-z0-9]` runs) and Lucene uses `WhitespaceTokenizer`.
- The indexer is not a port of `IndexWriter`, so indexing time is not comparable. For reference: Rust 28 s, Lucene 69 s plus 9 s `forceMerge`.
- One cloud VM. Per-type mean latencies varied by under 5% between runs; Lucene's cold-pass time varied more (614–710 ms).

## Reproduce

Needs JDK 21, Rust, `uv`, and ~4 GB of disk. `scripts/run-all.sh` downloads 3 English Wikipedia parquet shards (468,867 articles, 299M tokens) and the search-benchmark-game AOL queries, builds both indexes, verifies equivalence, and benchmarks.
