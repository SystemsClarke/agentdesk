//! The Slack bridge: Markdown <-> Slack, the question relay both ways, and the phone commands, against a fake Slack.

use crate::db::{Db, AGENT};
use crate::slack::{self, on_message, Bridge, Chat};
use crate::slack_cmd::Commands;
use crate::slackfmt::{fallback_text, md_to_slack, slack_to_md};
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
        Ok(b"\x89PNG fake".to_vec())
    }
}

fn bridge(e: &Env, fake: &Arc<Fake>) -> Bridge {
    Bridge::new(fake.clone(), DM, JOHN, e.dir.path().join("bridge_state.json"), e.dir.path().join("attachments"))
}

fn texts(f: &Fake) -> Vec<String> {
    f.posts.lock().unwrap().iter().map(|p| p.3.clone()).collect()
}

fn ask(db: &Db, subject: &str, body: &str) -> i64 {
    db.start_thread("question", subject, "builder", AGENT, body, None, None).unwrap()
}

fn dm_event(text: &str, thread_ts: Option<&str>) -> Value {
    let mut ev = json!({"type": "message", "channel": DM, "channel_type": "im", "user": JOHN, "text": text});
    if let Some(t) = thread_ts {
        ev["thread_ts"] = json!(t);
    }
    ev
}

#[test]
fn markdown_becomes_blocks_with_tables_lists_and_code() {
    let blocks = md_to_slack(
        "# Title\n\nSome **bold** and `code` and [a link](https://x.io).\n\n- one\n  - nested\n- [x] done\n\n| a | b |\n|---|--:|\n| 1 | 22 |\n\n---\n```rust\nfn main() {}\n```",
    );
    assert_eq!(blocks[0]["type"], "header");
    assert_eq!(blocks[0]["text"]["text"], "Title");
    let text = blocks[1]["text"]["text"].as_str().unwrap();
    assert!(text.contains("Some *bold* and `code` and <https://x.io|a link>."), "{text}");
    assert!(text.contains("• one\n    ◦ nested\n☑ done"), "{text}");
    assert!(text.contains("```\n+---+----+\n| a |  b |\n+---+----+\n| 1 | 22 |\n+---+----+\n```"), "{text}");
    assert!(blocks.iter().any(|b| b["type"] == "divider"));
    assert!(blocks.last().unwrap()["text"]["text"].as_str().unwrap().contains("```\nfn main() {}\n```"));
}

#[test]
fn a_wide_table_becomes_one_line_per_row_and_a_long_message_is_cut_into_sections() {
    let wide = md_to_slack("| name | description of the thing |\n|---|---|\n| alpha | the first of the letters in the greek alphabet |");
    assert!(wide[0]["text"]["text"].as_str().unwrap().starts_with("• *alpha*: description of the thing the first"), "{wide:?}");
    let long = (0..400).map(|i| format!("line number {i} with some padding text to fill the section up")).collect::<Vec<_>>().join("\n");
    let blocks = md_to_slack(&long);
    assert!(blocks.len() > 1 && blocks.iter().all(|b| b["text"]["text"].as_str().unwrap().chars().count() <= 3000));
    let many = (0..80).map(|i| format!("# h{i}")).collect::<Vec<_>>().join("\n\n");
    let b = md_to_slack(&many);
    assert_eq!(b.len(), 50);
    assert_eq!(b[49]["type"], "context");
}

#[test]
fn a_chart_is_a_note_and_the_fallback_text_is_plain() {
    let b = md_to_slack("```chart\n{\"type\":\"line\",\"title\":\"Burn\"}\n```");
    assert!(b[0]["text"]["text"].as_str().unwrap().contains("*Burn*"), "{b:?}");
    assert_eq!(fallback_text("# Hi\n**bold** and [x](https://y.z)\n```\ncode\n```\n> note", 200), "Hi bold** and x note");
    assert_eq!(fallback_text(&"word ".repeat(100), 10), "word word …");
}

