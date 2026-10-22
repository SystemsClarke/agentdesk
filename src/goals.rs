//! Goals (see the original's `docs/GOAL.md`): a hypothesis, a measure the daemon runs itself, a success line, a budget and an experiment
//! log. The lead identity (`<goal>-lead`) proposes the test and John approves it. While it runs, the daemon wakes the lead every cadence
//! and after each measure, and the lead spawns member identities (`<goal>-<name>`) within `max_members`. The loop ends when a measure
//! crosses the success line, when the budget's hours are spent, or when John stops it. A standing goal never ends that way: at its
//! success line it idles, and the lead is woken only while the measure is off the line.
//!
//! Like identities, everything an agent or John does is a write to the board; the supervisor (`supervisor.rs`) runs the timers, the
//! measurements (on a thread each, so a ten-minute build never stalls a tick) and the wakes. Behaviour follows the .NET `Goals.cs`.

use crate::db::{err, now_iso, Db, AGENT};
use crate::identities as ids;
use crate::identity::Caller;
use crate::paths;
use crate::sessions::Host;
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const AUTHOR: &str = "agentdesk";
const LEAD_CHARTER: &str = "You lead an AgentDesk goal: a hypothesis tested by a measure that the core runs itself. You think and dispatch;
members do the work. Keep each member's task small (one experiment) and pick the cheapest capable model
(haiku for mechanical work, sonnet by default, opus only for hard reasoning).
- member_spawn {goal, name, task, model} starts a member; past the goal's max_members (or its share of the session pool) it queues until a slot frees. It retires with member_done.
- experiment_start {goal, change} before a change, experiment_done {goal, n} once it is in place: the core runs the
  measure, records the value and posts the verdict on the goal thread. Never report a measured value yourself.
- The core wakes you with the goal's status every cadence and after every measure. Between wakes, stop.";

/// A measure that takes longer than this is killed and counts as an error.
const MEASURE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// `experiment_done` waits this long for the measure to finish before saying it is still measuring.
const INLINE_WAIT: Duration = Duration::from_secs(20);

/// Measures answered from the board itself, with no shell: `measure_cmd` = `internal:<name>`.
fn internal_sql(cmd: &str) -> Option<&'static str> {
    match cmd.trim().to_lowercase().as_str() {
        // Work to Hire items waiting for a dispatcher: open, and not reserved for a deliberate claim_work (claim=anyone).
        "internal:open_work" => Some(
            "SELECT COUNT(*) FROM threads WHERE channel='work' AND status='open' \
             AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta, '$.claim') END, 'auto') <> 'anyone'",
        ),
        // The ones the Dispatcher leaves to the Concierge's lead: marked triage (vague or large), or that failed three times.
        "internal:open_triage" => Some(
            "SELECT COUNT(*) FROM threads WHERE channel='work' AND status='open' AND json_valid(meta) AND json_extract(meta, '$.triage') = 1 \
             AND COALESCE(json_extract(meta, '$.claim'), 'auto') <> 'anyone'",
        ),
        _ => None,
    }
}

pub fn is_internal(cmd: Option<&str>) -> bool {
    cmd.map(|c| internal_sql(c).is_some()).unwrap_or(false)
}

pub fn measure_internal(db: &Db, cmd: &str) -> Result<f64> {
    match internal_sql(cmd) {
        Some(sql) => Ok(db.scalar_i64(sql, &[])?.unwrap_or(0) as f64),
        None => err(format!("no such internal measure: {cmd}")),
    }
}

// ---- who is calling

/// The identity this process is a session of (`AGENTDESK_IDENTITY`, stamped by the supervisor), if any.
pub fn identity_of(c: &Caller) -> Option<String> {
    // Inside an identity's session the supervisor stamps AGENTDESK_IDENTITY and AGENTDESK_AUTHOR with its name.
    std::env::var("AGENTDESK_IDENTITY").ok().filter(|n| !n.is_empty()).and(c.env_author.clone())
}

/// John: the window, or the CLI in his own terminal. Never an agent's session.
pub fn is_john(c: &Caller) -> bool {
    identity_of(c).is_none() && c.session_id.is_none() && c.harness.as_deref() != Some("claude-code")
}

// ---- success lines

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub op: String,
    pub target: f64,
}

impl Line {
    pub fn met(&self, v: f64) -> bool {
        match self.op.as_str() {
            "<" => v < self.target,
            "<=" => v <= self.target,
            ">" => v > self.target,
            ">=" => v >= self.target,
            _ => v == self.target,
        }
    }
}

pub fn parse_line(s: &str) -> Result<Line> {
    if s.trim().eq_ignore_ascii_case("pass") {
        return Ok(Line { op: "pass".into(), target: 1.0 });
    }
    let re = Regex::new(r"(?i)^\s*value\s*(<=|>=|==|<|>)\s*(-?\d+(?:\.\d+)?)\s*$").unwrap();
    match re.captures(s) {
        Some(m) => Ok(Line { op: m[1].to_string(), target: m[2].parse().unwrap_or(0.0) }),
        None => err("success is 'value < N', 'value <= N', 'value > N', 'value >= N', 'value == N' or 'pass'"),
    }
}

/// What John typed as a goal name, made into one: punctuation becomes dashes, a leading digit gets `g-`, cut to 32.
pub fn slug(name: &str) -> String {
    let s = Regex::new(r"[^A-Za-z0-9]+").unwrap().replace_all(name, "-").trim_matches('-').to_string();
    let s = if s.chars().next().is_some_and(|c| c.is_ascii_digit()) { format!("g-{s}") } else { s };
    if s.chars().count() > 32 {
        s.chars().take(32).collect::<String>().trim_end_matches('-').to_string()
    } else {
        s
    }
}

