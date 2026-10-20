//! Phone commands for the Slack bridge: John types `agents`, `concierge off`, `status`... in a bot's DM. A port of
//! `python/agentdesk/slackcmd.py`. Each command is the same `ui:*` request the Python one sent to the core; in here the core is this
//! process, so a request is `ui_api::call` on the bridge's own board handle instead of an `agentdesk ui` subprocess.
//!
//! Each command belongs to an area (agents, concierge, ops, swarms); the bridge can run several Slack bots from bots.json, each
//! answering only its own areas plus `help`. The `board` area is the question relay (and `wake`), which live in `slack.rs`.
//! Only John's Slack user may run them.

use crate::db::{anyhow_msg, Db};
use crate::identity::Caller;
use crate::swarm::Swarms;
use crate::{paths, ui_api};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const AREAS: [&str; 5] = ["board", "agents", "concierge", "ops", "swarms"];
const BOARD_HELP: &str = "Reply *in a question's thread* to answer it; `wake` there resumes the agent.";
const NO_SWARMS: &str = "Swarm slots need the Slack bridge's client (this bot has none).";
const SWARM_USAGE: &str = "Usage: `swarm new <name> <folder> <objective>` · `swarm approve <slot> [members=N hours=H cadence=M]` · `swarm end <slot>` · `swarm reset <slot>`";

/// (area, command word, help line); the help line is None for a command that is only a reply (`yes`).
const TABLE: [(&str, &str, Option<&str>); 9] = [
    ("agents", "agents", Some("`agents` list them")),
    ("agents", "agent", Some("`agent new <name> <folder> [charter]` · `agent start|stop|forget <name>`")),
    ("agents", "yes", None),
    ("concierge", "concierge", Some("`concierge` status · `concierge on|off`")),
    ("ops", "status", Some("`status` core, Concierge, agents, questions, usage")),
    ("ops", "update", Some("`update` versions · `update apply` update and restart")),
    ("ops", "governor", Some("`governor` status · `governor on|off` enforce the usage caps, or only advise")),
    ("swarms", "swarms", Some("`swarms` the 10 swarm slots")),
    ("swarms", "swarm", Some("`swarm new <name> <folder> <objective>` · `swarm approve|end|reset <slot>`")),
];

/// One request to the core: the answer, or `{"error": ...}` as `agentdesk ui` prints a failure. John is the caller: no session, no identity.
pub fn call(db: &Db, tool: &str, args: Value) -> Value {
    ui_api::call(db, &Caller::default(), tool, &args).unwrap_or_else(|e| json!({"error": anyhow_msg(&e)}))
}

/// `⚠ <error>` when the reply is an error document.
pub fn err(r: &Value) -> Option<String> {
    r["error"].as_str().filter(|e| !e.is_empty()).map(|e| format!("⚠ {e}"))
}

pub struct Commands {
    areas: Vec<String>,
    john: String,
    bot: String,
    /// `agent forget` waits for a `yes` within five minutes.
    pending: Mutex<Option<(String, Instant)>>,
    /// The swarm slots, on the bot that has the swarms area.
    swarm: Option<Arc<Swarms>>,
}

fn row(r: &Value) -> String {
    let gen = r["generation"].as_i64().map(|g| format!(" · gen {g}")).unwrap_or_default();
    format!("*{}* · {}{gen} · `{}`", r["name"].as_str().unwrap_or(""), r["state"].as_str().unwrap_or(""), r["folder"].as_str().unwrap_or(""))
}

fn word(text: &str, n: usize) -> String {
    text.split_whitespace().nth(n).unwrap_or("").to_lowercase()
}

