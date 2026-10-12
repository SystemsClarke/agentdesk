//! The Claude plan meter: `claude_usage.json` (written by the Claude Code status line or `agentdesk usage-feed`)
//! turned into the lines the window shows. Each reading is also recorded in `usage_samples`.

use crate::{db::Db, paths};
use serde_json::{json, Value};

fn pct(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim_end_matches('%').parse().ok()))
}

/// `{"lines": [...], "summary": "..."}`; empty lines when there is no feed or it is stale.
pub fn report() -> Value {
    let Ok(text) = std::fs::read_to_string(paths::usage_file()) else { return json!({"lines": [], "summary": "no usage feed"}) };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return json!({"lines": [], "summary": "usage feed unreadable"}) };
    let age = std::fs::metadata(paths::usage_file()).ok().and_then(|m| m.modified().ok()).and_then(|m| m.elapsed().ok()).map(|e| e.as_secs() / 60);
    let five = pct(&v["five_hour"]["used_percentage"]).or_else(|| pct(&v["five_hour_pct"])).or_else(|| pct(&v["session"]["used_percentage"]));
    let week = pct(&v["seven_day"]["used_percentage"]).or_else(|| pct(&v["week_pct"])).or_else(|| pct(&v["weekly"]["used_percentage"]));
    let mut lines = Vec::new();
    if let Some(f) = five {
        lines.push(format!("5-hour window: {f:.0}% used"));
    }
    if let Some(w) = week {
        lines.push(format!("Week: {w:.0}% used, {:.0}% left", 100.0 - w));
    }
    let stale = age.map(|a| a > 30).unwrap_or(false);
    let summary = match (five, week) {
        (None, None) => "no usage numbers".to_string(),
        _ => format!("{}{}", lines.join(" | "), if stale { " (stale)" } else { "" }),
    };
    json!({"lines": lines, "summary": summary, "week_pct": week, "five_hour_pct": five})
}

/// Records the current reading in `usage_samples` (once per timestamp), with the sessions running now: what the governor learns from.
pub fn sample(db: &Db) {
    crate::governor::record(db);
}

/// Claude Code status-line feed: reads the status-line JSON on stdin, writes `claude_usage.json`, prints a short line.
pub fn feed(input: &str) -> String {
    let v: Value = serde_json::from_str(input).unwrap_or(json!({}));
    let rl = &v["rate_limits"];
    if rl.is_object() {
        let out = json!({"five_hour": rl["five_hour"], "seven_day": rl["seven_day"], "ts": crate::db::now_iso()});
        let _ = std::fs::create_dir_all(paths::data_dir());
        let _ = std::fs::write(paths::usage_file(), out.to_string());
    }
    let model = v["model"]["display_name"].as_str().unwrap_or("claude");
    let r = report();
    format!("{model} | {}", r["summary"].as_str().unwrap_or(""))
}
