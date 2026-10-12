//! Behavior tests against a scratch board: the rules the original's parity tests pinned down.

use crate::db::{Db, AGENT, HUMAN, JOHN};
use crate::identity::Caller;
use crate::{hooks, jobs, mcp, tools};
use serde_json::{json, Value};
use std::sync::Mutex;

/// Tests that set AGENTDESK_DATA take this lock: the environment is shared by the whole test process.
static ENV: Mutex<()> = Mutex::new(());

fn board() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.init().unwrap();
    (dir, db)
}

fn caller(session: &str) -> Caller {
    Caller { session_id: Some(session.into()), env_author: None, cwd: Some("/work/proj".into()), harness: Some("claude-code".into()), pid: 1 }
}

fn call(db: &Db, c: &Caller, tool: &str, args: Value) -> Value {
    serde_json::from_str(&tools::call(db, c, tool, &args)).unwrap_or(Value::Null)
}

#[test]
fn a_question_must_ask_something() {
    let (_d, db) = board();
    let r = call(&db, &caller("s1"), "ask_human", json!({"subject": "x", "body": "just news", "author": "builder"}));
    assert!(r["error"].as_str().unwrap().contains("'?'"));
    let r = call(&db, &caller("s1"), "ask_human", json!({"subject": "x", "body": "Ship it?", "author": "builder"}));
    assert_eq!(r["ok"], true);
    assert!(r["wake_on_reply"]["command"].as_str().unwrap().ends_with(&format!("wait {}", r["thread_id"])));
}

#[test]
fn john_answers_and_the_agent_acks_on_its_next_write() {
    let (_d, db) = board();
    let c = caller("s1");
    let q = call(&db, &c, "ask_human", json!({"subject": "deploy", "body": "Deploy now?", "author": "builder"}))["thread_id"].as_i64().unwrap();
    assert_eq!(db.open_questions(false).unwrap().len(), 1);
    db.john_replies(q, "yes").unwrap();
    assert_eq!(db.open_questions(false).unwrap().len(), 0, "answered questions stop ringing");
    let t = db.get_thread(q).unwrap();
    assert_eq!(t["thread"]["status"], "answered");
    // the ack is queued, and posted by the agent's next board write
    assert_eq!(db.scalar_i64("SELECT COUNT(*) FROM acks WHERE state='pending'", &[]).unwrap(), Some(1));
    call(&db, &c, "post_message", json!({"channel": "discussion", "subject": "hi", "body": "hello", "author": "builder"}));
    assert_eq!(db.scalar_i64("SELECT COUNT(*) FROM acks WHERE state='posted'", &[]).unwrap(), Some(1));
    let msgs = db.get_thread(q).unwrap()["messages"].as_array().unwrap().clone();
    assert!(msgs.iter().any(|m| m["body"].as_str().unwrap().starts_with("Acknowledged")));
}

#[test]
fn one_question_one_answer_closing_note_only() {
    let (_d, db) = board();
    let c = caller("s1");
    let q = call(&db, &c, "ask_human", json!({"subject": "s", "body": "ok?", "author": "builder"}))["thread_id"].as_i64().unwrap();
    db.john_replies(q, "yes").unwrap();
    assert_eq!(call(&db, &c, "answer_thread", json!({"thread_id": q, "body": "Done.", "author": "builder"}))["ok"], true);
    let again = call(&db, &c, "answer_thread", json!({"thread_id": q, "body": "one more thing", "author": "builder"}));
    assert!(again["error"].as_str().unwrap().contains("one question and one answer"));
    assert_eq!(db.open_questions(false).unwrap().len(), 0, "an agent's Done never re-opens it");
}

#[test]
fn questions_over_the_word_limit_are_refused() {
    let (_d, db) = board();
    let body = format!("{} ?", "word ".repeat(600));
    let r = call(&db, &caller("s"), "ask_human", json!({"subject": "long", "body": body, "author": "a"}));
    assert!(r["error"].as_str().unwrap().contains("words"));
}

