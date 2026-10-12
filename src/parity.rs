//! Golden parity: each scenario replays the original server's recorded steps from `testdata/golden/seed.db` and must
//! reproduce every step's document and the final rows of every table. The clock is a tick counter (one second per
//! timestamp, starting where the recording did), so ids and timestamps match exactly; nothing is ignored. In the
//! recording, paths were normalised to `C:\parity`, session pids to 4242 and the wait command to `wait {tid}`.

use crate::db::{self, Db};
use crate::identity::Caller;
use crate::{tools, vault};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join("golden")
}

const TABLES: [&str; 10] = ["threads", "messages", "presence", "acks", "receipts", "pull_requests", "work_events", "handoffs", "sessions", "deliveries"];

fn caller(name: &str) -> Caller {
    let c = |sid: Option<&str>, author: Option<&str>, cwd: &str, harness: Option<&str>| Caller {
        session_id: sid.map(Into::into),
        env_author: author.map(Into::into),
        cwd: Some(cwd.into()),
        harness: harness.map(Into::into),
        pid: 4242,
    };
    match name {
        "alpha" => c(Some("a1b2c3d4-0000"), None, r"C:\parity\proj-alpha", Some("claude-code")),
        "beta" => c(Some("Be-7a-99"), None, r"C:\parity\proj-beta", Some("claude-code")),
        "crew" => c(Some("c0ffee"), Some("builder"), r"C:\parity\crew", Some("claude-code")),
        _ => c(None, None, r"C:\parity\proj-bare", None),
    }
}

fn step(op: &str, who: &str, args: Value) -> Value {
    json!({"op": op, "caller": who, "args": args})
}
fn john(thread_id: i64, body: &str) -> Value {
    step("john_reply", "", json!({"thread_id": thread_id, "body": body}))
}

fn words(n: usize) -> String {
    vec!["word"; n].join(" ")
}

/// Runs the steps and compares with `golden/<scenario>.json`. Returns each step's raw result and the db's dir.
fn run(scenario: &str, steps: Vec<Value>) -> (Vec<String>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentdesk.db");
    std::fs::copy(golden_dir().join("seed.db"), &path).unwrap();
    let db = Db::open(&path).unwrap();
    db::pin_clock(Some(1000));
    vault::FAKE.with(|f| f.set(true));
    let mut results = Vec::new();
    for s in &steps {
        let op = s["op"].as_str().unwrap();
        if op == "john_reply" {
            db.john_replies(s["args"]["thread_id"].as_i64().unwrap(), s["args"]["body"].as_str().unwrap()).unwrap();
            results.push(String::new());
        } else {
            results.push(tools::call(&db, &caller(s["caller"].as_str().unwrap()), op, &s["args"]));
        }
    }
    db::pin_clock(None);
    vault::FAKE.with(|f| f.set(false));
    let mut tables = Map::new();
    for t in TABLES {
        tables.insert(t.into(), Value::Array(db.rows(&format!("SELECT * FROM {t} ORDER BY rowid"), &[]).unwrap()));
    }
    let actual_steps: Vec<Value> = results.iter().map(|r| if r.is_empty() { Value::Null } else { serde_json::from_str(r).unwrap_or(Value::String(r.clone())) }).collect();
    let gpath = golden_dir().join(format!("{scenario}.json"));
    if std::env::var("AGENTDESK_BLESS").as_deref() == Ok("1") {
        std::fs::write(&gpath, serde_json::to_string_pretty(&json!({"steps": actual_steps, "tables": tables})).unwrap() + "\n").unwrap();
    }
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(&gpath).unwrap()).unwrap();
    let (g, a) = (serde_json::to_string_pretty(&golden["tables"]).unwrap(), serde_json::to_string_pretty(&Value::Object(tables)).unwrap());
    assert!(g == a, "{scenario}: final tables differ\n{}", first_diff(&g, &a));
    for (i, s) in steps.iter().enumerate() {
        if s["op"] == "formatting_help" {
            continue; // deliberate: the help text says what the terminal window does with tables, code and diagrams
        }
        let (g, a) = (&golden["steps"][i], &actual_steps[i]);
        assert!(
            serde_json::to_string_pretty(g).unwrap() == serde_json::to_string_pretty(a).unwrap(),
            "{scenario}: step {i} {} {} differs\n{}",
            s["op"],
            s["args"],
            first_diff(&serde_json::to_string_pretty(g).unwrap(), &serde_json::to_string_pretty(a).unwrap())
        );
    }
    (results, dir)
}

