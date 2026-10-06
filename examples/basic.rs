//! Index a few documents and search them: `cargo run --example basic`.

use lucene_rs::{
    DirectoryReader, Document, Field, IndexSearcher, IndexWriter, IndexWriterConfig, QueryParser,
    StandardAnalyzer, Store,
};

fn main() -> lucene_rs::Result<()> {
    let dir = std::env::temp_dir().join("lucene-rs-example");
    let mut writer = IndexWriter::open(&dir, IndexWriterConfig::default())?;
    writer.delete_all();
    for (id, title, body) in [
        (
            "1",
            "Rust",
            "A language empowering everyone to build reliable and efficient software.",
        ),
        (
            "2",
            "Lucene",
            "A high-performance, full-featured search engine library written in Java.",
        ),
        (
            "3",
            "Search",
            "Full-text search with an inverted index, BM25 scoring and block-max pruning.",
        ),
    ] {
        writer.add_document(
            &Document::new()
                .with(Field::string("id", id, Store::Yes))
                .with(Field::text("title", title, Store::Yes))
                .with(Field::text("body", body, Store::No)),
        )?;
    }
    writer.commit()?;

    let searcher = IndexSearcher::new(DirectoryReader::open(&dir)?);
    let query = QueryParser::new("body", StandardAnalyzer::new())
        .parse("\"full-text search\" OR library")?;
    let top = searcher.search(&query, 10)?;
    println!("{} hits", top.total_hits.value);
    for hit in &top.score_docs {
        let doc = searcher.doc(hit.doc)?;
        println!(
            "{:.3}  {}  {}",
            hit.score,
            doc.get_str("id").unwrap_or("?"),
            doc.get_str("title").unwrap_or("?")
        );
    }
    Ok(())
}
