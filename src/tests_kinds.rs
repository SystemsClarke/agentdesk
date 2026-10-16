//! Question kinds: ask, run, permission, packet (docs/question-types.md), against a scratch board.

use crate::db::{Db, HUMAN, JOHN};
use crate::identity::Caller;
use crate::kinds;
use crate::tests::{call, ENV};
use serde_json::{json, Value};

fn board() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.init().unwrap();
    (dir, db)
}

fn caller(session: &str) -> Caller {
    Caller { session_id: Some(session.into()), env_author: None, cwd: Some("/work/proj".into()), harness: Some("claude-code".into()), pid: 1 }
}

fn spec() -> Value {
    json!({
        "outcome": "Done when both GoCD runs have finished and the evidence is on ADO 201377.",
        "why": "Only John can trigger GoCD.",
        "happened": ["The diagnostic PR merged Thursday.", {"fact": "Nothing has run it since.", "link": "https://github.com/o/r/pull/3?utm_source=mail"}],
        "found": [
            "https://github.com/o/r/pull/3",
            {"url": "https://GITHUB.com/o/r/pull/3/#files", "label": "the PR"},
            "https://nam11.safelinks.protection.outlook.com/?url=https%3A%2F%2Fdev.azure.com%2Fv%2Fp%2F_workitems%2Fedit%2F201377&data=1",
            "javascript:alert(1)"
        ],
        "steps": [
            {"id": "run-1", "title": "Start a new run with MIN_FREE_PERCENT empty", "detail": "Expect: Symbols share space OK"},
            {"id": "which", "title": "Pick how to proceed", "input": "choice", "options": ["I run it", "agent retries"]},
            {"id": "note", "title": "Anything the agent should know", "input": "note"}
        ],
        "next": "I read both consoles and close the ADO item only when both match.",
        "deadline": {"date": "2026-10-17", "reason": "Closes the sprint item Friday."},
        "work": {"recommendation": "Run both today.", "confidence": "high"}
    })
}

fn packet(db: &Db, c: &Caller) -> i64 {
    let r =
        call(db, c, "ask_human", json!({"subject": "Two GoCD runs", "body": "Two runs are waiting on you.", "author": "builder", "meta": {"type": "packet", "packet": spec()}}));
    assert_eq!(r["ok"], true, "{r}");
    r["thread_id"].as_i64().unwrap()
}

fn status(db: &Db, tid: i64) -> String {
    db.get_thread(tid).unwrap()["thread"]["status"].as_str().unwrap().to_string()
}

fn meta_of(db: &Db, tid: i64) -> Value {
    serde_json::from_str(db.get_thread(tid).unwrap()["thread"]["meta"].as_str().unwrap()).unwrap()
}

fn first_body(db: &Db, tid: i64) -> String {
    db.get_thread(tid).unwrap()["messages"][0]["body"].as_str().unwrap().to_string()
}

#[test]
fn a_question_with_no_type_is_an_ask_and_keeps_its_rules() {
    let (_d, db) = board();
    let c = caller("s1");
    assert!(call(&db, &c, "ask_human", json!({"subject": "x", "body": "news", "author": "b"}))["error"].as_str().unwrap().contains("'?'"));
    let q = call(&db, &c, "ask_human", json!({"subject": "x", "body": "Ship it?", "author": "b", "meta": {"type": "ask"}}))["thread_id"].as_i64().unwrap();
    assert_eq!(kinds::kind_of(&db, q).unwrap(), "ask");
    let long = format!("{} ship it?", "word ".repeat(600));
    assert!(call(&db, &c, "ask_human", json!({"subject": "x", "body": long, "author": "b"}))["error"].as_str().unwrap().contains("words"));
}

#[test]
fn permission_cannot_be_asked_for_with_ask_human_and_unknown_types_are_refused() {
    let (_d, db) = board();
    let c = caller("s1");
    let r = call(&db, &c, "ask_human", json!({"subject": "x", "body": "Allow it?", "author": "b", "meta": {"type": "permission"}}));
    assert!(r["error"].as_str().unwrap().contains("propose_permission_rule"));
    let r = call(&db, &c, "ask_human", json!({"subject": "x", "body": "Allow it?", "author": "b", "meta": {"type": "poll"}}));
    assert!(r["error"].as_str().unwrap().contains("meta.type must be one of"));
}

