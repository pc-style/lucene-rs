#![allow(clippy::unwrap_used)]

use lucene_rs::{
    BooleanQuery, DirectoryReader, Document, Field, IndexSearcher, IndexWriter, IndexWriterConfig,
    MissingValue, Query, Sort, SortField, SortOrder, SortValue, Store, Term,
};
use std::ops::Bound::{Excluded, Included, Unbounded};

fn sort(field: &str, order: SortOrder, missing: MissingValue) -> Sort {
    Sort::Fields(vec![SortField {
        field: field.into(),
        order,
        missing,
    }])
}
fn ids(searcher: &IndexSearcher, query: &Query, sort: &Sort) -> Vec<String> {
    let mut cursor = None;
    let mut result = Vec::new();
    loop {
        let hits = searcher
            .search_after(query, sort, 2, cursor.as_ref())
            .unwrap();
        cursor = hits.last().cloned();
        for hit in hits {
            result.push(
                searcher
                    .doc(hit.doc)
                    .unwrap()
                    .get_str("id")
                    .unwrap()
                    .to_string(),
            );
        }
        if cursor.is_none() {
            break;
        }
    }
    result
}
fn doc(id: &str, n: Option<i64>) -> Document {
    let mut d = Document::new()
        .with(Field::string("id", id, Store::Yes))
        .with(Field::text("body", "rust search", Store::No));
    if let Some(n) = n {
        d.add(Field::numeric_i64("n", n, Store::No));
    }
    d
}

#[test]
fn numeric_ranges_sorting_and_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(
        dir.path(),
        IndexWriterConfig::default().max_buffered_docs(2),
    )
    .unwrap();
    for (id, n) in [
        ("a", Some(i64::MAX)),
        ("b", Some(-5)),
        ("c", None),
        ("d", Some(9_007_199_254_740_993)),
        ("e", Some(-5)),
        ("f", Some(i64::MIN)),
    ] {
        w.add_document(&doc(id, n)).unwrap();
    }
    w.commit().unwrap();
    let s = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    let asc = sort("n", SortOrder::Asc, MissingValue::Last);
    assert_eq!(
        ids(&s, &Query::MatchAll, &asc),
        ["f", "b", "e", "d", "a", "c"]
    );
    assert_eq!(
        ids(
            &s,
            &Query::MatchAll,
            &sort("n", SortOrder::Desc, MissingValue::First)
        ),
        ["c", "a", "d", "b", "e", "f"]
    );
    let range = Query::I64Range {
        field: "n".into(),
        lower: Excluded(-5),
        upper: Excluded(i64::MAX),
    };
    assert_eq!(ids(&s, &range, &asc), ["d"]);
    assert_eq!(
        s.count(&Query::I64Range {
            field: "n".into(),
            lower: Included(-5),
            upper: Included(-5)
        })
        .unwrap(),
        2
    );
    assert_eq!(
        s.count(&Query::I64Range {
            field: "n".into(),
            lower: Unbounded,
            upper: Unbounded
        })
        .unwrap(),
        5
    );
    let text = Query::term("body", "rust");
    let filtered = BooleanQuery::new().must(text.clone()).filter(range).build();
    let hit = s.search(&filtered, 10).unwrap().score_docs[0];
    assert_eq!(
        hit.score.to_bits(),
        s.search(&text, 10)
            .unwrap()
            .score_docs
            .iter()
            .find(|h| h.doc == hit.doc)
            .unwrap()
            .score
            .to_bits()
    );
    w.delete_documents(Term::new("id", "b"));
    w.update_document(Term::new("id", "d"), &doc("d", Some(0)))
        .unwrap();
    w.force_merge(1).unwrap();
    w.commit().unwrap();
    let fresh = IndexSearcher::new(s.reader().reopen().unwrap().unwrap());
    assert_eq!(
        ids(&fresh, &Query::MatchAll, &asc),
        ["f", "e", "d", "a", "c"]
    );
    assert_eq!(
        ids(&s, &Query::MatchAll, &asc),
        ["f", "b", "e", "d", "a", "c"]
    );
    fresh.reader().check_integrity().unwrap();
    drop(fresh);
    drop(s);
    w.close().unwrap();
    let reopened = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    assert_eq!(
        ids(&reopened, &Query::MatchAll, &asc),
        ["f", "e", "d", "a", "c"]
    );
}

