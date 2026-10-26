//! The event log: an append-only shadow record of what happens on the board, written in the same transaction as the row it
//! records, so a failed write leaves no event and a failed event rolls the write back. Events never conflict: each is keyed by
//! (origin, actor, actor_seq), so two boards replicate by comparing per-writer counters (a version vector), and `hlc` (a hybrid
//! logical clock: milliseconds << 16, plus a counter) orders them. Nothing reads the log to decide anything yet.

use crate::db::Db;
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// One event to append. `ts` defaults to now (read, not advanced, on a pinned test clock).
#[derive(Default)]
pub struct NewEvent {
    pub kind: String,
    pub actor: String,
    pub actor_kind: String,
    pub thread_id: Option<i64>,
    pub reference: Option<String>,
    pub body: Option<String>,
    pub data: Option<Value>,
    pub ts: Option<String>,
}

impl NewEvent {
    pub fn new(kind: &str, actor: &str, actor_kind: &str) -> NewEvent {
        NewEvent { kind: kind.into(), actor: actor.into(), actor_kind: actor_kind.into(), ..Default::default() }
    }
    pub fn thread(mut self, id: i64) -> Self {
        self.thread_id = Some(id);
        self
    }
    pub fn reference(mut self, r: String) -> Self {
        self.reference = Some(r);
        self
    }
    pub fn body(mut self, b: &str) -> Self {
        self.body = Some(b.into());
        self
    }
    pub fn data(mut self, d: Value) -> Self {
        self.data = Some(d);
        self
    }
    pub fn ts(mut self, ts: &str) -> Self {
        self.ts = Some(ts.into());
        self
    }

    /// `msg.posted` for a message row; its meta's `kind` (read-receipt, ack, handoff, phoenix...) goes in `data`.
    #[allow(clippy::too_many_arguments)]
    pub fn message(id: i64, thread_id: i64, author: &str, author_kind: &str, body: &str, reply_to: Option<i64>, meta: Option<&str>, ts: &str) -> NewEvent {
        let mut d = Map::new();
        d.insert("message_id".into(), json!(id));
        if let Some(r) = reply_to {
            d.insert("reply_to".into(), json!(r));
        }
        if let Some(k) = crate::db::parse_obj(meta).and_then(|m| m.get("kind").cloned()) {
            d.insert("kind".into(), k);
        }
        NewEvent::new("msg.posted", author, author_kind).thread(thread_id).reference(format!("message:{id}")).body(body).data(Value::Object(d)).ts(ts)
    }

    /// `ack`: the acknowledgement John's reply `message_id` is owed by `agent`, queued (`pending`) or posted.
    pub fn ack(message_id: i64, thread_id: Option<i64>, agent: &str, state: &str, actor: &str, actor_kind: &str, ts: &str) -> NewEvent {
        let e = NewEvent::new("ack", actor, actor_kind).reference(format!("message:{message_id}")).data(json!({"state": state, "agent": agent})).ts(ts);
        match thread_id {
            Some(t) => e.thread(t),
            None => e,
        }
    }

    /// `delivery`: how John's reply `message_id` reached its agent (hook, watcher, wake) and in what state.
    pub fn delivery(message_id: i64, thread_id: Option<i64>, method: &str, state: &str, ts: &str) -> NewEvent {
        let e = NewEvent::new("delivery", "agentdesk", "system").reference(format!("message:{message_id}")).data(json!({"method": method, "state": state})).ts(ts);
        match thread_id {
            Some(t) => e.thread(t),
            None => e,
        }
    }
}

static LAST_HLC: AtomicU64 = AtomicU64::new(0);

/// The next hybrid-logical-clock value after `last` at wall time `now_ms`: the clock's own value when it is ahead, else `last + 1`,
/// so it never goes backward when the wall clock steps back.
pub fn hlc_step(last: u64, now_ms: u64) -> u64 {
    (now_ms << 16).max(last + 1)
}

