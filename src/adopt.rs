//! Taking over Claude Code conversations and finding the folders John works in: `ui:adoptable`, `ui:adopt`, `ui:folders`.

use crate::db::{err, Db};
use crate::identities as ids;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// `~/.claude/projects` (`AGENTDESK_CLAUDE_PROJECTS` overrides the root).
pub fn projects_root() -> PathBuf {
    std::env::var_os("AGENTDESK_CLAUDE_PROJECTS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude").join("projects"))
}

pub struct Convo {
    pub session_id: String,
    pub folder: String,
    pub modified: DateTime<Utc>,
    pub first_message: String,
}

/// Removes `<tag ...>...</tag>` blocks (system reminders and the like) and collapses whitespace.
fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let name: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
        let close = format!("</{name}>");
        match (name.is_empty(), after.find(&close)) {
            (false, Some(end)) => rest = &after[end + close.len()..],
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn user_text(v: &Value) -> Option<String> {
    if v["type"].as_str() != Some("user") && v["message"]["role"].as_str() != Some("user") {
        return None;
    }
    let c = &v["message"]["content"];
    let raw = match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter(|p| p["type"].as_str() == Some("text")).filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
        _ => return None,
    };
    Some(strip_tags(&raw)).filter(|t| !t.is_empty())
}

fn read(path: &Path) -> Option<Convo> {
    let modified: DateTime<Utc> = std::fs::metadata(path).ok()?.modified().ok()?.into();
    let text = std::fs::read_to_string(path).ok()?;
    let (mut folder, mut first) = (String::new(), None);
    for line in text.lines().take(400) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if folder.is_empty() {
            if let Some(c) = v["cwd"].as_str() {
                folder = c.to_string();
            }
        }
        if first.is_none() {
            first = user_text(&v);
        }
        if !folder.is_empty() && first.is_some() {
            break;
        }
    }
    let first = first?;
    Some(Convo { session_id: path.file_stem()?.to_string_lossy().into_owned(), folder, modified, first_message: first.chars().take(80).collect() })
}

fn transcripts() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(projects_root()).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            if f.path().extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(f.path());
            }
        }
    }
    out
}

/// Conversations written in the last 24 hours that someone typed in, newest first.
pub fn adoptable() -> Vec<Convo> {
    let cutoff = Utc::now() - chrono::Duration::hours(24);
    let mut v: Vec<Convo> = transcripts().iter().filter_map(|p| read(p)).filter(|c| c.modified >= cutoff).collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.modified));
    v
}

pub fn adoptable_json() -> Value {
    json!({"sessions": adoptable().iter().map(|c| json!({"session_id": c.session_id, "folder": c.folder, "last_activity": c.modified.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(), "first_message": c.first_message})).collect::<Vec<_>>()})
}

/// An identity in the conversation's own folder with that `claude_session_id`, started.
pub fn adopt(db: &Db, session_id: &str, name: &str) -> Result<Value> {
    let Some(c) = transcripts().iter().filter(|p| p.file_stem().map(|s| s == session_id).unwrap_or(false)).find_map(|p| read(p)) else {
        return err(format!("no such conversation: {session_id}"));
    };
    if c.folder.is_empty() {
        return err("that conversation records no folder");
    }
    ids::create(db, name, &c.folder, None, None, false)?;
    db.exec("UPDATE identities SET claude_session_id=:s WHERE name=:n", &[(":s", &session_id), (":n", &name)])?;
    let mut row = ids::start(db, name)?;
    row["note"] = json!("If this conversation is still open in the Claude desktop app, close it there: two programs writing one conversation will garble it.");
    Ok(row)
}

/// The folders John works in, most likely first: identities and goals (weight 3), agent sessions and Claude conversations of the
/// last 90 days (weight 1, newest 300), ranked by use that halves every 21 days. Folders that are gone and scratch folders are left out.
type Score = (f64, i64, DateTime<Utc>, Vec<String>);

pub fn folders(db: &Db) -> Result<Value> {
    use std::collections::HashMap;
    let now = Utc::now();
    let mut score: HashMap<String, Score> = HashMap::new();
    let mut add = |folder: &str, who: Option<&str>, weight: f64, when: DateTime<Utc>| {
        if folder.is_empty() {
            return;
        }
        let age_days = (now - when).num_seconds() as f64 / 86400.0;
        let e = score.entry(folder.to_string()).or_insert((0.0, 0, when, vec![]));
        e.0 += weight * 0.5f64.powf(age_days / 21.0);
        e.1 += weight as i64;
        e.2 = e.2.max(when);
        if let Some(w) = who {
            if !e.3.iter().any(|x| x == w) && e.3.len() < 3 {
                e.3.push(w.to_string());
            }
        }
    };
    let ts = |v: &Value| v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc)).unwrap_or(now);
    for r in db.rows("SELECT name, folder, updated_ts FROM identities", &[]).unwrap_or_default() {
        add(r["folder"].as_str().unwrap_or(""), r["name"].as_str(), 3.0, ts(&r["updated_ts"]));
    }
    for r in db.rows("SELECT name, measure_folder AS folder, updated_ts FROM goals", &[]).unwrap_or_default() {
        add(r["folder"].as_str().unwrap_or(""), r["name"].as_str(), 3.0, ts(&r["updated_ts"]));
    }
    for r in db.rows("SELECT author, cwd, seen_ts FROM sessions", &[]).unwrap_or_default() {
        add(r["cwd"].as_str().unwrap_or(""), r["author"].as_str(), 1.0, ts(&r["seen_ts"]));
    }
    let cutoff = now - chrono::Duration::days(90);
    let mut convos: Vec<(DateTime<Utc>, PathBuf)> = transcripts()
        .into_iter()
        .filter_map(|p| {
            let m: DateTime<Utc> = std::fs::metadata(&p).ok()?.modified().ok().map(|t: SystemTime| t.into())?;
            (m >= cutoff).then_some((m, p))
        })
        .collect();
    convos.sort_by_key(|c| std::cmp::Reverse(c.0));
    for (m, p) in convos.into_iter().take(300) {
        if let Some(c) =
            std::fs::read_to_string(&p).ok().and_then(|t| t.lines().take(50).find_map(|l| serde_json::from_str::<Value>(l).ok().and_then(|v| v["cwd"].as_str().map(String::from))))
        {
            add(&c, None, 1.0, m);
        }
    }
    let temp = std::env::temp_dir().to_string_lossy().to_lowercase();
    let mut out: Vec<(String, Score)> =
        score.into_iter().filter(|(f, _)| Path::new(f).is_dir() && !f.to_lowercase().starts_with(&temp) && !f.to_lowercase().contains("appdata")).collect();
    out.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(
        json!({"folders": out.into_iter().take(30).map(|(f, e)| json!({"path": f, "who": e.3, "uses": e.1, "last": e.2.format("%Y-%m-%dT%H:%M:%SZ").to_string()})).collect::<Vec<_>>()}),
    )
}
