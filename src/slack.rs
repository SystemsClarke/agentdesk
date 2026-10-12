//! Bridges AgentDesk's open questions <-> a Slack DM with John. A port of `scripts/slack_bridge.py`, run by the daemon (or the window)
//! as threads instead of a supervised Python process.
//!
//! Outbound: polls the board's open questions (the view that drives the toast and the title-bar count) and posts each as a new Slack
//! thread in John's DM. `bridge_state.json` (next to the credentials) maps thread id -> Slack thread_ts and the `updated_ts` last seen,
//! so a restart does not re-post a question already in Slack, but one that goes open -> answered -> asked again does get re-posted.
//!
//! Inbound: Socket Mode delivers message events. A reply *inside* one of those Slack threads answers the AgentDesk thread exactly as
//! John answering from the window does (`Db::john_replies`). A top-level DM such as `status`, `agents` or `concierge off` runs a phone
//! command (`slack_cmd.rs`).
//!
//! Credentials (not in the repo): `bot-token.txt` (xoxb-...) and `app-token.txt` (xapp-...) in `~/.claude/slack-notify` (or
//! `AGENTDESK_SLACK_DIR`), optionally `bots.json` for several bots. `slack_bridge: false` in settings.json turns it off. Swarm slots
//! are not in this build.

use crate::db::Db;
use crate::slack_cmd::{Commands, AREAS};
use crate::{paths, slackfmt};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_DM: &str = "D0C3KDM3DNX";
const DEFAULT_JOHN: &str = "U0C4L06N4KA"; // only his messages in his DM are answers
const POLL: Duration = Duration::from_secs(15);

// ---- the Slack Web API

/// What the bridge needs from Slack: post a message (returns its ts) and fetch a private file.
pub trait Chat: Send + Sync {
    fn post(&self, channel: &str, thread_ts: Option<&str>, blocks: Option<Vec<Value>>, text: &str, broadcast: bool) -> Result<String>;
    fn download(&self, url: &str) -> Result<Vec<u8>>;
}

pub struct Web {
    token: String,
}

impl Web {
    pub fn new(token: &str) -> Web {
        Web { token: token.trim().to_string() }
    }

    pub fn call(&self, method: &str, token: &str, body: &Value) -> Result<Value> {
        for attempt in 0..2 {
            let req = ureq::post(&format!("https://slack.com/api/{method}")).set("Authorization", &format!("Bearer {token}")).timeout(Duration::from_secs(30));
            // chat.* takes JSON (blocks); the rest of the Web API is happiest with a form
            let r = if method.starts_with("chat.") {
                req.send_json(body.clone())
            } else {
                let pairs: Vec<(String, String)> = body
                    .as_object()
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string().trim_matches('"').to_string()))).collect())
                    .unwrap_or_default();
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

/// Any Web API method with this bot's token (the swarm slots use channels and history).
pub trait Api: Send + Sync {
    fn api(&self, method: &str, body: &Value) -> Result<Value>;
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
        std::io::Read::read_to_end(&mut resp.into_reader().take(25 * 1024 * 1024), &mut out)?;
        Ok(out)
    }
}

use std::io::Read as _;

// ---- the state file

fn load(path: &Path) -> Value {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({"posted": {}}))
}

fn save(path: &Path, state: &Value) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(state).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path); // a crash mid-write must not lose every mapping
    }
}

fn epoch(iso: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(iso).map(|t| t.timestamp() as f64).unwrap_or(0.0)
}

fn now_epoch() -> f64 {
    chrono::Utc::now().timestamp() as f64
}