fn first_diff(g: &str, a: &str) -> String {
    for (i, (x, y)) in g.lines().zip(a.lines()).enumerate() {
        if x != y {
            let ctx: Vec<&str> = g.lines().skip(i.saturating_sub(6)).take(8).collect();
            return format!("line {i}\n  golden: {x}\n  rust:   {y}\ncontext (golden):\n{}", ctx.join("\n"));
        }
    }
    format!("lengths differ: golden {} lines, rust {} lines", g.lines().count(), a.lines().count())
}

#[test]
fn post_read_reply_and_receipts() {
    run(
        "PostReadReplyAndReceipts",
        vec![
            step("formatting_help", "alpha", json!({})),
            step("post_message", "alpha", json!({"channel": "chat", "subject": "x", "body": "y"})),
            step(
                "post_message",
                "alpha",
                json!({"channel": "discussion", "subject": "Findings", "body": "Found it on build-01.vispero.local, see @builder and @claude-code:proj-beta#be7a, not user@example.com.\n```\n@echo off\n```"}),
            ),
            step("post_message", "beta", json!({"channel": "discussion", "subject": "", "body": "Thanks, on it.", "author": "claude", "thread_id": 5})),
            step("post_message", "alpha", json!({"channel": "discussion", "subject": "", "body": "?", "thread_id": 999})),
            step("post_message", "alpha", json!({"channel": "wiki", "subject": "Trap: flag X is off", "body": "Because."})),
            step("post_message", "alpha", json!({"channel": "wiki", "subject": "FAIL note", "body": "Boom."})),
            step("post_message", "alpha", json!({"channel": "wiki", "subject": "", "body": "More.", "thread_id": 6})),
            step("read_thread", "beta", json!({"thread_id": 5})),
            step("read_thread", "beta", json!({"thread_id": 5})),
            step("read_thread", "alpha", json!({"thread_id": 6})),
            step("read_thread", "alpha", json!({"thread_id": 999})),
            step("read_thread", "alpha", json!({"thread_id": 3})),
            step("answer_thread", "crew", json!({"thread_id": 5, "body": "Builder here."})),
            step("answer_thread", "bare", json!({"thread_id": 999, "body": "?"})),
            step("recent_messages", "alpha", json!({"limit": 5})),
            step("recent_messages", "alpha", json!({})),
        ],
    );
}

#[test]
fn questions_waiting_and_acks() {
    let ask = |n: usize| format!("{} ok?", words(n - 1));
    run(
        "QuestionsWaitingAndAcks",
        vec![
            step("open_questions", "alpha", json!({})),
            step(
                "ask_human",
                "alpha",
                json!({"subject": "Restart?", "body": "Can I restart @builder's host? Ünïcode ✓", "meta": {"kind": "sneaky", "priority": 2, "ratio": 1.5, "whole": 1, "tags": ["a", "ü"], "mentions": ["x"]}}),
            ),
            step("ask_human", "beta", json!({"subject": "Too long", "body": ask(501)})),
            step("ask_human", "beta", json!({"subject": "Just fits", "body": ask(500)})),
            step("ask_human", "beta", json!({"subject": "Just news", "body": "Build 12 finished green."})),
            step("open_questions", "alpha", json!({})),
            step("open_questions", "alpha", json!({"include_archived": true})),
            step("list_threads", "alpha", json!({"channel": "question", "include_archived": false})),
            step("list_threads", "alpha", json!({"status": "answered"})),
            step("list_threads", "alpha", json!({"limit": 3})),
            john(5, "Yes, go ahead."),
            step("open_questions", "alpha", json!({})),
            step("list_threads", "alpha", json!({"channel": "question"})),
            step("read_thread", "alpha", json!({"thread_id": 5})),
            step("answer_thread", "alpha", json!({"thread_id": 5, "body": "Done, restarted."})),
            step("open_questions", "alpha", json!({})),
            step("answer_thread", "alpha", json!({"thread_id": 5, "body": "One more thing."})),
            step("list_threads", "alpha", json!({"channel": "question"})),
            step("answer_thread", "alpha", json!({"thread_id": 6, "body": words(501)})),
            step("post_message", "alpha", json!({"channel": "discussion", "subject": "", "body": words(501), "thread_id": 6})),
            john(6, "No."),
            step("open_questions", "beta", json!({})),
        ],
    );
}

