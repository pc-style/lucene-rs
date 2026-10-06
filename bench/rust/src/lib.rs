//! Shared helpers for the benchmark binaries: the query file format and query construction.

// A measurement tool, not library code: numeric conversions on timings and counts are fine here.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use lucene_rs::{BooleanQuery, PhraseQuery, Query};

pub const FIELD: &str = "body";

/// One line of a query file: `KIND<TAB>space separated terms`, KIND in TERM, AND, OR, PHRASE.
pub struct BenchQuery {
    pub kind: String,
    pub line: String,
    pub query: Query,
}

/// Parses a query file.
///
/// # Errors
/// I/O errors, or `InvalidData` for a malformed line.
pub fn load_queries(path: &str) -> std::io::Result<Vec<BenchQuery>> {
    let bad = |l: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad query line: {l}"),
        )
    };
    std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (kind, text) = l.split_once('\t').ok_or_else(|| bad(l))?;
            let terms = text.split(' ');
            let query = match kind {
                "TERM" => Query::term(FIELD, text),
                "AND" => terms
                    .fold(BooleanQuery::new(), |b, t| b.must(Query::term(FIELD, t)))
                    .build(),
                "OR" => terms
                    .fold(BooleanQuery::new(), |b, t| b.should(Query::term(FIELD, t)))
                    .build(),
                "PHRASE" => terms
                    .fold(PhraseQuery::new(FIELD), PhraseQuery::term)
                    .build(),
                _ => return Err(bad(l)),
            };
            Ok(BenchQuery {
                kind: kind.to_string(),
                line: l.to_string(),
                query,
            })
        })
        .collect()
}

/// Mean and percentiles of per-query latencies, as a JSON object.
#[must_use]
pub fn summary(times_us: &mut [f64]) -> String {
    times_us.sort_by(f64::total_cmp);
    let n = times_us.len();
    let pct = |p: f64| {
        times_us
            .get(((n.saturating_sub(1)) as f64 * p).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    let mean = if n == 0 {
        0.0
    } else {
        times_us.iter().sum::<f64>() / n as f64
    };
    format!(
        "{{\"n\":{n},\"mean_us\":{mean:.2},\"p50_us\":{:.2},\"p90_us\":{:.2},\"p99_us\":{:.2}}}",
        pct(0.5),
        pct(0.9),
        pct(0.99)
    )
}
