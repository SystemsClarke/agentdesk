//! The Concierge's hands (docs/DISPATCH.md): while the Concierge is on, open Work to Hire items are claimed and handed to a worker by
//! plain code, in priority order, with no LLM deciding. The lead (concierge-lead) is woken only for items marked `triage` (a vague or
//! large one) and ones that failed three times. Order: meta.priority (0 urgent .. 4 whenever, default 2), then id. An item waits for its
//! meta.after (ids that must be done), and an `eager` item only runs while the week is behind its plan (the governor's trend ends under
//! its plan), which is how spare budget gets spent; being behind also lifts the cap on workers to what the governor can afford.

use crate::concierge::{BASE_MEMBERS, LEAD, NAME};
use crate::db::{parse_obj, py_dumps, Db, AGENT};
use crate::goals;
use crate::governor;
use crate::paths;
use anyhow::Result;
use chrono::{DateTime, Duration, Local, NaiveTime, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Map, Value};

const MAX_ATTEMPTS: i64 = 3;
const MECHANICAL: [&str; 8] = ["rename", "typo", "docstring", "changelog", "summar", "lookup", "format", "list "];

/// When a recurring item (meta.recur) has not come round yet, and the next one once it is done. `recur` is "HH:MM" (every day at that
/// local time) or N minutes / "Nh" (that long after the last finished): the "run every morning" job.
pub mod recur {
    use super::*;

    enum Every {
        After(Duration),
        Daily(NaiveTime),
    }

    fn parse(r: &str) -> Option<Every> {
        let r = r.trim();
        if r.len() == 5 {
            if let Ok(at) = NaiveTime::parse_from_str(r, "%H:%M") {
                return Some(Every::Daily(at));
            }
        }
        let unit = if r.ends_with('h') { 60.0 } else { 1.0 };
        let n: f64 = r.trim_end_matches(['m', 'h']).parse().ok()?;
        (n >= 1.0).then(|| Every::After(Duration::seconds((n * unit * 60.0) as i64)))
    }

    #[allow(dead_code)] // post_work validates with it once that tool is ported
    pub fn valid(recur: &str) -> bool {
        parse(recur).is_some()
    }

    /// The first run: a daily time waits for its next occurrence; an interval starts at once.
    #[allow(dead_code)] // post_work validates and schedules with it once that tool is ported
    pub fn first(recur: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        next_from(recur, now, true)
    }

    pub fn next(recur: &str, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        next_from(recur, from, false)
    }

    fn next_from(recur: &str, from: DateTime<Utc>, first: bool) -> Option<DateTime<Utc>> {
        match parse(recur)? {
            Every::After(e) => Some(if first { from } else { from + e }),
            Every::Daily(at) => {
                let local = from.with_timezone(&Local);
                let day = |d: chrono::NaiveDate| Local.from_local_datetime(&d.and_time(at)).earliest();
                let due = day(local.date_naive())?;
                let due = if due > local { due } else { day(local.date_naive() + Duration::days(1))? };
                Some(due.with_timezone(&Utc))
            }
        }
    }
}

fn meta_of(t: &Value) -> Map<String, Value> {
    parse_obj(t["meta"].as_str()).unwrap_or_default()
}

fn priority(m: &Map<String, Value>) -> i64 {
    m.get("priority").and_then(Value::as_i64).unwrap_or(2)
}

fn flag(m: &Map<String, Value>, k: &str) -> bool {
    m.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn model_for(m: &Map<String, Value>, subject: &str) -> &'static str {
    match m.get("model").and_then(Value::as_str) {
        Some("haiku") => "haiku",
        Some("sonnet") => "sonnet",
        Some("opus") => "opus",
        _ if MECHANICAL.iter().any(|k| subject.to_lowercase().contains(k)) => "haiku",
        _ => "sonnet",
    }
}

fn ready(db: &Db, m: &Map<String, Value>) -> bool {
    match m.get("after").and_then(Value::as_array) {
        None => true,
        Some(after) => {
            after.iter().all(|a| a.as_i64().is_some_and(|i| db.scalar_str("SELECT status FROM threads WHERE id=:i", &[(":i", &i)]).ok().flatten().as_deref() == Some("done")))
        }
    }
}

