//! The Slack bridge in Rust: run by the core as threads (`bridge.rs` supervises it, with the
//! same heartbeat file and `ui:status` bridge section as the Python one) instead of a supervised Python process. Selected by
//! `settings.json`'s `"slack_native"` (see `bridge.rs`).
//!
//! Outbound: polls the board's open questions (the view that drives the toast and the title-bar count) and posts each as a new Slack
//! thread in John's DM. `bridge_state.json` (next to the credentials) maps thread id -> Slack thread_ts and the `updated_ts` last seen,
//! so a restart does not re-post a question already in Slack, but one that goes open -> answered -> asked again does get re-posted.
//!
//! Inbound: Socket Mode delivers message events. A reply *inside* one of those Slack threads answers the AgentDesk thread exactly as John
//! answering from the window does (`Db::john_replies`, then `meta.via = slack`). A top-level DM such as `status`, `agents` or
//! `concierge off` runs a phone command (`slack_cmd.rs`); swarm slots are `swarm.rs`.
//!
//! Credentials (not in the repo): `bot-token.txt` (xoxb-...) and `app-token.txt` (xapp-...) in `~/.claude/slack-notify` (or
//! `AGENTDESK_SLACK_DIR`), optionally `bots.json` for several bots. `SLACK_BOT_TOKEN` / `SLACK_APP_TOKEN` in the environment stand in for
//! one bot's files (a test, a container), and `SLACK_API_URL` replaces `https://slack.com/api` (a fake Slack).
//!
//! Typing into a swarm lead: the bridge runs inside the core, which owns the sessions, so it writes to the session's terminal directly
//! through the host's registry. A bridge run by hand (`agentdesk slack-bridge`) is another process and uses the attach socket.

use crate::db::Db;
use crate::sessions::Registry;
use crate::slack_cmd::{Commands, AREAS};
use crate::swarm::Swarms;
use crate::{paths, slackfmt};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_DM: &str = "D0C3KDM3DNX";
const DEFAULT_JOHN: &str = "U0C4L06N4KA"; // only his messages in his DM are answers
const POLL_SECONDS: u64 = 15;

/// How often the outbound pass runs: every 15 s, or `AGENTDESK_SLACK_POLL` seconds (the parity suite shortens it).
fn poll() -> Duration {
    Duration::from_secs(env_token("AGENTDESK_SLACK_POLL").and_then(|s| s.parse().ok()).filter(|s| *s > 0).unwrap_or(POLL_SECONDS))
}

// ---- the Slack Web API

/// Where the Web API is: `SLACK_API_URL`, else Slack.
pub fn api_base() -> String {
    std::env::var("SLACK_API_URL").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| "https://slack.com/api".into()).trim_end_matches('/').to_string()
}

/// What the bridge needs from Slack: post a message (returns its ts) and fetch a private file.
pub trait Chat: Send + Sync {
    fn post(&self, channel: &str, thread_ts: Option<&str>, blocks: Option<Vec<Value>>, text: &str, broadcast: bool) -> Result<String>;
    fn download(&self, url: &str) -> Result<Vec<u8>>;
}

/// Any Web API method with this bot's token (the swarm slots use channels and history).
pub trait Api: Send + Sync {
    fn api(&self, method: &str, body: &Value) -> Result<Value>;
}

pub struct Web {
    token: String,
}

/// Slack answered `ok: false`: its error code (`missing_scope`, `channel_not_found`, ...).
#[derive(Debug)]
pub struct SlackError(pub String);

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "slack: {}", self.0)
    }
}

impl std::error::Error for SlackError {}

/// Slack's error code in `e`, if it is one.
pub fn slack_error(e: &anyhow::Error) -> Option<&str> {
    e.downcast_ref::<SlackError>().map(|s| s.0.as_str())
}

impl Web {
    pub fn new(token: &str) -> Web {
        Web { token: token.trim().to_string() }
    }

