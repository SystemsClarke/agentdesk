//! The board as vault notes, and the vault's git backup: a transcript per day (`agentdesk/YYYY-MM-DD.md`), a linked note per thread
//! (`agentdesk/threads/thread-N.md`), a map (`maps/AgentDesk MOC.md`), then a commit and push of those paths.
//!
//! The files are byte-for-byte what the original's `agentdesk/backup.py` and `vault_threads.py` wrote, so a vault touched by either
//! build does not churn. Notes are rewritten only when their text changed.

use crate::db::Db;
use crate::paths;
use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

fn agentdesk_dir() -> PathBuf {
    paths::vault_dir().join("agentdesk")
}

pub fn threads_dir() -> PathBuf {
    agentdesk_dir().join("threads")
}

/// A wikilink to a thread's note; the path is spelled out so it resolves whatever else in the vault is called thread-N.
pub fn link(tid: i64, label: Option<&str>) -> String {
    match label {
        Some(l) => format!("[[agentdesk/threads/thread-{tid}|{l}]]"),
        None => format!("[[agentdesk/threads/thread-{tid}]]"),
    }
}

fn yaml(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " "))
}

fn local(ts: &str) -> DateTime<Local> {
    DateTime::parse_from_rfc3339(ts).map(|d| d.with_timezone(&Local)).unwrap_or_else(|_| Utc::now().with_timezone(&Local))
}

fn write_if_changed(path: &Path, text: &str) -> Result<bool> {
    if std::fs::read_to_string(path).map(|old| old == text).unwrap_or(false) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(true)
}

/// `[start, end)` of a local day as UTC strings in `now_iso()`'s format. Each end is converted from its own local midnight, so a day
/// the clocks change is 23 or 25 hours long, not 24.
fn day_bounds(d: NaiveDate) -> (String, String) {
    let at = |day: NaiveDate| {
        let naive = day.and_hms_opt(0, 0, 0).unwrap();
        Local.from_local_datetime(&naive).earliest().unwrap_or_else(|| Local.from_utc_datetime(&naive)).with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    };
    (at(d), at(d.succ_opt().unwrap()))
}

fn bio_ids(db: &Db) -> Result<HashMap<String, i64>> {
    let mut out = HashMap::new();
    for r in db.rows("SELECT id, subject FROM threads WHERE subject LIKE 'bio:%' ORDER BY id", &[])? {
        out.insert(r["subject"].as_str().unwrap_or("")[4..].trim().to_lowercase(), r["id"].as_i64().unwrap_or(0));
    }
    Ok(out)
}

