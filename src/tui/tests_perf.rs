//! The window's speed, as tests: a big board must reload and draw inside a generous budget (a regression of the kind that made the
//! window laggy is tens of times over it, not a few percent), and a screen that has not changed must cost no frame.

use super::tests::{key, press, screen};
use super::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn file_app(threads: usize) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.init().unwrap();
    if threads > 0 {
        big_board(&db, threads);
    }
    (dir, App::with_db(db))
}

/// `n` threads over the four channels, each with a few replies and some Markdown, in one transaction.
fn big_board(db: &Db, n: usize) {
    // raw inserts: going through `start_thread` and `reply` for 20,000 rows is most of a debug test's time
    db.tx(|| {
        for i in 0..n {
            let ch = state::CHANNELS[i % 4];
            let ts = format!("2026-10-{:02}T{:02}:{:02}:00+00:00", 1 + i % 12, i % 24, i % 60);
            db.exec(
                "INSERT INTO threads (created_ts, updated_ts, channel, subject, opened_by, status) VALUES (:ts,:ts,:ch,:subj,:by,'open')",
                &[(":ts", &ts), (":ch", &ch), (":subj", &format!("thread {i} about the deploy of service {}", i % 37)), (":by", &format!("agent-{}", i % 27))],
            )?;
            let tid = db.scalar_i64("SELECT MAX(id) FROM threads", &[])?.unwrap_or(0);
            for r in 0..4 {
                let body = if r == 0 {
                    "Opening **message** with `code` and a [link](https://example.com).".to_string()
                } else {
                    format!("Reply {r}: the build is green.\n\n- one\n- two\n\n| a | b |\n|---|---|\n| {i} | {r} |")
                };
                db.exec(
                    "INSERT INTO messages (ts, thread_id, author, author_kind, body) VALUES (:ts,:t,:a,'agent',:b)",
                    &[(":ts", &ts), (":t", &tid), (":a", &format!("agent-{}", (i + r) % 27)), (":b", &body)],
                )?;
            }
        }
        Ok(())
    })
    .unwrap();
}

fn draw_once(a: &mut App, t: &mut Terminal<TestBackend>) -> std::time::Duration {
    let t0 = Instant::now();
    t.draw(|f| a.draw(f)).unwrap();
    t0.elapsed()
}

// Generous: the measured numbers on a 750-thread board are 1 to 80 ms in release, and a debug build is several times slower.
const RELOAD_BUDGET: Duration = Duration::from_secs(5);
const DRAW_BUDGET: Duration = Duration::from_millis(1500);

#[test]
fn a_board_of_five_thousand_threads_reloads_and_draws_inside_the_budget() {
    let (_d, mut a) = file_app(5000);
    let t0 = Instant::now();
    a.reload();
    let reload = t0.elapsed();
    assert!(reload < RELOAD_BUDGET, "reload took {reload:?}");
    // the lists are capped, not the whole board
    assert!(a.rows_of("discussion").len() <= 300 && !a.rows_of("discussion").is_empty());
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    for (key, name) in [(None, "main"), (Some('q'), "questions"), (Some('d'), "discussion"), (Some('w'), "wiki"), (Some('j'), "work")] {
        if let Some(c) = key {
            press(&mut a, c);
        }
        let took = draw_once(&mut a, &mut t);
        assert!(took < DRAW_BUDGET, "{name} drew in {took:?}");
    }
    // a thread, then moving through the list
    a.on_key(key(KeyCode::Enter));
    let took = draw_once(&mut a, &mut t);
    assert!(took < DRAW_BUDGET, "reader drew in {took:?}");
    for _ in 0..20 {
        a.on_key(key(KeyCode::PageDown));
        let took = draw_once(&mut a, &mut t);
        assert!(took < DRAW_BUDGET, "reader scroll drew in {took:?}");
    }
}