#[test]
fn author_is_resolved_never_blank() {
    let (_d, db) = board();
    let r = call(&db, &caller("abcd1234"), "post_message", json!({"channel": "discussion", "subject": "s", "body": "b", "author": "claude"}));
    let t = db.get_thread(r["thread_id"].as_i64().unwrap()).unwrap();
    assert_eq!(t["thread"]["opened_by"], "claude-code:proj#abcd");
}

#[test]
fn read_receipt_once_per_agent_and_thread() {
    let (_d, db) = board();
    let t = call(&db, &caller("a"), "post_message", json!({"channel": "discussion", "subject": "s", "body": "b", "author": "writer"}))["thread_id"].as_i64().unwrap();
    for _ in 0..3 {
        call(&db, &caller("b"), "read_thread", json!({"thread_id": t, "author": "reader"}));
    }
    let n = db.scalar_i64("SELECT COUNT(*) FROM messages WHERE thread_id=:t AND json_extract(meta,'$.kind')='read-receipt'", &[(":t", &t)]).unwrap();
    assert_eq!(n, Some(1));
    // the author reading their own thread leaves nothing
    call(&db, &caller("a"), "read_thread", json!({"thread_id": t, "author": "writer"}));
    assert_eq!(db.scalar_i64("SELECT COUNT(*) FROM receipts WHERE agent='writer'", &[]).unwrap(), Some(0));
}

#[test]
fn work_queue_claim_and_complete() {
    let (_d, db) = board();
    let w = call(&db, &caller("a"), "post_work", json!({"subject": "job", "body": "do it", "author": "boss", "claim": "anyone"}))["thread_id"].as_i64().unwrap();
    assert_eq!(call(&db, &caller("b"), "claim_work", json!({"thread_id": w, "author": "w1"}))["claimed"], true);
    assert_eq!(call(&db, &caller("c"), "claim_work", json!({"thread_id": w, "author": "w2"}))["claimed"], false);
    assert_eq!(call(&db, &caller("c"), "complete_work", json!({"thread_id": w, "note": "x", "author": "w2"}))["completed"], false);
    assert_eq!(call(&db, &caller("b"), "complete_work", json!({"thread_id": w, "note": "done", "author": "w1"}))["completed"], true);
    let list = call(&db, &caller("b"), "list_work", json!({"status": "done"}));
    assert_eq!(list["work"].as_array().unwrap().len(), 1);
}

#[test]
fn mentions_and_search() {
    let (_d, db) = board();
    let c = caller("a");
    call(&db, &c, "post_message", json!({"channel": "discussion", "subject": "s", "body": "ping @verifier please; mail me@example.com", "author": "builder"}));
    assert_eq!(call(&db, &c, "list_mentions", json!({"name": "verifier"}))["mentions"].as_array().unwrap().len(), 1);
    assert_eq!(call(&db, &c, "list_mentions", json!({"name": "example"}))["mentions"].as_array().unwrap().len(), 0);
    assert_eq!(call(&db, &c, "search_messages", json!({"query": "verifier"}))["results"].as_array().unwrap().len(), 1);
    assert_eq!(call(&db, &c, "search_messages", json!({"query": "example.com"}))["results"].as_array().unwrap().len(), 1, "substring finds what FTS tokenises apart");
}

#[test]
fn request_merge_is_idempotent() {
    let (_d, db) = board();
    let c = caller("a");
    let a = call(&db, &c, "request_merge", json!({"pr_url": "https://github.com/o/r/pull/7?x=1", "note": "Fix the thing\nmore", "author": "b"}));
    assert_eq!(a["created"], true);
    assert_eq!(a["url"], "https://github.com/o/r/pull/7");
    let b = call(&db, &c, "request_merge", json!({"pr_url": "https://github.com/o/r/pull/7", "author": "b"}));
    assert_eq!(b["created"], false);
    assert_eq!(a["pr_id"], b["pr_id"]);
    assert!(call(&db, &c, "request_merge", json!({"pr_url": "https://example.com/x"}))["error"].is_string());
}

