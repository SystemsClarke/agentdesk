//! Native search over the memory vault: a BM25 keyword ranker over the Markdown notes, no Python and no model.
//!
//! Every note is tokenised once into a weighted term-frequency table (frontmatter `title`, `tags` and `summary` and the file name count
//! several times a body word does, so a note *about* a thing beats one that merely mentions it). The tables live in one file in the data
//! folder (`vault_index.json`) and are rebuilt incrementally: a note is re-read only when its modified time or size changed, so a warm
//! search of ~1000 notes is a stat per note plus the ranking. Ranking is BM25 with a coverage factor (a note holding every query term
//! beats one holding a rare one), a bonus for an exact identifier or phrase found verbatim (hostnames, UUIDs and flags are split by the
//! tokeniser, so the verbatim check restores them), and a small one-hop boost for notes linked (`[[wikilink]]`, either direction) to a
//! strong match. The answer has the shape the Python search gave (`path`, `score`, `type`, `summary`, `body` for the top `full` hits) plus
//! `title`, `snippet` and `lines` (the 1-based line range the snippet came from).
//!
//! `settings.json`'s `"vault_search"` (or `AGENTDESK_VAULT_SEARCH`) picks the mode: `keyword` is this module alone, `semantic` asks the
//! Python half first (falling back here when it fails), and `auto` (the default) is this module first, then the Python half only when
//! nothing matched and the user already has its index (see `semantic_available`).

use crate::paths;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const VERSION: u64 = 1;
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// How many times a word in each frontmatter field counts, against 1 for a body word.
const W_TITLE: u32 = 5;
const W_TAGS: u32 = 3;
const W_SUMMARY: u32 = 3;
/// How much of the best linked neighbour's score a note gets.
const LINK_BOOST: f64 = 0.15;
/// How many top candidates are read back from disk for the verbatim-phrase bonus.
const PHRASE_CANDIDATES: usize = 40;
const SNIPPET_CHARS: usize = 240;

/// Folders under the vault that are not memory: the search index of the Python half, and the audit log.
const SKIP_TOP: [&str; 2] = ["_meta", "log"];

const STOP: &[&str] = &[
    "the", "and", "for", "are", "was", "with", "that", "this", "from", "has", "have", "not", "but", "you", "its", "into", "can", "will", "then", "than", "also", "been", "all",
    "any", "our", "out", "use", "used", "when", "what", "how", "why", "who", "does", "did", "which", "there", "their", "they", "them", "these", "those", "your", "his", "her",
    "she", "him", "is", "of", "to", "in", "on", "at", "by", "as", "it", "be", "or", "an", "if", "so", "we", "do", "no", "up",
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Auto,
    Keyword,
    Semantic,
}

pub fn parse_mode(s: &str) -> Mode {
    match s.trim().to_ascii_lowercase().as_str() {
        "keyword" => Mode::Keyword,
        "semantic" => Mode::Semantic,
        _ => Mode::Auto,
    }
}

/// `AGENTDESK_VAULT_SEARCH`, else settings.json's `vault_search`, else auto.
pub fn mode() -> Mode {
    if let Ok(v) = std::env::var("AGENTDESK_VAULT_SEARCH") {
        if !v.trim().is_empty() {
            return parse_mode(&v);
        }
    }
    paths::setting("vault_search").as_str().map(parse_mode).unwrap_or(Mode::Auto)
}

/// Whether the Python half has an embedding index to use: an interpreter, and the vectors it built (under the vault's `_meta`).
pub fn semantic_available() -> bool {
    paths::vault_dir().join("_meta").join("search-index").join("vault_vectors.npz").exists() && crate::pyhost::interpreter().is_some()
}

pub fn cache_path() -> PathBuf {
    paths::data_dir().join("vault_index.json")
}

#[derive(Clone, Default)]
struct Doc {
    path: String,
    mtime: u64,
    size: u64,
    title: String,
    ntype: String,
    summary: String,
    /// Lower-cased link targets (file names without the folder or `.md`).
    links: Vec<String>,
    len: u32,
    tf: HashMap<String, u32>,
}