    /// One Web API call with `token`. `chat.*` takes JSON (blocks); the rest of the Web API is happiest with a form. A 429 is waited out
    /// once, as Slack's `Retry-After` says.
    pub fn call(&self, method: &str, token: &str, body: &Value) -> Result<Value> {
        for attempt in 0..2 {
            let req = ureq::post(&format!("{}/{method}", api_base())).set("Authorization", &format!("Bearer {token}")).timeout(Duration::from_secs(30));
            let r = if method.starts_with("chat.") {
                req.send_json(body.clone())
            } else {
                let pairs: Vec<(String, String)> =
                    body.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect()).unwrap_or_default();
                req.send_form(&pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>())
            };
            match r {
                Ok(resp) => {
                    let v: Value = resp.into_json()?;
                    return if v["ok"] == json!(true) { Ok(v) } else { Err(SlackError(v["error"].as_str().unwrap_or("not_ok").to_string()).into()) };
                }
                Err(ureq::Error::Status(429, resp)) if attempt == 0 => {
                    std::thread::sleep(Duration::from_secs(resp.header("Retry-After").and_then(|s| s.parse().ok()).unwrap_or(5).min(60)));
                }
                Err(e) => return Err(anyhow!("slack {method}: {e}")),
            }
        }
        Err(anyhow!("slack {method}: rate limited"))
    }
}

impl Api for Web {
    fn api(&self, method: &str, body: &Value) -> Result<Value> {
        self.call(method, &self.token, body)
    }
}

impl Chat for Web {
    fn post(&self, channel: &str, thread_ts: Option<&str>, blocks: Option<Vec<Value>>, text: &str, broadcast: bool) -> Result<String> {
        let mut body = json!({"channel": channel, "text": text, "unfurl_links": false, "unfurl_media": false});
        if let Some(b) = blocks {
            body["blocks"] = json!(b);
        }
        if let Some(t) = thread_ts {
            body["thread_ts"] = json!(t);
            if broadcast {
                body["reply_broadcast"] = json!(true);
            }
        }
        Ok(self.call("chat.postMessage", &self.token, &body)?["ts"].as_str().unwrap_or("").to_string())
    }

    fn download(&self, url: &str) -> Result<Vec<u8>> {
        let resp = ureq::get(url).set("Authorization", &format!("Bearer {}", self.token)).timeout(Duration::from_secs(30)).call()?;
        let mut out = vec![];
        resp.into_reader().take(25 * 1024 * 1024).read_to_end(&mut out)?;
        Ok(out)
    }
}

// ---- the state file

fn load(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({"posted": {}}))
}

fn save(path: &Path, state: &Value) {
    let _ = paths::write_atomic(path, serde_json::to_string_pretty(state).unwrap_or_default());
    // a crash mid-write must not lose every mapping
}

fn epoch(iso: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(iso).map(|t| t.timestamp() as f64).unwrap_or(0.0)
}

fn now_epoch() -> f64 {
    chrono::Utc::now().timestamp() as f64
}

/// `file:///C:/a%20b/c.png`, as Python's `Path.as_uri()` writes it.
fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let enc: String = s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/:._-~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    if enc.starts_with('/') {
        format!("file://{enc}")
    } else {
        format!("file:///{enc}")
    }
}

// ---- the question relay

/// The short form of an author for a phone, as `identity.label` does it: `claude-code:project#abcd` keeps the tag and shortens the project.
fn label(author: &str) -> String {
    const MAX: usize = 11;
    let fit = |t: &str, limit: usize| if t.chars().count() <= limit { t.to_string() } else { format!("{}…", t.chars().take(limit.saturating_sub(1).max(1)).collect::<String>()) };
    let author = author.trim();
    let (base, tag) = author.split_once('#').unwrap_or((author, ""));
    let Some((_harness, project)) = base.split_once(':') else { return fit(author, MAX) };
    if tag.is_empty() {
        return fit(project, MAX);
    }
    match MAX.checked_sub(tag.chars().count() + 1).filter(|room| *room >= 2) {
        Some(room) => format!("{}#{tag}", fit(project, room)),
        None => fit(tag, MAX),
    }
}

/// The board side of the bridge: owns the DM and `bridge_state.json`. Only one bot has it.
pub struct Bridge {
    pub chat: Arc<dyn Chat>,
    pub dm: String,
    pub john: String,
    pub state: PathBuf,
    pub attachments: PathBuf,
    lock: Mutex<()>,
    last_relay: Mutex<Option<Value>>,
}

fn delivery_note(status: &str) -> Option<&'static str> {
    Some(match status {
        "picked-up" => "✓ The agent picked up your reply.",
        "woke" | "injected" => "✓ Your reply woke the agent; it's working on it.",
        "resumed" => "✓ Woke the agent: resumed its session with your reply.",
        "late" => "⚠ The agent hasn't picked up your reply after 15 min. Reply *wake* here to resume its session.",
        _ => return None,
    })
}

impl Bridge {
    pub fn new(chat: Arc<dyn Chat>, dm: &str, john: &str, state: PathBuf, attachments: PathBuf) -> Bridge {
        Bridge { chat, dm: dm.into(), john: john.into(), state, attachments, lock: Mutex::new(()), last_relay: Mutex::new(None) }
    }