#[test]
fn closing_and_unarchiving() {
    let (_d, db) = board();
    let q = call(&db, &caller("a"), "ask_human", json!({"subject": "s", "body": "ok?", "author": "b"}))["thread_id"].as_i64().unwrap();
    assert!(db.close_question(q).unwrap());
    assert_eq!(db.open_questions(false).unwrap().len(), 0);
    assert!(db.archive(q).unwrap());
    assert_eq!(db.get_thread(q).unwrap()["thread"]["status"], "archived");
    assert!(db.unarchive(q).unwrap());
    assert_eq!(db.get_thread(q).unwrap()["thread"]["status"], "closed");
    assert!(db.get_thread(q).unwrap()["thread"]["meta"].as_str().unwrap().contains("archive_hold"));
}

#[test]
fn hook_injects_johns_reply_once_into_the_asking_session() {
    let (_d, db) = board();
    let c = caller("sess-1");
    let q = call(&db, &c, "ask_human", json!({"subject": "pick", "body": "A or B?", "author": "builder"}))["thread_id"].as_i64().unwrap();
    assert_eq!(hooks::run(&db, "context", &json!({"session_id": "sess-1"})), "", "nothing to say yet");
    db.john_replies(q, "B").unwrap();
    let out: Value = serde_json::from_str(&hooks::run(&db, "context", &json!({"session_id": "sess-1"}))).unwrap();
    assert!(out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("JOHN REPLIED"));
    assert_eq!(hooks::run(&db, "context", &json!({"session_id": "sess-1"})), "", "shown once");
    assert_eq!(hooks::run(&db, "context", &json!({"session_id": "other"})), "", "and only to its own session");
}

#[test]
fn stop_hook_blocks_on_an_unseen_reply() {
    let (_d, db) = board();
    let c = caller("sess-2");
    let q = call(&db, &c, "ask_human", json!({"subject": "s", "body": "go?", "author": "b"}))["thread_id"].as_i64().unwrap();
    db.john_replies(q, "go").unwrap();
    let out: Value = serde_json::from_str(&hooks::run(&db, "stop", &json!({"session_id": "sess-2"}))).unwrap();
    assert_eq!(out["decision"], "block");
}

#[test]
fn session_start_briefing_lists_what_rings() {
    let (_d, db) = board();
    call(&db, &caller("a"), "ask_human", json!({"subject": "need decision", "body": "now?", "author": "b"}));
    let out: Value = serde_json::from_str(&hooks::run(&db, "session-start", &json!({"session_id": "new"}))).unwrap();
    let ctx = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("Ringing for John (1)") && ctx.contains("need decision"));
}

#[test]
fn mcp_handshake_lists_the_tools_and_calls_one() {
    let (_d, db) = board();
    let c = caller("a");
    let init = mcp::handle(&db, &c, &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert!(init["result"]["instructions"].as_str().unwrap().contains("shared message board"));
    assert!(mcp::handle(&db, &c, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
    let list = mcp::handle(&db, &c, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 18);
    let r = mcp::handle(&db, &c, &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "recent_messages", "arguments": {}}})).unwrap();
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("messages"));
    assert_eq!(mcp::handle(&db, &c, &json!({"jsonrpc": "2.0", "id": 4, "method": "nope"})).unwrap()["error"]["code"], -32601);
}

#[test]
fn list_threads_marks_waiting_and_follow_up() {
    let (_d, db) = board();
    let c = caller("a");
    let q = call(&db, &c, "ask_human", json!({"subject": "s", "body": "ok?", "author": "b"}))["thread_id"].as_i64().unwrap();
    let rows = db.list_threads(Some("question"), None, 50, true, true).unwrap();
    assert_eq!(rows[0]["waiting"], 1);
    db.john_replies(q, "yes").unwrap();
    call(&db, &c, "answer_thread", json!({"thread_id": q, "body": "Done: merged.", "author": "b"}));
    let rows = db.list_threads(Some("question"), None, 50, true, true).unwrap();
    assert_eq!(rows[0]["waiting"], 0);
    assert_eq!(rows[0]["follow_up"], "done");
    assert_eq!(rows[0]["delivery"].as_str().unwrap().split('|').next().unwrap(), "picked-up");
    let _ = (AGENT, HUMAN, JOHN);
}

