#!/usr/bin/env bash
# Fast iteration loop: build, verify (exhaustive + vs Lucene), short benchmark. Run from bench/.
set -euo pipefail
export PATH="$HOME/.cargo/bin:$HOME/opt/jdk/bin:$PATH"
cd "$(dirname "$0")/.."
(cd .. && cargo build --release -q)
B=../target/release/bench
$B idx-rust data/queries.final.tsv check
$B idx-rust data/queries.final.tsv dump results-rust.tsv
python3 scripts/compare.py results-lucene.tsv results-rust.tsv | grep -v '^DIFF' | tr '\n' ' '; echo
for r in ${RUNS:-1 2 3}; do
  $B idx-rust data/queries.final.tsv bench ${WARM:-20} ${ITERS:-30} > /tmp/quick-$r.json
done
python3 - <<'PY'
import json, statistics as st
runs = [json.load(open(f"/tmp/quick-{r}.json")) for r in (1, 2, 3)]
med = lambda f: st.median(f(d) for d in runs)
allm = med(lambda d: st.mean(d["per_query_us"]))
print("median of 3:", " ".join(f"{k} {med(lambda d: d[k]['mean_us']):.1f}" for k in ("TERM", "AND", "OR")), f"ALL {allm:.1f} us  cold {med(lambda d: d['cold_pass_ms']):.0f} ms")
PY
