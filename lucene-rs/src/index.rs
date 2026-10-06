//! Single-segment, single-field index: in-memory inversion, then one pass writing
//! postings (Lucene104 layout), a block-based term dictionary, and one norm byte per doc.

use crate::postings_writer::{PostingsWriter, TermMeta};
use crate::sim::int_to_byte4;
use crate::store::{In, Out};
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

const TERMS_PER_BLOCK: usize = 32;

#[derive(Default)]
struct TermPostings {
    last_doc: i32,
    doc_freq: u32,
    total_term_freq: u64,
    /// vInt(docDelta) vInt(freq) pairs
    buf: Vec<u8>,
}

pub struct IndexStats {
    pub max_doc: u32,
    pub num_terms: usize,
    pub sum_total_term_freq: u64,
    pub invert_secs: f64,
    pub write_secs: f64,
}

fn push_vint(buf: &mut Vec<u8>, mut v: u32) {
    while v & !0x7F != 0 {
        buf.push(((v & 0x7F) | 0x80) as u8);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn build_index(corpus: &Path, dir: &Path) -> std::io::Result<IndexStats> {
    let t0 = std::time::Instant::now();
    let mut ids: FxHashMap<Box<[u8]>, u32> = FxHashMap::default();
    let mut postings: Vec<TermPostings> = Vec::new();
    let mut norms: Vec<u8> = Vec::new();
    let doc_terms_cap = 512;
    let mut sum_ttf = 0u64;
    let mut reader = BufReader::with_capacity(1 << 20, File::open(corpus)?);
    let mut line = Vec::new();
    let mut doc: i32 = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let mut doc_terms: FxHashMap<&[u8], u32> =
            FxHashMap::with_capacity_and_hasher(doc_terms_cap, Default::default());
        let mut len = 0u32;
        for tok in line.split(|&b| b == b' ').filter(|t| !t.is_empty()) {
            *doc_terms.entry(tok).or_insert(0) += 1;
            len += 1;
        }
        for (&term, &freq) in doc_terms.iter() {
            let id = match ids.get(term) {
                Some(&id) => id,
                None => {
                    let id = postings.len() as u32;
                    ids.insert(term.into(), id);
                    postings.push(TermPostings { last_doc: -1, ..Default::default() });
                    id
                }
            };
            let p = &mut postings[id as usize];
            push_vint(&mut p.buf, (doc - p.last_doc) as u32);
            push_vint(&mut p.buf, freq);
            p.last_doc = doc;
            p.doc_freq += 1;
            p.total_term_freq += freq as u64;
        }
        sum_ttf += len as u64;
        norms.push(int_to_byte4(len));
        doc += 1;
    }
    let invert_secs = t0.elapsed().as_secs_f64();

    let t1 = std::time::Instant::now();
    let mut terms: Vec<(Box<[u8]>, u32)> = ids.into_iter().collect();
    terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut pw = PostingsWriter::new();
    let mut tim = Out::new();
    let mut index_terms: Vec<(Box<[u8]>, u64)> = Vec::new();
    let mut block: Vec<(&[u8], TermMeta)> = Vec::with_capacity(TERMS_PER_BLOCK);
    let flush_block = |block: &mut Vec<(&[u8], TermMeta)>, tim: &mut Out, index_terms: &mut Vec<(Box<[u8]>, u64)>| {
        index_terms.push((block[0].0.into(), tim.len() as u64));
        tim.write_vint(block.len() as u32);
        for (term, m) in block.iter() {
            tim.write_vint(term.len() as u32);
            tim.write_bytes(term);
            tim.write_vint(m.doc_freq);
            tim.write_vlong(m.total_term_freq - m.doc_freq as u64);
            if m.doc_freq == 1 {
                tim.write_vint(m.singleton_doc_id as u32);
            } else {
                tim.write_vlong(m.doc_start_fp);
            }
        }
        block.clear();
    };
    let mut sum_doc_freq = 0u64;
    for (term, id) in terms.iter() {
        let p = &postings[*id as usize];
        pw.start_term();
        let mut input = In::new(&p.buf, 0);
        let mut d: i32 = -1;
        while input.pos < p.buf.len() {
            d += input.read_vint() as i32;
            let f = input.read_vint();
            pw.start_doc(d, f, norms[d as usize]);
        }
        let meta = pw.finish_term(p.doc_freq, p.total_term_freq);
        sum_doc_freq += p.doc_freq as u64;
        block.push((term, meta));
        if block.len() == TERMS_PER_BLOCK {
            flush_block(&mut block, &mut tim, &mut index_terms);
        }
    }
    if !block.is_empty() {
        flush_block(&mut block, &mut tim, &mut index_terms);
    }
    let term_index_fp = tim.len() as u64;
    tim.write_vint(index_terms.len() as u32);
    for (t, fp) in &index_terms {
        tim.write_vint(t.len() as u32);
        tim.write_bytes(t);
        tim.write_vlong(*fp);
    }
    // padding so that fixed-width reads near the end never run off the mapping
    pw.doc_out.write_bytes(&[0u8; 64]);

    fs::create_dir_all(dir)?;
    fs::write(dir.join("postings.doc"), &pw.doc_out.buf)?;
    fs::write(dir.join("terms.tim"), &tim.buf)?;
    fs::write(dir.join("norms.nrm"), &norms)?;
    let mut meta = Out::new();
    meta.write_long(doc as u64); // maxDoc
    meta.write_long(doc as u64); // docCount (every doc has the field)
    meta.write_long(sum_ttf);
    meta.write_long(sum_doc_freq);
    meta.write_long(terms.len() as u64);
    meta.write_long(term_index_fp);
    fs::write(dir.join("meta.bin"), &meta.buf)?;
    Ok(IndexStats {
        max_doc: doc as u32,
        num_terms: terms.len(),
        sum_total_term_freq: sum_ttf,
        invert_secs,
        write_secs: t1.elapsed().as_secs_f64(),
    })
}

pub struct Index {
    pub doc: Mmap,
    tim: Mmap,
    pub norms: Mmap,
    pub max_doc: u32,
    pub doc_count: u64,
    pub sum_total_term_freq: u64,
    pub num_terms: u64,
    index_bytes: Vec<u8>,
    /// (offset into index_bytes, len, block fp)
    index_entries: Vec<(u32, u32, u64)>,
}

impl Index {
    pub fn open(dir: &Path) -> std::io::Result<Index> {
        let map = |name: &str| -> std::io::Result<Mmap> {
            let f = File::open(dir.join(name))?;
            unsafe { Mmap::map(&f) }
        };
        let meta = fs::read(dir.join("meta.bin"))?;
        let mut m = In::new(&meta, 0);
        let max_doc = m.read_long() as u32;
        let doc_count = m.read_long();
        let sum_total_term_freq = m.read_long();
        let _sum_doc_freq = m.read_long();
        let num_terms = m.read_long();
        let term_index_fp = m.read_long() as usize;
        let tim = map("terms.tim")?;
        let mut input = In::new(&tim, term_index_fp);
        let n = input.read_vint() as usize;
        let mut index_bytes = Vec::new();
        let mut index_entries = Vec::with_capacity(n);
        for _ in 0..n {
            let len = input.read_vint() as usize;
            let off = index_bytes.len();
            index_bytes.extend_from_slice(&tim[input.pos..input.pos + len]);
            input.pos += len;
            let fp = input.read_vlong();
            index_entries.push((off as u32, len as u32, fp));
        }
        Ok(Index {
            doc: map("postings.doc")?,
            tim,
            norms: map("norms.nrm")?,
            max_doc,
            doc_count,
            sum_total_term_freq,
            num_terms,
            index_bytes,
            index_entries,
        })
    }

    pub fn lookup(&self, term: &[u8]) -> Option<TermMeta> {
        let block = self.index_entries.partition_point(|&(off, len, _)| {
            &self.index_bytes[off as usize..(off + len) as usize] <= term
        });
        if block == 0 {
            return None;
        }
        let fp = self.index_entries[block - 1].2 as usize;
        let mut input = In::new(&self.tim, fp);
        let n = input.read_vint();
        for _ in 0..n {
            let len = input.read_vint() as usize;
            let t = &self.tim[input.pos..input.pos + len];
            input.pos += len;
            let doc_freq = input.read_vint();
            let total_term_freq = input.read_vlong() + doc_freq as u64;
            let (doc_start_fp, singleton_doc_id) = if doc_freq == 1 {
                (0, input.read_vint() as i32)
            } else {
                (input.read_vlong(), -1)
            };
            match t.cmp(term) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => {
                    return Some(TermMeta { doc_freq, total_term_freq, doc_start_fp, singleton_doc_id });
                }
                std::cmp::Ordering::Greater => return None,
            }
        }
        None
    }
}
