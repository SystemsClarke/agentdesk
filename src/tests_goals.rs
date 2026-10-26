//! Goals: lines, the lifecycle, measuring, members, wakes and the budget.

use crate::goals::{self, parse_line, slug};
use crate::identities as ids;
use crate::identity::Caller;
use crate::sessions::Host;
use crate::tests_identities::Env;
use serde_json::{json, Value};
use std::time::Duration;

fn john() -> Caller {
    Caller { session_id: None, env_author: None, cwd: None, harness: None, pid: 1 }
}

/// An agent's session: AGENTDESK_IDENTITY is stamped in its environment, and Claude Code sets the session id.
fn agent(name: &str) -> Caller {
    std::env::set_var("AGENTDESK_IDENTITY", name);
    Caller { session_id: Some(format!("sid-{name}")), env_author: Some(name.into()), cwd: None, harness: Some("claude-code".into()), pid: 2 }
}

fn as_john() {
    std::env::remove_var("AGENTDESK_IDENTITY");
}

/// A goal in the running state with a `echo <value>` measure, made the way the lead and John would.
fn running_goal(e: &Env, name: &str, value: &str, success: &str) -> Value {
    goals::create(&e.db, name, "Make it faster", e.folder()).unwrap();
    let lead = agent(&format!("{name}-lead"));
    goals::propose(&e.db, &lead, name, "caching helps", &format!("echo {value}"), success, 1).unwrap();
    as_john();
    goals::approve(&e.db, &john(), name, None, None, None).unwrap()
}

#[test]
fn success_lines_and_names() {
    assert!(parse_line("value < 10").unwrap().met(9.0) && !parse_line("value < 10").unwrap().met(10.0));
    assert!(parse_line("VALUE >= 3.5").unwrap().met(3.5));
    assert!(parse_line("value == 7").unwrap().met(7.0) && !parse_line("value == 7").unwrap().met(8.0));
    assert!(parse_line("pass").unwrap().met(1.0) && !parse_line("pass").unwrap().met(0.0));
    assert!(parse_line("faster").is_err() && parse_line("value ~ 3").is_err());
    assert_eq!(slug("Add GoCD Health"), "Add-GoCD-Health");
    assert_eq!(slug("3 builds"), "g-3-builds");
    assert_eq!(slug(&"x".repeat(40)).len(), 32);
}

#[test]
fn a_goal_is_made_proposed_by_its_lead_and_approved_by_john_alone() {
    let e = Env::new();
    as_john();
    let g = goals::create(&e.db, "Speed Up", "Make the build faster", e.folder()).unwrap();
    assert_eq!((g["name"].as_str(), g["state"].as_str(), g["lead"].as_str()), (Some("Speed-Up"), Some("draft"), Some("Speed-Up-lead")));
    let lead = ids::get(&e.db, "Speed-Up-lead").unwrap().unwrap();
    assert_eq!((lead["state"].as_str(), lead["model"].as_str()), (Some("queued"), Some("opus")), "the lead starts, with the task as its first prompt");
    assert!(lead["wake_prompt"].as_str().unwrap().contains("call goal_propose with name=Speed-Up"));
    assert!(goals::create(&e.db, "Speed Up", "again", e.folder()).is_err(), "names are unique");

    // Only the lead (or John) proposes; an outsider is refused.
    let other = agent("someone-else");
    assert!(goals::propose(&e.db, &other, "Speed-Up", "h", "echo 1", "value < 2", 1).is_err());
    let lead_caller = agent("Speed-Up-lead");
    assert!(goals::propose(&e.db, &lead_caller, "Speed-Up", "h", "echo 1", "nonsense", 1).is_err(), "the success line is checked");
    goals::propose(&e.db, &lead_caller, "Speed-Up", "caching helps", "echo 42", "value < 10", 3).unwrap();
    assert!(goals::approve(&e.db, &lead_caller, "Speed-Up", None, None, None).is_err(), "an agent never approves");
    as_john();
    let running = goals::approve(&e.db, &john(), "Speed-Up", Some(2), Some(1.5), Some(10.0)).unwrap();
    assert_eq!((running["state"].as_str(), running["max_members"].as_i64(), running["samples"].as_i64()), (Some("running"), Some(2), Some(3)));
    assert!(goals::approve(&e.db, &john(), "Speed-Up", None, None, None).is_err(), "already running");
    let s = goals::summary(&e.db, "Speed-Up").unwrap();
    assert!(s.starts_with("Goal Speed-Up (running): Make the build faster\nHypothesis: caching helps\nSuccess: value < 10, measured by `echo 42`"));
    assert!(s.contains("Budget: 0 of 2 members (), 1.5 h; wakes every 10 min.") && s.ends_with("Metric trend: no measurements yet"), "{s}");
}

