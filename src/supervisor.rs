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
use crate::identities as ids;
use crate::paths;
use crate::sessions::Host;
use anyhow::Result;
use serde_json::{json, Value};
use std::fs::File;

pub struct Supervisor {
    host: Host,
    lock: Option<File>,
    resumed: bool,
    /// The newest human message already looked at: replies after it may need to wake an identity.
    last_human: i64,
    goals: Option<crate::goals::Runner>,
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
        Supervisor { host: Host::new(), lock: None, resumed: false, last_human: 0, goals: None }
    }

    /// Takes the lock if nobody has it. Cheap to call every tick.
    pub fn acquire(&mut self) -> bool {
        if self.lock.is_some() {
            return true;
        }
        match lock_file() {
            Ok(f) if f.try_lock().is_ok() => {
                self.lock = Some(f);
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
        let rows = ids::list(db)?;
        let name = |r: &Value| r["name"].as_str().unwrap_or("").to_string();
        // A running row with no live process ended on its own; a live process whose row is no longer running is ended.
        for r in rows.iter().filter(|r| r["state"].as_str() == Some("running")) {
            if !self.host.alive(&name(r)) {
                db.exec("UPDATE identities SET state='stopped', pid=NULL, phoenix_msg=NULL, phoenix_go=0, updated_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name(r))])?;
                paths::log("INFO", &format!("identity {} ended", name(r)));
            }
        }
        for live in self.host.names() {
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
        let mut running = self.host.names().len();
        let mut queued: Vec<&Value> = rows.iter().filter(|r| r["state"].as_str() == Some("queued")).collect();
        queued.sort_by_key(|r| r["updated_ts"].as_str().unwrap_or("").to_string());
        for r in queued {
            if running >= ids::max_sessions() {
                break;
            }
            if !crate::goals::member_may_start(db, &name(r)) {
                continue; // its goal already runs max_members: it stays queued until one retires
            }
            match self.launch(db, &name(r), None) {
                Ok(()) => running += 1,
                Err(e) => {
                    paths::log("WARN", &format!("identity {} did not start: {e}", name(r)));
                    db.exec("UPDATE identities SET state='stopped', pid=NULL, updated_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name(r))])?;
                }
            }
        }
        Ok(())
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
        let Some(row) = ids::get(db, name)? else { return Ok(()) };
        let wake = row["wake_prompt"].as_str().filter(|w| !w.is_empty()).map(String::from);
        let prompt = prompt.or(wake).or_else(|| ids::pending_successor_prompt(db, &row));
        let (args, id) = ids::launch_args(&row, prompt.as_deref(), None);
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
