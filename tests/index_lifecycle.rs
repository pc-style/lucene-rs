//! Index lifecycle: stored fields, updates and deletes across flushes and merges, commit
//! visibility, reopen, locking, rollback, corruption detection, and query parsing end to end.

#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use lucene_rs::analysis::{StandardAnalyzer, WhitespaceAnalyzer};
use lucene_rs::index::{LogMergePolicy, OpenMode};
use lucene_rs::search::TotalHitsRelation;
use lucene_rs::*;
use std::path::Path;

fn doc(id: &str, title: &str, body: &str) -> Document {
    Document::new()
        .with(Field::string("id", id, Store::Yes))
        .with(Field::text("title", title, Store::Yes))
        .with(Field::text("body", body, Store::No))
}

fn ids(searcher: &IndexSearcher, q: &Query) -> Vec<String> {
    let top = searcher.search(q, 100).unwrap();
    let mut v: Vec<String> = top
        .score_docs
        .iter()
        .map(|h| {
            searcher
                .doc(h.doc)
                .unwrap()
                .get_str("id")
                .unwrap()
                .to_string()
        })
        .collect();
    v.sort();
    v
}

fn open(dir: &Path) -> IndexSearcher {
    IndexSearcher::new(DirectoryReader::open(dir).unwrap())
}

#[test]
fn stored_fields_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    let d = Document::new()
        .with(Field::text("title", "Hello Wörld", Store::Yes))
        .with(Field::stored("blob", vec![0u8, 255, 7]))
        .with(Field::stored("n", -42i64))
        .with(Field::stored("x", 2.5f64))
        .with(Field::text("tag", "a", Store::Yes))
        .with(Field::text("tag", "b", Store::Yes));
    w.add_document(&d).unwrap();
    w.commit().unwrap();
    let s = open(dir.path());
    let got = s.doc(0).unwrap();
    assert_eq!(got.get_str("title"), Some("Hello Wörld"));
    assert_eq!(
        got.get("blob").and_then(FieldValue::as_bytes),
        Some(&[0u8, 255, 7][..])
    );
    assert_eq!(got.get("n").and_then(FieldValue::as_i64), Some(-42));
    assert_eq!(got.get("x").and_then(FieldValue::as_f64), Some(2.5));
    assert_eq!(
        got.get_all("tag")
            .filter_map(FieldValue::as_str)
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(
        s.search(&Query::term("title", "wörld"), 10)
            .unwrap()
            .score_docs
            .len(),
        1
    );
}

#[test]
fn updates_and_deletes_respect_order_across_flushes_and_merges() {
    let dir = tempfile::tempdir().unwrap();
    let config = IndexWriterConfig::default()
        .max_buffered_docs(3)
        .merge_policy(LogMergePolicy {
            merge_factor: 3,
            min_merge_docs: 1,
            max_merge_docs: u32::MAX,
        });
    let mut w = IndexWriter::open(dir.path(), config).unwrap();
    for i in 0..20 {
        w.add_document(&doc(&i.to_string(), "t", &format!("common n{i}")))
            .unwrap();
    }
    // delete then re-add: the re-added doc must survive (delete applies to earlier docs only)
    w.delete_documents(Term::new("id", "5"));
    w.add_document(&doc("5", "t", "common replaced")).unwrap();
    w.update_document(Term::new("id", "7"), &doc("7", "t", "common updated"))
        .unwrap();
    w.delete_documents(Term::new("body", "n3"));
    w.commit().unwrap();

    let s = open(dir.path());
    assert_eq!(s.reader().num_docs(), 19); // 20 - n3
    assert_eq!(ids(&s, &Query::term("id", "5")), ["5"]);
    assert_eq!(ids(&s, &Query::term("body", "replaced")), ["5"]);
    assert_eq!(ids(&s, &Query::term("body", "n5")), Vec::<String>::new());
    assert_eq!(ids(&s, &Query::term("body", "updated")), ["7"]);
    assert_eq!(ids(&s, &Query::term("body", "n3")), Vec::<String>::new());
    assert_eq!(s.count(&Query::term("body", "common")).unwrap(), 19);
    assert_eq!(s.count(&Query::MatchAll).unwrap(), 19);

    // force-merge expunges deletes and keeps results
    w.force_merge(1).unwrap();
    w.commit().unwrap();
    let s2 = open(dir.path());
    assert_eq!(s2.reader().segments().len(), 1);
    assert_eq!(s2.reader().max_doc(), 19);
    assert_eq!(
        ids(&s2, &Query::term("body", "common")),
        ids(&s, &Query::term("body", "common"))
    );
}

#[test]
fn commits_are_atomic_and_reopen_reuses_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    w.add_document(&doc("a", "first", "x")).unwrap();
    w.commit().unwrap();
    let r1 = DirectoryReader::open(dir.path()).unwrap();
    w.add_document(&doc("b", "second", "x")).unwrap();
    w.flush().unwrap(); // flushed but not committed: invisible
    assert!(r1.reopen().unwrap().is_none());
    assert_eq!(DirectoryReader::open(dir.path()).unwrap().num_docs(), 1);
    w.commit().unwrap();
    let r2 = r1.reopen().unwrap().unwrap();
    assert_eq!(r2.num_docs(), 2);
    assert!(
        std::sync::Arc::ptr_eq(&r1.segments()[0], &r2.segments()[0]),
        "unchanged segment reused"
    );
    assert_eq!(
        r1.num_docs(),
        1,
        "old reader is a stable point-in-time view"
    );
}