fn render_thread(db: &Db, tid: i64, bios: &HashMap<String, i64>, known: &HashSet<i64>) -> Result<Option<String>> {
    let Some(t) = db.row("SELECT * FROM threads WHERE id = :t", &[(":t", &tid)])? else {
        return Ok(None);
    };
    let msgs = db.rows("SELECT id, ts, author, author_kind, body, reply_to FROM messages WHERE thread_id = :t ORDER BY id", &[(":t", &tid)])?;
    let days: BTreeSet<String> = msgs.iter().map(|m| local(m["ts"].as_str().unwrap_or("")).date_naive().to_string()).collect();
    let (subject, opened_by) = (t["subject"].as_str().unwrap_or(""), t["opened_by"].as_str().unwrap_or(""));
    let updated = days.iter().next_back().cloned().unwrap_or_else(|| local(t["created_ts"].as_str().unwrap_or("")).date_naive().to_string());
    let mut lines = vec![
        "---".to_string(),
        "type: agentdesk-thread".to_string(),
        format!("tags: [agentdesk, {}, {}]", t["channel"].as_str().unwrap_or(""), t["status"].as_str().unwrap_or("")),
        format!("summary: {}", yaml(subject)),
        format!("aliases: [{}]", yaml(subject)),
        format!("updated: {updated}"),
        "---".to_string(),
        format!("# {subject}"),
        String::new(),
    ];
    let bio_of_opener = bios.get(&opened_by.to_lowercase()).copied().filter(|b| *b != tid).map(|b| format!(" ({})", link(b, Some("bio")))).unwrap_or_default();
    lines.push(format!(
        "Thread {tid} · channel {} · status {} · opened by {opened_by}{bio_of_opener} · {} message(s)",
        t["channel"].as_str().unwrap_or(""),
        t["status"].as_str().unwrap_or(""),
        msgs.len()
    ));
    if !days.is_empty() {
        lines.push(format!("Days: {}", days.iter().map(|d| format!("[[agentdesk/{d}]]")).collect::<Vec<_>>().join(" ")));
    }
    lines.extend([String::new(), "## Messages".to_string(), String::new()]);
    let re = Regex::new(r"\b([Tt]hread) #?(\d+)\b").unwrap();
    for m in &msgs {
        let who = m["author"].as_str().unwrap_or("");
        let who_txt = match bios.get(&who.to_lowercase()) {
            Some(b) if *b != tid => link(*b, Some(who)),
            _ => who.to_string(),
        };
        let reply = m["reply_to"].as_i64().map(|r| format!(" (reply to #{r})")).unwrap_or_default();
        let body = m["body"].as_str().unwrap_or("").replace("\r\n", "\n");
        let body = re
            .replace_all(&body, |c: &regex::Captures| match c[2].parse::<i64>() {
                Ok(n) if n != tid && known.contains(&n) => format!("{} {}", &c[1], link(n, Some(&c[2]))),
                _ => c[0].to_string(),
            })
            .replace('\n', "\n  ");
        let stamp = local(m["ts"].as_str().unwrap_or("")).format("%Y-%m-%d %H:%M");
        lines.push(format!("- {stamp} **{who_txt}** ({}){reply} - {body}", m["author_kind"].as_str().unwrap_or("")));
    }
    Ok(Some(lines.join("\n").trim_end_matches('\n').to_string() + "\n"))
}

/// The note for every thread; returns how many files changed on disk.
pub fn write_threads(db: &Db) -> Result<usize> {
    let dir = threads_dir();
    std::fs::create_dir_all(&dir)?;
    let known: HashSet<i64> = db.rows("SELECT id FROM threads", &[])?.iter().filter_map(|r| r["id"].as_i64()).collect();
    let bios = bio_ids(db)?;
    let mut changed = 0;
    let mut ids: Vec<i64> = known.iter().copied().collect();
    ids.sort_unstable();
    for tid in ids {
        if let Some(text) = render_thread(db, tid, &bios, &known)? {
            if write_if_changed(&dir.join(format!("thread-{tid}.md")), &text)? {
                changed += 1;
            }
        }
    }
    Ok(changed)
}

/// `maps/AgentDesk MOC.md`: every thread by channel, newest first.
pub fn write_moc(db: &Db) -> Result<bool> {
    let mut lines = vec![
        "---".to_string(),
        "type: moc".to_string(),
        "tags: [moc, agentdesk]".to_string(),
        "summary: Every AgentDesk board thread, by channel - the board's history in the vault".to_string(),
        format!("updated: {}", Local::now().date_naive()),
        "---".to_string(),
        "Generated by the AgentDesk backup from the board; edit the threads, not this map.".to_string(),
        "Daily transcripts are in `agentdesk/YYYY-MM-DD`.".to_string(),
        String::new(),
    ];
    for ch in db.rows("SELECT DISTINCT channel FROM threads ORDER BY channel", &[])? {
        let ch = ch["channel"].as_str().unwrap_or("");
        lines.extend([format!("## {ch}"), String::new()]);
        for r in db.rows("SELECT id, subject, status FROM threads WHERE channel = :c ORDER BY id DESC", &[(":c", &ch)])? {
            lines.push(format!("- {} - {}", link(r["id"].as_i64().unwrap_or(0), r["subject"].as_str()), r["status"].as_str().unwrap_or("")));
        }
        lines.push(String::new());
    }
    write_if_changed(&paths::vault_dir().join("maps").join("AgentDesk MOC.md"), &(lines.join("\n").trim_end_matches('\n').to_string() + "\n"))
}