#[test]
fn a_run_question_needs_a_script_and_is_not_held_to_the_ask_rules() {
    let (_d, db) = board();
    let c = caller("s1");
    let r = call(&db, &c, "ask_human", json!({"subject": "Install", "body": "Please run the installer.", "author": "b", "meta": {"type": "run"}}));
    assert!(r["error"].as_str().unwrap().contains("fenced"));
    let long = format!("{}\n\n```powershell\nWrite-Output 'hi'\n```\n", "context ".repeat(700));
    let r = call(&db, &c, "ask_human", json!({"subject": "Install", "body": long, "author": "b", "meta": {"type": "run"}}));
    assert_eq!(r["ok"], true, "no '?' and over 500 words are fine for a run question: {r}");
    assert_eq!(r["type"], "run");
    let q = r["thread_id"].as_i64().unwrap();
    assert_eq!(kinds::kind_of(&db, q).unwrap(), "run");
    db.john_replies(q, "I ran the script: it finished (exit 0).").unwrap();
    assert_eq!(status(&db, q), "answered", "a run question closes when John's result comes back");
}

#[test]
fn an_incomplete_packet_is_refused_with_every_problem_at_once() {
    let (_d, db) = board();
    let c = caller("s1");
    let bad = json!({"outcome": "Fix the thing", "steps": [{"id": "Bad Id", "title": "x"}], "found": "nope"});
    let r = call(&db, &c, "ask_human", json!({"subject": "x", "body": "Please do this.", "author": "b", "meta": {"type": "packet", "packet": bad}}));
    let msg = r["error"].as_str().unwrap();
    for part in ["outcome must start with", "why is missing", "happened is missing", "found is missing", "steps[0].id", "next is missing", "deadline is missing"] {
        assert!(msg.contains(part), "'{part}' not in: {msg}");
    }
    let r = call(&db, &c, "ask_human", json!({"subject": "x", "body": "Please do this.", "author": "b", "meta": {"type": "packet"}}));
    assert!(r["error"].as_str().unwrap().contains("meta.packet"));
    assert_eq!(db.open_questions(false).unwrap().len(), 0, "a refused packet leaves nothing on the board");
}

#[test]
fn packet_fields_are_checked_one_by_one() {
    let mut s = spec();
    s["deadline"] = json!({"date": "2026-13-45", "reason": "x"});
    assert!(kinds::validate_packet(&s).unwrap_err().iter().any(|p| p.contains("real date")));
    let mut s = spec();
    s["steps"][1]["options"] = json!(["only one"]);
    assert!(kinds::validate_packet(&s).unwrap_err().iter().any(|p| p.contains("2 to 6 options")));
    let mut s = spec();
    s["steps"][2]["id"] = json!("run-1");
    assert!(kinds::validate_packet(&s).unwrap_err().iter().any(|p| p.contains("unique")));
    let mut s = spec();
    s["steps"] = json!((0..13).map(|i| json!({"id": format!("s{i}"), "title": "t"})).collect::<Vec<_>>());
    assert!(kinds::validate_packet(&s).unwrap_err().iter().any(|p| p.contains("at most 12")));
    let mut s = spec();
    s["happened"][1]["link"] = json!("not a link");
    assert!(kinds::validate_packet(&s).unwrap_err().iter().any(|p| p.contains("not a web link")));
    let mut s = spec();
    s["found"] = json!([]);
    assert!(kinds::validate_packet(&s).is_ok(), "a packet may have nothing to open");
}

#[test]
fn a_valid_packet_is_stored_rendered_and_has_its_links_cleaned() {
    let (_d, db) = board();
    let c = caller("s1");
    let r =
        call(&db, &c, "ask_human", json!({"subject": "Two GoCD runs", "body": "Two runs are waiting on you.", "author": "builder", "meta": {"type": "packet", "packet": spec()}}));
    assert_eq!(r["type"], "packet");
    assert_eq!(r["links"]["rejected"][0], "javascript:alert(1)", "a bad link is reported, not hidden");
    let q = r["thread_id"].as_i64().unwrap();
    assert_eq!(kinds::kind_of(&db, q).unwrap(), "packet");
    let meta = meta_of(&db, q);
    let found = meta["packet"]["found"].as_array().unwrap();
    assert_eq!(found.len(), 2, "the PR is listed once and the Safe Link is unwrapped: {found:?}");
    assert_eq!(found[0]["cited"], 2);
    assert_eq!(found[1]["url"], "https://dev.azure.com/v/p/_workitems/edit/201377");
    assert_eq!(meta["packet"]["happened"][1]["link"], "https://github.com/o/r/pull/3");
    let body = first_body(&db, q);
    for part in [
        "Two runs are waiting on you.",
        "Done when both GoCD runs",
        "### Why it is yours",
        "### Your steps",
        "- [ ] **Start a new run",
        "(I run it / agent retries)",
        "### Deadline",
        "**2026-10-17**",
        "### Show my work",
        "**Recommendation:** Run both today.",
    ] {
        assert!(body.contains(part), "'{part}' not in:\n{body}");
    }
    assert_eq!(db.open_questions(false).unwrap().len(), 1, "it rings like any question");
    let big = format!("{} ", "context ".repeat(700));
    let mut s = spec();
    s["why"] = json!(big);
    assert_eq!(
        call(&db, &c, "ask_human", json!({"subject": "Long", "body": "Details.", "author": "b", "meta": {"type": "packet", "packet": s}}))["ok"],
        true,
        "a packet is not held to 500 words or a '?'"
    );
}

