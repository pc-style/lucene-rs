#!/usr/bin/env bash
# End-to-end benchmark: data, both engines' indexes, equivalence checks, timing.
# Run from anywhere; needs java/javac 21+, cargo, uv, curl. About 4 GB of disk.
set -euo pipefail
cd "$(dirname "$0")/.."
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

(cd .. && cargo build --release -q -p lucene-rs-bench)
CP="java/classes:jars/lucene-core-$V.jar:jars/lucene-analysis-common-$V.jar"
javac -cp "$CP" -d java/classes java/Indexer.java java/Bench.java
JAVA="java -Xms8g -Xmx8g --add-modules jdk.incubator.vector -cp $CP"

../target/release/index data/corpus.txt idx-rust
../target/release/index data/corpus.txt idx-rust-pos --positions
$JAVA Indexer data/corpus.txt idx-lucene 2>/dev/null
$JAVA Indexer data/corpus.txt idx-lucene-pos --positions 2>/dev/null

for set in final:idx-rust:idx-lucene phrase:idx-rust-pos:idx-lucene-pos; do
  IFS=: read -r name rust lucene <<< "$set"
  ../target/release/bench "$rust" "data/queries.$name.tsv" dump "results-rust-$name.tsv"
  $JAVA Bench "$lucene" "data/queries.$name.tsv" dump "results-lucene-$name.tsv" 2>/dev/null
  python3 scripts/compare.py "results-lucene-$name.tsv" "results-rust-$name.tsv"
done

scripts/bench-all.sh
python3 scripts/report.py
