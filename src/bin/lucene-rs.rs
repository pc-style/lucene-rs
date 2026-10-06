//! `lucene-rs` command-line tool.
//!
//! ```text
//! lucene-rs index  <dir> <file.jsonl>... [--keyword FIELD]... [--create]
//! lucene-rs search <dir> <query> [-n N] [--field FIELD] [--and]
//! lucene-rs delete <dir> <field> <term>
//! lucene-rs merge  <dir> [--max-segments N]
//! lucene-rs stats  <dir>
//! lucene-rs check  <dir>
//! ```
//!
//! `index` reads JSON Lines: one object per document. String values become stored text fields
//! (or stored keyword fields with `--keyword`), numbers become stored numeric fields, arrays
//! become multi-valued fields.

use lucene_rs::index::OpenMode;
use lucene_rs::queryparser::Operator;
use lucene_rs::{
    DirectoryReader, Document, Field, FieldValue, IndexSearcher, IndexWriter, IndexWriterConfig,
    QueryParser, StandardAnalyzer, Store, Term,
};
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Instant;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "usage:
  lucene-rs index  <dir> <file.jsonl>... [--keyword FIELD]... [--create]
  lucene-rs search <dir> <query> [-n N] [--field FIELD] [--and]
  lucene-rs delete <dir> <field> <term>
  lucene-rs merge  <dir> [--max-segments N]
  lucene-rs stats  <dir>
  lucene-rs check  <dir>";

/// Positional arguments plus `--flag value` options (repeatable).
struct Args {
    positional: Vec<String>,
    options: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Args {
    fn parse(mut raw: impl Iterator<Item = String>, switches: &[&str]) -> Res<Self> {
        let mut a = Self {
            positional: Vec::new(),
            options: Vec::new(),
            switches: Vec::new(),
        };
        let it = &mut raw;
        while let Some(arg) = it.next() {
            if switches.contains(&arg.as_str()) {
                a.switches.push(arg);
            } else if arg.starts_with('-') && arg.len() > 1 {
                let v = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
                a.options.push((arg, v));
            } else {
                a.positional.push(arg);
            }
        }
        Ok(a)
    }
    fn get(&self, i: usize) -> Res<&str> {
        Ok(self.positional.get(i).map(String::as_str).ok_or(USAGE)?)
    }
    fn opt(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn opts<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.options
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }
}

fn json_to_doc(obj: &Map<String, Value>, keywords: &[&str]) -> Document {
    let mut doc = Document::new();
    let mut add = |name: &str, v: &Value| match v {
        Value::String(s) if keywords.contains(&name) => {
            doc.add(Field::string(name, s.clone(), Store::Yes));
        }
        Value::String(s) => {
            doc.add(Field::text(name, s.clone(), Store::Yes));
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                doc.add(Field::stored(name, i));
            } else if let Some(f) = n.as_f64() {
                doc.add(Field::stored(name, f));
            }
        }
        Value::Bool(b) => {
            doc.add(Field::string(name, b.to_string(), Store::Yes));
        }
        _ => {}
    };
    for (k, v) in obj {
        match v {
            Value::Array(items) => items.iter().for_each(|item| add(k, item)),
            v => add(k, v),
        }
    }
    doc
}

fn value_to_json(v: &FieldValue) -> Value {
    match v {
        FieldValue::Text(s) => json!(s),
        FieldValue::Bytes(b) => json!(b),
        FieldValue::I64(i) => json!(i),
        FieldValue::F64(f) => json!(f),
    }
}

fn index(a: &Args) -> Res<()> {
    let dir = a.get(1)?;
    let keywords: Vec<&str> = a.opts("--keyword").collect();
    let mode = if a.has("--create") {
        OpenMode::Create
    } else {
        OpenMode::CreateOrAppend
    };
    let mut w = IndexWriter::open(dir, IndexWriterConfig::default().open_mode(mode))?;
    let t = Instant::now();
    let mut n = 0u64;
    for file in a.positional.iter().skip(2) {
        let reader = std::io::BufReader::new(std::fs::File::open(file)?);
        for (lineno, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let obj: Map<String, Value> = serde_json::from_str(&line)
                .map_err(|e| format!("{file}:{}: {e}", lineno.saturating_add(1)))?;
            w.add_document(&json_to_doc(&obj, &keywords))?;
            n = n.saturating_add(1);
        }
    }
    w.close()?;
    eprintln!("indexed {n} documents in {:.2}s", t.elapsed().as_secs_f64());
    Ok(())
}