fn name_ok(name: &str) -> bool {
    Regex::new(r"^[A-Za-z][A-Za-z0-9-]{0,31}$").unwrap().is_match(name)
}

// ---- rows

fn get(db: &Db, name: &str) -> Result<Option<Value>> {
    db.row("SELECT * FROM goals WHERE name = :n", &[(":n", &name)])
}

fn need(db: &Db, name: &str) -> Result<Value> {
    match get(db, name)? {
        Some(g) => Ok(g),
        None => err(format!("no such goal: {name}")),
    }
}

fn st<'a>(g: &'a Value, k: &str) -> &'a str {
    g[k].as_str().unwrap_or("")
}

fn running(db: &Db, name: &str) -> Result<Value> {
    let g = need(db, name)?;
    if st(&g, "state") != "running" {
        return err(format!("goal {name} is {}, not running", st(&g, "state")));
    }
    Ok(g)
}

fn standing(g: &Value) -> bool {
    g["standing"].as_i64().unwrap_or(0) != 0
}

/// The caller must be the goal's lead, or John.
fn lead_only(c: &Caller, g: &Value) -> Result<()> {
    if is_john(c) || identity_of(c).is_some_and(|i| i.eq_ignore_ascii_case(st(g, "lead"))) {
        Ok(())
    } else {
        err(format!("only {} or John may do that", st(g, "lead")))
    }
}

/// The caller must be the goal's lead or one of its members, or John; returns the owner name.
fn member_of(db: &Db, c: &Caller, g: &Value) -> Result<String> {
    if is_john(c) {
        return Ok(crate::db::JOHN.to_string());
    }
    let name = st(g, "name");
    if let Some(id) = identity_of(c) {
        if id.eq_ignore_ascii_case(st(g, "lead")) || db.scalar_i64("SELECT 1 FROM goal_members WHERE identity=:i AND goal=:g", &[(":i", &id), (":g", &name)])?.is_some() {
            return Ok(id);
        }
    }
    err(format!("only goal {name}'s lead and members may run its experiments"))
}

fn thread_of(g: &Value) -> i64 {
    g["thread_id"].as_i64().unwrap_or(0)
}

// ---- John's side

pub fn create(db: &Db, name: &str, objective: &str, folder: &str) -> Result<Value> {
    let name = slug(name);
    if !name_ok(&name) {
        return err("a goal name is letters, digits and dashes (at most 32), starting with a letter");
    }
    if objective.trim().is_empty() {
        return err("objective is required");
    }
    if !std::path::Path::new(folder).is_dir() {
        return err(format!("no such folder: {folder}"));
    }
    let lead = format!("{name}-lead");
    if get(db, &name)?.is_some() {
        return err(format!("goal already exists: {name}"));
    }
    ids::create(db, &lead, folder, Some(LEAD_CHARTER), Some("opus"), false)?;
    let _ = ids::trust_folder(folder); // the user named this folder: its lead must not stop at the trust prompt
    let thread = db.start_thread(
        "discussion",
        &format!("goal: {name}"),
        AUTHOR,
        AGENT,
        &format!(
            "**Goal {name}** (draft): {objective}\n\nLead: {lead}, in `{folder}`. It proposes a hypothesis, a measure and a success line here; John (or another agent, with `goal_approve`) approves with `agentdesk goal approve {name}`."
        ),
        None,
        None,
    )?;
    let ts = now_iso();
    db.exec(
        "INSERT INTO goals (name, objective, measure_folder, lead, thread_id, created_ts, updated_ts) VALUES (:n,:o,:f,:l,:t,:ts,:ts)",
        &[(":n", &name), (":o", &objective), (":f", &folder), (":l", &lead), (":t", &thread), (":ts", &ts)],
    )?;
    ids::start_with_prompt(
        db,
        &lead,
        &format!(
            "You lead goal {name}. Objective: {objective}\n\nPropose how to test it: call goal_propose with name={name}, a hypothesis, a measure_cmd (the core runs it in {folder}; \
             the metric is the last number it prints, or exit code 0 means pass) and a success line (value < N, value <= N, value > N, value >= N, or pass). \
             Then stop: John or another agent approves it, and the core wakes you when the goal runs."
        ),
    )?;
    status(db, &name)
}

pub fn set_budget(db: &Db, c: &Caller, name: &str, max_members: Option<i64>, max_hours: Option<f64>, cadence: Option<f64>) -> Result<Value> {
    if !is_john(c) {
        return err("only John changes a goal's budget");
    }
    let g = need(db, name)?;
    if matches!(st(&g, "state"), "succeeded" | "stopped") {
        return err(format!("goal {name} is {}", st(&g, "state")));
    }
    db.exec(
        "UPDATE goals SET max_members=COALESCE(:m, max_members), max_hours=COALESCE(:h, max_hours), cadence_minutes=COALESCE(:c, cadence_minutes), updated_ts=:ts WHERE name=:n",
        &[(":m", &max_members.map(|m| m.max(1))), (":h", &max_hours), (":c", &cadence), (":ts", &now_iso()), (":n", &name)],
    )?;
    let g = need(db, name)?;
    db.reply(
        thread_of(&g),
        AUTHOR,
        AGENT,
        &format!("**Budget changed by John.** Now {} members, {} h; the lead wakes every {} min.", num(&g["max_members"]), num(&g["max_hours"]), num(&g["cadence_minutes"])),
        None,
        None,
    )?;
    Ok(g)
}