#[test]
fn ticking_a_step_keeps_the_packet_open_wakes_the_agent_and_updates_the_page() {
    let (_d, db) = board();
    let c = caller("s1");
    let q = packet(&db, &c);
    let after = db.scalar_i64("SELECT MAX(id) FROM messages WHERE thread_id=:t", &[(":t", &q)]).unwrap().unwrap();
    let p = kinds::step(&db, q, "run-1", true, None, Some("started it at 14:05")).unwrap();
    assert_eq!((p["done"].as_i64(), p["total"].as_i64()), (Some(1), Some(3)));
    assert_eq!(status(&db, q), "open", "a tick is not an answer");
    let woke = db.row("SELECT author_kind, body FROM messages WHERE thread_id=:t AND id>:a AND author_kind='human'", &[(":t", &q), (":a", &after)]).unwrap().unwrap();
    assert!(woke["body"].as_str().unwrap().contains("Ticked \"Start a new run"), "this is what `agentdesk wait` hands the agent: {woke}");
    assert!(woke["body"].as_str().unwrap().contains("started it at 14:05"));
    assert!(first_body(&db, q).contains("- [x] **Start a new run"), "the page shows the tick");
    assert_eq!(kinds::progress(&meta_of(&db, q)), (1, 3));
    assert_eq!(db.open_questions(false).unwrap().len(), 0, "after John acts it is the agent's turn, as for any question");
    call(&db, &c, "post_message", json!({"channel": "question", "subject": "x", "body": "Saw it: reading the console.", "author": "builder", "thread_id": q}));
    assert_eq!(db.open_questions(false).unwrap().len(), 1, "and the agent's reply is waiting on John again");

    assert!(kinds::step(&db, q, "which", true, None, None).unwrap_err().to_string().contains("pick one of"));
    assert!(kinds::step(&db, q, "which", true, Some("nonsense"), None).unwrap_err().to_string().contains("not one of"));
    kinds::step(&db, q, "which", true, Some("I run it"), None).unwrap();
    assert!(first_body(&db, q).contains("→ I run it"));
    assert!(kinds::step(&db, q, "nope", true, None, None).unwrap_err().to_string().contains("no step"));
    kinds::step(&db, q, "run-1", false, None, None).unwrap();
    assert_eq!(kinds::progress(&meta_of(&db, q)), (1, 3));
    assert!(first_body(&db, q).contains("- [ ] **Start a new run"));
}

#[test]
fn the_agent_can_keep_talking_while_a_packet_is_open_but_not_after_it_is_finished() {
    let (_d, db) = board();
    let c = caller("s1");
    let q = packet(&db, &c);
    kinds::step(&db, q, "run-1", true, None, None).unwrap();
    for n in 1..=3 {
        let r = call(&db, &c, "post_message", json!({"channel": "question", "subject": "x", "body": format!("update {n}"), "author": "builder", "thread_id": q}));
        assert_eq!(r["ok"], true, "update {n} while open: {r}");
    }
    kinds::note(&db, q, "also, the second host is down").unwrap();
    assert_eq!(status(&db, q), "open");
    kinds::finish(&db, q).unwrap();
    assert_eq!(status(&db, q), "answered");
    let body = db.get_thread(q).unwrap()["messages"].as_array().unwrap().last().unwrap()["body"].as_str().unwrap().to_string();
    assert!(body.contains("Finished with 2 step(s) not ticked") && body.contains("Pick how to proceed"), "{body}");
    assert_eq!(db.scalar_i64("SELECT COUNT(*) FROM acks WHERE state='pending'", &[]).unwrap(), Some(1), "finishing queues the agent's ack like an answer");
    assert_eq!(db.open_questions(false).unwrap().len(), 0);
    let first = call(&db, &c, "post_message", json!({"channel": "question", "subject": "x", "body": "thanks", "author": "builder", "thread_id": q}));
    assert_eq!(first["ok"], true, "one closing note");
    let second = call(&db, &c, "post_message", json!({"channel": "question", "subject": "x", "body": "and another", "author": "builder", "thread_id": q}));
    assert!(second["error"].as_str().unwrap().contains("one question and one answer"));
    assert!(kinds::step(&db, q, "run-1", true, None, None).unwrap_err().to_string().contains("not open"));
}