    fn state(&self) -> Value {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        load(&self.state)
    }

    fn put(&self, mutate: impl FnOnce(&mut Value)) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = load(&self.state);
        mutate(&mut s);
        save(&self.state, &s);
    }

    #[cfg(test)]
    pub fn put_for_test(&self) {
        self.put(|s| s["told"] = json!({}));
    }

    pub fn last_relay(&self) -> Option<Value> {
        self.last_relay.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Posts a question; a follow-up on one already in Slack goes into John's existing thread. Returns the thread's ts.
    fn post_question(&self, db: &Db, q: &Value, prev: Option<&Value>) -> Result<String> {
        let tid = q["thread_id"].as_i64().unwrap_or(0);
        let body = db.scalar_str("SELECT body FROM messages WHERE thread_id=:t ORDER BY id DESC LIMIT 1", &[(":t", &tid)])?.unwrap_or_default();
        let asker = label(q["opened_by"].as_str().filter(|a| !a.is_empty()).unwrap_or("an agent"));
        let subject = q["subject"].as_str().unwrap_or("");
        // A packet's steps are wrapped in hidden markers for the window; they are not for the phone.
        let body = body.replace(crate::kinds::STEPS_OPEN, "").replace(crate::kinds::STEPS_CLOSE, "");
        let kind = crate::kinds::kind_of(db, tid).unwrap_or(crate::kinds::ASK);
        let (noun, hint) = match kind {
            crate::kinds::PACKET => {
                ("Packet", "Reply *in this thread* to add a note. Tick steps and finish it on the packet's page, in a browser at your laptop (Alt+O in AgentDesk).")
            }
            crate::kinds::RUN => ("Run request", "Run the script in the AgentDesk window (Alt+R twice), or reply *in this thread* with what happened."),
            crate::kinds::PERMISSION => ("Permission rule", "Decide it in the AgentDesk window: Alt+A approves, Alt+D rejects. A reply here is only a note."),
            _ => ("Question", "Reply *in this thread* to answer. Photos work too."),
        };
        let head = match prev {
            None => format!("*☎ {noun} #{tid}* from *{}*\n*{}*", slackfmt::esc(&asker), slackfmt::inline(subject)),
            Some(_) => format!("*↩ Follow-up on #{tid}* from *{}*", slackfmt::esc(&asker)),
        };
        let mut blocks = vec![json!({"type": "section", "text": {"type": "mrkdwn", "text": head}}), json!({"type": "divider"})];
        blocks.extend(slackfmt::md_to_slack(&body));
        blocks.truncate(slackfmt::BLOCKS_MAX - 1); // the reply hint must survive a long body
        blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": hint}]}));
        let text = format!("{noun} #{tid} from {asker}: {subject} · {}", slackfmt::fallback_text(&body, 200));
        let prev_ts = prev.and_then(|p| p["ts"].as_str());
        let ts = self.chat.post(&self.dm, prev_ts, Some(blocks), &text, prev.is_some())?;
        Ok(prev_ts.map(String::from).unwrap_or(ts))
    }

    /// One outbound pass: new and re-asked questions, then a note per John's reply on whether it reached the agent.
    pub fn poll(&self, db: &Db) -> Result<()> {
        let state = self.state();
        for q in db.open_questions(false)? {
            let tid = q["thread_id"].as_i64().unwrap_or(0).to_string();
            let prev = state["posted"].get(&tid);
            if prev.is_some_and(|p| p["updated_ts"] == q["updated_ts"]) {
                continue;
            }
            let ts = self.post_question(db, &q, prev)?;
            // saved before anything else can fail, so a pass never re-posts
            self.put(|s| s["posted"][&tid] = json!({"ts": ts, "updated_ts": q["updated_ts"]}));
            *self.last_relay.lock().unwrap_or_else(|e| e.into_inner()) = Some(json!({"ts": crate::db::now_iso(), "thread_id": q["thread_id"]}));
        }
        self.tell_deliveries(db)
    }

    /// In each question's Slack thread, say once whether John's latest reply reached the agent.
    fn tell_deliveries(&self, db: &Db) -> Result<()> {
        let state = self.state();
        let Some(posted) = state["posted"].as_object() else { return Ok(()) };
        for (tid, info) in posted {
            let Ok(id) = tid.parse::<i64>() else { continue };
            let Some(h) = db.row("SELECT id, ts FROM messages WHERE thread_id=:t AND author_kind='human' ORDER BY id DESC LIMIT 1", &[(":t", &id)])? else { continue };
            let age = now_epoch() - epoch(h["ts"].as_str().unwrap_or(""));
            if age > 86400.0 {
                continue;
            }
            let mid = h["id"].as_i64().unwrap_or(0);
            let mut status = db
                .scalar_str(
                    "SELECT COALESCE(CASE WHEN a.state='posted' THEN 'picked-up' END, d.state, CASE WHEN a.message_id IS NOT NULL THEN 'pending' END, '') \
                     FROM messages h LEFT JOIN acks a ON a.message_id=h.id LEFT JOIN deliveries d ON d.message_id=h.id WHERE h.id=:m",
                    &[(":m", &mid)],
                )?
                .unwrap_or_default();
            if status == "pending" && age > 15.0 * 60.0 {
                status = "late".into();
            }
            let key = format!("{mid}:{status}");
            let Some(note) = delivery_note(&status) else { continue };
            if state["told"][tid].as_str() == Some(key.as_str()) {
                continue;
            }
            self.chat.post(&self.dm, info["ts"].as_str(), None, note, false)?;
            self.put(|s| s["told"][tid] = json!(key));
        }
        Ok(())
    }

    /// Images attached to a Slack reply, saved under the data folder; returns their paths.
    fn save_photos(&self, files: &[Value], thread_ts: &str) -> Vec<PathBuf> {
        let mut saved = vec![];
        for f in files {
            if !f["mimetype"].as_str().unwrap_or("").starts_with("image/") {
                continue;
            }
            let Some(url) = f["url_private_download"].as_str().or(f["url_private"].as_str()) else { continue };
            let name: String = f["name"].as_str().unwrap_or("photo.jpg").chars().filter(|c| c.is_alphanumeric() || "._-".contains(*c)).collect();
            let name = if name.is_empty() { "photo.jpg".to_string() } else { name };
            let dest = self.attachments.join(format!("{}_{}_{}", thread_ts.replace('.', "_"), f["id"].as_str().unwrap_or(""), name));
            let got = (|| -> Result<Vec<u8>> {
                std::fs::create_dir_all(&self.attachments)?;
                self.chat.download(url)
            })();
            match got {
                Ok(data) if data.first() == Some(&b'<') => {} // an HTML sign-in page, not the image: the files:read scope is missing
                Ok(data) => {
                    if std::fs::write(&dest, data).is_ok() {
                        saved.push(dest);
                    }
                }
                Err(e) => paths::log("WARN", &format!("slack: photo download failed: {e}")),
            }
        }
        saved
    }

    /// A message in John's DM that is not a command: an answer if it is inside a question's thread.
    pub fn handle_reply(&self, db: &Db, ev: &Value) -> Result<()> {
        let say = |text: &str, thread: Option<&str>| self.chat.post(&self.dm, thread, None, text, false).map(|_| ());
        if ev["channel"].as_str() != Some(self.dm.as_str())
            || ev["user"].as_str() != Some(self.john.as_str())
            || !ev["bot_id"].is_null()
            || !matches!(ev["subtype"].as_str(), None | Some("file_share"))
        {
            return Ok(()); // edits, deletes and anyone else's messages are not answers
        }
        let Some(thread_ts) = ev["thread_ts"].as_str() else {
            return say("Reply *inside the thread* of the question you're answering (use \"reply in thread\"), not as a new message.", None);
        };
        let tid = self.state()["posted"].as_object().and_then(|p| p.iter().find(|(_, v)| v["ts"].as_str() == Some(thread_ts)).and_then(|(k, _)| k.parse::<i64>().ok()));
        let Some(tid) = tid else {
            return say("I don't recognize that thread as an open question anymore (already answered, or from before this bridge started).", None);
        };
        let text = ev["text"].as_str().unwrap_or("");
        if matches!(text.trim().to_lowercase().trim_end_matches(['!', '.']), "wake" | "wake it" | "wake up") {
            let said = match crate::ui_api::wake(db, tid) {
                Ok(r) => r["said"].as_str().map(String::from).unwrap_or_else(|| "no answer from the core".into()),
                Err(e) => crate::db::anyhow_msg(&e),
            };
            let mut c = said.chars();
            let said = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
            return say(&format!("{said}."), Some(thread_ts));
        }
        let mut body = slackfmt::slack_to_md(text);
        let files: Vec<Value> = ev["files"].as_array().cloned().unwrap_or_default();
        let photos = self.save_photos(&files, thread_ts);
        if !photos.is_empty() {
            let links = photos.iter().map(|p| format!("![photo]({})", file_uri(p))).collect::<Vec<_>>().join("\n");
            body = if body.trim().is_empty() { links } else { format!("{body}\n\n{links}") };
        }
        if body.trim().is_empty() {
            return Ok(());
        }
        // On a packet or a permission question a reply is a note: the answer is Finish, or Approve / Reject, and those are in the window.
        let noted = matches!(crate::kinds::kind_of(db, tid).unwrap_or(crate::kinds::ASK), crate::kinds::PACKET | crate::kinds::PERMISSION);
        let mid = if noted { crate::kinds::note(db, tid, &body)? } else { db.john_replies(tid, &body)? };
        db.exec("UPDATE messages SET meta=:m WHERE id=:i", &[(":m", &crate::db::dumps(Some(&json!({"via": "slack"})))), (":i", &mid)])?;
        if noted {
            return say(&format!("Recorded as a note on #{tid}, and the agent is told. It stays open: finish or decide it in the AgentDesk window."), Some(thread_ts));
        }
        say(&format!("Recorded as your answer to #{tid}. Thanks!"), Some(thread_ts))
    }
}

