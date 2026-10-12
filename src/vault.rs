//! The memory vault: durable knowledge as a folder of Markdown notes. Wiki posts are mirrored in, notes
//! are searched by the exact terms of the query, and settled question threads are transcribed.

use crate::paths;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn slug(subject: &str) -> String {
    let s: String = subject.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    s.chars().take(60).collect()
}

fn atomic_write(path: &Path, text: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The golden tests' fake vault: same answers and same failures as the recording.
    pub static FAKE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A wiki thread as a note in `agentdesk/wiki/`. Returns what the post_message result reports.
pub fn mirror_thread(db: &crate::db::Db, thread_id: i64) -> Value {
    #[cfg(test)]
    if FAKE.with(|f| f.get()) {
        let subject = db.scalar_str("SELECT subject FROM threads WHERE id=:t", &[(":t", &thread_id)]).ok().flatten().unwrap_or_default();
        if subject.contains("FAIL") {
            return json!({"thread_id": thread_id, "status": "error", "reason": format!("RuntimeError('vault down: {subject}')")});
        }
        return json!({"thread_id": thread_id, "status": "mirrored", "note": format!("notes/{subject}.md"), "reasons": []});
    }
    if let Ok(r) = crate::pyhost::call("vault.mirror_thread", json!({"thread_id": thread_id})) {
        return r; // the original's own vault module, with its Memory Protocol checks
    }
    match mirror(db, thread_id) {
        Ok(p) => {
            let note = p.strip_prefix(paths::vault_dir()).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            json!({"thread_id": thread_id, "status": "mirrored", "note": note, "reasons": []})
        }
        Err(e) => json!({"thread_id": thread_id, "status": "error", "reason": e.to_string()}),
    }
}

fn mirror(db: &crate::db::Db, thread_id: i64) -> Result<PathBuf> {
    let doc = db.get_thread(thread_id)?;
    let subject = doc["thread"]["subject"].as_str().unwrap_or("untitled");
    let path = paths::vault_dir().join("agentdesk").join("wiki").join(format!("{thread_id}-{}.md", slug(subject)));
    atomic_write(&path, &render(&doc))?;
    Ok(path)
}

/// One linked note per thread: `agentdesk/threads/thread-N.md`.
pub fn write_thread_note(db: &crate::db::Db, thread_id: i64) -> Result<PathBuf> {
    let doc = db.get_thread(thread_id)?;
    let path = paths::vault_dir().join("agentdesk").join("threads").join(format!("thread-{thread_id}.md"));
    atomic_write(&path, &render(&doc))?;
    Ok(path)
}

fn render(doc: &Value) -> String {
    let t = &doc["thread"];
    let mut out = format!(
        "---\nthread: {}\nchannel: {}\nstatus: {}\nopened_by: {}\ncreated: {}\n---\n\n# {}\n",
        t["id"],
        t["channel"].as_str().unwrap_or(""),
        t["status"].as_str().unwrap_or(""),
        t["opened_by"].as_str().unwrap_or(""),
        t["created_ts"].as_str().unwrap_or(""),
        t["subject"].as_str().unwrap_or("")
    );
    for m in doc["messages"].as_array().into_iter().flatten() {
        let kind = m["meta"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["kind"].as_str().map(String::from)).unwrap_or_default();
        if crate::db::RECEIPT_KINDS.contains(&kind.as_str()) {
            continue;
        }
        out.push_str(&format!("\n## {} ({})\n\n{}\n", m["author"].as_str().unwrap_or(""), m["ts"].as_str().unwrap_or(""), m["body"].as_str().unwrap_or("")));
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false) {
                continue;
            }
            walk(&p, out);
        } else if p.extension().map(|x| x == "md").unwrap_or(false) {
            out.push(p);
        }
    }
}

/// Matches the exact terms of the query against every note; best matches first. `full` inlines the body of that many
/// top hits (a negative count inlines all but that many from the end).
pub fn search(query: &str, k: usize, full: i64) -> Value {
    #[cfg(test)]
    if FAKE.with(|f| f.get()) {
        if query == "offline" {
            return json!({"error": "Ollama is not reachable"});
        }
        let scores = [1.0, 0.5, 0.3333];
        let mut hits: Vec<Value> =
            (0..k.min(3)).map(|i| json!({"path": format!("notes/{query}-{i}.md"), "score": scores[i], "type": "note", "summary": format!("hit {i} for {query} ✓")})).collect();
        let n = if full >= 0 { full as usize } else { (hits.len() as i64 + full).max(0) as usize };
        for h in hits.iter_mut().take(n) {
            h["body"] = json!(format!("body of {}", h["path"].as_str().unwrap_or("")));
        }
        return json!({"hits": hits});
    }
    if let Ok(r) = crate::pyhost::call("vault.search", json!({"query": query, "k": k, "full": full.max(0)})) {
        return r;
    }
    let root = paths::vault_dir();
    let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut hits: Vec<(usize, PathBuf, String)> = Vec::new();
    if !terms.is_empty() {
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            let lower = text.to_lowercase();
            let matched = terms.iter().filter(|t| lower.contains(t.as_str())).count();
            if matched == 0 {
                continue;
            }
            let score = matched * 1000 + terms.iter().map(|t| lower.matches(t.as_str()).count()).sum::<usize>().min(999);
            hits.push((score, f, text));
        }
    }
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    let top = hits.iter().map(|h| h.0).max().unwrap_or(1).max(1) as f64;
    let n = hits.len().min(k);
    let inline = if full >= 0 { full as usize } else { (n as i64 + full).max(0) as usize };
    let out: Vec<Value> = hits
        .into_iter()
        .take(k)
        .enumerate()
        .map(|(i, (score, f, text))| {
            let lower = text.to_lowercase();
            let at = terms.iter().filter_map(|t| lower.find(t.as_str())).min().unwrap_or(0);
            let start = text.char_indices().map(|(i, _)| i).find(|&i| i + 80 >= at).unwrap_or(0);
            let snippet: String = text[start..].chars().take(240).collect();
            let rel = f.strip_prefix(&root).unwrap_or(&f).to_string_lossy().replace('\\', "/");
            let mut o = json!({"path": rel, "score": (score as f64 / top * 10000.0).round() / 10000.0, "type": "note", "summary": snippet.replace('\n', " ")});
            if i < inline {
                o["body"] = json!(text);
            }
            o
        })
        .collect();
    json!({"hits": out})
}