#[test]
fn slack_text_becomes_markdown() {
    assert_eq!(
        slack_to_md("*bold* _it_ ~gone~ <https://a.b|link> <https://c.d> <@U1|john> `*not*` a &lt; b"),
        "**bold** *it* ~~gone~~ [link](https://a.b) https://c.d @john `*not*` a < b"
    );
    assert_eq!(slack_to_md("• one\n• two"), "- one\n- two");
    assert_eq!(slack_to_md("```\n*x* &lt;y&gt;\n```"), "```\n*x* <y>\n```");
    assert_eq!(slack_to_md("snake_case_word and 2*3*4"), "snake_case_word and 2*3*4");
}

#[test]
fn a_question_is_posted_once_reposted_when_asked_again_and_its_delivery_is_told_once() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let tid = ask(&e.db, "Ship it?", "Should we **ship** it?");
    b.poll(&e.db).unwrap();
    b.poll(&e.db).unwrap();
    {
        let p = fake.posts.lock().unwrap();
        assert_eq!(p.len(), 1, "posted once: {p:?}");
        assert_eq!((p[0].0.as_str(), p[0].1.clone()), (DM, None));
        assert!(p[0].3.starts_with(&format!("Question #{tid} from builder: Ship it? · Should we ship it?")), "{}", p[0].3);
        let head = p[0].2.as_ref().unwrap()[0]["text"]["text"].as_str().unwrap().to_string();
        assert!(head.contains(&format!("☎ Question #{tid}")) && head.contains("*Ship it?*"), "{head}");
    }
    // John answers, then the agent asks again: a follow-up inside the same Slack thread, broadcast to the channel.
    e.db.john_replies(tid, "yes").unwrap();
    e.db.reply(tid, "builder", AGENT, "Done. Anything else?", None, None).unwrap();
    e.db.exec("UPDATE threads SET status='open', updated_ts='2999-01-01T00:00:00+00:00' WHERE id=:t", &[(":t", &tid)]).unwrap();
    b.poll(&e.db).unwrap();
    let p = fake.posts.lock().unwrap().clone();
    let follow = p.iter().find(|x| x.3.contains("Done. Anything else?")).expect("a follow-up was posted");
    assert_eq!((follow.1.as_deref(), follow.4), (Some("100.001"), true));
    drop(p);
    // the delivery note: John's reply is acked as picked up, and said once
    let mid = e.db.scalar_i64("SELECT MAX(id) FROM messages WHERE author_kind='human'", &[]).unwrap().unwrap();
    e.db.exec("INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES (:m,'wake','resumed',NULL,'2026-01-01T00:00:00+00:00')", &[(":m", &mid)]).unwrap();
    b.poll(&e.db).unwrap();
    b.poll(&e.db).unwrap();
    let notes = texts(&fake).iter().filter(|t| t.contains("resumed its session")).count();
    assert_eq!(notes, 1, "{:?}", texts(&fake));
}

#[test]
fn only_a_reply_inside_a_question_thread_from_john_answers_it() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let tid = ask(&e.db, "Pick one", "A or B?");
    b.poll(&e.db).unwrap();
    let ts = "100.001";
    let mut other = dm_event("B", Some(ts));
    other["user"] = json!("U2");
    b.handle_reply(&e.db, &other).unwrap();
    let mut bot = dm_event("B", Some(ts));
    bot["bot_id"] = json!("B1");
    b.handle_reply(&e.db, &bot).unwrap();
    let mut edit = dm_event("B", Some(ts));
    edit["subtype"] = json!("message_changed");
    b.handle_reply(&e.db, &edit).unwrap();
    assert_eq!(e.db.get_thread(tid).unwrap()["thread"]["status"], "open");
    let mut ev = dm_event("Take *B*", None);
    ev["channel"] = json!(DM);
    b.handle_reply(&e.db, &ev).unwrap();
    assert!(texts(&fake).last().unwrap().contains("Reply *inside the thread*"));
    b.handle_reply(&e.db, &dm_event("hello", Some("999.999"))).unwrap();
    assert!(texts(&fake).last().unwrap().contains("don't recognize that thread"));
    b.handle_reply(&e.db, &dm_event("Take *B*", Some(ts))).unwrap();
    let t = e.db.get_thread(tid).unwrap();
    assert_eq!(t["thread"]["status"], "answered");
    let last = t["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!((last["author_kind"].clone(), last["body"].clone()), (json!("human"), json!("Take **B**")));
    assert!(last["meta"].as_str().unwrap().contains("slack"), "{last}");
    assert!(texts(&fake).last().unwrap().contains(&format!("Recorded as your answer to #{tid}")));
    assert_eq!(
        e.db.scalar_i64("SELECT COUNT(*) FROM acks WHERE thread_id=:t", &[(":t", &tid)]).unwrap(),
        Some(1),
        "builder is not an identity, but the ack row follows john_replies' rule"
    );
}

