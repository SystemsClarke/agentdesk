//! The swarm half of `docs/ui-api.md` as `agentdesk ui <request>` answers it: identities, goals, the Concierge, the governor, swarm
//! slots, plain sessions, adoption, folders and wake. Called from `ui_api::call`.

use crate::db::{err, Db};
use crate::identity::Caller;
use crate::{concierge, folders, goals, governor, identities as ids, slots, wake};
use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

fn text<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn opt_str<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str())
}

fn int(a: &Value, k: &str) -> Option<i64> {
    a.get(k).and_then(|v| v.as_i64())
}

fn num(a: &Value, k: &str) -> Option<f64> {
    a.get(k).and_then(|v| v.as_f64())
}

/// Asks a running supervisor to act on the board now (it would at its next tick anyway); no supervisor is not an error.
fn kick(db: &Db) {
    if crate::supervisor::running() {
        let _ = crate::requests::submit(db, "kick", &json!({}), Duration::from_secs(10));
    }
}

fn session(db: &Db, action: &str, a: &Value) -> Result<Value> {
    crate::requests::submit(db, action, a, Duration::from_secs(15))
}

/// The enforcement bookkeeping the supervisor writes each tick (what it would have queued and shed, and who is held).
fn enforcement() -> Value {
    std::fs::read_to_string(crate::paths::data_dir().join("governor_state.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({"would_queue": 0, "would_shed": 0, "held": []}))
}

pub fn governor_doc(db: &Db) -> Value {
    governor::report(db, chrono::Utc::now(), false, Some(&enforcement()))
}

pub fn call(db: &Db, c: &Caller, name: &str, a: &Value) -> Option<Result<Value>> {
    Some(match name {
        "identity_create" => {
            ids::create(db, text(a, "name"), text(a, "folder"), opt_str(a, "charter").filter(|s| !s.is_empty()), opt_str(a, "model"), a["autostart"].as_bool().unwrap_or(false))
        }
        "identity_list" => ids::list(db).map(|l| json!({"identities": l})),
        "identity_start" => ids::start(db, text(a, "name")).and_then(|_| {
            kick(db);
            ids::get(db, text(a, "name")).map(|r| r.unwrap_or(Value::Null))
        }),
        "identity_stop" => match ids::get(db, text(a, "name")) {
            Ok(Some(_)) => ids::stop(db, text(a, "name")).and_then(|_| {
                kick(db);
                ids::get(db, text(a, "name")).map(|r| r.unwrap_or(Value::Null))
            }),
            Ok(None) => session(db, "stop", a), // a plain session name stops that session
            Err(e) => Err(e),
        },
        "identity_forget" => ids::forget(db, text(a, "name")),
        "adoptable" => Ok(folders::adoptable()),
        "adopt" => folders::adopt(db, text(a, "session_id"), text(a, "name")),
        "folders" => folders::ranked(db),
        "wake" => Ok(wake::doc(db, int(a, "thread_id").unwrap_or(0))),

        "goal_create" => goals::create(db, text(a, "name"), text(a, "objective"), text(a, "folder")),
        "goal_propose" => goals::propose(db, c, text(a, "name"), text(a, "hypothesis"), text(a, "measure_cmd"), text(a, "success"), int(a, "samples").unwrap_or(1)),
        "goal_approve" => goals::approve(db, c, text(a, "name"), int(a, "max_members"), num(a, "max_hours"), num(a, "cadence_minutes")),
        "goal_budget" => goals::set_budget(db, c, text(a, "name"), int(a, "max_members"), num(a, "max_hours"), num(a, "cadence_minutes")),
        "goal_stop" => goals::stop(db, c, text(a, "name")),
        "goal_list" => goals::list(db),
        "goal_status" => goals::status(db, text(a, "name")),

        "concierge" => concierge::toggle(db, c, &concierge::default_folder(db), a["on"].as_bool()),
        "governor" => Ok(governor_doc(db)),
        "governor_enforce" => {
            if !goals::is_john(c) {
                return Some(err("only John turns the governor's enforcement on or off"));
            }
            governor::set_enforce(a["on"].as_bool().unwrap_or(false)).map(|_| governor_doc(db))
        }

        "slot_list" => slots::list(db).map(|s| json!({"slots": s})),
        "slot_assign" => {
            slots::assign(db, int(a, "n").unwrap_or(0), opt_str(a, "goal"), opt_str(a, "persona"), opt_str(a, "persona_icon"), opt_str(a, "channel_id"), opt_str(a, "channel_name"))
        }
        "slot_clear" => slots::clear(db, int(a, "n").unwrap_or(0)),

        "session_start" => session(db, "start", a),
        "session_list" => session(db, "list", a),
        "session_stop" => session(db, "stop", a),
        "attach" => session(db, "attach", a),
        "input" => session(db, "input", a),
        "resize" => session(db, "resize", a),
        _ => return None,
    })
}

/// Extra sections of `ui:status` that come from the swarm: the Concierge, the session pool, the governor, goals and the Slack bridge.
pub fn status_extra(db: &Db) -> Value {
    let gov = governor_doc(db);
    json!({
        "concierge": concierge::state(db).unwrap_or(Value::Null),
        "sessions": gov["pool"].clone(),
        "governor": gov,
        "goals": goals::list(db).map(|g| g["goals"].clone()).unwrap_or_else(|_| json!([])),
        "bridge": bridge(),
    })
}

/// The Slack bridge's own state: whether it is switched on and whether its heartbeat is fresh.
fn bridge() -> Value {
    let settings: Value = std::fs::read_to_string(crate::paths::data_dir().join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    let enabled = settings["slack_bridge"] != json!(false);
    let beat: Option<Value> = std::fs::read_to_string(crate::paths::data_dir().join("slack_bridge.state")).ok().and_then(|t| serde_json::from_str(&t).ok());
    let age = beat
        .as_ref()
        .and_then(|b| b["ts"].as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds());
    let state = match (enabled, age) {
        (false, _) => "off",
        (true, Some(a)) if a < 90 => "running",
        (true, Some(_)) => "stale",
        (true, None) => "stopped",
    };
    json!({"enabled": enabled, "state": state, "pid": beat.as_ref().and_then(|b| b["pid"].as_i64()), "age_s": age})
}
