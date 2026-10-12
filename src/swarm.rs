//! Swarm slots on Slack (docs/GOAL.md, milestone 5): 10 reusable channels, each speaking as one goal's persona. A port of
//! `agentdesk/swarm.py`.
//!
//! The board stores slot -> channel, persona and goal (`slots.rs`); this module makes every Slack call and keeps that mapping current.
//! John's commands (`swarms`, `swarm new|approve|end|reset`, in `slack_cmd.rs`) land here, John's messages in a slot's channel (polled
//! with conversations.history, which needs channels:history) are typed into the goal's lead, and each new post on the goal's board
//! thread is relayed into the channel: verdicts and proposals top level, member notes in a thread under one "activity" message a day.
//!
//! Persona posts use chat.postMessage's username/icon overrides, which need the chat:write.customize scope. Without it Slack says
//! missing_scope; then every post is a plain one prefixed with the persona's name, and that is logged once.

use crate::db::Db;
use crate::identity::Caller;
use crate::slack::{slack_error, Api};
use crate::{goals, identities as ids, paths, slackfmt, slots};
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const ICONS: [&str; 10] = [
    ":large_blue_circle:",
    ":large_green_circle:",
    ":large_orange_circle:",
    ":large_purple_circle:",
    ":red_circle:",
    ":large_yellow_circle:",
    ":large_brown_circle:",
    ":white_circle:",
    ":black_circle:",
    ":large_blue_diamond:",
];
const PROPOSING: &str = "Proposing a hypothesis…";
/// A post that can never land: skip it, don't retry forever.
const GONE: [&str; 3] = ["channel_not_found", "is_archived", "not_in_channel"];

/// Types a line into a lead's session: (identity, text). The real one goes through the supervisor's attach endpoint.
pub type Input = dyn Fn(&str, &str) -> Result<()> + Send + Sync;

pub struct Swarms {
    api: Arc<dyn Api>,
    john: String,
    state_path: PathBuf,
    state: Mutex<Value>,
    /// False once Slack says the persona override needs chat:write.customize.
    customize: AtomicBool,
    input: Box<Input>,
    today: fn() -> String,
}

fn warn(r: &anyhow::Error) -> String {
    format!("⚠ {}", crate::db::anyhow_msg(r))
}

fn sv<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn ts_num(ts: &str) -> f64 {
    ts.parse().unwrap_or(0.0)
}

impl Swarms {
    pub fn new(api: Arc<dyn Api>, john: &str, state_path: PathBuf) -> Swarms {
        Swarms::with(api, john, state_path, Box::new(crate::attach::type_into), today)
    }