#[test]
fn doubles_keywords_and_rejected_update() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    for (id, value, tag) in [("a", 1.25, "z"), ("b", -2.5, "a"), ("c", 1.25, "b")] {
        w.add_document(
            &doc(id, None)
                .with(Field::numeric_f64("price", value, Store::No))
                .with(Field::sorted_keyword("tag", tag, Store::No)),
        )
        .unwrap();
    }
    assert!(
        w.update_document(
            Term::new("id", "a"),
            &doc("a", None).with(Field::numeric_f64("price", f64::NAN, Store::No))
        )
        .is_err()
    );
    assert!(
        w.add_document(&doc("bad", None).with(Field::numeric_i64("price", 1, Store::No)))
            .is_err()
    );
    assert!(
        w.add_document(&doc("bad", Some(1)).with(Field::numeric_i64("n", 2, Store::No)))
            .is_err()
    );
    w.close().unwrap();
    let s = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    let sort = Sort::Fields(vec![
        SortField {
            field: "price".into(),
            order: SortOrder::Asc,
            missing: MissingValue::Last,
        },
        SortField {
            field: "tag".into(),
            order: SortOrder::Asc,
            missing: MissingValue::Last,
        },
    ]);
    assert_eq!(ids(&s, &Query::MatchAll, &sort), ["b", "c", "a"]);
    let q = Query::F64Range {
        field: "price".into(),
        lower: Excluded(-2.5),
        upper: Included(1.25),
    };
    assert_eq!(ids(&s, &q, &sort), ["c", "a"]);
    assert!(
        s.count(&Query::F64Range {
            field: "price".into(),
            lower: Included(f64::INFINITY),
            upper: Unbounded
        })
        .is_err()
    );
    assert!(
        s.count(&Query::I64Range {
            field: "price".into(),
            lower: Unbounded,
            upper: Unbounded
        })
        .is_err()
    );
    assert_eq!(
        s.search_after(&q, &sort, 1, None).unwrap()[0].sort_values,
        [
            Some(SortValue::F64(1.25)),
            Some(SortValue::Keyword("b".into()))
        ]
    );
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let boundary = lucene_rs::FieldDoc {
            doc: 0,
            score: 1.0,
            sort_values: vec![
                Some(SortValue::F64(value)),
                Some(SortValue::Keyword("b".into())),
            ],
        };
        assert!(s.search_after(&q, &sort, 1, Some(&boundary)).is_err());
    }
}

#[test]
fn pagination_beyond_hit_threshold_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = IndexWriter::open(
        dir.path(),
        IndexWriterConfig::default().max_buffered_docs(200),
    )
    .unwrap();
    for n in 0..1007 {
        w.add_document(&doc(&n.to_string(), Some(n % 7))).unwrap();
    }
    w.close().unwrap();
    let s = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    let q = Query::MatchAll;
    assert_eq!(s.count(&q).unwrap(), 1007);
    let mut after = None;
    let mut docs = Vec::new();
    loop {
        let hits = s
            .search_after(&q, &Sort::Score, 73, after.as_ref())
            .unwrap();
        after = hits.last().cloned();
        docs.extend(hits.into_iter().map(|h| h.doc));
        if after.is_none() {
            break;
        }
    }
    assert_eq!(docs, (0..1007).collect::<Vec<_>>());
}

#[test]
fn signed_zero_ranges_are_distinct_in_rewrites() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    for (id, value) in [("negative", -0.0), ("positive", 0.0), ("one", 1.0)] {
        writer
            .add_document(
                &Document::new()
                    .with(Field::string("id", id, Store::Yes))
                    .with(Field::numeric_f64("n", value, Store::No)),
            )
            .unwrap();
    }
    writer.close().unwrap();
    let searcher = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    let range = |value| Query::F64Range {
        field: "n".into(),
        lower: Included(value),
        upper: Unbounded,
    };
    let negative = range(-0.0);
    let positive = range(0.0);
    assert_ne!(negative, positive);
    let query = BooleanQuery::new().must(negative).must(positive).build();
    assert_eq!(ids(&searcher, &query, &Sort::Score), ["positive", "one"]);
}

