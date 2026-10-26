//! A git repository of the person's conversations: `conversations/<yyyy>/<thread-id>-<slug>.md`, one append-only markdown file per board thread
//! the person took part in (a thread with a human message), so there is a diffable backup that is theirs alone.
//!
//! A file is a log. Its header is written once; after that a sync only appends: a block for each message not yet in the file (in id order), a
//! one-line footer for each delivery or ack, and a `> status:` footer when the thread's status changed. Re-running with nothing new writes
//! nothing and so commits nothing. Bodies are redacted with `vault_threads::redact` and indented two spaces, so no body line can look like
//! structure. Commit, signing and push are `vault_threads::sync`, with the vault's author and signing settings.
//!
//! Not kept (lossy by design, the board's own snapshots hold them): thread and message meta, a rename after the first sync, redacted tokens,
//! spaces in names (written as underscores), a body's leading and trailing blank lines (restore drops them), and a body's line endings
//! (`\r\n` and a lone `\r` are written, and restored, as `\n`). Agent session transcripts are a follow-up, not part of this.

use crate::db::Db;
use crate::{paths, vault_threads};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The repository: `conversations_dir` in settings.json, else `<data dir>/conversations`.
pub fn repo_dir() -> PathBuf {
    paths::setting("conversations_dir").as_str().filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| paths::data_dir().join("conversations"))
}

fn one_line(s: &str) -> String {
    vault_threads::redact(s).replace(['\r', '\n'], " ")
}

fn name(s: &str) -> String {
    one_line(s).replace(' ', "_")
}

fn slug(subject: &str) -> String {
    let mut out = String::new();
    for c in subject.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(40).collect();
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "thread".into()
    } else {
        out.into()
    }
}

/// The files already in each year directory, by thread id: each directory is listed once per run, on first use.
type Listing = HashMap<PathBuf, HashMap<i64, PathBuf>>;

fn list_year(dir: &Path) -> HashMap<i64, PathBuf> {
    let mut out = HashMap::new();
    for p in std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()) {
        let Some(n) = p.file_name().and_then(|n| n.to_str()).filter(|n| n.ends_with(".md")) else { continue };
        if let Some(id) = n.split_once('-').and_then(|(id, _)| id.parse::<i64>().ok()) {
            out.entry(id).or_insert(p);
        }
    }
    out
}

/// The file of a thread: the one already there for its id (a renamed subject keeps its file), else `<yyyy>/<id>-<slug>.md`. The slug is
/// built from the redacted subject, so a token in a subject never reaches the path, the history or the remote.
fn file_of(root: &Path, listing: &mut Listing, tid: i64, created: &str, subject: &str) -> PathBuf {
    let year = created.get(..4).filter(|y| y.chars().all(|c| c.is_ascii_digit())).unwrap_or("0000");
    let dir = root.join(year);
    let files = listing.entry(dir.clone()).or_insert_with(|| list_year(&dir));
    files.entry(tid).or_insert_with(|| dir.join(format!("{tid}-{}.md", slug(&vault_threads::redact(subject))))).clone()
}

fn block(m: &Value) -> String {
    let reply = m["reply_to"].as_i64().map(|r| format!(" | reply_to: {r}")).unwrap_or_default();
    let mut s = format!(
        "\n### {} {}\nid: {} | kind: {}{reply}\n\n",
        name(m["author"].as_str().unwrap_or("")),
        m["ts"].as_str().unwrap_or(""),
        m["id"].as_i64().unwrap_or(0),
        m["author_kind"].as_str().unwrap_or("")
    );
    let body = vault_threads::redact(m["body"].as_str().unwrap_or("")).replace("\r\n", "\n").replace('\r', "\n");
    for line in body.trim_end_matches('\n').lines() {
        if !line.is_empty() {
            s.push_str("  ");
            s.push_str(line);
        }
        s.push('\n');
    }
    s
}

/// What the file already holds: the message ids, the footer lines, the last status.
fn scan(text: &str) -> (HashSet<i64>, HashSet<String>, Option<String>) {
    let (mut ids, mut footers, mut status) = (HashSet::new(), HashSet::new(), None);
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("id: ") {
            if let Some(n) = rest.split(' ').next().and_then(|n| n.parse().ok()) {
                ids.insert(n);
            }
        } else if let Some(s) = l.strip_prefix("> status: ") {
            status = Some(s.to_string());
            footers.insert(l.to_string());
        } else if l.starts_with("> ") {
            footers.insert(l.to_string());
        } else if status.is_none() && l.starts_with("Thread ") {
            status = l.split(" | ").find_map(|p| p.strip_prefix("status ")).map(String::from);
        }
    }
    (ids, footers, status)
}

