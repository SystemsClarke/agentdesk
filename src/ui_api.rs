//! The window-to-core protocol of the original (`docs/ui-api.md`), as a command line: `agentdesk ui <request> [json]`.
//! There is no resident core here, so a request runs against the board file in this process. It exists so every `ui:*`
//! request of the original has one stable, scriptable entry point: the parity suite (`parity/`) calls each of them, and
//! so can any tool. A request this build does not implement answers `{"error": "not implemented: ..."}` and exits 2.

use crate::db::{anyhow_msg, Db, HUMAN, JOHN};
use crate::identity::Caller;
use crate::{jobs, paths, permissions, prs, tools, update, usage};
use anyhow::Result;
use serde_json::{json, Value};

fn int(a: &Value, k: &str) -> Result<i64> {
    match a.get(k).and_then(|v| v.as_i64()) {
        Some(n) => Ok(n),
        None => crate::db::err(format!("{k} is required")),
    }
}

fn text<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// A caller with no session id, no identity and not `claude-code`: the window, a plain terminal, Slack.
pub fn is_john(c: &Caller) -> bool {
    c.session_id.is_none() && c.harness.is_none() && std::env::var_os("AGENTDESK_IDENTITY").is_none() && c.env_author.is_none()
}

pub fn call(db: &Db, c: &Caller, req: &str, a: &Value) -> Result<Value> {
    let name = req.strip_prefix("ui:").unwrap_or(req);
    match name {
        "subscribe" => Ok(json!({"ok": true})),
        "thread" => db.get_thread(int(a, "thread_id")?),
        "reply" => {
            if text(a, "body").trim().is_empty() {
                return crate::db::err("body must not be blank");
            }
            Ok(json!({"ok": true, "message_id": db.john_replies(int(a, "thread_id")?, text(a, "body"))?}))
        }
        "close" => Ok(json!({"ok": true, "closed": db.close_question(int(a, "thread_id")?)?})),
        "unarchive" => Ok(json!({"ok": true, "unarchived": db.unarchive(int(a, "thread_id")?)?})),
        "post" => {
            if text(a, "body").trim().is_empty() {
                return crate::db::err("body must not be blank");
            }
            let subject = if text(a, "subject").trim().is_empty() { "(no subject)" } else { text(a, "subject") };
            Ok(json!({"ok": true, "thread_id": db.start_thread(text(a, "channel"), subject, JOHN, HUMAN, text(a, "body"), None, None)?}))
        }
        "threads" => {
            let limit = a.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
            let archived = a.get("include_archived").and_then(|v| v.as_bool()).unwrap_or(true);
            let ch = a.get("channel").and_then(|v| v.as_str());
            let st = a.get("status").and_then(|v| v.as_str());
            Ok(json!({"threads": db.list_threads(ch, st, limit, archived, false)?}))
        }
        "status" => {
            let prs: Vec<Value> = db.rows("SELECT * FROM pull_requests ORDER BY id DESC LIMIT 200", &[])?;
            Ok(json!({
                "slack": std::fs::read_to_string(paths::data_dir().join("slack_bridge.state")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()),
                "usage": usage::report(),
                "prs": prs,
                "goals": [],
            }))
        }
        "check_prs" => {
            prs::check(db);
            Ok(json!({"ok": true}))
        }
        "permission_list" => permissions::list(db),
        "permission_decide" | "permission_revert" => {
            if !is_john(c) {
                return crate::db::err("only John approves, rejects or reverts permission rules");
            }
            let id = int(a, "id")?;
            if name == "permission_revert" {
                permissions::revert(db, id)
            } else {
                permissions::decide(db, id, a.get("approve").and_then(|v| v.as_bool()).unwrap_or(false))
            }
        }
        "job_create" => {
            let f = |k: &str| a.get(k).and_then(|v| v.as_str());
            jobs::create(
                db,
                jobs::NewJob {
                    name: text(a, "name"),
                    folder: text(a, "folder"),
                    prompt: text(a, "prompt"),
                    at: f("at"),
                    days: f("days"),
                    every_minutes: a.get("every_minutes").and_then(|v| v.as_f64()),
                    model: f("model"),
                    catch_up_minutes: a.get("catch_up_minutes").and_then(|v| v.as_f64()),
                },
            )
        }
        "job_list" => Ok(json!({"jobs": jobs::list(db)?})),
        "job_run" => jobs::run_now(db, text(a, "name")),
        "job_enable" => jobs::enable(db, text(a, "name"), a.get("on").and_then(|v| v.as_bool()).unwrap_or(true)),
        "job_delete" => jobs::delete(db, text(a, "name")),
        "log_tail" => {
            let n = a.get("lines").and_then(|v| v.as_i64()).unwrap_or(100).clamp(1, 1000) as usize;
            let text = std::fs::read_to_string(paths::log_file()).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            Ok(json!({"lines": lines[lines.len().saturating_sub(n)..]}))
        }
        "web_url" => Ok(json!({"url": null})),
        "update" => {
            let (latest, newer) = update::check().unwrap_or_else(|_| (String::new(), false));
            Ok(json!({"installed": true, "current": update::CURRENT, "latest": latest, "pending": newer, "downloaded": false, "restarting": false}))
        }
        // the agents' own tools answer here too, with the same names and arguments as the MCP server
        t if tools::tool_names().contains(&t) => Ok(serde_json::from_str(&tools::call(db, c, t, a)).unwrap_or(Value::Null)),
        _ => crate::db::err(format!("not implemented: ui:{name}")),
    }
}

/// Runs one request and prints its document. Exit code: 0 ok, 1 an error document, 2 not implemented.
pub fn run(request: &str, args: Option<&str>) -> i32 {
    let a: Value = match args {
        Some(t) => match serde_json::from_str(t) {
            Ok(v) => v,
            Err(e) => {
                println!("{}", json!({"error": format!("arguments are not JSON: {e}")}));
                return 1;
            }
        },
        None => json!({}),
    };
    let db = match Db::open(&paths::db_path()).and_then(|d| d.init().map(|_| d)) {
        Ok(d) => d,
        Err(e) => {
            println!("{}", json!({"error": anyhow_msg(&e)}));
            return 1;
        }
    };
    match call(&db, &Caller::from_env(), request, &a) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            let msg = anyhow_msg(&e);
            println!("{}", json!({"error": msg}));
            if msg.starts_with("not implemented") {
                2
            } else {
                1
            }
        }
    }
}
