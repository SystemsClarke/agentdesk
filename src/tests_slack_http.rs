//! The Slack bridge over real sockets: the Web API client against a tiny HTTP server in this test (`SLACK_API_URL` points it there), the
//! bridge's question relay and swarm slots through that client, and Socket Mode against a local websocket. Nothing here reaches Slack.

use crate::db::{Db, AGENT};
use crate::slack::{self, Api, Bridge, Web};
use crate::swarm::Swarms;
use crate::tests_identities::Env;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    auth: String,
    ctype: String,
    body: String,
}

type Reply = (u16, Vec<(&'static str, String)>, Vec<u8>);

fn ok(v: Value) -> Reply {
    (200, vec![], v.to_string().into_bytes())
}

/// A Web API in this process: every request is recorded, and `handler` (given the request and how many came before it) answers.
struct FakeSlack {
    port: u16,
    seen: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
}

impl FakeSlack {
    fn start(handler: impl Fn(&Req, usize) -> Reply + Send + 'static) -> FakeSlack {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen, stop) = (Arc::new(Mutex::new(Vec::<Req>::new())), Arc::new(AtomicBool::new(false)));
        let (s, st) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        conn.set_nonblocking(false).unwrap();
                        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let Some(req) = read_request(&mut conn) else { continue };
                        let n = {
                            let mut g = s.lock().unwrap();
                            g.push(req.clone());
                            g.len() - 1
                        };
                        let (status, headers, body) = handler(&req, n);
                        let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                            head.push_str("Content-Type: application/json\r\n");
                        }
                        for (k, v) in headers {
                            head.push_str(&format!("{k}: {v}\r\n"));
                        }
                        let _ = conn.write_all(head.as_bytes());
                        let _ = conn.write_all(b"\r\n");
                        let _ = conn.write_all(&body);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        FakeSlack { port, seen, stop }
    }

    fn api(&self) -> String {
        format!("http://127.0.0.1:{}/api", self.port)
    }

    fn seen(&self) -> Vec<Req> {
        self.seen.lock().unwrap().clone()
    }

    fn calls(&self, method: &str) -> Vec<Req> {
        self.seen().into_iter().filter(|r| r.path == format!("/api/{method}")).collect()
    }
}

impl Drop for FakeSlack {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn read_request(conn: &mut TcpStream) -> Option<Req> {
    let mut buf = vec![];
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if conn.read(&mut byte).ok()? == 0 {
            return None;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let header = |name: &str| lines.clone().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string())).unwrap_or_default();
    let len: usize = header("content-length").parse().unwrap_or(0);
    let mut body = vec![0u8; len];
    conn.read_exact(&mut body).ok()?;
    Some(Req { method, path, auth: header("authorization"), ctype: header("content-type"), body: String::from_utf8_lossy(&body).to_string() })
}

/// `SLACK_API_URL` for the length of a test (the `Env` guard serialises tests that touch the environment).
struct ApiUrl;

impl ApiUrl {
    fn set(url: &str) -> ApiUrl {
        std::env::set_var("SLACK_API_URL", url);
        ApiUrl
    }
}

impl Drop for ApiUrl {
    fn drop(&mut self) {
        for k in ["SLACK_API_URL", "SLACK_BOT_TOKEN", "SLACK_APP_TOKEN", "AGENTDESK_SLACK_DM", "AGENTDESK_SLACK_USER", "AGENTDESK_SLACK_DIR"] {
            std::env::remove_var(k);
        }
    }
}

fn form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (crate::links::percent_decode(k), crate::links::percent_decode(v))
        })
        .collect()
}

fn field(body: &str, key: &str) -> String {
    form(body).into_iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or_default()
}

