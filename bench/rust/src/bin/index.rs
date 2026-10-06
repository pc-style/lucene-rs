//! `index <corpus.txt> <index-dir> [--positions]`: one document per line in field `body`,
//! whitespace-tokenized, single segment. Without `--positions` the field is indexed with
//! docs and freqs only, matching `bench/java/Indexer.java`.

use lucene_rs::analysis::WhitespaceAnalyzer;
use lucene_rs::index::OpenMode;
use lucene_rs::{Document, Field, FieldType, IndexOptions, IndexWriter, IndexWriterConfig};
use lucene_rs_bench::FIELD;
use std::io::BufRead;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(corpus), Some(dir)) = (args.get(1), args.get(2)) else {
        return Err("usage: index <corpus.txt> <index-dir> [--positions]".into());
    };
    let positions = args.iter().any(|a| a == "--positions");
    let options = if positions {
        IndexOptions::DocsAndFreqsAndPositions
    } else {
        IndexOptions::DocsAndFreqs
    };
    let field_type = FieldType::TEXT.with_index_options(options);
    let config = IndexWriterConfig::new(WhitespaceAnalyzer)
        .ram_buffer_size_mb(4096)
        .open_mode(OpenMode::Create);
    let t = Instant::now();
    let mut w = IndexWriter::open(dir, config)?;
    let mut docs = 0u64;
    for line in std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(corpus)?).lines() {
        w.add_document(&Document::new().with(Field::new(FIELD, line?, field_type)))?;
        docs = docs.saturating_add(1);
    }
    let added = t.elapsed().as_secs_f64();
    w.force_merge(1)?;
    w.close()?;
    println!(
        "{{\"engine\":\"rust\",\"docs\":{docs},\"positions\":{positions},\"index_secs\":{added:.2},\"total_secs\":{:.2}}}",
        t.elapsed().as_secs_f64()
    );
    Ok(())
}
