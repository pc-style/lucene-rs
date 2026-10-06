"""Per-query Lucene/Rust latency ratios, bucketed by how long the query takes in Rust."""
import json, statistics as st, sys
def load(pfx):
    runs = [json.load(open(f"results/{pfx}-{r}.json")) for r in (1, 2, 3)]
    return [st.median(x) for x in zip(*[r["per_query_us"] for r in runs])], runs
rust, rr = load("rust"); luc, lr = load("lucene")
kinds = [l.split("\t")[0] for l in open("data/queries.final.tsv")]
print(f"{'kind':5} {'rust mean us':>13} {'lucene mean us':>15} {'speedup':>8}")
for k in ("TERM", "AND", "OR", "ALL"):
    idx = [i for i, kk in enumerate(kinds) if k in (kk, "ALL")]
    r = sum(rust[i] for i in idx) / len(idx); l = sum(luc[i] for i in idx) / len(idx)
    print(f"{k:5} {r:13.1f} {l:15.1f} {l/r:8.2f}x")
print("\ncold first pass (all 901 queries):",
      "rust", round(st.median(x["cold_pass_ms"] for x in rr), 1), "ms;",
      "lucene", round(st.median(x["cold_pass_ms"] for x in lr), 1), "ms")
print("\nby Rust latency bucket: median Lucene/Rust ratio, and absolute gap")
order = sorted(range(len(rust)), key=lambda i: rust[i])
for lo, hi in [(0, 20), (20, 50), (50, 100), (100, 300), (300, 1e9)]:
    idx = [i for i in order if lo <= rust[i] < hi]
    if not idx: continue
    ratios = [luc[i] / rust[i] for i in idx]; gaps = [luc[i] - rust[i] for i in idx]
    print(f"  rust {lo:>4}-{hi if hi < 1e9 else 'inf':>4} us  n={len(idx):3}  ratio p50={st.median(ratios):.2f}  gap p50={st.median(gaps):6.1f} us")
print("\nfastest queries (fixed per-query overhead floor):")
for i in order[:5]:
    print(f"  {kinds[i]:4} rust {rust[i]:6.1f} us  lucene {luc[i]:6.1f} us")
