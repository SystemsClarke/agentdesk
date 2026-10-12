//! The 10 swarm slots (docs/GOAL.md): each a reusable Slack channel with a persona, hosting at most one goal. The board stores the
//! mapping; the Slack bridge (`swarm.rs`) makes every Slack call and writes what it did back here.

use crate::db::{err, now_iso, Db};
use anyhow::Result;
use serde_json::Value;

pub const COUNT: i64 = 10;

const LIST_SQL: &str = "WITH RECURSIVE k(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM k WHERE n < 10) \
    SELECT k.n, s.channel_id, s.channel_name, s.persona_name, s.persona_icon, s.goal, s.updated_ts, g.state, g.lead, g.thread_id, g.objective, \
      (SELECT value FROM experiments e WHERE e.goal=g.name AND e.measured_ts IS NOT NULL ORDER BY e.n DESC LIMIT 1) AS last_value, \
      (SELECT COUNT(*) FROM goal_members m WHERE m.goal=g.name) AS members \
    FROM k LEFT JOIN slots s ON s.n=k.n LEFT JOIN goals g ON g.name=s.goal ORDER BY k.n";

/// All 10 slots, empty ones included, each with its goal's state, lead, thread, last measured value and member count.
pub fn list(db: &Db) -> Result<Vec<Value>> {
    db.rows(LIST_SQL, &[])
}

fn check(n: i64) -> Result<()> {
    if (1..=COUNT).contains(&n) {
        Ok(())
    } else {
        err(format!("a slot is 1 to {COUNT}"))
    }
}

fn row(db: &Db, n: i64) -> Result<Value> {
    Ok(list(db)?.into_iter().find(|r| r["n"].as_i64() == Some(n)).unwrap_or(Value::Null))
}

/// Puts a goal (and its persona) in slot `n`, and records the channel the bridge made or renamed. Fields not given keep their value.
pub fn assign(db: &Db, n: i64, goal: Option<&str>, persona: Option<&str>, icon: Option<&str>, channel_id: Option<&str>, channel_name: Option<&str>) -> Result<Value> {
    check(n)?;
    if let Some(g) = goal {
        if db.scalar_i64("SELECT 1 FROM goals WHERE name=:g", &[(":g", &g)])?.is_none() {
            return err(format!("no such goal: {g}"));
        }
        if let Some(other) = db.scalar_i64("SELECT n FROM slots WHERE goal=:g AND n<>:n", &[(":g", &g), (":n", &n)])? {
            return err(format!("goal {g} is already in slot {other}"));
        }
    }
    db.exec(
        "INSERT INTO slots (n, channel_id, channel_name, persona_name, persona_icon, goal, updated_ts) VALUES (:n,:ci,:cn,:p,:i,:g,:ts) \
         ON CONFLICT(n) DO UPDATE SET channel_id=COALESCE(:ci, channel_id), channel_name=COALESCE(:cn, channel_name), persona_name=COALESCE(:p, persona_name), \
         persona_icon=COALESCE(:i, persona_icon), goal=COALESCE(:g, goal), updated_ts=:ts",
        &[(":n", &n), (":ci", &channel_id), (":cn", &channel_name), (":p", &persona), (":i", &icon), (":g", &goal), (":ts", &now_iso())],
    )?;
    row(db, n)
}

/// Frees slot `n`: no goal, no persona. Its channel stays, for the next swarm to rename and reuse.
pub fn clear(db: &Db, n: i64) -> Result<Value> {
    check(n)?;
    db.exec("UPDATE slots SET goal=NULL, persona_name=NULL, persona_icon=NULL, updated_ts=:ts WHERE n=:n", &[(":n", &n), (":ts", &now_iso())])?;
    row(db, n)
}
