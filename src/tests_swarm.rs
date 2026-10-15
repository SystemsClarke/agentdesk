//! Swarm slots: the slot table, John's `swarm` commands, John's messages in a slot's channel typed into the lead, and the goal's board thread
//! relayed into the channel, against a fake Slack.

use crate::db::{Db, AGENT};
use crate::identity::Caller;
use crate::slack::{Api, SlackError};
use crate::slack_cmd::Commands;
use crate::swarm::Swarms;
use crate::tests_identities::Env;
use crate::{goals, identities as ids, slots};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const JOHN: &str = "U1";

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
    /// method -> error code, once
    fail: Mutex<HashMap<String, String>>,
    /// method -> error code, every time
    fail_always: Mutex<HashMap<String, String>>,
    history: Mutex<Vec<Value>>,
    next: Mutex<u32>,
}

impl Api for Fake {
    fn api(&self, method: &str, body: &Value) -> anyhow::Result<Value> {
        self.calls.lock().unwrap().push((method.to_string(), body.clone()));
        if let Some(code) = self.fail.lock().unwrap().remove(method) {
            return Err(SlackError(code).into());
        }
        if let Some(code) = self.fail_always.lock().unwrap().get(method) {
            return Err(SlackError(code.clone()).into());
        }
        let mut n = self.next.lock().unwrap();
        *n += 1;
        Ok(match method {
            "chat.postMessage" => json!({"ok": true, "ts": format!("200.{:03}", *n)}),
            "conversations.create" => json!({"ok": true, "channel": {"id": format!("C{}", *n), "name": body["name"]}}),
            "conversations.rename" => json!({"ok": true, "channel": {"id": body["channel"], "name": body["name"]}}),
            "conversations.history" => {
                let oldest: f64 = body["oldest"].as_str().and_then(|o| o.parse().ok()).unwrap_or(0.0);
                let h = self.history.lock().unwrap();
                let mut msgs: Vec<Value> = h.iter().filter(|m| m["ts"].as_str().unwrap().parse::<f64>().unwrap() > oldest).cloned().collect();
                msgs.reverse(); // Slack lists newest first
                if body["limit"] == json!(1) {
                    msgs.truncate(1);
                }
                json!({"ok": true, "messages": msgs})
            }
            _ => json!({"ok": true}),
        })
    }
}

impl Fake {
    fn called(&self, method: &str) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|(m, _)| m == method).map(|(_, b)| b.clone()).collect()
    }
    fn posts(&self) -> Vec<Value> {
        self.called("chat.postMessage")
    }
}

type Typed = Arc<Mutex<Vec<(String, String)>>>;

fn swarms(e: &Env, fake: &Arc<Fake>) -> (Swarms, Typed) {
    std::env::remove_var("AGENTDESK_IDENTITY");
    let typed: Typed = Arc::default();
    let t = typed.clone();
    let sw = Swarms::with(
        fake.clone(),
        JOHN,
        e.dir.path().join("swarm_state.json"),
        Arc::new(move |lead: &str, text: &str| {
            t.lock().unwrap().push((lead.into(), text.into()));
            Ok(())
        }),
        || "2026-10-12".to_string(),
    );
    (sw, typed)
}

fn lead_proposes(db: &Db, goal: &str) {
    std::env::set_var("AGENTDESK_IDENTITY", format!("{goal}-lead"));
    let c = Caller { session_id: Some("s".into()), env_author: Some(format!("{goal}-lead")), cwd: None, harness: Some("claude-code".into()), pid: 2 };
    goals::propose(db, &c, goal, "it helps", "echo 5", "value < 3", 1).unwrap();
    std::env::remove_var("AGENTDESK_IDENTITY");
}

