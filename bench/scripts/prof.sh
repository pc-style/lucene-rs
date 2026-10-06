#!/usr/bin/env bash
# Function-level profile of the Rust bench per query kind (build with --features profile).
B=${B:-../target-prof/release/bench}  # build: cargo build --release -p lucene-rs-bench --features lucene-rs/profile --target-dir target-prof
for k in ${@:-or and term}; do
  echo "== $k"
  perf record -q -F 4999 -o /tmp/perf-$k.data $B idx-rust data/q-$k.tsv bench 20 60 >/dev/null 2>&1
  perf report -i /tmp/perf-$k.data --no-children --stdio --percent-limit 1 2>/dev/null | grep -E "^\s+[0-9]" | sed -E 's/ +bench +bench +\[\.\] / /; s/lucene_rs:://g' | head -24
done
