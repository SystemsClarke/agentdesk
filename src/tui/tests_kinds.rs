//! Question kinds in the window (docs/question-types.md): the tag in the list, a packet worked with keys, a permission rule decided
//! in the reader, a run question that shows its script and waits.

use super::tests::{key, press, screen};
use super::*;

fn app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.init().unwrap();
    (dir, App::with_db(db))
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

fn caller() -> crate::identity::Caller {
    crate::identity::Caller { session_id: Some("s1".into()), env_author: None, cwd: Some("/work".into()), harness: Some("claude-code".into()), pid: 1 }
}

fn ask(a: &App, args: Value) -> i64 {
    let r: Value = serde_json::from_str(&crate::tools::call(&a.db, &caller(), "ask_human", &args)).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    r["thread_id"].as_i64().unwrap()
}

fn propose(a: &App, d: &std::path::Path, rule: &str) -> i64 {
    let r: Value = serde_json::from_str(&crate::tools::call(
        &a.db,
        &caller(),
        "propose_permission_rule",
        &json!({"rule": rule, "reason": "format", "author": "builder", "scope": "project", "project_dir": d.to_string_lossy()}),
    ))
    .unwrap();
    r["thread_id"].as_i64().unwrap()
}

fn packet_args() -> Value {
    json!({"subject": "Two GoCD runs", "body": "Two runs are waiting on you.", "author": "builder", "meta": {"type": "packet", "packet": {
        "outcome": "Done when both runs have finished and the evidence is on the ticket.",
        "why": "Only John can trigger GoCD.",
        "happened": ["The diagnostic PR merged Thursday."],
        "found": ["https://github.com/o/r/pull/3"],
        "steps": [
            {"id": "run", "title": "Start a new run", "detail": "Expect: Symbols share space OK"},
            {"id": "how", "title": "Pick how to proceed", "input": "choice", "options": ["I run it", "agent retries"]},
            {"id": "note", "title": "Anything the agent should know", "input": "note"}
        ],
        "next": "I read both consoles.",
        "deadline": {"date": "2999-01-01", "reason": "Closes the sprint item."}
    }}})
}

fn status_of(a: &App, tid: i64) -> String {
    a.db.get_thread(tid).unwrap()["thread"]["status"].as_str().unwrap().to_string()
}

fn last_message(a: &App, tid: i64) -> Value {
    a.db.get_thread(tid).unwrap()["messages"].as_array().unwrap().last().unwrap().clone()
}

#[test]
fn the_questions_list_tags_each_kind_and_shows_a_packets_progress() {
    let _env = crate::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (d, mut a) = app();
    std::env::set_var("AGENTDESK_DATA", d.path());
    a.db.start_thread("question", "Plain question", "builder", "agent", "Ship it?", None, None).unwrap();
    ask(&a, json!({"subject": "Run this", "body": "Please run it.\n```sh\necho hi\n```", "author": "builder", "meta": {"type": "run"}}));
    let pk = ask(&a, packet_args());
    propose(&a, d.path(), "Bash(cargo fmt:*)");
    a.reload();
    press(&mut a, 'q');
    let s = screen(&mut a, 130, 24);
    println!("{s}");
    assert!(s.contains("KIND SUBJECT"), "{s}");
    for want in ["ASK  Plain question", "RUN  Run this", "PKT  [0/3] Two GoCD runs", "PERM permission rule: Bash(cargo fmt:*)"] {
        assert!(s.contains(want), "'{want}' not in:\n{s}");
    }
    crate::kinds::step(&a.db, pk, "run", true, None, None).unwrap();
    a.reload();
    assert!(screen(&mut a, 130, 24).contains("PKT  [1/3] Two GoCD runs"));
    a.on_key(key(KeyCode::Esc));
    press(&mut a, 'd');
    assert!(!screen(&mut a, 130, 24).contains("KIND"), "only the Questions list has the column");
}