#[test]
fn slots_are_ten_and_a_goal_lives_in_one_of_them() {
    let e = Env::new();
    assert_eq!(slots::list(&e.db).unwrap()["slots"].as_array().unwrap().len(), 10);
    assert!(slots::assign(&e.db, 11, &json!({})).is_err());
    assert!(slots::assign(&e.db, 1, &json!({"goal": "nope"})).is_err(), "no such goal");
    goals::create(&e.db, "alpha", "obj", e.folder()).unwrap();
    let s = slots::assign(&e.db, 1, &json!({"goal": "alpha", "persona": "alpha", "persona_icon": ":red_circle:", "channel_id": "C1", "channel_name": "swarm-alpha"})).unwrap();
    assert_eq!((s["goal"].clone(), s["channel_id"].clone(), s["state"].clone()), (json!("alpha"), json!("C1"), json!("draft")));
    assert!(slots::assign(&e.db, 2, &json!({"goal": "alpha"})).is_err(), "already in slot 1");
    let kept = slots::assign(&e.db, 1, &json!({"persona": "renamed"})).unwrap();
    assert_eq!((kept["goal"].clone(), kept["channel_id"].clone(), kept["persona_name"].clone()), (json!("alpha"), json!("C1"), json!("renamed")));
    let freed = slots::clear(&e.db, 1).unwrap();
    assert_eq!((freed["goal"].clone(), freed["persona_name"].clone(), freed["channel_id"].clone()), (Value::Null, Value::Null, json!("C1")));
}

#[test]
fn swarm_new_makes_the_goal_a_channel_a_persona_and_a_first_post_and_reuses_channels() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let (sw, _) = swarms(&e, &fake);
    assert!(sw.new_swarm(&e.db, "9bad", e.folder(), "x").starts_with("⚠ A swarm name"));
    let said = sw.new_swarm(&e.db, "Alpha", e.folder(), "make it faster");
    assert!(said.starts_with("Slot 1: *Alpha* in <#C") && said.contains("approve with `swarm approve 1`"), "{said}");
    let create = &fake.called("conversations.create")[0];
    assert_eq!((create["name"].clone(), create["is_private"].clone()), (json!("swarm-alpha"), json!(false)));
    assert_eq!(fake.called("conversations.invite")[0]["users"], JOHN);
    let post = &fake.posts()[0];
    assert_eq!((post["username"].clone(), post["icon_emoji"].clone()), (json!("Alpha"), json!(":large_blue_circle:")));
    assert!(post["text"].as_str().unwrap().contains("Proposing a hypothesis"));
    assert!(ids::get(&e.db, "Alpha-lead").unwrap().is_some(), "the goal's lead exists");
    // a second swarm takes the next free slot, with a channel of its own
    assert!(sw.new_swarm(&e.db, "beta", e.folder(), "o").starts_with("Slot 2: *beta*"));
    assert_eq!(fake.called("conversations.create").len(), 2);
    // end slot 1: it keeps its channel, and the next swarm reuses it by renaming
    assert!(sw.end(&e.db, 1).starts_with("Ended *Alpha* (stopped); slot 1 is free and keeps its channel."));
    assert!(sw.list(&e.db).lines().next().unwrap().starts_with("*1* · free · <#C"));
    assert!(sw.new_swarm(&e.db, "gamma", e.folder(), "o").starts_with("Slot 1: *gamma*"));
    assert_eq!(fake.called("conversations.rename")[0]["name"], "swarm-gamma");
    assert_eq!(fake.called("conversations.create").len(), 2, "no new channel was made");
    assert!(sw.list(&e.db).contains("gamma · draft · last — · 0 member(s)"));
}

#[test]
fn a_slack_refusal_of_the_channel_still_leaves_the_goal_in_its_slot() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    fake.fail_always.lock().unwrap().insert("conversations.create".into(), "restricted_action".into());
    let (sw, _) = swarms(&e, &fake);
    let said = sw.new_swarm(&e.db, "alpha", e.folder(), "o");
    assert!(said.contains("created in slot 1, but Slack refused the channel: restricted_action"), "{said}");
    assert_eq!(slots::list(&e.db).unwrap()["slots"][0]["goal"], "alpha");
    assert!(sw.new_swarm(&e.db, "alpha", e.folder(), "o").starts_with("⚠ goal already exists"));
}