#[test]
fn a_photo_in_a_reply_is_saved_and_linked() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let tid = ask(&e.db, "Look", "Which?");
    b.poll(&e.db).unwrap();
    let mut ev = dm_event("this one", Some("100.001"));
    ev["subtype"] = json!("file_share");
    ev["files"] = json!([{"id": "F1", "name": "a b.png", "mimetype": "image/png", "url_private": "https://files/x"}, {"id": "F2", "name": "doc.pdf", "mimetype": "application/pdf", "url_private": "https://files/y"}]);
    b.handle_reply(&e.db, &ev).unwrap();
    let body = e.db.scalar_str("SELECT body FROM messages WHERE thread_id=:t AND author_kind='human'", &[(":t", &tid)]).unwrap().unwrap();
    assert!(body.starts_with("this one\n\n![photo](file:///") && body.contains("100_001_F1_ab.png"), "{body}");
    assert!(!body.contains("doc.pdf"));
    assert!(e.dir.path().join("attachments").join("100_001_F1_ab.png").exists());
}

#[test]
fn wake_resumes_the_asking_identity_with_johns_reply() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    crate::identities::create(&e.db, "builder", e.folder(), None, None, false).unwrap();
    let tid = ask(&e.db, "Ship?", "Ship?");
    b.poll(&e.db).unwrap();
    e.db.john_replies(tid, "go ahead").unwrap();
    b.handle_reply(&e.db, &dm_event("wake!", Some("100.001"))).unwrap();
    assert!(texts(&fake).last().unwrap().starts_with("Woke builder."), "{:?}", texts(&fake));
    let row = crate::identities::get(&e.db, "builder").unwrap().unwrap();
    assert_eq!(row["state"], "queued");
    assert!(row["wake_prompt"].as_str().unwrap().contains("go ahead"));
    e.db.exec("UPDATE identities SET state='running' WHERE name='builder'", &[]).unwrap();
    assert_eq!(slack::wake(&e.db, tid), "builder is already running");
    assert!(slack::wake(&e.db, 9999).starts_with("could not wake it"));
}

