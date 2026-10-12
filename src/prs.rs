//! The merge list: pull requests agents asked John to merge. A row leaves the list only when GitHub (through
//! `gh pr view`) says it merged or closed; a failed check leaves it open with `last_error`.

use crate::db::{now_iso, Db, AGENT};
use serde_json::{json, Value};
use std::process::Command;

/// Checks every open PR once. Returns how many settled.
pub fn check(db: &Db) -> usize {
    let open = db.rows("SELECT * FROM pull_requests WHERE state='open' ORDER BY id", &[]).unwrap_or_default();
    let mut settled = 0;
    for pr in open {
        let (id, url) = (pr["id"].as_i64().unwrap_or(0), pr["url"].as_str().unwrap_or("").to_string());
        let out = Command::new("gh").args(["pr", "view", &url, "--json", "state,title"]).output();
        let ts = now_iso();
        let parsed = match out {
            Ok(o) if o.status.success() => serde_json::from_slice::<Value>(&o.stdout).map_err(|e| e.to_string()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().chars().take(300).collect()),
            Err(e) => Err(format!("gh not available: {e}")),
        };
        match parsed {
            Err(e) => {
                let _ = db.exec("UPDATE pull_requests SET checked_ts=:ts, last_error=:e WHERE id=:id", &[(":ts", &ts), (":e", &e), (":id", &id)]);
            }
            Ok(v) => {
                let state = v["state"].as_str().unwrap_or("OPEN").to_lowercase();
                let title = v["title"].as_str().unwrap_or("").to_string();
                if !title.is_empty() {
                    let _ = db.exec("UPDATE pull_requests SET title=:t WHERE id=:id", &[(":t", &title), (":id", &id)]);
                }
                if state == "merged" || state == "closed" {
                    let _ =
                        db.exec("UPDATE pull_requests SET state=:s, checked_ts=:ts, settled_ts=:ts, last_error=NULL WHERE id=:id", &[(":s", &state), (":ts", &ts), (":id", &id)]);
                    if let Some(tid) = pr["thread_id"].as_i64() {
                        let kind = if state == "merged" { "pr-merged" } else { "pr-closed" };
                        let body = format!(
                            "{} {}#{} was {state}.",
                            if state == "merged" { "Merged:" } else { "Closed without merging:" },
                            pr["repo"].as_str().unwrap_or(""),
                            pr["number"]
                        );
                        let _ = db.reply(tid, "agentdesk", AGENT, &body, None, Some(&json!({"kind": kind})));
                    }
                    settled += 1;
                } else {
                    let _ = db.exec("UPDATE pull_requests SET checked_ts=:ts, last_error=NULL WHERE id=:id", &[(":ts", &ts), (":id", &id)]);
                }
            }
        }
    }
    settled
}
