//! The Concierge (docs/GOAL.md, docs/DISPATCH.md): the standing goal `concierge` that keeps Work to Hire drained. The core's
//! dispatcher claims each ordinary open item and starts a worker for it, in priority order; the lead, concierge-lead, is woken
//! (`internal:open_triage`) only for items it must split. A worker completes its item with a report on its thread. Off until John turns it
//! on (the window, Slack `concierge on`, the ops console, `agentdesk concierge on`), and that is its approval: no per-item one.

use crate::db::{parse_obj, Db};
use crate::goals::{self, Standing};
use crate::identity::Caller;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashSet;

pub const NAME: &str = "concierge";
pub const LEAD: &str = "concierge-lead";
/// Workers at once while the week is on pace; the dispatcher lifts it while the week is behind.
pub const BASE_MEMBERS: i64 = 3;

const CHARTER: &str = "You are the AgentDesk Concierge's lead. The core's dispatcher already starts a worker for every ordinary open Work to Hire item,
by priority; you only get the ones it cannot route: items marked triage (vague or too large for one worker) and items that
three workers failed. The core wakes you while any wait. For each, oldest first:
1. list_work status=open, read_thread the item, then claim_work it. Never pass `author`: you post as concierge-lead.
2. Split it into small, self-contained items with post_work (priority 0-4, model haiku for mechanical work, `after` for order),
   or if it can be done directly, do it. A decision only John can make goes to ask_human, never a guess.
3. complete_work the original with a note saying what became of it (the new item ids).
Between wakes, stop.";

/// `ui:concierge`. With `on` (John only) it turns the Concierge on (creating or approving the goal) or off (the goal stops: members
/// forgotten, the lead stopped, held items back on the queue). Returns the state either way.
pub fn toggle(db: &Db, c: &Caller, folder: &str, on: Option<bool>) -> Result<Value> {
    if let Some(want) = on {
        if !goals::is_john(c) {
            return crate::db::err("only John turns the Concierge on or off");
        }
        if want {
            goals::ensure(
                db,
                c,
                &Standing {
                    name: NAME,
                    objective: "Keep Work to Hire drained: every open item claimed, worked by a small swarm, and completed with a report on its thread.",
                    folder,
                    charter: CHARTER,
                    model: "opus",
                    hypothesis: "The core dispatches every ordinary open item itself, and the lead splits the rest, so the queue empties.",
                    measure: "internal:open_work", // the dispatcher points it at open_triage once it runs
                    success: "value <= 0",
                    max_members: BASE_MEMBERS,
                    cadence_minutes: 10.0,
                },
            )?;
        } else if running(db)? {
            goals::stop(db, c, NAME)?;
        }
    }
    state(db)
}

fn running(db: &Db) -> Result<bool> {
    Ok(db.scalar_str("SELECT state FROM goals WHERE name=:n", &[(":n", &NAME)])?.as_deref() == Some("running"))
}

/// `ui:status`'s "concierge": on, the goal's state, the lead's, the open count the measure sees, the members, and the work items the
/// Concierge holds (newest first).
pub fn state(db: &Db) -> Result<Value> {
    let g = db.row("SELECT state, thread_id FROM goals WHERE name=:n", &[(":n", &NAME)])?;
    let gs = g.as_ref().and_then(|g| g["state"].as_str()).unwrap_or("off").to_string();
    let members = db.rows("SELECT identity, task, work_id FROM goal_members WHERE goal=:n ORDER BY created_ts", &[(":n", &NAME)])?;
    let mut mine: HashSet<String> = members.iter().filter_map(|m| m["identity"].as_str().map(str::to_lowercase)).collect();
    mine.insert(LEAD.to_string());
    let held: Vec<Value> = db
        .rows("SELECT id, meta FROM threads WHERE channel='work' AND status='claimed' ORDER BY updated_ts DESC", &[])?
        .into_iter()
        .filter(|t| parse_obj(t["meta"].as_str()).and_then(|m| m.get("assignee").and_then(|a| a.as_str().map(str::to_lowercase))).is_some_and(|w| mine.contains(&w)))
        .map(|t| t["id"].clone())
        .collect();
    Ok(json!({
        "on": gs == "running",
        "state": gs,
        "lead": LEAD,
        "lead_state": db.scalar_str("SELECT state FROM identities WHERE name=:n", &[(":n", &LEAD)])?,
        "thread_id": g.as_ref().map(|g| g["thread_id"].clone()),
        "open": goals::measure_internal(db, "internal:open_work")?,
        "members": members,
        "held": held,
    }))
}

/// The folder the Concierge's lead and workers run in: the goal's own once it exists, else `AGENTDESK_CONCIERGE_FOLDER`, else here.
pub fn default_folder(db: &Db) -> String {
    db.scalar_str("SELECT measure_folder FROM goals WHERE name=:n", &[(":n", &NAME)])
        .ok()
        .flatten()
        .or_else(|| std::env::var("AGENTDESK_CONCIERGE_FOLDER").ok().filter(|f| !f.is_empty()))
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default()
}
