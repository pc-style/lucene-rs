//! Same protocol as Bench.java:
//!   bench <index> <queries.tsv> dump <out.tsv>          top-10 per query, for diffing with Lucene
//!   bench <index> <queries.tsv> check                   pruned top-k vs exhaustive scoring
//!   bench <index> <queries.tsv> bench <warmup> <iters>  latency benchmark (JSON on stdout)
//!   bench <index> <queries.tsv> filter <out.tsv>        keep queries whose terms all exist, deduped
use lucene_rs::index::Index;
use lucene_rs::postings_reader::{NO_MORE_DOCS, PostingsEnum};
use lucene_rs::search::{Kind, TermScorer, search};
use lucene_rs::sim::Bm25;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

struct Query {
    kind: Kind,
    line: String,
    terms: Vec<String>,
}

fn load_queries(path: &str) -> Vec<Query> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (k, t) = l.split_once('\t').unwrap();
            let kind = match k {
                "TERM" => Kind::Term,
                "AND" => Kind::And,
                "OR" => Kind::Or,
                _ => panic!("bad kind {k}"),
            };
            Query { kind, line: l.to_string(), terms: t.split(' ').map(String::from).collect() }
        })
        .collect()
}

fn run(index: &Index, q: &Query) -> lucene_rs::search::TopDocs {
    let terms: Vec<&str> = q.terms.iter().map(|s| s.as_str()).collect();
    search(index, q.kind, &terms, 10)
}

/// Score every matching doc, no pruning. Sums in clause order in f64 like the bulk scorers.
fn exhaustive(index: &Index, q: &Query) -> (u64, Vec<(i32, f32)>) {
    let mut sums: HashMap<i32, (f64, usize)> = HashMap::new();
    for t in &q.terms {
        let meta = index.lookup(t.as_bytes()).unwrap();
        let sim = Bm25::new(1.0, meta.doc_freq as u64, index.doc_count, index.sum_total_term_freq);
        let mut s = TermScorer::new(PostingsEnum::new(&index.doc, &meta), sim, &index.norms, false);
        while s.pe.next_doc() != NO_MORE_DOCS {
            let e = sums.entry(s.doc_id()).or_insert((0.0, 0));
            e.0 += s.score() as f64;
            e.1 += 1;
        }
    }
    let need = if q.kind == Kind::And { q.terms.len() } else { 1 };
    let mut hits: Vec<(i32, f32)> =
        sums.into_iter().filter(|(_, (_, n))| *n >= need).map(|(d, (s, _))| (d, s as f32)).collect();
    let total = hits.len() as u64;
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    hits.truncate(10);
    (total, hits)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let t_open = Instant::now();
    let index = Index::open(Path::new(&args[1])).unwrap();
    let open_ms = t_open.elapsed().as_secs_f64() * 1e3;
    let queries = load_queries(&args[2]);
    match args[3].as_str() {
        "filter" => {
            let mut out = std::fs::File::create(&args[4]).unwrap();
            let mut kept = 0;
            for q in &queries {
                let mut seen = Vec::new();
                for t in &q.terms {
                    if !seen.contains(t) {
                        seen.push(t.clone());
                    }
                }
                let ok = seen.iter().all(|t| index.lookup(t.as_bytes()).is_some())
                    && (q.kind == Kind::Term || seen.len() >= 2);
                if ok {
                    writeln!(out, "{}\t{}", q.line.split('\t').next().unwrap(), seen.join(" ")).unwrap();
                    kept += 1;
                }
            }
            eprintln!("kept {kept}/{}", queries.len());
        }
        "dump" => {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&args[4]).unwrap());
            for q in &queries {
                let td = run(&index, q);
                let hits: Vec<String> = td.hits.iter().map(|(d, s)| format!("{d}:{s:.9e}")).collect();
                writeln!(out, "{}\t{}\t{}", q.line, td.total_hits, hits.join(" ")).unwrap();
            }
        }
        "check" => {
            let (mut bad, mut checked) = (0, 0);
            for q in &queries {
                let td = run(&index, q);
                let (total, exp) = exhaustive(&index, q);
                checked += 1;
                let mut ok = td.hits.len() == exp.len();
                for (a, b) in td.hits.iter().zip(exp.iter()) {
                    let close = (a.1 - b.1).abs() <= 1e-6 * b.1.abs().max(1e-6);
                    // docs may only differ where two hits tie on score
                    ok &= close && (a.0 == b.0 || exp.iter().any(|e| e.0 == a.0 && (e.1 - a.1).abs() <= 1e-6 * a.1));
                }
                // TopScoreDocCollector counts exactly up to the 1000-hit threshold
                ok &= if total <= 1000 { td.total_hits == total } else { td.total_hits >= 1000 && td.total_hits <= total };
                if !ok {
                    bad += 1;
                    if bad <= 5 {
                        eprintln!("MISMATCH {}\n  got  {} {:?}\n  want {} {:?}", q.line, td.total_hits, td.hits, total, exp);
                    }
                }
            }
            println!("check: {}/{} queries match exhaustive scoring", checked - bad, checked);
            if bad > 0 {
                std::process::exit(1);
            }
        }
        "bench" => {
            let warmup: usize = args[4].parse().unwrap();
            let iters: usize = args[5].parse().unwrap();
            let mut sink = 0u64;
            // cold pass: first execution of every query, in order
            let t = Instant::now();
            for q in &queries {
                sink += run(&index, q).total_hits;
            }
            let cold_pass_ms = t.elapsed().as_secs_f64() * 1e3;
            for _ in 0..warmup {
                for q in &queries {
                    sink += run(&index, q).total_hits;
                }
            }
            let mut times = vec![Vec::with_capacity(iters); queries.len()];
            for _ in 0..iters {
                for (i, q) in queries.iter().enumerate() {
                    let t = Instant::now();
                    sink += run(&index, q).total_hits;
                    times[i].push(t.elapsed().as_nanos() as f64 / 1e3);
                }
            }
            let medians: Vec<f64> = times
                .iter_mut()
                .map(|v| {
                    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    v[v.len() / 2]
                })
                .collect();
            let mut parts = vec![];
            for (name, kind) in [("TERM", Kind::Term), ("AND", Kind::And), ("OR", Kind::Or)] {
                let mut m: Vec<f64> =
                    queries.iter().zip(&medians).filter(|(q, _)| q.kind == kind).map(|(_, &t)| t).collect();
                if m.is_empty() {
                    continue;
                }
                m.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let mean = m.iter().sum::<f64>() / m.len() as f64;
                parts.push(format!(
                    "\"{name}\":{{\"n\":{},\"mean_us\":{mean:.2},\"p50_us\":{:.2},\"p90_us\":{:.2},\"p99_us\":{:.2}}}",
                    m.len(),
                    percentile(&m, 0.5),
                    percentile(&m, 0.9),
                    percentile(&m, 0.99)
                ));
            }
            let per_query: Vec<String> = medians.iter().map(|m| format!("{m:.2}")).collect();
            println!(
                "{{\"engine\":\"rust\",\"open_ms\":{open_ms:.2},\"cold_pass_ms\":{cold_pass_ms:.2},{},\"per_query_us\":[{}],\"sink\":{sink}}}",
                parts.join(","),
                per_query.join(",")
            );
        }
        m => panic!("unknown mode {m}"),
    }
}
