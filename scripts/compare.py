"""Compare top-10 dumps from Lucene and the Rust port: doc ids, float32 scores, total hit counts."""
import struct, sys
from collections import Counter

def f32(s):
    return struct.unpack("f", struct.pack("f", float(s)))[0]

def load(p):
    out = []
    for line in open(p):
        kind, terms, total, hits = line.rstrip("\n").split("\t")
        hits = [(int(d), f32(s)) for d, s in (h.split(":") for h in hits.split())] if hits else []
        out.append((kind, terms, int(total), hits))
    return out

a, b = load(sys.argv[1]), load(sys.argv[2])
assert len(a) == len(b)
stats = Counter()
for (k, t, ta, ha), (_, t2, tb, hb) in zip(a, b):
    assert t == t2
    stats["queries"] += 1
    stats["hits"] += len(ha)
    stats["same_docs_in_order"] += [d for d, _ in ha] == [d for d, _ in hb]
    stats["bit_identical_scores"] += sum(x[1] == y[1] for x, y in zip(ha, hb))
    stats["same_total_hits"] += ta == tb
    if [d for d, _ in ha] != [d for d, _ in hb] or ta != tb:
        print("DIFF", k, t, ta, tb, ha[:3], hb[:3])
for k, v in stats.items():
    print(f"{k}: {v}")