/// What an agent may grant when it approves a goal (John may grant anything): a goal an agent starts is a bounded one.
pub const AGENT_MAX_MEMBERS: i64 = 4;
pub const AGENT_MAX_HOURS: f64 = 24.0;

/// John, or a named agent other than the goal's own lead and members (a goal does not approve itself). Returns who is approving.
fn approver(db: &Db, c: &Caller, g: &Value, max_members: Option<i64>, max_hours: Option<f64>) -> Result<String> {
    if is_john(c) {
        return Ok("John".to_string());
    }
    let Some(me) = identity_of(c) else { return err("only John or an agent running as an identity approves a goal") };
    if me.eq_ignore_ascii_case(st(g, "lead"))
        || db.scalar_i64("SELECT COUNT(*) FROM goal_members WHERE identity=:i AND goal=:g", &[(":i", &me), (":g", &st(g, "name"))])?.unwrap_or(0) > 0
    {
        return err("a goal's own lead and members cannot approve it: another agent, or John, does");
    }
    if max_members.is_some_and(|m| m > AGENT_MAX_MEMBERS) || max_hours.is_some_and(|h| h > AGENT_MAX_HOURS) {
        return err(format!("an agent approves at most {AGENT_MAX_MEMBERS} members and {AGENT_MAX_HOURS} h; John grants more"));
    }
    Ok(me)
}

pub fn approve(db: &Db, c: &Caller, name: &str, max_members: Option<i64>, max_hours: Option<f64>, cadence: Option<f64>) -> Result<Value> {
    let g = need(db, name)?;
    let by = approver(db, c, &g, max_members, max_hours)?;
    if g["measure_cmd"].is_null() {
        return err(format!("goal {name} has no proposal yet (goal_propose)"));
    }
    if st(&g, "state") == "running" && (max_members.is_some() || max_hours.is_some() || cadence.is_some()) {
        return set_budget(db, c, name, max_members, max_hours, cadence); // John only
    }
    if matches!(st(&g, "state"), "running" | "succeeded") {
        return err(format!("goal {name} is already {}", st(&g, "state")));
    }
    let ts = now_iso();
    db.exec(
        "UPDATE goals SET state='running', started_ts=:ts, woke_ts=NULL, updated_ts=:ts, max_members=COALESCE(:m, max_members), max_hours=COALESCE(:h, max_hours), \
         cadence_minutes=COALESCE(:c, cadence_minutes) WHERE name=:n",
        &[(":ts", &ts), (":m", &max_members), (":h", &max_hours), (":c", &cadence), (":n", &name)],
    )?;
    let g = need(db, name)?;
    db.reply(
        thread_of(&g),
        AUTHOR,
        AGENT,
        &format!("**Approved by {by}.** Budget: {} members, {} h; the lead wakes every {} min.", num(&g["max_members"]), num(&g["max_hours"]), num(&g["cadence_minutes"])),
        None,
        None,
    )?;
    Ok(g) // the supervisor's next tick wakes the lead
}

pub fn stop(db: &Db, c: &Caller, name: &str) -> Result<Value> {
    let g = need(db, name)?;
    lead_only(c, &g)?;
    let by = if is_john(c) { "stopped by John".to_string() } else { format!("stopped by {}", identity_of(c).unwrap_or_default()) };
    end(db, &g, "stopped", &by)?;
    need(db, name)
}

/// A goal that is on until John turns it off (the Concierge): its test is fixed by the core, not proposed.
pub struct Standing<'a> {
    pub name: &'a str,
    pub objective: &'a str,
    pub folder: &'a str,
    pub charter: &'a str,
    pub model: &'a str,
    pub hypothesis: &'a str,
    pub measure: &'a str,
    pub success: &'a str,
    pub max_members: i64,
    pub cadence_minutes: f64,
}

/// John turning a standing goal on is its approval: it is created running (with its lead identity) the first time, and started again if it
/// was stopped. Nothing is launched here: the next tick measures, and wakes the lead only if there is work.
pub fn ensure(db: &Db, c: &Caller, s: &Standing) -> Result<Value> {
    if !is_john(c) {
        return err("only John turns a standing goal on");
    }
    parse_line(s.success)?;
    let lead = format!("{}-lead", s.name);
    let state = get(db, s.name)?.map(|g| st(&g, "state").to_string());
    if state.is_none() && ids::get(db, &lead)?.is_none() {
        ids::create(db, &lead, s.folder, Some(s.charter), Some(s.model), false)?;
    }
    let ts = now_iso();
    match state.as_deref() {
        None => {
            let thread = db.start_thread(
                "discussion",
                &format!("goal: {}", s.name),
                AUTHOR,
                AGENT,
                &format!(
                    "**Goal {}** (standing, turned on by John): {}

Lead: {lead}. Measure: `{}`, on its line when {}; there it idles and the core keeps re-checking, so it never ends by itself.",
                    s.name, s.objective, s.measure, s.success
                ),
                None,
                None,
            )?;
            db.exec(
                "INSERT INTO goals (name, objective, hypothesis, measure_cmd, measure_folder, success, max_members, cadence_minutes, state, lead, thread_id, started_ts, created_ts, updated_ts, standing)                  VALUES (:n,:o,:h,:m,:f,:s,:mm,:c,'running',:l,:t,:ts,:ts,:ts,1)",
                &[
                    (":n", &s.name),
                    (":o", &s.objective),
                    (":h", &s.hypothesis),
                    (":m", &s.measure),
                    (":f", &s.folder),
                    (":s", &s.success),
                    (":mm", &s.max_members),
                    (":c", &s.cadence_minutes),
                    (":l", &lead),
                    (":t", &thread),
                    (":ts", &ts),
                ],
            )?;
        }
        Some("running") => {}
        Some(_) => {
            db.exec("UPDATE goals SET state='running', standing=1, started_ts=:ts, woke_ts=NULL, updated_ts=:ts WHERE name=:n", &[(":ts", &ts), (":n", &s.name)])?;
            let g = need(db, s.name)?;
            db.reply(thread_of(&g), AUTHOR, AGENT, "**Turned on by John.**", None, None)?;
        }
    }
    status(db, s.name)
}

