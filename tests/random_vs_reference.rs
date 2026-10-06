//! Randomized end-to-end test: index random documents across many segments (flushes + merges),
//! then check every query shape against a brute-force BM25 reference computed from the raw
//! tokens. The reference shares only the BM25 formula (`Bm25`), which is checked against
//! Apache Lucene separately by the benchmark harness.

// The reference model is plain float/integer math over small synthetic data, and failures should
// panic loudly, so the numeric and unwrap lints are relaxed for this file only.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::panic
)]

use lucene_rs::analysis::WhitespaceAnalyzer;
use lucene_rs::index::{LogMergePolicy, Term};
use lucene_rs::search::TotalHitsRelation;
use lucene_rs::sim::{Bm25, int_to_byte4};
use lucene_rs::*;
use std::collections::HashMap;

struct Rng(u64);
impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    const fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Skewed pick so some words are common and some rare.
    fn word(&mut self) -> String {
        let r = self.below(1000) as f64 / 1000.0;
        format!("w{}", (r * r * r * 40.0) as usize)
    }
}

#[derive(Clone)]
struct RefDoc {
    body: Vec<String>,
    tags: Vec<String>,
}

fn field_tokens<'a>(d: &'a RefDoc, field: &str) -> &'a [String] {
    if field == "body" { &d.body } else { &d.tags }
}

/// What the reference knows about the index (no deletions: stats are exact).
struct Reference {
    docs: Vec<RefDoc>,
    df: HashMap<(String, String), u64>,
    /// field -> (docs with the field, total tokens; for docs-only `tags`, the sum of doc freqs)
    field_stats: HashMap<String, (u64, u64)>,
}

impl Reference {
    fn new(docs: Vec<RefDoc>) -> Self {
        let mut df = HashMap::new();
        let mut field_stats: HashMap<String, (u64, u64)> = HashMap::new();
        for d in &docs {
            for (field, toks) in [("body", &d.body), ("tags", &d.tags)] {
                let mut uniq: Vec<&String> = toks.iter().collect();
                uniq.sort();
                uniq.dedup();
                for t in &uniq {
                    *df.entry((field.to_string(), (*t).clone())).or_default() += 1;
                }
                let e = field_stats.entry(field.to_string()).or_default();
                if !toks.is_empty() {
                    e.0 += 1;
                }
                e.1 += if field == "tags" {
                    uniq.len() as u64
                } else {
                    toks.len() as u64
                };
            }
        }
        Self {
            docs,
            df,
            field_stats,
        }
    }

    fn stats(&self, field: &str) -> (u64, u64) {
        self.field_stats.get(field).copied().unwrap_or_default()
    }
    fn df(&self, field: &str, t: &str) -> u64 {
        self.df
            .get(&(field.to_string(), t.to_string()))
            .copied()
            .unwrap_or(0)
    }
    /// Score of one query on one doc, `None` if it does not match.
    fn score(&self, q: &Query, d: &RefDoc, boost: f32) -> Option<f64> {
        match q {
            Query::Term(term) => {
                let field = term.field.as_str();
                let text = term.text().unwrap();
                let toks = field_tokens(d, field);
                let freq = toks.iter().filter(|x| *x == text).count();
                if freq == 0 {
                    return None;
                }
                let (doc_count, ttf) = self.stats(field);
                // tags: docs only, no norms -> freq 1, norm 1
                if field == "tags" {
                    let sim = Bm25::for_term(boost, self.df(field, text), doc_count, ttf);
                    return Some(f64::from(sim.score(1.0, 1)));
                }
                let sim = Bm25::for_term(boost, self.df(field, text), doc_count, ttf);
                Some(f64::from(
                    sim.score(freq as f32, int_to_byte4(toks.len() as u32)),
                ))
            }
            Query::Boost(inner, b) => self.score(inner, d, boost * b),
            Query::ConstantScore(inner) => self.score(inner, d, 0.0).map(|_| f64::from(boost)),
            Query::MatchAll => Some(f64::from(boost)),
            Query::MatchNone => None,
            Query::Phrase(p) => {
                let toks = field_tokens(d, &p.field);
                let mut freq = 0;
                for start in 0..toks.len() {
                    if p.terms.iter().all(|(t, off)| {
                        toks.get(start + *off as usize)
                            .is_some_and(|x| x.as_bytes() == t.as_slice())
                    }) {
                        freq += 1;
                    }
                }
                if freq == 0 {
                    return None;
                }
                let (doc_count, ttf) = self.stats(&p.field);
                let idf = lucene_rs::sim::idf_sum(p.terms.iter().map(|(t, _)| {
                    lucene_rs::sim::idf(
                        self.df(&p.field, std::str::from_utf8(t).unwrap()),
                        doc_count,
                    )
                }));
                let sim = Bm25::new(boost, idf, lucene_rs::sim::avg_field_length(ttf, doc_count));
                Some(f64::from(
                    sim.score(freq as f32, int_to_byte4(toks.len() as u32)),
                ))
            }
            Query::Boolean(b) => {
                let mut score = 0f64;
                let mut should_matches = 0;
                let mut has_required = false;
                for (cq, occur) in &b.clauses {
                    let s = self.score(
                        cq,
                        d,
                        if matches!(occur, Occur::Must | Occur::Should) {
                            boost
                        } else {
                            0.0
                        },
                    );
                    match occur {
                        Occur::Must => {
                            has_required = true;
                            score += s?;
                        }
                        Occur::Filter => {
                            has_required = true;
                            s?;
                        }
                        Occur::MustNot => {
                            if s.is_some() {
                                return None;
                            }
                        }
                        Occur::Should => {
                            if let Some(s) = s {
                                score += s;
                                should_matches += 1;
                            }
                        }
                    }
                }
                let need = if has_required {
                    b.minimum_should_match
                } else {
                    b.minimum_should_match.max(1)
                };
                (should_matches >= need).then_some(score)
            }
        }
    }
}

