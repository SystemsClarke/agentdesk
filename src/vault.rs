//! The memory vault: durable knowledge as a folder of Markdown notes. Wiki posts are mirrored in, notes
//! are searched (natively, see `vault_search`), and settled question threads are transcribed.

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

/// Searches the notes (BM25 keyword ranking in `vault_search`, the Python half's embeddings when `vault_search` is `semantic`, or
/// `auto` and nothing matched); best matches first. `full` inlines the body of that many
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
    use crate::vault_search::{self as vs, Mode};
    let python = || crate::pyhost::call("vault.search", json!({"query": query, "k": k, "full": full.max(0)}));
    let mut semantic_error = None;
    if vs::mode() == Mode::Semantic {
        match python() {
            Ok(mut r) if r["hits"].is_array() => {
                r["mode"] = json!("semantic");
                return r;
            }
            Ok(r) => semantic_error = Some(r["error"].as_str().unwrap_or("the Python search returned no hits list").to_string()),
            Err(e) => semantic_error = Some(e.to_string()),
        }
    }
    let mut r = vs::search(query, k, full);
    if let Some(e) = semantic_error {
        r["semantic_error"] = json!(e); // asked for, not available: the keyword answer says so
    } else if vs::mode() == Mode::Auto && r["hits"].as_array().is_some_and(|h| h.is_empty()) && vs::semantic_available() {
        if let Ok(mut s) = python() {
            if s["hits"].as_array().is_some_and(|h| !h.is_empty()) {
                s["mode"] = json!("semantic");
                return s;
            }
        }
    }
    r
}
