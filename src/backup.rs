//! Hourly snapshot of the board (`VACUUM INTO`), thinned: all of them for 2 days, then one a day up to 14 days,
//! plus a note per answered thread in the vault. Run it from cron / Task Scheduler / launchd: `agentdesk backup`.

use crate::{db::Db, paths, vault, vault_threads};
use anyhow::Result;
use std::collections::HashSet;

pub fn run(db: &Db) -> Result<String> {
    let dir = paths::backup_dir();
    std::fs::create_dir_all(&dir)?;
    // Second resolution: two snapshots in one second (a manual run beside the scheduled one) must not overwrite or refuse each other.
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let mut path = dir.join(format!("agentdesk-{stamp}.db"));
    for n in 1.. {
        if !path.exists() {
            break;
        }
        path = dir.join(format!("agentdesk-{stamp}-{n}.db"));
    }
    db.exec(&format!("VACUUM INTO '{}'", path.to_string_lossy().replace('\'', "''")), &[])?;
    let integrity = verify(&path);
    let thinned = thin(&dir);
    let mut notes = 0;
    for r in db.rows("SELECT id FROM threads WHERE channel='question' AND status IN ('answered','closed','archived')", &[])? {
        if vault::write_thread_note(db, r["id"].as_i64().unwrap_or(0)).is_ok() {
            notes += 1;
        }
    }
    // The vault as the board's history: a transcript for today, a linked note per thread, a map, then a commit and push.
    let today = chrono::Local::now().date_naive();
    vault_threads::write_day(db, today)?;
    let threads = vault_threads::write_threads(db)?;
    vault_threads::write_moc(db)?;
    let git = vault_threads::sync(&paths::vault_dir());
    write_state(&git, threads);
    let git_note = match (git["committed"].as_bool(), git["pushed"].as_bool(), git["error"].as_str()) {
        (_, _, Some(e)) => format!(", GIT ERROR: {e}"),
        (c, p, None) => format!(", {}{}", if c == Some(true) { "committed" } else { "no change" }, if p == Some(true) { ", pushed" } else { "" }),
    };
    Ok(format!(
        "snapshot {} (integrity {integrity}, {thinned} older removed), {notes} question notes, {threads} thread notes in {}{git_note}",
        path.display(),
        paths::vault_dir().display()
    ))
}

/// `PRAGMA integrity_check` on a snapshot: "ok", or what SQLite complained about. A snapshot that fails is renamed `<name>.corrupt`, so it
/// neither counts as a restore candidate nor gets thinned as a good one.
pub(crate) fn verify(path: &std::path::Path) -> String {
    let verdict = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| {
            let mut st = c.prepare("PRAGMA integrity_check")?;
            let rows: Vec<String> = st.query_map([], |r| r.get::<_, String>(0))?.flatten().collect();
            Ok(if rows == ["ok"] { "ok".to_string() } else { rows.into_iter().take(3).collect::<Vec<_>>().join("; ") })
        })
        .unwrap_or_else(|e| format!("unreadable: {e}"));
    if verdict != "ok" {
        let mut bad = path.as_os_str().to_owned();
        bad.push(".corrupt");
        let _ = std::fs::rename(path, bad);
    }
    verdict
}

/// `vault_state.json` in the data folder: how the vault's git backup went this run, for the window's header and the status screens.
fn write_state(git: &serde_json::Value, threads: usize) {
    let state = serde_json::json!({
        "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
        "committed": git["committed"], "pushed": git["pushed"], "unpushed": git["unpushed"], "error": git["error"], "threads_written": threads,
    });
    let target = paths::data_dir().join("vault_state.json");
    let tmp = target.with_extension("json.tmp");
    if std::fs::write(&tmp, state.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &target);
    }
}

fn thin(dir: &std::path::Path) -> usize {
    let now = chrono::Utc::now();
    let mut files: Vec<(chrono::DateTime<chrono::Utc>, std::path::PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let stamp = n.strip_prefix("agentdesk-")?.strip_suffix(".db")?.chars().take(15).collect::<String>(); // a "-N" suffix marks a same-second twin
            let t = chrono::NaiveDateTime::parse_from_str(&stamp, "%Y%m%dT%H%M%S").ok()?;
            Some((t.and_utc(), e.path()))
        })
        .collect();
    files.sort();
    let mut kept_days = HashSet::new();
    let mut removed = 0;
    for (t, p) in files.into_iter().rev() {
        let age = now - t;
        let drop = if age.num_days() >= 14 {
            true
        } else if age.num_hours() >= 48 {
            !kept_days.insert(t.date_naive())
        } else {
            false
        };
        if drop && std::fs::remove_file(p).is_ok() {
            removed += 1;
        }
    }
    removed
}