fn random_query(rng: &mut Rng, depth: u32) -> Query {
    let term = |rng: &mut Rng| {
        if rng.below(6) == 0 {
            Query::term("tags", format!("t{}", rng.below(8)))
        } else {
            Query::term("body", rng.word())
        }
    };
    match rng.below(if depth > 1 { 6 } else { 9 }) {
        0 | 1 => term(rng),
        2 => {
            let n = 2 + rng.below(3);
            (0..n)
                .fold(BooleanQuery::new(), |b, _| b.should(term(rng)))
                .build()
        }
        3 => {
            let n = 2 + rng.below(2);
            (0..n)
                .fold(BooleanQuery::new(), |b, _| b.must(term(rng)))
                .build()
        }
        4 => {
            let mut p = PhraseQuery::new("body");
            for _ in 0..2 + rng.below(2) {
                p = p.term(rng.word());
            }
            p.build()
        }
        5 => term(rng).boost(0.5 + rng.below(4) as f32),
        _ => {
            let mut b = BooleanQuery::new();
            for _ in 0..=rng.below(4) {
                let occur = [
                    Occur::Must,
                    Occur::Should,
                    Occur::Should,
                    Occur::MustNot,
                    Occur::Filter,
                ][rng.below(5)];
                b = b.clause(random_query(rng, depth + 1), occur);
            }
            if rng.below(4) == 0 {
                b = b.minimum_should_match(1 + rng.below(2));
            }
            match rng.below(8) {
                0 => b.build().constant_score(),
                1 => BooleanQuery::new()
                    .must(Query::MatchAll)
                    .must_not(term(rng))
                    .build(),
                _ => b.build(),
            }
        }
    }
}

fn build_index(dir: &std::path::Path, docs: &[RefDoc], rng: &mut Rng) {
    let config = IndexWriterConfig::new(WhitespaceAnalyzer)
        .max_buffered_docs(97)
        .merge_policy(LogMergePolicy {
            merge_factor: 4,
            min_merge_docs: 50,
            max_merge_docs: u32::MAX,
        });
    let mut w = IndexWriter::open(dir, config).unwrap();
    for (i, d) in docs.iter().enumerate() {
        let mut doc = Document::new()
            .with(Field::text("body", d.body.join(" "), Store::No))
            .with(Field::stored("id", i as i64));
        for t in &d.tags {
            doc.add(Field::string("tags", t.clone(), Store::No));
        }
        w.add_document(&doc).unwrap();
        if rng.below(500) == 0 {
            w.commit().unwrap();
        }
    }
    w.commit().unwrap();
}