#[test]
fn approve_end_and_reset_follow_the_slot() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let (sw, _) = swarms(&e, &fake);
    assert_eq!(sw.approve(&e.db, 1, &json!({})), "Slot 1 is free.");
    assert_eq!(sw.approve(&e.db, 11, &json!({})), "A slot is 1 to 10.");
    sw.new_swarm(&e.db, "alpha", e.folder(), "make it faster");
    assert!(sw.approve(&e.db, 1, &json!({})).contains("no proposal yet"));
    lead_proposes(&e.db, "alpha");
    let said = sw.approve(&e.db, 1, &json!({"max_members": 2, "max_hours": 5.0, "cadence_minutes": 10.0}));
    assert_eq!(said, "Approved *alpha*: 2 members, 5 h, the lead wakes every 10 min.");
    assert!(fake.posts().iter().any(|p| p["text"].as_str().unwrap().contains("Approved by John")), "the approval note is relayed at once");
    // reset: stop, forget the lead, archive the channel, a fresh goal alpha-2 with the same objective
    let said = sw.reset(&e.db, 1);
    assert!(said.starts_with("Slot 1: *alpha-2* in <#C"), "{said}");
    assert_eq!(fake.called("conversations.archive").len(), 1);
    assert!(ids::get(&e.db, "alpha-lead").unwrap().is_none() && ids::get(&e.db, "alpha-2-lead").unwrap().is_some());
    assert_eq!(goals::status(&e.db, "alpha-2").unwrap()["objective"], "make it faster");
    assert_eq!(goals::status(&e.db, "alpha").unwrap()["state"], "stopped");
    assert!(sw.reset(&e.db, 1).contains("*alpha-3*"), "names are never reused");
}

#[test]
fn the_goal_threads_posts_reach_the_channel_once_in_the_right_place() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let (sw, _) = swarms(&e, &fake);
    sw.new_swarm(&e.db, "alpha", e.folder(), "o");
    let tid = goals::status(&e.db, "alpha").unwrap()["thread_id"].as_i64().unwrap();
    let before = fake.posts().len();
    assert_eq!(sw.relay(&e.db).unwrap(), 0, "the opener is not relayed");
    e.db.reply(tid, "agentdesk", AGENT, "**Experiment 1**: value 4, improved.", None, None).unwrap();
    e.db.reply(tid, "alpha-lead", AGENT, "Proposal: caching.", None, None).unwrap();
    e.db.reply(tid, "alpha-m1", AGENT, "ran the benchmark", None, None).unwrap();
    e.db.reply(tid, "alpha-m2", AGENT, "second note", None, None).unwrap();
    e.db.reply(tid, "john", crate::db::HUMAN, "carry on", None, None).unwrap();
    e.db.reply(tid, "alpha-m1", AGENT, "got it", None, Some(&json!({"kind": "ack"}))).unwrap();
    assert_eq!(sw.relay(&e.db).unwrap(), 4);
    assert_eq!(sw.relay(&e.db).unwrap(), 0, "each once");
    let posts = fake.posts()[before..].to_vec();
    let text = |i: usize| posts[i]["text"].as_str().unwrap().to_string();
    assert!(text(0).contains("Experiment 1") && posts[0]["thread_ts"].is_null(), "agentdesk's verdict is top level");
    assert!(text(1).contains("Proposal") && posts[1]["thread_ts"].is_null(), "the lead's proposal is top level");
    assert!(text(2).contains("Activity, 2026-10-12") && posts[2]["thread_ts"].is_null());
    assert!(text(3).contains("alpha-m1: ran the benchmark") && posts[3]["thread_ts"] == json!("200.003") || posts[3]["thread_ts"].is_string(), "{posts:?}");
    assert_eq!(posts.len(), 5, "one activity message a day, then both notes under it");
    assert_eq!(posts[3]["thread_ts"], posts[4]["thread_ts"]);
}