/// Python's `{x:g}` for a count.
fn count(v: &Value) -> String {
    let n = v.as_f64().unwrap_or(0.0);
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn held_list(v: &Value, hash: bool) -> Option<String> {
    let held = v.as_array().filter(|h| !h.is_empty())?;
    Some(
        held.iter()
            .map(|x| if hash { format!("#{}", x.as_str().map(String::from).unwrap_or_else(|| x.to_string())) } else { x.as_str().unwrap_or("").to_string() })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn concierge_line(c: &Value) -> String {
    let held = held_list(&c["held"], true).map(|h| format!(", holding {h}")).unwrap_or_default();
    let swarm = c["members"].as_array().filter(|m| !m.is_empty()).map(|m| format!(", {} in its swarm", m.len())).unwrap_or_default();
    format!("Concierge is {}, {} open item(s) waiting{held}{swarm}.", if c["on"] == json!(true) { "on" } else { "off" }, count(&c["open"]))
}

fn governor_line(g: &Value) -> String {
    let enforcing = g["enforcing"] == json!(true);
    let would = if enforcing { String::new() } else { format!(", would have queued {} and shed {}", count(&g["would_queue"]), count(&g["would_shed"])) };
    let held = held_list(&g["held"], false).map(|h| format!(", holding {h}")).unwrap_or_default();
    format!("Governor is {}{would}{held}.\n{}", if enforcing { "*enforcing*" } else { "advisory" }, g["summary"].as_str().unwrap_or("").trim_start_matches("governor: "))
}

fn ago(iso: Option<&str>) -> f64 {
    iso.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() as f64).unwrap_or(1e9)
}

impl Commands {
    pub fn new(areas: &[String], john: &str, bot: &str) -> Commands {
        Commands { areas: areas.to_vec(), john: john.into(), bot: bot.into(), pending: Mutex::new(None), swarm: None }
    }

    pub fn with_swarm(mut self, s: Arc<Swarms>) -> Commands {
        self.swarm = Some(s);
        self
    }

    fn has(&self, word: &str) -> bool {
        TABLE.iter().any(|(a, w, _)| *w == word && self.areas.iter().any(|x| x == a))
    }

    pub fn help(&self) -> String {
        let mut lines: Vec<String> = TABLE.iter().filter(|(a, _, h)| h.is_some() && self.areas.iter().any(|x| x == a)).map(|(_, _, h)| h.unwrap().to_string()).collect();
        if self.areas.iter().any(|a| a == "board") {
            lines.push(BOARD_HELP.into());
        }
        lines.push("`help` this list".into());
        format!("*{}* commands\n{}", self.bot, lines.iter().map(|l| format!("• {l}")).collect::<Vec<_>>().join("\n"))
    }

    /// The reply to post, or None when this is not one of this bot's commands (or not John's).
    pub fn handle(&self, db: &Db, text: &str, user: Option<&str>) -> Option<String> {
        let text = crate::slackfmt::unescape(text);
        let text = text.trim();
        let w = word(text, 0);
        if w != "help" && !self.has(&w) {
            return None;
        }
        let john = user == Some(self.john.as_str());
        paths::log(
            "INFO",
            &format!("slack_cmd: bot={} user={} {}: {:?}", self.bot, user.unwrap_or("?"), if john { "ran" } else { "REFUSED" }, text.chars().take(200).collect::<String>()),
        );
        if !john {
            return None; // silently: never run anything for anyone else
        }
        if w == "help" {
            return Some(self.help());
        }
        Some(match w.as_str() {
            "agents" => self.agents(db),
            "agent" => self.agent(db, text),
            "yes" => self.yes(db),
            "concierge" => self.concierge(db, text),
            "status" => self.status(db),
            "update" => self.update(db, text),
            "governor" => self.governor(db, text),
            "swarms" => self.swarm.as_ref().map(|s| s.list(db)).unwrap_or_else(|| NO_SWARMS.to_string()),
            _ => self.swarm_cmd(db, text),
        })
    }

    fn swarm_cmd(&self, db: &Db, text: &str) -> String {
        let Some(s) = &self.swarm else { return NO_SWARMS.into() };
        if let Some(m) = Regex::new(r#"(?is)^swarm\s+new\s+(\S+)\s+("[^"]+"|\S+)\s+(\S.*)"#).unwrap().captures(text) {
            return s.new_swarm(db, &m[1], m[2].trim_matches('"'), m[3].trim());
        }
        let Some(m) = Regex::new(r"(?i)^swarm\s+(approve|end|reset)\s+(\d+)((?:\s+\w+=[\d.]+)*)\s*$").unwrap().captures(text) else { return SWARM_USAGE.into() };
        let (verb, n) = (m[1].to_lowercase(), m[2].parse::<i64>().unwrap_or(0));
        let mut opts = json!({});
        for o in m[3].split_whitespace() {
            let (k, v) = o.split_once('=').unwrap_or(("", ""));
            let parsed = match k.to_lowercase().as_str() {
                "members" => v.parse::<i64>().ok().map(|x| ("max_members", json!(x))),
                "hours" => v.parse::<f64>().ok().map(|x| ("max_hours", json!(x))),
                "cadence" => v.parse::<f64>().ok().map(|x| ("cadence_minutes", json!(x))),
                _ => None,
            };
            match parsed {
                Some((key, val)) if verb == "approve" => opts[key] = val,
                _ => return SWARM_USAGE.into(),
            }
        }
        match verb.as_str() {
            "approve" => s.approve(db, n, &opts),
            "end" => s.end(db, n),
            _ => s.reset(db, n),
        }
    }

    fn agents(&self, db: &Db) -> String {
        let rows = call(db, "ui:identity_list", json!({}))["identities"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            "No agents yet. `agent new <name> <folder> [charter]`".into()
        } else {
            rows.iter().map(row).collect::<Vec<_>>().join("\n")
        }
    }

    fn agent(&self, db: &Db, text: &str) -> String {
        if let Some(m) = Regex::new(r#"(?is)^agent\s+new\s+(\S+)\s+("[^"]+"|\S+)\s*(.*)"#).unwrap().captures(text) {
            let (name, folder, charter) = (m[1].to_string(), m[2].trim_matches('"').to_string(), m[3].trim().to_string());
            let made = call(db, "ui:identity_create", json!({"name": name, "folder": folder, "charter": if charter.is_empty() { Value::Null } else { json!(charter) }}));
            if let Some(e) = err(&made) {
                return e;
            }
            let started = call(db, "ui:identity_start", json!({"name": name}));
            return err(&started).unwrap_or_else(|| format!("Created {}", row(&started)));
        }
        let Some(m) = Regex::new(r"(?i)^agent\s+(start|stop|forget)\s+(\S+)\s*$").unwrap().captures(text) else {
            return "Usage: `agent new <name> <folder> [charter]` · `agent start|stop|forget <name>`".into();
        };
        let (verb, name) = (m[1].to_lowercase(), m[2].to_string());
        if verb == "forget" {
            *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some((name.clone(), Instant::now()));
            return format!("Reply `yes` to forget *{name}* (its Claude conversation is kept).");
        }
        let r = call(db, &format!("ui:identity_{verb}"), json!({"name": name}));
        err(&r).unwrap_or_else(|| row(&r))
    }

    fn yes(&self, db: &Db) -> String {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
        match pending {
            Some((name, when)) if when.elapsed().as_secs() <= 300 => {
                let r = call(db, "ui:identity_forget", json!({"name": name}));
                err(&r).unwrap_or_else(|| format!("Forgot *{}*.", r["forgotten"].as_str().unwrap_or(&name)))
            }
            _ => "Nothing to confirm.".into(),
        }
    }

    fn concierge(&self, db: &Db, text: &str) -> String {
        let args = match word(text, 1).as_str() {
            "on" => json!({"on": true}),
            "off" => json!({"on": false}),
            "" => json!({}),
            _ => return "Usage: `concierge` status · `concierge on|off`".into(),
        };
        let r = call(db, "ui:concierge", args);
        err(&r).unwrap_or_else(|| concierge_line(&r))
    }

    fn status(&self, db: &Db) -> String {
        let s = call(db, "ui:status", json!({}));
        if let Some(e) = err(&s) {
            return e;
        }
        let ids = call(db, "ui:identity_list", json!({}))["identities"].as_array().cloned().unwrap_or_default();
        let qs = call(db, "list_threads", json!({"channel": "question", "status": "open", "limit": 200}));
        let (c, sessions) = (&s["concierge"], &s["sessions"]);
        let n = |state: &str| ids.iter().filter(|r| r["state"].as_str() == Some(state)).count();
        let governor = s["governor"]["summary"].as_str().unwrap_or("");
        let held = held_list(&c["held"], true).map(|h| format!(", holding {h}")).unwrap_or_default();
        let agents = match sessions["summary"].as_str().filter(|x| !x.is_empty()) {
            Some(sum) => format!("*Agents* {sum}, {} queued", n("queued")),
            None => format!("*Agents* {} running / cap {}, {} queued", n("running"), sessions["max"].as_i64().map(|m| m.to_string()).unwrap_or_else(|| "?".into()), n("queued")),
        };
        let mut lines = vec![
            format!("*Slack* {}", if ago(s["slack"]["ts"].as_str()) < 90.0 { "up" } else { "down" }),
            format!("*Concierge* {}{held}", if c["on"] == json!(true) { "on" } else { "off" }),
            agents,
            format!("*Open questions* {}", qs["threads"].as_array().map(Vec::len).unwrap_or(0)),
            format!("*Usage* {}", s["usage"]["summary"].as_str().unwrap_or("?")),
        ];
        if !governor.is_empty() {
            lines.push(format!("*Governor* {}", governor.trim_start_matches("governor: ")));
        }
        lines.join("\n")
    }

    fn governor(&self, db: &Db, text: &str) -> String {
        let r = match word(text, 1).as_str() {
            "on" => call(db, "ui:governor_enforce", json!({"on": true})),
            "off" => call(db, "ui:governor_enforce", json!({"on": false})),
            "" => call(db, "ui:governor", json!({})),
            _ => return "Usage: `governor` status · `governor on|off` enforce its caps, or only advise".into(),
        };
        err(&r).unwrap_or_else(|| governor_line(&r))
    }

    fn update(&self, db: &Db, text: &str) -> String {
        let r = call(db, "ui:update", json!({"apply": text.to_lowercase().split_whitespace().skip(1).collect::<Vec<_>>() == ["apply"]}));
        if r["error"].as_str().is_some_and(|e| e.contains("unknown request")) {
            return "Update isn't available yet (this core has no `ui:update`).".into();
        }
        if let Some(e) = err(&r) {
            return e;
        }
        r.as_object()
            .map(|o| {
                o.iter()
                    .filter(|(_, v)| !v.is_object() && !v.is_array())
                    .map(|(k, v)| {
                        let shown = match v {
                            Value::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
                            Value::String(s) => s.clone(),
                            Value::Null => "None".into(),
                            other => other.to_string(),
                        };
                        format!("*{k}* {shown}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}