fn search(a: &Args) -> Res<()> {
    let searcher = IndexSearcher::new(DirectoryReader::open(a.get(1)?)?);
    let field = a.opt("--field").unwrap_or("body");
    let n: usize = a.opt("-n").map_or(Ok(10), str::parse)?;
    let op = if a.has("--and") {
        Operator::And
    } else {
        Operator::Or
    };
    let query = QueryParser::new(field, StandardAnalyzer::new())
        .default_operator(op)
        .parse(a.get(2)?)?;
    let t = Instant::now();
    let top = searcher.search(&query, n)?;
    let took_ms = t.elapsed().as_secs_f64() * 1e3;
    let mut out = std::io::stdout().lock();
    for h in &top.score_docs {
        let doc = searcher.doc(h.doc)?;
        let mut fields = Map::new();
        for f in doc.fields() {
            match fields.get_mut(&f.name) {
                Some(Value::Array(arr)) => arr.push(value_to_json(&f.value)),
                Some(existing) => *existing = json!([existing.clone(), value_to_json(&f.value)]),
                None => {
                    fields.insert(f.name.clone(), value_to_json(&f.value));
                }
            }
        }
        writeln!(
            out,
            "{}",
            json!({"doc": h.doc, "score": h.score, "fields": fields})
        )?;
    }
    let exact = top.total_hits.relation == lucene_rs::search::TotalHitsRelation::EqualTo;
    eprintln!(
        "{}{} hits in {took_ms:.2} ms",
        if exact { "" } else { ">=" },
        top.total_hits.value
    );
    Ok(())
}

fn delete(a: &Args) -> Res<()> {
    let mut w = IndexWriter::open(a.get(1)?, IndexWriterConfig::default())?;
    w.delete_documents(Term::new(a.get(2)?, a.get(3)?));
    w.close()?;
    Ok(())
}

fn merge(a: &Args) -> Res<()> {
    let max: usize = a.opt("--max-segments").map_or(Ok(1), str::parse)?;
    let mut w = IndexWriter::open(a.get(1)?, IndexWriterConfig::default())?;
    w.force_merge(max)?;
    w.close()?;
    Ok(())
}

fn stats(a: &Args) -> Res<()> {
    let r = DirectoryReader::open(a.get(1)?)?;
    let mut fields = Map::new();
    for seg in r.segments() {
        for f in seg.field_infos().iter() {
            let c = r.collection_stats(&f.name);
            fields.entry(f.name.clone()).or_insert_with(|| {
                json!({
                    "index_options": format!("{:?}", f.index_options),
                    "norms": f.has_norms,
                    "doc_count": c.map(|c| c.doc_count),
                    "sum_total_term_freq": c.map(|c| c.sum_total_term_freq),
                })
            });
        }
    }
    let segments: Vec<Value> = r
        .segments()
        .iter()
        .map(|s| json!({"name": s.name(), "max_doc": s.max_doc(), "num_docs": s.num_docs()}))
        .collect();
    let stats = json!({
        "generation": r.generation(),
        "max_doc": r.max_doc(),
        "num_docs": r.num_docs(),
        "deleted_docs": r.num_deleted_docs(),
        "segments": segments,
        "fields": fields,
    });
    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}

fn check(a: &Args) -> Res<()> {
    let r = DirectoryReader::open(a.get(1)?)?;
    r.check_integrity()?;
    println!(
        "OK: {} segments, {} documents, all checksums valid",
        r.segments().len(),
        r.num_docs()
    );
    Ok(())
}

fn run() -> Res<()> {
    let a = Args::parse(std::env::args().skip(1), &["--create", "--and"])?;
    match a.get(0)? {
        "index" => index(&a),
        "search" => search(&a),
        "delete" => delete(&a),
        "merge" => merge(&a),
        "stats" => stats(&a),
        "check" => check(&a),
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
