# Benchmark: lucene-rs vs Apache Lucene

This directory compares lucene-rs with Apache Lucene 10.5.2 on the same corpus and queries,
first for equivalence (same top-10, same scores), then for speed.

## Setup

- **Corpus**: 3 shards of English Wikipedia (`wikimedia/wikipedia`, 20231101.en), 468,867
  articles, 299M tokens. `scripts/prep.py` lowercases and keeps `[a-z0-9]` runs so both engines
  see identical tokens (Lucene uses `WhitespaceTokenizer`, lucene-rs `WhitespaceAnalyzer`).
- **Queries**: the AOL-derived set from
  [search-benchmark-game](https://github.com/quickwit-oss/search-benchmark-game): 300 AND and
  301 OR queries, 300 single terms sampled from their tokens, and 300 exact phrases.
- **Indexes**: one field `body`, one segment (`force_merge(1)` in both). Term/AND/OR queries run
  on docs+freqs indexes, phrases on indexes with positions.
- **Harness**: `rust/` (lucene-rs public API) and `java/` (Lucene API) implement the same
  protocol: `search(query, 10)`, single thread, one pass cold, then 50 warm-up / 30 timed
  passes for term/AND/OR, or 20 warm-up / 20 timed passes for phrases. Each query's median
  is reported. Three alternating rounds; the table aggregates the per-query medians of the three.
- **Machine**: Amp orb, 8 vCPU Intel Xeon @ 2.60GHz (AVX-512), 15 GB RAM, Linux 6.1. Rust 1.99
  with `-C target-cpu=native`; JDK 21.0.12 (Temurin), `-Xms4g -Xmx4g`, G1,
  `--add-modules jdk.incubator.vector` (Lucene reports "Java vector incubator API enabled;
  uses preferredBitSize=512").

## Equivalence

`scripts/compare.py` diffs the top-10 dumps:

| query set | queries | same docs, same order | bit-identical scores | same hit count |
|---|---:|---:|---:|---:|
| term / AND / OR | 901 | 901 | 8,986 / 8,986 | 901 |
| exact phrase | 300 | 300 | 2,459 / 2,459 | 300 |

Matching hit counts mean the collector's 1,000-hit threshold and the block skipping fire at the
same points in both engines.

## Results

<!-- bench:detail:start -->
| queries | rust mean | rust p50 | rust p99 | lucene mean | lucene p50 | lucene p99 | speedup |
|---|---:|---:|---:|---:|---:|---:|---:|
| TERM (300) | 27.7 µs | 20.7 µs | 121 µs | 54.9 µs | 36.3 µs | 311 µs | 1.98x |
| AND (300) | 109.8 µs | 81.7 µs | 564 µs | 165.0 µs | 116.4 µs | 796 µs | 1.50x |
| OR (301) | 122.7 µs | 77.2 µs | 999 µs | 203.5 µs | 142.0 µs | 1416 µs | 1.66x |
| all of the above (901) | 86.8 µs | 52.5 µs | 584 µs | 141.2 µs | 94.0 µs | 817 µs | 1.63x |
| PHRASE (300) | 369.9 µs | 137.0 µs | 4033 µs | 728.3 µs | 210.8 µs | 5824 µs | 1.97x |

Cold start: opening the index takes 2.2 ms vs 378 ms, and the first (unwarmed) pass over the 901 queries 106 ms vs 672 ms.
<!-- bench:detail:end -->

### Where the gap comes from

The first, deliberately literal port (commit `8474ff3`) was already 1.41x faster than Lucene
with the same algorithms and data layout. The gap scaled with the work per query rather than
being a fixed per-query cost. In an async-profiler run of Lucene, MemorySegment access checks
on mmap'd reads were about 8% of OR-query time. Later optimizations, all keeping results
bit-identical:

- per-bit-width generated block decoders (like Lucene's generated `ForUtil`), instead of a
  generic loop;
- 16-wide SIMD `findNextGEQ` with a binary-search fallback, and an AVX2 prefix sum for doc IDs;
- computing a block's max score as `score(max(freq * normInverse))`, exact because BM25 is
  monotonic under IEEE rounding (one division instead of one per impact);
- reusing per-query scratch buffers instead of allocating and zeroing them;
- for phrases: block and per-doc score bounds from term impacts, and lazy position matching
  that starts from the rarest term in each doc and stops once no candidate is left.

### JVM experiments (on the first port)

With Lucene's settings varied, mean µs for TERM / AND / OR:

| JVM config | TERM | AND | OR |
|---|---:|---:|---:|
| JDK 21, G1, Panama vectors (baseline) | 54.5 | 164.0 | 202.1 |
| JDK 21, ParallelGC | 53.3 | 163.1 | 194.5 |
| JDK 21, 200 warm-up passes instead of 50 | 55.1 | 166.4 | 202.2 |
| JDK 21, no `jdk.incubator.vector` | 56.0 | 200.5 | 233.1 |
| JDK 25, G1, Panama vectors | 61.0 | 182.4 | 239.6 |

GC choice and longer warm-up do not matter; the vector API helps Lucene; JDK 25 was slower
than 21 on this machine. Raw files are in `results/jvm-experiments/`.

## Reproduce

```sh
scripts/run-all.sh      # download, prep, build 4 indexes, compare, benchmark, report
scripts/quick.sh        # after a code change: rebuild, re-diff against Lucene, quick timing
scripts/prof.sh or and  # perf profile per query kind (see the build line in the script)
```

Needs JDK 21+, Rust, `uv` and ~4 GB of disk. Indexing times are not compared: the Lucene
indexer goes through Lucene's `IndexWriter` with its own flush/merge behavior.

`scripts/report.py` writes `results/summary.json` from the raw timing files and query-kind
files. Run `python3 scripts/render.py` to regenerate both SVG themes and README tables,
or `python3 scripts/render.py --check` to check them without writing. Rendering the checked-in
summary needs only Python; it does not rerun benchmarks or download the corpus.
