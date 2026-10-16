//! Permission rules for Claude Code, proposed by agents and approved by John. Nothing is written to
//! settings.json until John approves: an agent can propose, never approve. An approval backs the file up,
//! adds the rule to `permissions.allow` once and leaves every other key as it was.

use crate::db::{err, now_iso, Db, AGENT};
use crate::identity::{resolve, Caller};
use crate::paths;
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn is_broad(rule: &str) -> bool {
    Regex::new(r"^[A-Za-z][A-Za-z0-9_\-:.*]*(\(\s*:?\*?\s*\))?$").unwrap().is_match(rule)
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}...", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

fn settings_for(scope: &str, project_dir: Option<&str>) -> Result<PathBuf> {
    if scope == "user" {
        return Ok(paths::claude_settings());
    }
    match project_dir {
        Some(d) => Ok(PathBuf::from(d).join(".claude").join("settings.json")),
        None => err("a project rule needs a project_dir"),
    }
}

pub fn allowed(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["permissions"]["allow"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()))
        .unwrap_or_default()
}

pub fn propose(db: &Db, c: &Caller, a: &Value) -> Result<Value> {
    let rule = a["rule"].as_str().unwrap_or("").trim().to_string();
    if rule.is_empty() || rule.len() > 300 || !Regex::new(r"^[A-Za-z][A-Za-z0-9_\-:.*]*(\([^\r\n]+\))?$").unwrap().is_match(&rule) {
        return err("rule must be one Claude Code permission rule such as Bash(python tools/run.py:*) or mcp__server__tool, on one line");
    }
    let scope = a["scope"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("user").trim().to_lowercase();
    if scope != "user" && scope != "project" {
        return err("scope is user or project");
    }
    let reason = a["reason"].as_str().unwrap_or("").trim().to_string();
    if reason.is_empty() {
        return err("reason is required: what John asked for that the classifier blocked");
    }
    let dir = if scope == "project" { a["project_dir"].as_str().map(String::from).or_else(|| c.cwd.clone()) } else { None };
    if scope == "project" && !dir.as_deref().map(|d| std::path::Path::new(d).is_dir()).unwrap_or(false) {
        return err(format!("no such project_dir: {}", dir.unwrap_or_default()));
    }
    let who = resolve(a["author"].as_str(), c);
    let path = settings_for(&scope, dir.as_deref())?;
    if allowed(&path).contains(&rule) {
        return Ok(json!({"already_allowed": true, "rule": rule, "note": format!("{} already allows it: retry the action.", path.display())}));
    }
    if let Some(p) = db.row("SELECT id, thread_id FROM permission_proposals WHERE rule=:r AND scope=:s AND status='pending'", &[(":r", &rule), (":s", &scope)])? {
        return Ok(json!({"id": p["id"], "thread_id": p["thread_id"], "status": "pending", "note": "already proposed and waiting for John"}));
    }
    let blocked = a["blocked_action"].as_str().filter(|b| !b.trim().is_empty());
    let body = format!(
        "**Rule:** `{rule}` ({scope})\n**Asked by:** {who}\n**Why:** {}{}{}\n\nApprove it? In AgentDesk, Alt+A approves and Alt+D rejects, right here in this question; nothing is written until you do.",
        cut(&reason, 500),
        blocked.map(|b| format!("\n**Blocked action:** `{}`", cut(b, 300))).unwrap_or_default(),
        if is_broad(&rule) { "\n**Careful: this rule is broad, it allows every use of the tool.**" } else { "" }
    );
    let tid = db.start_thread("question", &format!("permission rule: {rule}"), &who, AGENT, &body, Some(&json!({"type": "permission"})), None)?;
    db.exec(
        "INSERT INTO permission_proposals (rule, scope, project_dir, reason, blocked, requested_by, thread_id, status, created_ts) VALUES (:r,:s,:d,:why,:b,:w,:t,'pending',:ts)",
        &[(":r", &rule), (":s", &scope), (":d", &dir), (":why", &reason), (":b", &blocked), (":w", &who), (":t", &tid), (":ts", &now_iso())],
    )?;
    let id = db.conn.last_insert_rowid();
    Ok(json!({"id": id, "thread_id": tid, "status": "pending",
        "note": "John has been asked. Nothing is written to settings.json until he approves in AgentDesk; carry on with other work and retry the action once his answer is on the thread."}))
}

pub fn list(db: &Db) -> Result<Value> {
    let mut rows = db.rows("SELECT * FROM permission_proposals ORDER BY id DESC LIMIT 200", &[])?;
    for r in rows.iter_mut() {
        r["broad"] = json!(is_broad(r["rule"].as_str().unwrap_or("")));
    }
    let settings = paths::claude_settings();
    Ok(json!({"proposals": rows, "allow": allowed(&settings), "settings": settings.to_string_lossy()}))
}

fn tell(db: &Db, p: &Value, result: &str, john_line: Option<&str>) -> Result<()> {
    if let Some(tid) = p["thread_id"].as_i64() {
        db.reply(tid, "AgentDesk", AGENT, result, None, None)?;
        if let Some(line) = john_line {
            db.john_replies(tid, line)?;
        }
    }
    Ok(())
}

fn need(db: &Db, id: i64) -> Result<Value> {
    match db.row("SELECT * FROM permission_proposals WHERE id=:i", &[(":i", &id)])? {
        Some(p) => Ok(p),
        None => err(format!("no such proposal: {id}")),
    }
}

/// Adds or removes one rule in `permissions.allow` after a backup copy, keeping every other key. A file that
/// is not valid JSON is refused untouched. Returns the backup's path.
fn edit(path: &PathBuf, rule: &str, add: bool) -> Result<Option<String>> {
    let mut backup = None;
    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        let v: Value = match serde_json::from_str(&text) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) => return err(format!("{} is not a JSON object: not touched. Fix it by hand, then approve again.", path.display())),
            Err(e) => return err(format!("{} is not valid JSON ({e}): not touched. Fix it by hand, then approve again.", path.display())),
        };
        let dir = paths::data_dir().join("permission_backups");
        std::fs::create_dir_all(&dir)?;
        let origin = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "settings".into());
        let b = dir.join(format!("{}-{origin}.json", chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")));
        std::fs::write(&b, &text)?;
        backup = Some(b.to_string_lossy().into_owned());
        v
    } else if !add {
        return Ok(None);
    } else {
        json!({})
    };
    let perms = root.as_object_mut().unwrap().entry("permissions").or_insert_with(|| json!({}));
    if !perms.is_object() {
        *perms = json!({});
    }
    let allow = perms.as_object_mut().unwrap().entry("allow").or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }
    let list = allow.as_array_mut().unwrap();
    if add && !list.iter().any(|x| x.as_str() == Some(rule)) {
        list.push(json!(rule));
    } else if !add {
        list.retain(|x| x.as_str() != Some(rule));
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&root)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(backup)
}