/// One Slack message event, for one bot: a command (top-level, from John) or, for the bot with the board, a reply.
pub fn on_message(db: &Db, ev: &Value, chat: &dyn Chat, cmds: &Commands, dm: &str, bridge: Option<&Bridge>) {
    if !ev["bot_id"].is_null() || !matches!(ev["subtype"].as_str(), None | Some("file_share")) || ev["channel_type"].as_str() != Some("im") {
        return;
    }
    let channel = ev["channel"].as_str().unwrap_or(dm);
    let reply = if ev["thread_ts"].is_null() { cmds.handle(db, ev["text"].as_str().unwrap_or(""), ev["user"].as_str()) } else { None };
    let result = match (reply, bridge) {
        (Some(out), _) => chat.post(channel, None, Some(slackfmt::md_to_slack(&out).into_iter().take(50).collect()), &slackfmt::fallback_text(&out, 200), false).map(|_| ()),
        (None, Some(b)) => b.handle_reply(db, ev),
        (None, None) => Ok(()),
    };
    if let Err(e) = result {
        paths::log("WARN", &format!("slack: handling a message failed: {}", crate::db::anyhow_msg(&e)));
    }
}

// ---- the bots

pub struct Bot {
    pub name: String,
    pub areas: Vec<String>,
    pub creds: PathBuf,
    pub dm: String,
}

