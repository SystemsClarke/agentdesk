//! The Concierge and its dispatcher: turning it on, who gets a worker and in what order, triage, `after`, `eager`, recurring items.

use crate::concierge;
use crate::db::{Db, AGENT};
use crate::dispatcher::{self, recur};
use crate::identity::Caller;
use crate::tests_identities::Env;
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

fn john() -> Caller {
    std::env::remove_var("AGENTDESK_IDENTITY");
    Caller { session_id: None, env_author: None, cwd: None, harness: None, pid: 1 }
}

fn agent() -> Caller {
    std::env::set_var("AGENTDESK_IDENTITY", "someone");
    Caller { session_id: Some("sid".into()), env_author: Some("someone".into()), cwd: None, harness: Some("claude-code".into()), pid: 2 }
}

fn item(db: &Db, subject: &str, meta: Value) -> i64 {
    db.start_thread("work", subject, "john", AGENT, "do the thing", Some(&meta), None).unwrap()
}

fn status(db: &Db, id: i64) -> String {
    db.scalar_str("SELECT status FROM threads WHERE id=:i", &[(":i", &id)]).unwrap().unwrap()
}

fn on(e: &Env) {
    concierge::toggle(&e.db, &john(), e.folder(), Some(true)).unwrap();
}

#[test]
fn only_john_turns_the_concierge_on_and_off_and_it_survives_a_second_on() {
    let e = Env::new();
    assert!(concierge::toggle(&e.db, &agent(), e.folder(), Some(true)).is_err());
    assert_eq!(concierge::state(&e.db).unwrap()["state"], "off");
    let s = concierge::toggle(&e.db, &john(), e.folder(), Some(true)).unwrap();
    assert_eq!((s["on"].clone(), s["lead"].clone()), (json!(true), json!("concierge-lead")));
    assert_eq!(e.db.scalar_i64("SELECT standing FROM goals WHERE name='concierge'", &[]).unwrap(), Some(1));
    concierge::toggle(&e.db, &john(), e.folder(), Some(true)).unwrap(); // idempotent: no second goal, no second lead
    assert_eq!(e.db.scalar_i64("SELECT COUNT(*) FROM goals", &[]).unwrap(), Some(1));
    assert!(concierge::toggle(&e.db, &agent(), e.folder(), Some(false)).is_err());
    let s = concierge::toggle(&e.db, &john(), e.folder(), Some(false)).unwrap();
    assert_eq!(s["on"], json!(false));
    assert_eq!(s["state"], "stopped");
    assert!(concierge::toggle(&e.db, &john(), e.folder(), Some(true)).unwrap()["on"] == json!(true), "turned back on after a stop");
}

#[test]
fn a_dispatcher_with_the_concierge_off_starts_nothing() {
    let e = Env::new();
    let id = item(&e.db, "do it", json!({}));
    assert!(dispatcher::tick(&e.db, None).unwrap().is_empty());
    assert_eq!(status(&e.db, id), "open");
}

#[test]
fn the_dispatcher_hands_items_out_by_priority_then_id_up_to_the_cap_and_picks_the_model() {
    let e = Env::new();
    on(&e);
    let a = item(&e.db, "refactor the parser", json!({}));
    let b = item(&e.db, "rename the helper", json!({"priority": 1}));
    let c = item(&e.db, "urgent thing", json!({"priority": 0, "model": "opus"}));
    let d = item(&e.db, "later", json!({"priority": 4}));
    let started = dispatcher::tick(&e.db, None).unwrap();
    assert_eq!(started, vec![c, b, a], "priority 0, then 1, then 2; the cap is 3");
    assert_eq!(status(&e.db, d), "open");
    let model = |id: i64| e.db.scalar_str("SELECT i.model FROM goal_members m JOIN identities i ON i.name=m.identity WHERE m.work_id=:w", &[(":w", &id)]).unwrap().unwrap();
    assert_eq!((model(c), model(b), model(a)), ("opus".to_string(), "haiku".to_string(), "sonnet".to_string()));
    assert_eq!(status(&e.db, a), "claimed");
    let assignee = e.db.scalar_str("SELECT json_extract(meta,'$.assignee') FROM threads WHERE id=:i", &[(":i", &a)]).unwrap().unwrap();
    assert!(assignee.starts_with("concierge-w"), "the claim was handed to the worker: {assignee}");
    assert!(dispatcher::tick(&e.db, None).unwrap().is_empty(), "full, and workers still queued");
}