/// What a refresh did, for the tests and the log.
#[derive(Debug, Default, PartialEq)]
pub struct Stats {
    pub reparsed: usize,
    pub reused: usize,
    pub removed: usize,
}

#[derive(Clone, Copy)]
pub struct Opts {
    pub link_boost: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { link_boost: true }
    }
}

fn stem(t: &str) -> String {
    if t.len() > 3 && t.ends_with('s') && !t.ends_with("ss") && !t.ends_with("us") && !t.ends_with("is") {
        t[..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// Lower-cased alphanumeric runs, plural `s` dropped, stop words and one-letter words left out.
fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= 2 && !STOP.contains(&w.as_str()))
        .map(|w| stem(&w))
        .collect()
}

/// The frontmatter lines, the body, and the number of lines before the body.
fn split_frontmatter(text: &str) -> (Vec<&str>, &str, usize) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(|l| l.trim() == "---") != Some(true) {
        return (Vec::new(), text, 0);
    }
    let mut fm = Vec::new();
    let mut used = text.split_inclusive('\n').next().map_or(0, str::len);
    for (n, l) in lines.enumerate() {
        used += l.len();
        if l.trim() == "---" {
            return (fm, &text[used..], n + 2);
        }
        fm.push(l.trim_end());
    }
    (Vec::new(), text, 0) // never closed: not frontmatter
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(|c| c == '"' || c == '\'')
}

fn parse_note(rel: &str, text: &str, mtime: u64, size: u64) -> Doc {
    let (fm, body, _) = split_frontmatter(text);
    let (mut title, mut ntype, mut summary, mut tags) = (String::new(), String::new(), String::new(), String::new());
    let mut i = 0;
    while i < fm.len() {
        let line = fm[i];
        i += 1;
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "title" => title = unquote(v).to_string(),
            "type" => ntype = unquote(v).to_string(),
            "summary" | "description" if summary.is_empty() => summary = unquote(v).to_string(),
            "tags" | "tag" => {
                tags.push(' ');
                tags.push_str(&v.replace(['[', ']', ',', '#', '"', '\''], " "));
                while i < fm.len() && fm[i].trim_start().starts_with('-') {
                    tags.push(' ');
                    tags.push_str(fm[i].trim_start().trim_start_matches('-').trim().trim_start_matches('#'));
                    i += 1;
                }
            }
            _ => {}
        }
    }
    let file_stem = rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".md");
    if title.is_empty() {
        title = body.lines().find_map(|l| l.strip_prefix("# ")).map(|t| t.trim().to_string()).unwrap_or_else(|| file_stem.to_string());
    }
    let mut tf: HashMap<String, u32> = HashMap::new();
    for (text, w) in [(title.as_str(), W_TITLE), (file_stem, W_TITLE), (tags.as_str(), W_TAGS), (summary.as_str(), W_SUMMARY), (body, 1)] {
        for t in tokens(text) {
            *tf.entry(t).or_insert(0) += w;
        }
    }
    let mut links: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(a) = rest.find("[[") {
        rest = &rest[a + 2..];
        let Some(b) = rest.find("]]") else { break };
        let inner = &rest[..b];
        rest = &rest[b + 2..];
        let target = inner.split('|').next().unwrap_or("").split('#').next().unwrap_or("").rsplit('/').next().unwrap_or("").trim().trim_end_matches(".md").to_lowercase();
        if !target.is_empty() && !links.contains(&target) {
            links.push(target);
        }
    }
    Doc { path: rel.to_string(), mtime, size, title, ntype: if ntype.is_empty() { "note".into() } else { ntype }, summary, links, len: tf.values().sum(), tf }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if name.starts_with('.') || (dir == root && SKIP_TOP.contains(&name.as_str())) {
                continue;
            }
            walk(root, &p, out);
        } else if ft.is_file() && name.ends_with(".md") {
            out.push(p);
        }
    }
}