#[test]
fn a_packet_is_worked_from_the_reader_with_its_keys_and_finished_by_john() {
    let (_d, mut a) = app();
    let pk = ask(&a, packet_args());
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    let s = screen(&mut a, 120, 48);
    println!("{s}");
    for want in [
        "Kind: PKT",
        "PACKET",
        "0 of 3 steps",
        "due 2999-01-01",
        "▶ [ ] Start a new run",
        "Expect: Symbols share space OK",
        "(I run it / agent retries)",
        "Alt+T tick",
        "Done when both runs",
    ] {
        assert!(s.contains(want), "'{want}' not in:\n{s}");
    }
    assert!(!s.contains("### Your steps") && !s.contains("<!--"), "the steps are drawn live, not twice");

    for c in "started it at 14:05".chars() {
        press(&mut a, c);
    }
    a.on_key(alt('t'));
    assert_eq!(status_of(&a, pk), "open", "a tick does not close it");
    let s = screen(&mut a, 120, 48);
    assert!(s.contains("1 of 3 steps") && s.contains("▶ [x] Start a new run") && s.contains("> started it at 14:05"), "{s}");
    assert!(a.reply.is_blank(), "the reply box became the note");

    a.on_key(alt('t'));
    assert!(screen(&mut a, 120, 48).contains("0 of 3 steps"), "Alt+T again un-ticks");
    a.on_key(alt('t'));

    a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
    a.on_key(alt('t'));
    assert!(a.flash.as_ref().is_some_and(|f| f.0.contains("pick one of")), "a choice cannot be ticked without a value: {:?}", a.flash);
    a.on_key(alt('v'));
    let s = screen(&mut a, 120, 48);
    assert!(s.contains("[x] Pick how to proceed") && s.contains("→ I run it") && s.contains("2 of 3 steps"), "{s}");
    a.on_key(alt('v'));
    assert!(screen(&mut a, 120, 48).contains("→ agent retries"), "Alt+V goes to the next option");

    a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
    a.on_key(alt('t'));
    assert!(screen(&mut a, 120, 48).contains("3 of 3 steps"));
    a.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    let s = screen(&mut a, 120, 48);
    assert!(s.contains("▶ [x] Pick how to proceed"), "Alt+Up moves back:\n{s}");

    a.on_key(alt('d'));
    assert!(a.confirm.as_ref().is_some_and(|c| c.0.contains("Every step is ticked")));
    assert_eq!(status_of(&a, pk), "open", "it asks first");
    press(&mut a, 'y');
    assert_eq!(status_of(&a, pk), "answered");
    assert_eq!(last_message(&a, pk)["body"], "Done: every step is ticked.");
}

#[test]
fn what_john_types_on_a_packet_is_a_note_and_leaves_it_open() {
    let (_d, mut a) = app();
    let pk = ask(&a, packet_args());
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    for c in "the second host is down".chars() {
        press(&mut a, c);
    }
    a.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(status_of(&a, pk), "open", "a reply on a packet is a note, not the answer");
    let last = last_message(&a, pk);
    assert_eq!((last["author"].as_str().unwrap(), last["body"].as_str().unwrap()), ("john", "the second host is down"));
    assert!(a.flash.as_ref().is_some_and(|f| f.0.contains("stays open")));
}

#[test]
fn a_permission_question_is_approved_from_the_reader() {
    let _env = crate::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (d, mut a) = app();
    std::env::set_var("AGENTDESK_DATA", d.path());
    let tid = propose(&a, d.path(), "Bash(cargo fmt:*)");
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    let s = screen(&mut a, 120, 40);
    println!("{s}");
    assert!(s.contains("Kind: PERM") && s.contains("PERMISSION") && s.contains("waiting for you") && s.contains("Alt+A approves"), "{s}");
    a.on_key(alt('a'));
    assert!(a.confirm.as_ref().is_some_and(|c| c.0.contains("Allow Bash(cargo fmt:*)")));
    assert!(!d.path().join(".claude/settings.json").exists(), "it asks first");
    press(&mut a, 'y');
    assert!(std::fs::read_to_string(d.path().join(".claude/settings.json")).unwrap().contains("Bash(cargo fmt:*)"));
    assert_eq!(status_of(&a, tid), "answered");
    assert!(screen(&mut a, 120, 40).contains("approved"));
}

#[test]
fn a_permission_question_is_rejected_from_the_reader_and_the_screen_is_gone() {
    let _env = crate::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
    let (d, mut a) = app();
    std::env::set_var("AGENTDESK_DATA", d.path());
    let tid = propose(&a, d.path(), "Bash(cargo clippy:*)");
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    a.on_key(alt('d'));
    assert!(a.confirm.as_ref().is_some_and(|c| c.0.contains("Reject Bash(cargo clippy:*)")));
    press(&mut a, 'y');
    assert!(!d.path().join(".claude/settings.json").exists(), "a rejection writes nothing");
    assert_eq!(status_of(&a, tid), "answered");
    a.on_key(key(KeyCode::Esc));
    a.on_key(key(KeyCode::Esc));
    press(&mut a, 'x');
    assert!(!screen(&mut a, 110, 30).contains("PERMISSION RULES"), "there is no separate Permission rules screen");
}

#[test]
fn a_run_question_shows_its_script_at_once_and_does_not_run_it() {
    let (_d, mut a) = app();
    ask(&a, json!({"subject": "Run this", "body": "Please run it and tell me what you see.\n```sh\necho hello-from-run\n```", "author": "builder", "meta": {"type": "run"}}));
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    assert!(a.runner.as_ref().is_some_and(|r| r.state == runner::State::Ready), "the pane is open and nothing has run");
    let s = screen(&mut a, 120, 40);
    assert!(s.contains("Kind: RUN") && s.contains("shown, not run") && s.contains("> echo hello-from-run") && s.contains("Alt+R runs it"), "{s}");
    a.on_key(key(KeyCode::Esc));
    let _ = screen(&mut a, 120, 40); // the pane closes on the next draw
    assert!(a.runner.is_none(), "leaving the reader closes the pane");
}

