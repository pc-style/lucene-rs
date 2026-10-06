#!/usr/bin/env bash
# Fast iteration loop after a code change: rebuild, diff top-10s against Lucene, short timing run.
# Assumes run-all.sh has built the indexes and Lucene result dumps once.
set -euo pipefail
cd "$(dirname "$0")/.."
(cd .. && cargo build --release -q -p lucene-rs-bench)
B=../target/release/bench
for set in final:idx-rust phrase:idx-rust-pos; do
  IFS=: read -r name idx <<< "$set"
  $B "$idx" "data/queries.$name.tsv" dump "results-rust-$name.tsv"
  python3 scripts/compare.py "results-lucene-$name.tsv" "results-rust-$name.tsv" | grep -v '^DIFF' | tr '\n' ' '; echo
done
$B idx-rust data/queries.final.tsv bench 10 10 | python3 -c "import json,sys; d=json.load(sys.stdin); print({k: d[k]['mean_us'] for k in ('TERM','AND','OR')})"