#[test]
fn phone_commands_run_only_for_john_and_only_in_the_bots_areas() {
    let e = Env::new();
    let all: Vec<String> = ["board", "agents", "concierge", "ops", "swarms"].iter().map(|s| s.to_string()).collect();
    let cmds = Commands::new(&all, JOHN, "AgentDesk");
    assert!(cmds.handle(&e.db, "agents", Some("U2")).is_none(), "never for anyone else");
    assert!(cmds.handle(&e.db, "random chatter", Some(JOHN)).is_none());
    let help = cmds.handle(&e.db, "help", Some(JOHN)).unwrap();
    assert!(help.contains("`concierge` status") && help.contains("Reply *in a question's thread*"), "{help}");
    assert!(cmds.handle(&e.db, "agents", Some(JOHN)).unwrap().starts_with("No agents yet"));
    let made = cmds.handle(&e.db, &format!("agent new scout \"{}\" look around", e.folder()), Some(JOHN)).unwrap();
    assert!(made.starts_with("Created *scout* · queued"), "{made}");
    assert!(cmds.handle(&e.db, "agents", Some(JOHN)).unwrap().contains("*scout*"));
    assert!(cmds.handle(&e.db, "agent forget scout", Some(JOHN)).unwrap().contains("Reply `yes`"));
    assert_eq!(cmds.handle(&e.db, "yes", Some(JOHN)).unwrap(), "Forgot *scout*.");
    assert_eq!(cmds.handle(&e.db, "yes", Some(JOHN)).unwrap(), "Nothing to confirm.");
    assert!(cmds.handle(&e.db, "agent frobnicate", Some(JOHN)).unwrap().starts_with("Usage"));
    assert!(cmds.handle(&e.db, "concierge", Some(JOHN)).unwrap().starts_with("Concierge is off, 0 open item(s) waiting"));
    std::env::set_var("AGENTDESK_CONCIERGE_FOLDER", e.folder());
    assert!(cmds.handle(&e.db, "concierge on", Some(JOHN)).unwrap().starts_with("Concierge is on"));
    assert!(cmds.handle(&e.db, "concierge off", Some(JOHN)).unwrap().starts_with("Concierge is off"));
    std::env::remove_var("AGENTDESK_CONCIERGE_FOLDER");
    let status = cmds.handle(&e.db, "status", Some(JOHN)).unwrap();
    assert!(status.contains("*Slack* down") && status.contains("*Open questions* 0"), "{status}");
    assert!(cmds.handle(&e.db, "swarms", Some(JOHN)).unwrap().contains("need the Slack bridge"));
    let only_agents = Commands::new(&["agents".to_string()], JOHN, "Helper");
    assert!(only_agents.handle(&e.db, "concierge", Some(JOHN)).is_none(), "another bot's area");
    assert!(!only_agents.help().contains("concierge"));
}

#[test]
fn on_message_runs_a_command_for_a_top_level_dm_and_a_reply_for_a_thread() {
    let e = Env::new();
    let fake = Arc::new(Fake::default());
    let b = bridge(&e, &fake);
    let cmds = Commands::new(&["board".to_string(), "agents".to_string()], JOHN, "AgentDesk");
    let tid = ask(&e.db, "Ship?", "Ship?");
    b.poll(&e.db).unwrap();
    on_message(&e.db, &dm_event("agents", None), fake.as_ref(), &cmds, DM, Some(&b));
    assert!(texts(&fake).last().unwrap().contains("No agents yet"));
    on_message(&e.db, &dm_event("yes ship it", Some("100.001")), fake.as_ref(), &cmds, DM, Some(&b));
    assert_eq!(e.db.get_thread(tid).unwrap()["thread"]["status"], "answered");
    let mut channel = dm_event("agents", None);
    channel["channel_type"] = json!("channel");
    let before = texts(&fake).len();
    on_message(&e.db, &channel, fake.as_ref(), &cmds, DM, Some(&b));
    assert_eq!(texts(&fake).len(), before, "only DMs");
}

#[test]
fn bots_json_names_bots_with_areas_and_only_one_owns_the_board() {
    let dir = tempfile::tempdir().unwrap();
    let none = slack::load_bots(&dir.path().join("bots.json"), dir.path(), DM).unwrap();
    assert_eq!((none.len(), none[0].areas.len(), none[0].dm.as_str()), (1, 5, DM));
    let cfg = dir.path().join("bots.json");
    std::fs::write(&cfg, r#"[{"name":"A","areas":["board","ops"]},{"name":"B","areas":["agents"],"creds":"b","dm":"D2"}]"#).unwrap();
    let bots = slack::load_bots(&cfg, dir.path(), DM).unwrap();
    assert_eq!((bots[1].dm.as_str(), bots[1].creds.clone()), ("D2", dir.path().join("b")));
    std::fs::write(&cfg, r#"[{"name":"A","areas":["nonsense"]}]"#).unwrap();
    assert!(slack::load_bots(&cfg, dir.path(), DM).is_err());
    std::fs::write(&cfg, r#"[{"name":"A","areas":["board"]},{"name":"B","areas":["board"]}]"#).unwrap();
    assert!(slack::load_bots(&cfg, dir.path(), DM).is_err());
}