#[test]
fn search_after_is_a_value_boundary_not_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
    for (id, n) in [("a", 7), ("b", -3), ("c", 7), ("d", 12)] {
        writer.add_document(&doc(id, Some(n))).unwrap();
    }
    writer.close().unwrap();
    let searcher = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    let order = sort("n", SortOrder::Asc, MissingValue::Last);
    let scored = BooleanQuery::new()
        .should(Query::term("id", "a").constant_score().boost(4.0))
        .should(Query::term("id", "b").constant_score().boost(2.0))
        .should(Query::term("id", "c").constant_score().boost(2.0))
        .should(Query::term("id", "d").constant_score())
        .build();
    let first_scores = searcher
        .search_after(&scored, &Sort::Score, 2, None)
        .unwrap();
    let next_scores = searcher
        .search_after(&scored, &Sort::Score, 2, first_scores.last())
        .unwrap();
    assert_eq!(
        first_scores
            .iter()
            .map(|h| (h.doc, h.score.to_bits()))
            .collect::<Vec<_>>(),
        [(0, 4.0f32.to_bits()), (1, 2.0f32.to_bits())]
    );
    assert_eq!(
        next_scores
            .iter()
            .map(|h| (h.doc, h.score.to_bits()))
            .collect::<Vec<_>>(),
        [(2, 2.0f32.to_bits()), (3, 1.0f32.to_bits())]
    );
    let first = searcher
        .search_after(&Query::MatchAll, &order, 2, None)
        .unwrap();
    assert_eq!(first.iter().map(|h| h.doc).collect::<Vec<_>>(), [1, 0]);
    // Reconstruct the boundary without any engine-owned state, including on an
    // independently opened reader of the same commit. Equal sort values use doc ID.
    let boundary = lucene_rs::FieldDoc {
        doc: 0,
        score: 1.0,
        sort_values: vec![Some(SortValue::I64(7))],
    };
    let reopened = IndexSearcher::new(DirectoryReader::open(dir.path()).unwrap());
    let rest = reopened
        .search_after(&Query::MatchAll, &order, 2, Some(&boundary))
        .unwrap();
    assert_eq!(rest.iter().map(|h| h.doc).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(
        reopened
            .search_after(&Query::MatchAll, &order, 2, rest.last())
            .unwrap(),
        Vec::<lucene_rs::FieldDoc>::new()
    );
    // Batch-size limits are application policy, not the collector's 10,000-hit cap.
    assert_eq!(
        searcher
            .search_after(&Query::MatchAll, &order, 10_001, None)
            .unwrap()
            .len(),
        4
    );
    assert!(
        searcher
            .search_after(&Query::MatchAll, &order, 0, None)
            .is_err()
    );
    for values in [
        vec![],
        vec![Some(SortValue::Keyword("wrong".into()))],
        vec![Some(SortValue::F64(f64::NAN))],
    ] {
        let invalid = lucene_rs::FieldDoc {
            sort_values: values,
            ..boundary.clone()
        };
        assert!(
            searcher
                .search_after(&Query::MatchAll, &order, 2, Some(&invalid))
                .is_err()
        );
    }
    assert!(
        searcher
            .search_after(&Query::MatchAll, &Sort::Score, 2, Some(&boundary))
            .is_err()
    );
    let invalid = lucene_rs::FieldDoc {
        score: f32::NAN,
        sort_values: vec![],
        ..boundary
    };
    assert!(
        searcher
            .search_after(&Query::MatchAll, &Sort::Score, 2, Some(&invalid))
            .is_err()
    );
}

#[test]
fn missing_or_corrupt_doc_values_fail_to_open() {
    for remove in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = IndexWriter::open(dir.path(), IndexWriterConfig::default()).unwrap();
        writer.add_document(&doc("a", Some(42))).unwrap();
        writer.close().unwrap();
        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "dvm"))
            .unwrap();
        if remove {
            std::fs::remove_file(path).unwrap();
        } else {
            std::fs::write(path, b"corrupt").unwrap();
        }
        assert!(DirectoryReader::open(dir.path()).is_err());
    }
}