// ---- the question relay

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

    /// Posts a question; a follow-up on one already in Slack goes into John's existing thread. Returns the thread's ts.
    fn post_question(&self, db: &Db, q: &Value, prev: Option<&Value>) -> Result<String> {
        let tid = q["thread_id"].as_i64().unwrap_or(0);
        let body = db.scalar_str("SELECT body FROM messages WHERE thread_id=:t ORDER BY id DESC LIMIT 1", &[(":t", &tid)])?.unwrap_or_default();
        let asker = q["opened_by"].as_str().filter(|a| !a.is_empty()).unwrap_or("an agent");
        let subject = q["subject"].as_str().unwrap_or("");
        let head = match prev {
            None => format!("*☎ Question #{tid}* from *{}*\n*{}*", slackfmt::esc(asker), slackfmt::inline(subject)),
            Some(_) => format!("*↩ Follow-up on #{tid}* from *{}*", slackfmt::esc(asker)),
        };
        let mut blocks = vec![json!({"type": "section", "text": {"type": "mrkdwn", "text": head}}), json!({"type": "divider"})];
        blocks.extend(slackfmt::md_to_slack(&body));
        blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": "Reply *in this thread* to answer. Photos work too."}]}));
        blocks.truncate(50);
        let text = format!("Question #{tid} from {asker}: {subject} · {}", slackfmt::fallback_text(&body, 200));
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
            let said = wake(db, tid);
            let mut c = said.chars();
            let said = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
            return say(&format!("{said}."), Some(thread_ts));
        }
        let mut body = slackfmt::slack_to_md(text);
        let files: Vec<Value> = ev["files"].as_array().cloned().unwrap_or_default();
        let photos = self.save_photos(&files, thread_ts);
        if !photos.is_empty() {
            let links = photos.iter().map(|p| format!("![photo](file:///{})", p.to_string_lossy().replace('\\', "/"))).collect::<Vec<_>>().join("\n");
            body = if body.trim().is_empty() { links } else { format!("{body}\n\n{links}") };
        }
        if body.trim().is_empty() {
            return Ok(());
        }
        db.john_replies(tid, &body)?;
        db.exec(
            "UPDATE messages SET meta=json_set(CASE WHEN json_valid(meta) THEN meta ELSE '{}' END, '$.via', 'slack') WHERE id=(SELECT MAX(id) FROM messages WHERE thread_id=:t)",
            &[(":t", &tid)],
        )?;
        say(&format!("Recorded as your answer to #{tid}. Thanks!"), Some(thread_ts))
    }
}

/// `wake` in a question's thread: resume the asking agent now, with John's latest reply (see `wake.rs`).
pub fn wake(db: &Db, tid: i64) -> String {
    crate::wake::wake(db, tid).unwrap_or_else(|e| format!("could not wake it: {}", crate::db::anyhow_msg(&e)))
}

/// One Slack message event, for one bot: a command (top-level, from John) or, for the bot with the board, a reply.
pub fn on_message(db: &Db, ev: &Value, chat: &dyn Chat, cmds: &Commands, dm: &str, bridge: Option<&Bridge>) {
    if !ev["bot_id"].is_null() || !matches!(ev["subtype"].as_str(), None | Some("file_share")) || ev["channel_type"].as_str() != Some("im") {
        return;
    }
    let channel = ev["channel"].as_str().unwrap_or(dm);
    let result = if ev["thread_ts"].is_null()
        && cmds
            .handle(db, ev["text"].as_str().unwrap_or(""), ev["user"].as_str())
            .map(|out| {
                let blocks = slackfmt::md_to_slack(&out);
                chat.post(channel, None, Some(blocks), &slackfmt::fallback_text(&out, 200), false)
            })
            .is_some()
    {
        Ok(())
    } else if let Some(b) = bridge {
        b.handle_reply(db, ev)
    } else {
        Ok(())
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

fn setting(key: &str) -> Value {
    std::fs::read_to_string(paths::data_dir().join("settings.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).map(|v| v[key].clone()).unwrap_or(Value::Null)
}

// ---- Socket Mode

fn connect(app_token: &str) -> Result<tungstenite::WebSocket<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>> {
    let opened = Web::new("").call("apps.connections.open", app_token, &json!({}))?;
    let url = opened["url"].as_str().ok_or_else(|| anyhow!("no socket url"))?.to_string();
    let host = url.trim_start_matches("wss://").split('/').next().unwrap_or("").to_string();
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = std::net::TcpStream::connect((host.as_str(), 443))?;
    tcp.set_read_timeout(Some(Duration::from_secs(2)))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), host.clone().try_into()?)?;
    let (ws, _) = tungstenite::client(url.as_str(), rustls::StreamOwned::new(conn, tcp)).map_err(|e| anyhow!("socket handshake: {e}"))?;
    Ok(ws)
}

/// Listens until `stop`; reconnects (Slack asks for it every few hours, and networks drop). Each message event goes to `handle`.
fn socket_loop(app_token: &str, stop: &AtomicBool, handle: &dyn Fn(&Value)) {
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
        while !stop.load(Ordering::Relaxed) {
            match ws.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                    if let Some(id) = v["envelope_id"].as_str() {
                        let _ = ws.send(tungstenite::Message::text(json!({"envelope_id": id}).to_string()));
                    }
                    match v["type"].as_str() {
                        Some("events_api") if v["payload"]["event"]["type"].as_str() == Some("message") => handle(&v["payload"]["event"]),
                        Some("disconnect") => break,
                        _ => {}
                    }
                }
                Ok(tungstenite::Message::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => {
                    paths::log("WARN", &format!("slack: socket ended: {e}"));
                    break;
                }
            }
        }
        let _ = ws.close(None);
    }
}

