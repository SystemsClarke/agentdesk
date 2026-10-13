//! The background work the original core did on timers: toasts for new questions and follow-ups, the recurring
//! jobs tick, the merge-list check, the usage meter, and the sweep that files settled questions into the vault.
//! Run by the TUI (on a thread) or headless with `agentdesk daemon`. Every step is safe to run in two processes at
//! once: each claims its work with a conditional UPDATE.

use crate::db::Db;
use crate::{jobs, notify, paths, prs, usage, vault};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Settled questions older than this are filed (written to the vault, status `archived`).
const SWEEP_AFTER_MINUTES: i64 = 24 * 60;

pub fn run(stop: Arc<AtomicBool>) {
    // the ops console lives as long as this loop
    let _console = if std::env::var_os("AGENTDESK_NO_WEB").is_none() { crate::web::start() } else { None };
    let Ok(db) = Db::open(&paths::db_path()) else { return };
    if db.init().is_err() {
        return;
    }
    let mut known: HashSet<i64> = db.open_questions(false).unwrap_or_default().iter().filter_map(|q| q["thread_id"].as_i64()).collect();
    let mut last_msg = db.max_message_id();
    let mut last_human = db.scalar_i64("SELECT COALESCE(MAX(id),0) FROM messages WHERE author_kind='human'", &[]).ok().flatten().unwrap_or(0);
    let start = Instant::now();
    let mut t_update = None::<Instant>;
    let mut t_super = None::<Instant>;
    let mut supervisor = crate::supervisor::Supervisor::new();
    let (mut t_jobs, mut t_prs, mut t_usage, mut t_sweep, mut t_toast) = (None::<Instant>, None::<Instant>, None::<Instant>, None::<Instant>, None::<Instant>);
    let due = |t: &mut Option<Instant>, every: Duration, first: Duration| -> bool {
        let now = Instant::now();
        let go = match t {
            None => now.duration_since(start) >= first,
            Some(prev) => now.duration_since(*prev) >= every,
        };
        if go {
            *t = Some(now);
        }
        go
    };
    while !stop.load(Ordering::Relaxed) {
        if due(&mut t_toast, Duration::from_secs(2), Duration::ZERO) {
            toasts(&db, &mut known, &mut last_msg);
            ack_watch(&db, &mut last_human);
        }
        if due(&mut t_super, Duration::from_secs(2), Duration::ZERO) {
            if let Err(e) = supervisor.tick(&db) {
                paths::log("WARN", &format!("supervisor: {e}"));
            }
        }
        if due(&mut t_jobs, Duration::from_secs(30), Duration::ZERO) {
            let _ = jobs::tick(&db);
        }
        if due(&mut t_prs, Duration::from_secs(60), Duration::from_secs(5)) {
            prs::check(&db);
        }
        if due(&mut t_usage, Duration::from_secs(300), Duration::ZERO) {
            usage::sample(&db);
        }
        if due(&mut t_sweep, Duration::from_secs(60), Duration::from_secs(10)) {
            sweep(&db);
        }
        if due(&mut t_update, Duration::from_secs(4 * 3600), Duration::from_secs(30)) && std::env::var_os("AGENTDESK_NO_UPDATE_CHECK").is_none() {
            crate::update::check_and_record();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    supervisor.shutdown(); // its sessions end with it; the next supervisor resumes them
}

fn toasts(db: &Db, known: &mut HashSet<i64>, last_msg: &mut i64) {
    let now = db.open_questions(false).unwrap_or_default();
    for q in &now {
        let id = q["thread_id"].as_i64().unwrap_or(0);
        if known.insert(id) {
            notify::toast(&format!("Question from {}", q["opened_by"].as_str().unwrap_or("an agent")), &format!("#{id} {}", q["subject"].as_str().unwrap_or("")));
        }
    }
    let ids: HashSet<i64> = now.iter().filter_map(|q| q["thread_id"].as_i64()).collect();
    known.retain(|i| ids.contains(i)); // a question that was answered and asks again rings again
    let max = db.max_message_id();
    if max > *last_msg {
        for f in db.follow_ups(*last_msg, max).unwrap_or_default() {
            let done = f["mark"].as_str() == Some("done");
            let first = f["body"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string();
            notify::toast(
                &format!("{} from {}", if done { "Done" } else { "Follow-up" }, f["author"].as_str().unwrap_or("agent")),
                &format!("#{} {}: {first}", f["thread_id"], f["subject"].as_str().unwrap_or("")),
            );
        }
        *last_msg = max;
    }
}

/// Files settled questions: a note per thread in the vault, then status `archived`.
pub fn sweep(db: &Db) {
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(SWEEP_AFTER_MINUTES)).format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
    let rows = db
        .rows(
            "SELECT id FROM threads WHERE channel='question' AND status IN ('answered','closed') AND updated_ts < :c \
             AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta,'$.archive_hold') END, 0) = 0",
            &[(":c", &cutoff)],
        )
        .unwrap_or_default();
    for r in rows {
        let id = r["id"].as_i64().unwrap_or(0);
        if vault::write_thread_note(db, id).is_ok() {
            let _ = db.archive(id);
        }
    }
}

/// John's replies made by another process (the Slack bridge writes to the board directly): the opener's acknowledgement is queued here,
/// as the original's board watcher did for every human message, so it is delivered on the agent's next write.
fn ack_watch(db: &Db, last_human: &mut i64) {
    let rows = db
        .rows(
            "SELECT m.id, m.thread_id, t.opened_by FROM messages m JOIN threads t ON t.id = m.thread_id \
             WHERE m.author_kind='human' AND m.id > :last AND t.channel='question' \
               AND (SELECT author_kind FROM messages WHERE thread_id = t.id ORDER BY id LIMIT 1) = 'agent' ORDER BY m.id",
            &[(":last", &*last_human)],
        )
        .unwrap_or_default();
    for r in rows {
        let (mid, tid, who) = (r["id"].as_i64().unwrap_or(0), r["thread_id"].as_i64().unwrap_or(0), r["opened_by"].as_str().unwrap_or("").to_string());
        let _ = db.exec(
            "INSERT OR IGNORE INTO acks (message_id, thread_id, agent, state, created_ts) VALUES (:m,:t,:a,'pending',:ts)",
            &[(":m", &mid), (":t", &tid), (":a", &who), (":ts", &crate::db::now_iso())],
        );
        *last_human = (*last_human).max(mid);
    }
    if let Ok(Some(max)) = db.scalar_i64("SELECT MAX(id) FROM messages WHERE author_kind='human'", &[]).map(|m| m.filter(|m| *m > *last_human)) {
        *last_human = max;
    }
}
