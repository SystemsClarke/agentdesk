//! Identities: long-lived, named Claude sessions the supervisor (`sessions.rs`, `service.rs`) keeps running, resumes after a restart, and
//! hands from one generation to the next (Phoenix) when a session runs out of room.
//!
//! This module is the board side: the rows, the prompts and the launch arguments. It starts nothing. `state` is the *desired* state
//! (`queued` = start it, `running`, `stopped`): `agentdesk identity start|stop` and the `retire` tool only write it, and the supervisor,
//! which owns the child processes, makes it so. That is what lets any process (a hook, the window, the CLI) ask for a change.
//!
//! Behaviour follows the .NET `Identities.cs`: the charter text, `--resume` only when claude has written a transcript, the state file
//! given verbatim to a successor, the 2-minute cooldown between restarts, the pending-handoff prompt for a launch that never happened.

use crate::db::{err, now_iso, Db};
use crate::harness;
use crate::paths;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const MODELS: [&str; 3] = ["haiku", "sonnet", "opus"];
/// Two restarts of one identity are at least this far apart: a successor that hands off at once is not restarted again.
pub const COOLDOWN_MINUTES: i64 = 2;

/// Prepended to every identity's system prompt; `{0}` is its name.
const CHAIN: &str = "You are one generation of {0}, a long-lived AgentDesk agent. The identity {0} continues past this session.
Your successor starts automatically from your handoff, and nothing you did not write down reaches it.
If you work on a goal (its lead or a member), the core runs its measure (experiment_done) and wakes its lead: never report a measured value yourself.
When you get the 60% context warning (PHOENIX), finish the step you are on and call pass_the_torch
with a standalone handoff: what you own, what is mid-flight, and what is next.
Then stop. The system ends this session and starts your successor from that handoff.
When your work is done and nothing is waiting on you, call retire, then stop: it ends this session and frees its slot.
John's reply, or the core's next wake, starts you again with your conversation intact.";

const STATE_CHARTER: &str = "Your state file is {0}. It is your working memory across generations, and the one thing that survives you whole: a handoff written
in a hurry loses things, and this does not have to. Keep it current by EDITING it in place (rewrite a line, delete what stopped being
true; never just append a log), kept short and structured: DECISIONS (each with why), OPEN (what is mid-flight and its next step),
TRIED and UNTRIED (ideas, with what happened), KEY FACTS to keep verbatim (ids, paths, exact commands and strings, numbers), and WHO
owns what. Update it after each decision and before pass_the_torch. Your successor is given it verbatim along with your handoff.";

/// `settings.json`'s `max_sessions`: how many identities may run at once (default 5).
pub fn max_sessions() -> usize {
    paths::setting("max_sessions").as_u64().map(|n| n as usize).unwrap_or(5).max(1)
}

fn state_file_on() -> bool {
    !paths::setting("phoenix_state_file").as_bool().map(|b| !b).unwrap_or(false)
}

/// How much of the state file goes inline into a successor's prompt: `phoenix_state_chars` (500 to 20,000), default 12,000.
fn state_limit() -> usize {
    paths::setting("phoenix_state_chars").as_u64().map(|n| (n as usize).clamp(500, 20_000)).unwrap_or(12_000)
}

/// An identity's state file: its working memory across generations, in the data folder.
pub fn state_path(name: &str) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    paths::data_dir().join("state").join(format!("{safe}.md"))
}