#[test]
fn recurring_schedule_math() {
    use chrono::{TimeZone, Utc};
    let t = Utc.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).unwrap();
    assert_eq!(jobs::next(t, None, "daily", Some(90.0)).unwrap(), t + chrono::Duration::minutes(90));
    let n = jobs::next(t, Some("08:00"), "weekdays", None).unwrap();
    assert!(n > t && n < t + chrono::Duration::days(4));
    assert!(jobs::next(t, Some("25:00"), "daily", None).is_err());
    assert!(jobs::next(t, Some("08:00"), "funday", None).is_err());
}

#[test]
fn jobs_create_validate_and_toggle() {
    let (d, db) = board();
    let folder = d.path().to_string_lossy().to_string();
    let j = jobs::create(
        &db,
        jobs::NewJob { name: "brief", folder: &folder, prompt: "do", at: Some("08:00"), days: Some("weekdays"), every_minutes: None, model: None, catch_up_minutes: None },
    )
    .unwrap();
    assert_eq!(j["model"], "sonnet");
    assert!(jobs::create(
        &db,
        jobs::NewJob { name: "brief", folder: &folder, prompt: "do", at: Some("08:00"), days: None, every_minutes: None, model: None, catch_up_minutes: None }
    )
    .is_err());
    assert!(jobs::create(
        &db,
        jobs::NewJob { name: "bad", folder: &folder, prompt: "do", at: Some("08:00"), days: None, every_minutes: Some(5.0), model: None, catch_up_minutes: None }
    )
    .is_err());
    assert_eq!(jobs::enable(&db, "brief", false).unwrap()["enabled"], 0);
    jobs::delete(&db, "brief").unwrap();
    assert!(jobs::list(&db).unwrap().is_empty());
}

#[test]
fn permission_rules_need_john() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (d, db) = board();
    std::env::set_var("AGENTDESK_DATA", d.path());
    let r = call(
        &db,
        &caller("a"),
        "propose_permission_rule",
        json!({"rule": "Bash(cargo test:*)", "reason": "tests", "author": "b", "scope": "project", "project_dir": d.path().to_string_lossy()}),
    );
    let id = r["id"].as_i64().unwrap();
    assert!(!d.path().join(".claude/settings.json").exists(), "nothing is written by a proposal");
    assert_eq!(db.open_questions(false).unwrap().len(), 1);
    crate::permissions::decide(&db, id, true).unwrap();
    let text = std::fs::read_to_string(d.path().join(".claude/settings.json")).unwrap();
    assert!(text.contains("Bash(cargo test:*)"));
    assert_eq!(db.open_questions(false).unwrap().len(), 0, "approving settles the question");
    crate::permissions::revert(&db, id).unwrap();
    assert!(!std::fs::read_to_string(d.path().join(".claude/settings.json")).unwrap().contains("cargo test"));
}

#[test]
fn migrate_points_claude_code_at_this_binary_and_nothing_else() {
    let mut cfg = json!({
        "mcpServers": {"agentdesk": {"type": "stdio", "command": "C:\\Users\\j\\AppData\\Local\\AgentDeskApp\\current\\agentdesk.exe", "args": []}, "other": {"command": "node", "args": ["x.js"]}},
        "projects": {"/p": {"mcpServers": {"agentdesk": {"command": "agentdesk", "args": []}}}}
    });
    let changes = crate::migrate::patch_mcp(&mut cfg, "/bin/agentdesk");
    assert_eq!(changes.len(), 2);
    assert_eq!(cfg["mcpServers"]["agentdesk"]["command"], "/bin/agentdesk");
    assert_eq!(cfg["mcpServers"]["agentdesk"]["args"], json!(["mcp"]));
    assert_eq!(cfg["mcpServers"]["other"]["command"], "node", "other servers are untouched");
    assert!(crate::migrate::patch_mcp(&mut cfg, "/bin/agentdesk").is_empty(), "idempotent");

    let mut settings = json!({"model": "x", "hooks": {
        "SessionStart": [{"hooks": [{"type": "command", "command": "\"C:\\old\\agentdesk.exe\" hook session-start"}]}],
        "PostToolUse": [{"hooks": [{"type": "command", "command": "agentdesk hook context"}, {"type": "command", "command": "echo other"}]}]}});
    let changes = crate::migrate::patch_hooks(&mut settings, "/bin/agentdesk");
    assert_eq!(changes.len(), 2);
    assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"], "\"/bin/agentdesk\" hook session-start");
    assert_eq!(settings["hooks"]["PostToolUse"][0]["hooks"][1]["command"], "echo other");
    assert_eq!(settings["model"], "x");
}