pub fn creds_dir() -> PathBuf {
    std::env::var_os("AGENTDESK_SLACK_DIR").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| crate::paths::home_dir().join(".claude").join("slack-notify"))
}

/// bots.json: `[{"name", "areas", "creds": <folder with bot-token.txt and app-token.txt>, "dm"?}]`. No file: one bot with every area.
pub fn load_bots(config: &Path, default_creds: &Path, default_dm: &str) -> Result<Vec<Bot>> {
    let Ok(text) = std::fs::read_to_string(config) else {
        return Ok(vec![Bot { name: "AgentDesk".into(), areas: AREAS.iter().map(|a| a.to_string()).collect(), creds: default_creds.to_path_buf(), dm: default_dm.into() }]);
    };
    let list: Vec<Value> = serde_json::from_str(&text)?;
    let mut bots = vec![];
    for b in list {
        let name = b["name"].as_str().unwrap_or("bot").to_string();
        let areas: Vec<String> = b["areas"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        if let Some(bad) = areas.iter().find(|a| !AREAS.contains(&a.as_str())) {
            return Err(anyhow!("bot {name}: unknown area {bad}"));
        }
        bots.push(Bot { name, areas, creds: default_creds.join(b["creds"].as_str().unwrap_or(".")), dm: b["dm"].as_str().unwrap_or(default_dm).into() });
    }
    if bots.iter().filter(|b| b.areas.iter().any(|a| a == "board")).count() > 1 {
        return Err(anyhow!("only one bot can have the board area (it owns bridge_state.json)"));
    }
    Ok(bots)
}

fn token(creds: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(creds.join(file)).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn env_token(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The DM and John's user id: the environment, then settings.json, then the defaults the Python bridge has.
fn dm_and_john() -> (String, String) {
    let pick = |env: &str, key: &str, default: &str| env_token(env).or_else(|| paths::setting(key).as_str().map(String::from)).unwrap_or_else(|| default.to_string());
    (pick("AGENTDESK_SLACK_DM", "slack_dm", DEFAULT_DM), pick("AGENTDESK_SLACK_USER", "slack_user", DEFAULT_JOHN))
}

/// A bot with the tokens it runs on. No app token: no Socket Mode, but its outbound work (questions, swarm channels) still runs.
struct Creds {
    bot: Bot,
    bot_token: String,
    app_token: Option<String>,
}

fn configured_bots() -> Result<Vec<Creds>> {
    let (dm, _) = dm_and_john();
    if let Some(t) = env_token("SLACK_BOT_TOKEN") {
        let bot = Bot { name: "AgentDesk".into(), areas: AREAS.iter().map(|a| a.to_string()).collect(), creds: creds_dir(), dm };
        return Ok(vec![Creds { bot, bot_token: t, app_token: env_token("SLACK_APP_TOKEN") }]);
    }
    let creds = creds_dir();
    Ok(load_bots(&creds.join("bots.json"), &creds, &dm)?
        .into_iter()
        .filter_map(|bot| {
            let bot_token = token(&bot.creds, "bot-token.txt")?;
            let app_token = token(&bot.creds, "app-token.txt");
            Some(Creds { bot, bot_token, app_token })
        })
        .collect())
}

/// Why this process should not run a bridge, or None when it is configured. A temp or test board never posts to John's Slack.
pub fn unconfigured() -> Option<&'static str> {
    if std::env::var_os("AGENTDESK_DATA").is_some() && std::env::var_os("AGENTDESK_SLACK_DIR").is_none() && env_token("SLACK_BOT_TOKEN").is_none() {
        return Some("not supervised on a custom AGENTDESK_DATA without AGENTDESK_SLACK_DIR or SLACK_BOT_TOKEN");
    }
    match configured_bots() {
        Ok(b) if !b.is_empty() => None,
        Ok(_) => Some("not configured: no Slack credentials (set AGENTDESK_SLACK_DIR, or put bot-token.txt and app-token.txt in ~/.claude/slack-notify)"),
        Err(_) => Some("not configured: bots.json is not valid (see core.log)"),
    }
}

// ---- Socket Mode

/// What one Socket Mode frame asks of us: the acknowledgement to send back, the message event it carries, and whether Slack is about to
/// close the connection (it asks for a reconnect every few hours).
#[derive(Debug, Default, PartialEq)]
pub struct Frame {
    pub ack: Option<String>,
    pub event: Option<Value>,
    pub reconnect: bool,
}

pub fn parse_frame(text: &str) -> Option<Frame> {
    let v: Value = serde_json::from_str(text).ok()?;
    let mut f = Frame { ack: v["envelope_id"].as_str().map(|id| json!({"envelope_id": id}).to_string()), ..Default::default() };
    match v["type"].as_str() {
        Some("events_api") if v["payload"]["event"]["type"].as_str() == Some("message") => f.event = Some(v["payload"]["event"].clone()),
        Some("disconnect") => f.reconnect = true,
        _ => {}
    }
    Some(f)
}

trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}
type Socket = tungstenite::WebSocket<Box<dyn Conn>>;

/// Opens a Socket Mode connection (`wss://...`; `ws://` is for a local fake).
fn connect(app_token: &str) -> Result<Socket> {
    let opened = Web::new("").call("apps.connections.open", app_token, &json!({}))?;
    let url = opened["url"].as_str().ok_or_else(|| anyhow!("no socket url"))?.to_string();
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow!("socket url {url}"))?;
    let authority = rest.split('/').next().unwrap_or("");
    let tls = scheme == "wss";
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(if tls { 443 } else { 80 })),
        None => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    let tcp = std::net::TcpStream::connect((host.as_str(), port))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let timeouts = tcp.try_clone()?; // the same socket: the read timeout is shortened once the handshake is done
    let stream: Box<dyn Conn> = if tls {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let conn = rustls::ClientConnection::new(Arc::new(config), host.clone().try_into()?)?;
        Box::new(rustls::StreamOwned::new(conn, tcp))
    } else {
        Box::new(tcp)
    };
    let (ws, _) = tungstenite::client(url.as_str(), stream).map_err(|e| anyhow!("socket handshake: {e}"))?;
    timeouts.set_read_timeout(Some(Duration::from_secs(2)))?; // so the loop can look at `stop`
    Ok(ws)
}

