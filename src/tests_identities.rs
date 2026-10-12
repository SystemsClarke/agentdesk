//! Identities, the session host and the supervisor, against a stand-in for `claude` that just stays up.

use crate::db::{Db, AGENT};
use crate::hooks;
use crate::identities as ids;
use crate::identity::Caller;
use crate::supervisor::Supervisor;
use crate::tests::{call, ENV};
use serde_json::{json, Value};

/// Ignores its arguments and stays up, so the supervisor has a real process to start, watch and end.
fn standin_claude(dir: &std::path::Path) -> std::path::PathBuf {
    if cfg!(windows) {
        let p = dir.join("claude-standin.cmd");
        std::fs::write(&p, "@echo off\r\nping -n 120 127.0.0.1 >nul\r\n").unwrap();
        p
    } else {
        let p = dir.join("claude-standin.sh");
        std::fs::write(&p, "#!/bin/sh\nsleep 120\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

/// A scratch data folder, projects folder and stand-in claude, with the environment pointed at them. Hold the guard for the test.
pub(crate) struct Env {
    _guard: std::sync::MutexGuard<'static, ()>,
    pub(crate) dir: tempfile::TempDir,
    pub(crate) db: Db,
}

impl Env {
    pub(crate) fn new() -> Env {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects")).unwrap();
        std::env::set_var("AGENTDESK_DATA", dir.path().join("data"));
        std::env::set_var("AGENTDESK_CLAUDE_PROJECTS", dir.path().join("projects"));
        std::env::set_var("AGENTDESK_CLAUDE", standin_claude(dir.path()));
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.init().unwrap();
        Env { _guard: guard, dir, db }
    }
    pub(crate) fn folder(&self) -> &str {
        self.dir.path().to_str().unwrap()
    }
    #[allow(dead_code)]
    pub(crate) fn data(&self) -> std::path::PathBuf {
        self.dir.path().join("data")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for k in ["AGENTDESK_DATA", "AGENTDESK_CLAUDE_PROJECTS", "AGENTDESK_CLAUDE", "AGENTDESK_IDENTITY"] {
            std::env::remove_var(k);
        }
    }
}

fn args_of(row: &Value, prompt: Option<&str>) -> Vec<String> {
    ids::launch_args(row, prompt, None).0
}

#[test]
fn identities_are_created_asked_to_run_and_stopped_by_writing_the_desired_state() {
    let e = Env::new();
    assert!(ids::create(&e.db, "bad name", e.folder(), None, None, false).is_err());
    assert!(ids::create(&e.db, "alpha", "/no/such/folder", None, None, false).is_err());
    assert!(ids::create(&e.db, "alpha", e.folder(), None, Some("gpt"), false).is_err());
    let row = ids::create(&e.db, "alpha", e.folder(), Some("be brief"), Some("Opus"), false).unwrap();
    assert_eq!((row["state"].as_str(), row["model"].as_str()), (Some("stopped"), Some("opus")));
    assert!(ids::create(&e.db, "ALPHA", e.folder(), None, None, false).is_err(), "names are case-insensitive");
    assert_eq!(ids::start(&e.db, "alpha").unwrap()["state"], "queued");
    assert_eq!(ids::stop(&e.db, "alpha").unwrap()["state"], "stopped");
    assert!(ids::retire(&e.db, "alpha").is_err(), "only a running identity can retire");
    ids::forget(&e.db, "alpha").unwrap();
    assert!(ids::get(&e.db, "alpha").unwrap().is_none());
}

#[test]
fn launch_args_resume_only_a_conversation_claude_has_written_and_carry_the_charter() {
    let e = Env::new();
    ids::create(&e.db, "alpha", e.folder(), Some("be brief"), Some("opus"), false).unwrap();
    let mut row = ids::get(&e.db, "alpha").unwrap().unwrap();
    let a = args_of(&row, Some("do the thing"));
    assert_eq!(a[0], "--session-id");
    assert!(a.windows(2).any(|w| w == ["--model", "opus"]));
    assert!(a.iter().any(|x| x == "--add-dir"), "the state folder, so the session may edit its state file");
    let charter = &a[a.iter().position(|x| x == "--append-system-prompt").unwrap() + 1];
    assert!(charter.starts_with("You are one generation of alpha, a long-lived AgentDesk agent."));
    assert!(charter.contains("Your state file is ") && charter.ends_with("be brief"));
    assert_eq!(a.last().unwrap(), "do the thing", "the prompt is the first message");

    // A session id with no transcript is still new; with one, it is resumed.
    let id = "11111111-2222-4333-8444-555555555555";
    row["claude_session_id"] = json!(id);
    assert_eq!(args_of(&row, None)[0], "--session-id");
    std::fs::create_dir_all(e.dir.path().join("projects").join("proj")).unwrap();
    std::fs::write(e.dir.path().join("projects").join("proj").join(format!("{id}.jsonl")), "{}\n").unwrap();
    let a = args_of(&row, None);
    assert_eq!((a[0].as_str(), a[1].as_str()), ("--resume", id));
}

#[test]
fn a_successor_gets_the_handoff_and_the_state_file_verbatim_cut_at_the_limit() {
    let e = Env::new();
    let path = ids::state_path("keeper");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "DECISIONS\n- use the queue, because the lock was held for 30 s").unwrap();
    let p = ids::successor_prompt("keeper", 2, "Owns the parser.");
    assert!(p.starts_with("You are keeper, generation 2. Your previous generation handed off with:\n\nOwns the parser."));
    assert!(p.contains("exactly as your previous generation left it") && p.contains("use the queue, because the lock was held for 30 s"));

    std::fs::write(&path, format!("{}TAIL-MARKER", "x".repeat(5_000))).unwrap();
    std::fs::write(e.data().join("settings.json"), r#"{"phoenix_state_chars": 1000}"#).unwrap();
    let cut = ids::successor_prompt("keeper", 2, "h");
    assert!(cut.contains("cut here") && !cut.contains("TAIL-MARKER") && cut.contains(&"x".repeat(1000)) && !cut.contains(&"x".repeat(1001)));

    std::fs::write(e.data().join("settings.json"), r#"{"phoenix_state_file": false}"#).unwrap();
    assert!(!ids::successor_prompt("keeper", 2, "h").contains("state file"), "switched off, there is none");
}

#[test]
fn the_supervisor_starts_queued_identities_ends_stopped_ones_and_resumes_after_a_restart() {
    let e = Env::new();
    ids::create(&e.db, "alpha", e.folder(), None, None, false).unwrap();
    ids::start(&e.db, "alpha").unwrap();
    let mut sup = Supervisor::new();
    sup.tick(&e.db).unwrap();
    let row = ids::get(&e.db, "alpha").unwrap().unwrap();
    assert_eq!(row["state"], "running");
    assert!(row["pid"].as_i64().unwrap_or(0) > 0 && row["claude_session_id"].as_str().is_some());
    let cmd = sup.host().command("alpha").unwrap().to_vec();
    assert!(cmd.iter().any(|a| a == "--session-id") && cmd.iter().any(|a| a == "--append-system-prompt"));
    assert!(crate::supervisor::running(), "the lock is held while a supervisor is up");

    ids::stop(&e.db, "alpha").unwrap(); // the CLI, the window or `retire` only write the state; the supervisor acts
    sup.tick(&e.db).unwrap();
    assert!(sup.host().names().is_empty(), "the session was ended");

    // A supervisor that dies leaves `running` rows; the next one resumes them.
    ids::start(&e.db, "alpha").unwrap();
    sup.tick(&e.db).unwrap();
    sup.shutdown();
    drop(sup);
    assert_eq!(ids::get(&e.db, "alpha").unwrap().unwrap()["state"], "running", "the board still says running");
    let mut next = Supervisor::new();
    next.tick(&e.db).unwrap();
    assert_eq!(next.host().names(), vec!["alpha".to_string()], "the new supervisor resumed it");
    next.shutdown();
}

#[test]
fn a_handoff_restarts_the_identity_from_it_once_the_turn_ends() {
    let e = Env::new();
    ids::create(&e.db, "phx", e.folder(), None, None, false).unwrap();
    ids::start(&e.db, "phx").unwrap();
    let mut sup = Supervisor::new();
    sup.tick(&e.db).unwrap();
    let sid = ids::get(&e.db, "phx").unwrap().unwrap()["claude_session_id"].as_str().unwrap().to_string();

    // Inside the session: AGENTDESK_IDENTITY and AGENTDESK_AUTHOR are stamped, and it calls pass_the_torch.
    std::env::set_var("AGENTDESK_IDENTITY", "phx");
    let me = Caller { session_id: Some(sid.clone()), env_author: Some("phx".into()), cwd: None, harness: Some("claude-code".into()), pid: 1 };
    call(&e.db, &me, "pass_the_torch", json!({"handoff": "Owns the parser. Next: its tests."}));
    sup.tick(&e.db).unwrap();
    assert_eq!(ids::get(&e.db, "phx").unwrap().unwrap()["generation"], 1, "not before its turn ends");
    assert_eq!(hooks::run(&e.db, "stop", &json!({"session_id": sid, "stop_hook_active": false})), "");
    sup.tick(&e.db).unwrap();

    let row = ids::get(&e.db, "phx").unwrap().unwrap();
    assert_eq!((row["generation"].as_i64(), row["state"].as_str()), (Some(2), Some("running")));
    let next = row["claude_session_id"].as_str().unwrap().to_string();
    assert_ne!(next, sid, "a successor is a new conversation");
    let cmd = sup.host().command("phx").unwrap().to_vec();
    assert_eq!(cmd.last().unwrap(), "You are phx, generation 2. Your previous generation handed off with:\n\nOwns the parser. Next: its tests.");
    let chain = e.db.row("SELECT * FROM phoenix_chain", &[]).unwrap().unwrap();
    assert_eq!((chain["identity"].as_str(), chain["generation"].as_i64(), chain["claude_session_id"].as_str()), (Some("phx"), Some(1), Some(sid.as_str())));
    assert!(e.db.row("SELECT 1 FROM messages WHERE body LIKE 'generation 2 started from handoff #%'", &[]).unwrap().is_some());

    // The successor hands off straight away: within the cooldown nothing happens.
    let again = Caller { session_id: Some(next.clone()), ..me };
    call(&e.db, &again, "pass_the_torch", json!({"handoff": "Again, already."}));
    hooks::run(&e.db, "stop", &json!({"session_id": next, "stop_hook_active": false}));
    sup.tick(&e.db).unwrap();
    assert_eq!(ids::get(&e.db, "phx").unwrap().unwrap()["generation"], 2);
    sup.shutdown();
}

#[test]
fn a_launch_that_never_happened_still_gives_the_successor_its_handoff() {
    let e = Env::new();
    ids::create(&e.db, "crashy", e.folder(), None, None, false).unwrap();
    e.db.start_thread("discussion", "bio: crashy", "crashy", AGENT, "**Handoff recorded** (ts).\n\nOwns the lexer. Next: fuzz it.", None, None).unwrap();
    let mid = e.db.scalar_i64("SELECT MAX(id) FROM messages", &[]).unwrap().unwrap();
    // Phoenix's bookkeeping done, the launch not: the state the supervisor leaves when it dies in between.
    e.db.exec("INSERT INTO phoenix_chain (identity, generation, claude_session_id, handoff_msg, ts) VALUES ('crashy',1,'old',:m,'2026-10-12T00:00:00+00:00')", &[(":m", &mid)])
        .unwrap();
    e.db.exec("UPDATE identities SET state='queued', claude_session_id=NULL, generation=2 WHERE name='crashy'", &[]).unwrap();
    let mut sup = Supervisor::new();
    sup.tick(&e.db).unwrap();
    let cmd = sup.host().command("crashy").unwrap().to_vec();
    assert_eq!(cmd.last().unwrap(), "You are crashy, generation 2. Your previous generation handed off with:\n\nOwns the lexer. Next: fuzz it.");
    sup.shutdown();
}

#[test]
fn a_reply_from_john_starts_the_stopped_identity_that_asked_with_it() {
    let e = Env::new();
    ids::create(&e.db, "asker", e.folder(), None, None, false).unwrap();
    let mut sup = Supervisor::new();
    sup.tick(&e.db).unwrap(); // the supervisor is up and knows the newest reply so far
    let tid = e.db.start_thread("question", "Ship it?", "asker", AGENT, "Ship it?", None, None).unwrap();
    e.db.john_replies(tid, "yes, ship it").unwrap();
    e.db.exec("UPDATE messages SET ts='2020-01-01T00:00:00+00:00' WHERE author_kind='human'", &[]).unwrap(); // older than the 5-second grace
    sup.tick(&e.db).unwrap();
    assert_eq!(ids::get(&e.db, "asker").unwrap().unwrap()["state"], "running", "woken, not left stopped");
    let cmd = sup.host().command("asker").unwrap().to_vec();
    assert!(cmd.last().unwrap().contains("John replied on AgentDesk thread") && cmd.last().unwrap().contains("yes, ship it"));
    assert_eq!(e.db.row("SELECT state FROM deliveries WHERE method='wake'", &[]).unwrap().unwrap()["state"], "resumed");
    sup.shutdown();
}

#[test]
fn attach_replays_output_takes_input_and_refuses_a_wrong_key() {
    use std::io::{BufRead, BufReader, Read, Write};
    let e = Env::new();
    let mut sup = Supervisor::new();
    assert!(sup.acquire());
    let (program, args): (&str, Vec<String>) =
        if cfg!(windows) { ("cmd", vec!["/c".into(), "echo hello-attach& ping -n 30 127.0.0.1 >nul".into()]) } else { ("sh", vec!["-c".into(), "echo hello-attach; cat".into()]) };
    sup.host().launch("att", e.folder(), program, &args, &[]).unwrap();
    let (port, key) = crate::attach::read_addr().expect("supervisor.addr is written");
    let connect = |line: String| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        c.write_all(line.as_bytes()).unwrap();
        c
    };
    let mut bad = BufReader::new(connect("ATTACH wrong att 80 24\n".to_string()));
    let mut l = String::new();
    bad.read_line(&mut l).unwrap();
    assert!(l.starts_with("ERR"), "{l}");
    let mut none = BufReader::new(connect(format!("ATTACH {key} nobody 80 24\n")));
    l.clear();
    none.read_line(&mut l).unwrap();
    assert!(l.starts_with("ERR") && l.contains("not running"), "{l}");

    let mut c = connect(format!("ATTACH {key} att 100 30\n"));
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let mut answered = false;
    while !String::from_utf8_lossy(&seen).contains("hello-attach") {
        let n = c.read(&mut buf).unwrap_or_else(|e| panic!("{e}; so far {:?}", String::from_utf8_lossy(&seen)));
        assert!(n > 0, "closed before the output arrived: {:?}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&buf[..n]);
        if !answered && String::from_utf8_lossy(&seen).contains("\x1b[6n") {
            // a terminal answers the cursor query ConPTY opens with; the host only does it for a session nobody is attached to
            answered = true;
            c.write_all(format!("INPUT {}\n", crate::attach::base64_encode(b"\x1b[1;1R")).as_bytes()).unwrap();
        }
    }
    assert!(String::from_utf8_lossy(&seen).starts_with("OK\n"));
    if !cfg!(windows) {
        // cat echoes what is typed: input reaches the session, and its output comes back down
        c.write_all(format!("INPUT {}\nRESIZE 90 20\n", crate::attach::base64_encode(b"ping-from-client\r")).as_bytes()).unwrap();
        while !String::from_utf8_lossy(&seen).contains("ping-from-client") {
            let n = c.read(&mut buf).unwrap();
            assert!(n > 0);
            seen.extend_from_slice(&buf[..n]);
        }
    }
    sup.shutdown();
    assert!(crate::attach::read_addr().is_none(), "supervisor.addr is removed");
}

#[test]
fn base64_round_trips() {
    for s in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"\x00\xff\x10\r\n"] {
        assert_eq!(crate::attach::base64_decode(&crate::attach::base64_encode(s)).unwrap(), s);
    }
    assert_eq!(crate::attach::base64_encode(b"Man"), "TWFu");
}
