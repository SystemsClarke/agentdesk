//! Plain headless sessions (`ui:session_start` and friends): a command run in a folder on a pseudo-terminal the supervisor owns,
//! with no identity behind it. Like identities, the board holds what is *wanted* (`plain_sessions.state`) and the supervisor
//! makes it so; input and resizes go through `session_inbox`, which the supervisor drains.

use crate::db::{err, now_iso, Db};
use crate::sessions::Host;
use anyhow::Result;
use serde_json::{json, Value};

/// What a plain session must not inherit from whatever started the supervisor.
fn scrub() -> Vec<String> {
    std::env::vars().map(|(k, _)| k).filter(|k| k.starts_with("CLAUDE") || k == "AGENTDESK_SESSION" || k == "AGENTDESK_AUTHOR" || k == "AGENTDESK_IDENTITY").collect()
}

pub fn get(db: &Db, name: &str) -> Result<Option<Value>> {
    db.row("SELECT * FROM plain_sessions WHERE name=:n", &[(":n", &name)])
}

/// Ask for a session: the supervisor launches `command` (default `claude`) in `folder`.
pub fn request(db: &Db, name: &str, folder: &str, command: Option<&str>) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return err("name is required: letters, digits, - _ .");
    }
    if !std::path::Path::new(folder).is_dir() {
        return err(format!("no such folder: {folder}"));
    }
    if let Some(r) = get(db, name)? {
        if r["state"].as_str() != Some("stopped") {
            return err(format!("{name} is already running"));
        }
        db.exec("DELETE FROM plain_sessions WHERE name=:n", &[(":n", &name)])?;
    }
    db.exec(
        "INSERT INTO plain_sessions (name, folder, command, state, created_ts) VALUES (:n,:f,:c,'queued',:ts)",
        &[(":n", &name), (":f", &folder), (":c", &command), (":ts", &now_iso())],
    )?;
    Ok(())
}

pub fn list(db: &Db) -> Result<Vec<Value>> {
    Ok(db
        .rows("SELECT name, folder, command, pid, started_ts AS started FROM plain_sessions WHERE state='running' ORDER BY created_ts", &[])?
        .into_iter()
        .map(|mut r| {
            r["command"] = json!(r["command"].as_str().unwrap_or("claude"));
            r["viewers"] = json!(0);
            r
        })
        .collect())
}

pub fn stop(db: &Db, name: &str) -> Result<()> {
    if get(db, name)?.is_none() {
        return err(format!("no such session: {name}"));
    }
    db.exec("UPDATE plain_sessions SET state='stopped', pid=NULL WHERE name=:n", &[(":n", &name)])?;
    Ok(())
}

pub fn queue_input(db: &Db, name: &str, data: &str) -> Result<()> {
    db.exec("INSERT INTO session_inbox (name, kind, data, ts) VALUES (:n,'input',:d,:ts)", &[(":n", &name), (":d", &data), (":ts", &now_iso())])?;
    Ok(())
}

pub fn queue_resize(db: &Db, name: &str, cols: i64, rows: i64) -> Result<()> {
    db.exec("INSERT INTO session_inbox (name, kind, cols, rows, ts) VALUES (:n,'resize',:c,:r,:ts)", &[(":n", &name), (":c", &cols), (":r", &rows), (":ts", &now_iso())])?;
    Ok(())
}

fn split(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quote) = (String::new(), None::<char>);
    for c in command.chars() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (q, Some(open)) if q == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// One supervisor tick for plain sessions: launch what is queued, end what was stopped, mark what died, deliver input.
pub fn tick(db: &Db, host: &mut Host) -> Result<()> {
    for r in db.rows("SELECT * FROM plain_sessions", &[])? {
        let name = r["name"].as_str().unwrap_or("").to_string();
        let key = format!("plain:{name}");
        match r["state"].as_str().unwrap_or("") {
            "queued" => {
                let parts = split(r["command"].as_str().unwrap_or("claude"));
                let (program, args) = match parts.split_first() {
                    Some((p, a)) => (p.clone(), a.to_vec()),
                    None => ("claude".to_string(), vec![]),
                };
                let env = vec![("AGENTDESK_HEADLESS".to_string(), name.clone())];
                match host.launch_with(&key, r["folder"].as_str().unwrap_or("."), &program, &args, &env, &scrub()) {
                    Ok(pid) => {
                        db.exec("UPDATE plain_sessions SET state='running', pid=:p, started_ts=:ts WHERE name=:n", &[(":p", &(pid as i64)), (":ts", &now_iso()), (":n", &name)])?;
                    }
                    Err(e) => {
                        crate::paths::log("WARN", &format!("session {name} did not start: {e}"));
                        db.exec("UPDATE plain_sessions SET state='stopped' WHERE name=:n", &[(":n", &name)])?;
                    }
                }
            }
            "running" => {
                if !host.alive(&key) {
                    db.exec("UPDATE plain_sessions SET state='stopped', pid=NULL WHERE name=:n", &[(":n", &name)])?;
                }
            }
            _ => {
                if host.names().contains(&key) {
                    host.stop(&key);
                }
            }
        }
    }
    for m in db.rows("SELECT * FROM session_inbox ORDER BY id", &[])? {
        let n = m["name"].as_str().unwrap_or("");
        // a plain session, else an identity's session (the Slack bridge types into a goal's lead by its name)
        let key = if host.names().contains(&format!("plain:{n}")) { format!("plain:{n}") } else { n.to_string() };
        let id = m["id"].as_i64().unwrap_or(0);
        match m["kind"].as_str() {
            Some("input") => {
                let _ = host.input(&key, m["data"].as_str().unwrap_or("").as_bytes());
            }
            Some("resize") => {
                let _ = host.resize(&key, m["cols"].as_i64().unwrap_or(120) as u16, m["rows"].as_i64().unwrap_or(30) as u16);
            }
            _ => {}
        }
        db.exec("DELETE FROM session_inbox WHERE id=:i", &[(":i", &id)])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn commands_split_like_a_shell() {
        assert_eq!(super::split(r#"python3 -c "import time; time.sleep(3)""#), vec!["python3", "-c", "import time; time.sleep(3)"]);
        assert_eq!(super::split("claude"), vec!["claude"]);
    }
}
