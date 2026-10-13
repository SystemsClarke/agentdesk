//! Claude plan usage, as the original reports it: `claude_usage.json` in the data folder holds the 5-hour and weekly windows
//! (`five_hour` / `seven_day`: `used` and `resets_at`), when they were read (`captured_ts`) and the monthly spend (`extra`).
//! It is written by the Claude Code status-line feeder (`agentdesk usage-feed`) and by the 5-minute `claude -p /usage` probe
//! (`agentdesk usage-probe`), and read into the lines and summary the window shows.

use crate::{db::Db, paths};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Utc};
use regex::Regex;
use serde_json::{json, Map, Value};

pub fn span(seconds: f64) -> String {
    let t = seconds.max(0.0) as i64;
    let (d, h, m) = (t / 86400, t % 86400 / 3600, t % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

/// A reading's time: a number is epoch seconds (milliseconds when above 1e11), a string is ISO 8601.
fn ts(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => {
            let n = n.as_f64()?;
            DateTime::from_timestamp_millis(if n > 1e11 { n as i64 } else { (n * 1000.0) as i64 })
        }
        Value::String(s) => {
            DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc)).or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok().map(|t| t.and_utc()))
        }
        _ => None,
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn used(w: &Value) -> String {
    match &w["used"] {
        Value::Null => "?".into(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn load() -> Option<Map<String, Value>> {
    serde_json::from_str::<Value>(&std::fs::read_to_string(paths::usage_file()).ok()?).ok()?.as_object().cloned().filter(|o| !o.is_empty())
}

/// What the window shows: `lines` rotate on the main menu prompt, `summary` is SysOp's Claude plan row.
pub fn report_at(d: Option<Map<String, Value>>, now: DateTime<Utc>) -> Value {
    let Some(d) = d else { return json!({"lines": [], "summary": "no feed yet (add `agentdesk usage-feed` to your Claude Code status line)"}) };
    let ins = |r: DateTime<Utc>| span((r - now).num_seconds() as f64);
    let (five, week) = (d.get("five_hour").cloned().unwrap_or(Value::Null), d.get("seven_day").cloned().unwrap_or(Value::Null));
    let captured = d.get("captured_ts").and_then(ts);
    let stale = captured.filter(|c| (now - *c).num_minutes() > 30).map(|c| format!(", as of {} ago", span((now - c).num_seconds() as f64))).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let reset = ts(&five["resets_at"]);
    if reset.map(|r| r <= now).unwrap_or(false) {
        lines.push("time left: fresh 5-hour window, the meter just reset".into());
    } else if five.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        lines.push(format!("time left: {} in your 5-hour window, {}% used{stale}", reset.map(ins).unwrap_or_else(|| "unknown".into()), used(&five)));
    }
    let wreset = ts(&week["resets_at"]);
    if num(&week["used"]).map(|u| u >= 75.0).unwrap_or(false) && !wreset.map(|r| r <= now).unwrap_or(false) {
        lines.push(format!("time left: {} on the week, {}% used, getting close{stale}", wreset.map(ins).unwrap_or_else(|| "unknown".into()), used(&week)));
    }
    let mut spend = String::new();
    if let Some(x) = d.get("extra") {
        if let (Some(spent), Some(limit)) = (num(&x["spent"]), num(&x["limit"])) {
            let pct = if limit != 0.0 { (100.0 * spent / limit).round() as i64 } else { 0 };
            let age =
                ts(&x["captured_ts"]).filter(|a| (now - *a).num_seconds() > 3600).map(|a| format!(", as of {} ago", span((now - a).num_seconds() as f64))).unwrap_or_default();
            spend = format!("monthly spend: ${spent:.2} of ${limit:.0} ({pct}%){}{age}", if pct >= 90 { " · nearly capped" } else { "" });
            lines.push(spend.clone());
        }
    }
    let mut bits: Vec<String> = Vec::new();
    for (win, label) in [(&five, "5h"), (&week, "week")] {
        if win.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            let r = ts(&win["resets_at"]).filter(|r| *r > now).map(|r| format!(", resets in {}", ins(r))).unwrap_or_default();
            bits.push(format!("{label} {}%{r}", used(win)));
        }
    }
    if !spend.is_empty() {
        bits.push(spend.replace("monthly spend: ", "spend "));
    }
    if let Some(c) = captured {
        bits.push(format!("reported {} ago", span((now - c).num_seconds() as f64)));
    }
    json!({"lines": lines, "summary": if bits.is_empty() { "feed has no plan limits in it".to_string() } else { bits.join(" · ") }})
}

pub fn report() -> Value {
    report_at(load(), Utc::now())
}

fn write_feed(d: &Map<String, Value>) {
    let _ = std::fs::create_dir_all(paths::data_dir());
    let file = paths::usage_file();
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(d.clone())).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &file);
    }
    let _ = std::fs::remove_file(&tmp);
}

fn find<'a>(v: &'a Value, names: &[&str]) -> Option<&'a Value> {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                if x.is_object() && names.iter().any(|n| k.to_lowercase().contains(n)) {
                    return Some(x);
                }
            }
            o.values().find_map(|x| find(x, names))
        }
        Value::Array(a) => a.iter().find_map(|x| find(x, names)),
        _ => None,
    }
}