/// The process's next HLC value, also past `observed` (the newest value already in the log, which another process may have written).
pub fn hlc_next(observed: u64, now_ms: u64) -> u64 {
    let mut prev = LAST_HLC.load(Ordering::SeqCst);
    loop {
        let next = hlc_step(prev.max(observed), now_ms);
        match LAST_HLC.compare_exchange(prev, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn random_u64(salt: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(salt);
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.finish()
}

/// A UUIDv7-shaped id from an HLC value: 48 bits of milliseconds, then the HLC's low 12 counter bits, then 62 random bits, so ids sort
/// in HLC order.
pub fn uuid7(hlc: u64) -> String {
    let ms = hlc >> 16;
    let r = random_u64(hlc);
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70 | ((hlc >> 8) & 0x0f) as u8;
    b[7] = (hlc & 0xff) as u8;
    b[8..].copy_from_slice(&r.to_be_bytes());
    b[8] = 0x80 | (b[8] & 0x3f);
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn ms_of(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts).map(|t| t.timestamp_millis().max(0) as u64).unwrap_or(0)
}

#[cfg(test)]
thread_local! {
    /// A test makes the next appends fail, to prove the row they record is rolled back with them.
    pub static FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Db {
    fn events_on(&self) -> bool {
        if !self.events_ready.get() {
            let have = self.scalar_i64("SELECT 1 FROM sqlite_master WHERE type='table' AND name='events'", &[]).ok().flatten().is_some();
            self.events_ready.set(have);
        }
        self.events_ready.get()
    }

    /// This board's stable node id (the `origin` of every event it writes), made once.
    pub fn node_id(&self) -> Result<String> {
        if let Some(id) = self.scalar_str("SELECT value FROM event_marks WHERE name='node'", &[])? {
            return Ok(id);
        }
        let id = format!("{:016x}", random_u64(wall_ms()));
        self.exec("INSERT OR IGNORE INTO event_marks (name, value) VALUES ('node', :v)", &[(":v", &id)])?;
        Ok(self.scalar_str("SELECT value FROM event_marks WHERE name='node'", &[])?.unwrap_or(id))
    }

    /// Appends one event in the caller's transaction (or its own). A board without the `events` table yet (an older build made it and
    /// none of this version has opened it) gets none: 0 is returned.
    pub fn append_event(&self, ev: NewEvent) -> Result<i64> {
        if !self.events_on() {
            return Ok(0);
        }
        #[cfg(test)]
        if FAIL.with(|f| f.get()) {
            anyhow::bail!("event log: forced failure");
        }
        self.tx(|| {
            let origin = self.node_id()?;
            let seq = self.scalar_i64("SELECT COALESCE(MAX(actor_seq), 0) + 1 FROM events WHERE origin=:o AND actor=:a", &[(":o", &origin), (":a", &ev.actor)])?.unwrap_or(1);
            let last = self.scalar_i64("SELECT hlc FROM events ORDER BY seq DESC LIMIT 1", &[])?.unwrap_or(0).max(0) as u64;
            let hlc = hlc_next(last, wall_ms());
            let ts = ev.ts.clone().unwrap_or_else(crate::db::peek_iso);
            self.exec(
                "INSERT INTO events (id, ts, hlc, origin, actor, actor_kind, actor_seq, kind, thread_id, ref, body, data) \
                 VALUES (:id,:ts,:hlc,:o,:a,:ak,:as,:k,:t,:r,:b,:d)",
                &[
                    (":id", &uuid7(hlc)),
                    (":ts", &ts),
                    (":hlc", &(hlc as i64)),
                    (":o", &origin),
                    (":a", &ev.actor),
                    (":ak", &ev.actor_kind),
                    (":as", &seq),
                    (":k", &ev.kind),
                    (":t", &ev.thread_id),
                    (":r", &ev.reference),
                    (":b", &ev.body),
                    (":d", &ev.data.as_ref().map(|d| d.to_string())),
                ],
            )?;
            Ok(self.conn.last_insert_rowid())
        })
    }

    /// `INSERT OR REPLACE` a delivery of John's reply and its event, in one transaction.
    pub fn record_delivery(&self, message_id: i64, method: &str, state: &str, ts: &str) -> Result<()> {
        self.tx(|| {
            self.exec(
                "INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES(:m, :how, :st, NULL, :ts)",
                &[(":m", &message_id), (":how", &method), (":st", &state), (":ts", &ts)],
            )?;
            let tid = self.scalar_i64("SELECT thread_id FROM messages WHERE id=:m", &[(":m", &message_id)])?;
            self.append_event(NewEvent::delivery(message_id, tid, method, state, ts))?;
            Ok(())
        })
    }

    /// Events with `seq > after`, oldest first, `data` parsed.
    pub fn events_after(&self, after: i64, limit: i64) -> Result<Vec<Value>> {
        if !self.events_on() {
            return Ok(vec![]);
        }
        let mut rows = self.rows("SELECT * FROM events WHERE seq > :a ORDER BY seq LIMIT :l", &[(":a", &after), (":l", &limit)])?;
        for r in rows.iter_mut() {
            if let Some(d) = r["data"].as_str().and_then(|d| serde_json::from_str::<Value>(d).ok()) {
                r["data"] = d;
            }
        }
        Ok(rows)
    }

    pub fn max_event_seq(&self) -> i64 {
        if !self.events_on() {
            return 0;
        }
        self.scalar_i64("SELECT MAX(seq) FROM events", &[]).ok().flatten().unwrap_or(0)
    }

    /// One time, in `init`'s transaction: an event for every existing thread, message, delivery, ack and permission proposal, in time
    /// order. The `backfill` mark makes a second run add nothing.
    pub fn backfill_events(&self) -> Result<usize> {
        if self.scalar_i64("SELECT 1 FROM event_marks WHERE name='backfill'", &[])?.is_some() {
            return Ok(0);
        }
        let origin = self.node_id()?;
        // (ts, order within one second, row id, event)
        let mut all: Vec<(String, u8, i64, NewEvent)> = Vec::new();
        let text = |v: &Value| v.as_str().unwrap_or("").to_string();
        for t in self.rows(
            "SELECT t.id, t.created_ts, t.channel, t.subject, t.opened_by, \
             COALESCE((SELECT author_kind FROM messages WHERE thread_id=t.id ORDER BY id LIMIT 1), 'agent') AS kind FROM threads t",
            &[],
        )? {
            let id = t["id"].as_i64().unwrap_or(0);
            let ev = NewEvent::new("thread.created", &text(&t["opened_by"]), &text(&t["kind"]))
                .thread(id)
                .data(json!({"channel": t["channel"], "subject": t["subject"]}))
                .ts(&text(&t["created_ts"]));
            all.push((text(&t["created_ts"]), 0, id, ev));
        }
        for m in self.rows("SELECT id, ts, thread_id, author, author_kind, body, reply_to, meta FROM messages", &[])? {
            let id = m["id"].as_i64().unwrap_or(0);
            let ev = NewEvent::message(
                id,
                m["thread_id"].as_i64().unwrap_or(0),
                &text(&m["author"]),
                &text(&m["author_kind"]),
                &text(&m["body"]),
                m["reply_to"].as_i64(),
                m["meta"].as_str(),
                &text(&m["ts"]),
            );
            all.push((text(&m["ts"]), 1, id, ev));
        }
        for d in self.rows("SELECT d.message_id, d.method, d.state, d.ts, m.thread_id FROM deliveries d LEFT JOIN messages m ON m.id=d.message_id", &[])? {
            let id = d["message_id"].as_i64().unwrap_or(0);
            all.push((text(&d["ts"]), 2, id, NewEvent::delivery(id, d["thread_id"].as_i64(), &text(&d["method"]), &text(&d["state"]), &text(&d["ts"]))));
        }
        for a in self.rows("SELECT message_id, thread_id, agent, state, created_ts, settled_ts FROM acks", &[])? {
            let id = a["message_id"].as_i64().unwrap_or(0);
            let ts = a["settled_ts"].as_str().unwrap_or(a["created_ts"].as_str().unwrap_or("")).to_string();
            let (actor, kind) = if a["state"] == "posted" { (text(&a["agent"]), "agent") } else { ("agentdesk".to_string(), "system") };
            all.push((ts.clone(), 3, id, NewEvent::ack(id, a["thread_id"].as_i64(), &text(&a["agent"]), &text(&a["state"]), &actor, kind, &ts)));
        }
        for p in self.rows("SELECT * FROM permission_proposals", &[])? {
            let id = p["id"].as_i64().unwrap_or(0);
            let r = format!("proposal:{id}");
            let with_thread = |e: NewEvent| match p["thread_id"].as_i64() {
                Some(t) => e.thread(t),
                None => e,
            };
            let ts = text(&p["created_ts"]);
            let req = NewEvent::new("approval.requested", &text(&p["requested_by"]), "agent").reference(r.clone()).ts(&ts).data(json!({"rule": p["rule"], "scope": p["scope"]}));
            all.push((ts, 4, id, with_thread(req)));
            if let Some(ts) = p["decided_ts"].as_str() {
                let st = if p["status"] == "rejected" { "rejected" } else { "approved" };
                let e = NewEvent::new("approval.decided", crate::db::JOHN, crate::db::HUMAN).reference(r.clone()).ts(ts).data(json!({"status": st, "rule": p["rule"]}));
                all.push((ts.to_string(), 5, id, with_thread(e)));
            }
            if let Some(ts) = p["reverted_ts"].as_str() {
                let e = NewEvent::new("approval.reverted", crate::db::JOHN, crate::db::HUMAN).reference(r.clone()).ts(ts).data(json!({"rule": p["rule"]}));
                all.push((ts.to_string(), 6, id, with_thread(e)));
            }
        }
        all.sort_by(|a, b| (&a.0, a.1, a.2).cmp(&(&b.0, b.1, b.2)));
        let mut seqs: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut last = self.scalar_i64("SELECT hlc FROM events ORDER BY seq DESC LIMIT 1", &[])?.unwrap_or(0).max(0) as u64;
        let mut st = self
            .conn
            .prepare("INSERT INTO events (id, ts, hlc, origin, actor, actor_kind, actor_seq, kind, thread_id, ref, body, data) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)")?;
        for (ts, _, _, ev) in &all {
            let seq = match seqs.get_mut(&ev.actor) {
                Some(n) => {
                    *n += 1;
                    *n
                }
                None => {
                    let n = self.scalar_i64("SELECT COALESCE(MAX(actor_seq), 0) + 1 FROM events WHERE origin=:o AND actor=:a", &[(":o", &origin), (":a", &ev.actor)])?.unwrap_or(1);
                    seqs.insert(ev.actor.clone(), n);
                    n
                }
            };
            last = hlc_step(last, ms_of(ts));
            st.execute(rusqlite::params![
                uuid7(last),
                ts,
                last as i64,
                origin,
                ev.actor,
                ev.actor_kind,
                seq,
                ev.kind,
                ev.thread_id,
                ev.reference,
                ev.body,
                ev.data.as_ref().map(|d| d.to_string())
            ])?;
        }
        self.exec("INSERT INTO event_marks (name, value) VALUES ('backfill', :v)", &[(":v", &all.len().to_string())])?;
        Ok(all.len())
    }
}
