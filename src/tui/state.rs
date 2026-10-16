//! What the window reads from the board on each refresh: the four channel lists and the status the main menu, SysOp,
//! Who's On and Options screens show besides them.

use crate::db::{Db, RECEIPT_KINDS};
use crate::{jobs, kinds, paths, permissions, usage};
use chrono::{DateTime, Local};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

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
    /// ask, run, permission or packet (docs/question-types.md); a thread with no `type` is an ask.
    pub kind: String,
    /// Steps ticked and steps in all, for a packet.
    pub progress: Option<(usize, usize)>,
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
            kind: kinds::kind_in(Some(&meta)).to_string(),
            progress: (kinds::kind_in(Some(&meta)) == kinds::PACKET).then(|| kinds::progress(&meta)),
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
    /// A hash of what the reader draws from this message: the rendered Markdown is cached against it.
    pub rev: u64,
}

#[derive(Clone, Debug)]
pub struct Detail {
    pub thread: ThreadRow,
    pub messages: Vec<Msg>,
    /// The thread's kind (a permission proposal made before kinds existed is found through its row in `permission_proposals`).
    pub kind: String,
    /// The thread's meta: a packet's spec and John's progress live here.
    pub meta: Value,
    /// `(author, body)` of every message with nothing cut out, for the script runner: a packet's steps are cut out of the displayed
    /// first message (the window draws them live), but a command inside a step must still be runnable.
    pub run_msgs: Vec<(String, String)>,
}

impl Detail {
    pub fn load(db: &Db, id: i64) -> Option<Detail> {
        let doc = db.get_thread(id).ok()?;
        let mut thread = ThreadRow::from(&doc["thread"]);
        thread.waiting = doc["thread"]["waiting"].as_i64() == Some(1);
        let tmeta = meta_of(&doc["thread"]["meta"]);
        let tkind = kinds::kind_of(db, id).unwrap_or(kinds::ASK).to_string();
        let messages = doc["messages"]
            .as_array()?
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let meta = meta_of(&m["meta"]);
                let mut body: String = m["body"].as_str().unwrap_or("").into();
                // An older packet's first message still holds the whole packet: the window draws its steps live, so they are cut out.
                if i == 0 && tkind == kinds::PACKET {
                    body = kinds::without_steps(&body);
                }
                let kind = kind_of(&m["meta"]);
                let rev = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    (&body, &kind).hash(&mut h);
                    h.finish()
                };
                Msg { author: m["author"].as_str().unwrap_or("").into(), kind, ts: ts(&m["ts"]), body, via: meta["via"].as_str().unwrap_or("").into(), rev }
            })
            .collect();
        let run_msgs = doc["messages"]
            .as_array()?
            .iter()
            .enumerate()
            .map(|(i, m)| {
                // a packet's commands live in its spec (the first message is only the stub), so they are read from there
                let body =
                    if i == 0 && tkind == kinds::PACKET { kinds::render_packet("", &tmeta["packet"], &tmeta["state"]) } else { m["body"].as_str().unwrap_or("").to_string() };
                (m["author"].as_str().unwrap_or("").to_string(), body)
            })
            .collect();
        Some(Detail { thread, messages, kind: tkind, meta: tmeta, run_msgs })
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
    pub requested_by: String,
    pub thread_id: Option<i64>,
    pub status: String,
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

/// slack_bridge.state: when it last beat, how often it polls, and its last relay (when, which thread).
pub type SlackBeat = (Option<DateTime<Local>>, i64, Option<(Option<DateTime<Local>>, i64)>);

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
    pub slack: Option<SlackBeat>,
    /// Whether the bridge's heartbeat is fresh and its process alive: asked once per refresh, never per frame.
    pub slack_up: bool,
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

thread_local! {
    /// `ui-bench` turns this on to learn what each step of a refresh costs; off (None) it is one thread-local read per step.
    static PROBE: std::cell::RefCell<Option<Vec<(&'static str, f64)>>> = const { std::cell::RefCell::new(None) };
}

/// Runs one step of a refresh and, while a probe is armed, records how long it took.
fn step<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    if PROBE.with(|p| p.borrow().is_none()) {
        return f();
    }
    let t = std::time::Instant::now();
    let r = f();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    PROBE.with(|p| p.borrow_mut().as_mut().map(|v| v.push((name, ms))));
    r
}