#[test]
fn the_web_client_speaks_json_for_chat_and_a_form_for_the_rest_and_reads_slacks_errors() {
    let e = Env::new();
    let fake = FakeSlack::start(|req, _| match req.path.as_str() {
        "/api/chat.postMessage" => ok(json!({"ok": true, "ts": "1.5"})),
        "/api/conversations.rename" => ok(json!({"ok": false, "error": "channel_not_found"})),
        "/api/conversations.history" => ok(json!({"ok": true, "messages": []})),
        "/api/boom" => (500, vec![], b"oops".to_vec()),
        _ => ok(json!({"ok": true})),
    });
    let _u = ApiUrl::set(&fake.api());
    let _ = &e;
    let web = Web::new(" xoxb-tok \n");
    let ts = web.api("chat.postMessage", &json!({"channel": "D1", "text": "hi", "blocks": [{"type": "divider"}]})).unwrap()["ts"].clone();
    assert_eq!(ts, "1.5");
    let post = &fake.calls("chat.postMessage")[0];
    assert_eq!((post.method.as_str(), post.auth.as_str()), ("POST", "Bearer xoxb-tok"), "the token is trimmed");
    assert!(post.ctype.starts_with("application/json"), "{}", post.ctype);
    let sent: Value = serde_json::from_str(&post.body).unwrap();
    assert_eq!((sent["channel"].clone(), sent["blocks"][0]["type"].clone()), (json!("D1"), json!("divider")));
    // the rest of the Web API is a form, and an `ok: false` is a SlackError carrying Slack's code
    let err = web.api("conversations.rename", &json!({"channel": "C1", "name": "swarm a&b", "is_private": false})).unwrap_err();
    assert_eq!(slack::slack_error(&err), Some("channel_not_found"));
    let r = &fake.calls("conversations.rename")[0];
    assert!(r.ctype.starts_with("application/x-www-form-urlencoded"), "{}", r.ctype);
    assert_eq!((field(&r.body, "channel"), field(&r.body, "name"), field(&r.body, "is_private")), ("C1".into(), "swarm a&b".into(), "false".into()));
    // anything but 200 is an error that is not Slack's
    let err = web.api("boom", &json!({})).unwrap_err();
    assert!(slack::slack_error(&err).is_none() && err.to_string().contains("boom"), "{err}");
}

#[test]
fn a_rate_limited_call_waits_the_retry_after_and_goes_again_once() {
    let e = Env::new();
    let fake = FakeSlack::start(|_, n| if n == 0 { (429, vec![("Retry-After", "0".into())], b"{}".to_vec()) } else { ok(json!({"ok": true, "ts": "9.9"})) });
    let _u = ApiUrl::set(&fake.api());
    let _ = &e;
    assert_eq!(Web::new("t").api("chat.postMessage", &json!({"channel": "D1", "text": "x"})).unwrap()["ts"], "9.9");
    assert_eq!(fake.seen().len(), 2);
    let always = FakeSlack::start(|_, _| (429, vec![("Retry-After", "0".into())], b"{}".to_vec()));
    std::env::set_var("SLACK_API_URL", always.api());
    assert!(Web::new("t").api("chat.postMessage", &json!({})).is_err());
    assert_eq!(always.seen().len(), 2, "one retry, not a loop");
}

#[test]
fn a_question_goes_out_and_a_photo_reply_comes_back_through_the_web_api() {
    let e = Env::new();
    let png = b"\x89PNG\r\n\x1a\nfake".to_vec();
    let served = png.clone();
    let fake = FakeSlack::start(move |req, _| {
        if req.path.starts_with("/files/") {
            return (200, vec![("Content-Type", "image/png".into())], served.clone());
        }
        ok(json!({"ok": true, "ts": "100.001"}))
    });
    let _u = ApiUrl::set(&fake.api());
    let web: Arc<Web> = Arc::new(Web::new("xoxb-real"));
    let bridge = Bridge::new(web, "D1", "U1", e.dir.path().join("bridge_state.json"), e.dir.path().join("attachments"));
    let tid = e.db.start_thread("question", "Ship it?", "builder", AGENT, "Should we **ship**?", None, None).unwrap();
    bridge.poll(&e.db).unwrap();
    bridge.poll(&e.db).unwrap();
    let posts = fake.calls("chat.postMessage");
    assert_eq!(posts.len(), 1, "posted once");
    let body: Value = serde_json::from_str(&posts[0].body).unwrap();
    assert_eq!(body["channel"], "D1");
    assert!(body["blocks"][0]["text"]["text"].as_str().unwrap().contains(&format!("Question #{tid}")), "{body}");
    assert_eq!((body["unfurl_links"].clone(), body["thread_ts"].clone()), (json!(false), Value::Null));
    // John answers in the thread with a photo: downloaded with the bot's token, saved, linked in the answer
    let ev = json!({"channel": "D1", "channel_type": "im", "user": "U1", "thread_ts": "100.001", "text": "this one", "subtype": "file_share",
                    "files": [{"id": "F1", "name": "pic.png", "mimetype": "image/png", "url_private_download": format!("http://127.0.0.1:{}/files/pic.png", fake.port)}]});
    bridge.handle_reply(&e.db, &ev).unwrap();
    let dl = fake.seen().into_iter().find(|r| r.path == "/files/pic.png").expect("the photo was fetched");
    assert_eq!((dl.method.as_str(), dl.auth.as_str()), ("GET", "Bearer xoxb-real"));
    assert_eq!(std::fs::read(e.dir.path().join("attachments").join("100_001_F1_pic.png")).unwrap(), png);
    let t = e.db.get_thread(tid).unwrap();
    assert_eq!(t["thread"]["status"], "answered");
    let last = t["messages"].as_array().unwrap().last().unwrap().clone();
    assert!(last["body"].as_str().unwrap().starts_with("this one\n\n![photo](file:///") && last["meta"].as_str().unwrap().contains("\"via\": \"slack\""), "{last}");
    let said: Value = serde_json::from_str(&fake.calls("chat.postMessage").last().unwrap().body).unwrap();
    assert_eq!((said["thread_ts"].clone(), said["text"].as_str().unwrap().starts_with("Recorded as your answer")), (json!("100.001"), true));
}