#[test]
fn finishing_with_every_step_ticked_says_so() {
    let (_d, db) = board();
    let q = packet(&db, &caller("s1"));
    kinds::step(&db, q, "run-1", true, None, None).unwrap();
    kinds::step(&db, q, "which", true, Some("agent retries"), None).unwrap();
    kinds::step(&db, q, "note", true, None, Some("none")).unwrap();
    kinds::finish(&db, q).unwrap();
    let last = db.get_thread(q).unwrap()["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["body"], "Done: every step is ticked.");
    assert_eq!((last["author"].as_str().unwrap(), last["author_kind"].as_str().unwrap()), (JOHN, HUMAN));
}

#[test]
fn the_agent_revises_its_packet_and_johns_ticks_on_surviving_steps_stay() {
    let (_d, db) = board();
    let c = caller("s1");
    let q = packet(&db, &c);
    kinds::step(&db, q, "run-1", true, None, None).unwrap();
    kinds::step(&db, q, "which", true, Some("I run it"), None).unwrap();
    let new_steps = json!([
        {"id": "run-1", "title": "Start a new run with MIN_FREE_PERCENT empty"},
        {"id": "creds", "title": "Paste the service account name", "input": "note"}
    ]);
    let r = call(
        &db,
        &c,
        "update_question",
        json!({"thread_id": q, "author": "builder", "packet": {"steps": new_steps, "next": "I wait for the name."}, "summary": "Dropped the choice: you will run it. Added the account name."}),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["version"], 2);
    let meta = meta_of(&db, q);
    assert_eq!(kinds::progress(&meta), (1, 2), "run-1 stays ticked, the removed step's state is gone");
    assert!(meta["state"].get("which").is_none());
    assert_eq!(meta["changed"], json!(["steps", "next"]));
    let body = first_body(&db, q);
    assert!(body.contains("- [x] **Start a new run") && body.contains("Paste the service account name") && body.contains("I wait for the name."));
    let last = db.get_thread(q).unwrap()["messages"].as_array().unwrap().last().unwrap().clone();
    assert!(last["body"].as_str().unwrap().starts_with("Revised (version 2): Dropped the choice"));
    assert_eq!(status(&db, q), "open");
}

#[test]
fn only_the_asking_agent_can_revise_and_only_a_valid_open_packet() {
    let (_d, db) = board();
    let c = caller("s1");
    let q = packet(&db, &c);
    let other = call(&db, &caller("s2"), "update_question", json!({"thread_id": q, "author": "someone-else", "packet": {"next": "x"}}));
    assert!(other["error"].as_str().unwrap().contains("only builder, who asked"));
    let bad = call(&db, &c, "update_question", json!({"thread_id": q, "author": "builder", "packet": {"deadline": {"date": "soon", "reason": "x"}}}));
    assert!(bad["error"].as_str().unwrap().contains("would not be valid"));
    let unknown = call(&db, &c, "update_question", json!({"thread_id": q, "author": "builder", "packet": {"colour": "red"}}));
    assert!(unknown["error"].as_str().unwrap().contains("not a part of a packet"));
    let none = call(&db, &c, "update_question", json!({"thread_id": q, "author": "builder", "packet": {}}));
    assert!(none["error"].is_string());
    assert_eq!(meta_of(&db, q)["rev"], 1, "refused changes leave the packet as it was");
    let ask = call(&db, &c, "ask_human", json!({"subject": "x", "body": "ok?", "author": "builder"}))["thread_id"].as_i64().unwrap();
    assert!(call(&db, &c, "update_question", json!({"thread_id": ask, "author": "builder", "packet": {"next": "x"}}))["error"].as_str().unwrap().contains("not a packet"));
    kinds::finish(&db, q).unwrap();
    assert!(call(&db, &c, "update_question", json!({"thread_id": q, "author": "builder", "packet": {"next": "x"}}))["error"].as_str().unwrap().contains("not open"));
}

#[test]
fn a_permission_proposal_is_a_permission_question_and_old_ones_are_still_recognised() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (d, db) = board();
    std::env::set_var("AGENTDESK_DATA", d.path());
    let r = call(
        &db,
        &caller("a"),
        "propose_permission_rule",
        json!({"rule": "Bash(cargo fmt:*)", "reason": "formatting", "author": "b", "scope": "project", "project_dir": d.path().to_string_lossy()}),
    );
    let tid = r["thread_id"].as_i64().unwrap();
    assert_eq!(kinds::kind_of(&db, tid).unwrap(), "permission");
    assert_eq!(meta_of(&db, tid)["type"], "permission");
    // one made before kinds existed: no type in its meta, but a row in permission_proposals
    let old = db.start_thread("question", "permission rule: Bash(old:*)", "b", "agent", "Approve it?", None, None).unwrap();
    assert_eq!(kinds::kind_of(&db, old).unwrap(), "ask");
    db.exec(
        "INSERT INTO permission_proposals (rule, scope, project_dir, reason, blocked, requested_by, thread_id, status, created_ts) VALUES ('Bash(old:*)','user',NULL,'x',NULL,'b',:t,'pending','2026-01-01T00:00:00+00:00')",
        &[(":t", &old)],
    )
    .unwrap();
    assert_eq!(kinds::kind_of(&db, old).unwrap(), "permission");
    let long = format!("{} approve?", "reason ".repeat(600));
    assert!(db.reply(old, "b", "agent", &long, None, None).is_ok(), "a permission question is not held to 500 words");
}

