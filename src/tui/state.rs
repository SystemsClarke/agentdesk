//! What the window reads from the board on each refresh: the four channel lists and the status the main menu, SysOp,
//! Who's On and Options screens show besides them.

use crate::db::{Db, RECEIPT_KINDS};
use crate::{jobs, paths, permissions, usage};
use chrono::{DateTime, Local};
use serde_json::Value;
use std::collections::HashMap;

pub const CHANNELS: [&str; 4] = ["question", "discussion", "wiki", "work"];

pub fn ts(v: &Value) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(v.as_str()?).ok().map(|t| t.with_timezone(&Local))
}

fn kind_of(meta: &Value) -> String {
    meta.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["kind"].as_str().map(String::from)).unwrap_or_default()
}

fn meta_of(meta: &Value) -> Value {
    meta.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).unwrap_or(Value::Null)
}

#[derive(Clone, Debug)]
pub struct ThreadRow {
    pub id: i64,
    pub channel: String,
    pub subject: String,
    pub opened_by: String,
    pub status: String,
    pub created: Option<DateTime<Local>>,
    pub updated: Option<DateTime<Local>>,
    pub message_count: i64,
    pub last_author: Option<String>,
    pub delivery: Option<String>,
    pub waiting: bool,
    pub follow_up: Option<String>,
    pub holder: Option<String>,
}