/// Starts recording the steps of `State::load` on this thread.
pub fn probe_start() {
    PROBE.with(|p| *p.borrow_mut() = Some(Vec::new()));
}

/// Stops recording and returns every step in the order it ran.
pub fn probe_take() -> Vec<(&'static str, f64)> {
    PROBE.with(|p| p.borrow_mut().take()).unwrap_or_default()
}

fn table_exists(db: &Db, name: &str) -> bool {
    db.scalar_i64("SELECT 1 FROM sqlite_master WHERE type='table' AND name=:n", &[(":n", &name)]).ok().flatten().is_some()
}

impl State {
    fn load_swarm(&mut self, db: &Db, memo: Option<&mut Memo>) {
        let list = step("goals::list", || crate::goals::list(db)).ok().and_then(|v| v["goals"].as_array().cloned()).unwrap_or_default();
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
        if let Ok(c) = step("concierge::state", || crate::concierge::state(db)) {
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
        if let Ok(slots) = step("slots::list", || crate::slots::list(db)) {
            for sl in slots["slots"].as_array().cloned().unwrap_or_default() {
                if let (Some(g), Some(n)) = (sl["goal"].as_str(), sl["n"].as_i64()) {
                    self.slots.insert(g.to_lowercase(), n);
                }
            }
        }
        self.budget = match memo {
            Some(m) if m.budget.as_ref().is_some_and(|(at, _)| at.elapsed() < BUDGET_TTL) => m.budget.as_ref().and_then(|(_, b)| b.clone()),
            memo => {
                let b = step("governor::report", || budget_of(db));
                if let Some(m) = memo {
                    m.budget = Some((Instant::now(), b.clone()));
                }
                b
            }
        };
    }

    /// The window's refresh: only the channel lists `want` names are loaded (the others stay as the window had them), and with a
    /// `memo` the slow-moving budget is reused for `BUDGET_TTL`.
    pub fn load_for(db: &Db, want: &Want, memo: Option<&mut Memo>) -> State {
        let mut st = State::default();
        st.load_rows(db, want);
        st.load_rest(db, memo);
        st
    }

    pub fn load(db: &Db, show_archived: bool) -> State {
        State::load_for(db, &Want { archived: show_archived, rows: [true; 4], read: None, goal: None }, None)
    }

    fn load_rows(&mut self, db: &Db, want: &Want) {
        for (i, ch) in CHANNELS.into_iter().enumerate().filter(|(i, _)| want.rows[*i]) {
            let all: Vec<ThreadRow> =
                step(["list_threads question", "list_threads discussion", "list_threads wiki", "list_threads work"][i], || db.list_threads(Some(ch), None, 300, true, false))
                    .unwrap_or_default()
                    .iter()
                    .map(ThreadRow::from)
                    .collect();
            let mut rows: Vec<ThreadRow> = if ch == "question" { all.into_iter().filter(|r| (r.status == "archived") == want.archived).collect() } else { all };
            if ch == "question" {
                rows.sort_by_key(|r| !(want.archived || r.waiting)); // stable: ringing first
            }
            self.rows.insert(ch.into(), rows);
        }
    }

    fn load_rest(&mut self, db: &Db, memo: Option<&mut Memo>) {
        let st = self;
        st.open_qs = step("open_questions", || db.open_questions(false)).unwrap_or_default().iter().map(|q| ThreadRow::from(&serde_json::json!({"id": q["thread_id"], "subject": q["subject"], "opened_by": q["opened_by"], "created_ts": q["created_ts"], "updated_ts": q["updated_ts"], "channel": "question", "status": "open"}))).collect();
        st.prs = step("pull_requests", || db.rows("SELECT * FROM pull_requests ORDER BY id DESC LIMIT 200", &[]))
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
        st.recent = step("recent posts", || db.rows(&format!("{post_sql} ORDER BY m.id DESC LIMIT 8"), &[])).unwrap_or_default().iter().map(post_from).collect();
        let day_ago = (chrono::Utc::now() - chrono::Duration::hours(24)).format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let callers = step("callers", || {
            db.rows(
                &format!("{post_sql} WHERE m.author <> 'john' AND m.ts >= :d AND m.id = (SELECT MAX(x.id) FROM messages x WHERE x.author = m.author AND x.author_kind='agent') ORDER BY m.id DESC LIMIT 40"),
                &[(":d", &day_ago)],
            )
        })
        .unwrap_or_default();
        st.callers = callers.iter().map(post_from).collect();
        st.bios = step("bios", || db.rows("SELECT id, subject FROM threads WHERE channel='discussion' AND subject LIKE 'bio: %' ORDER BY id DESC", &[]))
            .unwrap_or_default()
            .iter()
            .filter_map(|r| Some((r["subject"].as_str()?.get(5..)?.trim().to_lowercase(), r["id"].as_i64()?)))
            .collect();
        let last_john = step("last john post", || db.row(&format!("{post_sql} WHERE m.author = 'john' ORDER BY m.id DESC LIMIT 1"), &[])).ok().flatten();
        if let Some(j) = &last_john {
            st.john_last = Some(post_from(j));
            let after = step("max john id", || db.scalar_i64("SELECT MAX(id) FROM messages WHERE author='john'", &[])).ok().flatten().unwrap_or(0);
            st.since_posts = step("since posts", || {
                db.scalar_i64(
                    "SELECT COUNT(*) FROM messages WHERE id > :a AND author <> 'john' AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta,'$.kind') END,'') NOT IN ('ack','ack-note','read-receipt')",
                    &[(":a", &after)],
                )
            })
            .ok()
                .flatten()
                .unwrap_or(0);
        }
        st.last_filed = step("last filed", || db.list_threads(Some("question"), Some("archived"), 1, true, false)).ok().and_then(|v| v.first().map(ThreadRow::from));
        let u = step("usage report", usage::report);
        st.usage_lines = u["lines"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        st.usage_summary = u["summary"].as_str().unwrap_or("").into();
        st.vault = step("vault_state.json", || std::fs::read_to_string(paths::data_dir().join("vault_state.json")))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| Some(VaultHealth { ts: ts(&v["ts"])?, unpushed: v["unpushed"].as_i64(), error: v["error"].as_str().filter(|e| !e.is_empty()).map(String::from) }));
        let beat = step("slack_bridge.state", || std::fs::read_to_string(paths::data_dir().join("slack_bridge.state"))).ok();
        st.slack_up = step("slack_up (pid check)", || beat.as_deref().map(super::live::slack_up).unwrap_or(false));
        st.slack = beat
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .map(|v| (ts(&v["ts"]), v["poll_s"].as_i64().unwrap_or(0), v["last_relay"].as_object().map(|r| (ts(&r["ts"].clone()), r["thread_id"].as_i64().unwrap_or(0)))));
        st.jobs = step("jobs", || jobs::list(db))
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
        if let Ok(p) = step("permissions", || permissions::list(db)) {
            st.rules = p["proposals"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| Rule {
                            id: r["id"].as_i64().unwrap_or(0),
                            rule: r["rule"].as_str().unwrap_or("").into(),
                            scope: r["scope"].as_str().unwrap_or("").into(),
                            requested_by: r["requested_by"].as_str().unwrap_or("").into(),
                            thread_id: r["thread_id"].as_i64(),
                            status: r["status"].as_str().unwrap_or("").into(),
                            broad: r["broad"].as_bool().unwrap_or(false),
                        })
                        .collect()
                })
                .unwrap_or_default();
            st.allow = p["allow"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            st.settings_path = p["settings"].as_str().unwrap_or("").into();
        }
        st.has_session_tables = step("table_exists", || table_exists(db, "identities"));
        if st.has_session_tables {
            st.agents = step("identities", || db.rows("SELECT name, folder, host, model, state, generation FROM identities ORDER BY name", &[]))
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
        st.max_sessions = step("max_sessions", crate::identities::max_sessions) as i64;
        if st.has_session_tables {
            step("swarm (goals+concierge+slots+governor)", || st.load_swarm(db, memo));
        }
    }
}

/// What a refresh can reuse from the one before: the usage governor's report is the slowest query on the board (it trains on
/// every usage sample) and moves slowly.
#[derive(Default)]
pub struct Memo {
    budget: Option<(Instant, Option<Budget>)>,
}

const BUDGET_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// What a refresh loads: which of the four channel lists (question, discussion, wiki, work), the open thread and the open goal.
/// Two refreshes that want the same are interchangeable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Want {
    pub archived: bool,
    pub rows: [bool; 4],
    pub read: Option<i64>,
    pub goal: Option<String>,
}