#[test]
fn a_measure_records_the_value_and_the_verdict_and_a_met_line_ends_the_goal() {
    let e = Env::new();
    running_goal(&e, "g1", "42", "value < 10");
    let lead = agent("g1-lead");
    let n = goals::experiment_start(&e.db, &lead, "g1", "add a cache").unwrap()["n"].as_i64().unwrap();
    assert!(goals::experiment_start(&e.db, &agent("outsider"), "g1", "x").is_err(), "only the lead and members run experiments");

    let first = goals::measure(&e.db, "g1", n, Duration::from_secs(30)).unwrap();
    assert_eq!((first["value"].as_f64(), first["verdict"].as_str()), (Some(42.0), Some("no gain")), "42 is not under 10, and there is nothing to beat");
    assert!(e.db.row("SELECT 1 FROM goals WHERE name='g1' AND woke_ts IS NULL AND state='running'", &[]).unwrap().is_some(), "the lead is woken with the result");

    e.db.exec("UPDATE goals SET measure_cmd='echo 30' WHERE name='g1'", &[]).unwrap();
    let n2 = goals::experiment_start(&e.db, &lead, "g1", "smaller cache").unwrap()["n"].as_i64().unwrap();
    assert_eq!(goals::measure(&e.db, "g1", n2, Duration::from_secs(30)).unwrap()["verdict"], "improved", "30 beats the best so far, 42, on a < line");

    e.db.exec("UPDATE goals SET measure_cmd='echo 7' WHERE name='g1'", &[]).unwrap();
    let n3 = goals::experiment_start(&e.db, &lead, "g1", "no cache at all").unwrap()["n"].as_i64().unwrap();
    assert_eq!(goals::measure(&e.db, "g1", n3, Duration::from_secs(30)).unwrap()["verdict"], "met");
    let g = goals::status(&e.db, "g1").unwrap();
    assert_eq!(g["state"], "succeeded");
    assert_eq!(g["history"], json!([42.0, 30.0, 7.0]));
    assert_eq!(ids::get(&e.db, "g1-lead").unwrap().unwrap()["state"], "stopped", "its lead stopped");
    assert!(e.db.row("SELECT 1 FROM messages WHERE body LIKE '**Goal succeeded**: experiment #3 measured 7%'", &[]).unwrap().is_some());
    assert!(goals::experiment_start(&e.db, &lead, "g1", "after the end").is_err(), "a finished goal runs no more experiments");
}

#[test]
fn john_changes_a_running_goals_success_line_and_the_next_measure_uses_it() {
    let e = Env::new();
    running_goal(&e, "g1s", "42", "value < 10");
    let lead = agent("g1s-lead");
    assert!(goals::set_success(&e.db, &lead, "g1s", "value < 50").is_err(), "an agent cannot move the bar");
    as_john();
    assert!(goals::set_success(&e.db, &john(), "g1s", "faster").is_err(), "a malformed line is refused");
    assert_eq!(goals::status(&e.db, "g1s").unwrap()["success"], "value < 10", "a refused change leaves the line alone");
    let g = goals::set_success(&e.db, &john(), "g1s", "value < 50").unwrap();
    assert_eq!(g["success"], "value < 50");
    assert!(e.db.row("SELECT 1 FROM messages WHERE body='**Success line changed by John:** value < 10 → value < 50'", &[]).unwrap().is_some());

    let n = goals::experiment_start(&e.db, &agent("g1s-lead"), "g1s", "x").unwrap()["n"].as_i64().unwrap();
    assert_eq!(goals::measure(&e.db, "g1s", n, Duration::from_secs(30)).unwrap()["verdict"], "met", "42 meets the new line");

    assert!(goals::set_success(&e.db, &john(), "g1s", "value < 5").is_err(), "a succeeded goal is not changed");
    goals::create(&e.db, "draft1", "o", e.folder()).unwrap();
    assert!(goals::set_success(&e.db, &john(), "draft1", "value < 5").is_err(), "a draft is re-proposed, not changed");
}

