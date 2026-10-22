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
        "INSERT INTO identities (name, folder, charter, host, state, autostart, created_ts, updated_ts, model) VALUES (:n,:f,:c,'local','stopped',:a,:ts,:ts,:m)",
        &[(":n", &name), (":f", &folder), (":c", &charter), (":a", &(autostart as i64)), (":ts", &ts), (":m", &model)],
    )?;
    Ok(get(db, name)?.unwrap_or(Value::Null))
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

/// claude writes no transcript until the first message, so only a conversation that has one can be resumed.
pub(crate) fn has_transcript(session_id: &str) -> bool {
    let root = std::env::var_os("AGENTDESK_CLAUDE_PROJECTS").map(PathBuf::from).unwrap_or_else(|| crate::paths::home_dir().join(".claude").join("projects"));
    std::fs::read_dir(root).into_iter().flatten().flatten().any(|d| d.path().join(format!("{session_id}.jsonl")).exists())
}

/// What to run for this identity: `--resume <id>` when there is a conversation to resume, else `--session-id` a new (or never-used) id,
/// the model tier, the state folder, the chain charter plus its own, then the prompt as the first message. Returns (args, session id).
pub fn launch_args(row: &Value, prompt: Option<&str>, model: Option<&str>) -> (Vec<String>, String) {
    let name = row["name"].as_str().unwrap_or("");
    let id = row["claude_session_id"].as_str().map(String::from);
    let resume = id.as_deref().map(has_transcript).unwrap_or(false);
    let id = id.unwrap_or_else(new_session_id);
    let mut charter = CHAIN.replace("{0}", name);
    let state_on = state_file_on();
    if state_on {
        let path = state_path(name);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        charter.push('\n');
        charter.push_str(&STATE_CHARTER.replace("{0}", &path.display().to_string()));
    }
    if let Some(own) = row["charter"].as_str().filter(|c| !c.is_empty()) {
        charter.push_str("\n\n");
        charter.push_str(own);
    }
    let tier = model.or_else(|| row["model"].as_str()).filter(|m| MODELS.contains(m));
    let mut args = vec![if resume { "--resume" } else { "--session-id" }.to_string(), id.clone()];
    if let Some(t) = tier {
        args.extend(["--model".to_string(), t.to_string()]);
    }
    if state_on {
        args.extend(["--add-dir".to_string(), state_path(name).parent().unwrap().display().to_string()]);
    }
    args.extend(["--append-system-prompt".to_string(), charter]);
    if let Some(p) = prompt {
        args.push(p.to_string());
    }
    (args, id)
}

fn new_session_id() -> String {
    // A v4-shaped UUID from the clock and the process: claude only needs it unique and well-formed.
    let mut h: [u8; 16] = [0; 16];
    let seed = format!("{:?}{}{:?}", std::time::SystemTime::now(), std::process::id(), std::thread::current().id());
    let mut x: u64 = seed.bytes().fold(0xcbf29ce484222325, |a, b| (a ^ u64::from(b)).wrapping_mul(0x100000001b3));
    for b in h.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
    h[6] = (h[6] & 0x0f) | 0x40;
    h[8] = (h[8] & 0x3f) | 0x80;
    let hex: Vec<String> = h.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", hex[0..4].concat(), hex[4..6].concat(), hex[6..8].concat(), hex[8..10].concat(), hex[10..16].concat())
}

/// Phoenix needs a free recorded-restart slot: two minutes since the identity's last chain row.
pub fn cooled_down(db: &Db, name: &str) -> bool {
    let last = db.scalar_str("SELECT MAX(ts) FROM phoenix_chain WHERE identity=:n", &[(":n", &name)]).ok().flatten();
    match last.and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()) {
        Some(t) => chrono::Utc::now().signed_duration_since(t) >= chrono::Duration::minutes(COOLDOWN_MINUTES),
        None => true,
    }
}