/// The text of a thread's file after this sync: `old` with whatever is new appended, or None when the thread is gone.
fn render(db: &Db, tid: i64, old: Option<&str>) -> Result<Option<String>> {
    let Some(t) = db.row("SELECT * FROM threads WHERE id = :t", &[(":t", &tid)])? else { return Ok(None) };
    let status = t["status"].as_str().unwrap_or("");
    let mut text = match old {
        Some(o) => o.to_string(),
        None => format!(
            "# {}\n\nThread {tid} | kind {} | channel {} | status {status} | opened by {} | created {}\n",
            one_line(t["subject"].as_str().unwrap_or("")),
            crate::kinds::kind_of(db, tid)?,
            t["channel"].as_str().unwrap_or(""),
            name(t["opened_by"].as_str().unwrap_or("")),
            t["created_ts"].as_str().unwrap_or("")
        ),
    };
    let (ids, mut seen, last_status) = scan(&text);
    let msgs = db.rows("SELECT id, ts, author, author_kind, body, reply_to FROM messages WHERE thread_id = :t ORDER BY id", &[(":t", &tid)])?;
    let mut add = String::new();
    for m in msgs.iter().filter(|m| !ids.contains(&m["id"].as_i64().unwrap_or(0))) {
        add.push_str(&block(m));
    }
    let mut footer = |l: String| {
        if seen.insert(l.clone()) {
            add.push_str(&l);
            add.push('\n');
        }
    };
    for d in
        db.rows("SELECT d.message_id, d.method, d.state FROM deliveries d JOIN messages m ON m.id = d.message_id WHERE m.thread_id = :t ORDER BY d.message_id", &[(":t", &tid)])?
    {
        footer(format!("> delivery #{}: {} {}", d["message_id"], one_line(d["method"].as_str().unwrap_or("")), d["state"].as_str().unwrap_or("")));
    }
    for a in db.rows("SELECT message_id, agent, state FROM acks WHERE thread_id = :t ORDER BY message_id, agent", &[(":t", &tid)])? {
        footer(format!("> ack #{} by {}: {}", a["message_id"], name(a["agent"].as_str().unwrap_or("")), a["state"].as_str().unwrap_or("")));
    }
    if old.is_some() && last_status.as_deref() != Some(status) {
        add.push_str(&format!("> status: {status}\n"));
    }
    if !add.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&add);
    Ok(Some(text))
}