#[test]
fn a_measure_that_fails_or_prints_no_number_is_an_error_not_a_value() {
    let e = Env::new();
    running_goal(&e, "g2", "hello", "value < 10");
    let lead = agent("g2-lead");
    let n = goals::experiment_start(&e.db, &lead, "g2", "c").unwrap()["n"].as_i64().unwrap();
    let r = goals::measure(&e.db, "g2", n, Duration::from_secs(30)).unwrap();
    assert!(r["value"].is_null() && r["verdict"].as_str().unwrap().starts_with("error: no number on stdout"), "{r}");
    let (v, why) = goals::run_once("exit 3", e.folder(), false, Duration::from_secs(30));
    assert!(v.is_none() && why.as_deref() == Some("exit code 3"));
    let (pass, _) = goals::run_once("exit 3", e.folder(), true, Duration::from_secs(30));
    assert_eq!(pass, Some(0.0), "on a pass line a failing exit is 0, not an error");
    let (slow, why) = goals::run_once(if cfg!(windows) { "ping -n 20 127.0.0.1" } else { "sleep 20" }, e.folder(), false, Duration::from_millis(300));
    assert!(slow.is_none() && why.unwrap().starts_with("timed out"));
}

#[test]
fn experiment_done_is_measured_by_the_supervisors_runner_while_the_agent_waits() {
    let e = Env::new();
    running_goal(&e, "g3", "42", "value < 10");
    let lead = agent("g3-lead");
    let n = goals::experiment_start(&e.db, &lead, "g3", "c").unwrap()["n"].as_i64().unwrap();
    e.db.exec("UPDATE experiments SET measuring=1 WHERE goal='g3'", &[]).unwrap(); // what experiment_done writes before it waits
    let mut host = Host::new();
    let mut runner = goals::Runner::new(e.db.path.clone());
    runner.tick(&e.db, &mut host).unwrap();
    let mut done = None;
    for _ in 0..100 {
        if let Some(r) = e.db.row("SELECT * FROM experiments WHERE goal='g3' AND n=:n AND measured_ts IS NOT NULL", &[(":n", &n)]).unwrap() {
            done = Some(r);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let r = done.expect("the runner measured it on a thread");
    assert_eq!((r["value"].as_f64(), r["measuring"].as_i64()), (Some(42.0), Some(0)));
    // An experiment_done that is not measured in time says so, and the verdict still arrives on the thread.
    let n2 = goals::experiment_start(&e.db, &lead, "g3", "again").unwrap()["n"].as_i64().unwrap();
    let r = goals::experiment_done_wait(&e.db, &lead, "g3", n2, Duration::from_millis(300)).unwrap();
    assert_eq!(r["measuring"], true);
    assert!(goals::experiment_done_wait(&e.db, &lead, "g3", 99, Duration::from_millis(10)).is_err(), "no such experiment");
}

#[test]
fn a_lead_spawns_members_who_queue_past_max_members_and_retire_with_member_done() {
    let e = Env::new();
    running_goal(&e, "g4", "42", "value < 10");
    e.db.exec("UPDATE goals SET max_members=1 WHERE name='g4'", &[]).unwrap();
    let lead = agent("g4-lead");
    assert!(goals::spawn(&e.db, &agent("outsider"), "g4", "m1", "t", None, None).is_err(), "only the lead spawns");
    assert!(goals::spawn(&e.db, &lead, "g4", "m1", "t", Some("gpt"), None).is_err());
    let m1 = goals::spawn(&e.db, &lead, "g4", "m1", "benchmark the cache", Some("haiku"), None).unwrap();
    assert_eq!((m1["name"].as_str(), m1["model"].as_str(), m1["state"].as_str()), (Some("g4-m1"), Some("haiku"), Some("queued")));
    assert!(goals::spawn(&e.db, &lead, "g4", "m1", "again", None, None).is_err(), "already a member");
    goals::spawn(&e.db, &lead, "g4", "m2", "second task", None, None).unwrap();
    let charter = ids::get(&e.db, "g4-m1").unwrap().unwrap()["charter"].as_str().unwrap().to_string();
    assert!(charter.contains("You are a member of the AgentDesk goal g4, dispatched by its lead g4-lead.") && charter.contains("Your task: benchmark the cache"));

    let m = agent("g4-m1");
    let r = goals::member_done(&e.db, &m, "Cache helps by 20%.").unwrap();
    assert_eq!(r["retired"], "g4-m1");
    assert!(ids::get(&e.db, "g4-m1").unwrap().is_none(), "forgotten, so the supervisor ends its session");
    assert!(!goals::is_member(&e.db, "g4-m1"));
    assert!(e.db.row("SELECT 1 FROM messages WHERE body='**Done.** Cache helps by 20%.'", &[]).unwrap().is_some());
    assert!(goals::member_done(&e.db, &lead, "x").is_err(), "the lead is not a member");
}

#[test]
fn a_members_work_item_is_handed_over_and_goes_back_to_the_lead_if_unfinished() {
    let e = Env::new();
    running_goal(&e, "g5", "42", "value < 10");
    let lead = agent("g5-lead");
    let item = e.db.start_thread("work", "Fix the flaky test", "john", crate::db::HUMAN, "details", Some(&json!({"claim": "auto"})), None).unwrap();
    assert!(goals::spawn(&e.db, &lead, "g5", "w1", "t", None, Some(item)).is_err(), "it must be claimed by the lead first");
    assert!(e.db.claim_task(item, "g5-lead").unwrap());
    goals::spawn(&e.db, &lead, "g5", "w1", "t", None, Some(item)).unwrap();
    assert_eq!(crate::db::parse_obj(e.db.row("SELECT meta FROM threads WHERE id=:i", &[(":i", &item)]).unwrap().unwrap()["meta"].as_str()).unwrap()["assignee"], "g5-w1");
    goals::member_done(&e.db, &agent("g5-w1"), "ran out of time").unwrap();
    let back = e.db.row("SELECT meta FROM threads WHERE id=:i", &[(":i", &item)]).unwrap().unwrap();
    assert_eq!(crate::db::parse_obj(back["meta"].as_str()).unwrap()["assignee"], "g5-lead", "an unfinished item returns to the lead");
}

#[test]
fn stopping_a_goal_forgets_its_members_and_puts_their_work_back_on_the_queue() {
    let e = Env::new();
    running_goal(&e, "g6", "42", "value < 10");
    let lead = agent("g6-lead");
    let item = e.db.start_thread("work", "Task", "john", crate::db::HUMAN, "details", None, None).unwrap();
    e.db.claim_task(item, "g6-lead").unwrap();
    goals::spawn(&e.db, &lead, "g6", "m1", "t", None, None).unwrap();
    assert!(goals::stop(&e.db, &agent("outsider"), "g6").is_err());
    goals::stop(&e.db, &lead, "g6").unwrap();
    let g = goals::status(&e.db, "g6").unwrap();
    assert_eq!(g["state"], "stopped");
    assert!(ids::get(&e.db, "g6-m1").unwrap().is_none() && ids::get(&e.db, "g6-lead").unwrap().unwrap()["state"] == "stopped");
    assert_eq!(e.db.row("SELECT status FROM threads WHERE id=:i", &[(":i", &item)]).unwrap().unwrap()["status"], "open", "its claimed work item is open again");
}

#[test]
fn the_runner_wakes_the_lead_at_cadence_and_ends_a_goal_whose_hours_are_spent() {
    let e = Env::new();
    running_goal(&e, "g7", "42", "value < 10");
    let mut host = Host::new();
    let mut runner = goals::Runner::new(e.db.path.clone());
    runner.tick(&e.db, &mut host).unwrap(); // approved just now, never woken: due
    let lead = ids::get(&e.db, "g7-lead").unwrap().unwrap();
    assert_eq!(lead["state"], "queued");
    assert!(lead["wake_prompt"].as_str().unwrap().starts_with("[AgentDesk goal wake] Goal g7 (running)"), "a stopped lead starts with the goal's status as its first prompt");
    let woke = e.db.row("SELECT woke_ts FROM goals WHERE name='g7'", &[]).unwrap().unwrap()["woke_ts"].clone();
    assert!(woke.is_string(), "woke_ts is recorded");

    e.db.exec("UPDATE goals SET started_ts='2020-01-01T00:00:00+00:00', max_hours=1 WHERE name='g7'", &[]).unwrap();
    runner.tick(&e.db, &mut host).unwrap();
    assert_eq!(goals::status(&e.db, "g7").unwrap()["state"], "exhausted");
}

#[test]
fn a_standing_goal_on_an_internal_measure_idles_at_its_line_and_wakes_its_lead_off_it() {
    let e = Env::new();
    as_john();
    ids::create(&e.db, "concierge-lead", e.folder(), None, None, false).unwrap();
    let ts = crate::db::now_iso();
    e.db.exec(
        "INSERT INTO goals (name, objective, hypothesis, measure_cmd, measure_folder, success, max_members, cadence_minutes, state, lead, thread_id, started_ts, created_ts, updated_ts, standing) \
         VALUES ('concierge','Hire work','h','internal:open_work',:f,'value <= 0',4,60,'running','concierge-lead',NULL,:ts,:ts,:ts,1)",
        &[(":f", &e.folder()), (":ts", &ts)],
    )
    .unwrap();
    e.db.exec("UPDATE goals SET woke_ts=:ts", &[(":ts", &ts)]).unwrap(); // woken just now: not due at cadence
    let mut host = Host::new();
    let mut runner = goals::Runner::new(e.db.path.clone());
    runner.tick(&e.db, &mut host).unwrap();
    assert_eq!(ids::get(&e.db, "concierge-lead").unwrap().unwrap()["state"], "stopped", "no open work: on its line, it idles");

    e.db.start_thread("work", "Do a thing", "john", crate::db::HUMAN, "details", Some(&json!({"claim": "auto"})), None).unwrap();
    runner.tick(&e.db, &mut host).unwrap();
    let lead = ids::get(&e.db, "concierge-lead").unwrap().unwrap();
    assert_eq!(lead["state"], "queued", "off its line, the lead is woken");
    assert!(lead["wake_prompt"].as_str().unwrap().contains("standing: at its line it idles") && lead["wake_prompt"].as_str().unwrap().contains("now 1"));
    ids::stop(&e.db, "concierge-lead").unwrap();
    runner.tick(&e.db, &mut host).unwrap();
    assert_eq!(ids::get(&e.db, "concierge-lead").unwrap().unwrap()["state"], "stopped", "the same value does not wake it again before cadence");
}

#[test]
fn a_phoenix_successor_is_told_where_its_goal_stands() {
    let e = Env::new();
    running_goal(&e, "g8", "42", "value < 10");
    let p = ids::successor_prompt_for(&e.db, "g8-lead", 2, "Owns the benchmarks.");
    assert!(p.starts_with("You are g8-lead, generation 2.") && p.contains("Your goal's status now, from the core:\nGoal g8 (running)"), "{p}");
}

#[test]
fn another_agent_can_start_and_approve_a_goal_within_a_budget() {
    let e = Env::new();
    // an agent starts a goal; its lead proposes
    let creator = agent("planner");
    goals::create(&e.db, "Tidy Up", "Tidy the repo", e.folder()).unwrap();
    let lead = agent("Tidy-Up-lead");
    goals::propose(&e.db, &lead, "Tidy-Up", "fewer files", "echo 3", "value < 5", 1).unwrap();
    // the lead does not approve its own goal
    let e1 = goals::approve(&e.db, &lead, "Tidy-Up", None, None, None).unwrap_err().to_string();
    assert!(e1.contains("own lead"), "{e1}");
    // the agent that started it approves, within what an agent may grant
    let c2 = agent("planner");
    let too_big = goals::approve(&e.db, &c2, "Tidy-Up", Some(goals::AGENT_MAX_MEMBERS + 1), None, None).unwrap_err().to_string();
    assert!(too_big.contains("John grants more"), "{too_big}");
    let running = goals::approve(&e.db, &c2, "Tidy-Up", Some(2), Some(6.0), None).unwrap();
    assert_eq!((running["state"].as_str(), running["max_members"].as_i64()), (Some("running"), Some(2)));
    let thread = running["thread_id"].as_i64().unwrap();
    let said: String =
        e.db.rows("SELECT body FROM messages WHERE thread_id=:t ORDER BY id", &[(":t", &thread)]).unwrap().iter().filter_map(|r| r["body"].as_str().map(String::from)).collect();
    assert!(said.contains("Approved by planner."), "the thread says who approved: {said}");
    let _ = creator;
    // a running goal's budget is still John's to change
    assert!(goals::approve(&e.db, &c2, "Tidy-Up", Some(3), None, None).is_err());
    as_john();
}