#[test]
fn run_once_posts_the_board_to_the_dm_with_the_environments_token_and_url() {
    let e = Env::new();
    let fake = FakeSlack::start(|_, _| ok(json!({"ok": true, "ts": "5.5"})));
    let _u = ApiUrl::set(&fake.api());
    assert!(slack::unconfigured().is_some_and(|w| w.starts_with("not supervised")), "a custom data folder with no credentials never posts to John's Slack");
    std::env::set_var("SLACK_BOT_TOKEN", "xoxb-env");
    std::env::set_var("AGENTDESK_SLACK_DM", "DENV");
    assert_eq!(slack::unconfigured(), None);
    // the board `run_once` opens is the data folder's own
    let db = Db::open(&crate::paths::db_path()).unwrap();
    db.init().unwrap();
    let tid = db.start_thread("question", "Deploy?", "builder", AGENT, "Now?", None, None).unwrap();
    slack::run_once(slack::attach_input()).unwrap();
    let post = &fake.calls("chat.postMessage")[0];
    let body: Value = serde_json::from_str(&post.body).unwrap();
    assert_eq!((post.auth.as_str(), body["channel"].clone()), ("Bearer xoxb-env", json!("DENV")));
    assert!(body["text"].as_str().unwrap().starts_with(&format!("Question #{tid} from builder: Deploy?")), "{body}");
    let beat: Value = serde_json::from_str(&std::fs::read_to_string(crate::paths::data_dir().join("slack_bridge.state")).unwrap()).unwrap();
    assert_eq!((beat["pid"].clone(), beat["last_relay"]["thread_id"].clone()), (json!(std::process::id()), json!(tid)), "the heartbeat ui:status reads");
    assert!(crate::bridge::running(), "and the supervisor sees a live bridge");
    // the state file the Python bridge keeps is kept here: a second pass does not post again
    slack::run_once(slack::attach_input()).unwrap();
    assert_eq!(fake.calls("chat.postMessage").len(), 1);
    drop(e);
}

#[test]
fn swarm_posts_as_the_persona_and_falls_back_when_slack_lacks_the_customize_scope() {
    let e = Env::new();
    let fake = FakeSlack::start(|req, _| match req.path.as_str() {
        "/api/conversations.create" => ok(json!({"ok": true, "channel": {"id": "C77", "name": "swarm-alpha"}})),
        "/api/chat.postMessage" if req.body.contains("\"username\"") => ok(json!({"ok": false, "error": "missing_scope"})),
        "/api/chat.postMessage" => ok(json!({"ok": true, "ts": "7.7"})),
        _ => ok(json!({"ok": true})),
    });
    let _u = ApiUrl::set(&fake.api());
    std::env::remove_var("AGENTDESK_IDENTITY");
    let sw = Swarms::new(Arc::new(Web::new("xoxb-sw")), "U1", e.dir.path().join("swarm_state.json"), Arc::new(|_: &str, _: &str| Ok(())));
    let said = sw.new_swarm(&e.db, "alpha", e.folder(), "make it faster");
    assert!(said.starts_with("Slot 1: *alpha* in <#C77>"), "{said}");
    let posts: Vec<Value> = fake.calls("chat.postMessage").iter().map(|r| serde_json::from_str(&r.body).unwrap()).collect();
    assert_eq!(posts.len(), 2, "the persona post, refused, then a plain one");
    assert_eq!((posts[0]["username"].clone(), posts[0]["icon_emoji"].clone()), (json!("alpha"), json!(":large_blue_circle:")));
    assert!(posts[1]["username"].is_null() && posts[1]["text"].as_str().unwrap().starts_with("alpha: Proposing"), "{posts:?}");
    let invite = fake.calls("conversations.invite")[0].clone();
    assert_eq!((field(&invite.body, "channel"), field(&invite.body, "users")), ("C77".into(), "U1".into()));
    assert_eq!(fake.calls("conversations.create").len(), 1);
}

// ---- Socket Mode