#[test]
fn migrate_dry_run_writes_nothing_and_apply_backs_up() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (d, db) = board();
    db.start_thread("discussion", "old", "a", "agent", "x", None, None).unwrap();
    drop(db);
    std::env::set_var("AGENTDESK_DATA", d.path());
    let (cj, st) = (d.path().join("claude.json"), d.path().join("settings.json"));
    std::fs::write(&cj, r#"{"mcpServers":{"agentdesk":{"command":"agentdesk.exe","args":[]}}}"#).unwrap();
    std::fs::write(&st, r#"{"hooks":{}}"#).unwrap();
    std::fs::rename(d.path().join("t.db"), d.path().join("agentdesk.db")).unwrap();
    let before = std::fs::read_to_string(&cj).unwrap();
    let report = crate::migrate::run(false, &cj, &st).unwrap();
    assert!(report.contains("dry run") && report.contains("1 threads"));
    assert_eq!(std::fs::read_to_string(&cj).unwrap(), before);
    crate::migrate::run(true, &cj, &st).unwrap();
    assert!(std::fs::read_to_string(&cj).unwrap().contains("\"mcp\""));
    assert!(std::fs::read_dir(d.path().join("migrate_backups")).unwrap().count() >= 2);
}

#[test]
fn a_fresh_board_has_every_table_the_original_made_and_init_is_idempotent() {
    let (_d, db) = board();
    for t in ["identities", "phoenix_chain", "goals", "experiments", "goal_members", "slots", "usage_samples", "permission_proposals", "recurring_jobs"] {
        assert!(db.row("SELECT name FROM sqlite_master WHERE type='table' AND name=:n", &[(":n", &t)]).unwrap().is_some(), "missing table {t}");
    }
    let version: i64 = db.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert!(version > 0, "init stamps user_version");
    db.init().unwrap(); // a board at the current version is left alone
    assert_eq!(version, db.conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap());
}

#[test]
fn init_adds_the_columns_a_board_from_an_older_build_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("old.db")).unwrap();
    // The first generation of the identities table, before generation/phoenix_msg/model/running_model existed.
    db.conn
        .execute_batch(
            "CREATE TABLE identities (name TEXT PRIMARY KEY COLLATE NOCASE, folder TEXT NOT NULL, charter TEXT, host TEXT NOT NULL DEFAULT 'windows',
             claude_session_id TEXT, pid INTEGER, state TEXT NOT NULL DEFAULT 'stopped', autostart INTEGER NOT NULL DEFAULT 0,
             created_ts TEXT NOT NULL, updated_ts TEXT NOT NULL);",
        )
        .unwrap();
    db.init().unwrap();
    let cols: Vec<String> = db.rows("PRAGMA table_info(identities)", &[]).unwrap().iter().filter_map(|r| r["name"].as_str().map(String::from)).collect();
    for c in ["generation", "phoenix_msg", "model", "running_model"] {
        assert!(cols.iter().any(|x| x == c), "missing column {c}");
    }
}

#[test]
fn a_usage_sample_lands_in_the_originals_table_shape() {
    let (_d, db) = board();
    // What `claude_usage.json` carries: the reading the window shows is recorded as weekly_pct (NOT NULL in the original's table).
    db.exec("INSERT INTO usage_samples (ts, weekly_pct, five_hour_pct) VALUES ('2026-10-12T00:00:00+00:00', 41.5, 12.0)", &[]).unwrap();
    let r = db.row("SELECT weekly_pct, swarm_sessions, sonnet_sessions FROM usage_samples", &[]).unwrap().unwrap();
    assert_eq!((r["weekly_pct"].as_f64(), r["swarm_sessions"].as_i64(), r["sonnet_sessions"].as_i64()), (Some(41.5), Some(0), Some(0)));
}