fn doc_to_json(d: &Doc) -> Value {
    let mut tf = Map::new();
    for (k, v) in &d.tf {
        tf.insert(k.clone(), json!(v));
    }
    json!({"p": d.path, "m": d.mtime, "s": d.size, "t": d.title, "y": d.ntype, "u": d.summary, "l": d.links, "n": d.len, "f": tf})
}

fn doc_from_json(v: &Value) -> Option<Doc> {
    Some(Doc {
        path: v["p"].as_str()?.to_string(),
        mtime: v["m"].as_u64()?,
        size: v["s"].as_u64()?,
        title: v["t"].as_str()?.to_string(),
        ntype: v["y"].as_str()?.to_string(),
        summary: v["u"].as_str()?.to_string(),
        links: v["l"].as_array()?.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        len: v["n"].as_u64()? as u32,
        tf: v["f"].as_object()?.iter().filter_map(|(k, x)| x.as_u64().map(|n| (k.clone(), n as u32))).collect(),
    })
}

fn load_cache(cache: &Path, vault: &Path) -> HashMap<String, Doc> {
    let Some(v) = std::fs::read_to_string(cache).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return HashMap::new() };
    if v["version"].as_u64() != Some(VERSION) || v["root"].as_str() != Some(&vault.to_string_lossy()) {
        return HashMap::new();
    }
    v["docs"].as_array().into_iter().flatten().filter_map(doc_from_json).map(|d| (d.path.clone(), d)).collect()
}

fn save_cache(cache: &Path, vault: &Path, docs: &[Doc]) {
    let v = json!({"version": VERSION, "root": vault.to_string_lossy(), "docs": docs.iter().map(doc_to_json).collect::<Vec<_>>()});
    if let Some(d) = cache.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // Unique per write: two searches in the one process must not interleave in the same temp file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = cache.with_extension(format!("tmp{}-{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    if std::fs::write(&tmp, v.to_string()).is_ok() && std::fs::rename(&tmp, cache).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The index, brought up to date with the vault: unchanged notes (same modified time and size) are reused from the cache file.
fn refresh(vault: &Path, cache: &Path) -> (Vec<Doc>, Stats) {
    let mut old = load_cache(cache, vault);
    let had_cache = !old.is_empty();
    let mut files = Vec::new();
    walk(vault, vault, &mut files);
    files.sort();
    let mut stats = Stats::default();
    let mut docs = Vec::with_capacity(files.len());
    for f in files {
        let Ok(md) = std::fs::metadata(&f) else { continue };
        let rel = f.strip_prefix(vault).unwrap_or(&f).to_string_lossy().replace('\\', "/");
        let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
        if let Some(d) = old.remove(&rel).filter(|d| d.mtime == mtime && d.size == md.len()) {
            stats.reused += 1;
            docs.push(d);
            continue;
        }
        let Ok(bytes) = std::fs::read(&f) else { continue };
        stats.reparsed += 1;
        docs.push(parse_note(&rel, &String::from_utf8_lossy(&bytes), mtime, md.len()));
    }
    stats.removed = old.len();
    if stats.reparsed > 0 || stats.removed > 0 || (!had_cache && !docs.is_empty()) {
        save_cache(cache, vault, &docs);
    }
    (docs, stats)
}

/// Query words (stemmed, no stop words unless that leaves nothing), each once.
fn query_terms(query: &str) -> Vec<String> {
    let mut t = tokens(query);
    if t.is_empty() {
        t = query.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| stem(&w.to_lowercase())).collect();
    }
    let mut seen = HashSet::new();
    t.retain(|w| seen.insert(w.clone()));
    t
}

/// Strings that, found verbatim in a note, are strong evidence (an identifier the tokeniser split, or the whole phrase).
fn phrases(query: &str) -> Vec<String> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<String> = q.split_whitespace().filter(|c| c.chars().count() >= 4 && c.chars().any(|ch| !ch.is_alphanumeric())).map(String::from).collect();
    if q.split_whitespace().count() > 1 && !out.contains(&q) {
        out.push(q);
    }
    out
}

