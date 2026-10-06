#!/usr/bin/env bash
# Alternating Rust/Lucene latency runs (3 rounds) on term/AND/OR and phrase query sets.
set -euo pipefail
cd "$(dirname "$0")/.."
CP="java/classes:jars/lucene-core-10.5.2.jar:jars/lucene-analysis-common-10.5.2.jar"
JAVA="java -Xms4g -Xmx4g --add-modules jdk.incubator.vector -cp $CP Bench"
mkdir -p results
for r in 1 2 3; do
  ../target/release/bench idx-rust data/queries.final.tsv bench 50 30 > results/rust-$r.json
  ../target/release/bench idx-rust-pos data/queries.phrase.tsv bench 20 20 > results/rust-phrase-$r.json
  $JAVA idx-lucene data/queries.final.tsv bench 50 30 2>/dev/null > results/lucene-$r.json
  $JAVA idx-lucene-pos data/queries.phrase.tsv bench 20 20 2>/dev/null > results/lucene-phrase-$r.json
done
