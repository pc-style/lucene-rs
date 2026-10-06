#!/usr/bin/env bash
# End-to-end: data, both indexes, equivalence checks, benchmark. Run from the repo root.
# Needs: java/javac 21 on PATH, cargo, uv, curl, jq.
set -euo pipefail
V=10.5.2
mkdir -p jars data results
for a in lucene-core lucene-analysis-common; do
  [ -f jars/$a-$V.jar ] || curl -sSL -o jars/$a-$V.jar https://repo1.maven.org/maven2/org/apache/lucene/$a/$V/$a-$V.jar
done
for i in 00000 00001 00002; do
  [ -f data/wiki-$i.parquet ] || curl -sSL -o data/wiki-$i.parquet \
    "https://huggingface.co/datasets/wikimedia/wikipedia/resolve/main/20231101.en/train-$i-of-00041.parquet"
done
[ -f data/queries-sbg.txt ] || curl -sSL -o data/queries-sbg.txt \
  https://raw.githubusercontent.com/quickwit-oss/search-benchmark-game/master/queries.txt
[ -f data/corpus.txt ] || uv run -q scripts/prep.py data

(cd lucene-rs && cargo build --release -q)
CP="java/classes:jars/lucene-core-$V.jar:jars/lucene-analysis-common-$V.jar"
javac -cp "$CP" -d java/classes java/Indexer.java java/Bench.java
JAVA="java -Xms4g -Xmx4g --add-modules jdk.incubator.vector"

lucene-rs/target/release/index data/corpus.txt idx-rust
$JAVA -Xms8g -Xmx8g -cp "$CP" Indexer data/corpus.txt idx-lucene 2>/dev/null
lucene-rs/target/release/bench idx-rust data/queries.tsv filter data/queries.final.tsv

lucene-rs/target/release/bench idx-rust data/queries.final.tsv check
lucene-rs/target/release/bench idx-rust data/queries.final.tsv dump results-rust.tsv
$JAVA -cp "$CP" Bench idx-lucene data/queries.final.tsv dump results-lucene.tsv 2>/dev/null
python3 scripts/compare.py results-lucene.tsv results-rust.tsv

for r in 1 2 3; do
  lucene-rs/target/release/bench idx-rust data/queries.final.tsv bench 50 30 > results/rust-$r.json
  $JAVA -cp "$CP" Bench idx-lucene data/queries.final.tsv bench 50 30 2>/dev/null > results/lucene-$r.json
done
python3 scripts/analyze.py