    pub fn with(api: Arc<dyn Api>, john: &str, state_path: PathBuf, input: Box<Input>, today: fn() -> String) -> Swarms {
        let mut state = std::fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
        for k in ["goals", "channels"] {
            if !state[k].is_object() {
                state[k] = json!({}); // channel id -> ts of the last message poll() looked at
            }
        }
        Swarms { api, john: john.into(), state_path, state: Mutex::new(state), customize: AtomicBool::new(true), input, today }
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut Value) -> T) -> T {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s)
    }

    fn save(&self) {
        let text = self.with_state(|s| serde_json::to_string_pretty(s).unwrap_or_default());
        let tmp = self.state_path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.state_path);
        }
    }

    fn log(&self, msg: &str) {
        paths::log("INFO", &format!("swarm: {msg}"));
    }

    // ---- plumbing

    /// Posts Markdown into the slot's channel as its persona; returns the message ts.
    fn post(&self, slot: &Value, md: &str, thread_ts: Option<&str>) -> Result<String> {
        let n = slot["n"].as_i64().unwrap_or(1);
        let persona = slot["persona_name"].as_str().filter(|p| !p.is_empty()).unwrap_or("swarm");
        let icon = slot["persona_icon"].as_str().filter(|p| !p.is_empty()).map(String::from).unwrap_or_else(|| ICONS[((n - 1).rem_euclid(10)) as usize].to_string());
        let customize = self.customize.load(Ordering::Relaxed);
        let md = if customize { md.to_string() } else { format!("**{persona}:** {md}") };
        let mut blocks = slackfmt::md_to_slack(&md);
        blocks.truncate(50);
        let mut body =
            json!({"channel": slot["channel_id"], "text": slackfmt::fallback_text(&md.replace("**", ""), 200), "blocks": blocks, "unfurl_links": false, "unfurl_media": false});
        if let Some(t) = thread_ts {
            body["thread_ts"] = json!(t);
        }
        if customize {
            body["username"] = json!(persona);
            body[if icon.starts_with("http") { "icon_url" } else { "icon_emoji" }] = json!(icon);
        }
        match self.api.api("chat.postMessage", &body) {
            Ok(v) => Ok(v["ts"].as_str().unwrap_or("").to_string()),
            Err(e) if customize && slack_error(&e) == Some("missing_scope") => {
                self.customize.store(false, Ordering::Relaxed);
                self.log("chat:write.customize is missing: posting as the bot, each post prefixed with the persona's name. Add the scope to the Slack app and reinstall it.");
                self.post(slot, &md, thread_ts)
            }
            Err(e) => Err(e),
        }
    }

    /// The slot's channel renamed to swarm-<name> (or a new one when it has none, or `fresh`), with John invited.
    fn channel(&self, slot: &Value, name: &str, fresh: bool) -> Result<(String, String)> {
        let want = format!("swarm-{}", name.to_lowercase());
        let mut ch = None;
        if let (Some(id), false) = (slot["channel_id"].as_str().filter(|c| !c.is_empty()), fresh) {
            match self.api.api("conversations.rename", &json!({"channel": id, "name": want})) {
                Ok(v) => ch = Some(v["channel"].clone()),
                Err(e) if matches!(slack_error(&e), Some("channel_not_found" | "is_archived")) => {}
                Err(e) => return Err(e),
            }
        }
        let ch = match ch {
            Some(c) => c,
            None => self.api.api("conversations.create", &json!({"name": want, "is_private": false}))?["channel"].clone(),
        };
        let id = sv(&ch, "id").to_string();
        if let Err(e) = self.api.api("conversations.invite", &json!({"channel": id, "users": self.john})) {
            if !matches!(slack_error(&e), Some("already_in_channel" | "cant_invite_self")) {
                return Err(e);
            }
        }
        Ok((id, sv(&ch, "name").to_string()))
    }

    /// goal_create, then the slot's channel and persona, then the persona's first post. Returns John's reply.
    #[allow(clippy::too_many_arguments)]
    fn start(&self, db: &Db, n: i64, slot: &Value, name: &str, objective: &str, folder: &str, fresh: bool) -> String {
        if let Err(e) = goals::create(db, name, objective, folder) {
            return warn(&e);
        }
        let icon = ICONS[(n - 1) as usize];
        let (cid, cname) = match self.channel(slot, name, fresh) {
            Ok(c) => c,
            Err(e) => {
                let _ = slots::assign(db, n, Some(name), Some(name), Some(icon), if fresh { Some("") } else { None }, None);
                return format!(
                    "⚠ Goal *{name}* created in slot {n}, but Slack refused the channel: {}.",
                    slack_error(&e).map(String::from).unwrap_or_else(|| crate::db::anyhow_msg(&e))
                );
            }
        };
        let row = match slots::assign(db, n, Some(name), Some(name), Some(icon), Some(&cid), Some(&cname)) {
            Ok(r) => r,
            Err(e) => return warn(&e),
        };
        match self.post(&row, PROPOSING, None) {
            Ok(ts) => {
                self.with_state(|s| s["channels"][&cid] = json!(ts)); // a new goal's channel: nothing before this is input
                self.save();
            }
            Err(e) => return warn(&e),
        }
        format!("Slot {n}: *{name}* in <#{cid}>. Its lead is proposing a hypothesis; approve with `swarm approve {n}`.")
    }

    fn goal_slot(&self, db: &Db, n: i64) -> std::result::Result<Value, String> {
        if !(1..=slots::COUNT).contains(&n) {
            return Err(format!("A slot is 1 to {}.", slots::COUNT));
        }
        let s = slots::list(db).map_err(|e| warn(&e))?.into_iter().find(|s| s["n"].as_i64() == Some(n)).unwrap_or(Value::Null);
        if s["goal"].as_str().is_some_and(|g| !g.is_empty()) {
            Ok(s)
        } else {
            Err(format!("Slot {n} is free."))
        }
    }

    // ---- John's commands

    pub fn list(&self, db: &Db) -> String {
        let all = match slots::list(db) {
            Ok(a) => a,
            Err(e) => return warn(&e),
        };
        all.iter()
            .map(|s| {
                let n = s["n"].as_i64().unwrap_or(0);
                let ch = s["channel_id"].as_str().filter(|c| !c.is_empty()).map(|c| format!(" · <#{c}>")).unwrap_or_default();
                if s["goal"].as_str().is_none_or(str::is_empty) {
                    return format!("*{n}* · free{ch}");
                }
                let last = s["last_value"].as_f64().map(|v| format!("{v}")).unwrap_or_else(|| "—".into());
                let persona = s["persona_name"].as_str().filter(|p| !p.is_empty()).unwrap_or_else(|| sv(s, "goal"));
                format!("*{n}* · {persona} · {} · last {last} · {} member(s){ch}", s["state"].as_str().unwrap_or("?"), s["members"].as_i64().unwrap_or(0))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn new_swarm(&self, db: &Db, name: &str, folder: &str, objective: &str) -> String {
        if !Regex::new(r"^[A-Za-z][A-Za-z0-9-]{0,31}$").unwrap().is_match(name) {
            return "⚠ A swarm name is letters, digits and dashes (at most 32), starting with a letter.".into();
        }
        let all = match slots::list(db) {
            Ok(a) => a,
            Err(e) => return warn(&e),
        };
        // reuse a channel before making one
        let free =
            all.iter().filter(|s| s["goal"].as_str().is_none_or(str::is_empty)).min_by_key(|s| (s["channel_id"].as_str().is_none_or(str::is_empty), s["n"].as_i64().unwrap_or(0)));
        match free {
            None => format!("⚠ All {} slots are busy: `swarm end <slot>` one first.", slots::COUNT),
            Some(slot) => self.start(db, slot["n"].as_i64().unwrap_or(1), slot, name, objective, folder, false),
        }
    }

    pub fn approve(&self, db: &Db, n: i64, members: Option<i64>, hours: Option<f64>, cadence: Option<f64>) -> String {
        let s = match self.goal_slot(db, n) {
            Ok(s) => s,
            Err(why) => return why,
        };
        match goals::approve(db, &Caller::default(), sv(&s, "goal"), members, hours, cadence) {
            Err(e) => warn(&e),
            Ok(g) => {
                self.relay_slot(db, &s);
                format!(
                    "Approved *{}*: {} members, {} h, the lead wakes every {} min.",
                    sv(&s, "goal"),
                    goals::num(&g["max_members"]),
                    goals::num(&g["max_hours"]),
                    goals::num(&g["cadence_minutes"])
                )
            }
        }
    }

    pub fn end(&self, db: &Db, n: i64) -> String {
        let s = match self.goal_slot(db, n) {
            Ok(s) => s,
            Err(why) => return why,
        };
        let goal = sv(&s, "goal").to_string();
        match goals::stop(db, &Caller::default(), &goal) {
            Err(e) => warn(&e),
            Ok(r) => {
                self.relay_slot(db, &s); // the core's "Goal stopped" note, before the slot forgets the goal
                let _ = slots::clear(db, n);
                self.with_state(|st| st["goals"].as_object_mut().map(|g| g.remove(&goal)));
                self.save();
                format!("Ended *{goal}* ({}); slot {n} is free and keeps its channel.", sv(&r, "state"))
            }
        }
    }

    pub fn reset(&self, db: &Db, n: i64) -> String {
        let s = match self.goal_slot(db, n) {
            Ok(s) => s,
            Err(why) => return why,
        };
        let old = sv(&s, "goal").to_string();
        let st = match goals::status(db, &old).and_then(|st| goals::stop(db, &Caller::default(), &old).map(|_| st)) {
            Ok(st) => st,
            Err(e) => return warn(&e),
        };
        let lead = st["lead"].as_str().map(String::from).unwrap_or_else(|| format!("{old}-lead"));
        let _ = ids::forget(db, &lead); // the chatbot starts over as a new identity
        if let Some(c) = s["channel_id"].as_str().filter(|c| !c.is_empty()) {
            if let Err(e) = self.api.api("conversations.archive", &json!({"channel": c})) {
                if slack_error(&e) != Some("already_archived") {
                    self.log(&format!("slot {n}: archiving {c} failed: {}", slack_error(&e).map(String::from).unwrap_or_else(|| crate::db::anyhow_msg(&e))));
                }
            }
        }
        self.with_state(|state| state["goals"].as_object_mut().map(|g| g.remove(&old)));
        self.save();
        let base = Regex::new(r"-\d+$").unwrap().replace(&old, "").to_string();
        let mut k = 2;
        while goals::status(db, &format!("{base}-{k}")).is_ok() {
            k += 1; // goal names are never reused
        }
        self.start(db, n, &s, &format!("{base}-{k}"), sv(&st, "objective"), sv(&st, "measure_folder"), true)
    }

    // ---- inbound: John in a slot's channel

    /// One conversations.history pass over every slot's channel; John's new messages go to the goal's lead. The watermark per channel
    /// is kept in the state file, so a restart neither replays nor drops a message. A channel seen for the first time starts at its
    /// newest message: its history is not input. Returns how many were relayed.
    pub fn poll(&self, db: &Db) -> Result<usize> {
        let mut sent = 0;
        for s in slots::list(db)? {
            let Some(cid) = s["channel_id"].as_str().filter(|c| !c.is_empty()).map(String::from) else { continue };
            match self.poll_channel(db, &s, &cid) {
                Ok(n) => sent += n,
                Err(e) => self.log(&format!("slot {}: polling {cid} failed: {}", s["n"], slack_error(&e).map(String::from).unwrap_or_else(|| crate::db::anyhow_msg(&e)))), // one channel failing must not stop the others
            }
        }
        Ok(sent)
    }

    fn poll_channel(&self, db: &Db, s: &Value, cid: &str) -> Result<usize> {
        let mark = self.with_state(|st| st["channels"][cid].as_str().map(String::from));
        let Some(mark) = mark else {
            let h = self.api.api("conversations.history", &json!({"channel": cid, "limit": 1}))?;
            let first = h["messages"].as_array().and_then(|m| m.first()).and_then(|m| m["ts"].as_str()).unwrap_or("0").to_string();
            self.with_state(|st| st["channels"][cid] = json!(first));
            self.save();
            return Ok(0);
        };
        let h = self.api.api("conversations.history", &json!({"channel": cid, "oldest": mark, "limit": 200}))?;
        let mut msgs: Vec<Value> = h["messages"].as_array().cloned().unwrap_or_default();
        msgs.sort_by(|a, b| ts_num(sv(a, "ts")).total_cmp(&ts_num(sv(b, "ts"))));
        let (mut sent, mut mark) = (0, mark);
        for m in msgs {
            if ts_num(sv(&m, "ts")) <= ts_num(&mark) {
                continue;
            }
            // only John steers a swarm; bots (the persona itself) and edits are not input
            if m["user"].as_str() == Some(self.john.as_str()) && m["bot_id"].is_null() && matches!(m["subtype"].as_str(), None | Some("file_share")) {
                sent += self.to_lead(s, &m);
            }
            mark = sv(&m, "ts").to_string();
            self.with_state(|st| st["channels"][cid] = json!(mark));
            self.save();
        }
        let _ = db;
        Ok(sent)
    }

    fn to_lead(&self, s: &Value, m: &Value) -> usize {
        let text = slackfmt::slack_to_md(sv(m, "text")).trim().to_string();
        if text.is_empty() {
            return 0;
        }
        let ts = m["ts"].as_str();
        let Some(goal) = s["goal"].as_str().filter(|g| !g.is_empty()) else {
            let _ = self.post(s, &format!("Slot {} is free. Start a swarm with `swarm new <name> <folder> <objective>`.", s["n"]), ts);
            return 0;
        };
        let lead = s["lead"].as_str().map(String::from).unwrap_or_else(|| format!("{goal}-lead"));
        match (self.input)(&lead, &format!("John (via Slack): {}", text.replace('\n', " / "))) {
            Ok(()) => 1,
            Err(e) => {
                let _ = self.post(s, &format!("⚠ Couldn't reach {lead} (goal {}): {}", s["state"].as_str().unwrap_or("?"), crate::db::anyhow_msg(&e)), ts);
                0
            }
        }
    }

    // ---- outbound: the goal's board thread into the channel

    /// One pass over every slot with a goal; returns how many posts went out.
    pub fn relay(&self, db: &Db) -> Result<usize> {
        Ok(slots::list(db)?.iter().filter(|s| s["goal"].as_str().is_some_and(|g| !g.is_empty())).map(|s| self.relay_slot(db, s)).sum())
    }

    pub fn relay_slot(&self, db: &Db, s: &Value) -> usize {
        let (Some(cid), Some(tid)) = (s["channel_id"].as_str().filter(|c| !c.is_empty()), s["thread_id"].as_i64()) else { return 0 };
        let goal = sv(s, "goal").to_string();
        let msgs: Vec<Value> = db.get_thread(tid).ok().and_then(|d| d["messages"].as_array().cloned()).unwrap_or_default();
        let mut last = self.with_state(|st| st["goals"][&goal]["last"].as_i64());
        if last.is_none() {
            last = Some(msgs.first().and_then(|m| m["id"].as_i64()).unwrap_or(0)); // the thread's opener says what "swarm new" already said
            self.with_state(|st| st["goals"][&goal]["last"] = json!(last));
        }
        let mut last = last.unwrap_or(0);
        let mut sent = 0;
        for m in msgs {
            let id = m["id"].as_i64().unwrap_or(0);
            if id <= last {
                continue;
            }
            let landed = (|| -> Result<bool> {
                if skip(&m) {
                    return Ok(false);
                }
                let author = sv(&m, "author");
                if author == "agentdesk" || s["lead"].as_str() == Some(author) {
                    self.post(s, sv(&m, "body"), None)?;
                } else {
                    let act = self.activity(s, &goal, cid)?;
                    self.post(s, &format!("**{author}:** {}", sv(&m, "body")), Some(&act))?;
                }
                Ok(true)
            })();
            match landed {
                Ok(true) => sent += 1,
                Ok(false) => {}
                Err(e) => {
                    let code = slack_error(&e).map(String::from);
                    self.log(&format!("slot {}: relaying message {id} failed: {}", s["n"], code.clone().unwrap_or_else(|| crate::db::anyhow_msg(&e))));
                    if !code.as_deref().is_some_and(|c| GONE.contains(&c)) {
                        break; // try again next pass
                    }
                }
            }
            last = id;
            self.with_state(|st| st["goals"][&goal]["last"] = json!(id));
            self.save();
        }
        sent
    }

    /// The ts of today's "activity" message in the slot's channel, posted on first use.
    fn activity(&self, s: &Value, goal: &str, cid: &str) -> Result<String> {
        let day = (self.today)();
        let a = self.with_state(|st| st["goals"][goal]["activity"].clone());
        if sv(&a, "day") != day || sv(&a, "channel") != cid {
            let ts = self.post(s, &format!("Activity, {day}: member notes in this thread."), None)?;
            self.with_state(|st| st["goals"][goal]["activity"] = json!({"day": day, "channel": cid, "ts": ts}));
            self.save();
            return Ok(ts);
        }
        Ok(sv(&a, "ts").to_string())
    }
}

/// Receipts are noise; John knows what he wrote.
fn skip(m: &Value) -> bool {
    let meta = m["meta"].as_str().and_then(|t| serde_json::from_str::<Value>(t).ok()).or_else(|| m["meta"].is_object().then(|| m["meta"].clone()));
    let kind = meta.as_ref().and_then(|m| m["kind"].as_str());
    matches!(kind, Some("ack" | "ack-note" | "read-receipt")) || m["author_kind"].as_str() == Some("human")
}

/// The loop the bridge runs every 15 s on a bot with the swarms area.
pub fn pass(sw: &Swarms, db: &Db) {
    if let Err(e) = sw.relay(db) {
        paths::log("WARN", &format!("swarm relay: {}", crate::db::anyhow_msg(&e)));
    }
    if let Err(e) = sw.poll(db) {
        paths::log("WARN", &format!("swarm poll: {}", crate::db::anyhow_msg(&e)));
    }
}
