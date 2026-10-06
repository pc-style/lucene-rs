"""Summarize bench/results/{rust,lucene}{,-phrase}-{1,2,3}.json (median of 3 runs per query)."""
import json, statistics as st

def load(p):
    return [json.load(open(f"results/{p}-{r}.json")) for r in (1, 2, 3)]

def pct(a, q):
    a = sorted(a)
    return a[round((len(a) - 1) * q)]

def report(rust, luc, qfile, kinds):
    lines = [l.split("\t")[0] for l in open(qfile)]
    pr = [st.median(x) for x in zip(*[d["per_query_us"] for d in rust])]
    pl = [st.median(x) for x in zip(*[d["per_query_us"] for d in luc])]
    for k in kinds:
        idx = [i for i, kk in enumerate(lines) if k in (kk, "ALL")]
        r = [pr[i] for i in idx]; l = [pl[i] for i in idx]
        print(f"| {k} ({len(idx)}) | {st.mean(r):.1f} | {pct(r,.5):.1f} | {pct(r,.99):.0f} | {st.mean(l):.1f} | {pct(l,.5):.1f} | {pct(l,.99):.0f} | {st.mean(l)/st.mean(r):.2f}x |")
    print(f"cold pass: rust {st.median(d['cold_pass_ms'] for d in rust):.0f} ms, lucene {st.median(d['cold_pass_ms'] for d in luc):.0f} ms;"
          f" open: rust {st.median(d['open_ms'] for d in rust):.1f} ms, lucene {st.median(d['open_ms'] for d in luc):.0f} ms")

print("| queries | rust mean | rust p50 | rust p99 | lucene mean | lucene p50 | lucene p99 | speedup |")
print("|---|---:|---:|---:|---:|---:|---:|---:|")
report(load("rust"), load("lucene"), "data/queries.final.tsv", ["TERM", "AND", "OR", "ALL"])
report(load("rust-phrase"), load("lucene-phrase"), "data/queries.phrase.tsv", ["PHRASE"])
