//! Recurring jobs: a prompt that runs itself on a schedule (every morning at 08:00, weekdays, every two hours).
//! A run is a fresh headless `claude -p` session of the job's identity: it does the work and exits. The
//! scheduler knows nothing about what a job does: the prompt is opaque. A run due while the last is still going
//! is skipped, not stacked; a run missed while nothing was ticking fires once if it is under `catch_up_minutes`
//! late, else it is skipped and the status says so.

use crate::db::{err, now_iso, Db};
use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Utc, Weekday};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::process::{Child, Stdio};
use std::sync::{Mutex, OnceLock};

const SUFFIX: &str =
    "\n\nThis is a scheduled run of the recurring job `{name}`: nobody is at the keyboard. Do the work above, post what you found or changed to the board as usual, then stop.";
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn running() -> &'static Mutex<HashMap<String, Child>> {
    static R: OnceLock<Mutex<HashMap<String, Child>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

fn parse_at(at: &str) -> Result<NaiveTime> {
    let t = at.trim();
    let ok = t.split_once(':').and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?, m.len() == 2)));
    match ok {
        Some((h, m, true)) if h < 24 && m < 60 => Ok(NaiveTime::from_hms_opt(h, m, 0).unwrap()),
        _ => err(format!("at must be HH:mm (24-hour, local time), got {at}")),
    }
}

fn parse_days(days: &str) -> Result<HashSet<u32>> {
    let d = days.trim().to_lowercase();
    let set: HashSet<u32> = match d.as_str() {
        "" | "daily" | "everyday" => (0..7).collect(),
        "weekdays" => (1..=5).collect(),
        "weekends" => [0, 6].into_iter().collect(),
        list => {
            let mut s = HashSet::new();
            for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let key: String = part.chars().take(3).collect();
                match DAYS.iter().position(|d| *d == key) {
                    Some(i) => {
                        s.insert(i as u32);
                    }
                    None => return err(format!("unknown day: {part} (use daily, weekdays, weekends or mon,tue,...)")),
                }
            }
            s
        }
    };
    if set.is_empty() {
        return err("days matches no day");
    }
    Ok(set)
}

/// The first run strictly after `after`.
pub fn next(after: DateTime<Utc>, at: Option<&str>, days: &str, every_minutes: Option<f64>) -> Result<DateTime<Utc>> {
    if let Some(m) = every_minutes {
        return Ok(after + Duration::seconds((m * 60.0) as i64));
    }
    let Some(at) = at else { return err("give either at (HH:mm) or every_minutes") };
    let time = parse_at(at)?;
    let allowed = parse_days(days)?;
    let local = after.with_timezone(&Local);
    for d in 0..=8 {
        let date = local.date_naive() + Duration::days(d);
        let dow = match date.weekday() {
            Weekday::Sun => 0,
            Weekday::Mon => 1,
            Weekday::Tue => 2,
            Weekday::Wed => 3,
            Weekday::Thu => 4,
            Weekday::Fri => 5,
            Weekday::Sat => 6,
        };
        if !allowed.contains(&dow) {
            continue;
        }
        let wall = date.and_time(time);
        // the spring-forward gap has no such local time: run just after it
        let when = Local.from_local_datetime(&wall).earliest().or_else(|| Local.from_local_datetime(&(wall + Duration::hours(1))).earliest());
        if let Some(w) = when {
            let w = w.with_timezone(&Utc);
            if w > after {
                return Ok(w);
            }
        }
    }
    err(format!("days matches no day: {days}"))
}

fn need(db: &Db, name: &str) -> Result<Value> {
    match db.row("SELECT * FROM recurring_jobs WHERE name=:n", &[(":n", &name)])? {
        Some(j) => Ok(j),
        None => err(format!("no such job: {name}")),
    }
}

pub struct NewJob<'a> {
    pub name: &'a str,
    pub folder: &'a str,
    pub prompt: &'a str,
    pub at: Option<&'a str>,
    pub days: Option<&'a str>,
    pub every_minutes: Option<f64>,
    pub model: Option<&'a str>,
    pub catch_up_minutes: Option<f64>,
}

