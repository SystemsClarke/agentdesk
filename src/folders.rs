//! Where John works: the folders behind his identities, goals, sessions and Claude conversations (`ui:folders`), and the conversations
//! someone typed in recently that could become identities (`ui:adoptable`, `ui:adopt`).

use crate::db::Db;
use crate::identities as ids;
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

/// A recent conversation of Claude Code: its id, folder, when it last moved and the first thing someone typed.
pub struct Conversation {
    pub session_id: String,
    pub folder: String,
    pub modified: SystemTime,
    pub first_message: Option<String>,
}

fn root() -> PathBuf {
    std::env::var_os("AGENTDESK_CLAUDE_PROJECTS").map(PathBuf::from).unwrap_or_else(|| crate::paths::home_dir().join(".claude").join("projects"))
}

fn iso(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// What a person typed: a string message, or the text parts of a list, with system reminders cut out.
fn typed(msg: &Value) -> Option<String> {
    let raw = match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
        _ => return None,
    };
    let clean = Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").unwrap().replace_all(&raw, "").trim().to_string();
    (!clean.is_empty() && !clean.starts_with('<')).then_some(clean)
}

pub fn conversations() -> Vec<Conversation> {
    let mut out = vec![];
    for dir in std::fs::read_dir(root()).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(modified) = f.metadata().ok().and_then(|m| m.modified().ok()) else { continue };
            let (mut folder, mut first) = (None::<String>, None::<String>);
            for line in std::fs::read_to_string(&p).unwrap_or_default().lines().take(400) {
                let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
                if folder.is_none() {
                    folder = v["cwd"].as_str().map(String::from);
                }
                if first.is_none() && v["type"].as_str() == Some("user") {
                    first = typed(&v["message"]);
                }
                if folder.is_some() && first.is_some() {
                    break;
                }
            }
            if let Some(folder) = folder {
                out.push(Conversation { session_id: p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string(), folder, modified, first_message: first });
            }
        }
    }
    out
}

/// Conversations from the last 24 hours that someone typed in, newest first.
pub fn adoptable() -> Value {
    let cutoff = SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
    let mut c: Vec<Conversation> = conversations().into_iter().filter(|c| c.modified >= cutoff && c.first_message.is_some()).collect();
    c.sort_by_key(|c| std::cmp::Reverse(c.modified));
    json!({"sessions": c.iter().map(|c| json!({
        "session_id": c.session_id, "folder": c.folder, "last_activity": iso(c.modified),
        "first_message": c.first_message.as_deref().unwrap_or("").chars().take(200).collect::<String>(),
    })).collect::<Vec<_>>()})
}

/// Turns a conversation into an identity in its own folder and starts it.
pub fn adopt(db: &Db, session_id: &str, name: &str) -> Result<Value> {
    let Some(c) = conversations().into_iter().find(|c| c.session_id == session_id) else {
        return crate::db::err(format!("no such conversation: {session_id}"));
    };
    if !std::path::Path::new(&c.folder).is_dir() {
        return crate::db::err(format!("its folder is gone: {}", c.folder));
    }
    ids::create(db, name, &c.folder, None, None, false)?;
    db.exec("UPDATE identities SET claude_session_id=:s WHERE name=:n", &[(":s", &session_id), (":n", &name)])?;
    let mut row = ids::start(db, name)?;
    row["note"] = json!("Close that conversation in its other window: two sessions on one conversation would fight over it.");
    Ok(row)
}

/// The folders John works in, most used lately first: each use counts for half as much every 21 days.
pub fn ranked(db: &Db) -> Result<Value> {
    #[derive(Default)]
    struct F {
        who: Vec<String>,
        uses: i64,
        score: f64,
        last: String,
    }
    let mut by: BTreeMap<String, F> = BTreeMap::new();
    let now = chrono::Utc::now();
    let mut note = |folder: &str, who: &str, at: &str| {
        if folder.is_empty() || !std::path::Path::new(folder).is_dir() {
            return;
        }
        let age = chrono::DateTime::parse_from_rfc3339(at).map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 86400.0).unwrap_or(30.0).max(0.0);
        let f = by.entry(folder.to_string()).or_default();
        if !f.who.iter().any(|w| w == who) {
            f.who.push(who.to_string());
        }
        f.uses += 1;
        f.score += 0.5f64.powf(age / 21.0);
        if at > f.last.as_str() {
            f.last = at.to_string();
        }
    };
    for r in db.rows("SELECT name, folder, updated_ts FROM identities", &[])? {
        note(r["folder"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or("identity"), r["updated_ts"].as_str().unwrap_or(""));
    }
    for r in db.rows("SELECT name, measure_folder, updated_ts FROM goals", &[])? {
        note(r["measure_folder"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or("goal"), r["updated_ts"].as_str().unwrap_or(""));
    }
    for r in db.rows("SELECT author, cwd, seen_ts FROM sessions", &[])? {
        note(r["cwd"].as_str().unwrap_or(""), r["author"].as_str().unwrap_or("session"), r["seen_ts"].as_str().unwrap_or(""));
    }
    for c in conversations() {
        note(&c.folder, "claude", &iso(c.modified));
    }
    let mut v: Vec<(String, F)> = by.into_iter().collect();
    v.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then(a.0.cmp(&b.0)));
    Ok(json!({"folders": v.iter().map(|(p, f)| json!({"path": p, "who": f.who, "uses": f.uses, "last": f.last})).collect::<Vec<_>>()}))
}