/// (document index, score) for every document that contains at least one query word, best first.
fn rank(docs: &[Doc], vault: &Path, query: &str, opts: Opts) -> Vec<(usize, f64)> {
    let terms = query_terms(query);
    if terms.is_empty() || docs.is_empty() {
        return Vec::new();
    }
    let n = docs.len() as f64;
    let avg = (docs.iter().map(|d| d.len as f64).sum::<f64>() / n).max(1.0);
    let idf: Vec<f64> = terms
        .iter()
        .map(|t| {
            let df = docs.iter().filter(|d| d.tf.contains_key(t)).count() as f64;
            (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    let mut scores = vec![0.0f64; docs.len()];
    for (i, d) in docs.iter().enumerate() {
        let norm = K1 * (1.0 - B + B * d.len as f64 / avg);
        let (mut s, mut hit) = (0.0, 0usize);
        for (t, w) in terms.iter().zip(&idf) {
            if let Some(&tf) = d.tf.get(t) {
                let tf = tf as f64;
                s += w * tf * (K1 + 1.0) / (tf + norm);
                hit += 1;
            }
        }
        if hit > 0 {
            scores[i] = s * (0.4 + 0.6 * hit as f64 / terms.len() as f64);
        }
    }
    // A verbatim identifier or phrase: read the best candidates back and look for it as written.
    let ph = phrases(query);
    if !ph.is_empty() {
        let mut order: Vec<usize> = (0..docs.len()).filter(|&i| scores[i] > 0.0).collect();
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(docs[a].path.cmp(&docs[b].path)));
        for &i in order.iter().take(PHRASE_CANDIDATES) {
            let Ok(text) = std::fs::read_to_string(vault.join(&docs[i].path)) else { continue };
            let lower = text.to_lowercase();
            for p in &ph {
                if lower.contains(p.as_str()) {
                    let weight: f64 = tokens(p).iter().filter_map(|t| terms.iter().position(|q| q == t)).map(|j| idf[j]).sum();
                    scores[i] += 0.5 * weight;
                }
            }
        }
    }
    if opts.link_boost {
        let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, d) in docs.iter().enumerate() {
            by_stem.entry(d.path.rsplit('/').next().unwrap_or(&d.path).trim_end_matches(".md").to_lowercase()).or_default().push(i);
        }
        let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); docs.len()];
        for (i, d) in docs.iter().enumerate() {
            for l in &d.links {
                for &j in by_stem.get(l).into_iter().flatten() {
                    if j != i {
                        neighbours[i].push(j);
                        neighbours[j].push(i);
                    }
                }
            }
        }
        let base = scores.clone();
        for i in 0..docs.len() {
            if base[i] > 0.0 {
                let best = neighbours[i].iter().map(|&j| base[j]).fold(0.0, f64::max);
                scores[i] += LINK_BOOST * best;
            }
        }
    }
    let mut out: Vec<(usize, f64)> = scores.iter().enumerate().filter(|(_, &s)| s > 0.0).map(|(i, &s)| (i, s)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(docs[a.0].path.cmp(&docs[b.0].path)));
    out
}

/// The best few lines of the note for the query, with their 1-based line range in the file.
fn snippet(text: &str, doc: &Doc, terms: &[String]) -> (String, [usize; 2]) {
    let (_, body, offset) = split_frontmatter(text);
    let lines: Vec<(usize, &str)> = body.lines().enumerate().map(|(i, l)| (offset + i + 1, l.trim())).collect();
    let mut best: Option<(usize, usize)> = None; // (index into lines, distinct terms)
    for (idx, (_, l)) in lines.iter().enumerate() {
        let have: HashSet<String> = tokens(l).into_iter().collect();
        let c = terms.iter().filter(|t| have.contains(*t)).count();
        if c > best.map_or(0, |b| b.1) {
            best = Some((idx, c));
        }
    }
    let from = match best {
        Some((i, _)) => i,
        None if !doc.summary.is_empty() => return (doc.summary.chars().take(SNIPPET_CHARS).collect(), [1, 1]),
        None => lines.iter().position(|(_, l)| !l.is_empty()).unwrap_or(0),
    };
    let mut parts = Vec::new();
    let mut last = lines.get(from).map_or(1, |l| l.0);
    for (n, l) in lines.iter().skip(from).take(3) {
        if !l.is_empty() {
            parts.push(*l);
            last = *n;
        }
    }
    let start = lines.get(from).map_or(1, |l| l.0);
    (parts.join(" ").chars().take(SNIPPET_CHARS).collect(), [start, last.max(start)])
}

