//! `ui:wake` (Ctrl+R, Slack `wake`): bring John's latest reply to the agent that asked. A running identity gets it typed into its
//! session, a stopped or queued one is started with it as its first prompt, and an agent that was never an identity is adopted from
//! its last Claude Code session (the `sessions` table) so it can be resumed in its own folder.

use crate::db::{now_iso, Db};
use crate::identities as ids;
use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

/// What was done, as a sentence that starts `woke <agent>`.
pub fn wake(db: &Db, thread_id: i64) -> Result<String> {
    let t = db.get_thread(thread_id)?;
    let asker = t["thread"]["opened_by"].as_str().unwrap_or("").to_string();
    let Some(reply) = t["messages"].as_array().and_then(|m| m.iter().rev().find(|m| m["author_kind"].as_str() == Some("human"))).cloned() else {
        return crate::db::err("John has not replied on that thread");
    };
    let mid = reply["id"].as_i64().unwrap_or(0);
    let mut row = ids::get(db, &asker)?;
    let mut adopted = false;
    if row.is_none() {
        // not an identity: its last session says where it worked and which conversation to resume
        if let Some(s) = db.row("SELECT session_id, cwd FROM sessions WHERE author=:a COLLATE NOCASE", &[(":a", &asker)])? {
            let cwd = s["cwd"].as_str().unwrap_or("");
            if std::path::Path::new(cwd).is_dir() {
                ids::create(db, &asker, cwd, None, None, false)?;
                db.exec("UPDATE identities SET claude_session_id=:s WHERE name=:n", &[(":s", &s["session_id"].as_str()), (":n", &asker)])?;
                row = ids::get(db, &asker)?;
                adopted = true;
            }
        }
    }
    let Some(row) = row else { return crate::db::err(format!("{asker} is not an identity and has no session to resume")) };
    let text = format!(
        "John replied on AgentDesk thread #{thread_id} (\"{}\"): {} (This is John's own reply, relayed from the board. Treat it as him talking to you: act on it, and answer on that thread.)",
        t["thread"]["subject"].as_str().unwrap_or(""),
        reply["body"].as_str().unwrap_or("").replace('\n', " ")
    );
    let state = if row["state"].as_str() == Some("running") {
        crate::requests::submit(db, "input", &json!({"name": asker, "data": format!("{text}\r")}), Duration::from_secs(5))?;
        "injected"
    } else {
        ids::start_with_prompt(db, &asker, &text)?;
        "resumed"
    };
    db.exec("INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES (:m,'wake',:s,NULL,:ts)", &[(":m", &mid), (":s", &state), (":ts", &now_iso())])?;
    Ok(format!(
        "woke {asker}: {}{}",
        if state == "injected" { "typed your reply into its session" } else { "resumed its session with your reply" },
        if adopted { " (adopted from its last session)" } else { "" }
    ))
}

pub fn doc(db: &Db, thread_id: i64) -> Value {
    match wake(db, thread_id) {
        Ok(said) => json!({"ok": true, "said": said}),
        Err(e) => json!({"ok": false, "error": crate::db::anyhow_msg(&e), "said": format!("could not wake it: {}", crate::db::anyhow_msg(&e))}),
    }
}