#[test]
fn a_frame_asks_nothing_of_the_machine() {
    // `bridge::running()` started `tasklist` on every frame. The top line now shows what the last refresh found, so it follows
    // `st.slack_up` and nothing else: with no heartbeat file in sight, setting it is all it takes.
    let (_d, mut a) = file_app(50);
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    for (up, shows) in [(true, "SlackNet ● up"), (false, "SlackNet ○ down")] {
        a.st.slack_up = up;
        for c in ['m', 's', 'b', 'q'] {
            press(&mut a, c);
            a.st.slack_up = up; // a screen change reloads, and with it the answer
            let s = screen(&mut a, 120, 40);
            assert!(s.lines().next().unwrap().contains(shows), "{c}: {s}");
            let took = (0..10).map(|_| draw_once(&mut a, &mut t)).max().unwrap();
            assert!(took < Duration::from_millis(250), "{c}: a frame took {took:?}");
        }
    }
}

#[test]
fn a_screen_that_has_not_changed_costs_no_frame() {
    let (_d, mut a) = file_app(20);
    a.fixed_clock = Some((7, 7));
    a.time_key = (7, 7);
    assert!(a.tick(), "the first frame is owed");
    for _ in 0..50 {
        assert!(!a.tick(), "nothing changed, so nothing is drawn");
    }
    // a key is a frame, then quiet again
    handle(&mut a, Event::Key(key(KeyCode::Char('q'))));
    assert!(a.tick());
    assert!(!a.tick());
    // events that change nothing on screen do not draw: focus, the release half of a key, a mouse move
    handle(&mut a, Event::FocusGained);
    handle(&mut a, Event::FocusLost);
    let mut release = key(KeyCode::Down);
    release.kind = KeyEventKind::Release;
    handle(&mut a, Event::Key(release));
    assert!(!a.tick());
    handle(&mut a, Event::Resize(100, 30));
    assert!(a.tick());
    // a flash message is a frame when it appears and one when it ends, and nothing in between
    a.say("hello", "ye");
    assert!(a.tick());
    assert!(!a.tick());
    a.flash.as_mut().unwrap().2 = Instant::now() - FLASH - Duration::from_millis(1);
    assert!(a.tick(), "the message ending changes the screen");
    assert!(a.flash.is_none());
    assert!(!a.tick());
    // the clock: on the minute, relative times move on
    a.fixed_clock = Some((8, 8));
    assert!(a.tick());
    assert!(!a.tick());
}

#[test]
fn the_loop_sleeps_until_something_needs_it() {
    let (_d, mut a) = file_app(0);
    assert!(a.next_wake() >= Duration::from_millis(100), "an idle window wakes a few times a second, not busily");
    a.say("soon", "ye");
    assert!(a.next_wake() <= FLASH + Duration::from_millis(10));
    a.flash.as_mut().unwrap().2 = Instant::now() - Duration::from_millis(3190);
    assert!(a.next_wake() < Duration::from_millis(50), "wakes for the end of the message");
}