#[test]
fn write_lock_rollback_and_create_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    assert!(matches!(
        IndexWriter::open(dir.path(), IndexWriterConfig::default()),
        Err(Error::LockObtainFailed(_))
    ));
    w.add_document(&doc("a", "t", "x")).unwrap();
    w.commit().unwrap();
    w.add_document(&doc("b", "t", "x")).unwrap();
    drop(w); // no commit: "b" is discarded

    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    assert_eq!(w.num_docs(), 1);
    w.delete_all();
    w.add_document(&doc("c", "t", "x")).unwrap();
    w.close().unwrap();
    assert_eq!(ids(&open(dir.path()), &Query::MatchAll), ["c"]);

    let mut w = IndexWriter::open(
        dir.path(),
        IndexWriterConfig::default().open_mode(OpenMode::Create),
    )
    .unwrap();
    w.add_document(&doc("d", "t", "x")).unwrap();
    w.close().unwrap();
    assert_eq!(ids(&open(dir.path()), &Query::MatchAll), ["d"]);
    // only the files of the newest commit remain
    let files: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        files.iter().filter(|f| f.starts_with("segments_")).count(),
        1,
        "{files:?}"
    );
}

#[test]
fn inconsistent_fields_and_phrases_without_positions_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    w.add_document(&Document::new().with(Field::string("k", "v", Store::No)))
        .unwrap();
    let bad = Document::new().with(Field::text("k", "v w", Store::No));
    assert!(matches!(
        w.add_document(&bad),
        Err(Error::IllegalArgument(_))
    ));
    w.commit().unwrap();
    let s = open(dir.path());
    let phrase = PhraseQuery::new("k").term("v").term("w").build();
    assert!(matches!(
        s.search(&phrase, 10),
        Err(Error::IllegalArgument(_))
    ));
    assert!(matches!(
        DirectoryReader::open(tempfile::tempdir().unwrap().path()),
        Err(Error::IndexNotFound(_))
    ));
}

#[test]
fn corruption_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    for i in 0..50 {
        w.add_document(&doc(&i.to_string(), "t", "some text here"))
            .unwrap();
    }
    w.close().unwrap();
    let r = DirectoryReader::open(dir.path()).unwrap();
    r.check_integrity().unwrap();
    let name = r.segments()[0].name().to_string();
    drop(r);
    let path = dir.path().join(format!("{name}.doc"));
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x55;
    std::fs::write(&path, &bytes).unwrap();
    let r = DirectoryReader::open(dir.path()).unwrap();
    assert!(matches!(r.check_integrity(), Err(Error::Corrupt(_))));
    // a truncated file fails to open
    std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
    assert!(matches!(
        DirectoryReader::open(dir.path()),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn query_parser_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut w =
        IndexWriter::open(dir.path(), IndexWriterConfig::new(StandardAnalyzer::new())).unwrap();
    w.add_document(&doc(
        "1",
        "The Rust Programming Language",
        "systems programming without fear",
    ))
    .unwrap();
    w.add_document(&doc(
        "2",
        "Programming Pearls",
        "classic essays on programming",
    ))
    .unwrap();
    w.add_document(&doc("3", "Rust in Action", "systems programming in rust"))
        .unwrap();
    w.close().unwrap();
    let s = open(dir.path());
    let qp = QueryParser::new("body", StandardAnalyzer::new());
    let q = |text: &str| ids(&s, &qp.parse(text).unwrap());
    assert_eq!(q("programming"), ["1", "2", "3"]);
    assert_eq!(q("+programming -rust"), ["1", "2"]);
    assert_eq!(q("title:rust"), ["1", "3"]);
    assert_eq!(q("\"systems programming\""), ["1", "3"]);
    assert_eq!(q("\"programming systems\""), Vec::<String>::new());
    assert_eq!(q("title:(pearls OR action)"), ["2", "3"]);
    assert_eq!(q("essays AND classic"), ["2"]);
    let top = s.search(&qp.parse("rust^3 essays").unwrap(), 10).unwrap();
    assert_eq!(top.total_hits.relation, TotalHitsRelation::EqualTo);
    assert_eq!(
        s.doc(top.score_docs[0].doc).unwrap().get_str("id"),
        Some("3")
    );
}

#[test]
fn analyzers_apply_at_index_and_query_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::new(WhitespaceAnalyzer)).unwrap();
    w.add_document(&doc("1", "t", "Hello World")).unwrap();
    w.close().unwrap();
    let s = open(dir.path());
    assert_eq!(s.count(&Query::term("body", "Hello")).unwrap(), 1);
    assert_eq!(
        s.count(&Query::term("body", "hello")).unwrap(),
        0,
        "whitespace analyzer keeps case"
    );
}