#[test]
fn johns_messages_in_a_slot_channel_are_typed_into_the_lead_and_nobody_elses() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let (sw, typed) = swarms(&e, &fake);
    sw.new_swarm(&e.db, "alpha", e.folder(), "o");
    let cid = slots::list(&e.db).unwrap()["slots"][0]["channel_id"].as_str().unwrap().to_string();
    // the channel's own history (the persona's first post) is not input; the watermark starts at it
    fake.history.lock().unwrap().push(json!({"ts": "100.0", "user": "U9", "text": "old"}));
    assert_eq!(sw.poll(&e.db).unwrap(), 0);
    {
        let mut h = fake.history.lock().unwrap();
        h.push(json!({"ts": "300.0", "user": JOHN, "text": "try *caching*\nthen measure"}));
        h.push(json!({"ts": "301.0", "user": "U2", "text": "someone else"}));
        h.push(json!({"ts": "302.0", "user": JOHN, "bot_id": "B1", "text": "the persona itself"}));
        h.push(json!({"ts": "303.0", "user": JOHN, "subtype": "message_changed", "text": "an edit"}));
    }
    assert_eq!(sw.poll(&e.db).unwrap(), 1);
    assert_eq!(sw.poll(&e.db).unwrap(), 0, "each once");
    assert_eq!(typed.lock().unwrap().clone(), vec![("alpha-lead".to_string(), "John (via Slack): try **caching** / then measure".to_string())]);
    // a restart neither replays nor drops: a new Swarms over the same state file picks up after the watermark
    let (again, typed2) = swarms(&e, &fake);
    fake.history.lock().unwrap().push(json!({"ts": "400.0", "user": JOHN, "text": "one more"}));
    assert_eq!(again.poll(&e.db).unwrap(), 1);
    assert_eq!(typed2.lock().unwrap().len(), 1);
    // a free slot's channel answers with how to start one
    sw.end(&e.db, 1);
    fake.history.lock().unwrap().push(json!({"ts": "500.0", "user": JOHN, "text": "hello?"}));
    assert_eq!(sw.poll(&e.db).unwrap(), 0);
    let last = fake.posts().last().unwrap().clone();
    assert!(last["text"].as_str().unwrap().contains("Slot 1 is free") && last["thread_ts"] == "500.0" && last["channel"] == cid.as_str(), "{last}");
}

#[test]
fn a_lead_that_cannot_be_typed_to_is_reported_in_the_channel() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    std::env::remove_var("AGENTDESK_IDENTITY");
    let sw = Swarms::with(fake.clone(), JOHN, e.dir.path().join("s.json"), Arc::new(|_: &str, _: &str| Err(anyhow::anyhow!("alpha-lead is not running"))), || "d".into());
    sw.new_swarm(&e.db, "alpha", e.folder(), "o");
    sw.poll(&e.db).unwrap();
    fake.history.lock().unwrap().push(json!({"ts": "300.0", "user": JOHN, "text": "steer"}));
    assert_eq!(sw.poll(&e.db).unwrap(), 0);
    assert!(fake.posts().last().unwrap()["text"].as_str().unwrap().contains("Couldn't reach alpha-lead (goal draft): alpha-lead is not running"));
}