#[test]
fn socket_frames_are_acknowledged_and_only_message_events_get_through() {
    let hello = json!({"type": "hello", "num_connections": 1}).to_string();
    assert_eq!(slack::parse_frame(&hello), Some(slack::Frame::default()), "no envelope, nothing to ack");
    let msg = json!({"envelope_id": "e1", "type": "events_api", "accepts_response_payload": false,
                     "payload": {"type": "event_callback", "event": {"type": "message", "text": "status", "channel_type": "im"}}});
    let f = slack::parse_frame(&msg.to_string()).unwrap();
    assert_eq!(f.ack.as_deref(), Some(r#"{"envelope_id":"e1"}"#));
    assert_eq!(f.event.unwrap()["text"], "status");
    assert!(!f.reconnect);
    let other = json!({"envelope_id": "e2", "type": "events_api", "payload": {"event": {"type": "reaction_added"}}});
    let f = slack::parse_frame(&other.to_string()).unwrap();
    assert!(f.ack.is_some() && f.event.is_none(), "every envelope is acknowledged, but only messages are handled");
    let slash = json!({"envelope_id": "e3", "type": "slash_commands", "payload": {"command": "/x"}});
    assert!(slack::parse_frame(&slash.to_string()).unwrap().ack.is_some());
    let bye = slack::parse_frame(&json!({"type": "disconnect", "reason": "link_disabled"}).to_string()).unwrap();
    assert!(bye.reconnect && bye.ack.is_none() && bye.event.is_none());
    assert!(slack::parse_frame("not json").is_none());
}

#[test]
fn socket_mode_connects_acknowledges_events_and_reconnects_after_a_disconnect() {
    let e = Env::new();
    let ws = TcpListener::bind("127.0.0.1:0").unwrap();
    let ws_port = ws.local_addr().unwrap().port();
    let acks: Arc<Mutex<Vec<String>>> = Arc::default();
    let a = acks.clone();
    let server = std::thread::spawn(move || {
        for round in 0..2 {
            let (conn, _) = ws.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut sock = tungstenite::accept(conn).unwrap();
            sock.send(tungstenite::Message::text(json!({"type": "hello"}).to_string())).unwrap();
            let env =
                json!({"envelope_id": format!("e{round}"), "type": "events_api", "payload": {"event": {"type": "message", "text": format!("m{round}"), "channel_type": "im"}}});
            sock.send(tungstenite::Message::text(env.to_string())).unwrap();
            if let Ok(tungstenite::Message::Text(t)) = sock.read() {
                a.lock().unwrap().push(t.to_string());
            }
            if round == 0 {
                sock.send(tungstenite::Message::text(json!({"type": "disconnect", "reason": "refresh_requested"}).to_string())).unwrap();
            }
            let _ = sock.close(None);
            while sock.read().is_ok() {}
        }
    });
    let opened = FakeSlack::start(move |req, _| {
        assert_eq!(req.auth, "Bearer xapp-test", "the app token opens the socket");
        ok(json!({"ok": true, "url": format!("ws://127.0.0.1:{ws_port}/link/?ticket=t")}))
    });
    let _u = ApiUrl::set(&opened.api());
    let _ = &e;
    let (tx, rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let looper = std::thread::spawn(move || slack::socket_loop("xapp-test", &s, &tx));
    let first = rx.recv_timeout(Duration::from_secs(20)).expect("the first event");
    let second = rx.recv_timeout(Duration::from_secs(20)).expect("the event after the reconnect");
    stop.store(true, Ordering::Relaxed);
    looper.join().unwrap();
    server.join().unwrap();
    assert_eq!((first["text"].clone(), second["text"].clone()), (json!("m0"), json!("m1")));
    assert_eq!(acks.lock().unwrap().clone(), vec![r#"{"envelope_id":"e0"}"#.to_string(), r#"{"envelope_id":"e1"}"#.to_string()]);
    assert_eq!(opened.calls("apps.connections.open").len(), 2, "one connection per round");
}

#[test]
fn slack_native_in_settings_decides_who_runs_the_bridge_and_state_files_make_their_folder() {
    let e = Env::new();
    let data = crate::paths::data_dir();
    std::fs::create_dir_all(&data).unwrap();
    std::env::remove_var("AGENTDESK_BRIDGE_CMD");
    // native unless a stand-in command is set (the parity suite's stub bridges rely on it); `slack_native: true` wins over the stand-in
    std::fs::write(data.join("settings.json"), "{}").unwrap();
    assert!(crate::bridge::use_native());
    std::env::set_var("AGENTDESK_BRIDGE_CMD", "stub");
    assert!(!crate::bridge::use_native());
    std::fs::write(data.join("settings.json"), r#"{"slack_native": true}"#).unwrap();
    assert!(crate::bridge::use_native());
    std::env::remove_var("AGENTDESK_BRIDGE_CMD");
    std::fs::write(data.join("settings.json"), r#"{"slack_native": false}"#).unwrap();
    assert!(crate::bridge::use_native(), "the old setting is ignored");
    // credentials from the environment mean a folder that may not exist: the first state write makes it
    let deep = e.dir.path().join("no").join("such").join("bridge_state.json");
    let b = Bridge::new(Arc::new(Web::new("t")), "D1", "U1", deep.clone(), e.dir.path().join("a"));
    b.put_for_test();
    assert!(deep.exists());
}