#[test]
fn the_worker_brings_in_a_change_without_the_window_thread_waiting_for_it() {
    let (d, mut a) = file_app(200);
    press(&mut a, 'd');
    let before = a.rows_of("discussion").len();
    a.start_refresher(d.path().join("t.db"));
    // settle: the first answer lands, then nothing changes and nothing is drawn
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(600) {
        a.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..20 {
        assert!(!a.tick(), "an identical refresh draws nothing");
        std::thread::sleep(Duration::from_millis(5));
    }
    // another connection posts; the window sees it, and no tick waits on the load
    let other = Db::open(&d.path().join("t.db")).unwrap();
    other.start_thread("discussion", "posted from elsewhere", "agent-z", "agent", "hello", None, None).unwrap();
    let t0 = Instant::now();
    let mut worst = Duration::ZERO;
    let mut drew = false;
    while t0.elapsed() < Duration::from_secs(5) && !drew {
        let t = Instant::now();
        drew = a.tick();
        worst = worst.max(t.elapsed());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(drew, "the new thread never reached the window");
    assert!(a.rows_of("discussion").len() > before.min(299) || a.rows_of("discussion").iter().any(|r| r.subject == "posted from elsewhere"));
    assert!(worst < Duration::from_millis(250), "a tick took {worst:?}");
}

#[test]
fn an_answer_older_than_a_reload_in_place_is_dropped_and_asked_for_again() {
    // the window writes (a job toggled, a reply sent) and reloads in place; a worker answer that was asked for before that write
    // must not put the old state back on screen
    let (_d, mut a) = file_app(10);
    let (ask, asked) = mpsc::channel::<Want>();
    let (reply, answer) = mpsc::channel::<Fresh>();
    a.refresher = Some(Refresher { ask, answer, out: Some(a.epoch), started: Instant::now() - Duration::from_secs(1), stale: false, sig: 0 });
    let old = Fresh::load(&a.db, a.want(), None, None);
    a.reload(); // the write
    a.db.start_thread("discussion", "after the write", "agent-w", "agent", "x", None, None).unwrap();
    a.reload();
    let shows = a.rows_of("discussion").iter().any(|r| r.subject == "after the write");
    assert!(shows);
    let mut old = old;
    old.sig = 99;
    reply.send(old).unwrap();
    a.tick();
    assert!(a.rows_of("discussion").iter().any(|r| r.subject == "after the write"), "the old answer must not be applied");
    assert!(asked.try_recv().is_ok(), "and a fresh one is asked for at once");
}

#[test]
fn each_screen_asks_only_for_what_it_shows() {
    let (_d, mut a) = file_app(40);
    press(&mut a, 'q');
    let asked_for = a.want();
    assert!(asked_for.rows[0] && !asked_for.rows[1], "the question list, and not the discussion list");
    press(&mut a, 'm');
    assert!(a.want().rows.iter().all(|r| *r), "the main menu counts every list");
    press(&mut a, 'o');
    assert_eq!(a.want().rows, [false, false, false, true], "other screens need only the work list (the top line's held item)");
}

#[test]
fn the_reader_renders_a_long_thread_once() {
    let (_d, mut a) = file_app(0);
    let tid = a.db.start_thread("discussion", "long one", "agent-a", "agent", "start", None, None).unwrap();
    db_fill(&a.db, tid, 300);
    a.reload();
    a.open_thread(tid, Screen::List);
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let cold = draw_once(&mut a, &mut t);
    assert_eq!(a.md_cache.borrow().lines.len(), 301, "every message is cached");
    let warm = (0..5).map(|_| draw_once(&mut a, &mut t)).max().unwrap();
    assert!(warm <= cold || warm < Duration::from_millis(20), "cold {cold:?}, warm {warm:?}");
    // the window onto the thread shows the end first, and scrolling up reaches the start
    let s = screen(&mut a, 100, 30);
    assert!(s.contains("message number 299"), "{s}");
    a.scroll = 0;
    let s = screen(&mut a, 100, 30);
    assert!(s.contains("Msg #") && s.contains("start"), "{s}");
    a.scroll = 40;
    let s = screen(&mut a, 100, 30);
    assert!(s.contains("replied") && s.contains("message number"), "{s}");
}

fn db_fill(db: &Db, tid: i64, n: usize) {
    db.tx(|| {
        for i in 0..n {
            db.reply(tid, &format!("agent-{}", i % 5), "agent", &format!("message number {i}\n\nwith **bold** text and a list:\n\n- a\n- b"), None, None)?;
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn an_identical_refresh_has_the_same_signature_and_a_changed_one_does_not() {
    let (_d, a) = file_app(30);
    let load = |a: &App| Fresh::load(&a.db, a.want(), None, None);
    let (first, mut second) = (load(&a), load(&a));
    // what a refresh reads from files and the environment is shared with every other test in this process, which switches
    // AGENTDESK_DATA and rewrites settings as it likes: take those from the first load so only the board is compared
    second.st.max_sessions = first.st.max_sessions;
    second.st.allow = first.st.allow.clone();
    second.st.settings_path = first.st.settings_path.clone();
    second.st.usage_lines = first.st.usage_lines.clone();
    second.st.usage_summary = first.st.usage_summary.clone();
    second.st.slack = first.st.slack;
    second.st.slack_up = first.st.slack_up;
    second.update = first.update.clone();
    // two loads build their HashMaps separately (each with its own random order): the description must not show it
    assert!(
        first.describe() == second.describe(),
        "the same board gave two descriptions:
{}
{}",
        first.describe(),
        second.describe()
    );
    assert_eq!(first.describe(), first.describe());
    a.db.start_thread("discussion", "new", "agent-q", "agent", "x", None, None).unwrap();
    assert_ne!(first.describe(), load(&a).describe());
}