/// John approves (writes the rule) or rejects.
pub fn decide(db: &Db, id: i64, approve: bool) -> Result<Value> {
    let p = need(db, id)?;
    if p["status"].as_str() != Some("pending") {
        return err(format!("proposal {id} is already {}", p["status"].as_str().unwrap_or("")));
    }
    let rule = p["rule"].as_str().unwrap_or("").to_string();
    let path = settings_for(p["scope"].as_str().unwrap_or("user"), p["project_dir"].as_str())?;
    if approve {
        let backup = edit(&path, &rule, true)?;
        db.exec(
            "UPDATE permission_proposals SET status='approved', decided_ts=:ts, settings_path=:p, backup=:b WHERE id=:id",
            &[(":ts", &now_iso()), (":p", &path.to_string_lossy().into_owned()), (":b", &backup), (":id", &id)],
        )?;
        tell(db, &p, &format!("Approved in AgentDesk: `{rule}` is now in `permissions.allow` of {}. Retry the action.", path.display()), Some("Approved in AgentDesk."))?;
    } else {
        db.exec("UPDATE permission_proposals SET status='rejected', decided_ts=:ts WHERE id=:id", &[(":ts", &now_iso()), (":id", &id)])?;
        tell(db, &p, &format!("Rejected in AgentDesk: `{rule}` was not added. Do not retry the action; ask John another way."), Some("Rejected in AgentDesk."))?;
    }
    need(db, id)
}

/// Takes an approved rule back out of settings.json.
pub fn revert(db: &Db, id: i64) -> Result<Value> {
    let p = need(db, id)?;
    if p["status"].as_str() != Some("approved") {
        return err(format!("proposal {id} is {}, not approved: nothing to revert", p["status"].as_str().unwrap_or("")));
    }
    let rule = p["rule"].as_str().unwrap_or("").to_string();
    let path = PathBuf::from(p["settings_path"].as_str().unwrap_or(""));
    let backup = edit(&path, &rule, false)?;
    db.exec("UPDATE permission_proposals SET status='reverted', reverted_ts=:ts, backup=COALESCE(:b, backup) WHERE id=:id", &[(":ts", &now_iso()), (":b", &backup), (":id", &id)])?;
    tell(db, &p, &format!("Reverted in AgentDesk: `{rule}` was taken back out of {}.", path.display()), None)?;
    need(db, id)
}
