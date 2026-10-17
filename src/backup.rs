//! Hourly snapshot of the board (`VACUUM INTO`), thinned: all of them for 2 days, then one a day up to 14 days,
//! plus a note per answered thread in the vault. Run it from cron / Task Scheduler / launchd: `agentdesk backup`.

use crate::{db::Db, paths, vault_threads};
use anyhow::Result;
use chrono::Timelike;
use serde_json::json;
use std::collections::HashSet;

/// One backup cycle. `skip_vault`: the snapshot only (an ad hoc run against a real board must not rewrite the vault).
pub fn run_with(db: &Db, skip_vault: bool) -> Result<String> {
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
    if skip_vault {
        return Ok(format!("snapshot {} (integrity {integrity}, {thinned} older removed), vault skipped", path.display()));
    }
    // The vault as the board's history: a transcript for today, a linked note per thread, a map, then a commit and push.
    let today = chrono::Local::now().date_naive();
    vault_threads::write_day(db, today)?;
    let threads = vault_threads::write_threads(db)?;
    vault_threads::write_moc(db)?;
    let vault = paths::vault_dir();
    let mut git = vault_threads::sync(&vault, &vault_threads::DAILY, "board backup");
    // Once a day, in the small hours, the notes other sessions wrote are committed too: not hourly, so a note someone is still writing is not caught half done.
    let (day, now) = (today.to_string(), chrono::Local::now());
    let swept = if now.hour() == 3 && last_sweep().as_deref() != Some(&day) {
        let s = vault_threads::sync(&vault, &vault_threads::SWEEP, "notes sweep");
        if git["error"].is_null() {
            git["error"] = s["error"].clone();
        }
        git["committed"] = json!(git["committed"] == true || s["committed"] == true);
        git["pushed"] = json!(git["pushed"] == true || s["pushed"] == true);
        git["unpushed"] = s["unpushed"].clone();
        Some(day)
    } else {
        last_sweep()
    };
    write_state(&git, threads, swept);
    let git_note = match (git["committed"].as_bool(), git["pushed"].as_bool(), git["error"].as_str()) {
        (_, _, Some(e)) => format!(", GIT ERROR: {e}"),
        (c, p, None) => format!(", {}{}", if c == Some(true) { "committed" } else { "no change" }, if p == Some(true) { ", pushed" } else { "" }),
    };
    Ok(format!("snapshot {} (integrity {integrity}, {thinned} older removed), {threads} thread notes changed in {}{git_note}", path.display(), paths::vault_dir().display()))
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
fn write_state(git: &serde_json::Value, threads: usize, last_sweep: Option<String>) {
    let state = serde_json::json!({
        "ts": crate::db::iso(chrono::Utc::now()),
        "committed": git["committed"], "pushed": git["pushed"], "unpushed": git["unpushed"], "error": git["error"], "threads_written": threads,
        "last_sweep": last_sweep,
    });
    let _ = paths::write_atomic(&paths::data_dir().join("vault_state.json"), state.to_string());
}

/// The day the notes were last committed (`vault_state.json`).
fn last_sweep() -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(paths::data_dir().join("vault_state.json")).ok()?).ok()?;
    v["last_sweep"].as_str().map(String::from)
}

fn thin(dir: &std::path::Path) -> usize {
    let now = chrono::Utc::now();
    let mut files: Vec<(chrono::DateTime<chrono::Utc>, std::path::PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let stamp = n.strip_prefix("agentdesk-")?.strip_suffix(".db")?.split('-').next()?.to_string(); // a "-N" suffix marks a same-second twin
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
            !kept_days.insert(t.with_timezone(&chrono::Local).date_naive()) // the newest snapshot of each local day stays
        } else {
            false
        };
        if drop && std::fs::remove_file(p).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The snapshots thinning would delete, and the disk they would free.
pub fn prune_dry_run() -> String {
    let dir = paths::backup_dir();
    let now = chrono::Utc::now();
    let mut files: Vec<(chrono::DateTime<chrono::Utc>, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let stamp = n.strip_prefix("agentdesk-")?.strip_suffix(".db")?.split('-').next()?.to_string();
            Some((chrono::NaiveDateTime::parse_from_str(&stamp, "%Y%m%dT%H%M%S").ok()?.and_utc(), e.path()))
        })
        .collect();
    files.sort();
    let (mut kept, mut doomed, mut bytes) = (HashSet::new(), 0, 0u64);
    for (t, p) in files.into_iter().rev() {
        let age = now - t;
        if age.num_days() >= 14 || (age.num_hours() >= 48 && !kept.insert(t.with_timezone(&chrono::Local).date_naive())) {
            doomed += 1;
            bytes += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        }
    }
    format!("would delete {doomed} snapshot(s), freeing {:.1} MB", bytes as f64 / 1_048_576.0)
}

