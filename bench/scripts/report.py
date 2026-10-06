"""Summarize bench/results/{rust,lucene}{,-phrase}-{1,2,3}.json (median of 3 runs per query).

Prints the detailed table and writes results/summary.json, the single source for the charts and
README tables (render them with `python3 scripts/render.py`). Needs data/queries.*.tsv, which
`scripts/run-all.sh` creates, to know each query's kind.
"""
import json, statistics as st

RUNS = (1, 2, 3)


def load(p):
    return [json.load(open(f"results/{p}-{r}.json")) for r in RUNS]


def pct(a, q):
    a = sorted(a)
    return a[round((len(a) - 1) * q)]


def stats(xs):
    return {"mean_us": round(st.mean(xs), 2), "p50_us": round(pct(xs, 0.5), 2), "p99_us": round(pct(xs, 0.99), 2)}


def query_sets(rust, luc, qfile, kinds):
    lines = [l.split("\t")[0] for l in open(qfile)]
    if any(len(d["per_query_us"]) != len(lines) for d in rust + luc):
        raise ValueError(f"{qfile}: query count does not match timing results")
    for d in rust + luc:
        if any(lines.count(k) != d[k]["n"] for k in kinds if k != "ALL"):
            raise ValueError(f"{qfile}: query kinds do not match timing results")
    pr = [st.median(x) for x in zip(*[d["per_query_us"] for d in rust])]
    pl = [st.median(x) for x in zip(*[d["per_query_us"] for d in luc])]
    out = []
    for k in kinds:
        idx = [i for i, kk in enumerate(lines) if k in (kk, "ALL")]
        out.append({
            "kind": k,
            "n": len(idx),
            "rust": stats([pr[i] for i in idx]),
            "lucene": stats([pl[i] for i in idx]),
        })
    return out


def median_of(runs, key):
    return round(st.median(d[key] for d in runs), 2)


rust, luc = load("rust"), load("lucene")
summary = {
    "query_sets": query_sets(rust, luc, "data/queries.final.tsv", ["TERM", "AND", "OR", "ALL"])
    + query_sets(load("rust-phrase"), load("lucene-phrase"), "data/queries.phrase.tsv", ["PHRASE"]),
    # cold start on the docs+freqs index used by term/AND/OR queries
    "open_ms": {"rust": median_of(rust, "open_ms"), "lucene": median_of(luc, "open_ms")},
    "cold_pass_ms": {"rust": median_of(rust, "cold_pass_ms"), "lucene": median_of(luc, "cold_pass_ms")},
}
with open("results/summary.json", "w") as f:
    json.dump(summary, f, indent=2)
    f.write("\n")

print("| queries | rust mean | rust p50 | rust p99 | lucene mean | lucene p50 | lucene p99 | speedup |")
print("|---|---:|---:|---:|---:|---:|---:|---:|")
for q in summary["query_sets"]:
    r, l = q["rust"], q["lucene"]
    print(f"| {q['kind']} ({q['n']}) | {r['mean_us']:.1f} | {r['p50_us']:.1f} | {r['p99_us']:.0f} | {l['mean_us']:.1f} | {l['p50_us']:.1f} | {l['p99_us']:.0f} | {l['mean_us'] / r['mean_us']:.2f}x |")
o, c = summary["open_ms"], summary["cold_pass_ms"]
print(f"cold pass: rust {c['rust']:.0f} ms, lucene {c['lucene']:.0f} ms; open: rust {o['rust']:.1f} ms, lucene {o['lucene']:.0f} ms")
print("wrote results/summary.json")