#[test]
fn without_the_customize_scope_posts_are_plain_and_prefixed_and_a_gone_channel_is_skipped() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let (sw, _) = swarms(&e, &fake);
    fake.fail.lock().unwrap().insert("chat.postMessage".into(), "missing_scope".into());
    sw.new_swarm(&e.db, "alpha", e.folder(), "o");
    let posts = fake.posts();
    assert!(posts[0]["username"].is_string() && posts[1]["username"].is_null(), "retried without the override");
    assert!(posts[1]["text"].as_str().unwrap().starts_with("alpha: Proposing"), "{posts:?}");
    let tid = goals::status(&e.db, "alpha").unwrap()["thread_id"].as_i64().unwrap();
    e.db.reply(tid, "agentdesk", AGENT, "lost", None, None).unwrap();
    fake.fail_always.lock().unwrap().insert("chat.postMessage".into(), "is_archived".into());
    assert_eq!(sw.relay(&e.db).unwrap(), 0);
    fake.fail_always.lock().unwrap().clear();
    e.db.reply(tid, "agentdesk", AGENT, "found", None, None).unwrap();
    assert_eq!(sw.relay(&e.db).unwrap(), 1, "the unpostable message was skipped, not retried forever");
}

#[test]
fn the_swarm_commands_run_through_the_phone_commands() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let (sw, _) = swarms(&e, &fake);
    let all: Vec<String> = ["swarms"].iter().map(|s| s.to_string()).collect();
    let cmds = Commands::new(&all, JOHN, "Swarm").with_swarm(Arc::new(sw));
    assert!(cmds.handle(&e.db, "swarms", Some(JOHN)).unwrap().lines().count() == 10);
    assert!(cmds.handle(&e.db, "swarm new", Some(JOHN)).unwrap().starts_with("Usage: `swarm new"));
    let made = cmds.handle(&e.db, &format!("swarm new alpha \"{}\" make it faster", e.folder()), Some(JOHN)).unwrap();
    assert!(made.starts_with("Slot 1: *alpha*"), "{made}");
    assert!(cmds.handle(&e.db, "swarm approve 1 turbo=3", Some(JOHN)).unwrap().starts_with("Usage"));
    assert!(cmds.handle(&e.db, "swarm end 2", Some(JOHN)).unwrap().contains("Slot 2 is free"));
    assert!(cmds.handle(&e.db, "swarm end 1", Some(JOHN)).unwrap().starts_with("Ended *alpha*"));
    assert!(cmds.handle(&e.db, "swarm end 1", Some("U2")).is_none());
    let none = Commands::new(&["swarms".to_string()], JOHN, "x");
    assert!(none.handle(&e.db, "swarms", Some(JOHN)).unwrap().contains("need the Slack bridge's client"));
}

#[test]
fn typing_into_a_lead_reaches_its_terminal_from_inside_the_core_and_through_the_attach_socket() {
    use crate::supervisor::Supervisor;
    let Some(py) = crate::pyhost::interpreter() else { return }; // a stand-in session that reads one line and writes it to a file
    let e = Env::new();
    let (direct, attached) = (e.dir.path().join("direct.txt"), e.dir.path().join("attached.txt"));
    let mut sup = Supervisor::new();
    assert!(sup.acquire());
    for (name, out) in [("lead", &direct), ("lead2", &attached)] {
        let code = format!("import sys\nl = sys.stdin.readline()\nopen(r'{}', 'w').write(l)\nimport time\ntime.sleep(10)", out.display());
        sup.host().launch(name, e.folder(), &py, &["-c".into(), code], &[]).unwrap();
    }
    let wait = |p: &std::path::Path| {
        for _ in 0..100 {
            if std::fs::read_to_string(p).map(|t| t.contains("hi")).unwrap_or(false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::fs::read_to_string(p).unwrap_or_default().trim().to_string()
    };
    let inside = crate::slack::registry_input(sup.host().registry());
    assert!(inside("nobody", "x").is_err(), "no such session");
    inside("LEAD", "John (via Slack): hi").unwrap(); // the name is matched without regard to case, as the attach socket does
    assert_eq!(wait(&direct), "John (via Slack): hi");
    let outside = crate::slack::attach_input();
    assert!(outside("nobody", "x").is_err());
    outside("lead2", "John (via Slack): hi").unwrap();
    assert_eq!(wait(&attached), "John (via Slack): hi");
    sup.shutdown();
}