/// Reads one connection until it ends. Each frame is acknowledged at once (Slack wants that within 3 s), and its message event goes to
/// `events`: a slow command must not keep the acknowledgement or the pings waiting.
fn read_socket(ws: &mut Socket, stop: &AtomicBool, events: &Sender<Value>) {
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                last = Instant::now();
                let Some(f) = parse_frame(&t) else { continue };
                if let Some(ack) = f.ack {
                    if ws.send(tungstenite::Message::text(ack)).is_err() {
                        break;
                    }
                }
                if let Some(ev) = f.event {
                    let _ = events.send(ev);
                }
                if f.reconnect {
                    break;
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => last = Instant::now(), // pings are answered by the library
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                if last.elapsed() > Duration::from_secs(300) {
                    paths::log("WARN", "slack: socket silent for 5 minutes, reconnecting");
                    break;
                }
            }
            Err(e) => {
                paths::log("WARN", &format!("slack: socket ended: {e}"));
                break;
            }
        }
    }
    let _ = ws.close(None);
}

/// Listens until `stop`; reconnects, backing off from 1 s to 1 min when it cannot connect (Slack asks for a reconnect every few hours, and
/// networks drop).
pub fn socket_loop(app_token: &str, stop: &AtomicBool, events: &Sender<Value>) {
    let mut backoff = 1;
    while !stop.load(Ordering::Relaxed) {
        let mut ws = match connect(app_token) {
            Ok(ws) => ws,
            Err(e) => {
                paths::log("WARN", &format!("slack: cannot connect: {e}"));
                for _ in 0..backoff * 2 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        backoff = 1;
        paths::log("INFO", "slack: socket connected");
        read_socket(&mut ws, stop, events);
    }
}

// ---- typing into a session

/// Types a line into a session and presses Enter: (session name, text).
pub type Input = dyn Fn(&str, &str) -> Result<()> + Send + Sync;

/// Text and Enter in one write read as a paste, so they are two.
const ENTER_AFTER: Duration = Duration::from_millis(300);

/// From inside the core: straight to the session's terminal through the host's registry.
pub fn registry_input(registry: Registry) -> Arc<Input> {
    Arc::new(move |name, text| {
        let shared = registry.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, s)| s.clone());
        let shared = shared.ok_or_else(|| anyhow!("{name} is not running"))?;
        shared.write(text.as_bytes())?;
        std::thread::sleep(ENTER_AFTER);
        shared.write(b"\r")
    })
}

