#!/usr/bin/env bash
# Interleaved A/B timing of two bench binaries on the same index (cancels VM drift).
# usage: ab.sh <binA> <binB> [rounds]
cd "$(dirname "$0")/.."
A=$1; B=$2; R=${3:-4}
for r in $(seq $R); do
  $A idx-rust data/queries.final.tsv bench 15 25 > /tmp/ab-a-$r.json
  $B idx-rust data/queries.final.tsv bench 15 25 > /tmp/ab-b-$r.json
done
python3 - "$R" "$A" "$B" <<'PY'
import json, statistics as st, sys
R = int(sys.argv[1])
load = lambda x: [json.load(open(f"/tmp/ab-{x}-{r}.json")) for r in range(1, R + 1)]
a, b = load("a"), load("b")
kinds = [l.split("\t")[0] for l in open("data/queries.final.tsv")]
def mean(runs, k):
    # per-query median across runs, then mean over queries of kind k
    pq = [st.median(x) for x in zip(*[d["per_query_us"] for d in runs])]
    sel = [v for v, kk in zip(pq, kinds) if k in (kk, "ALL")]
    return sum(sel) / len(sel)
print(f"{'':6}{'A':>9}{'B':>9}{'B/A':>8}   A={sys.argv[2]} B={sys.argv[3]}")
for k in ("TERM", "AND", "OR", "ALL"):
    ma, mb = mean(a, k), mean(b, k)
    print(f"{k:6}{ma:9.1f}{mb:9.1f}{mb/ma:8.3f}")
PY