fn heartbeat(last_relay: Option<Value>) {
    let doc = json!({"pid": std::process::id(), "ts": crate::db::now_iso(), "poll_s": POLL.as_secs(), "last_relay": last_relay});
    let path = paths::data_dir().join("slack_bridge.state");
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, doc.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn lock_file() -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::data_dir().join("slack_bridge.lock")).ok()?;
    f.try_lock().is_ok().then_some(f)
}

/// Starts the bridge on its own threads, if it is configured and nobody else (the window or the daemon) already runs it.
pub fn spawn(stop: Arc<AtomicBool>) {
    if setting("slack_bridge") == json!(false) {
        return;
    }
    if std::env::var_os("AGENTDESK_DATA").is_some() && std::env::var_os("AGENTDESK_SLACK_DIR").is_none() {
        return; // temp and test boards never post to John's Slack
    }
    let creds = creds_dir();
    let bots = match load_bots(&creds.join("bots.json"), &creds, setting("slack_dm").as_str().unwrap_or(DEFAULT_DM)) {
        Ok(b) => b.into_iter().filter(|b| token(&b.creds, "bot-token.txt").is_some() && token(&b.creds, "app-token.txt").is_some()).collect::<Vec<_>>(),
        Err(e) => {
            paths::log("WARN", &format!("slack: bots.json: {e}"));
            return;
        }
    };
    if bots.is_empty() {
        return;
    }
    let Some(lock) = lock_file() else { return };
    let john = setting("slack_user").as_str().unwrap_or(DEFAULT_JOHN).to_string();
    let mut bridge = None;
    let mut swarms: Vec<Arc<crate::swarm::Swarms>> = vec![];
    for bot in bots {
        let web: Arc<dyn Chat> = Arc::new(Web::new(&token(&bot.creds, "bot-token.txt").unwrap_or_default()));
        let app = token(&bot.creds, "app-token.txt").unwrap_or_default();
        let mut cmds = Commands::new(&bot.areas, &john, &bot.name);
        if bot.areas.iter().any(|a| a == "swarms") {
            let api: Arc<dyn Api> = Arc::new(Web::new(&token(&bot.creds, "bot-token.txt").unwrap_or_default()));
            let sw = Arc::new(crate::swarm::Swarms::new(api, &john, paths::data_dir().join("swarm_state.json")));
            swarms.push(sw.clone());
            cmds = cmds.with_swarm(sw);
        }
        let b = bot
            .areas
            .iter()
            .any(|a| a == "board")
            .then(|| Arc::new(Bridge::new(web.clone(), &bot.dm, &john, bot.creds.join("bridge_state.json"), paths::data_dir().join("attachments"))));
        if b.is_some() {
            bridge = b.clone();
        }
        let (stop, dm) = (stop.clone(), bot.dm.clone());
        std::thread::spawn(move || {
            let Ok(db) = Db::open(&paths::db_path()) else { return };
            socket_loop(&app, &stop.clone(), &|ev| on_message(&db, ev, web.as_ref(), &cmds, &dm, b.as_deref()));
        });
    }
    std::thread::spawn(move || {
        let _lock = lock; // held for the life of the bridge
        let db = Db::open(&paths::db_path()).ok();
        let mut last = None::<Instant>;
        while !stop.load(Ordering::Relaxed) {
            if last.is_none_or(|t| t.elapsed() >= POLL) {
                last = Some(Instant::now());
                if let Some(db) = &db {
                    if let Some(b) = &bridge {
                        if let Err(e) = b.poll(db) {
                            paths::log("WARN", &format!("slack: poll error: {}", crate::db::anyhow_msg(&e)));
                        }
                    }
                    for sw in &swarms {
                        crate::swarm::pass(sw, db);
                    }
                }
                heartbeat(bridge.as_ref().and_then(|b| b.last_relay.lock().ok().and_then(|l| l.clone())));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}