/// The state file as the last generation left it, to put in front of the next (None when there is none or it is off), cut at the limit
/// with a note, so one runaway file cannot fill the successor's context.
fn state_for_successor(name: &str) -> Option<String> {
    if !state_file_on() {
        return None;
    }
    let path = state_path(name);
    let text = std::fs::read_to_string(&path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let limit = state_limit();
    let body = if text.chars().count() > limit {
        format!("{}\n[... cut here: the file is {} characters; read the rest at {}]", text.chars().take(limit).collect::<String>(), text.chars().count(), path.display())
    } else {
        text.to_string()
    };
    Some(format!("Your state file ({}), exactly as your previous generation left it. Trust it over the handoff where they differ, and keep it current:\n\n{body}", path.display()))
}

/// The first prompt of generation `generation`: the handoff, then the state file verbatim.
pub fn successor_prompt(name: &str, generation: i64, handoff: &str) -> String {
    let mut p = format!("You are {name}, generation {generation}. Your previous generation handed off with:\n\n{handoff}");
    if let Some(state) = state_for_successor(name) {
        p.push_str("\n\n");
        p.push_str(&state);
    }
    p
}

// ---- the rows

pub fn get(db: &Db, name: &str) -> Result<Option<Value>> {
    db.row("SELECT * FROM identities WHERE name = :n", &[(":n", &name)])
}

pub fn list(db: &Db) -> Result<Vec<Value>> {
    db.rows("SELECT * FROM identities ORDER BY name COLLATE NOCASE", &[])
}

pub fn create(db: &Db, name: &str, folder: &str, charter: Option<&str>, model: Option<&str>, autostart: bool) -> Result<Value> {
    create_with(db, name, folder, charter, model, autostart, None)
}

/// `create`, on a given harness (default `claude-code`).
pub fn create_with(db: &Db, name: &str, folder: &str, charter: Option<&str>, model: Option<&str>, autostart: bool, harness: Option<&str>) -> Result<Value> {
    let harness = harness.map(str::trim).filter(|h| !h.is_empty()).unwrap_or(harness::DEFAULT);
    if !harness::known(harness) {
        return err(format!("unknown harness: {harness}"));
    }
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return err("name is required: letters, digits, - _ .");
    }
    if !std::path::Path::new(folder).is_dir() {
        return err(format!("no such folder: {folder}"));
    }
    let model = model.map(|m| m.trim().to_lowercase()).filter(|m| !m.is_empty()).unwrap_or_else(|| "sonnet".to_string());
    if !MODELS.contains(&model.as_str()) {
        return err(format!("model must be haiku, sonnet or opus, got {model}"));
    }
    if get(db, name)?.is_some() {
        return err(format!("identity already exists: {name}"));
    }
    let ts = now_iso();
    db.exec(
        "INSERT INTO identities (name, folder, charter, host, state, autostart, created_ts, updated_ts, model, harness) VALUES (:n,:f,:c,'local','stopped',:a,:ts,:ts,:m,:h)",
        &[(":n", &name), (":f", &folder), (":c", &charter), (":a", &(autostart as i64)), (":ts", &ts), (":m", &model), (":h", &harness)],
    )?;
    Ok(get(db, name)?.unwrap_or(Value::Null))
}

/// Is `folder` inside the system temp directory (compared as real paths, so a short `PROFILE~1` spelling matches)?
fn in_temp_dir(folder: &str) -> bool {
    match (std::fs::canonicalize(folder), std::fs::canonicalize(std::env::temp_dir())) {
        (Ok(f), Ok(tmp)) => f.starts_with(tmp),
        _ => false,
    }
}