const LIST_SQL: &str = "SELECT g.name, g.state, g.objective, g.lead, g.thread_id, g.success, \
  (SELECT COUNT(*) FROM experiments e WHERE e.goal=g.name) AS experiments, \
  (SELECT value FROM experiments e WHERE e.goal=g.name AND e.measured_ts IS NOT NULL ORDER BY e.n DESC LIMIT 1) AS last_value, \
  (SELECT COUNT(*) FROM goal_members m WHERE m.goal=g.name) AS members, g.max_members, g.standing \
  FROM goals g ORDER BY g.updated_ts DESC";

pub fn list(db: &Db) -> Result<Value> {
    Ok(json!({"goals": db.rows(LIST_SQL, &[])?}))
}

/// The goal, its experiments, the metric history (measured values, oldest first), its members, and the summary text agents get.
pub fn status(db: &Db, name: &str) -> Result<Value> {
    let mut g = need(db, name)?;
    g["experiments"] = json!(db.rows("SELECT * FROM experiments WHERE goal=:g ORDER BY n", &[(":g", &name)])?);
    g["history"] =
        json!(db.rows("SELECT value FROM experiments WHERE goal=:g AND value IS NOT NULL ORDER BY n", &[(":g", &name)])?.iter().map(|r| r["value"].clone()).collect::<Vec<_>>());
    g["members"] = json!(db.rows("SELECT identity, task, created_ts, work_id FROM goal_members WHERE goal=:g ORDER BY created_ts", &[(":g", &name)])?);
    g["summary"] = json!(summary(db, name)?);
    Ok(g)
}

// ---- the lead's side

pub fn propose(db: &Db, c: &Caller, name: &str, hypothesis: &str, measure_cmd: &str, success: &str, samples: i64) -> Result<Value> {
    let g = need(db, name)?;
    lead_only(c, &g)?;
    if st(&g, "state") != "draft" {
        return err(format!("goal {name} is {}: proposals are for drafts", st(&g, "state")));
    }
    parse_line(success)?;
    if hypothesis.trim().is_empty() || measure_cmd.trim().is_empty() {
        return err("hypothesis and measure_cmd are required");
    }
    if measure_cmd.trim().to_lowercase().starts_with("internal:") {
        measure_internal(db, measure_cmd)?;
    }
    db.exec(
        "UPDATE goals SET hypothesis=:h, measure_cmd=:m, success=:s, samples=:k, updated_ts=:ts WHERE name=:n",
        &[(":h", &hypothesis), (":m", &measure_cmd), (":s", &success.trim()), (":k", &samples.clamp(1, 9)), (":ts", &now_iso()), (":n", &name)],
    )?;
    db.reply(
        thread_of(&g),
        st(&g, "lead"),
        AGENT,
        &format!(
            "**Proposal.** Hypothesis: {hypothesis}\n\nMeasure: `{measure_cmd}` in `{}`, success when {}.\n\nWaiting for John: `agentdesk goal approve {name}`.",
            st(&g, "measure_folder"),
            success.trim()
        ),
        None,
        None,
    )?;
    need(db, name)
}

pub fn experiment_start(db: &Db, c: &Caller, goal: &str, change: &str) -> Result<Value> {
    let g = running(db, goal)?;
    let owner = member_of(db, c, &g)?;
    let n = db.scalar_i64("SELECT COALESCE(MAX(n),0)+1 FROM experiments WHERE goal=:g", &[(":g", &goal)])?.unwrap_or(1);
    db.exec(
        "INSERT INTO experiments (goal, n, change, owner, started_ts) VALUES (:g,:n,:c,:o,:ts)",
        &[(":g", &goal), (":n", &n), (":c", &change), (":o", &owner), (":ts", &now_iso())],
    )?;
    Ok(json!({"goal": goal, "n": n, "note": "Make the change, then call experiment_done with this n: the core runs the measure."}))
}

/// `experiment_done`: asks the supervisor to measure, then waits up to `INLINE_WAIT` for the recorded result.
pub fn experiment_done(db: &Db, c: &Caller, goal: &str, n: i64) -> Result<Value> {
    experiment_done_wait(db, c, goal, n, INLINE_WAIT)
}

