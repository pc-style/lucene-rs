"""Render bench/results/summary.json into the README charts and tables.

    python3 bench/scripts/render.py          # rewrite docs/assets/bench-{light,dark}.svg and the
                                             # marked tables in README.md and bench/README.md
    python3 bench/scripts/render.py --check  # exit 1 if any of them is out of date (CI)

Standard library only. summary.json comes from `scripts/report.py`.
"""
import json, sys
from html import escape
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SUMMARY = ROOT / "bench/results/summary.json"

QUERY_ROWS = [  # (kind in summary.json, label in README and chart)
    ("TERM", "single term"),
    ("AND", "AND of 2–4 terms"),
    ("OR", "OR of 2–4 terms"),
    ("PHRASE", "exact phrase"),
]

THEMES = {
    "light": {"text": "#1f2328", "muted": "#59636e", "rule": "#d1d9e0", "rust": "#c4471c", "lucene": "#b6bfc8"},
    "dark": {"text": "#e6edf3", "muted": "#9198a1", "rule": "#3d444d", "rust": "#f07a45", "lucene": "#555e69"},
}


def speedup(lucene, rust):
    s = lucene / rust
    return f"{s:.2f}x" if s < 3 else f"{s:.1f}x" if s < 20 else f"{s:.0f}x"


def us(v):
    return f"{v:.1f} µs"


def ms(v):
    return f"{v:.1f} ms" if v < 10 else f"{v:.0f} ms"


def rows(summary):
    """Chart/table rows: (label, rust, lucene, formatter)."""
    sets = {q["kind"]: q for q in summary["query_sets"]}
    out = [
        (f"{label} ({sets[k]['n']})", sets[k]["rust"]["mean_us"], sets[k]["lucene"]["mean_us"], us)
        for k, label in QUERY_ROWS
    ]
    n_all = sets["ALL"]["n"]
    out.append(("open index", summary["open_ms"]["rust"], summary["open_ms"]["lucene"], ms))
    out.append((f"first pass over {n_all} queries (cold)", summary["cold_pass_ms"]["rust"], summary["cold_pass_ms"]["lucene"], ms))
    return out


def summary_table(summary):
    lines = ["| queries | lucene-rs | Lucene | speedup |", "|---|---:|---:|---:|"]
    for label, r, l, fmt in rows(summary):
        lines.append(f"| {label} | {fmt(r)} | {fmt(l)} | {speedup(l, r)} |")
    return "\n".join(lines)