/// Records in Claude Code's own config (`~/.claude.json`, or `AGENTDESK_CLAUDE_JSON`) that the user trusts `folder`, so a session AgentDesk
/// starts there is not stopped at "Is this a project you trust?": nobody is at a hosted session to press Enter, and it would sit there
/// forever without taking a turn. Called where the user CHOOSES the folder (a new goal, a new identity, `agentdesk trust`), never for a
/// folder an agent picked. Returns whether anything changed; a config that is missing or not valid JSON is left alone.
pub fn trust_folder(folder: &str) -> Result<bool> {
    let explicit = std::env::var_os("AGENTDESK_CLAUDE_JSON").map(std::path::PathBuf::from);
    if explicit.is_none() && in_temp_dir(folder) {
        return Ok(false); // a scratch folder (a test's, a build's) is never a working folder: the real config is not filled with them
    }
    let path = explicit.unwrap_or_else(|| crate::paths::home_dir().join(".claude.json"));
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(false) };
    let mut cfg: Value = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{} is not valid JSON ({e}): left alone", path.display()))?;
    let norm = |s: &str| s.trim_start_matches(r"\\?\").replace('\\', "/").trim_end_matches('/').to_lowercase();
    let key = folder.trim_start_matches(r"\\?\").replace('\\', "/").trim_end_matches('/').to_string();
    let Some(root) = cfg.as_object_mut() else { return Ok(false) };
    let projects = root.entry("projects").or_insert_with(|| json!({}));
    let Some(projects) = projects.as_object_mut() else { return Ok(false) };
    let existing = projects.keys().find(|k| norm(k) == norm(&key)).cloned().unwrap_or(key);
    let entry = projects.entry(existing).or_insert_with(|| json!({}));
    if entry["hasTrustDialogAccepted"] == true {
        return Ok(false);
    }
    let Some(entry) = entry.as_object_mut() else { return Ok(false) };
    entry.insert("hasTrustDialogAccepted".into(), json!(true));
    crate::paths::write_atomic(&path, serde_json::to_string_pretty(&cfg)?)?;
    Ok(true)
}

fn need(db: &Db, name: &str) -> Result<Value> {
    match get(db, name)? {
        Some(r) => Ok(r),
        None => err(format!("no such identity: {name}")),
    }
}

/// Ask for it to run: `queued`. The supervisor starts it when it has a slot. A running one is left alone.
pub fn start(db: &Db, name: &str) -> Result<Value> {
    let row = need(db, name)?;
    if row["state"].as_str() == Some("stopped") {
        db.exec("UPDATE identities SET state='queued', updated_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name)])?;
    }
    need(db, name)
}

/// Ask for it to stop: its conversation is kept (`start` resumes it).
/// `start`, with a first prompt for it (a reply to deliver, a goal wake, a task): kept until the supervisor launches it, then consumed.
/// A running identity is left alone (its caller types into the session instead).
pub fn start_with_prompt(db: &Db, name: &str, prompt: &str) -> Result<Value> {
    let row = need(db, name)?;
    if row["state"].as_str() != Some("running") {
        db.exec("UPDATE identities SET wake_prompt=:p, state='queued', updated_ts=:ts WHERE name=:n", &[(":p", &prompt), (":ts", &now_iso()), (":n", &name)])?;
    }
    need(db, name)
}

pub fn stop(db: &Db, name: &str) -> Result<Value> {
    need(db, name)?;
    db.exec("UPDATE identities SET state='stopped', pid=NULL, phoenix_msg=NULL, phoenix_go=0, updated_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name)])?;
    need(db, name)
}

pub fn forget(db: &Db, name: &str) -> Result<Value> {
    need(db, name)?;
    db.exec("DELETE FROM identities WHERE name=:n", &[(":n", &name)])?;
    Ok(json!({"forgotten": name}))
}

/// `retire`, called by an identity's own session: like typing /exit. Stopped, not forgotten; the supervisor ends the process.
pub fn retire(db: &Db, name: &str) -> Result<Value> {
    let row = need(db, name)?;
    if row["state"].as_str() != Some("running") {
        return err(format!("{name} is not running, so there is nothing to retire"));
    }
    stop(db, name)?;
    Ok(json!({"retiring": name, "note": "Your session is being ended now: stop."}))
}

/// `pass_the_torch` from an identity's own session records which message holds its handoff; the Stop hook then says the turn is over.
pub fn torch_passed(db: &Db, name: &str, session_id: &str, bio_thread: i64, author: &str) {
    let _ = db.exec(
        "UPDATE identities SET phoenix_msg=(SELECT MAX(id) FROM messages WHERE thread_id=:t AND author=:a) \
         WHERE name=:n AND claude_session_id=:s AND state='running'",
        &[(":t", &bio_thread), (":a", &author), (":n", &name), (":s", &session_id)],
    );
}

/// The Stop hook let the turn end: a session with a recorded handoff may now be restarted.
pub fn turn_ended(db: &Db, session_id: &str) {
    let _ = db.exec("UPDATE identities SET phoenix_go=1 WHERE claude_session_id=:s AND phoenix_msg IS NOT NULL AND state='running'", &[(":s", &session_id)]);
}

/// The prompt for a successor whose launch never happened: Phoenix had recorded the handoff and cleared the old session id, then the
/// supervisor went down before the replacement started. Such an identity has no conversation, its generation is one past its latest
/// chain row, and that row names the handoff. None when it is not in that gap.
pub fn pending_successor_prompt(db: &Db, row: &Value) -> Option<String> {
    if !row["claude_session_id"].is_null() {
        return None;
    }
    let name = row["name"].as_str()?;
    let generation = row["generation"].as_i64()?;
    let last = db.row("SELECT generation, handoff_msg FROM phoenix_chain WHERE identity=:n ORDER BY generation DESC LIMIT 1", &[(":n", &name)]).ok()??;
    let msg = last["handoff_msg"].as_i64()?;
    if generation != last["generation"].as_i64()? + 1 {
        return None;
    }
    let body = db.row("SELECT body FROM messages WHERE id=:m", &[(":m", &msg)]).ok()??["body"].as_str()?.to_string();
    let handoff = body.split_once("\n\n").map(|(_, rest)| rest).unwrap_or(&body); // after pass_the_torch's "Handoff recorded" line
    Some(successor_prompt_for(db, name, generation, handoff))
}

/// `successor_prompt`, plus the status of the goal the identity leads or works on (a Phoenix successor starts knowing where the goal is).
pub fn successor_prompt_for(db: &Db, name: &str, generation: i64, handoff: &str) -> String {
    let mut p = successor_prompt(name, generation, handoff);
    if let Some(ctx) = crate::goals::context(db, name) {
        p.push_str("\n\n");
        p.push_str(&ctx);
    }
    p
}

// ---- launching

/// How to start this identity, through its harness (`harness::Harness::start`): the chain charter plus its own as the session's
/// instructions, the state folder, the model (the one asked for, else its stored tier), then the prompt as the first message.
/// Returns (launch, session id).
pub fn launch(row: &Value, prompt: Option<&str>, model: Option<&str>) -> (harness::Launch, String) {
    let name = row["name"].as_str().unwrap_or("");
    let mut charter = CHAIN.replace("{0}", name);
    let state_dir = state_file_on().then(|| {
        let path = state_path(name);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        charter.push('\n');
        charter.push_str(&STATE_CHARTER.replace("{0}", &path.display().to_string()));
        path.parent().unwrap().to_path_buf()
    });
    if let Some(own) = row["charter"].as_str().filter(|c| !c.is_empty()) {
        charter.push_str("\n\n");
        charter.push_str(own);
    }
    let spec = harness::StartSpec {
        session_id: row["claude_session_id"].as_str(),
        model: model.or_else(|| row["model"].as_str()),
        system_prompt: &charter,
        add_dir: state_dir.as_deref(),
        prompt,
    };
    harness::of(row).start(&spec)
}

/// `launch`'s arguments: (args, session id).
#[cfg_attr(not(test), allow(dead_code))] // tests read it
pub fn launch_args(row: &Value, prompt: Option<&str>, model: Option<&str>) -> (Vec<String>, String) {
    let (l, id) = launch(row, prompt, model);
    (l.args, id)
}

/// Phoenix needs a free recorded-restart slot: two minutes since the identity's last chain row.
pub fn cooled_down(db: &Db, name: &str) -> bool {
    let last = db.scalar_str("SELECT MAX(ts) FROM phoenix_chain WHERE identity=:n", &[(":n", &name)]).ok().flatten();
    match last.and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()) {
        Some(t) => chrono::Utc::now().signed_duration_since(t) >= chrono::Duration::minutes(COOLDOWN_MINUTES),
        None => true,
    }
}
