//! Phone commands for the Slack bridge: John types `agents`, `concierge off`, `status`... in a bot's DM. A port of
//! `agentdesk/slackcmd.py`, minus the pipe: the commands call the board and the core's functions directly.
//!
//! Each command belongs to an area (agents, concierge, ops, swarms); the bridge can run several Slack bots from bots.json, each
//! answering only its own areas plus `help`. The `board` area is the question relay (and `wake`), which live in `slack.rs`.
//! Only John's Slack user may run them.

use crate::db::Db;
use crate::swarm::Swarms;
use crate::{concierge, governor, identities as ids, paths, update, usage};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const AREAS: [&str; 5] = ["board", "agents", "concierge", "ops", "swarms"];
const BOARD_HELP: &str = "Reply *in a question's thread* to answer it; `wake` there resumes the agent.";
const NO_SWARMS: &str = "Swarm slots need the Slack bridge's client (this bot has none).";
const SWARM_USAGE: &str = "Usage: `swarm new <name> <folder> <objective>` · `swarm approve <slot> [members=N hours=H cadence=M]` · `swarm end <slot>` · `swarm reset <slot>`";

/// (area, command word, help line); the help line is None for a command that is only a reply (`yes`).
const TABLE: [(&str, &str, Option<&str>); 10] = [
    ("agents", "agents", Some("`agents` list them")),
    ("agents", "agent", Some("`agent new <name> <folder> [charter]` · `agent start|stop|forget <name>`")),
    ("agents", "yes", None),
    ("concierge", "concierge", Some("`concierge` status · `concierge on|off`")),
    ("ops", "status", Some("`status` core, Concierge, agents, questions, usage")),
    ("ops", "update", Some("`update` versions · `update apply` update and restart")),
    ("ops", "governor", Some("`governor` status · `governor on|off` enforce the usage caps, or only advise")),
    ("swarms", "swarms", Some("`swarms` the 10 swarm slots")),
    ("swarms", "swarm", Some("`swarm new <name> <folder> <objective>` · `swarm approve|end|reset <slot>`")),
    ("board", "", None),
];

pub struct Commands {
    areas: Vec<String>,
    john: String,
    bot: String,
    /// `agent forget` waits for a `yes` within five minutes.
    pending: Mutex<Option<(String, Instant)>>,
    /// The swarm slots, on the bot that has the swarms area.
    swarm: Option<Arc<Swarms>>,
}

fn warn(r: &anyhow::Error) -> String {
    format!("⚠ {}", crate::db::anyhow_msg(r))
}

fn row(r: &Value) -> String {
    let gen = r["generation"].as_i64().filter(|g| *g > 0).map(|g| format!(" · gen {g}")).unwrap_or_default();
    format!("*{}* · {}{gen} · `{}`", r["name"].as_str().unwrap_or(""), r["state"].as_str().unwrap_or(""), r["folder"].as_str().unwrap_or(""))
}