pub fn experiment_done_wait(db: &Db, c: &Caller, goal: &str, n: i64, wait: Duration) -> Result<Value> {
    let g = running(db, goal)?;
    member_of(db, c, &g)?;
    let Some(e) = db.row("SELECT measured_ts FROM experiments WHERE goal=:g AND n=:n", &[(":g", &goal), (":n", &n)])? else {
        return err(format!("no experiment {n} on goal {goal}"));
    };
    if !e["measured_ts"].is_null() {
        return err(format!("experiment {n} was already measured"));
    }
    db.exec("UPDATE experiments SET measuring=1 WHERE goal=:g AND n=:n", &[(":g", &goal), (":n", &n)])?;
    let start = Instant::now();
    while start.elapsed() < wait {
        if let Some(done) = db.row("SELECT * FROM experiments WHERE goal=:g AND n=:n AND measured_ts IS NOT NULL", &[(":g", &goal), (":n", &n)])? {
            return Ok(done);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(json!({"goal": goal, "n": n, "measuring": true, "note": "The verdict is posted on the goal thread and the lead is woken with it."}))
}

/// Starts member `<goal>-<name>`. With `work_id`, the lead's claim on that Work to Hire item is handed to the member, which completes it
/// (`complete_work`) before it retires.
pub fn spawn(db: &Db, c: &Caller, goal: &str, name: &str, task: &str, model: Option<&str>, work_id: Option<i64>) -> Result<Value> {
    lead_only(c, &running(db, goal)?)?;
    spawn_as_lead(db, goal, name, task, model, work_id)
}

/// `spawn` for the core itself: the Dispatcher starts workers on the goal lead's behalf, with no session of the lead's to check.
pub fn spawn_as_lead(db: &Db, goal: &str, name: &str, task: &str, model: Option<&str>, work_id: Option<i64>) -> Result<Value> {
    let model = model.map(|m| m.trim().to_lowercase()).filter(|m| !m.is_empty()).unwrap_or_else(|| "sonnet".to_string());
    if !ids::MODELS.contains(&model.as_str()) {
        return err("model is haiku, sonnet or opus");
    }
    if !name_ok(name) {
        return err("a member name is letters, digits and dashes");
    }
    let g = running(db, goal)?;
    let id = format!("{goal}-{name}");
    if db.scalar_i64("SELECT 1 FROM goal_members WHERE identity=:i", &[(":i", &id)])?.is_some() {
        return err(format!("{id} is already a member"));
    }
    let lead = st(&g, "lead").to_string();
    if let Some(w) = work_id {
        if !db.hand_over_task(w, &lead, &id)? {
            return err(format!("work item #{w} is not claimed by {lead}: claim_work it first"));
        }
    }
    db.exec(
        "INSERT INTO goal_members (identity, goal, task, created_ts, work_id) VALUES (:i,:g,:t,:ts,:w)",
        &[(":i", &id), (":g", &goal), (":t", &task), (":ts", &now_iso()), (":w", &work_id)],
    )?;
    let how = match work_id {
        Some(item) => format!(
            "Your task is Work to Hire item #{item}, and its claim is yours: read_thread {item} for the whole request. When the work is done,\n\
             call complete_work thread_id={item} with your report (what you did, what you found, anything left for John); it is posted on the item's thread.\n\
             Then call member_done with a one-line summary; that retires you. If you cannot finish it, say why in member_done: the item goes back to {lead}."
        ),
        None => format!(
            "Call experiment_start {goal}, change before you change anything, and experiment_done with its n once the change is in place:\n\
             the core runs the measure and posts the verdict on thread #{}. Never report a measured value yourself.\n\
             When the task is done, call member_done with a short summary; that retires you.",
            thread_of(&g)
        ),
    };
    let charter = format!(
        "You are a member of the AgentDesk goal {goal}, dispatched by its lead {lead}. Objective: {}\nHypothesis: {}\nYour task: {task}\n{how}",
        st(&g, "objective"),
        g["hypothesis"].as_str().unwrap_or("null")
    );
    let made = ids::create(db, &id, st(&g, "measure_folder"), Some(&charter), Some(&model), false)
        .and_then(|_| ids::start_with_prompt(db, &id, &format!("Your task for goal {goal}: {task}")));
    match made {
        Ok(row) => Ok(row), // the session pool queues it past max_members or its share
        Err(e) => {
            db.exec("DELETE FROM goal_members WHERE identity=:i", &[(":i", &id)])?;
            if let Some(w) = work_id {
                db.hand_over_task(w, &id, &lead)?;
            }
            Err(e)
        }
    }
}

pub fn is_member(db: &Db, identity: &str) -> bool {
    db.scalar_i64("SELECT 1 FROM goal_members WHERE identity=:i", &[(":i", &identity)]).ok().flatten().is_some()
}

/// A member retires: its summary goes on the goal thread, then its identity is forgotten and the lead is woken.
pub fn member_done(db: &Db, c: &Caller, summary: &str) -> Result<Value> {
    let Some(id) = identity_of(c) else { return err("member_done is for goal members") };
    let Some(m) = db.row("SELECT goal, work_id FROM goal_members WHERE identity=:i", &[(":i", &id)])? else {
        return err(format!("{id} is not a goal member"));
    };
    let goal = st(&m, "goal").to_string();
    let g = need(db, &goal)?;
    // A work item it did not complete goes back to the lead, which decides what happens to it.
    let back = match m["work_id"].as_i64() {
        Some(w) if db.hand_over_task(w, &id, st(&g, "lead"))? => format!("\n\nWork item #{w} was not completed: it is back with {}.", st(&g, "lead")),
        _ => String::new(),
    };
    db.reply(thread_of(&g), &id, AGENT, &format!("**Done.** {summary}{back}"), None, None)?;
    db.exec("DELETE FROM goal_members WHERE identity=:i", &[(":i", &id)])?;
    ids::forget(db, &id)?; // the supervisor ends the live session whose row is gone
    db.exec("UPDATE goals SET woke_ts=NULL WHERE name=:n", &[(":n", &goal)])?; // wake the lead on the next tick
    Ok(json!({"retired": id}))
}

/// For a Phoenix successor's first prompt: the status of the goal its identity leads or works on.
pub fn context(db: &Db, identity: &str) -> Option<String> {
    let goal = db.scalar_str("SELECT name FROM goals WHERE lead=:i UNION SELECT goal FROM goal_members WHERE identity=:i LIMIT 1", &[(":i", &identity)]).ok().flatten()?;
    Some(format!("Your goal's status now, from the core:\n{}", summary(db, &goal).ok()?))
}

/// What an agent needs about the goal: hypothesis, success line, budget, the last 5 experiments and the metric trend.
pub fn summary(db: &Db, name: &str) -> Result<String> {
    let g = need(db, name)?;
    let members: Vec<String> = db.rows("SELECT identity FROM goal_members WHERE goal=:g", &[(":g", &name)])?.iter().map(|r| st(r, "identity").to_string()).collect();
    let last = db.rows("SELECT * FROM (SELECT * FROM experiments WHERE goal=:g ORDER BY n DESC LIMIT 5) ORDER BY n", &[(":g", &name)])?;
    let trend: Vec<String> = db.rows("SELECT value FROM experiments WHERE goal=:g AND value IS NOT NULL ORDER BY n", &[(":g", &name)])?.iter().map(|r| num(&r["value"])).collect();
    let cmd = g["measure_cmd"].as_str();
    let now = match cmd {
        Some(c) if is_internal(Some(c)) => format!("; now {}", num(&json!(measure_internal(db, c)?))),
        _ => String::new(),
    };
    let shown = |k: &str, dflt: &str| g[k].as_str().map(String::from).unwrap_or_else(|| dflt.to_string());
    let exps = if last.is_empty() {
        "none".to_string()
    } else {
        last.iter()
            .map(|e| {
                format!(
                    "#{} by {}, {}: {} ({})",
                    e["n"],
                    e["owner"].as_str().unwrap_or("null"),
                    st(e, "change"),
                    if e["value"].is_null() { "unmeasured".to_string() } else { num(&e["value"]) },
                    e["verdict"].as_str().unwrap_or("open")
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    Ok(format!(
        "Goal {name} ({}{}): {}\nHypothesis: {}\nSuccess: {}, measured by `{}` in {} ({} run(s), median{now})\nBudget: {} of {} members ({}), {} h; wakes every {} min. Thread #{}.\nLast experiments: {exps}\nMetric trend: {}",
        st(&g, "state"),
        if standing(&g) { ", standing: at its line it idles, and it never ends by itself" } else { "" },
        st(&g, "objective"),
        shown("hypothesis", "(none yet)"),
        shown("success", "(none yet)"),
        cmd.unwrap_or(""),
        st(&g, "measure_folder"),
        g["samples"],
        members.len(),
        g["max_members"],
        members.join(", "),
        num(&g["max_hours"]),
        num(&g["cadence_minutes"]),
        thread_of(&g),
        if trend.is_empty() { "no measurements yet".to_string() } else { trend.join(" -> ") }
    ))
}

/// A number as the original printed it: integers without a decimal point.
fn num(v: &Value) -> String {
    match v.as_f64() {
        Some(f) if f == f.trunc() && f.abs() < 1e15 => format!("{}", f as i64),
        Some(f) => format!("{f}"),
        None => v.to_string(),
    }
}

/// Marks a goal ended, posts it on the goal thread, forgets its members and stops its lead.
fn end(db: &Db, g: &Value, state: &str, note: &str) -> Result<()> {
    let name = st(g, "name");
    if db.exec("UPDATE goals SET state=:s, updated_ts=:ts WHERE name=:n AND state IN ('draft','running')", &[(":s", &state), (":ts", &now_iso()), (":n", &name)])? == 0 {
        return Ok(());
    }
    db.reply(thread_of(g), AUTHOR, AGENT, &format!("**Goal {state}**: {note}.\n\n{}", summary(db, name)?), None, None)?;
    let members: Vec<String> = db.rows("SELECT identity FROM goal_members WHERE goal=:g", &[(":g", &name)])?.iter().map(|r| st(r, "identity").to_string()).collect();
    db.exec("DELETE FROM goal_members WHERE goal=:g", &[(":g", &name)])?;
    // Work to Hire items its lead or members still hold go back on the queue, rather than staying claimed by nobody running.
    let mut holders: HashSet<String> = members.iter().map(|m| m.to_lowercase()).collect();
    holders.insert(st(g, "lead").to_lowercase());
    let mut reopened = vec![];
    for t in db.rows("SELECT id, meta FROM threads WHERE channel='work' AND status='claimed'", &[])? {
        let who = crate::db::parse_obj(t["meta"].as_str()).and_then(|m| m.get("assignee").and_then(|a| a.as_str().map(|s| s.to_lowercase())));
        if who.is_some_and(|w| holders.contains(&w)) {
            let id = t["id"].as_i64().unwrap_or(0);
            if db.reopen_task(id)? {
                reopened.push(format!("#{id}"));
            }
        }
    }
    if !reopened.is_empty() {
        db.reply(thread_of(g), AUTHOR, AGENT, &format!("Back on Work to Hire, open: {}.", reopened.join(", ")), None, None)?;
    }
    for m in members {
        ids::forget(db, &m)?;
    }
    ids::stop(db, st(g, "lead"))?;
    Ok(())
}

// ---- measuring

/// One measurement: the last number the command prints in the goal's folder (a non-zero exit is an error), or for a `pass` line, exit 0 as 1
/// and anything else as 0.
pub fn run_once(cmd: &str, folder: &str, pass: bool, timeout: Duration) -> (Option<f64>, Option<String>) {
    use std::io::Read;
    use std::process::Stdio;
    #[cfg(windows)]
    let mut c = {
        use std::os::windows::process::CommandExt;
        let mut c = crate::paths::new_command("cmd.exe");
        c.raw_arg(format!("/d /s /c \"{cmd}\""));
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = crate::paths::new_command("sh");
        c.args(["-c", cmd]);
        c
    };
    c.current_dir(folder).stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return (None, Some(format!("could not run it: {e}"))),
    };
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return (None, Some(format!("timed out after {} s", timeout.as_secs())));
            }
            Err(e) => return (None, Some(e.to_string())),
        }
    };
    let stdout = reader.join().unwrap_or_default();
    if pass {
        return (Some(if status.success() { 1.0 } else { 0.0 }), None);
    }
    if !status.success() {
        return (None, Some(format!("exit code {}", status.code().unwrap_or(-1))));
    }
    match Regex::new(r"-?\d+(?:\.\d+)?").unwrap().find_iter(&stdout).last() {
        Some(m) => (m.as_str().parse().ok(), None),
        None => (None, Some("no number on stdout".into())),
    }
}

fn sample(db: &Db, cmd: &str, folder: &str, pass: bool, timeout: Duration) -> (Option<f64>, Option<String>) {
    if !is_internal(Some(cmd)) {
        return run_once(cmd, folder, pass, timeout);
    }
    match measure_internal(db, cmd) {
        Ok(v) => (Some(if pass { f64::from(v != 0.0) } else { v }), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

/// Runs the measure for experiment `n` and records the value and the verdict (`met`, `improved`, `no gain` or an error), posts it on the goal
/// thread, and either ends the goal (met) or asks for the lead to be woken.
pub fn measure(db: &Db, goal: &str, n: i64, timeout: Duration) -> Result<Value> {
    let g = need(db, goal)?;
    let line = parse_line(st(&g, "success"))?;
    let mut values = vec![];
    let mut error = None;
    for _ in 0..g["samples"].as_i64().unwrap_or(1).max(1) {
        match sample(db, st(&g, "measure_cmd"), st(&g, "measure_folder"), line.op == "pass", timeout) {
            (Some(v), _) => values.push(v),
            (None, e) => {
                error = e;
                break;
            }
        }
        if error.is_some() {
            break;
        }
    }
    let value = if error.is_none() {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        values.get(values.len() / 2).copied()
    } else {
        None
    };
    let prior: Vec<f64> = db.rows("SELECT value FROM experiments WHERE goal=:g AND value IS NOT NULL", &[(":g", &goal)])?.iter().filter_map(|r| r["value"].as_f64()).collect();
    let verdict = match value {
        None => format!("error: {}", error.unwrap_or_default()),
        Some(v) if line.met(v) => "met".to_string(),
        Some(v) => {
            let better = !prior.is_empty()
                && match line.op.chars().next() {
                    Some('<') => prior.iter().cloned().fold(f64::INFINITY, f64::min) > v,
                    Some('>') => prior.iter().cloned().fold(f64::NEG_INFINITY, f64::max) < v,
                    _ => false,
                };
            if better { "improved" } else { "no gain" }.to_string()
        }
    };
    db.exec(
        "UPDATE experiments SET measured_ts=:ts, value=:v, verdict=:d, measuring=0 WHERE goal=:g AND n=:n",
        &[(":ts", &now_iso()), (":v", &value), (":d", &verdict), (":g", &goal), (":n", &n)],
    )?;
    let e = db.row("SELECT * FROM experiments WHERE goal=:g AND n=:n", &[(":g", &goal), (":n", &n)])?.unwrap_or(Value::Null);
    db.reply(
        thread_of(&g),
        AUTHOR,
        AGENT,
        &format!(
            "Experiment #{n} by {} ({}): **{}**, {verdict}.",
            e["owner"].as_str().unwrap_or("null"),
            st(&e, "change"),
            value.map(|v| num(&json!(v))).unwrap_or_else(|| "no value".into())
        ),
        None,
        None,
    )?;
    if verdict == "met" && !standing(&g) {
        end(db, &need(db, goal)?, "succeeded", &format!("experiment #{n} measured {} ({})", num(&json!(value)), st(&g, "success")))?;
    } else if verdict != "met" {
        db.exec("UPDATE goals SET woke_ts=NULL WHERE name=:n", &[(":n", &goal)])?;
        // wake the lead with the result on the next tick
    }
    Ok(e)
}

// ---- the supervisor's goal work

/// The goal timers: runs requested measurements (a thread each), ends goals whose hours are spent, and wakes leads. Owned by the supervisor.
pub struct Runner {
    db_path: PathBuf,
    inflight: Arc<Mutex<HashSet<String>>>,
    /// The value a standing goal last woke its lead for; cleared when it is back on its success line.
    woke_for: HashMap<String, f64>,
    pub timeout: Duration,
}

impl Runner {
    pub fn new(db_path: PathBuf) -> Runner {
        Runner { db_path, inflight: Arc::new(Mutex::new(HashSet::new())), woke_for: HashMap::new(), timeout: MEASURE_TIMEOUT }
    }

    pub fn tick(&mut self, db: &Db, host: &mut Host) -> Result<()> {
        self.start_measures(db)?;
        let now = chrono::Utc::now();
        let mut due = vec![];
        for g in db.rows("SELECT * FROM goals WHERE state='running'", &[])? {
            let name = st(&g, "name").to_string();
            let minutes = g["cadence_minutes"].as_f64().unwrap_or(30.0);
            let cadence_due = match g["woke_ts"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
                None => true,
                Some(w) => (now - w.with_timezone(&chrono::Utc)).num_seconds() as f64 / 60.0 >= minutes,
            };
            if standing(&g) {
                if cadence_due || is_internal(g["measure_cmd"].as_str()) {
                    self.recheck(db, host, &g, cadence_due)?; // an internal measure costs nothing: every tick
                }
            } else if g["started_ts"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_some_and(|s| (now - s.with_timezone(&chrono::Utc)).num_seconds() as f64 / 3600.0 >= g["max_hours"].as_f64().unwrap_or(24.0))
            {
                end(db, &g, "exhausted", &format!("budget spent: {} h", num(&g["max_hours"])))?;
            } else if cadence_due {
                due.push(name);
            }
        }
        for name in due {
            wake(db, host, &name)?;
        }
        Ok(())
    }

    /// Experiments whose `experiment_done` asked for a measurement: one thread per goal at a time.
    fn start_measures(&mut self, db: &Db) -> Result<()> {
        for e in db.rows("SELECT goal, n FROM experiments WHERE measuring=1 AND measured_ts IS NULL ORDER BY goal, n", &[])? {
            let (goal, n) = (st(&e, "goal").to_string(), e["n"].as_i64().unwrap_or(0));
            if !self.inflight.lock().map(|mut s| s.insert(goal.clone())).unwrap_or(false) {
                continue;
            }
            let (path, inflight, timeout) = (self.db_path.clone(), self.inflight.clone(), self.timeout);
            std::thread::spawn(move || {
                if let Ok(db) = Db::open(&path) {
                    if let Err(e) = measure(&db, &goal, n, timeout) {
                        paths::log("WARN", &format!("goals: measuring {goal} #{n}: {e}"));
                        let _ = db.exec("UPDATE experiments SET measuring=0 WHERE goal=:g AND n=:n", &[(":g", &goal), (":n", &n)]);
                    }
                }
                if let Ok(mut s) = inflight.lock() {
                    s.remove(&goal);
                }
            });
        }
        Ok(())
    }

    /// A standing goal's tick: on its line it idles and the lead is not woken; off it, the lead is woken when the value changed since its last
    /// wake, or at cadence.
    fn recheck(&mut self, db: &Db, host: &mut Host, g: &Value, cadence_due: bool) -> Result<()> {
        let name = st(g, "name").to_string();
        let line = parse_line(st(g, "success"))?;
        let (value, error) = sample(db, st(g, "measure_cmd"), st(g, "measure_folder"), line.op == "pass", self.timeout);
        if let Some(v) = value.filter(|v| line.met(*v)) {
            let _ = v;
            self.woke_for.remove(&name);
            if cadence_due {
                db.exec("UPDATE goals SET woke_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name)])?;
            }
            return Ok(());
        }
        if let Some(e) = error {
            paths::log("WARN", &format!("standing goal {name}: {e}"));
        }
        if !cadence_due && value.is_some() && self.woke_for.get(&name) == value.as_ref() {
            return Ok(());
        }
        if let Some(v) = value {
            self.woke_for.insert(name.clone(), v);
        }
        wake(db, host, &name)
    }
}

/// Types the goal's status into the lead's session and presses Enter, or starts the lead with it as its prompt.
pub fn wake(db: &Db, host: &mut Host, name: &str) -> Result<()> {
    let Some(g) = get(db, name)? else { return Ok(()) };
    if st(&g, "state") != "running" {
        return Ok(());
    }
    db.exec("UPDATE goals SET woke_ts=:ts WHERE name=:n", &[(":ts", &now_iso()), (":n", &name)])?;
    let lead = st(&g, "lead").to_string();
    let text = format!(
        "[AgentDesk goal wake] {} / Next: {}",
        summary(db, name)?.replace('\n', " / "),
        if standing(&g) {
            "act on the measure as your charter says (member_spawn for the work). Then stop until the next wake."
        } else {
            "decide the next experiment and dispatch it (member_spawn), or run it yourself (experiment_start, experiment_done). Then stop until the next wake."
        }
    );
    let who = db.row("SELECT state, pid FROM identities WHERE name=:n", &[(":n", &lead)])?;
    match who {
        None => paths::log("WARN", &format!("goal {name}: its lead {lead} is gone")),
        Some(w) if w["state"].as_str() != Some("running") => {
            ids::start_with_prompt(db, &lead, &text)?; // stopped or queued: the text is its first prompt
        }
        Some(w) if w["pid"].is_null() => {} // mid-Phoenix: its successor starts with the goal's status
        Some(_) => {
            if host.input(&lead, text.as_bytes()).is_ok() {
                std::thread::sleep(Duration::from_millis(300)); // text and Enter in one write read as a paste
                let _ = host.input(&lead, b"\r");
            }
        }
    }
    Ok(())
}
