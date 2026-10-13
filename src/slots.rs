//! The ten swarm slots (`ui:slot_list`, `ui:slot_assign`, `ui:slot_clear`). The core only stores the mapping; the Slack bridge
//! makes every Slack call.

use crate::db::{err, now_iso, Db};
use anyhow::Result;
use serde_json::{json, Value};

fn ensure(db: &Db) -> Result<()> {
    for n in 1..=10i64 {
        db.exec("INSERT OR IGNORE INTO slots (n, updated_ts) VALUES (:n, :ts)", &[(":n", &n), (":ts", &now_iso())])?;
    }
    Ok(())
}

const JOINED: &str = "SELECT s.n, s.channel_id, s.channel_name, s.persona_name, s.persona_icon, s.goal, s.updated_ts, \
    g.state, g.lead, g.thread_id, g.objective, \
    (SELECT value FROM experiments e WHERE e.goal = g.name AND e.value IS NOT NULL ORDER BY e.n DESC LIMIT 1) AS last_value, \
    (SELECT COUNT(*) FROM goal_members m WHERE m.goal = g.name) AS members \
    FROM slots s LEFT JOIN goals g ON g.name = s.goal";

pub fn list(db: &Db) -> Result<Value> {
    ensure(db)?;
    Ok(json!({"slots": db.rows(&format!("{JOINED} ORDER BY s.n"), &[])?}))
}

fn one(db: &Db, n: i64) -> Result<Value> {
    match db.row(&format!("{JOINED} WHERE s.n=:n"), &[(":n", &n)])? {
        Some(r) => Ok(r),
        None => err(format!("slot is 1 to 10, got {n}")),
    }
}

/// Records what the bridge did for slot `n`; arguments not given keep their value. The goal must exist and be in no other slot.
pub fn assign(db: &Db, n: i64, a: &Value) -> Result<Value> {
    ensure(db)?;
    one(db, n)?;
    let goal = a.get("goal").and_then(|v| v.as_str()).filter(|g| !g.is_empty());
    if let Some(g) = goal {
        if db.row("SELECT name FROM goals WHERE name=:g", &[(":g", &g)])?.is_none() {
            return err(format!("no such goal: {g}"));
        }
        if let Some(other) = db.scalar_i64("SELECT n FROM slots WHERE goal=:g AND n<>:n", &[(":g", &g), (":n", &n)])? {
            return err(format!("{g} is already in slot {other}"));
        }
        db.exec("UPDATE slots SET goal=:g WHERE n=:n", &[(":g", &g), (":n", &n)])?;
    }
    for (key, col) in [("persona", "persona_name"), ("persona_icon", "persona_icon"), ("channel_id", "channel_id"), ("channel_name", "channel_name")] {
        if let Some(v) = a.get(key).and_then(|v| v.as_str()) {
            db.exec(&format!("UPDATE slots SET {col}=:v WHERE n=:n"), &[(":v", &v), (":n", &n)])?;
        }
    }
    db.exec("UPDATE slots SET updated_ts=:ts WHERE n=:n", &[(":ts", &now_iso()), (":n", &n)])?;
    one(db, n)
}

/// Frees the slot: no goal and no persona. Its channel stays for the next swarm to reuse.
pub fn clear(db: &Db, n: i64) -> Result<Value> {
    ensure(db)?;
    one(db, n)?;
    db.exec("UPDATE slots SET goal=NULL, persona_name=NULL, persona_icon=NULL, updated_ts=:ts WHERE n=:n", &[(":ts", &now_iso()), (":n", &n)])?;
    one(db, n)
}
