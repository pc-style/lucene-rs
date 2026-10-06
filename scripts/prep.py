# /// script
# dependencies = ["pyarrow"]
# ///
"""Build a normalized corpus and query file shared by the Lucene and Rust harnesses.

Normalization (done once here so both engines see identical tokens):
lowercase, every char outside [a-z0-9] becomes a space, tokens longer than
255 chars are dropped (Lucene's WhitespaceTokenizer splits those), one doc per line.
"""
import glob, json, random, re, sys
import pyarrow.parquet as pq

DATA = sys.argv[1]
NON_ALNUM = re.compile(r"[^a-z0-9]+")


def norm(s: str) -> str:
    toks = NON_ALNUM.sub(" ", s.lower()).split()
    return " ".join(t for t in toks if len(t) <= 255)


n = 0
with open(f"{DATA}/corpus.txt", "w") as out:
    for path in sorted(glob.glob(f"{DATA}/wiki-*.parquet")):
        pf = pq.ParquetFile(path)
        for batch in pf.iter_batches(batch_size=4096, columns=["title", "text"]):
            for title, text in zip(batch.column("title").to_pylist(), batch.column("text").to_pylist()):
                line = norm(f"{title} {text}")
                if line:
                    out.write(line + "\n")
                    n += 1
print("docs", n)

queries = []
terms = set()
for line in open(f"{DATA}/queries-sbg.txt"):
    q = json.loads(line)
    tag = q["tags"][0]
    if tag not in ("intersection", "union"):
        continue
    toks = norm(q["query"].replace("+", " ")).split()
    if len(toks) < 2:
        continue
    queries.append(("AND" if tag == "intersection" else "OR", toks))
    terms.update(toks)

random.seed(42)
term_qs = [("TERM", [t]) for t in random.sample(sorted(terms), 300)]
with open(f"{DATA}/queries.tsv", "w") as out:
    for kind, toks in term_qs + queries:
        out.write(f"{kind}\t{' '.join(toks)}\n")
print("queries", len(term_qs) + len(queries))
