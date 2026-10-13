//! The supervisor: makes the board's identities real. It owns the session host, so only one process may run it: whichever holds
//! `supervisor.lock` in the data folder (the window's service thread or `agentdesk daemon`; the other one skips this step).
//!
//! Each tick it reconciles what the board *wants* (`identities.state`) with what is running:
//! - after a restart, whatever was `running` is resumed (queued, started one at a time);
//! - a session that ended on its own is marked `stopped` (a reply from John or `identity start` brings it back, conversation intact);
//! - a session whose identity was stopped, forgotten or retired is ended;
//! - a session that passed the torch and finished its turn is replaced by its successor (Phoenix), in the same slot;
//! - queued identities start, oldest first, while fewer than `max_sessions` are running.

use crate::db::{now_iso, Db};
use crate::governor as gov;
use crate::identities as ids;
use crate::paths;
use crate::sessions::Host;
use anyhow::Result;
use serde_json::{json, Value};
use std::fs::File;

const DISPATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

pub struct Supervisor {
    host: Host,
    lock: Option<File>,
    resumed: bool,
    /// The newest human message already looked at: replies after it may need to wake an identity.
    last_human: i64,
    goals: Option<crate::goals::Runner>,
    attach: Option<crate::attach::Server>,
    /// The Slack bridge's keeper thread and its stop flag.
    bridge: Option<(std::sync::Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>)>,
    /// Swarm identities held queued by the pool or the governor, and why (logged once per hold).
    held: std::collections::BTreeMap<String, String>,
    /// Advisory mode: what enforcement would have shed, so each is logged once.
    would_shed: std::collections::BTreeSet<String>,
    warned: Option<String>,
    /// When the dispatcher last ran: it hands out Work to Hire every `DISPATCH_EVERY`, not every tick.
    dispatched: Option<std::time::Instant>,
}

/// Whether some process holds the supervisor lock right now (a CLI command warns when nothing will act on what it wrote).
pub fn running() -> bool {
    let Ok(f) = lock_file() else { return false };
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(_) => true,
    }
}

fn lock_file() -> std::io::Result<File> {
    std::fs::create_dir_all(paths::data_dir())?;
    std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::data_dir().join("supervisor.lock"))
}

fn claude_program() -> String {
    std::env::var("AGENTDESK_CLAUDE").ok().filter(|c| !c.is_empty()).unwrap_or_else(|| "claude".to_string())
}

impl Supervisor {
    pub fn new() -> Supervisor {
        Supervisor {
            host: Host::new(),
            lock: None,
            resumed: false,
            last_human: 0,
            goals: None,
            attach: None,
            bridge: None,
            held: Default::default(),
            would_shed: Default::default(),
            warned: None,
            dispatched: None,
        }
    }

    /// Takes the lock if nobody has it. Cheap to call every tick.
    pub fn acquire(&mut self) -> bool {
        if self.lock.is_some() {
            return true;
        }
        match lock_file() {
            Ok(f) if f.try_lock().is_ok() => {
                self.lock = Some(f);
                match crate::attach::start(self.host.registry()) {
                    Ok(server) => self.attach = Some(server),
                    Err(e) => paths::log("WARN", &format!("attach endpoint did not start: {e}")),
                }
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (flag, registry) = (stop.clone(), self.host.registry());
                self.bridge = Some((stop, std::thread::spawn(move || crate::bridge::run(flag, registry))));
                true
            }
            _ => false,
        }
    }

    /// The session host, for tests that look at what was launched.
    #[cfg(test)]
    pub fn host(&mut self) -> &mut Host {
        &mut self.host
    }

