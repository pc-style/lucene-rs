use lucene_rs::index::Index;
use std::time::Instant;
fn main() {
    let index = Index::open(std::path::Path::new("../idx-rust")).unwrap();
    let terms: Vec<String> = std::fs::read_to_string("../data/queries.final.tsv").unwrap().lines()
        .flat_map(|l| l.split('\t').nth(1).unwrap().split(' ').map(String::from).collect::<Vec<_>>()).collect();
    let mut sink = 0u64;
    for _ in 0..3 { for t in &terms { sink += index.lookup(t.as_bytes()).unwrap().doc_freq as u64; } }
    let n = 50; let t0 = Instant::now();
    for _ in 0..n { for t in &terms { sink += index.lookup(t.as_bytes()).unwrap().doc_freq as u64; } }
    println!("{} lookups, {:.0} ns/lookup (sink {sink})", terms.len(), t0.elapsed().as_nanos() as f64 / (n * terms.len()) as f64);
}
