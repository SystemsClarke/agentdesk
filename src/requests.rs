//! Requests to whoever owns the session host. A plain session (`ui:session_start`) lives in the supervisor's process, and `agentdesk ui`
//! is another one, so the CLI writes a row here and the supervisor (a fast tick of the daemon or the window) does it and writes the
//! answer back. The original had one resident core with a named pipe; this is the same conversation through the board file.

use crate::db::{now_iso, py_dumps, Db};
use crate::sessions::Host;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Asks the supervisor to do `action` and waits (up to `wait`) for the answer. Errors say when nobody is there to do it.
pub fn submit(db: &Db, action: &str, args: &Value, wait: Duration) -> Result<Value> {
    if !crate::supervisor::running() {
        return crate::db::err("no supervisor is running: start `agentdesk daemon` (or the window)");
    }
    let id = db.insert("INSERT INTO session_requests (action, args, ts) VALUES (:a,:g,:ts)", &[(":a", &action), (":g", &py_dumps(args)), (":ts", &now_iso())])?;
    let start = Instant::now();
    while start.elapsed() < wait {
        if let Some(r) = db.row("SELECT state, result FROM session_requests WHERE id=:i", &[(":i", &id)])? {
            if r["state"].as_str() != Some("pending") {
                let v: Value = r["result"].as_str().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
                db.exec("DELETE FROM session_requests WHERE id=:i", &[(":i", &id)])?;
                return if r["state"].as_str() == Some("error") { crate::db::err(v["error"].as_str().unwrap_or("failed")) } else { Ok(v) };
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    db.exec("DELETE FROM session_requests WHERE id=:i", &[(":i", &id)])?;
    crate::db::err("the supervisor did not answer in time")
}

fn s<'a>(a: &'a Value, k: &str) -> &'a str {
    a[k].as_str().unwrap_or("")
}

fn run(host: &mut Host, action: &str, a: &Value) -> Result<Value> {
    let name = s(a, "name");
    match action {
        "start" => {
            let (folder, command) = (s(a, "folder"), s(a, "command"));
            if name.is_empty() || command.trim().is_empty() {
                return crate::db::err("name and command are required");
            }
            if !std::path::Path::new(folder).is_dir() {
                return crate::db::err(format!("no such folder: {folder}"));
            }
            let (program, args): (&str, Vec<String>) =
                if cfg!(windows) { ("cmd", vec!["/d".into(), "/s".into(), "/c".into(), command.into()]) } else { ("sh", vec!["-c".into(), command.into()]) };
            let pid = host.launch_clean(
                name,
                folder,
                program,
                &args,
                &[("AGENTDESK_HEADLESS".into(), name.into())],
                &["CLAUDE", "AGENTDESK_SESSION", "AGENTDESK_AUTHOR", "AGENTDESK_IDENTITY"],
            )?;
            host.mark_plain(name);
            Ok(json!({"name": name, "pid": pid}))
        }
        "stop" => {
            if !host.alive(name) {
                return crate::db::err(format!("{name} is not running"));
            }
            host.stop(name);
            Ok(json!({"stopped": name}))
        }
        "list" => Ok(json!({"sessions": host.info()})),
        "input" => {
            host.input(name, s(a, "data").as_bytes())?;
            Ok(json!({"ok": true}))
        }
        "resize" => {
            host.resize(name, a["cols"].as_u64().unwrap_or(160) as u16, a["rows"].as_u64().unwrap_or(40) as u16)?;
            Ok(json!({"ok": true}))
        }
        "attach" => {
            if !host.alive(name) {
                return crate::db::err(format!("{name} is not running"));
            }
            host.resize(name, a["cols"].as_u64().unwrap_or(160) as u16, a["rows"].as_u64().unwrap_or(40) as u16)?;
            Ok(json!({"attached": true, "replayed": host.replay_len(name)}))
        }
        other => crate::db::err(format!("unknown session request: {other}")),
    }
}

/// The supervisor's side: answers every pending request.
pub fn process(db: &Db, host: &mut Host) {
    let Ok(rows) = db.rows("SELECT id, action, args FROM session_requests WHERE state='pending' ORDER BY id", &[]) else { return };
    for r in rows {
        let args: Value = r["args"].as_str().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
        let (state, result) = match run(host, r["action"].as_str().unwrap_or(""), &args) {
            Ok(v) => ("done", v),
            Err(e) => ("error", json!({"error": crate::db::anyhow_msg(&e)})),
        };
        let _ = db.exec("UPDATE session_requests SET state=:s, result=:r WHERE id=:i", &[(":s", &state), (":r", &py_dumps(&result)), (":i", &r["id"].as_i64().unwrap_or(0))]);
    }
}
