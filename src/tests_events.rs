//! The event log: written in the same transaction as its row, gapless per actor, a clock that never goes backward, and a backfill
//! that runs once.

use crate::db::{Db, AGENT, HUMAN, JOHN};
use crate::events::{hlc_next, hlc_step, uuid7, FAIL};
use serde_json::Value;

fn board() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.init().unwrap();
    (dir, db)
}

fn count(db: &Db, sql: &str) -> i64 {
    db.scalar_i64(sql, &[]).unwrap().unwrap_or(0)
}

fn kinds(db: &Db) -> Vec<String> {
    db.rows("SELECT kind FROM events ORDER BY seq", &[]).unwrap().iter().map(|r| r["kind"].as_str().unwrap().to_string()).collect()
}

#[test]
fn an_event_is_written_in_the_same_transaction_as_its_row() {
    let (_d, db) = board();
    let tid = db.start_thread("discussion", "s", "alpha", AGENT, "first", None, None).unwrap();
    assert_eq!(kinds(&db), ["thread.created", "msg.posted"]);
    let (msgs, evs, threads) = (count(&db, "SELECT COUNT(*) FROM messages"), count(&db, "SELECT COUNT(*) FROM events"), count(&db, "SELECT COUNT(*) FROM threads"));

    FAIL.with(|f| f.set(true));
    let replied = db.reply(tid, "alpha", AGENT, "second", None, None);
    let started = db.start_thread("discussion", "t", "alpha", AGENT, "x", None, None);
    FAIL.with(|f| f.set(false));
    assert!(replied.is_err() && started.is_err(), "a failed event fails the write");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), msgs, "the message was rolled back with its event");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM threads"), threads, "the thread too");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM events"), evs);

    assert!(db.reply(9999, "alpha", AGENT, "nowhere", None, None).is_err(), "a failed write");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM events"), evs, "leaves no event");

    let mid = db.reply(tid, "alpha", AGENT, "second", None, None).unwrap();
    let ev = db.events_after(evs, 10).unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0]["kind"].as_str(), ev[0]["ref"].as_str(), ev[0]["body"].as_str()), (Some("msg.posted"), Some(format!("message:{mid}").as_str()), Some("second")));
    assert_eq!(ev[0]["data"]["message_id"], mid);
    assert_eq!(ev[0]["id"].as_str().unwrap().len(), 36);
}

#[test]
fn each_write_path_records_its_kind() {
    let (_d, db) = board();
    let tid = db.start_thread("question", "ship?", "alpha", AGENT, "Ship it?", None, None).unwrap();
    db.john_replies(tid, "yes").unwrap();
    assert_eq!(db.deliver_pending_acks("alpha", "got it").unwrap().len(), 1);
    let john = db.scalar_i64("SELECT MAX(id) FROM messages WHERE author_kind='human'", &[]).unwrap().unwrap();
    db.record_delivery(john, "hook", "injected", "2026-01-01T00:00:00+00:00").unwrap();
    assert!(db.archive(tid).unwrap());
    db.set_first_body(tid, "Ship it? (revised)").unwrap();
    assert_eq!(kinds(&db), ["thread.created", "msg.posted", "msg.posted", "thread.status", "ack", "ack", "msg.posted", "delivery", "thread.status", "packet.revised"]);
    let acks = db.rows("SELECT actor, data FROM events WHERE kind='ack' ORDER BY seq", &[]).unwrap();
    assert!(acks[0]["data"].as_str().unwrap().contains("pending") && acks[1]["data"].as_str().unwrap().contains("posted"), "{acks:?}");
    let receipt = db.rows("SELECT data FROM events WHERE kind='msg.posted' ORDER BY seq DESC LIMIT 1", &[]).unwrap();
    assert!(receipt[0]["data"].as_str().unwrap().contains("\"kind\":\"ack\""), "the meta kind is in data: {receipt:?}");
    let answered = db.row("SELECT actor, actor_kind, data FROM events WHERE kind='thread.status' ORDER BY seq LIMIT 1", &[]).unwrap().unwrap();
    assert_eq!((answered["actor"].as_str(), answered["actor_kind"].as_str()), (Some(JOHN), Some(HUMAN)));
    assert!(answered["data"].as_str().unwrap().contains("answered"));
}