/// Checks one query's top-`k` hits, hit count and `count()` against the reference.
fn check_query(searcher: &IndexSearcher, reference: &Reference, q: &Query, k: usize) {
    let top = searcher.search(q, k).unwrap();
    let rq = q.rewrite();
    let expected: Vec<(u32, f64)> = reference
        .docs
        .iter()
        .enumerate()
        .filter_map(|(i, d)| reference.score(&rq, d, 1.0).map(|s| (i as u32, s)))
        .collect();
    // every hit matches with the reference score
    for h in &top.score_docs {
        let e = expected
            .iter()
            .find(|e| e.0 == h.doc)
            .unwrap_or_else(|| panic!("{q:?}: doc {} should not match", h.doc));
        assert!(
            (e.1 as f32 - h.score).abs() <= 1e-5 * e.1.abs().max(1.0) as f32,
            "{q:?}: doc {} score {} vs {}",
            h.doc,
            h.score,
            e.1
        );
    }
    // hits are the true top k (allowing float-rounding ties at the boundary)
    assert_eq!(top.score_docs.len(), expected.len().min(k), "{q:?}");
    if let Some(last) = top.score_docs.last() {
        let better = expected
            .iter()
            .filter(|e| e.1 as f32 > f32::mul_add(last.score, 1.0 + 1e-5, 1e-6))
            .count();
        assert!(better < top.score_docs.len(), "{q:?}: missed a better hit");
    }
    for w in top.score_docs.windows(2) {
        assert!(
            w[0].score > w[1].score || (w[0].score == w[1].score && w[0].doc < w[1].doc),
            "{q:?}: bad order"
        );
    }
    match top.total_hits.relation {
        TotalHitsRelation::EqualTo => {
            assert_eq!(top.total_hits.value, expected.len() as u64, "{q:?}");
        }
        TotalHitsRelation::GreaterThanOrEqualTo => {
            assert!(top.total_hits.value <= expected.len() as u64 && top.total_hits.value >= 1000);
        }
    }
    assert_eq!(
        searcher.count(q).unwrap(),
        expected.len() as u64,
        "count {q:?}"
    );
}

#[test]
fn random_queries_match_brute_force_reference() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let docs: Vec<RefDoc> = (0..3000)
        .map(|_| RefDoc {
            body: (0..rng.below(40)).map(|_| rng.word()).collect(),
            tags: (0..rng.below(3))
                .map(|_| format!("t{}", rng.below(8)))
                .collect(),
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    build_index(dir.path(), &docs, &mut rng);
    let reader = DirectoryReader::open(dir.path()).unwrap();
    assert!(
        reader.segments().len() > 1,
        "test should cover multiple segments"
    );
    assert_eq!(reader.num_docs(), 3000);
    let searcher = IndexSearcher::new(reader);
    let reference = Reference::new(docs);

    for _ in 0..1500 {
        let q = random_query(&mut rng, 0);
        let k = 1 + rng.below(20);
        check_query(&searcher, &reference, &q, k);
    }

    // force-merging to one segment must not change results (stats are index-wide)
    let before: Vec<_> = (0..200).map(|_| random_query(&mut rng, 0)).collect();
    let tops: Vec<_> = before
        .iter()
        .map(|q| searcher.search(q, 10).unwrap())
        .collect();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::new(WhitespaceAnalyzer)).unwrap();
    w.force_merge(1).unwrap();
    w.commit().unwrap();
    drop(w);
    let merged = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    assert_eq!(merged.reader().segments().len(), 1);
    for (q, t) in before.iter().zip(&tops) {
        let m = merged.search(q, 10).unwrap();
        assert_eq!(m.score_docs.len(), t.score_docs.len(), "{q:?}");
        for (a, b) in m.score_docs.iter().zip(&t.score_docs) {
            assert_eq!(a.doc, b.doc, "{q:?}");
            assert!(
                (a.score - b.score).abs() <= 1e-6 * a.score.abs().max(1.0),
                "{q:?}"
            );
        }
    }
    let id = merged.doc(1234).unwrap();
    assert_eq!(
        id.get("id").and_then(lucene_rs::FieldValue::as_i64),
        Some(1234)
    );
    let _ = Term::new("x", "y");
}