/// From another process: through the supervisor's attach socket.
pub fn attach_input() -> Arc<Input> {
    Arc::new(|name, text| {
        use std::io::{BufRead, BufReader};
        let (port, key) = crate::attach::read_addr().ok_or_else(|| anyhow!(crate::attach::NOT_RUNNING))?;
        let mut conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
        conn.set_read_timeout(Some(Duration::from_secs(5)))?;
        writeln!(conn, "ATTACH {key} {name} 160 40")?;
        let mut line = String::new();
        BufReader::new(conn.try_clone()?).read_line(&mut line)?;
        if line.trim() != "OK" {
            return Err(anyhow!("{}", line.trim().strip_prefix("ERR ").unwrap_or(line.trim())));
        }
        writeln!(conn, "INPUT {}", crate::attach::base64_encode(text.as_bytes()))?;
        std::thread::sleep(ENTER_AFTER);
        writeln!(conn, "INPUT {}", crate::attach::base64_encode(b"\r"))?;
        writeln!(conn, "DETACH")?;
        // Do not just drop the socket: it still holds the session's replayed output unread, and closing a socket with unread data sends a reset,
        // which on Linux discards the lines the server has received but not yet read (the Enter). Say we are done writing and wait for the
        // server to close its end.
        let _ = conn.shutdown(std::net::Shutdown::Write);
        let mut sink = [0u8; 4096];
        while matches!(std::io::Read::read(&mut conn, &mut sink), Ok(n) if n > 0) {}
        Ok(())
    })
}

// ---- running it

fn heartbeat(last_relay: Option<Value>) {
    let doc = json!({"pid": std::process::id(), "ts": crate::db::now_iso(), "poll_s": poll().as_secs(), "last_relay": last_relay});
    let _ = paths::write_atomic(&paths::data_dir().join("slack_bridge.state"), doc.to_string());
}

/// One bot, running.
struct Running {
    web: Arc<Web>,
    cmds: Arc<Commands>,
    dm: String,
    app_token: Option<String>,
    bridge: Option<Arc<Bridge>>,
    swarm: Option<Arc<Swarms>>,
}