def detail_table(summary):
    lines = [
        "| queries | rust mean | rust p50 | rust p99 | lucene mean | lucene p50 | lucene p99 | speedup |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for q in summary["query_sets"]:
        r, l = q["rust"], q["lucene"]
        name = "all of the above" if q["kind"] == "ALL" else q["kind"]
        lines.append(
            f"| {name} ({q['n']}) | {us(r['mean_us'])} | {us(r['p50_us'])} | {r['p99_us']:.0f} µs"
            f" | {us(l['mean_us'])} | {us(l['p50_us'])} | {l['p99_us']:.0f} µs | {speedup(l['mean_us'], r['mean_us'])} |"
        )
    o, c = summary["open_ms"], summary["cold_pass_ms"]
    n_all = next(q["n"] for q in summary["query_sets"] if q["kind"] == "ALL")
    lines += [
        "",
        f"Cold start: opening the index takes {ms(o['rust'])} vs {ms(o['lucene'])}, and the first"
        f" (unwarmed) pass over the {n_all} queries {ms(c['rust'])} vs {ms(c['lucene'])}.",
    ]
    return "\n".join(lines)


def svg(summary, theme):
    t = THEMES[theme]
    data = rows(summary)
    width, pad = 860, 28
    label_w, speed_w, value_w = 236, 86, 84
    bar_x = pad + label_w
    bar_max = width - pad - speed_w - value_w - bar_x
    bar_h, bar_gap, row_h = 13, 5, 54
    top = 108
    split = len(QUERY_ROWS)  # rule between per-query latency and cold-start rows
    height = top + row_h * len(data) + 22 + 40
    font = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"

    el = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"'
        f' font-family="{font}" role="img" aria-labelledby="title desc">',
        '<title id="title">lucene-rs vs Apache Lucene 10.5.2</title>',
        f'<desc id="desc">{escape(" ; ".join(f"{lb}: lucene-rs {f(r)}, Lucene {f(l)}, {speedup(l, r)} faster" for lb, r, l, f in data))}</desc>',
        f'<style>text{{fill:{t["text"]};font-variant-numeric:tabular-nums}} .m{{fill:{t["muted"]}}}</style>',
        f'<text x="{pad}" y="40" font-size="20" font-weight="600">lucene-rs vs Apache Lucene 10.5.2</text>',
        f'<text class="m" x="{pad}" y="64" font-size="13">469k Wikipedia articles · Single thread · Bars scaled per row; shorter is faster</text>',
    ]
    # legend
    lx = pad
    for name, color in (("lucene-rs", t["rust"]), ("Lucene 10.5.2 (JDK 21)", t["lucene"])):
        el.append(f'<rect x="{lx}" y="80" width="12" height="12" rx="2" fill="{color}"/>')
        el.append(f'<text x="{lx + 18}" y="90.5" font-size="12.5">{escape(name)}</text>')
        lx += 18 + len(name) * 7.4 + 22
    el.append(f'<text class="m" x="{width - pad}" y="90.5" font-size="12.5" text-anchor="end">speedup</text>')

    y = top
    for i, (label, r, l, fmt) in enumerate(data):
        if i == split:
            y += 22
            el.append(f'<line x1="{pad}" x2="{width - pad}" y1="{y - 14}" y2="{y - 14}" stroke="{t["rule"]}"/>')
        mid = y + bar_h + bar_gap / 2
        el.append(f'<text x="{pad}" y="{mid + 4.5:.1f}" font-size="13.5">{escape(label)}</text>')
        top_v = max(r, l)
        for j, (v, color) in enumerate(((r, t["rust"]), (l, t["lucene"]))):
            by = y + j * (bar_h + bar_gap)
            w = max(2.0, bar_max * v / top_v)
            el.append(f'<rect x="{bar_x}" y="{by}" width="{w:.1f}" height="{bar_h}" rx="2" fill="{color}"/>')
            cls = "" if j == 0 else ' class="m"'
            el.append(f'<text{cls} x="{bar_x + w + 8:.1f}" y="{by + 10.5}" font-size="12">{fmt(v)}</text>')
        el.append(
            f'<text x="{width - pad}" y="{mid + 5.5:.1f}" font-size="17" font-weight="600" text-anchor="end"'
            f' style="fill:{t["rust"]}">{speedup(l, r)}</text>'
        )
        y += row_h
    el.append(
        f'<text class="m" x="{pad}" y="{height - 22}" font-size="11.5">Recorded results · 3 rounds · 30 timed passes (phrases: 20) · Xeon @ 2.6 GHz · Methodology: bench/README.md</text>'
    )
    el.append("</svg>")
    return "\n".join(el) + "\n"


def replace_block(text, name, body, path):
    start, end = f"<!-- {name}:start -->", f"<!-- {name}:end -->"
    a, b = text.find(start), text.find(end)
    if a < 0 or b < a:
        sys.exit(f"{path}: missing {start} ... {end} markers")
    return text[: a + len(start)] + "\n" + body + "\n" + text[b:]


def main():
    check = "--check" in sys.argv[1:]
    summary = json.loads(SUMMARY.read_text())
    outputs = {ROOT / f"docs/assets/bench-{theme}.svg": svg(summary, theme) for theme in THEMES}
    for rel, name, body in (
        ("README.md", "bench:summary", summary_table(summary)),
        ("bench/README.md", "bench:detail", detail_table(summary)),
    ):
        path = ROOT / rel
        current = outputs.get(path) or path.read_text()
        outputs[path] = replace_block(current, name, body, rel)

    stale = [p for p, s in outputs.items() if not p.exists() or p.read_text() != s]
    if check:
        if stale:
            names = ", ".join(str(p.relative_to(ROOT)) for p in stale)
            sys.exit(f"out of date: {names}\nrun: python3 bench/scripts/render.py")
        print("benchmark charts and tables are up to date")
        return
    for p in stale:
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(s := outputs[p])
        print(f"wrote {p.relative_to(ROOT)} ({len(s)} bytes)")


if __name__ == "__main__":
    main()
