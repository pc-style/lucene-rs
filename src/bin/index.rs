use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (corpus, dir) = (Path::new(&args[1]), Path::new(&args[2]));
    let t = std::time::Instant::now();
    let s = lucene_rs::index::build_index(corpus, dir).expect("index");
    println!(
        "{{\"engine\":\"rust\",\"docs\":{},\"terms\":{},\"tokens\":{},\"invert_secs\":{:.2},\"write_secs\":{:.2},\"total_secs\":{:.2}}}",
        s.max_doc, s.num_terms, s.sum_total_term_freq, s.invert_secs, s.write_secs, t.elapsed().as_secs_f64()
    );
}