/// One local day's transcript, plus the pointer line in the vault's `log/` for that day.
pub fn write_day(db: &Db, date: NaiveDate) -> Result<PathBuf> {
    let (start, end) = day_bounds(date);
    let rows = db.rows(
        "SELECT m.id, m.ts, m.thread_id, m.author, m.author_kind, m.body, m.reply_to, t.subject, t.channel, t.status, t.opened_by \
         FROM messages m JOIN threads t ON t.id = m.thread_id WHERE m.ts >= :s AND m.ts < :e ORDER BY m.id",
        &[(":s", &start), (":e", &end)],
    )?;
    let human = rows.iter().filter(|r| r["author_kind"] == json!("human")).count();
    let threads: HashSet<i64> = rows.iter().filter_map(|r| r["thread_id"].as_i64()).collect();
    let mut lines = vec![
        format!("# AgentDesk daily transcript - {date}"),
        String::new(),
        format!("{} threads touched, {} messages ({human} human, {} agent).", threads.len(), rows.len(), rows.len() - human),
    ];
    if rows.is_empty() {
        lines.extend([String::new(), "Nothing happened on this day - the backup ran and found no messages.".to_string()]);
    }
    lines.extend([String::new(), "## Threads touched".to_string(), String::new()]);
    if rows.is_empty() {
        lines.push("None.".to_string());
    }
    let mut seen = HashSet::new();
    for r in &rows {
        let tid = r["thread_id"].as_i64().unwrap_or(0);
        if seen.insert(tid) {
            lines.push(format!(
                "- {}: \"{}\" - channel {}, status {}, opened by {}",
                link(tid, Some(&format!("thread {tid}"))),
                r["subject"].as_str().unwrap_or(""),
                r["channel"].as_str().unwrap_or(""),
                r["status"].as_str().unwrap_or(""),
                r["opened_by"].as_str().unwrap_or("")
            ));
        }
    }
    lines.extend([String::new(), "## Messages".to_string(), String::new()]);
    if rows.is_empty() {
        lines.push("None.".to_string());
    }
    for r in &rows {
        let tid = r["thread_id"].as_i64().unwrap_or(0);
        let reply = r["reply_to"].as_i64().map(|x| format!(" (reply to #{x})")).unwrap_or_default();
        let body = r["body"].as_str().unwrap_or("").replace("\r\n", "\n").replace('\n', "\n  ");
        lines.push(format!(
            "- {} **{}** ({}) [{}]{reply} - {body}",
            local(r["ts"].as_str().unwrap_or("")).format("%H:%M"),
            r["author"].as_str().unwrap_or(""),
            r["author_kind"].as_str().unwrap_or(""),
            link(tid, Some(&format!("thread {tid}")))
        ));
    }
    let file = agentdesk_dir().join(format!("{date}.md"));
    write_if_changed(&file, &(lines.join("\n").trim_end_matches('\n').to_string() + "\n"))?;
    let pointer = format!("- [[agentdesk/{date}]] - AgentDesk daily transcript");
    let log = paths::vault_dir().join("log").join(format!("{date}.md"));
    match std::fs::read_to_string(&log) {
        Ok(existing) => {
            if !existing.lines().any(|l| l == pointer) {
                let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
                std::fs::write(&log, format!("{existing}{sep}{pointer}\n"))?;
            }
        }
        Err(_) => {
            std::fs::create_dir_all(log.parent().unwrap())?;
            std::fs::write(&log, format!("{pointer}\n"))?;
        }
    }
    Ok(file)
}

// --- git ----------------------------------------------------------------------

const PATHS: [&str; 3] = ["agentdesk", "log", "maps/AgentDesk MOC.md"];

fn git(vault: &Path, args: &[&str], env: &[(&str, String)]) -> Option<std::process::Output> {
    crate::paths::new_command("git").args(args).current_dir(vault).envs(env.iter().map(|(k, v)| (*k, v.as_str()))).output().ok()
}

fn text(o: &std::process::Output) -> String {
    let t = String::from_utf8_lossy(if o.stderr.is_empty() { &o.stdout } else { &o.stderr }).trim().to_string();
    t.chars().take(300).collect()
}