    pub fn shutdown(&mut self) {
        self.host.stop_all();
        self.attach = None; // removes supervisor.addr
        if let Some((stop, handle)) = self.bridge.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = handle.join();
        }
    }

    pub fn tick(&mut self, db: &Db) -> Result<()> {
        if !self.acquire() {
            return Ok(());
        }
        if !self.resumed {
            // A new supervisor knows no sessions: what the board says was running is resumed, and autostart identities start.
            db.exec("UPDATE identities SET state='queued', pid=NULL, updated_ts=:ts WHERE state='running'", &[(":ts", &now_iso())])?;
            db.exec("UPDATE identities SET state='queued', updated_ts=:ts WHERE state='stopped' AND autostart=1", &[(":ts", &now_iso())])?;
            self.last_human = db.scalar_i64("SELECT COALESCE(MAX(id), 0) FROM messages WHERE author_kind='human'", &[])?.unwrap_or(0);
            self.resumed = true;
        }
        self.wake(db)?;
        crate::plain::tick(db, &mut self.host)?;
        for r in ids::list(db)?.iter().filter(|r| r["state"].as_str() == Some("running") && r["wake_prompt"].as_str().is_some_and(|w| !w.is_empty())) {
            let (n, w) = (r["name"].as_str().unwrap_or(""), r["wake_prompt"].as_str().unwrap_or(""));
            if self.host.alive(n) && self.host.type_text(n, &w.replace('\n', " ")).is_ok() {
                db.exec("UPDATE identities SET wake_prompt=NULL WHERE name=:n", &[(":n", &n)])?;
            }
        }
        let rows = ids::list(db)?;
        let name = |r: &Value| r["name"].as_str().unwrap_or("").to_string();
        // A running row with no live process ended on its own; a live process whose row is no longer running is ended.
        for r in rows.iter().filter(|r| r["state"].as_str() == Some("running")) {
            if !self.host.alive(&name(r)) {
                db.exec("UPDATE identities SET state='stopped', pid=NULL, phoenix_msg=NULL, phoenix_go=0, updated_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name(r))])?;
                paths::log("INFO", &format!("identity {} ended", name(r)));
            }
        }
        for live in self.host.names().into_iter().filter(|n| !n.starts_with("plain:")) {
            let wanted = rows.iter().any(|r| name(r).eq_ignore_ascii_case(&live) && r["state"].as_str() == Some("running"));
            if !wanted {
                self.host.stop(&live);
                paths::log("INFO", &format!("identity {live} stopped"));
            }
        }
        for r in rows.iter().filter(|r| r["state"].as_str() == Some("running") && r["phoenix_go"].as_i64() == Some(1) && !r["phoenix_msg"].is_null()) {
            if let Err(e) = self.phoenix(db, r) {
                paths::log("WARN", &format!("phoenix for {} failed: {e}", name(r)));
            }
        }
        let path = db.path.clone();
        self.goals.get_or_insert_with(|| crate::goals::Runner::new(path)).tick(db, &mut self.host)?;
        self.dispatch(db);
        self.pool(db)
    }

    /// The Concierge's dispatcher: while it is on, open Work to Hire items get a worker (see `dispatcher`).
    fn dispatch(&mut self, db: &Db) {
        if self.dispatched.is_some_and(|t| t.elapsed() < DISPATCH_EVERY) {
            return;
        }
        if self.dispatched.is_none() {
            crate::dispatcher::start(db);
        }
        self.dispatched = Some(std::time::Instant::now());
        if let Err(e) = crate::dispatcher::tick(db, None) {
            paths::log("WARN", &format!("dispatcher tick failed: {e}"));
        }
    }

    /// Starts what the session pool allows, then sheds what the usage governor says is too much. Identities of John's own answer only to
    /// `max_sessions`; a goal's lead or member also needs a free slot under the pool's ceiling (the plan, while `governor_enforce` is on),
    /// its goal's fair share and its `max_members`. A held identity stays queued, and the next tick tries again.
    fn pool(&mut self, db: &Db) -> Result<()> {
        let hard = ids::max_sessions() as i64;
        let v = gov::judge(db, chrono::Utc::now(), false);
        let mut queued = ids::list(db)?.into_iter().filter(|r| r["state"].as_str() == Some("queued")).collect::<Vec<_>>();
        // John's own identities before a goal's, then the oldest.
        queued.sort_by_key(|r| (role(db, r["name"].as_str().unwrap_or("")).0.is_some(), r["updated_ts"].as_str().unwrap_or("").to_string()));
        for r in &queued {
            let name = r["name"].as_str().unwrap_or("").to_string();
            let pool = gov::Budget::read(db, hard, &v);
            if pool.running >= hard {
                break;
            }
            let (goal, lead) = role(db, &name);
            let mut model = None;
            if let Some(g) = goal.as_deref() {
                if let Some(why) = pool.can_start(Some(g), lead, pool.ceiling()) {
                    let by_plan = v.enforce() && pool.running >= pool.governed();
                    let hold = if by_plan { format!("the usage governor caps the pool at {}: {}", pool.governed(), v.why) } else { why.clone() };
                    if self.held.insert(name.clone(), hold).is_none() {
                        paths::log(
                            "INFO",
                            &if by_plan {
                                format!("governor: queued {name} ({} running, cap {}): {}", pool.running, pool.governed(), v.why)
                            } else {
                                format!("pool: queued {name}: {why}")
                            },
                        );
                    }
                    continue;
                }
                if !v.enforce() {
                    if let Some(would) = pool.can_start(Some(g), lead, pool.governed()) {
                        let because = if pool.running >= pool.governed() { v.why.clone() } else { would };
                        paths::log("INFO", &format!("governor (advisory): would have queued {name} ({} running, cap {}): {because}", pool.running, pool.governed()));
                    }
                }
                let stored = r["model"].as_str().unwrap_or("sonnet");
                let want = v.model(stored, lead);
                if want != stored {
                    let (advisory, verb) = if v.enforce() { ("", "launching") } else { (" (advisory)", "would have launched") };
                    paths::log("INFO", &format!("governor{advisory}: {verb} {name} at {want} instead of {stored} (step down)"));
                    if v.enforce() {
                        model = Some(want);
                    }
                }
            }
            self.held.remove(&name);
            if let Err(e) = self.launch_at(db, &name, None, model.as_deref()) {
                paths::log("WARN", &format!("identity {name} did not start: {e}"));
                db.exec("UPDATE identities SET state='stopped', pid=NULL, updated_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name)])?;
            }
        }
        self.held.retain(|n, _| ids::get(db, n).ok().flatten().is_some_and(|r| r["state"].as_str() == Some("queued")));
        self.shed(db, hard, &v)
    }

    /// When the plan's total is below the number running, swarm sessions stop (members before leads, the Concierge's members first, then
    /// the longest idle first). A shed identity goes back to queued, so it resumes its conversation when the governor allows. John's own
    /// are never shed, only warned about. Advisory, it only logs what it would have shed. With a stale reading there is nothing to shed on.
    fn shed(&mut self, db: &Db, hard: i64, v: &gov::Verdict) -> Result<()> {
        let pool = gov::Budget::read(db, hard, v);
        let excess = if v.fresh { (pool.running - pool.governed()).max(0) } else { 0 };
        let victims: Vec<String> =
            if excess == 0 { Vec::new() } else { db.rows(SHED_ORDER, &[])?.iter().filter_map(|r| r["name"].as_str().map(String::from)).take(excess as usize).collect() };
        if excess > victims.len() as i64 {
            let johns: Vec<String> = db
                .rows(
                    "SELECT name FROM identities WHERE state='running' AND name NOT IN (SELECT identity FROM goal_members) AND name NOT IN (SELECT lead FROM goals) ORDER BY name",
                    &[],
                )?
                .iter()
                .filter_map(|r| r["name"].as_str().map(String::from))
                .collect();
            let warning = format!(
                "governor: {} sessions running against a cap of {}; John's own identities ({}) are over it and are never stopped automatically",
                pool.running,
                pool.governed(),
                johns.join(", ")
            );
            if self.warned.as_ref() != Some(&warning) {
                paths::log("WARN", &warning);
            }
            self.warned = Some(warning);
        } else {
            self.warned = None;
        }
        if v.enforce() {
            self.would_shed.clear();
            for name in &victims {
                db.exec("UPDATE identities SET state='queued', pid=NULL, updated_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", name)])?;
                self.host.stop(name);
                self.held.insert(name.clone(), v.why.clone());
                paths::log("WARN", &format!("governor: shed {name} ({} running, cap {}): {}", pool.running, pool.governed(), v.why));
            }
        } else {
            for name in victims.iter().filter(|n| self.would_shed.insert((*n).clone())) {
                paths::log("INFO", &format!("governor (advisory): would have shed {name} ({} running, cap {}): {}", pool.running, pool.governed(), v.why));
            }
            self.would_shed.retain(|n| victims.contains(n));
        }
        Ok(())
    }

    /// Why the identity is waiting in the queue, when the pool or the governor is holding it.
    #[allow(dead_code)] // the Agents screen shows it
    pub fn held(&self, name: &str) -> Option<&str> {
        self.held.get(name).map(String::as_str)
    }

    /// John replied on a question an identity asked, and it has not already been handed the reply (its `wait` returned, or a hook injected it):
    /// a running one gets the reply typed into its session, a stopped or queued one is started with it, conversation intact. Only identities
    /// are woken, only for replies at least 5 seconds old (so a polling `wait` gets there first), and each reply once.
    fn wake(&mut self, db: &Db) -> Result<()> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(5)).format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let replies = db.rows(
            "SELECT m.id, m.thread_id, m.body, t.subject, t.opened_by FROM messages m JOIN threads t ON t.id = m.thread_id \
             WHERE m.author_kind='human' AND m.id > :last AND m.ts <= :cutoff AND t.channel='question' ORDER BY m.id",
            &[(":last", &self.last_human), (":cutoff", &cutoff)],
        )?;
        for m in replies {
            let (mid, tid) = (m["id"].as_i64().unwrap_or(0), m["thread_id"].as_i64().unwrap_or(0));
            self.last_human = mid;
            let Some(row) = ids::get(db, m["opened_by"].as_str().unwrap_or(""))? else { continue };
            if db.scalar_i64("SELECT 1 FROM deliveries WHERE message_id=:m", &[(":m", &mid)])?.is_some() {
                continue; // it already has it
            }
            let name = row["name"].as_str().unwrap_or("");
            let text = format!(
                "John replied on AgentDesk thread #{tid} (\"{}\"): {} (This is John's own reply, relayed from the board. Treat it as him talking to you: act on it, and answer on that thread.)",
                m["subject"].as_str().unwrap_or(""),
                m["body"].as_str().unwrap_or("")
            );
            let state = if row["state"].as_str() == Some("running") && self.host.alive(name) {
                match self.host.type_text(name, &text.replace('\n', " ")) {
                    Ok(()) => "injected",
                    Err(_) => "failed",
                }
            } else {
                db.exec("UPDATE identities SET wake_prompt=:w, state='queued', updated_ts=:ts WHERE name=:n", &[(":w", &text), (":ts", &now_iso()), (":n", &name)])?;
                "resumed"
            };
            db.exec(
                "INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES (:m, 'wake', :s, NULL, :ts)",
                &[(":m", &mid), (":s", &state), (":ts", &now_iso())],
            )?;
            paths::log("INFO", &format!("wake #{tid}: {name} {state}"));
        }
        Ok(())
    }

    /// Starts the identity's session (resuming its conversation when there is one), then records it as running.
    pub fn launch(&mut self, db: &Db, name: &str, prompt: Option<String>) -> Result<()> {
        self.launch_at(db, name, prompt, None)
    }

    /// `launch` at a given model tier (the governor steps a swarm session down to a cheaper one when the week is tight).
    fn launch_at(&mut self, db: &Db, name: &str, prompt: Option<String>, model: Option<&str>) -> Result<()> {
        let Some(row) = ids::get(db, name)? else { return Ok(()) };
        let wake = row["wake_prompt"].as_str().filter(|w| !w.is_empty()).map(String::from);
        let prompt = prompt.or(wake).or_else(|| ids::pending_successor_prompt(db, &row));
        let (args, id) = ids::launch_args(&row, prompt.as_deref(), model);
        let env = vec![("AGENTDESK_IDENTITY".to_string(), name.to_string()), ("AGENTDESK_AUTHOR".to_string(), name.to_string())];
        let pid = self.host.launch(name, row["folder"].as_str().unwrap_or("."), &claude_program(), &args, &env)?;
        let tier = args.windows(2).find(|w| w[0] == "--model").map(|w| w[1].clone());
        db.exec(
            "UPDATE identities SET state='running', pid=:p, claude_session_id=:s, running_model=:m, wake_prompt=NULL, updated_ts=:ts WHERE name=:n",
            &[(":p", &i64::from(pid)), (":s", &id), (":m", &tier), (":ts", &now_iso()), (":n", &name)],
        )?;
        Ok(())
    }

    /// Phoenix: record the generation, end the session, and start its successor in the same slot, a new conversation whose first prompt
    /// is the handoff plus the state file. At most one restart per identity per `COOLDOWN_MINUTES`.
    fn phoenix(&mut self, db: &Db, row: &Value) -> Result<()> {
        let name = row["name"].as_str().unwrap_or("");
        let (gen, msg) = (row["generation"].as_i64().unwrap_or(1), row["phoenix_msg"].as_i64().unwrap_or(0));
        db.exec("UPDATE identities SET phoenix_msg=NULL, phoenix_go=0 WHERE name=:n", &[(":n", &name)])?;
        if !ids::cooled_down(db, name) {
            paths::log("WARN", &format!("identity {name} handed off again within {} minutes of its last restart: not restarting it", ids::COOLDOWN_MINUTES));
            return Ok(());
        }
        let Some(m) = db.row("SELECT thread_id, body FROM messages WHERE id=:m", &[(":m", &msg)])? else { return Ok(()) };
        let body = m["body"].as_str().unwrap_or("");
        let handoff = body.split_once("\n\n").map(|(_, rest)| rest).unwrap_or(body).to_string(); // after pass_the_torch's "Handoff recorded" line
        db.exec(
            "INSERT INTO phoenix_chain (identity, generation, claude_session_id, handoff_msg, ts) VALUES (:n,:g,:s,:m,:ts)",
            &[(":n", &name), (":g", &gen), (":s", &row["claude_session_id"].as_str()), (":m", &msg), (":ts", &now_iso())],
        )?;
        // Still running but with no session: the slot is the successor's, and nothing queued takes it.
        db.exec("UPDATE identities SET pid=NULL, claude_session_id=NULL, generation=:g, updated_ts=:ts WHERE name=:n", &[(":g", &(gen + 1)), (":ts", &now_iso()), (":n", &name)])?;
        self.host.stop(name);
        self.launch(db, name, Some(ids::successor_prompt_for(db, name, gen + 1, &handoff)))?;
        db.reply(
            m["thread_id"].as_i64().unwrap_or(0),
            name,
            crate::db::AGENT,
            &format!("generation {} started from handoff #{msg}", gen + 1),
            Some(msg),
            Some(&json!({"kind": "phoenix"})),
        )?;
        paths::log("INFO", &format!("identity {name}: generation {} started from handoff #{msg}", gen + 1));
        Ok(())
    }
}