#[test]
fn actor_seq_is_gapless_and_monotonic_per_actor() {
    let (_d, db) = board();
    let tid = db.start_thread("discussion", "s", "alpha", AGENT, "a1", None, None).unwrap();
    for (who, body) in [("beta", "b1"), ("alpha", "a2"), ("alpha", "a3"), ("beta", "b2"), ("alpha", "a4")] {
        db.reply(tid, who, AGENT, body, None, None).unwrap();
    }
    for who in ["alpha", "beta"] {
        let seqs: Vec<i64> =
            db.rows("SELECT actor_seq FROM events WHERE actor=:a ORDER BY seq", &[(":a", &who)]).unwrap().iter().map(|r| r["actor_seq"].as_i64().unwrap()).collect();
        assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>(), "{who}");
    }
    assert_eq!(count(&db, "SELECT COUNT(DISTINCT origin) FROM events"), 1);
    let hlcs: Vec<i64> = db.rows("SELECT hlc FROM events ORDER BY seq", &[]).unwrap().iter().map(|r| r["hlc"].as_i64().unwrap()).collect();
    assert!(hlcs.windows(2).all(|w| w[0] < w[1]), "{hlcs:?}");
    let ids: Vec<String> = db.rows("SELECT id FROM events ORDER BY seq", &[]).unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids sort in time order: {ids:?}");
}

#[test]
fn the_clock_never_goes_backward_when_the_wall_clock_does() {
    assert_eq!(hlc_step(0, 1000), 1000 << 16);
    assert_eq!(hlc_step(1000 << 16, 999), (1000 << 16) + 1, "a clock behind the last value counts on from it");
    let t = 4_000_000_000_000u64; // past any real clock, so another test's values do not interfere
    let mut seen = vec![hlc_next(0, t)];
    for now in [t - 60_000, t - 1, t, t + 1, t - 5_000] {
        seen.push(hlc_next(0, now));
    }
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
    assert!(seen[2] >> 16 == t && seen[4] >> 16 == t + 1, "{seen:?}");
    let (a, b) = (uuid7(seen[0]), uuid7(seen[1]));
    assert!(a < b && a.as_bytes()[14] == b'7' && "89ab".contains(a.as_bytes()[19] as char), "{a} {b}");
}

#[test]
fn backfill_makes_one_event_per_row_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    {
        let db = Db::open(&path).unwrap();
        db.init().unwrap();
        let q = db.start_thread("question", "ship?", "alpha", AGENT, "Ship it?", None, None).unwrap();
        db.john_replies(q, "yes").unwrap();
        db.deliver_pending_acks("alpha", "got it").unwrap();
        let d = db.start_thread("discussion", "notes", "beta", AGENT, "n1", None, None).unwrap();
        db.reply(d, "alpha", AGENT, "n2", None, None).unwrap();
        db.exec("INSERT INTO deliveries(message_id, method, state, detail, ts) SELECT MAX(id), 'hook', 'injected', NULL, '2026-01-02T00:00:00+00:00' FROM messages WHERE author_kind='human'", &[]).unwrap();
        db.exec(
            "INSERT INTO permission_proposals (rule, scope, reason, requested_by, thread_id, status, created_ts, decided_ts) VALUES ('Bash(x:*)','user','x','beta',:t,'approved','2026-01-01T00:00:00+00:00','2026-01-01T00:01:00+00:00')",
            &[(":t", &q)],
        )
        .unwrap();
        // the board as an older build left it: no log, an older schema version
        db.conn.execute_batch("DROP TABLE events; DROP TABLE event_marks; PRAGMA user_version=0;").unwrap();
    }
    let db = Db::open(&path).unwrap();
    db.init().unwrap();
    let per = |kind: &str| db.scalar_i64("SELECT COUNT(*) FROM events WHERE kind=:k", &[(":k", &kind)]).unwrap().unwrap();
    assert_eq!(per("msg.posted"), count(&db, "SELECT COUNT(*) FROM messages"));
    assert_eq!(per("thread.created"), count(&db, "SELECT COUNT(*) FROM threads"));
    assert_eq!(per("delivery"), count(&db, "SELECT COUNT(*) FROM deliveries"));
    assert_eq!(per("ack"), count(&db, "SELECT COUNT(*) FROM acks"));
    assert_eq!((per("approval.requested"), per("approval.decided")), (1, 1));
    let total = count(&db, "SELECT COUNT(*) FROM events");
    let ts: Vec<String> = db.rows("SELECT ts FROM events ORDER BY seq", &[]).unwrap().iter().map(|r| r["ts"].as_str().unwrap().to_string()).collect();
    assert!(ts.windows(2).all(|w| w[0] <= w[1]), "in time order: {ts:?}");

    assert_eq!(db.backfill_events().unwrap(), 0, "a second run adds nothing");
    db.conn.execute_batch("PRAGMA user_version=0;").unwrap();
    db.init().unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM events"), total);

    // the log goes on from the backfill: the next write is one more event, its actor's next number
    let before = db.scalar_i64("SELECT MAX(actor_seq) FROM events WHERE actor='alpha'", &[]).unwrap().unwrap();
    db.reply(1, "alpha", AGENT, "later", None, None).unwrap();
    let last: Value = db.events_after(total, 10).unwrap().remove(0);
    assert_eq!(last["actor_seq"].as_i64(), Some(before + 1));
}
