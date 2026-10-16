//! Question kinds on the phone: what Slack says about each kind, and what a reply to each one is.

use crate::slack::{Bridge, Chat};
use crate::tests_identities::Env;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const DM: &str = "D1";
const JOHN: &str = "U1";

type Post = (String, Option<String>, Option<Vec<Value>>, String, bool);

#[derive(Default)]
struct Fake {
    posts: Mutex<Vec<Post>>,
}

impl Chat for Fake {
    fn post(&self, channel: &str, thread_ts: Option<&str>, blocks: Option<Vec<Value>>, text: &str, broadcast: bool) -> anyhow::Result<String> {
        let mut p = self.posts.lock().unwrap();
        p.push((channel.into(), thread_ts.map(String::from), blocks, text.into(), broadcast));
        Ok(format!("100.{:03}", p.len()))
    }
    fn download(&self, _url: &str) -> anyhow::Result<Vec<u8>> {
        Ok(b"x".to_vec())
    }
}

fn bridge(e: &Env, fake: &Arc<Fake>) -> Bridge {
    Bridge::new(fake.clone(), DM, JOHN, e.dir.path().join("bridge_state.json"), e.dir.path().join("attachments"))
}

fn caller() -> crate::identity::Caller {
    crate::identity::Caller { session_id: Some("s1".into()), env_author: None, cwd: Some("/work".into()), harness: Some("claude-code".into()), pid: 1 }
}

fn ask(e: &Env, args: Value) -> i64 {
    let r: Value = serde_json::from_str(&crate::tools::call(&e.db, &caller(), "ask_human", &args)).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    r["thread_id"].as_i64().unwrap()
}

fn packet(e: &Env) -> i64 {
    ask(
        e,
        json!({"subject": "Two GoCD runs", "body": "Two runs are waiting on you.", "author": "builder", "meta": {"type": "packet", "packet": {
            "outcome": "Done when both runs have finished.", "why": "Only John can trigger GoCD.", "happened": ["The PR merged."], "found": [],
            "steps": [{"id": "run", "title": "Start a new run"}], "next": "I read the consoles.",
            "deadline": {"date": "2999-01-01", "reason": "Closes the sprint item."}}}}),
    )
}

fn reply(text: &str, ts: &str) -> Value {
    json!({"type": "message", "channel": DM, "channel_type": "im", "user": JOHN, "text": text, "thread_ts": ts})
}

fn head_and_hint(fake: &Fake, i: usize) -> (String, String) {
    let p = fake.posts.lock().unwrap();
    let blocks = p[i].2.as_ref().unwrap();
    (blocks[0]["text"]["text"].as_str().unwrap().to_string(), blocks.last().unwrap()["elements"][0]["text"].as_str().unwrap().to_string())
}

#[test]
fn each_kind_says_what_it_is_and_how_to_respond() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let pk = packet(&e);
    ask(&e, json!({"subject": "Run this", "body": "Please run it.\n```sh\necho hi\n```", "author": "builder", "meta": {"type": "run"}}));
    crate::tools::call(
        &e.db,
        &caller(),
        "propose_permission_rule",
        &json!({"rule": "Bash(cargo fmt:*)", "reason": "format", "author": "builder", "scope": "project", "project_dir": e.dir.path().to_string_lossy()}),
    );
    e.db.start_thread("question", "Plain", "builder", "agent", "Ship it?", None, None).unwrap();
    b.poll(&e.db).unwrap();
    let n = fake.posts.lock().unwrap().len();
    assert_eq!(n, 4);
    let mut seen: Vec<(String, String)> = (0..n).map(|i| head_and_hint(&fake, i)).collect();
    seen.sort();
    let find = |noun: &str| seen.iter().find(|(h, _)| h.contains(noun)).unwrap_or_else(|| panic!("no '{noun}' in {seen:?}")).clone();
    assert!(find(&format!("☎ Packet #{pk}")).1.contains("add a note") && find("Packet").1.contains("finish it in the AgentDesk window"));
    assert!(find("☎ Run request").1.contains("Alt+R twice"));
    assert!(find("☎ Permission rule").1.contains("Alt+A approves, Alt+D rejects") && find("Permission rule").1.contains("only a note"));
    assert!(find("☎ Question").1.contains("answer. Photos work too"), "an ordinary question reads exactly as before");
    let texts: Vec<String> = fake.posts.lock().unwrap().iter().map(|p| p.3.clone()).collect();
    assert!(texts.iter().any(|t| t.starts_with(&format!("Packet #{pk} from builder: Two GoCD runs"))), "{texts:?}");
    let body: String = fake.posts.lock().unwrap().iter().filter_map(|p| p.2.clone()).flatten().map(|b| b.to_string()).collect::<Vec<_>>().join(" ");
    assert!(body.contains("Your steps") && !body.contains("<!--"), "the step markers are for the window, not the phone");
}

#[test]
fn a_reply_on_a_packet_is_a_note_and_does_not_close_it() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let pk = packet(&e);
    b.poll(&e.db).unwrap();
    b.handle_reply(&e.db, &reply("the second host is down", "100.001")).unwrap();
    let t = e.db.get_thread(pk).unwrap();
    assert_eq!(t["thread"]["status"], "open", "a note is not the answer");
    let last = t["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!((last["author"].as_str().unwrap(), last["body"].as_str().unwrap()), ("john", "the second host is down"));
    assert!(last["meta"].as_str().unwrap().contains("slack"), "{last}");
    let said = fake.posts.lock().unwrap().last().unwrap().3.clone();
    assert!(said.contains(&format!("Recorded as a note on #{pk}")) && said.contains("stays open"), "{said}");
    assert_eq!(e.db.scalar_i64("SELECT COUNT(*) FROM acks WHERE thread_id=:t", &[(":t", &pk)]).unwrap(), Some(0), "nothing was answered, so nothing is acked yet");
}

#[test]
fn a_reply_on_a_permission_question_does_not_decide_the_rule() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let r: Value = serde_json::from_str(&crate::tools::call(
        &e.db,
        &caller(),
        "propose_permission_rule",
        &json!({"rule": "Bash(cargo fmt:*)", "reason": "format", "author": "builder", "scope": "project", "project_dir": e.dir.path().to_string_lossy()}),
    ))
    .unwrap();
    let tid = r["thread_id"].as_i64().unwrap();
    b.poll(&e.db).unwrap();
    b.handle_reply(&e.db, &reply("yes, go ahead", "100.001")).unwrap();
    assert_eq!(e.db.get_thread(tid).unwrap()["thread"]["status"], "open", "still waiting on Approve or Reject");
    assert_eq!(e.db.row("SELECT status FROM permission_proposals WHERE thread_id=:t", &[(":t", &tid)]).unwrap().unwrap()["status"], "pending");
    assert!(!e.dir.path().join(".claude/settings.json").exists(), "a phone reply writes no rule");
}

#[test]
fn a_reply_on_a_run_question_is_its_answer() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let tid = ask(&e, json!({"subject": "Run this", "body": "Please run it.\n```sh\necho hi\n```", "author": "builder", "meta": {"type": "run"}}));
    b.poll(&e.db).unwrap();
    b.handle_reply(&e.db, &reply("it printed hi", "100.001")).unwrap();
    assert_eq!(e.db.get_thread(tid).unwrap()["thread"]["status"], "answered");
    assert!(fake.posts.lock().unwrap().last().unwrap().3.contains(&format!("Recorded as your answer to #{tid}")));
}