#[test]
fn claim_anyone_triage_eager_after_and_due_keep_an_item_off_the_dispatcher() {
    let e = Env::new();
    on(&e);
    let blocker = item(&e.db, "first", json!({"claim": "anyone"}));
    let anyone = item(&e.db, "for a human claim", json!({"claim": "anyone"}));
    let triage = item(&e.db, "vague", json!({"triage": true}));
    let eager = item(&e.db, "spare budget only", json!({"eager": true}));
    let later = item(&e.db, "second", json!({"after": [blocker]}));
    let future = item(&e.db, "not yet", json!({"due": "2999-01-01T00:00:00Z"}));
    assert!(dispatcher::tick(&e.db, None).unwrap().is_empty());
    for id in [anyone, triage, eager, later, future] {
        assert_eq!(status(&e.db, id), "open");
    }
    assert!(e.db.claim_task(blocker, "x").unwrap() && e.db.complete_task(blocker, "x").unwrap());
    assert_eq!(dispatcher::tick(&e.db, None).unwrap(), vec![later], "once its `after` is done");
    let future_run = Utc.with_ymd_and_hms(3000, 1, 1, 0, 0, 0).unwrap();
    e.db.exec("UPDATE goal_members SET goal='concierge'", &[]).unwrap();
    // The first worker is still queued, so nothing more starts: free it and let the due date pass.
    e.db.exec("UPDATE identities SET state='running'", &[]).unwrap();
    assert_eq!(dispatcher::tick(&e.db, Some(future_run)).unwrap(), vec![future]);
}

#[test]
fn an_item_that_three_workers_failed_goes_to_the_lead_marked_triage() {
    let e = Env::new();
    on(&e);
    let id = item(&e.db, "stubborn", json!({"attempts": 3}));
    assert!(dispatcher::tick(&e.db, None).unwrap().is_empty());
    assert_eq!(status(&e.db, id), "open");
    let meta = e.db.scalar_str("SELECT meta FROM threads WHERE id=:i", &[(":i", &id)]).unwrap().unwrap();
    assert!(meta.contains("\"triage\": true"), "{meta}");
    let said = e.db.scalar_str("SELECT body FROM messages WHERE thread_id=:i ORDER BY id DESC LIMIT 1", &[(":i", &id)]).unwrap().unwrap();
    assert!(said.contains("3 workers ended without completing it"), "{said}");
    assert_eq!(crate::goals::measure_internal(&e.db, "internal:open_triage").unwrap(), 1.0);
}

#[test]
fn a_finished_recurring_item_posts_its_next_run_once() {
    let e = Env::new();
    on(&e);
    let id = item(&e.db, "morning report", json!({"recur": "60m", "priority": 1}));
    let t = dispatcher::tick(&e.db, None).unwrap();
    assert_eq!(t, vec![id]);
    let worker = e.db.scalar_str("SELECT identity FROM goal_members WHERE work_id=:w", &[(":w", &id)]).unwrap().unwrap();
    assert!(e.db.complete_task(id, &worker).unwrap());
    e.db.exec("DELETE FROM goal_members", &[]).unwrap(); // the worker retired
    dispatcher::tick(&e.db, None).unwrap();
    dispatcher::tick(&e.db, None).unwrap();
    let all = e.db.rows("SELECT id, status, meta FROM threads WHERE channel='work' ORDER BY id", &[]).unwrap();
    assert_eq!(all.len(), 2, "one renewal, however many ticks");
    let next = &all[1];
    assert_eq!(next["status"], "open");
    let m: Value = serde_json::from_str(next["meta"].as_str().unwrap()).unwrap();
    assert_eq!((m["recur"].clone(), m["priority"].clone()), (json!("60m"), json!(1)));
    let due = chrono::DateTime::parse_from_rfc3339(m["due"].as_str().unwrap()).unwrap();
    assert!(due > Utc::now() + chrono::Duration::minutes(55), "an hour after the last run: {due}");
    assert!(e.db.scalar_str("SELECT meta FROM threads WHERE id=:i", &[(":i", &id)]).unwrap().unwrap().contains("renewed"));
}

#[test]
fn recur_accepts_times_and_intervals_and_computes_the_next_run() {
    for ok in ["09:30", "15m", "2h", " 90 "] {
        assert!(recur::valid(ok), "{ok}");
    }
    for bad in ["", "soon", "0m", "25:00", "9:3"] {
        assert!(!recur::valid(bad), "{bad}");
    }
    let t = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
    assert_eq!(recur::next("30m", t), Some(t + chrono::Duration::minutes(30)));
    assert_eq!(recur::next("2h", t), Some(t + chrono::Duration::hours(2)));
    assert_eq!(recur::first("30m", t), Some(t), "an interval starts at once");
    let daily = recur::next("09:30", t).unwrap();
    assert!(daily > t && daily <= t + chrono::Duration::hours(25), "the next 09:30, local: {daily}");
}
