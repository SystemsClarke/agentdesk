//! The background work the original core did on timers: toasts for new questions and follow-ups, the recurring
//! jobs tick, the merge-list check, the usage meter, and the sweep that files settled questions into the vault.
//! Run by the TUI (on a thread) or headless with `agentdesk daemon`. Every step is safe to run in two processes at
//! once: each claims its work with a conditional UPDATE.

use crate::db::Db;
use crate::{jobs, notify, paths, prs, usage};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Settled questions older than this are filed (written to the vault, status `archived`).
const SWEEP_AFTER_MINUTES: i64 = 24 * 60;
/// How old the newest snapshot may be before the core takes another.
const BACKUP_EVERY: Duration = Duration::from_secs(55 * 60);

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
    let mut t_backup = None::<Instant>;
    let backing_up = Arc::new(AtomicBool::new(false));
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
        if crate::daemon::stop_path().exists() {
            break; // `agentdesk core stop`
        }
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
        // The hourly backup runs in the core: a scheduled task would start the executable from outside, which a security product may not trust.
        if due(&mut t_backup, Duration::from_secs(600), backup_first_delay()) && std::env::var_os("AGENTDESK_NO_BACKUP").is_none() {
            backup_if_due(&backing_up);
        }
        if due(&mut t_update, Duration::from_secs(4 * 3600), Duration::from_secs(30)) && std::env::var_os("AGENTDESK_NO_UPDATE_CHECK").is_none() {
            crate::update::check_and_record();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    supervisor.shutdown(); // its sessions end with it; the next supervisor resumes them
}

/// How long after start the core first checks whether a backup is due: two minutes, or `AGENTDESK_BACKUP_FIRST_SECS` (the parity suite
/// sets it to a second so the in-core backup can be observed).
fn backup_first_delay() -> Duration {
    Duration::from_secs(std::env::var("AGENTDESK_BACKUP_FIRST_SECS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(120))
}

/// Snapshots the board (and files the vault) when the newest snapshot is an hour old, on its own thread so the loop keeps ticking.
fn backup_if_due(running: &Arc<AtomicBool>) {
    let newest = std::fs::read_dir(paths::backup_dir()).ok().into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()?.modified().ok()).max();
    let fresh = newest.is_some_and(|t| t.elapsed().is_ok_and(|age| age < BACKUP_EVERY));
    if fresh || running.swap(true, Ordering::SeqCst) {
        return;
    }
    let running = running.clone();
    std::thread::spawn(move || {
        match Db::open(&paths::db_path()).and_then(|db| crate::backup::run_with(&db, false)) {
            Ok(note) => paths::log("INFO", &format!("backup: {}", note.lines().next().unwrap_or(""))),
            Err(e) => paths::log("WARN", &format!("backup failed: {e}")),
        }
        running.store(false, Ordering::SeqCst);
    });
}

fn toasts(db: &Db, known: &mut HashSet<i64>, last_msg: &mut i64) {
    let now = db.open_questions(false).unwrap_or_default();
    for q in &now {
        let id = q["thread_id"].as_i64().unwrap_or(0);
        if known.insert(id) {
            notify::toast(&format!("Question from {}", q["opened_by"].as_str().unwrap_or("an agent")), &format!("#{id} {}", q["subject"].as_str().unwrap_or("")), Some(id));
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
                f["thread_id"].as_i64(),
            );
        }
        *last_msg = max;
    }
}

/// Files settled questions: a note per thread in the vault, then status `archived`.
pub fn sweep(db: &Db) {
    let cutoff = crate::db::iso(chrono::Utc::now() - chrono::Duration::minutes(SWEEP_AFTER_MINUTES));
    let rows = db
        .rows(
            "SELECT id FROM threads WHERE channel='question' AND status IN ('answered','closed') AND updated_ts < :c \
             AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta,'$.archive_hold') END, 0) = 0",
            &[(":c", &cutoff)],
        )
        .unwrap_or_default();
    for r in rows {
        let id = r["id"].as_i64().unwrap_or(0);
        if crate::vault_threads::write_thread(db, id).is_ok() {
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

#[cfg(test)]
mod backup_tests {
    use super::*;
    use crate::tests_identities::Env;

    #[test]
    fn a_fresh_snapshot_means_no_new_one_and_no_thread() {
        let _e = Env::new();
        std::fs::create_dir_all(paths::backup_dir()).unwrap();
        std::fs::write(paths::backup_dir().join("agentdesk-20200101T000000.db"), b"x").unwrap();
        let running = Arc::new(AtomicBool::new(false));
        backup_if_due(&running);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!running.load(Ordering::SeqCst), "nothing was started");
        assert_eq!(std::fs::read_dir(paths::backup_dir()).unwrap().count(), 1, "no second snapshot");
    }

    #[test]
    fn a_backup_in_progress_is_not_started_twice() {
        let _e = Env::new();
        let running = Arc::new(AtomicBool::new(true));
        backup_if_due(&running);
        assert!(!paths::backup_dir().exists() || std::fs::read_dir(paths::backup_dir()).unwrap().count() == 0);
    }
}