pub fn create(db: &Db, j: NewJob) -> Result<Value> {
    if j.name.is_empty() || !j.name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return err("name is required: letters, digits, - _ .");
    }
    if j.prompt.trim().is_empty() {
        return err("prompt is required");
    }
    if !std::path::Path::new(j.folder).is_dir() {
        return err(format!("no such folder: {}", j.folder));
    }
    if j.at.is_some() == j.every_minutes.is_some() {
        return err("give exactly one of at (HH:mm local time) or every_minutes");
    }
    if j.every_minutes.map(|m| m < 1.0).unwrap_or(false) {
        return err("every_minutes must be at least 1");
    }
    let model = j.model.map(|m| m.trim().to_lowercase()).filter(|m| !m.is_empty()).unwrap_or_else(|| "sonnet".into());
    if !["haiku", "sonnet", "opus"].contains(&model.as_str()) {
        return err(format!("model must be haiku, sonnet or opus, got {model}"));
    }
    let days = j.days.map(|d| d.trim().to_lowercase()).filter(|d| !d.is_empty()).unwrap_or_else(|| "daily".into());
    let nxt = next(Utc::now(), j.at, &days, j.every_minutes)?; // validates at and days
    if db.scalar_i64("SELECT 1 FROM recurring_jobs WHERE name=:n", &[(":n", &j.name)])?.is_some() {
        return err(format!("job already exists: {}", j.name));
    }
    db.exec(
        "INSERT INTO recurring_jobs (name, folder, prompt, model, at, days, every_minutes, catch_up_minutes, next_run_ts, created_ts) VALUES (:n,:f,:p,:m,:a,:d,:e,:c,:x,:ts)",
        &[
            (":n", &j.name),
            (":f", &j.folder),
            (":p", &j.prompt),
            (":m", &model),
            (":a", &j.at),
            (":d", &days),
            (":e", &j.every_minutes),
            (":c", &j.catch_up_minutes.unwrap_or(240.0)),
            (":x", &iso(nxt)),
            (":ts", &now_iso()),
        ],
    )?;
    need(db, j.name)
}

pub fn list(db: &Db) -> Result<Vec<Value>> {
    db.rows("SELECT * FROM recurring_jobs ORDER BY name", &[])
}

pub fn enable(db: &Db, name: &str, on: bool) -> Result<Value> {
    let job = need(db, name)?;
    // turning it on schedules from now: it does not "catch up" the days it was off
    let nxt = if on { Some(iso(next(Utc::now(), job["at"].as_str(), job["days"].as_str().unwrap_or("daily"), job["every_minutes"].as_f64())?)) } else { None };
    db.exec("UPDATE recurring_jobs SET enabled=:e, next_run_ts=:x WHERE name=:n", &[(":e", &(on as i64)), (":x", &nxt), (":n", &name)])?;
    need(db, name)
}

pub fn delete(db: &Db, name: &str) -> Result<Value> {
    need(db, name)?;
    db.exec("DELETE FROM recurring_jobs WHERE name=:n", &[(":n", &name)])?;
    Ok(json!({"deleted": name}))
}

fn set_status(db: &Db, name: &str, status: &str) {
    let _ = db.exec("UPDATE recurring_jobs SET last_status=:s WHERE name=:n", &[(":s", &status), (":n", &name)]);
}