/// A goal's member or lead is a swarm identity (the goal, and whether it leads it): the governor gates, sheds and re-tiers those. Any
/// other identity is John's own.
fn role(db: &Db, name: &str) -> (Option<String>, bool) {
    if let Ok(Some(g)) = db.scalar_str("SELECT goal FROM goal_members WHERE identity=:n", &[(":n", &name)]) {
        return (Some(g), false);
    }
    match db.scalar_str("SELECT name FROM goals WHERE lead=:n COLLATE NOCASE", &[(":n", &name)]) {
        Ok(Some(g)) => (Some(g), true),
        _ => (None, false),
    }
}

/// Running swarm sessions in the order the governor sheds them. A session mid-Phoenix (no pid) is left alone.
const SHED_ORDER: &str = "SELECT i.name FROM identities i LEFT JOIN presence p ON p.author = i.name \
    WHERE i.state='running' AND i.pid IS NOT NULL \
      AND (i.name IN (SELECT identity FROM goal_members) OR i.name IN (SELECT lead FROM goals)) \
    ORDER BY (i.name IN (SELECT identity FROM goal_members)) DESC, \
             (i.name IN (SELECT identity FROM goal_members WHERE goal='concierge')) DESC, \
             MAX(COALESCE(p.seen_ts, ''), i.updated_ts), i.name";