fn script(word: &str) -> String {
    let lang = if cfg!(windows) { "cmd" } else { "sh" };
    format!("Please run this and tell me what you see.\n```{lang}\necho {word}\n```")
}

fn wait_for_runner(a: &mut App) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while a.runner.as_ref().is_some_and(|r| r.state == runner::State::Running) && std::time::Instant::now() < end {
        a.tick();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    a.tick();
}

#[test]
fn a_run_question_sends_its_result_to_the_agent_when_the_script_finishes() {
    let (_d, mut a) = app();
    let tid = ask(&a, json!({"subject": "Run this", "body": script("from-the-run"), "author": "builder", "meta": {"type": "run"}}));
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    assert_eq!(status_of(&a, tid), "open");
    a.on_key(alt('r')); // the pane is already open with the script shown: one press runs it
    wait_for_runner(&mut a);
    assert_eq!(status_of(&a, tid), "answered", "the result went back by itself");
    let last = last_message(&a, tid);
    let body = last["body"].as_str().unwrap();
    assert_eq!(last["author"], "john");
    assert!(body.contains("it finished (exit 0)") && body.contains("from-the-run"), "{body}");
    assert!(a.flash.as_ref().is_some_and(|f| f.0.contains("Result sent")), "{:?}", a.flash);
    let count = a.db.get_thread(tid).unwrap()["messages"].as_array().unwrap().len();
    for _ in 0..5 {
        a.tick();
    }
    assert_eq!(a.db.get_thread(tid).unwrap()["messages"].as_array().unwrap().len(), count, "a finished script reports once");
}

#[test]
fn a_question_with_a_script_says_so_and_does_not_send_the_output_by_itself() {
    let (_d, mut a) = app();
    let tid = ask(&a, json!({"subject": "A script for you", "body": format!("{} ok?", script("from-an-ask")), "author": "builder"}));
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    let s = screen(&mut a, 120, 40);
    assert!(s.contains("▶ RUN") && s.contains("a script is in this thread") && s.contains("Alt+R"), "{s}");
    a.on_key(alt('r'));
    assert!(!screen(&mut a, 120, 40).contains("a script is in this thread"), "the hint goes once the pane is open");
    a.on_key(alt('r'));
    wait_for_runner(&mut a);
    assert_eq!(status_of(&a, tid), "open", "on an ask the output stays John's to send");
}

#[test]
fn a_command_inside_a_packet_step_can_be_run_from_the_reader() {
    let (_d, mut a) = app();
    let lang = if cfg!(windows) { "cmd" } else { "sh" };
    let mut args = packet_args();
    args["meta"]["packet"]["steps"][0]["detail"] = json!(format!("Run this:\n```{lang}\necho from-a-packet-step\n```"));
    let pk = ask(&a, args);
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    let s = screen(&mut a, 120, 50);
    assert!(s.contains("▶ RUN") && s.contains("commands in the steps"), "the packet says a command can be run: {s}");
    assert!(!s.contains("echo from-a-packet-step\n```"), "the steps are still drawn live, once");
    a.on_key(alt('r'));
    let s = screen(&mut a, 120, 50);
    assert!(s.contains("Script 1/1") && s.contains("> echo from-a-packet-step"), "the command from the step is shown, not run: {s}");
    assert!(!screen(&mut a, 120, 50).contains("commands in the steps"), "the hint goes once the pane is open");
    a.on_key(alt('r'));
    wait_for_runner(&mut a);
    let s = screen(&mut a, 120, 50);
    assert!(s.contains("from-a-packet-step") && s.contains("done (exit 0)"), "{s}");
    assert_eq!(status_of(&a, pk), "open", "a command run inside a packet does not close it or send anything");
}

#[test]
fn the_packet_key_hint_fits_the_default_window_width() {
    let (_d, mut a) = app();
    ask(&a, packet_args());
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    let s = screen(&mut a, 110, 40);
    let hint = s.lines().find(|l| l.contains("Alt+T tick")).unwrap_or_else(|| panic!("no hint line in:\n{s}"));
    assert!(hint.trim_end().ends_with("Alt+D finish"), "the hint is cut off at 110 columns: '{hint}'");
}

#[test]
fn the_reader_key_legend_follows_the_kind() {
    let (_d, mut a) = app();
    ask(&a, packet_args());
    a.db.start_thread("question", "Plain question", "builder", "agent", "Ship it?", None, None).unwrap();
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    let s = screen(&mut a, 160, 30);
    let opened = s.contains("PACKET");
    if opened {
        assert!(s.lines().any(|l| l.contains("Alt+T") && l.contains("tick") && l.contains("finish")), "{s}");
        a.on_key(key(KeyCode::Esc));
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Enter));
    }
    let s = screen(&mut a, 160, 30);
    assert!(s.contains("Plain question") && !s.lines().any(|l| l.contains("Alt+T") && l.contains("tick")), "a plain question has no packet keys: {s}");
}