fn word(text: &str, n: usize) -> String {
    text.split_whitespace().nth(n).unwrap_or("").to_lowercase()
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
        TABLE.iter().any(|(a, w, _)| *w == word && !word.is_empty() && self.areas.iter().any(|x| x == a))
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
        let text = text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
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
            "update" => self.update(text),
            "governor" => self.governor(db, text),
            "swarms" => self.swarm.as_ref().map(|s| s.list(db)).unwrap_or_else(|| NO_SWARMS.to_string()),
            _ => self.swarm_cmd(db, text),
        })
    }

    fn swarm_cmd(&self, db: &Db, text: &str) -> String {
        let Some(s) = &self.swarm else { return NO_SWARMS.into() };
        if let Some(m) = Regex::new(r#"(?is)^swarm\s+new\s+(\S+)\s+("[^"]+"|\S+)\s+(\S.*)$"#).unwrap().captures(text) {
            return s.new_swarm(db, &m[1], m[2].trim_matches('"'), m[3].trim());
        }
        let Some(m) = Regex::new(r"(?i)^swarm\s+(approve|end|reset)\s+(\d+)((?:\s+\w+=[\d.]+)*)\s*$").unwrap().captures(text) else { return SWARM_USAGE.into() };
        let (verb, n) = (m[1].to_lowercase(), m[2].parse::<i64>().unwrap_or(0));
        let (mut members, mut hours, mut cadence) = (None, None, None);
        for o in m[3].split_whitespace() {
            let (k, v) = o.split_once('=').unwrap_or(("", ""));
            let ok = match (verb.as_str(), k.to_lowercase().as_str()) {
                ("approve", "members") => v.parse().map(|x| members = Some(x)).is_ok(),
                ("approve", "hours") => v.parse().map(|x| hours = Some(x)).is_ok(),
                ("approve", "cadence") => v.parse().map(|x| cadence = Some(x)).is_ok(),
                _ => false,
            };
            if !ok {
                return SWARM_USAGE.into();
            }
        }
        match verb.as_str() {
            "approve" => s.approve(db, n, members, hours, cadence),
            "end" => s.end(db, n),
            _ => s.reset(db, n),
        }
    }

    fn agents(&self, db: &Db) -> String {
        match ids::list(db) {
            Ok(rows) if !rows.is_empty() => rows.iter().map(row).collect::<Vec<_>>().join("\n"),
            Ok(_) => "No agents yet. `agent new <name> <folder> [charter]`".into(),
            Err(e) => warn(&e),
        }
    }

    fn agent(&self, db: &Db, text: &str) -> String {
        if let Some(m) = Regex::new(r#"(?is)^agent\s+new\s+(\S+)\s+("[^"]+"|\S+)\s*(.*)$"#).unwrap().captures(text) {
            let charter = m[3].trim();
            let made = ids::create(db, &m[1], m[2].trim_matches('"'), if charter.is_empty() { None } else { Some(charter) }, None, false).and_then(|_| ids::start(db, &m[1]));
            return match made {
                Ok(r) => format!("Created {}", row(&r)),
                Err(e) => warn(&e),
            };
        }
        let Some(m) = Regex::new(r"(?i)^agent\s+(start|stop|forget)\s+(\S+)\s*$").unwrap().captures(text) else {
            return "Usage: `agent new <name> <folder> [charter]` · `agent start|stop|forget <name>`".into();
        };
        let (verb, name) = (m[1].to_lowercase(), m[2].to_string());
        if verb == "forget" {
            *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some((name.clone(), Instant::now()));
            return format!("Reply `yes` to forget *{name}* (its Claude conversation is kept).");
        }
        match if verb == "start" { ids::start(db, &name) } else { ids::stop(db, &name) } {
            Ok(r) => row(&r),
            Err(e) => warn(&e),
        }
    }

    fn yes(&self, db: &Db) -> String {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
        match pending {
            Some((name, when)) if when.elapsed().as_secs() <= 300 => match ids::forget(db, &name) {
                Ok(_) => format!("Forgot *{name}*."),
                Err(e) => warn(&e),
            },
            _ => "Nothing to confirm.".into(),
        }
    }

    fn concierge(&self, db: &Db, text: &str) -> String {
        let on = match word(text, 1).as_str() {
            "on" => Some(true),
            "off" => Some(false),
            "" => None,
            _ => return "Usage: `concierge` status · `concierge on|off`".into(),
        };
        match concierge::toggle(db, &crate::identity::Caller::default(), &concierge::default_folder(db), on) {
            Ok(c) => concierge_line(&c),
            Err(e) => warn(&e),
        }
    }

    fn status(&self, db: &Db) -> String {
        let ids = ids::list(db).unwrap_or_default();
        let count = |s: &str| ids.iter().filter(|r| r["state"].as_str() == Some(s)).count();
        let c = concierge::state(db).unwrap_or(Value::Null);
        let held =
            c["held"].as_array().filter(|h| !h.is_empty()).map(|h| format!(", holding {}", h.iter().map(|x| format!("#{x}")).collect::<Vec<_>>().join(", "))).unwrap_or_default();
        let slack_up = std::fs::read_to_string(paths::data_dir().join("slack_bridge.state"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["ts"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()))
            .is_some_and(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < 90);
        let questions = db.open_questions(false).map(|q| q.len()).unwrap_or(0);
        let rep = governor::report(db, chrono::Utc::now(), false, None);
        let mut lines = vec![
            format!("*Slack* {}", if slack_up { "up" } else { "down" }),
            format!("*Concierge* {}{held}", if c["on"] == json!(true) { "on" } else { "off" }),
            format!(
                "*Agents* {}",
                rep["pool"]["summary"].as_str().map(|s| format!("{s}, {} queued", count("queued"))).unwrap_or_else(|| format!(
                    "{} running, {} queued",
                    count("running"),
                    count("queued")
                ))
            ),
            format!("*Open questions* {questions}"),
            format!("*Usage* {}", usage::report()["summary"].as_str().unwrap_or("?")),
        ];
        if let Some(g) = rep["summary"].as_str() {
            lines.push(format!("*Governor* {}", g.trim_start_matches("governor: ")));
        }
        lines.join("\n")
    }

    fn update(&self, text: &str) -> String {
        if word(text, 1) == "apply" {
            return match update::apply() {
                Ok(v) => format!("*updated* to {v}. Restart the window or daemon to run it."),
                Err(e) => warn(&e),
            };
        }
        match update::check() {
            Ok((latest, newer)) => format!("*current* {}\n*latest* {latest}\n*newer* {newer}", update::CURRENT),
            Err(e) => warn(&e),
        }
    }

    fn governor(&self, db: &Db, text: &str) -> String {
        match word(text, 1).as_str() {
            "on" | "off" => {
                if let Err(e) = governor::set_enforce(word(text, 1) == "on") {
                    return warn(&e);
                }
            }
            "" => {}
            _ => return "Usage: `governor` status · `governor on|off` enforce its caps, or only advise".into(),
        }
        let g = governor::report(db, chrono::Utc::now(), false, None);
        let mode = if g["enforcing"] == json!(true) { "*enforcing*" } else { "advisory" };
        format!("Governor is {mode}.\n{}", g["summary"].as_str().unwrap_or("").trim_start_matches("governor: "))
    }
}

fn concierge_line(c: &Value) -> String {
    let held =
        c["held"].as_array().filter(|h| !h.is_empty()).map(|h| format!(", holding {}", h.iter().map(|x| format!("#{x}")).collect::<Vec<_>>().join(", "))).unwrap_or_default();
    let swarm = c["members"].as_array().filter(|m| !m.is_empty()).map(|m| format!(", {} in its swarm", m.len())).unwrap_or_default();
    format!("Concierge is {}, {} open item(s) waiting{held}{swarm}.", if c["on"] == json!(true) { "on" } else { "off" }, c["open"].as_f64().unwrap_or(0.0))
}