#[test]
fn the_help_tool_and_the_two_new_tools_are_on_offer() {
    let (_d, db) = board();
    let names = crate::tools::tool_names();
    assert!(names.contains(&"update_question") && names.contains(&"question_help"));
    let help = crate::tools::call(&db, &caller("s"), "question_help", &json!({}));
    for part in ["meta.packet", "run: John must run a script", "permission: do not ask for it with ask_human", "Done when", "Never send an invite yourself"] {
        assert!(help.contains(part), "'{part}' missing from question_help");
    }
}

#[test]
fn the_steps_block_can_be_cut_out_for_a_view_that_draws_them_live() {
    let md = kinds::render_packet("Intro.", &kinds::validate_packet(&spec()).unwrap(), &json!({}));
    assert!(md.contains("### Your steps"));
    let cut = kinds::without_steps(&md);
    assert!(!cut.contains("### Your steps") && !cut.contains("MIN_FREE_PERCENT") && cut.contains("### What the agent does next") && cut.contains("### Deadline"));
    assert_eq!(kinds::without_steps("no markers here"), "no markers here");
}

#[test]
fn a_note_is_for_a_packet_or_a_permission_question_and_never_closes_either() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (d, db) = board();
    std::env::set_var("AGENTDESK_DATA", d.path());
    let c = caller("s1");
    let perm = call(
        &db,
        &c,
        "propose_permission_rule",
        json!({"rule": "Bash(cargo fmt:*)", "reason": "format", "author": "b", "scope": "project", "project_dir": d.path().to_string_lossy()}),
    )["thread_id"]
        .as_i64()
        .unwrap();
    let mid = kinds::note(&db, perm, "what does this allow exactly?").unwrap();
    assert!(mid > 0);
    assert_eq!(status(&db, perm), "open", "the rule is still waiting for Approve or Reject");
    let ask = call(&db, &c, "ask_human", json!({"subject": "x", "body": "ok?", "author": "b"}))["thread_id"].as_i64().unwrap();
    assert!(kinds::note(&db, ask, "hi").unwrap_err().to_string().contains("takes an answer, not a note"));
    db.john_replies(perm, "decided").unwrap();
    assert!(kinds::note(&db, perm, "late").unwrap_err().to_string().contains("not open"));
}

#[test]
fn tags_and_limits_by_kind() {
    assert_eq!([kinds::tag("ask"), kinds::tag("run"), kinds::tag("permission"), kinds::tag("packet"), kinds::tag("anything")], ["ASK", "RUN", "PERM", "PKT", "ASK"]);
    assert!(!kinds::exempt_from_limits("ask") && kinds::exempt_from_limits("run") && kinds::exempt_from_limits("permission") && kinds::exempt_from_limits("packet"));
    assert!(kinds::stays_open("packet") && kinds::stays_open("run") && !kinds::stays_open("ask") && !kinds::stays_open("permission"));
    assert_eq!(kinds::kind_in(None), "ask");
    assert_eq!(kinds::kind_in(Some(&json!({"type": "packet"}))), "packet");
    assert_eq!(kinds::kind_in(Some(&json!({"type": "weird"}))), "ask");
}