/// Commits what the backup wrote (only `agentdesk/`, `log/` and the map, so a note another session is mid-way through writing is left for
/// its author) and pushes. Never fails the backup: the problem is in the result, and the next run retries.
///
/// Commit identity comes from `AGENTDESK_GIT_AUTHOR_NAME` / `AGENTDESK_GIT_AUTHOR_EMAIL` (default: the repository's own config), signing
/// from `AGENTDESK_GIT_SIGNING_KEY` (an OpenPGP key id; unset means the repository's config; a failing signature falls back to unsigned).
pub fn sync(vault: &Path) -> Value {
    let mut out = json!({"committed": false, "pushed": false, "unpushed": null, "error": null});
    if !vault.join(".git").exists() {
        out["error"] = json!("the vault is not a git repository");
        return out;
    }
    let mut env: Vec<(&str, String)> = vec![];
    if let Ok(n) = std::env::var("AGENTDESK_GIT_AUTHOR_NAME") {
        env.push(("GIT_AUTHOR_NAME", n));
    }
    if let Ok(e) = std::env::var("AGENTDESK_GIT_AUTHOR_EMAIL") {
        env.push(("GIT_AUTHOR_EMAIL", e));
    }
    let mut add = vec!["add", "--"];
    add.extend(PATHS);
    git(vault, &add, &[]);
    let mut diff = vec!["diff", "--cached", "--quiet", "--"];
    diff.extend(PATHS);
    if git(vault, &diff, &[]).map(|o| !o.status.success()).unwrap_or(false) {
        let msg = format!("AgentDesk board backup {}", Local::now().format("%Y-%m-%d %H:%M"));
        let mut signed = vec![];
        if let Ok(k) = std::env::var("AGENTDESK_GIT_SIGNING_KEY") {
            let key = format!("user.signingkey={k}");
            signed.extend(["-c".to_string(), "gpg.format=openpgp".to_string(), "-c".to_string(), key, "-c".to_string(), "commit.gpgsign=true".to_string()]);
        }
        let commit = |extra: &[String]| {
            let mut args: Vec<&str> = extra.iter().map(String::as_str).collect();
            args.extend(["commit", "-q", "-m", &msg, "--"]);
            args.extend(PATHS);
            git(vault, &args, &env)
        };
        let mut r = commit(&signed);
        if r.as_ref().map(|o| !o.status.success()).unwrap_or(true) && !signed.is_empty() {
            r = commit(&["-c".to_string(), "commit.gpgsign=false".to_string()]);
        }
        match r {
            Some(o) if o.status.success() => out["committed"] = json!(true),
            Some(o) => {
                out["error"] = json!(format!("commit failed: {}", text(&o)));
                return out;
            }
            None => {
                out["error"] = json!("git could not be run");
                return out;
            }
        }
    }
    let branch = git(vault, &["rev-parse", "--abbrev-ref", "HEAD"], &[]).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let ahead = |o: Option<std::process::Output>| o.filter(|o| o.status.success()).and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i64>().ok());
    let range = format!("origin/{branch}..HEAD");
    let unpushed = ahead(git(vault, &["rev-list", "--count", &range], &[]));
    out["unpushed"] = json!(unpushed);
    if unpushed != Some(0) {
        let mut r = git(vault, &["push", "origin", "HEAD"], &[]);
        if r.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
            // someone else pushed first: rebase once, then try again
            if git(vault, &["pull", "--rebase", "--autostash", "origin", &branch], &[]).map(|o| !o.status.success()).unwrap_or(true) {
                git(vault, &["rebase", "--abort"], &[]);
            }
            r = git(vault, &["push", "origin", "HEAD"], &[]);
        }
        match r {
            Some(o) if o.status.success() => {
                out["pushed"] = json!(true);
                out["unpushed"] = json!(0);
            }
            Some(o) => out["error"] = json!(format!("push failed: {}", text(&o))),
            None => out["error"] = json!("git could not be run"),
        }
    }
    out
}