/// One refresh, loaded off any frame: the state, and the open thread and goal the window was showing.
pub struct Fresh {
    pub want: Want,
    pub st: State,
    pub update: Option<String>,
    pub detail: Option<Option<Detail>>,
    pub goal_doc: Option<Option<Value>>,
    /// `signature()`, filled in by the worker (0 on the window's own thread, which has no use for it).
    pub sig: u64,
}

impl Fresh {
    /// `known` is the open thread when the caller has just loaded it (opening a thread loads it to see that it exists).
    pub fn load(db: &Db, want: Want, memo: Option<&mut Memo>, known: Option<Detail>) -> Fresh {
        let st = State::load_for(db, &want, memo);
        let detail = want.read.map(|id| match known {
            Some(d) if d.thread.id == id => Some(d),
            _ => step("thread", || Detail::load(db, id)),
        });
        let goal_doc = want.goal.as_ref().map(|n| step("goal", || crate::goals::status(db, n).ok()));
        Fresh { update: step("update::pending", update_pending), want, st, detail, goal_doc, sig: 0 }
    }
}

/// `Fresh` is built on a worker and handed over: it must stay free of `Rc` and friends.
#[allow(dead_code)]
fn fresh_is_send(f: Fresh) -> impl Send {
    f
}

impl Fresh {
    /// A hash of everything this refresh would put on screen, so an identical one can be dropped without a redraw. Taken on the
    /// worker. The destructuring is exhaustive on purpose: a field added to `State` must be added here or this stops compiling.
    pub fn signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.describe().hash(&mut h);
        h.finish()
    }

    /// Everything on screen, as one string with a fixed order (what `signature` hashes).
    pub fn describe(&self) -> String {
        use std::collections::BTreeMap;
        let State {
            rows,
            open_qs,
            prs,
            recent,
            callers,
            bios,
            john_last,
            since_posts,
            last_filed,
            usage_lines,
            usage_summary,
            vault,
            jobs,
            rules,
            allow,
            settings_path,
            agents,
            has_session_tables,
            goals,
            slots,
            concierge_on,
            swarm,
            held_id,
            budget,
            slack,
            slack_up,
            max_sessions,
        } = &self.st;
        let ordered = |m: &HashMap<String, Vec<ThreadRow>>| format!("{:?}", m.iter().collect::<BTreeMap<_, _>>());
        let detail = self.detail.as_ref().map(|d| d.as_ref().map(|d| (&d.thread, d.messages.iter().map(|m| (m.rev, m.ts, &m.author, &m.via)).collect::<Vec<_>>())));
        format!(
            "{} {open_qs:?} {prs:?} {recent:?} {callers:?} {:?} {john_last:?} {since_posts} {last_filed:?} {usage_lines:?} {usage_summary} {vault:?} {jobs:?} {rules:?} {allow:?} {settings_path} {agents:?} {has_session_tables} {goals:?} {:?} {concierge_on} {swarm:?} {held_id:?} {budget:?} {slack:?} {slack_up} {max_sessions} {:?} {:?} {:?}",
            ordered(rows),
            bios.iter().collect::<BTreeMap<_, _>>(),
            slots.iter().collect::<BTreeMap<_, _>>(),
            self.update,
            detail,
            self.goal_doc,
        )
    }
}

fn update_pending() -> Option<String> {
    crate::update::pending()
}

fn budget_of(db: &Db) -> Option<Budget> {
    let g = crate::governor::report(db, chrono::Utc::now(), false, None);
    if g["samples"].as_i64().unwrap_or(0) <= 0 {
        return None;
    }
    let f = |k: &str| g[k].as_f64().unwrap_or(0.0);
    let arr = |k: &str| g[k].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
    Some(Budget {
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
    })
}