fn row_counts(c: &rusqlite::Connection) -> serde_json::Value {
    let n = |t: &str| c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    serde_json::json!({"threads": n("threads"), "messages": n("messages")})
}

/// `agentdesk restore <archive>`: a dry run unless `yes`. Restores through SQLite's online backup API (safe with live writers),
/// snapshots the state it is about to overwrite first, refuses an archive stamped after the live board's newest message unless
/// `force_newer`, brings the schema current and says so on the board.
pub fn restore(db: &Db, archive: &std::path::Path, yes: bool, force_newer: bool, json: bool) -> Result<String> {
    if !archive.is_file() {
        anyhow::bail!("no such archive: {}", archive.display());
    }
    let live_path = paths::db_path();
    let arc = rusqlite::Connection::open_with_flags(archive, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (live_counts, archive_counts) = (row_counts(&db.conn), row_counts(&arc));
    if !yes {
        let r = serde_json::json!({"dry_run": true, "archive": archive.to_string_lossy(), "live_db": live_path.to_string_lossy(), "live_counts": live_counts, "archive_counts": archive_counts});
        return Ok(if json {
            r.to_string()
        } else {
            format!(
                "DRY RUN -- nothing changed. Pass --yes to restore for real.\n  archive:  {}  {archive_counts}\n  live db:  {}  {live_counts}",
                archive.display(),
                live_path.display()
            )
        });
    }
    let stamp = archive
        .file_stem()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("agentdesk-"))
        .and_then(|n| n.split('-').next())
        .and_then(|n| chrono::NaiveDateTime::parse_from_str(n, "%Y%m%dT%H%M%S").ok())
        .map(|t| t.and_utc());
    let live_latest = db.scalar_str("SELECT MAX(ts) FROM messages", &[])?.and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()).map(|t| t.with_timezone(&chrono::Utc));
    if let (Some(a), Some(l)) = (stamp, live_latest) {
        if a > l && !force_newer {
            anyhow::bail!(
                "refused: archive {} is stamped {a}, after the live database's latest message ({l}): restoring forward is refused by default; pass --force-newer to do it anyway",
                archive.display()
            );
        }
    }
    // the way back: the state about to be overwritten
    let dir = paths::backup_dir();
    std::fs::create_dir_all(&dir)?;
    let s = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let mut pre = dir.join(format!("agentdesk-{s}.db"));
    for n in 1.. {
        if !pre.exists() {
            break;
        }
        pre = dir.join(format!("agentdesk-{s}-{n}.db"));
    }
    db.exec(&format!("VACUUM INTO '{}'", pre.to_string_lossy().replace('\'', "''")), &[])?;
    let mut live = rusqlite::Connection::open(&live_path)?;
    live.busy_timeout(std::time::Duration::from_secs(30))?;
    {
        let b = rusqlite::backup::Backup::new(&arc, &mut live)?;
        b.run_to_completion(64, std::time::Duration::from_millis(50), None)?;
    }
    drop(live);
    let fresh = Db::open(&live_path)?;
    fresh.init()?; // an older snapshot comes back current
    let after = row_counts(&fresh.conn);
    let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or("");
    fresh.start_thread(
        "discussion",
        &format!("Database restored from {name}"),
        "watcher",
        crate::db::AGENT,
        &format!(
            "Restored `{}` from archive `{name}`. Row counts before: {live_counts}, after: {after}. A pre-restore snapshot was taken first: `{}`. notify_state.json was not touched, so a toast may re-fire or miss one until it next runs -- known, not swallowed.",
            live_path.display(),
            pre.file_name().and_then(|n| n.to_str()).unwrap_or("")
        ),
        None,
        None,
    )?;
    let out = serde_json::json!({"archive": archive.to_string_lossy(), "pre_restore_snapshot": pre.to_string_lossy(), "before": live_counts, "after": after});
    Ok(if json { out.to_string() } else { format!("restored {} from {name}: {live_counts} -> {after}", live_path.display()) })
}
