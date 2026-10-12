//! Hourly snapshot of the board (`VACUUM INTO`), thinned: all of them for 2 days, then one a day up to 14 days,
//! plus a note per answered thread in the vault. Run it from cron / Task Scheduler / launchd: `agentdesk backup`.

use crate::{db::Db, paths, vault};
use anyhow::Result;
use std::collections::HashSet;

pub fn run(db: &Db) -> Result<String> {
    let dir = paths::backup_dir();
    std::fs::create_dir_all(&dir)?;
    let name = format!("agentdesk-{}.db", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(&name);
    db.exec(&format!("VACUUM INTO '{}'", path.to_string_lossy().replace('\'', "''")), &[])?;
    let thinned = thin(&dir);
    let mut notes = 0;
    for r in db.rows("SELECT id FROM threads WHERE channel='question' AND status IN ('answered','closed','archived')", &[])? {
        if vault::write_thread_note(db, r["id"].as_i64().unwrap_or(0)).is_ok() {
            notes += 1;
        }
    }
    Ok(format!("snapshot {} ({} older removed), {notes} thread notes in {}", path.display(), thinned, paths::vault_dir().display()))
}

fn thin(dir: &std::path::Path) -> usize {
    let now = chrono::Utc::now();
    let mut files: Vec<(chrono::DateTime<chrono::Utc>, std::path::PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let stamp = n.strip_prefix("agentdesk-")?.strip_suffix(".db")?.to_string();
            let t = chrono::NaiveDateTime::parse_from_str(&stamp, "%Y%m%d-%H%M%S").ok()?;
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
