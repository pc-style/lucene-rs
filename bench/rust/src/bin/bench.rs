//! Same protocol as `bench/java/Bench.java`:
//!   bench <index> <queries.tsv> dump <out.tsv>            top-10 per query, for diffing with Lucene
//!   bench <index> <queries.tsv> bench <warmup> <iters>    latency benchmark (JSON on stdout)

#![allow(clippy::as_conversions, clippy::cast_precision_loss)]

use lucene_rs::search::TotalHitsRelation;
use lucene_rs::{DirectoryReader, IndexSearcher};
use lucene_rs_bench::{load_queries, summary};
use std::io::Write;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| args.get(i).map(String::as_str).ok_or("missing argument");
    let t_open = Instant::now();
    let searcher = IndexSearcher::new(DirectoryReader::open(arg(1)?)?);
    let open_ms = t_open.elapsed().as_secs_f64() * 1e3;
    let queries = load_queries(arg(2)?)?;
    match arg(3)? {
        "dump" => {
            let mut out = std::io::BufWriter::new(std::fs::File::create(arg(4)?)?);
            for q in &queries {
                let top = searcher.search(&q.query, 10)?;
                let hits: Vec<String> = top
                    .score_docs
                    .iter()
                    .map(|h| format!("{}:{:.9e}", h.doc, h.score))
                    .collect();
                let relation = match top.total_hits.relation {
                    TotalHitsRelation::EqualTo => "eq",
                    TotalHitsRelation::GreaterThanOrEqualTo => "gte",
                };
                writeln!(
                    out,
                    "{}\t{}\t{}\t{}",
                    q.line,
                    top.total_hits.value,
                    relation,
                    hits.join(" ")
                )?;
            }
        }
        "bench" => {
            let warmup: usize = arg(4)?.parse()?;
            let iters: usize = arg(5)?.parse()?;
            let mut sink = 0u64;
            let mut run = |q: &lucene_rs::Query| -> Result<(), lucene_rs::Error> {
                sink = sink.wrapping_add(searcher.search(q, 10)?.total_hits.value);
                Ok(())
            };
            let t = Instant::now();
            for q in &queries {
                run(&q.query)?;
            }
            let cold_pass_ms = t.elapsed().as_secs_f64() * 1e3;
            for _ in 0..warmup {
                for q in &queries {
                    run(&q.query)?;
                }
            }
            let mut times: Vec<Vec<f64>> = vec![Vec::with_capacity(iters); queries.len()];
            for _ in 0..iters {
                for (q, ts) in queries.iter().zip(times.iter_mut()) {
                    let t = Instant::now();
                    run(&q.query)?;
                    ts.push(t.elapsed().as_nanos() as f64 / 1e3);
                }
            }
            let medians: Vec<f64> = times
                .iter_mut()
                .map(|v| {
                    v.sort_by(f64::total_cmp);
                    v.get(v.len() / 2).copied().unwrap_or(0.0)
                })
                .collect();
            let mut parts = Vec::new();
            for kind in ["TERM", "AND", "OR", "PHRASE"] {
                let mut m: Vec<f64> = queries
                    .iter()
                    .zip(&medians)
                    .filter(|(q, _)| q.kind == kind)
                    .map(|(_, &t)| t)
                    .collect();
                if !m.is_empty() {
                    parts.push(format!("\"{kind}\":{}", summary(&mut m)));
                }
            }
            let per_query: Vec<String> = medians.iter().map(|m| format!("{m:.2}")).collect();
            println!(
                "{{\"engine\":\"rust\",\"open_ms\":{open_ms:.2},\"cold_pass_ms\":{cold_pass_ms:.2},{},\"per_query_us\":[{}],\"sink\":{sink}}}",
                parts.join(","),
                per_query.join(",")
            );
        }
        m => return Err(format!("unknown mode {m}").into()),
    }
    Ok(())
}