/// Fires every enabled job that is due and moves each to its next run. Safe to call from several processes:
/// the one whose conditional UPDATE moves `next_run_ts` owns that run.
pub fn tick(db: &Db) -> Result<Vec<String>> {
    let now = Utc::now();
    let mut fire = Vec::new();
    for job in db.rows("SELECT * FROM recurring_jobs WHERE enabled=1 AND next_run_ts IS NOT NULL AND next_run_ts <= :now", &[(":now", &iso(now))])? {
        let name = job["name"].as_str().unwrap_or("").to_string();
        let due_s = job["next_run_ts"].as_str().unwrap_or("").to_string();
        let Ok(due) = DateTime::parse_from_rfc3339(&due_s) else { continue };
        let late = (now - due.with_timezone(&Utc)).num_minutes() as f64;
        let nxt = next(now, job["at"].as_str(), job["days"].as_str().unwrap_or("daily"), job["every_minutes"].as_f64())?;
        let claimed = db.exec("UPDATE recurring_jobs SET next_run_ts=:x WHERE name=:n AND next_run_ts=:old", &[(":x", &iso(nxt)), (":n", &name), (":old", &due_s)])? == 1;
        if !claimed {
            continue;
        }
        if late > job["catch_up_minutes"].as_f64().unwrap_or(240.0) {
            set_status(db, &name, &format!("missed: due {due_s}, {late:.0} min late (nothing was ticking?); not run"));
        } else {
            fire.push(name);
        }
    }
    for n in &fire {
        fire_job(db, n, "schedule");
    }
    Ok(fire)
}

/// Runs a job now, off schedule (the next scheduled run is unchanged).
pub fn run_now(db: &Db, name: &str) -> Result<Value> {
    need(db, name)?;
    fire_job(db, name, "manual");
    need(db, name)
}

fn fire_job(db: &Db, name: &str, why: &str) {
    let Ok(job) = need(db, name) else { return };
    {
        let mut map = running().lock().unwrap();
        if let Some(child) = map.get_mut(name) {
            if matches!(child.try_wait(), Ok(None)) {
                set_status(db, name, &format!("skipped ({why}): previous run still going"));
                return;
            }
            map.remove(name);
        }
    }
    match spawn(&job) {
        Ok(child) => {
            running().lock().unwrap().insert(name.to_string(), child);
            let _ = db.exec("UPDATE recurring_jobs SET last_run_ts=:ts, runs=runs+1 WHERE name=:n", &[(":ts", &now_iso()), (":n", &name)]);
            set_status(db, name, &format!("started ({why})"));
            crate::paths::log("INFO", &format!("job {name}: started ({why})"));
            let name = name.to_string();
            std::thread::spawn(move || watch(&name));
        }
        Err(e) => {
            set_status(db, name, &format!("failed to start ({why}): {e}"));
            crate::paths::log("WARN", &format!("job {name}: failed to start: {e}"));
        }
    }
}

fn spawn(job: &Value) -> Result<Child> {
    let name = job["name"].as_str().unwrap_or("");
    let prompt = format!("{}{}", job["prompt"].as_str().unwrap_or(""), SUFFIX.replace("{name}", name));
    let dir = crate::paths::data_dir().join("jobs");
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{name}.log")))?;
    let mut cmd = crate::paths::command(&crate::paths::claude_program());
    cmd.arg("-p").arg(&prompt).arg("--model").arg(job["model"].as_str().unwrap_or("sonnet"));
    cmd.current_dir(job["folder"].as_str().unwrap_or("."));
    for (k, _) in std::env::vars() {
        if k.starts_with("CLAUDE") || k == "AGENTDESK_SESSION" {
            cmd.env_remove(k);
        }
    }
    cmd.env("AGENTDESK_AUTHOR", name).env("AGENTDESK_IDENTITY", name);
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    Ok(cmd.spawn()?)
}

/// Waits for the run to end and records how it went.
fn watch(name: &str) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let status = {
            let mut map = running().lock().unwrap();
            match map.get_mut(name).map(|c| c.try_wait()) {
                Some(Ok(Some(st))) => {
                    map.remove(name);
                    Some(if st.success() { "finished ok".to_string() } else { format!("finished with {st}") })
                }
                Some(Ok(None)) => None,
                _ => return,
            }
        };
        if let Some(s) = status {
            if let Ok(db) = Db::open(&crate::paths::db_path()) {
                set_status(&db, name, &s);
            }
            return;
        }
    }
}

pub fn is_running(name: &str) -> bool {
    running().lock().unwrap().get_mut(name).map(|c| matches!(c.try_wait(), Ok(None))).unwrap_or(false)
}