#[test]
fn work_queue() {
    run(
        "WorkQueue",
        vec![
            step("post_work", "alpha", json!({"subject": "x", "body": "y", "claim": "mine"})),
            step("post_work", "alpha", json!({"subject": "Build it", "body": "Please build."})),
            step("list_work", "alpha", json!({})),
            step("list_work", "alpha", json!({"status": "open"})),
            step("claim_work", "beta", json!({"thread_id": 5})),
            step("claim_work", "crew", json!({"thread_id": 5})),
            step("claim_work", "crew", json!({"thread_id": 4})),
            step("complete_work", "crew", json!({"thread_id": 5, "note": "Not mine."})),
            step("complete_work", "beta", json!({"thread_id": 5, "note": "Built."})),
            step("complete_work", "beta", json!({"thread_id": 5, "note": "Again."})),
            step("claim_work", "beta", json!({"thread_id": 999})),
            step("complete_work", "beta", json!({"thread_id": 999, "note": "?"})),
            step("list_work", "alpha", json!({"status": "done"})),
            step("read_thread", "alpha", json!({"thread_id": 5})),
        ],
    );
}

#[test]
fn request_merge() {
    run(
        "RequestMerge",
        vec![
            step("request_merge", "alpha", json!({"pr_url": "https://gitlab.com/x/y/merge_requests/1"})),
            step(
                "request_merge",
                "alpha",
                json!({"pr_url": " https://www.GitHub.com/palencharj/agentdesk/pull/007/files ", "thread_id": 1, "note": "  Fix the thing\nDetails here."}),
            ),
            step("request_merge", "beta", json!({"pr_url": "https://github.com/palencharj/agentdesk/pull/7", "thread_id": 1, "note": "dupe"})),
            step("request_merge", "alpha", json!({"pr_url": "http://github.com/a/b/pull/9?diff=split"})),
            step("request_merge", "alpha", json!({"pr_url": "https://github.com/a/b/pull/11", "thread_id": 999})),
            step("read_thread", "beta", json!({"thread_id": 1})),
            step("recent_messages", "alpha", json!({"limit": 3})),
        ],
    );
}

#[test]
fn request_merge_blank_note_falls_back_to_url() {
    let (results, dir) =
        run("RequestMergeBlankNoteFallsBackToUrl", vec![step("request_merge", "alpha", json!({"pr_url": "https://github.com/a/b/pull/10", "thread_id": 1, "note": "   "}))]);
    assert_eq!(serde_json::from_str::<Value>(&results[0]).unwrap()["created"], true);
    let db = Db::open(&dir.path().join("agentdesk.db")).unwrap();
    assert_eq!(db.scalar_str("SELECT title FROM pull_requests WHERE number=10", &[]).unwrap().unwrap(), "https://github.com/a/b/pull/10");
}

#[test]
fn torch_and_mentions() {
    run(
        "TorchAndMentions",
        vec![
            step("open_questions", "crew", json!({"author": "builder"})),
            step("open_questions", "alpha", json!({"author": "claude"})),
            step("pass_the_torch", "crew", json!({"handoff": "Owns X. Next: Y."})),
            step("open_questions", "crew", json!({"author": "builder"})),
            step("pass_the_torch", "alpha", json!({"handoff": "Mid-way through Z."})),
            step("list_threads", "alpha", json!({"channel": "discussion"})),
            step("post_message", "alpha", json!({"channel": "discussion", "subject": "ping", "body": "ping @claude-code:proj-beta#be7a and @Builder"})),
            step("list_mentions", "alpha", json!({"name": "builder"})),
            step("list_mentions", "alpha", json!({"name": "BUILDER", "limit": 1})),
            step("list_mentions", "beta", json!({})),
        ],
    );
}

#[test]
fn search() {
    run(
        "Search",
        vec![
            step("post_message", "alpha", json!({"channel": "discussion", "subject": "Deploy log", "body": "deploy step one"})),
            step("post_message", "beta", json!({"channel": "discussion", "subject": "", "body": "deploy step two, then builder", "thread_id": 5})),
            step("search_messages", "alpha", json!({"query": "deploy"})),
            step("search_messages", "alpha", json!({"query": "deploy", "limit": 2})),
            step("search_messages", "alpha", json!({"query": "build-01.vispero"})),
            step("search_messages", "alpha", json!({"query": "\"unbalanced"})),
            step("search_messages", "alpha", json!({"query": "builder", "limit": 1})),
            step("search_messages", "alpha", json!({"query": "o", "limit": -1})),
            step("search_messages", "alpha", json!({"query": "nothingmatches"})),
            step("search_vault", "alpha", json!({"query": "flag", "full": 1})),
            step("search_vault", "alpha", json!({"query": "flag", "k": 2, "full": -1})),
            step("search_vault", "alpha", json!({"query": "offline"})),
        ],
    );
}