/// Writes every thread the person took part in; returns how many files were created or appended to.
pub fn write_all(db: &Db, root: &Path) -> Result<usize> {
    let mut changed = 0;
    let mut listing = Listing::new();
    for r in db.rows("SELECT id, created_ts, subject FROM threads WHERE id IN (SELECT thread_id FROM messages WHERE author_kind = 'human') ORDER BY id", &[])? {
        let tid = r["id"].as_i64().unwrap_or(0);
        let file = file_of(root, &mut listing, tid, r["created_ts"].as_str().unwrap_or(""), r["subject"].as_str().unwrap_or(""));
        let old = std::fs::read_to_string(&file).ok();
        let Some(new) = render(db, tid, old.as_deref())? else { continue };
        if old.as_deref() != Some(&new) {
            paths::write_atomic(&file, new.as_bytes())?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn git_ok(root: &Path, args: &[&str]) -> Option<String> {
    let o = paths::new_command("git").args(args).current_dir(root).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// One sync: render, init the repository if it is absent, commit and push (a failure is in the result, never raised).
pub fn sync(db: &Db, root: &Path) -> Result<Value> {
    std::fs::create_dir_all(root)?;
    let written = write_all(db, root)?;
    if !root.join(".git").exists() && git_ok(root, &["init", "-q", "-b", "main"]).is_none() {
        return Ok(json!({"files_changed": written, "committed": false, "pushed": false, "error": "git init failed"}));
    }
    let mut g = vault_threads::sync(root, &["."], "conversations");
    if git_ok(root, &["remote"]).is_none_or(|r| r.is_empty()) && g["error"].as_str().is_some_and(|e| e.starts_with("push failed")) {
        g["error"] = Value::Null; // no remote configured: nothing to push to
        g["unpushed"] = Value::Null;
    }
    g["files_changed"] = json!(written);
    Ok(g)
}

/// The backup's hourly call: the person's repository, as a clause for the backup's report. Never fails the backup.
pub fn sync_for_backup(db: &Db) -> String {
    match sync(db, &repo_dir()) {
        Ok(g) => match g["error"].as_str() {
            Some(e) => format!(", conversations: {} file(s), GIT ERROR: {e}", g["files_changed"]),
            None => format!(", conversations: {} file(s) changed{}", g["files_changed"], if g["pushed"] == true { ", pushed" } else { "" }),
        },
        Err(e) => format!(", conversations FAILED: {e:#}"),
    }
}

// --- restore ------------------------------------------------------------------

#[derive(Debug, Default)]
struct Msg {
    id: i64,
    author: String,
    ts: String,
    kind: String,
    reply_to: Option<i64>,
    body: String,
}

#[derive(Debug, Default)]
struct Thread {
    id: i64,
    subject: String,
    kind: String,
    channel: String,
    status: String,
    opened_by: String,
    created: String,
    msgs: Vec<Msg>,
    footers: usize,
}

fn finish(cur: Option<(Msg, Vec<&str>)>, t: &mut Thread) {
    if let Some((mut m, body)) = cur {
        let body: Vec<&str> = body.into_iter().skip_while(|l| l.is_empty()).collect();
        m.body = body.iter().map(|l| l.strip_prefix("  ").unwrap_or(l)).collect::<Vec<_>>().join("\n").trim_end_matches('\n').to_string();
        t.msgs.push(m);
    }
}

fn parse(text: &str) -> Option<Thread> {
    let mut t = Thread::default();
    let mut lines = text.lines();
    t.subject = lines.next()?.strip_prefix("# ")?.to_string();
    let info = lines.find(|l| l.starts_with("Thread "))?;
    for (i, part) in info.split(" | ").enumerate() {
        match part.split_once(' ') {
            Some((_, n)) if i == 0 => t.id = n.parse().ok()?,
            Some(("kind", v)) => t.kind = v.into(),
            Some(("channel", v)) => t.channel = v.into(),
            Some(("status", v)) => t.status = v.into(),
            Some(("opened", v)) => t.opened_by = v.strip_prefix("by ")?.into(),
            Some(("created", v)) => t.created = v.into(),
            _ => {}
        }
    }
    let mut cur: Option<(Msg, Vec<&str>)> = None;
    for l in lines {
        if let Some(h) = l.strip_prefix("### ") {
            finish(cur.take(), &mut t);
            let (author, ts) = h.rsplit_once(' ')?;
            cur = Some((Msg { author: author.into(), ts: ts.into(), ..Msg::default() }, vec![]));
        } else if let Some(f) = l.strip_prefix("> ") {
            finish(cur.take(), &mut t);
            match f.strip_prefix("status: ") {
                Some(s) => t.status = s.into(),
                None => t.footers += 1,
            }
        } else if let Some((m, body)) = cur.as_mut() {
            if m.id == 0 && l.starts_with("id: ") {
                for (i, part) in l.split(" | ").enumerate() {
                    match part.split_once(": ") {
                        Some(("id", v)) if i == 0 => m.id = v.parse().ok()?,
                        Some(("kind", v)) => m.kind = v.into(),
                        Some(("reply_to", v)) => m.reply_to = v.parse().ok(),
                        _ => {}
                    }
                }
            } else {
                body.push(l);
            }
        }
    }
    finish(cur.take(), &mut t);
    Some(t)
}

fn read_repo(root: &Path) -> Result<Vec<Thread>> {
    let mut out = vec![];
    for y in std::fs::read_dir(root)?.flatten().filter(|e| e.path().is_dir() && e.file_name() != ".git") {
        for f in std::fs::read_dir(y.path())?.flatten().filter(|f| f.path().extension().is_some_and(|x| x == "md")) {
            match parse(&std::fs::read_to_string(f.path())?) {
                Some(t) if t.id > 0 => out.push(t),
                _ => bail!("not a conversation file: {}", f.path().display()),
            }
        }
    }
    out.sort_by_key(|t| t.id);
    Ok(out)
}

/// `agentdesk conversations restore <repo> [--dry-run=false]`: counts what the files hold; imports only into an empty board. Deliveries and
/// acks are counted, not imported (they are transient delivery state, not conversation).
pub fn restore(db: &Db, root: &Path, dry_run: bool) -> Result<String> {
    if !root.is_dir() {
        bail!("no such repository: {}", root.display());
    }
    let threads = read_repo(root)?;
    let messages: usize = threads.iter().map(|t| t.msgs.len()).sum();
    let footers: usize = threads.iter().map(|t| t.footers).sum();
    let summary = format!("{} thread(s), {messages} message(s), {footers} delivery/ack footer(s) not imported", threads.len());
    if dry_run {
        return Ok(format!("DRY RUN -- nothing changed. Pass --dry-run=false to import into an empty board.\n  {}: {summary}", root.display()));
    }
    let (have_t, have_m) = (db.scalar_i64("SELECT COUNT(*) FROM threads", &[])?.unwrap_or(0), db.scalar_i64("SELECT COUNT(*) FROM messages", &[])?.unwrap_or(0));
    if have_t + have_m > 0 {
        bail!("refused: the board is not empty ({have_t} thread(s), {have_m} message(s)); a conversations restore only imports into an empty board");
    }
    db.tx(|| {
        for t in &threads {
            let updated = t.msgs.last().map(|m| m.ts.clone()).unwrap_or_else(|| t.created.clone());
            let meta = (t.kind != "ask" && !t.kind.is_empty()).then(|| json!({"type": t.kind}).to_string());
            db.exec(
                "INSERT INTO threads (id, created_ts, updated_ts, channel, subject, opened_by, status, meta) VALUES (:id,:c,:u,:ch,:s,:o,:st,:m)",
                &[(":id", &t.id), (":c", &t.created), (":u", &updated), (":ch", &t.channel), (":s", &t.subject), (":o", &t.opened_by), (":st", &t.status), (":m", &meta)],
            )?;
            for m in &t.msgs {
                db.exec(
                    "INSERT INTO messages (id, ts, thread_id, author, author_kind, body, reply_to) VALUES (:id,:ts,:t,:a,:k,:b,:r)",
                    &[(":id", &m.id), (":ts", &m.ts), (":t", &t.id), (":a", &m.author), (":k", &m.kind), (":b", &m.body), (":r", &m.reply_to)],
                )?;
            }
        }
        Ok(())
    })?;
    Ok(format!("imported {summary}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_identities::Env;

    fn add(e: &Env) -> i64 {
        let t = e.db.start_thread("question", "Which port should the bridge use?", "john", "human", "I think 8080.\n\n### not a heading\n  indented", None, None).unwrap();
        e.db.reply(t, "worker_one", "agent", "8080 works", Some(1), None).unwrap();
        t
    }

    fn file(e: &Env, root: &Path, t: i64) -> PathBuf {
        file_of(root, &mut Listing::new(), t, &e.db.scalar_str("SELECT created_ts FROM threads WHERE id=:t", &[(":t", &t)]).unwrap().unwrap(), "x")
    }

    #[test]
    fn a_rerun_with_nothing_new_changes_nothing_and_new_messages_only_append() {
        let e = Env::new();
        let root = e.dir.path().join("conv");
        let t = add(&e);
        e.db.start_thread("discussion", "agents only", "bot", "agent", "no human here", None, None).unwrap();
        assert_eq!(write_all(&e.db, &root).unwrap(), 1, "only the thread the person took part in");
        let f = file(&e, &root, t);
        assert!(f.to_string_lossy().ends_with(&format!("{t}-which-port-should-the-bridge-use.md")), "{f:?}");
        let first = std::fs::read_to_string(&f).unwrap();
        assert_eq!(write_all(&e.db, &root).unwrap(), 0, "a rerun writes nothing");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), first);
        e.db.reply(t, "john", "human", "then 8080", None, None).unwrap();
        e.db.exec("UPDATE threads SET status='answered' WHERE id=:t", &[(":t", &t)]).unwrap();
        assert_eq!(write_all(&e.db, &root).unwrap(), 1);
        let second = std::fs::read_to_string(&f).unwrap();
        assert!(second.starts_with(&first), "the old text is untouched: the diff is pure additions");
        assert!(second.len() > first.len() && second.contains("  then 8080") && second.ends_with("> status: answered\n"));
        assert_eq!(write_all(&e.db, &root).unwrap(), 0);
    }

    #[test]
    fn tokens_are_redacted() {
        let e = Env::new();
        let root = e.dir.path().join("conv");
        e.db.start_thread("discussion", "keys", "john", "human", &format!("key ghp_{}", "a1B2".repeat(9)), None, None).unwrap();
        write_all(&e.db, &root).unwrap();
        let t = &read_repo(&root).unwrap()[0];
        assert_eq!(t.msgs[0].body, "key [redacted token]");
    }

    #[test]
    fn a_token_in_a_subject_reaches_neither_the_path_nor_the_file() {
        let e = Env::new();
        let root = e.dir.path().join("conv");
        let token = format!("ghp_{}", "a1B2".repeat(9));
        let t = e.db.start_thread("discussion", &format!("leak {token}"), "john", "human", "hi", None, None).unwrap();
        assert_eq!(write_all(&e.db, &root).unwrap(), 1);
        let files: Vec<PathBuf> = std::fs::read_dir(root.join(&e.db.scalar_str("SELECT created_ts FROM threads WHERE id=:t", &[(":t", &t)]).unwrap().unwrap()[..4]))
            .unwrap()
            .map(|f| f.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        let (path, text) = (files[0].to_string_lossy().to_string(), std::fs::read_to_string(&files[0]).unwrap());
        assert!(path.ends_with(&format!("{t}-leak-redacted-token.md")), "{path}");
        let name = files[0].file_name().unwrap().to_string_lossy().to_lowercase();
        for s in [path.as_str(), text.as_str()] {
            assert!(!s.contains(&token) && !s.contains("ghp_") && !s.contains("a1B2"), "{s}");
        }
        assert!(!name.contains("ghp") && !name.contains("a1b2"), "the slug lowercases and dashes, so check it that way too: {name}");
        e.db.exec("UPDATE threads SET subject='renamed' WHERE id=:t", &[(":t", &t)]).unwrap();
        e.db.reply(t, "john", "human", "more", None, None).unwrap();
        assert_eq!(write_all(&e.db, &root).unwrap(), 1);
        assert_eq!(file(&e, &root, t), files[0], "a renamed subject keeps its file");
        assert!(std::fs::read_to_string(&files[0]).unwrap().contains("  more"));
    }

    #[test]
    fn sync_inits_the_repository_commits_once_and_a_rerun_makes_no_commit() {
        let e = Env::new();
        let root = e.dir.path().join("conv");
        std::env::set_var("AGENTDESK_GIT_AUTHOR_NAME", "t");
        std::env::set_var("AGENTDESK_GIT_AUTHOR_EMAIL", "t@example.com");
        add(&e);
        let g = sync(&e.db, &root).unwrap();
        let again = sync(&e.db, &root).unwrap();
        std::env::remove_var("AGENTDESK_GIT_AUTHOR_NAME");
        std::env::remove_var("AGENTDESK_GIT_AUTHOR_EMAIL");
        assert_eq!(g["committed"], true, "{g}");
        assert!(g["error"].is_null(), "no remote is not an error: {g}");
        assert_eq!(git_ok(&root, &["rev-list", "--count", "HEAD"]).as_deref(), Some("1"));
        assert_eq!(again["committed"], false, "{again}");
        assert_eq!(git_ok(&root, &["rev-list", "--count", "HEAD"]).as_deref(), Some("1"));
    }

    #[test]
    fn the_files_round_trip_into_a_fresh_board_and_only_an_empty_board() {
        let e = Env::new();
        let root = e.dir.path().join("conv");
        let t = add(&e);
        e.db.reply(t, "john", "human", "line one\n\nline three", None, None).unwrap();
        e.db.exec("UPDATE threads SET status='answered' WHERE id=:t", &[(":t", &t)]).unwrap();
        write_all(&e.db, &root).unwrap();
        let fresh = Db::open(&e.dir.path().join("fresh.db")).unwrap();
        fresh.init().unwrap();
        assert!(restore(&fresh, &root, true).unwrap().contains("1 thread(s), 3 message(s)"));
        assert_eq!(fresh.scalar_i64("SELECT COUNT(*) FROM messages", &[]).unwrap(), Some(0), "a dry run imports nothing");
        assert!(restore(&e.db, &root, false).unwrap_err().to_string().contains("not empty"));
        restore(&fresh, &root, false).unwrap();
        for q in [
            "SELECT id, ts, thread_id, author, author_kind, body, reply_to FROM messages ORDER BY id",
            "SELECT id, created_ts, channel, subject, opened_by, status FROM threads ORDER BY id",
        ] {
            assert_eq!(e.db.rows(q, &[]).unwrap(), fresh.rows(q, &[]).unwrap(), "{q}");
        }
        assert!(restore(&fresh, &root, false).is_err(), "a second import is refused");
    }
}
