"""Gate strict top-10 dump equality: query, documents, f32 bits, and reported hits.

A reported hit count may be a lower bound. Equal dumps do not prove that the
engines skipped identical blocks or independently verify the exact match count.
"""

from collections import Counter
from dataclasses import dataclass
import math
import re
import struct
import sys


@dataclass(frozen=True)
class Row:
    kind: str
    terms: str
    total: int
    relation: str
    hits: list[tuple[int, int]]  # (document ID, raw f32 score bits)


def unsigned(text, maximum):
    if re.fullmatch(r"[0-9]+", text) is None or int(text) > maximum:
        raise ValueError(f"invalid unsigned integer: {text!r}")
    return int(text)


def f32_bits(text):
    value = float(text)
    if not math.isfinite(value):
        raise ValueError("score must be finite")
    encoded = struct.pack("<f", value)
    return struct.unpack("<I", encoded)[0]


def load(path):
    rows = []
    with open(path, encoding="utf-8") as source:
        for number, line in enumerate(source, 1):
            try:
                columns = line.rstrip("\n").split("\t")
                if len(columns) == 4:
                    raise ValueError("legacy dump has no hit-count relation; rerun both dump commands")
                if len(columns) != 5:
                    raise ValueError("expected KIND, terms, total, relation, hits (five TSV columns)")
                kind, terms, total, relation, encoded_hits = columns
                if kind not in {"TERM", "AND", "OR", "PHRASE"} or not terms.strip():
                    raise ValueError("invalid query kind or empty query text")
                if relation not in {"eq", "gte"}:
                    raise ValueError("hit-count relation must be eq or gte")
                hits = []
                for hit in encoded_hits.split():
                    doc, score = hit.split(":")
                    hits.append((unsigned(doc, 2**32 - 1), f32_bits(score)))
                rows.append(Row(kind, terms, unsigned(total, 2**64 - 1), relation, hits))
            except (ValueError, OverflowError, struct.error) as error:
                raise ValueError(f"{path}:{number}: {error}") from error
    if not rows:
        raise ValueError(f"{path}: empty dump")
    return rows


def main(argv):
    if len(argv) != 2:
        print("usage: compare.py <lucene-dump.tsv> <rust-dump.tsv>", file=sys.stderr)
        return 2
    try:
        left, right = load(argv[0]), load(argv[1])
    except (OSError, UnicodeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    stats = Counter(dict.fromkeys(
        ["queries", "hits", "same_docs_in_order", "bit_identical_scores", "same_total_hits", "same_hit_relations"], 0
    ))
    different = len(left) != len(right)
    if different:
        print(f"DIFF row count: {len(left)} != {len(right)}")
    for number, (a, b) in enumerate(zip(left, right), 1):
        same_docs = [doc for doc, _ in a.hits] == [doc for doc, _ in b.hits]
        same_scores = [bits for _, bits in a.hits] == [bits for _, bits in b.hits]
        stats["queries"] += 1
        stats["hits"] += len(a.hits)
        stats["same_docs_in_order"] += same_docs
        stats["bit_identical_scores"] += sum(x[1] == y[1] for x, y in zip(a.hits, b.hits))
        stats["same_total_hits"] += a.total == b.total
        stats["same_hit_relations"] += a.relation == b.relation
        reasons = []
        if (a.kind, a.terms) != (b.kind, b.terms):
            reasons.append("query identity")
        if len(a.hits) != len(b.hits):
            reasons.append("hit length")
        if not same_docs:
            reasons.append("documents/order")
        if not same_scores:
            reasons.append("score bits")
        if a.total != b.total:
            reasons.append("total hit value")
        if a.relation != b.relation:
            reasons.append("hit-count relation")
        if reasons:
            different = True
            print(f"DIFF row {number}: {', '.join(reasons)}; {a!r} != {b!r}")
    for key, value in stats.items():
        print(f"{key}: {value}")
    return int(different)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