fn window(d: Option<&Value>) -> Option<Value> {
    let d = d?.as_object()?;
    let u = ["used_percentage", "utilization", "percentUsed", "percent_used", "used"].iter().find_map(|k| d.get(*k)).cloned();
    let u = match (u, d.contains_key("utilization")) {
        (Some(v), true) if v.as_f64().map(|f| f > 0.0 && f <= 1.0).unwrap_or(false) => Some(json!((v.as_f64().unwrap() * 100.0).round() as i64)),
        (v, _) => v,
    };
    let reset = ["resets_at", "resetsAt", "reset_at"].iter().find_map(|k| d.get(*k)).cloned();
    if u.is_none() && reset.is_none() {
        return None;
    }
    let used = u.map(|v| v.as_f64().map(|f| json!(f.round() as i64)).unwrap_or(v)).unwrap_or(Value::Null);
    Some(json!({"used": used, "resets_at": reset.unwrap_or(Value::Null)}))
}

/// The Claude Code status-line command: reads the session JSON on stdin, records the plan windows (keeping the last-known
/// spend), prints the prompt.
pub fn feed(input: &str) -> String {
    let data: Value = serde_json::from_str(input).unwrap_or(json!({}));
    let five = window(find(&data, &["five_hour", "5h", "five-hour"]));
    let week = window(find(&data, &["seven_day", "weekly", "7d", "seven-day"]));
    if five.is_some() || week.is_some() {
        let mut out = load().unwrap_or_default();
        out.insert("source".into(), json!("claude code status line"));
        out.insert("captured_ts".into(), json!(Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()));
        out.insert("five_hour".into(), five.unwrap_or(Value::Null));
        out.insert("seven_day".into(), week.unwrap_or(Value::Null));
        write_feed(&out);
    }
    let cwd = data["workspace"]["current_dir"].as_str().or_else(|| data["cwd"].as_str()).unwrap_or("");
    format!("PS {cwd}>")
}

/// "Sep 26, 3:59am (America/New_York)" or "Sep 26, 4am", read as this machine's local time, to ISO UTC.
fn parse_reset(text: &str, now: DateTime<Utc>) -> Value {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let re = Regex::new(r"^\s*(\w{3}) (\d{1,2}), (\d{1,2})(?::(\d{2}))?\s*([ap]m)").unwrap();
    let Some(m) = re.captures(text) else { return Value::Null };
    let Some(month) = MONTHS.iter().position(|x| *x == &m[1]) else { return Value::Null };
    let hour = m[3].parse::<u32>().unwrap_or(0) % 12 + if &m[5] == "pm" { 12 } else { 0 };
    let minute = m.get(4).and_then(|x| x.as_str().parse().ok()).unwrap_or(0);
    let local_now = now.with_timezone(&Local);
    let day = m[2].parse().unwrap_or(1);
    let mk = |year: i32| NaiveDate::from_ymd_opt(year, month as u32 + 1, day).and_then(|d| d.and_hms_opt(hour, minute, 0)).and_then(|t| Local.from_local_datetime(&t).earliest());
    let Some(mut local) = mk(local_now.year()) else { return Value::Null };
    if (local - local_now).num_days() < -30 {
        // "Jan 2" read in late December
        local = mk(local_now.year() + 1).unwrap_or(local);
    }
    json!(local.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
}

/// Folds `claude -p /usage` output into the feed, keeping what else it holds (the spend). False if it named no window.
pub fn apply(output: &str, now: DateTime<Utc>) -> bool {
    let mut d = load().unwrap_or_default();
    let mut found = false;
    for (key, label) in [("five_hour", "Current session"), ("seven_day", "Current week (all models)")] {
        let re = Regex::new(&format!(r"{}: (\d+)% used(?: · resets (.+))?", regex::escape(label))).unwrap();
        if let Some(m) = re.captures(output) {
            found = true;
            d.insert(key.into(), json!({"used": m[1].parse::<i64>().unwrap_or(0), "resets_at": parse_reset(m.get(2).map(|x| x.as_str()).unwrap_or(""), now)}));
        }
    }
    if !found {
        return false;
    }
    d.insert("source".into(), json!("claude -p /usage"));
    d.insert("captured_ts".into(), json!(now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()));
    write_feed(&d);
    true
}

/// The probe: the arguments the original passes (no hooks, no model call, no transcript left behind).
pub const PROBE_ARGS: [&str; 5] = ["-p", "/usage", "--setting-sources", "project", "--no-session-persistence"];

/// Asks the claude CLI for plan usage. True if the feed was updated.
pub fn probe() -> bool {
    let exe = std::env::var("AGENTDESK_CLAUDE").unwrap_or_else(|_| "claude".into());
    let Ok(child) = crate::paths::command(&exe).args(PROBE_ARGS).current_dir(std::env::temp_dir()).stdin(std::process::Stdio::null()).output() else { return false };
    apply(&String::from_utf8_lossy(&child.stdout), Utc::now())
}

/// Records the current reading in `usage_samples` (once per timestamp), with the sessions running now: what the governor learns from.
pub fn sample(db: &Db) {
    crate::governor::record(db);
}