/// The search itself, against an explicit vault and cache file: `{"hits": [...]}`, best first, scores scaled so the best is 1.0.
/// `full` inlines the body of that many top hits (a negative count: all but that many from the end).
pub fn search_in(vault: &Path, cache: &Path, query: &str, k: usize, full: i64, opts: Opts) -> Value {
    let (docs, _) = refresh(vault, cache);
    let ranked = rank(&docs, vault, query, opts);
    let terms = query_terms(query);
    let top = ranked.first().map_or(1.0, |r| r.1).max(f64::MIN_POSITIVE);
    let n = ranked.len().min(k);
    let inline = if full >= 0 { full as usize } else { (n as i64 + full).max(0) as usize };
    let hits: Vec<Value> = ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(rank_i, &(i, s))| {
            let d = &docs[i];
            let text = std::fs::read_to_string(vault.join(&d.path)).unwrap_or_default();
            let (snip, lines) = snippet(&text, d, &terms);
            let mut o = json!({
                "path": d.path, "title": d.title, "score": (s / top * 10000.0).round() / 10000.0, "type": d.ntype,
                "summary": if d.summary.is_empty() { "(no summary)" } else { d.summary.as_str() },
                "snippet": snip, "lines": lines,
            });
            if rank_i < inline {
                o["body"] = json!(text.trim());
            }
            o
        })
        .collect();
    json!({"hits": hits, "mode": "keyword"})
}