fn due(m: &Map<String, Value>, now: DateTime<Utc>) -> bool {
    match m.get("due").and_then(Value::as_str) {
        None => true,
        Some(d) => DateTime::parse_from_rfc3339(d).map(|d| d.with_timezone(&Utc) <= now).unwrap_or(true),
    }
}

/// Whether the week is running behind its plan (its last six hours' pace would end under the plan's landing), and how many more sessions
/// the governor says are affordable.
fn spend(db: &Db, now: DateTime<Utc>) -> (bool, i64) {
    let r = governor::report(db, now, false, None);
    let num = |k: &str| r[k].as_f64().unwrap_or(0.0);
    let behind = match (r["trend_end_pct"].as_f64(), r["plan_end_pct"].as_f64()) {
        (Some(trend), Some(plan)) => trend < plan - 2.0,
        _ => false,
    };
    let mut spare = r["caps"]["new_sessions"].as_i64().unwrap_or(0);
    // The sessions running now may burn far less than the allowance (idle ones cost little): the headroom between what the week is really
    // burning and what the plan allows buys more workers, at the governor's cost per session. It is re-read every tick, so as the workers
    // burn, the headroom closes.
    if behind && num("reset_in_hours") > 0.0 {
        let trend_rate = (num("trend_end_pct") - num("used")) / num("reset_in_hours");
        spare = spare.max(((num("allowed_rate") - trend_rate).max(0.0) / num("session_rate").max(0.05)).floor() as i64);
    }
    (behind, spare)
}

/// A finished recurring item posts its next occurrence, once: a fresh open item, due when the schedule says.
fn renew(db: &Db, now: DateTime<Utc>) -> Result<()> {
    let sql = "SELECT id, subject, opened_by, meta FROM threads WHERE channel='work' AND status='done' AND json_valid(meta) \
               AND json_extract(meta, '$.recur') IS NOT NULL AND json_extract(meta, '$.renewed') IS NULL";
    for t in db.rows(sql, &[])? {
        let mut meta = meta_of(&t);
        let Some(next) = meta.get("recur").and_then(Value::as_str).and_then(|r| recur::next(r, now)) else { continue };
        let mut fresh = Map::new();
        for k in ["claim", "priority", "model", "eager", "after", "recur"] {
            if let Some(v) = meta.get(k) {
                fresh.insert(k.into(), v.clone());
            }
        }
        fresh.insert("due".into(), json!(next.to_rfc3339_opts(SecondsFormat::Secs, true)));
        let tid = t["id"].as_i64().unwrap_or(0);
        let body = db.scalar_str("SELECT body FROM messages WHERE thread_id=:t ORDER BY id LIMIT 1", &[(":t", &tid)])?.unwrap_or_default();
        let id = db.start_thread("work", t["subject"].as_str().unwrap_or(""), t["opened_by"].as_str().unwrap_or(""), AGENT, &body, Some(&Value::Object(fresh)), None)?;
        meta.insert("renewed".into(), json!(id));
        db.exec("UPDATE threads SET meta=:m WHERE id=:i", &[(":m", &py_dumps(&Value::Object(meta))), (":i", &tid)])?;
    }
    Ok(())
}

/// Puts the item back on the queue marked for the lead: a worker could not do it, or it needs splitting.
fn triage(db: &Db, id: i64, mut meta: Map<String, Value>, why: &str) -> Result<()> {
    meta.insert("triage".into(), json!(true));
    meta.remove("assignee");
    meta.remove("claimed_ts");
    db.exec("UPDATE threads SET meta=:m WHERE id=:i", &[(":m", &py_dumps(&Value::Object(meta))), (":i", &id)])?;
    db.reply(id, AGENT, AGENT, &format!("Dispatcher: {why}, so it goes to the Concierge's lead to split or finish."), None, None)?;
    Ok(())
}