fn build(input: &Arc<Input>) -> Result<Vec<Running>> {
    let (_, john) = dm_and_john();
    let mut out = vec![];
    for c in configured_bots()? {
        let web = Arc::new(Web::new(&c.bot_token));
        let mut cmds = Commands::new(&c.bot.areas, &john, &c.bot.name);
        let swarm = c.bot.areas.iter().any(|a| a == "swarms").then(|| Arc::new(Swarms::new(web.clone(), &john, paths::data_dir().join("swarm_state.json"), input.clone())));
        if let Some(s) = &swarm {
            cmds = cmds.with_swarm(s.clone());
        }
        let bridge = c
            .bot
            .areas
            .iter()
            .any(|a| a == "board")
            .then(|| Arc::new(Bridge::new(web.clone(), &c.bot.dm, &john, c.bot.creds.join("bridge_state.json"), paths::data_dir().join("attachments"))));
        out.push(Running { web, cmds: Arc::new(cmds), dm: c.bot.dm, app_token: c.app_token, bridge, swarm });
    }
    Ok(out)
}

/// The 15 s pass: the board's questions out, then each swarm's relay and poll.
fn pass(bots: &[Running], db: &Db) {
    for r in bots {
        if let Some(b) = &r.bridge {
            if let Err(e) = b.poll(db) {
                paths::log("WARN", &format!("slack: poll error: {}", crate::db::anyhow_msg(&e)));
            }
        }
        if let Some(s) = &r.swarm {
            crate::swarm::pass(s, db);
        }
    }
}

/// `agentdesk slack-bridge --once`: one outbound pass, no Socket Mode, then exit.
pub fn run_once(input: Arc<Input>) -> Result<()> {
    let bots = build(&input)?;
    if bots.is_empty() {
        return Err(anyhow!("no Slack credentials: set SLACK_BOT_TOKEN, or put bot-token.txt in {}", creds_dir().display()));
    }
    let db = Db::open(&paths::db_path())?;
    db.init()?;
    pass(&bots, &db);
    heartbeat(bots.iter().find_map(|r| r.bridge.as_ref().and_then(|b| b.last_relay())));
    Ok(())
}

/// Runs the bridge until `stop` is set: a Socket Mode thread and an event worker per bot, a poller for the outbound work and the swarms,
/// and the heartbeat (`slack_bridge.state`) from this thread. Returns an error when it cannot start.
pub fn run(stop: Arc<AtomicBool>, input: Arc<Input>) -> Result<()> {
    let bots = Arc::new(build(&input)?);
    if bots.is_empty() {
        return Err(anyhow!("no Slack credentials: set SLACK_BOT_TOKEN, or put bot-token.txt in {}", creds_dir().display()));
    }
    Db::open(&paths::db_path())?.init()?; // a bridge run by hand may be the first to touch the board
    let mut threads = vec![];
    for (i, r) in bots.iter().enumerate() {
        let Some(app) = r.app_token.clone() else { continue };
        let (tx, rx): (Sender<Value>, Receiver<Value>) = channel();
        let s = stop.clone();
        threads.push(std::thread::spawn(move || socket_loop(&app, &s, &tx)));
        let (bots, s) = (bots.clone(), stop.clone());
        threads.push(std::thread::spawn(move || {
            let Ok(db) = Db::open(&paths::db_path()) else { return };
            let r = &bots[i];
            while !s.load(Ordering::Relaxed) {
                if let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) {
                    on_message(&db, &ev, r.web.as_ref(), &r.cmds, &r.dm, r.bridge.as_deref());
                }
            }
        }));
    }
    let (poll_bots, s) = (bots.clone(), stop.clone());
    threads.push(std::thread::spawn(move || {
        let Ok(db) = Db::open(&paths::db_path()) else { return };
        let mut last = None::<Instant>;
        while !s.load(Ordering::Relaxed) {
            if last.is_none_or(|t| t.elapsed() >= poll()) {
                last = Some(Instant::now());
                pass(&poll_bots, &db);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }));
    while !stop.load(Ordering::Relaxed) {
        heartbeat(bots.iter().find_map(|r| r.bridge.as_ref().and_then(|b| b.last_relay())));
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

/// `agentdesk slack-bridge [--once]`: this process is the bridge, for a machine whose core does not run it (or a test). A bridge that is
/// already beating, the core's or another's, is not joined: two would double-post.
pub fn run_cli(once: bool) -> Result<()> {
    if crate::bridge::running() {
        return Err(anyhow!("a Slack bridge is already running (see slack_bridge.state in {})", paths::data_dir().display()));
    }
    if once {
        return run_once(attach_input());
    }
    println!("slack bridge running on {}", paths::db_path().display());
    run(Arc::new(AtomicBool::new(false)), attach_input())
}
