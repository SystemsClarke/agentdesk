//! The window-to-core protocol of the original (`docs/ui-api.md`), as a command line: `agentdesk ui <request> [json]`.
//! There is no resident core here, so a request runs against the board file in this process. It exists so every `ui:*`
//! request of the original has one stable, scriptable entry point: the parity suite (`parity/`) calls each of them, and
//! so can any tool. A request this build does not implement answers `{"error": "not implemented: ..."}` and exits 2.

use crate::db::{anyhow_msg, Db, HUMAN, JOHN};
use crate::identity::Caller;
use crate::{adopt, concierge, goals, governor, identities as ids, jobs, paths, permissions, plain, prs, slots, supervisor, tools, update, usage};
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
    goals::is_john(c) && c.session_id.is_none() && c.harness.is_none() && std::env::var_os("AGENTDESK_IDENTITY").is_none() && c.env_author.is_none()
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
            let now = chrono::Utc::now();
            let gov = governor::report(db, now, false, None);
            let v = governor::judge(db, now, false);
            let pool = governor::Budget::read(db, ids::max_sessions() as i64, &v).report();
            Ok(json!({
                "slack": std::fs::read_to_string(paths::data_dir().join("slack_bridge.state")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()),
                "concierge": concierge::state(db)?,
                "bridge": crate::bridge::status(),
                "sessions": pool,
                "usage": usage::report(),
                "governor": gov,
                "prs": prs,
                "goals": goals::list(db)?["goals"].clone(),
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
        "web_url" => Ok(json!({"url": crate::web::url()})),
        "update" => {
            let (latest, newer) = update::check().unwrap_or_else(|_| (String::new(), false));
            Ok(json!({"installed": true, "current": update::CURRENT, "latest": latest, "pending": newer, "downloaded": false, "restarting": false}))
        }
        "governor" => Ok(governor::report(db, chrono::Utc::now(), false, Some(&json!({"would_queue": 0, "would_shed": 0, "held": []})))),
        "governor_enforce" => {
            if !is_john(c) {
                return crate::db::err("only John turns enforcement on or off");
            }
            governor::set_enforce(a.get("on").and_then(|v| v.as_bool()).unwrap_or(false))?;
            Ok(governor::report(db, chrono::Utc::now(), false, Some(&json!({"would_queue": 0, "would_shed": 0, "held": []}))))
        }
        "concierge" => {
            let folder = std::env::var("AGENTDESK_CONCIERGE_FOLDER")
                .ok()
                .filter(|f| !f.is_empty())
                .unwrap_or_else(|| std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
            concierge::toggle(db, c, &folder, a.get("on").and_then(|v| v.as_bool()))
        }
        "identity_create" => {
            let f = |k: &str| a.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            ids::create(db, text(a, "name"), text(a, "folder"), f("charter"), f("model"), a.get("autostart").and_then(|v| v.as_bool()).unwrap_or(false))
        }
        "identity_list" => Ok(json!({"identities": ids::list(db)?})),
        "identity_start" => {
            let row = ids::start(db, text(a, "name"))?;
            // the supervisor starts it within a tick when the pool has a slot; say what happened, not what was asked
            let full = governor::Budget::read(db, ids::max_sessions() as i64, &governor::judge(db, chrono::Utc::now(), false)).running >= ids::max_sessions() as i64;
            if supervisor::running() && !full && row["state"].as_str() == Some("queued") {
                for _ in 0..40 {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    if ids::get(db, text(a, "name"))?.map(|r| r["state"].as_str() != Some("queued")).unwrap_or(true) {
                        break;
                    }
                }
            }
            ids::get(db, text(a, "name"))?.ok_or_else(|| anyhow::anyhow!("no such identity"))
        }
        "identity_stop" => {
            if ids::get(db, text(a, "name"))?.is_none() && plain::get(db, text(a, "name"))?.is_some() {
                plain::stop(db, text(a, "name"))?;
                return Ok(json!({"stopped": text(a, "name")}));
            }
            ids::stop(db, text(a, "name"))
        }
        "identity_forget" => ids::forget(db, text(a, "name")),
        "adoptable" => Ok(adopt::adoptable_json()),
        "adopt" => adopt::adopt(db, text(a, "session_id"), text(a, "name")),
        "folders" => adopt::folders(db),
        "wake" => wake(db, int(a, "thread_id")?),
        "goal_create" => {
            let name = goals::slug(text(a, "name"));
            goals::create(db, &name, text(a, "objective"), text(a, "folder"))
        }
        "goal_propose" => {
            goals::propose(db, c, text(a, "name"), text(a, "hypothesis"), text(a, "measure_cmd"), text(a, "success"), a.get("samples").and_then(|v| v.as_i64()).unwrap_or(1))
        }
        "goal_approve" | "goal_budget" => {
            let (m, h, k) = (a.get("max_members").and_then(|v| v.as_i64()), a.get("max_hours").and_then(|v| v.as_f64()), a.get("cadence_minutes").and_then(|v| v.as_f64()));
            if name == "goal_approve" {
                goals::approve(db, c, text(a, "name"), m, h, k)
            } else {
                goals::set_budget(db, c, text(a, "name"), m, h, k)
            }
        }
        "goal_stop" => goals::stop(db, c, text(a, "name")),
        "goal_list" => goals::list(db),
        "goal_status" => goals::status(db, text(a, "name")),
        "slot_list" => slots::list(db),
        "slot_assign" => slots::assign(db, int(a, "n")?, a),
        "slot_clear" => slots::clear(db, int(a, "n")?),
        "dictate" => {
            let action = text(a, "action");
            if !["start", "poll", "stop", "arm", "disarm"].contains(&action) {
                return crate::db::err("action is start, poll, stop, arm or disarm");
            }
            // a missing microphone or a plugin failure comes back as state done with error, never as a crash
            Ok(crate::dictation::call(action).unwrap_or_else(|e| json!({"state": "done", "text": "", "error": e.to_string(), "progress": 0.0})))
        }
        "session_start" | "session_stop" | "session_list" | "input" | "resize" | "attach" => sessions_api(db, name, a),
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

/// Plain sessions live in the supervisor (the process holding `supervisor.lock`); a request is written to the board and the
/// supervisor acts on it within a tick, so these wait briefly for it.
fn sessions_api(db: &Db, name: &str, a: &Value) -> Result<Value> {
    use std::time::{Duration, Instant};
    let wait = |ok: &dyn Fn() -> bool| {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && !ok() {
            std::thread::sleep(Duration::from_millis(150));
        }
    };
    let n = text(a, "name");
    if matches!(name, "session_start" | "session_stop" | "input" | "resize" | "attach") && !supervisor::running() {
        return crate::db::err(crate::attach::NOT_RUNNING);
    }
    match name {
        "session_list" => Ok(json!({"sessions": plain::list(db)?})),
        "session_start" => {
            plain::request(db, n, text(a, "folder"), a.get("command").and_then(|v| v.as_str()).filter(|c| !c.is_empty()))?;
            wait(&|| plain::get(db, n).ok().flatten().map(|r| r["state"].as_str() != Some("queued")).unwrap_or(true));
            match plain::get(db, n)? {
                Some(r) if r["state"].as_str() == Some("running") => Ok(json!({"name": n, "pid": r["pid"]})),
                _ => crate::db::err(format!("{n} did not start: see core.log")),
            }
        }
        "session_stop" => {
            plain::stop(db, n)?;
            Ok(json!({"stopped": n}))
        }
        "input" => {
            plain::queue_input(db, n, text(a, "data"))?;
            wait(&|| db.scalar_i64("SELECT COUNT(*) FROM session_inbox WHERE name=:n", &[(":n", &n)]).ok().flatten().unwrap_or(0) == 0);
            Ok(json!({"ok": true}))
        }
        "resize" => {
            plain::queue_resize(db, n, a.get("cols").and_then(|v| v.as_i64()).unwrap_or(120), a.get("rows").and_then(|v| v.as_i64()).unwrap_or(30))?;
            wait(&|| db.scalar_i64("SELECT COUNT(*) FROM session_inbox WHERE name=:n", &[(":n", &n)]).ok().flatten().unwrap_or(0) == 0);
            Ok(json!({"ok": true}))
        }
        _ => attach_probe(n, a),
    }
}

/// `ui:attach` over the supervisor's TCP endpoint: makes sure the session exists and reports whether output was replayed.
fn attach_probe(n: &str, a: &Value) -> Result<Value> {
    use std::io::{BufRead, BufReader, Read, Write};
    let Some((port, key)) = crate::attach::read_addr() else { return crate::db::err("The core has no attach endpoint: restart it with `agentdesk core restart`.") };
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
    conn.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    let (cols, rows) = (a.get("cols").and_then(|v| v.as_i64()).unwrap_or(120), a.get("rows").and_then(|v| v.as_i64()).unwrap_or(30));
    for target in [n.to_string(), format!("plain:{n}")] {
        let mut c = conn.try_clone()?;
        writeln!(c, "ATTACH {key} {target} {cols} {rows}")?;
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim() == "OK" {
            conn.set_read_timeout(Some(std::time::Duration::from_millis(400)))?;
            let mut buf = [0u8; 64];
            let replayed = reader.read(&mut buf).map(|k| k > 0).unwrap_or(false);
            let _ = writeln!(c, "DETACH");
            return Ok(json!({"attached": n, "replayed": replayed}));
        }
        conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
        conn.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    }
    crate::db::err(format!("no such session: {n}"))
}

/// `ui:wake`: John's latest reply on a question goes to the agent that asked it (never automatically beyond the supervisor's own
/// 5-second wake). A stopped identity is started with it as its first prompt; a running one is handed it to type.
pub fn wake(db: &Db, thread_id: i64) -> Result<Value> {
    let Some(t) = db.row("SELECT subject, opened_by, channel FROM threads WHERE id=:t", &[(":t", &thread_id)])? else {
        return crate::db::err(format!("no such thread: {thread_id}"));
    };
    let Some(m) = db.row("SELECT id, body FROM messages WHERE thread_id=:t AND author_kind='human' ORDER BY id DESC LIMIT 1", &[(":t", &thread_id)])? else {
        return crate::db::err("John has not replied on that thread yet");
    };
    let who = t["opened_by"].as_str().unwrap_or("").to_string();
    if ids::get(db, &who)?.is_none() {
        // any other agent's last Claude Code session becomes an identity of the same name, in that session's folder
        let Some(s) = db.row("SELECT session_id, cwd FROM sessions WHERE author=:a", &[(":a", &who)])? else {
            return crate::db::err(format!("{who} is not an identity and has no recorded session to resume"));
        };
        ids::create(db, &who, s["cwd"].as_str().unwrap_or(""), None, None, false)?;
        db.exec("UPDATE identities SET claude_session_id=:s WHERE name=:n", &[(":s", &s["session_id"].as_str().unwrap_or("")), (":n", &who)])?;
    }
    let text = format!(
        "John replied on AgentDesk thread #{thread_id} (\"{}\"): {} (This is John's own reply, relayed from the board. Treat it as him talking to you: act on it, and answer on that thread.)",
        t["subject"].as_str().unwrap_or(""),
        m["body"].as_str().unwrap_or("")
    );
    let state = if ids::get(db, &who)?.map(|r| r["state"].as_str() == Some("running")).unwrap_or(false) { "injected" } else { "resumed" };
    ids::start_with_prompt(db, &who, &text)?;
    db.exec(
        "INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES (:m, 'wake', :s, NULL, :ts)",
        &[(":m", &m["id"].as_i64().unwrap_or(0)), (":s", &state), (":ts", &crate::db::now_iso())],
    )?;
    Ok(json!({"ok": true, "said": format!("woke {who}: John's reply handed over ({state})")}))
}