/// Points the lead's measure at triage only: the lead is not woken for every open item.
pub fn start(db: &Db) {
    let r = db.exec("UPDATE goals SET measure_cmd='internal:open_triage' WHERE name=:n AND measure_cmd='internal:open_work'", &[(":n", &NAME)]);
    if let Err(e) = r {
        paths::log("WARN", &format!("dispatcher start failed: {e}")); // a locked db at boot must not end the dispatcher for the whole run
    }
}

/// Dispatches while there is room; returns the item ids it started a worker for.
pub fn tick(db: &Db, now: Option<DateTime<Utc>>) -> Result<Vec<i64>> {
    let now = now.unwrap_or_else(Utc::now);
    let mut started = vec![];
    let Some(g) = db.row("SELECT max_members FROM goals WHERE name=:n AND state='running'", &[(":n", &NAME)])? else { return Ok(started) };
    let members = db.scalar_i64("SELECT COUNT(*) FROM goal_members WHERE goal=:n", &[(":n", &NAME)])?.unwrap_or(0);
    // Behind the plan (the week is on course to end with budget unspent): the queue is where it goes, so allow as many workers as the
    // governor says it can afford, beyond the usual few. On pace, back to the usual few.
    let (behind, spare) = spend(db, now);
    let cap = if behind { BASE_MEMBERS.max(members + spare) } else { BASE_MEMBERS };
    if Some(cap) != g["max_members"].as_i64() {
        db.exec("UPDATE goals SET max_members=:m WHERE name=:n", &[(":m", &cap), (":n", &NAME)])?;
    }
    let room = cap - members;
    // A member still queued for a session is a worker already waiting: more would only pile up claimed items.
    let waiting =
        db.scalar_i64("SELECT COUNT(*) FROM goal_members m JOIN identities i ON i.name=m.identity WHERE m.goal=:n AND i.state<>'running'", &[(":n", &NAME)])?.unwrap_or(0);
    if room <= 0 || waiting > 0 {
        return Ok(started);
    }
    renew(db, now)?;
    let mut picks: Vec<(i64, Value, Map<String, Value>)> = db
        .rows("SELECT id, subject, meta FROM threads WHERE channel='work' AND status='open'", &[])?
        .into_iter()
        .map(|t| (t["id"].as_i64().unwrap_or(0), t.clone(), meta_of(&t)))
        .filter(|(_, _, m)| m.get("claim").and_then(Value::as_str) != Some("anyone") && !flag(m, "triage") && (behind || !flag(m, "eager")) && ready(db, m) && due(m, now))
        .collect();
    picks.sort_by_key(|(id, _, m)| (priority(m), *id));
    picks.truncate(room as usize);
    for (id, item, meta) in picks {
        let model = model_for(&meta, item["subject"].as_str().unwrap_or(""));
        let tries = meta.get("attempts").and_then(Value::as_i64).unwrap_or(0) + 1;
        if tries > MAX_ATTEMPTS {
            triage(db, id, meta, &format!("{MAX_ATTEMPTS} workers ended without completing it"))?;
            continue;
        }
        if !db.claim_task(id, LEAD)? {
            continue; // another claimed it first
        }
        let mut meta = db.row("SELECT meta FROM threads WHERE id=:i", &[(":i", &id)])?.map(|t| meta_of(&t)).unwrap_or_default();
        meta.insert("attempts".into(), json!(tries));
        db.exec("UPDATE threads SET meta=:m WHERE id=:i", &[(":m", &py_dumps(&Value::Object(meta))), (":i", &id)])?;
        let name = format!("w{id}-{:04x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) & 0xffff);
        let task = format!("Work to Hire item #{id}: {}. Read it with read_thread {id}.", item["subject"].as_str().unwrap_or(""));
        match goals::spawn_as_lead(db, NAME, &name, &task, Some(model), Some(id)) {
            Ok(_) => {
                started.push(id);
                paths::log("INFO", &format!("dispatcher: work item #{id} to a {model} worker"));
            }
            Err(e) => {
                paths::log("WARN", &format!("dispatcher: could not start a worker for #{id}: {e}"));
                db.reopen_task(id)?;
            }
        }
    }
    Ok(started)
}