impl ThreadRow {
    pub fn from(v: &Value) -> ThreadRow {
        let meta = meta_of(&v["meta"]);
        ThreadRow {
            id: v["id"].as_i64().unwrap_or(0),
            channel: v["channel"].as_str().unwrap_or("").into(),
            subject: v["subject"].as_str().unwrap_or("").into(),
            opened_by: v["opened_by"].as_str().unwrap_or("").into(),
            status: v["status"].as_str().unwrap_or("").into(),
            created: ts(&v["created_ts"]),
            updated: ts(&v["updated_ts"]),
            message_count: v["message_count"].as_i64().unwrap_or(0),
            last_author: v["last_author"].as_str().map(String::from),
            delivery: v["delivery"].as_str().map(String::from),
            waiting: v["waiting"].as_i64() == Some(1),
            follow_up: v["follow_up"].as_str().map(String::from),
            holder: meta["assignee"].as_str().map(String::from),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub author: String,
    pub kind: String,
    pub ts: Option<DateTime<Local>>,
    pub body: String,
    pub via: String,
}

#[derive(Clone, Debug)]
pub struct Detail {
    pub thread: ThreadRow,
    pub messages: Vec<Msg>,
}

impl Detail {
    pub fn load(db: &Db, id: i64) -> Option<Detail> {
        let doc = db.get_thread(id).ok()?;
        let mut thread = ThreadRow::from(&doc["thread"]);
        thread.waiting = doc["thread"]["waiting"].as_i64() == Some(1);
        let messages = doc["messages"]
            .as_array()?
            .iter()
            .map(|m| {
                let meta = meta_of(&m["meta"]);
                Msg {
                    author: m["author"].as_str().unwrap_or("").into(),
                    kind: kind_of(&m["meta"]),
                    ts: ts(&m["ts"]),
                    body: m["body"].as_str().unwrap_or("").into(),
                    via: meta["via"].as_str().unwrap_or("").into(),
                }
            })
            .collect();
        Some(Detail { thread, messages })
    }
}

#[derive(Clone, Debug)]
pub struct PrRow {
    pub url: String,
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub requested_by: String,
    pub thread_id: Option<i64>,
    pub state: String,
    pub checked: Option<DateTime<Local>>,
    pub last_error: Option<String>,
    pub triage: Option<String>,
    pub scan: bool,
}

#[derive(Clone, Debug)]
pub struct Post {
    pub author: String,
    pub ts: Option<DateTime<Local>>,
    pub thread_id: i64,
    pub subject: String,
    pub channel: String,
    pub kind: String,
    pub first: bool,
    pub via: String,
}

#[derive(Clone, Debug)]
pub struct JobRow {
    pub name: String,
    pub folder: String,
    pub prompt: String,
    pub model: String,
    pub at: Option<String>,
    pub days: String,
    pub every: Option<f64>,
    pub enabled: bool,
    pub next: Option<DateTime<Local>>,
    pub last: Option<DateTime<Local>>,
    pub status: Option<String>,
    pub runs: i64,
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub id: i64,
    pub rule: String,
    pub scope: String,
    pub reason: String,
    pub blocked: Option<String>,
    pub requested_by: String,
    pub thread_id: Option<i64>,
    pub status: String,
    pub created: Option<DateTime<Local>>,
    pub decided: Option<DateTime<Local>>,
    pub path: Option<String>,
    pub broad: bool,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub name: String,
    pub folder: String,
    pub host: String,
    pub model: String,
    pub state: String,
    pub generation: i64,
}

#[derive(Clone, Debug)]
pub struct GoalRow {
    pub name: String,
    pub state: String,
    pub objective: String,
    pub lead: String,
    pub success: Option<String>,
    pub experiments: i64,
    pub last_value: Option<f64>,
    pub members: i64,
    pub max_members: i64,
    pub standing: bool,
}

/// The usage governor in the shape the budget panel shows (`ui:governor`).
#[derive(Clone, Debug, Default)]
pub struct Budget {
    pub samples: i64,
    pub remaining: f64,
    pub reset_in_hours: f64,
    pub projected_end: f64,
    pub sessions: i64,
    pub swarms: i64,
    pub members: i64,
    pub reason: String,
    pub series: Vec<f64>,
    pub forecast: Vec<f64>,
    pub mode: String,
    pub pool: String,
    pub status: String,
    pub plan_end: f64,
}

#[derive(Clone, Debug)]
pub struct SwarmMember {
    pub identity: String,
    pub task: String,
    pub work_id: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct VaultHealth {
    pub ts: DateTime<Local>,
    pub unpushed: Option<i64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct State {
    pub rows: HashMap<String, Vec<ThreadRow>>,
    pub open_qs: Vec<ThreadRow>,
    pub prs: Vec<PrRow>,
    pub recent: Vec<Post>,
    pub callers: Vec<Post>,
    pub bios: HashMap<String, i64>,
    pub john_last: Option<Post>,
    pub since_posts: i64,
    pub last_filed: Option<ThreadRow>,
    pub usage_lines: Vec<String>,
    pub usage_summary: String,
    pub vault: Option<VaultHealth>,
    pub jobs: Vec<JobRow>,
    pub rules: Vec<Rule>,
    pub allow: Vec<String>,
    pub settings_path: String,
    pub agents: Vec<Agent>,
    pub has_session_tables: bool,
    pub goals: Vec<GoalRow>,
    pub slots: HashMap<String, i64>,
    pub concierge_on: bool,
    pub swarm: Vec<SwarmMember>,
    pub held_id: Option<i64>,
    pub budget: Option<Budget>,
    pub slack: Option<(Option<DateTime<Local>>, i64, Option<(Option<DateTime<Local>>, i64)>)>,
    pub max_sessions: i64,
}

fn post_from(v: &Value) -> Post {
    let meta = meta_of(&v["meta"]);
    Post {
        author: v["author"].as_str().unwrap_or("").into(),
        ts: ts(&v["ts"]),
        thread_id: v["thread_id"].as_i64().unwrap_or(0),
        subject: v["subject"].as_str().unwrap_or("").into(),
        channel: v["channel"].as_str().unwrap_or("").into(),
        kind: kind_of(&v["meta"]),
        first: v["first"].as_i64() == Some(1),
        via: meta["via"].as_str().unwrap_or("").into(),
    }
}

fn table_exists(db: &Db, name: &str) -> bool {
    db.scalar_i64("SELECT 1 FROM sqlite_master WHERE type='table' AND name=:n", &[(":n", &name)]).ok().flatten().is_some()
}

impl State {
    fn load_swarm(&mut self, db: &Db) {
        let list = crate::goals::list(db).ok().and_then(|v| v["goals"].as_array().cloned()).unwrap_or_default();
        self.goals = list
            .iter()
            .map(|g| GoalRow {
                name: g["name"].as_str().unwrap_or("").into(),
                state: g["state"].as_str().unwrap_or("").into(),
                objective: g["objective"].as_str().unwrap_or("").into(),
                lead: g["lead"].as_str().unwrap_or("").into(),
                success: g["success"].as_str().map(String::from),
                experiments: g["experiments"].as_i64().unwrap_or(0),
                last_value: g["last_value"].as_f64(),
                members: g["members"].as_i64().unwrap_or(0),
                max_members: g["max_members"].as_i64().unwrap_or(3),
                standing: g["standing"].as_i64() == Some(1),
            })
            .collect();
        if !self.goals.iter().any(|g| g.standing && g.name == "concierge") {
            self.goals.push(GoalRow {
                name: "concierge".into(),
                state: "off".into(),
                objective: "Keep Work to Hire drained".into(),
                lead: "concierge-lead".into(),
                success: Some("value <= 0".into()),
                experiments: 0,
                last_value: None,
                members: 0,
                max_members: 3,
                standing: true,
            });
        }
        if let Ok(c) = crate::concierge::state(db) {
            self.concierge_on = c["on"].as_bool().unwrap_or(false);
            self.held_id = c["held"].as_array().and_then(|h| h.first()).and_then(|v| v.as_i64());
            self.swarm = c["members"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|m| SwarmMember {
                            identity: m["identity"].as_str().unwrap_or("").into(),
                            task: m["task"].as_str().unwrap_or("").into(),
                            work_id: m["work_id"].as_i64(),
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        if let Ok(slots) = crate::slots::list(db) {
            for sl in slots["slots"].as_array().cloned().unwrap_or_default() {
                if let (Some(g), Some(n)) = (sl["goal"].as_str(), sl["n"].as_i64()) {
                    self.slots.insert(g.to_lowercase(), n);
                }
            }
        }
        let g = crate::governor::report(db, chrono::Utc::now(), false, None);
        if g["samples"].as_i64().unwrap_or(0) > 0 {
            let f = |k: &str| g[k].as_f64().unwrap_or(0.0);
            let arr = |k: &str| g[k].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
            self.budget = Some(Budget {
                samples: g["samples"].as_i64().unwrap_or(0),
                remaining: f("remaining"),
                reset_in_hours: f("reset_in_hours"),
                projected_end: f("projected_end_pct"),
                sessions: g["caps"]["total_sessions"].as_i64().unwrap_or(0),
                swarms: g["caps"]["swarms"].as_i64().unwrap_or(0),
                members: g["caps"]["members_per_swarm"].as_i64().unwrap_or(0),
                reason: g["reason"].as_str().unwrap_or("").into(),
                series: arr("series"),
                forecast: arr("forecast"),
                mode: if g["enforcing"].as_bool() == Some(true) { "enforcing".into() } else { "advisory".into() },
                pool: g["pool"]["summary"].as_str().unwrap_or("").into(),
                status: g["status"].as_str().unwrap_or("").into(),
                plan_end: f("plan_end_pct"),
            });
        }
    }

    pub fn load(db: &Db, show_archived: bool, show_decided: bool) -> State {
        let mut st = State::default();
        for ch in CHANNELS {
            let all: Vec<ThreadRow> = db.list_threads(Some(ch), None, 300, true, false).unwrap_or_default().iter().map(ThreadRow::from).collect();
            let mut rows: Vec<ThreadRow> = if ch == "question" { all.into_iter().filter(|r| (r.status == "archived") == show_archived).collect() } else { all };
            if ch == "question" {
                rows.sort_by_key(|r| !(show_archived || r.waiting)); // stable: ringing first
            }
            st.rows.insert(ch.into(), rows);
        }
        st.open_qs = db.open_questions(false).unwrap_or_default().iter().map(|q| ThreadRow::from(&serde_json::json!({"id": q["thread_id"], "subject": q["subject"], "opened_by": q["opened_by"], "created_ts": q["created_ts"], "updated_ts": q["updated_ts"], "channel": "question", "status": "open"}))).collect();
        st.prs = db
            .rows("SELECT * FROM pull_requests ORDER BY id DESC LIMIT 200", &[])
            .unwrap_or_default()
            .iter()
            .map(|p| PrRow {
                url: p["url"].as_str().unwrap_or("").into(),
                repo: p["repo"].as_str().unwrap_or("").into(),
                number: p["number"].as_i64().unwrap_or(0),
                title: p["title"].as_str().unwrap_or("").into(),
                requested_by: p["requested_by"].as_str().unwrap_or("").into(),
                thread_id: p["thread_id"].as_i64(),
                state: p["state"].as_str().unwrap_or("").into(),
                checked: ts(&p["checked_ts"]),
                last_error: p["last_error"].as_str().map(String::from),
                triage: p["triage"].as_str().map(String::from),
                scan: p["source"].as_str().map(|s| s != "agent-registered").unwrap_or(false),
            })
            .collect();
        let post_sql = "SELECT m.author, m.ts, m.thread_id, m.meta, t.subject, t.channel, (m.id = (SELECT MIN(id) FROM messages WHERE thread_id = m.thread_id)) AS first FROM messages m JOIN threads t ON t.id = m.thread_id";
        st.recent = db.rows(&format!("{post_sql} ORDER BY m.id DESC LIMIT 8"), &[]).unwrap_or_default().iter().map(post_from).collect();
        let day_ago = (chrono::Utc::now() - chrono::Duration::hours(24)).format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let callers = db
            .rows(
                &format!("{post_sql} WHERE m.author <> 'john' AND m.ts >= :d AND m.id = (SELECT MAX(x.id) FROM messages x WHERE x.author = m.author AND x.author_kind='agent') ORDER BY m.id DESC LIMIT 40"),
                &[(":d", &day_ago)],
            )
            .unwrap_or_default();
        st.callers = callers.iter().map(post_from).collect();
        st.bios = db
            .rows("SELECT id, subject FROM threads WHERE channel='discussion' AND subject LIKE 'bio: %' ORDER BY id DESC", &[])
            .unwrap_or_default()
            .iter()
            .filter_map(|r| Some((r["subject"].as_str()?.get(5..)?.trim().to_lowercase(), r["id"].as_i64()?)))
            .collect();
        let last_john = db.row(&format!("{post_sql} WHERE m.author = 'john' ORDER BY m.id DESC LIMIT 1"), &[]).ok().flatten();
        if let Some(j) = &last_john {
            st.john_last = Some(post_from(j));
            let after = db.scalar_i64("SELECT MAX(id) FROM messages WHERE author='john'", &[]).ok().flatten().unwrap_or(0);
            st.since_posts = db
                .scalar_i64(
                    "SELECT COUNT(*) FROM messages WHERE id > :a AND author <> 'john' AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta,'$.kind') END,'') NOT IN ('ack','ack-note','read-receipt')",
                    &[(":a", &after)],
                )
                .ok()
                .flatten()
                .unwrap_or(0);
        }
        st.last_filed = db.list_threads(Some("question"), Some("archived"), 1, true, false).ok().and_then(|v| v.first().map(ThreadRow::from));
        let u = usage::report();
        st.usage_lines = u["lines"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        st.usage_summary = u["summary"].as_str().unwrap_or("").into();
        st.vault = std::fs::read_to_string(paths::data_dir().join("vault_state.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| Some(VaultHealth { ts: ts(&v["ts"])?, unpushed: v["unpushed"].as_i64(), error: v["error"].as_str().filter(|e| !e.is_empty()).map(String::from) }));
        st.slack = std::fs::read_to_string(paths::data_dir().join("slack_bridge.state"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .map(|v| (ts(&v["ts"]), v["poll_s"].as_i64().unwrap_or(0), v["last_relay"].as_object().map(|r| (ts(&r["ts"].clone()), r["thread_id"].as_i64().unwrap_or(0)))));
        st.jobs = jobs::list(db)
            .unwrap_or_default()
            .iter()
            .map(|j| JobRow {
                name: j["name"].as_str().unwrap_or("").into(),
                folder: j["folder"].as_str().unwrap_or("").into(),
                prompt: j["prompt"].as_str().unwrap_or("").into(),
                model: j["model"].as_str().unwrap_or("").into(),
                at: j["at"].as_str().map(String::from),
                days: j["days"].as_str().unwrap_or("daily").into(),
                every: j["every_minutes"].as_f64(),
                enabled: j["enabled"].as_i64() == Some(1),
                next: ts(&j["next_run_ts"]),
                last: ts(&j["last_run_ts"]),
                status: j["last_status"].as_str().map(String::from),
                runs: j["runs"].as_i64().unwrap_or(0),
            })
            .collect();
        if let Ok(p) = permissions::list(db) {
            st.rules = p["proposals"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| Rule {
                            id: r["id"].as_i64().unwrap_or(0),
                            rule: r["rule"].as_str().unwrap_or("").into(),
                            scope: r["scope"].as_str().unwrap_or("").into(),
                            reason: r["reason"].as_str().unwrap_or("").into(),
                            blocked: r["blocked"].as_str().map(String::from),
                            requested_by: r["requested_by"].as_str().unwrap_or("").into(),
                            thread_id: r["thread_id"].as_i64(),
                            status: r["status"].as_str().unwrap_or("").into(),
                            created: ts(&r["created_ts"]),
                            decided: ts(&r["decided_ts"]),
                            path: r["settings_path"].as_str().map(String::from),
                            broad: r["broad"].as_bool().unwrap_or(false),
                        })
                        .collect()
                })
                .unwrap_or_default();
            st.allow = p["allow"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            st.settings_path = p["settings"].as_str().unwrap_or("").into();
        }
        let _ = show_decided;
        st.has_session_tables = table_exists(db, "identities");
        if st.has_session_tables {
            st.agents = db
                .rows("SELECT name, folder, host, model, state, generation FROM identities ORDER BY name", &[])
                .unwrap_or_default()
                .iter()
                .map(|a| Agent {
                    name: a["name"].as_str().unwrap_or("").into(),
                    folder: a["folder"].as_str().unwrap_or("").into(),
                    host: a["host"].as_str().unwrap_or("").into(),
                    model: a["model"].as_str().unwrap_or("").into(),
                    state: a["state"].as_str().unwrap_or("").into(),
                    generation: a["generation"].as_i64().unwrap_or(1),
                })
                .collect();
        }
        let _ = RECEIPT_KINDS;
        st.max_sessions = crate::identities::max_sessions() as i64;
        if st.has_session_tables {
            st.load_swarm(db);
        }
        st
    }
}