/// The live vault and the data folder's cache.
pub fn search(query: &str, k: usize, full: i64) -> Value {
    search_in(&paths::vault_dir(), &cache_path(), query, k, full, Opts::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_identities::Env;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("vault")
    }

    fn paths_of(v: &Value) -> Vec<String> {
        v["hits"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap().to_string()).collect()
    }

    fn run(cache: &Path, q: &str) -> Value {
        search_in(&fixture(), cache, q, 10, 0, Opts::default())
    }

    #[test]
    fn ranks_the_note_about_the_thing_first() {
        let d = tempfile::tempdir().unwrap();
        let r = run(&d.path().join("c.json"), "docker agent");
        let p = paths_of(&r);
        assert_eq!(p[0], "notes/gocd-agent-docker.md", "{p:?}");
        assert!(p.contains(&"notes/docker-cleanup.md".to_string()));
        let scores: Vec<f64> = r["hits"].as_array().unwrap().iter().map(|h| h["score"].as_f64().unwrap()).collect();
        assert_eq!(scores[0], 1.0);
        assert!(scores.windows(2).all(|w| w[0] >= w[1]), "best first: {scores:?}");
    }

    #[test]
    fn frontmatter_outweighs_body_mentions() {
        let d = tempfile::tempdir().unwrap();
        let p = paths_of(&run(&d.path().join("c.json"), "kubernetes"));
        assert_eq!(p[..2], ["notes/cluster-ops.md", "notes/random-journal.md"], "tags/summary/title beat three body mentions: {p:?}");
    }

    #[test]
    fn an_identifier_is_found_as_written() {
        let d = tempfile::tempdir().unwrap();
        let p = paths_of(&run(&d.path().join("c.json"), "build-01.example.local"));
        assert_eq!(p[0], "notes/host-layout.md", "{p:?}");
    }

    #[test]
    fn a_linked_note_is_boosted_one_hop() {
        let d = tempfile::tempdir().unwrap();
        let c = d.path().join("c.json");
        let with = paths_of(&run(&c, "sync"));
        let without = paths_of(&search_in(&fixture(), &c, "sync", 10, 0, Opts { link_boost: false }));
        assert_eq!(with[0], "notes/obsidian-sync.md");
        let pos = |v: &[String], n: &str| v.iter().position(|p| p == n).unwrap();
        assert!(pos(&with, "notes/vault-conflicts.md") < pos(&with, "notes/backup-rotation.md"), "linked first: {with:?}");
        assert!(pos(&without, "notes/backup-rotation.md") < pos(&without, "notes/vault-conflicts.md"), "the shorter note leads without the boost: {without:?}");
    }

    #[test]
    fn the_output_shape() {
        let d = tempfile::tempdir().unwrap();
        let r = search_in(&fixture(), &d.path().join("c.json"), "build-01.example.local", 5, 1, Opts::default());
        let h = &r["hits"][0];
        assert_eq!(
            (h["path"].as_str(), h["title"].as_str(), h["type"].as_str(), h["summary"].as_str()),
            (Some("notes/host-layout.md"), Some("Host layout"), Some("reference"), Some("where the agents run"))
        );
        assert!(h["score"].is_f64() && h["snippet"].as_str().unwrap().contains("build-01.example.local runs the agents"));
        let l = h["lines"].as_array().unwrap();
        assert_eq!((l.len(), l[0].as_u64()), (2, Some(7)), "the line in the file, frontmatter counted: {h}");
        assert!(h["body"].as_str().unwrap().contains("build-01.example.local runs the agents"));
        assert!(r["hits"][1].get("body").is_none(), "only `full` hits carry a body");
        assert_eq!(search_in(&fixture(), &d.path().join("c.json"), "quetzalcoatl", 5, 0, Opts::default())["hits"], json!([]));
        assert_eq!(search_in(&fixture(), &d.path().join("c.json"), "", 5, 0, Opts::default())["hits"], json!([]));
        let neg = search_in(&fixture(), &d.path().join("c.json"), "docker", 3, -1, Opts::default());
        let bodies = neg["hits"].as_array().unwrap().iter().filter(|h| h.get("body").is_some()).count();
        assert_eq!(bodies, neg["hits"].as_array().unwrap().len() - 1, "a negative full inlines all but that many from the end");
    }

    #[test]
    fn notes_outside_the_memory_are_not_searched() {
        let d = tempfile::tempdir().unwrap();
        let p = paths_of(&run(&d.path().join("c.json"), "xylophone"));
        assert!(p.is_empty(), "_meta, log and dot folders are skipped: {p:?}");
    }

    #[test]
    fn the_index_is_rebuilt_incrementally() {
        let d = tempfile::tempdir().unwrap();
        let vault = d.path().join("vault");
        let cache = d.path().join("data").join("idx.json");
        std::fs::create_dir_all(vault.join("notes")).unwrap();
        for f in std::fs::read_dir(fixture().join("notes")).unwrap().flatten() {
            std::fs::copy(f.path(), vault.join("notes").join(f.file_name())).unwrap();
        }
        let total = std::fs::read_dir(vault.join("notes")).unwrap().count();
        assert_eq!(refresh(&vault, &cache).1, Stats { reparsed: total, reused: 0, removed: 0 }, "first build reads everything");
        assert!(cache.exists());
        assert_eq!(refresh(&vault, &cache).1, Stats { reparsed: 0, reused: total, removed: 0 }, "nothing changed, nothing re-read");
        assert!(search_in(&vault, &cache, "zeppelin", 5, 0, Opts::default())["hits"].as_array().unwrap().is_empty());
        std::fs::write(vault.join("notes").join("new-note.md"), "---\nsummary: airships\n---\nthe zeppelin hangar is in building nine\n").unwrap();
        std::fs::write(vault.join("notes").join("random-journal.md"), "rewritten, and now about zeppelin too\n").unwrap();
        std::fs::remove_file(vault.join("notes").join("docker-cleanup.md")).unwrap();
        assert_eq!(refresh(&vault, &cache).1, Stats { reparsed: 2, reused: total - 2, removed: 1 });
        let p = paths_of(&search_in(&vault, &cache, "zeppelin", 5, 0, Opts::default()));
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p.contains(&"notes/new-note.md".to_string()) && p.contains(&"notes/random-journal.md".to_string()));
        assert!(!paths_of(&search_in(&vault, &cache, "docker", 20, 0, Opts::default())).contains(&"notes/docker-cleanup.md".to_string()), "a deleted note is gone");
        assert_eq!(refresh(&vault, &cache).1.reparsed, 0, "and the cache was saved");
        std::fs::write(&cache, "not json").unwrap();
        assert_eq!(refresh(&vault, &cache).1.reparsed, total, "a damaged cache is a rebuild, not a failure");
    }

    #[test]
    fn a_thousand_notes_index_once_and_reuse_after() {
        let d = tempfile::tempdir().unwrap();
        let vault = d.path().join("vault");
        std::fs::create_dir_all(vault.join("notes")).unwrap();
        for i in 0..1000 {
            let body: String = (0..120).map(|j| format!("word{} ", (i * 7 + j * 13) % 900)).collect();
            std::fs::write(vault.join("notes").join(format!("n{i}.md")), format!("---\nsummary: note {i}\ntags: [t{}]\n---\n{body}\nsee [[n{}]]\n", i % 10, (i + 1) % 1000))
                .unwrap();
        }
        let cache = d.path().join("idx.json");
        assert_eq!(refresh(&vault, &cache).1.reparsed, 1000);
        let t = std::time::Instant::now();
        let r = search_in(&vault, &cache, "word42 word77", 5, 0, Opts::default());
        assert!(!r["hits"].as_array().unwrap().is_empty());
        assert_eq!(refresh(&vault, &cache).1, Stats { reparsed: 0, reused: 1000, removed: 0 });
        assert!(t.elapsed().as_secs() < 10, "a warm search of 1000 notes: {:?}", t.elapsed());
    }

    #[test]
    fn the_mode_setting() {
        assert_eq!((parse_mode("keyword"), parse_mode("Semantic"), parse_mode("auto"), parse_mode("nonsense")), (Mode::Keyword, Mode::Semantic, Mode::Auto, Mode::Auto));
        let env = Env::new();
        std::env::remove_var("AGENTDESK_VAULT_SEARCH");
        assert_eq!(mode(), Mode::Auto, "the default");
        std::fs::create_dir_all(env.data()).unwrap();
        std::fs::write(env.data().join("settings.json"), r#"{"vault_search": "keyword"}"#).unwrap();
        assert_eq!(mode(), Mode::Keyword);
        std::env::set_var("AGENTDESK_VAULT_SEARCH", "semantic");
        assert_eq!(mode(), Mode::Semantic, "the environment wins");
        std::env::remove_var("AGENTDESK_VAULT_SEARCH");
    }

    #[test]
    fn the_tool_answers_natively_without_python() {
        let env = Env::new();
        std::env::set_var("AGENTDESK_VAULT", fixture());
        std::env::set_var("AGENTDESK_PYTHON_BIN", "definitely-not-python");
        std::env::set_var("AGENTDESK_VAULT_SEARCH", "semantic"); // asked for, impossible: the keyword answer, saying why
        let r = crate::vault::search("kubernetes", 3, 0);
        let cached = env.data().join("vault_index.json").exists();
        for k in ["AGENTDESK_VAULT", "AGENTDESK_PYTHON_BIN", "AGENTDESK_VAULT_SEARCH"] {
            std::env::remove_var(k);
        }
        assert_eq!(r["hits"][0]["path"], "notes/cluster-ops.md");
        assert_eq!(r["mode"], "keyword");
        assert!(r["semantic_error"].is_string(), "{r}");
        assert!(cached, "the index lives in the data folder");
    }
}
