use super::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.init().unwrap();
    (dir, App::with_db(db))
}

pub fn screen(app: &mut App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| app.draw(f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

pub fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

pub fn press(app: &mut App, c: char) {
    app.on_key(key(KeyCode::Char(c)));
}

#[test]
fn main_menu_has_every_original_option() {
    let (_d, mut a) = app();
    a.db.start_thread("question", "Deploy to prod?", "builder", "agent", "Ship now?", None, None).unwrap();
    a.reload();
    let s = screen(&mut a, 110, 40);
    println!("{s}");
    for want in [
        "Main menu",
        "[Q] Questions",
        "[J] Work to Hire",
        "[D] Discussion",
        "[P] Pull Requests",
        "[W] Wiki",
        "[S] SysOp console",
        "[R] Recurring jobs",
        "[X] Permission rules",
        "[B] Who's on",
        "[O] Options",
        "[A] Agents & goals",
        "[G] Hang up",
        "Recent callers",
        "1 ringing for you",
    ] {
        assert!(s.contains(want), "main menu lacks {want}");
    }
}

#[test]
fn letter_keys_reach_every_screen() {
    let (_d, mut a) = app();
    for (c, title) in [
        ('q', "Questions"),
        ('d', "Discussion"),
        ('w', "Wiki"),
        ('j', "Work to Hire"),
        ('p', "Pull Requests"),
        ('s', "SysOp console"),
        ('b', "Who's on"),
        ('o', "Options"),
        ('a', "Agents"),
        ('r', "Recurring jobs"),
        ('x', "Permission rules"),
    ] {
        a.on_key(key(KeyCode::Esc));
        press(&mut a, c);
        let s = screen(&mut a, 110, 36);
        assert!(s.lines().next().unwrap().contains(title), "{c} should open {title}:\n{s}");
    }
    a.on_key(key(KeyCode::Esc));
    press(&mut a, 'g');
    assert!(a.quit_at.is_some());
}

#[test]
fn questions_list_read_reply_close_and_archive() {
    let (_d, mut a) = app();
    let q = a.db.start_thread("question", "Deploy to prod?", "builder", "agent", "Ship **now**?\n\n| env | state |\n|---|---|\n| prod | ok |", None, None).unwrap();
    a.reload();
    press(&mut a, 'q');
    let s = screen(&mut a, 110, 20);
    println!("{s}");
    assert!(s.contains("WAIT") && s.contains("Deploy to prod?") && s.contains("1 still ringing"));
    a.on_key(key(KeyCode::Enter));
    let s = screen(&mut a, 110, 30);
    println!("{s}");
    assert!(s.contains("Msg #1") && s.contains("WAITING ON YOU") && s.contains("prod") && s.contains("Reply"));
    for c in "go ahead".chars() {
        press(&mut a, c);
    }
    a.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(a.db.get_thread(q).unwrap()["thread"]["status"], "answered");
    let s = screen(&mut a, 110, 30);
    assert!(s.contains("answered") && s.contains("go ahead"));
    a.on_key(key(KeyCode::Esc));
    let s = screen(&mut a, 110, 20);
    assert!(s.contains("answered · stays here 24h") || s.contains("1 answered"), "{s}");
    // close and confirm
    let q2 = a.db.start_thread("question", "Another?", "builder", "agent", "ok?", None, None).unwrap();
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT));
    assert!(a.confirm.is_some());
    press(&mut a, 'y');
    assert_eq!(a.db.get_thread(q2).unwrap()["thread"]["status"], "closed");
}

#[test]
fn compose_posts_and_esc_twice_discards() {
    let (_d, mut a) = app();
    press(&mut a, 'd');
    press(&mut a, 'n');
    for c in "Notes".chars() {
        a.on_key(key(KeyCode::Char(c)));
    }
    a.on_key(key(KeyCode::Enter));
    for c in "body text".chars() {
        a.on_key(key(KeyCode::Char(c)));
    }
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.screen, Screen::Compose, "first Esc only asks");
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.screen, Screen::List);
    press(&mut a, 'n');
    for c in "Real".chars() {
        a.on_key(key(KeyCode::Char(c)));
    }
    a.on_key(key(KeyCode::Enter));
    a.on_key(key(KeyCode::Char('x')));
    a.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert_eq!(a.screen, Screen::Read);
    assert_eq!(a.db.list_threads(Some("discussion"), None, 10, true, false).unwrap().len(), 1);
}

#[test]
fn jobs_rules_prs_and_options_screens_work() {
    let (d, mut a) = app();
    let folder = d.path().to_string_lossy().to_string();
    jobs::create(
        &a.db,
        jobs::NewJob { name: "brief", folder: &folder, prompt: "do it", at: Some("08:00"), days: Some("weekdays"), every_minutes: None, model: None, catch_up_minutes: None },
    )
    .unwrap();
    a.db.register_pr("https://github.com/o/r/pull/3", "o/r", 3, "Fix retries", "builder", None).unwrap();
    a.reload();
    press(&mut a, 'r');
    let s = screen(&mut a, 120, 30);
    assert!(s.contains("brief") && s.contains("08:00 weekdays") && s.contains("do it"), "{s}");
    press(&mut a, 'e');
    assert!(!a.st.jobs[0].enabled);
    a.on_key(key(KeyCode::Esc));
    press(&mut a, 'p');
    let s = screen(&mut a, 120, 30);
    assert!(s.contains("o/r#3") && s.contains("Fix retries"), "{s}");
    a.on_key(key(KeyCode::Esc));
    press(&mut a, 'o');
    let t0 = a.theme.clone();
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Right));
    assert_ne!(a.theme, t0);
}

#[test]
fn a_script_in_a_question_is_shown_then_run_from_the_reader() {
    let (_d, mut a) = app();
    let (lang, script, want) = if cfg!(windows) { ("cmd", "echo from-the-pane", "from-the-pane") } else { ("sh", "echo from-the-pane", "from-the-pane") };
    let body = format!("Please run this and tell me what you see?\n```{lang}\n{script}\n```");
    a.db.start_thread("question", "A script for you", "builder", "agent", &body, None, None).unwrap();
    a.reload();
    press(&mut a, 'q');
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.screen, Screen::Read);
    let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
    a.on_key(alt('r'));
    let s = screen(&mut a, 120, 40);
    assert!(s.contains("Script 1/1") && s.contains(&format!("> {script}")) && s.contains("Alt+R runs it"), "shown, not run: {s}");
    a.on_key(alt('r'));
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while a.runner.as_ref().is_some_and(|r| r.state == runner::State::Running) && std::time::Instant::now() < end {
        a.tick();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    a.tick();
    let s = screen(&mut a, 120, 40);
    assert!(s.contains(want) && s.contains("done (exit 0)"), "{s}");
    a.on_key(alt('y'));
    assert!(a.reply.lines.join("\n").contains("I ran the script: it finished (exit 0)."), "the output goes into the reply to edit");
    a.on_key(alt('x'));
    assert!(a.runner.is_none());
    // a thread with no script says so
    a.on_key(key(KeyCode::Esc));
}

#[test]
fn the_folder_step_lists_the_folders_in_use_filters_them_and_enter_takes_the_pick() {
    let _e = crate::tests_identities::Env::new();
    let (_d, mut a) = app();
    // not under the temp folder: folders there are scratch and left out of the list
    let work = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
    let (alpha, beta) = (work.path().join("alpha-repo"), work.path().join("beta-repo"));
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    crate::identities::create(&a.db, "one", &alpha.to_string_lossy(), None, None, false).unwrap();
    crate::identities::create(&a.db, "two", &beta.to_string_lossy(), None, None, false).unwrap();
    a.folder_loaded = None;
    a.new_agent();
    for c in "newbie".chars() {
        press(&mut a, c);
    }
    a.on_key(key(KeyCode::Enter)); // the name; now the folder step
    assert!(a.on_folder_step());
    let s = screen(&mut a, 120, 40);
    assert!(s.contains("Folders you use") && s.contains("alpha-repo") && s.contains("beta-repo"), "{s}");
    for c in "beta".chars() {
        press(&mut a, c);
    }
    let s = screen(&mut a, 120, 40);
    assert!(s.contains("beta-repo") && !s.contains("alpha-repo"), "typing filters: {s}");
    a.on_key(key(KeyCode::Tab));
    assert!(a.subject.ends_with("beta-repo"), "Tab fills the box: {}", a.subject);
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.answers.get(1).map(|p| p.ends_with("beta-repo")), Some(true), "Enter takes it: {:?}", a.answers);
}

// ---- the mouse

/// The (column, row) of the first cell where `needle` is drawn in a `screen()` dump.
fn find(s: &str, needle: &str) -> (u16, u16) {
    for (y, line) in s.lines().enumerate() {
        if let Some(at) = line.find(needle) {
            return (line[..at].chars().count() as u16, y as u16);
        }
    }
    panic!(
        "{needle:?} is not on the screen:
{s}"
    );
}

fn three_questions() -> (tempfile::TempDir, App) {
    let (d, mut a) = app();
    for subj in ["First one", "Second one", "Third one"] {
        a.db.start_thread("question", subj, "builder", "agent", "body", None, None).unwrap();
    }
    a.reload();
    press(&mut a, 'q');
    (d, a)
}

#[test]
fn a_click_selects_the_row_that_was_drawn_and_a_quick_second_click_opens_it() {
    let (_d, mut a) = three_questions();
    let s = screen(&mut a, 110, 30);
    let (x, y) = find(&s, "Third one");
    let (_, y2) = find(&s, "Second one");
    assert_eq!(y2, y + 1, "one line per row");
    let t = Instant::now();
    a.click(x, y2, t);
    assert_eq!(a.sel_now(), 1);
    assert_eq!(a.screen, Screen::List, "one click only selects");
    let s2 = screen(&mut a, 110, 30);
    assert!(s2.lines().nth(y2 as usize).unwrap().contains('▶'), "the picked row is the marked one");
    a.click(x, y2, t + Duration::from_millis(200));
    assert_eq!(a.screen, Screen::Read);
    assert_eq!(a.detail.as_ref().map(|d| d.thread.subject.clone()).as_deref(), Some("Second one"), "the same thread Enter would have opened");
}

#[test]
fn two_clicks_on_different_rows_or_too_far_apart_do_not_open() {
    let (_d, mut a) = three_questions();
    let s = screen(&mut a, 110, 30);
    let (x, y) = find(&s, "Third one");
    let t = Instant::now();
    a.click(x, y, t);
    a.click(x, y + 1, t + Duration::from_millis(100));
    assert_eq!((a.screen, a.sel_now()), (Screen::List, 1));
    a.click(x, y + 1, t + Duration::from_millis(900));
    assert_eq!(a.screen, Screen::List);
    a.click(x, y + 3, t + Duration::from_millis(1000)); // below the last row, on its rule: nothing there
    assert_eq!(a.screen, Screen::List);
}

#[test]
fn a_click_on_a_blank_cell_does_nothing_and_a_stale_map_is_not_trusted() {
    let (_d, mut a) = three_questions();
    let s = screen(&mut a, 110, 30);
    let (x, y) = find(&s, "Third one");
    a.click(0, 28, Instant::now()); // the line above the footer is blank
    assert_eq!(a.sel_now(), 0);
    a.on_key(key(KeyCode::Esc)); // the screen moves on; nothing is drawn yet
    a.click(x, y + 1, Instant::now());
    assert_eq!((a.screen, a.sel_now()), (Screen::Main, 0));
}

#[test]
fn clicking_a_main_menu_entry_goes_to_that_screen() {
    let (_d, mut a) = app();
    let s = screen(&mut a, 110, 40);
    let (x, y) = find(&s, "[W] Wiki");
    a.click(x + 2, y, Instant::now());
    assert_eq!((a.screen, a.channel), (Screen::List, "wiki"));
    a.on_key(key(KeyCode::Esc));
    let s = screen(&mut a, 110, 40);
    let (x, y) = find(&s, "[O] Options"); // the right-hand column
    a.click(x, y, Instant::now());
    assert_eq!(a.screen, Screen::Options);
}

#[test]
fn clicking_a_footer_hint_is_pressing_its_key() {
    let (_d, mut a) = three_questions();
    let s = screen(&mut a, 110, 30);
    let (x, y) = find(&s, "main menu"); // the label of "Esc"
    assert_eq!(y, 29, "the footer is the last line");
    a.click(x, y, Instant::now());
    assert_eq!(a.screen, Screen::Main);
    // a confirmation answers to Y
    let (_d, mut a) = three_questions();
    a.confirm = Some(("Close it?".into(), Confirm::CloseThread(1)));
    let s = screen(&mut a, 110, 30);
    let (x, y) = find(&s, "no");
    a.click(x, y, Instant::now());
    assert!(a.confirm.is_none(), "N declined");
}

#[test]
fn the_wheel_moves_the_selection_in_a_list_and_scrolls_the_reader() {
    let (_d, mut a) = three_questions();
    screen(&mut a, 110, 30);
    a.wheel(1);
    assert_eq!(a.sel_now(), 2, "three rows, clamped to the last");
    a.wheel(-1);
    assert_eq!(a.sel_now(), 0);
    a.screen = Screen::Read;
    a.scroll = 5;
    a.wheel(-1);
    assert_eq!(a.scroll, 2);
    a.wheel(1);
    assert_eq!(a.scroll, 5);
}

#[test]
fn the_options_screen_toggles_the_mouse_and_says_how_to_copy_text() {
    let (_d, mut a) = app();
    a.prefs.insert("mouse".into(), json!(true)); // not whatever the real settings file says
    press(&mut a, 'o');
    let s = screen(&mut a, 140, 40);
    assert!(s.contains("Shift"), "Shift+drag is mentioned: {s}");
    let (x, y) = find(&s, "Mouse  ");
    let t = Instant::now();
    a.click(x, y, t);
    a.click(x, y, t + Duration::from_millis(100)); // double click is Enter: toggles
    assert_eq!(a.mouse_request, Some(false), "the loop is asked to stop reporting it");
    assert!(!a.pref_b("mouse", true));
}

#[test]
fn a_click_after_a_key_in_the_same_batch_is_ignored_until_the_next_draw() {
    let (_d, mut a) = three_questions();
    let s = screen(&mut a, 110, 30);
    let (x, y) = find(&s, "Third one");
    handle(&mut a, Event::Key(key(KeyCode::Down)));
    a.click(x, y + 2, Instant::now());
    assert_eq!(a.sel_now(), 1, "the key moved the selection; the stale click did nothing");
    screen(&mut a, 110, 30);
    a.click(x, y + 2, Instant::now());
    assert_eq!(a.sel_now(), 2);
}

#[test]
fn open_at_starts_on_the_reader_for_that_thread() {
    let (_d, mut a) = app();
    let q = a.db.start_thread("question", "Deploy to prod?", "builder", "agent", "Ship now?", None, None).unwrap();
    a.reload();
    a.open_at(q);
    assert_eq!(a.screen, Screen::Read);
    assert_eq!(a.read_tid, Some(q));
    let s = screen(&mut a, 110, 30);
    assert!(s.contains("Msg #1") && s.contains("Ship now?"), "{s}");
    // Esc goes back to the question list, not out of the window
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.screen, Screen::List);
    assert!(!a.quit);
}

#[test]
fn open_at_an_unknown_thread_stays_on_the_main_menu_and_says_so() {
    let (_d, mut a) = app();
    a.open_at(999);
    assert_eq!(a.screen, Screen::Main);
    assert!(a.flash.as_ref().is_some_and(|f| f.0.contains("#999 is no longer on the board")));
}

#[test]
fn open_and_tui_thread_arguments_parse() {
    use crate::{Cli, Cmd};
    use clap::Parser;
    let p = |a: &[&str]| Cli::try_parse_from(std::iter::once("agentdesk").chain(a.iter().copied()));
    assert!(matches!(p(&["open", "12"]).unwrap().cmd, Some(Cmd::Open { thread: 12 })));
    assert!(matches!(p(&["open", "#12"]).unwrap().cmd, Some(Cmd::Open { thread: 12 })));
    assert!(matches!(p(&["tui", "--thread", "7"]).unwrap().cmd, Some(Cmd::Tui { thread: Some(7) })));
    assert!(matches!(p(&["tui", "--thread=#7"]).unwrap().cmd, Some(Cmd::Tui { thread: Some(7) })));
    assert!(matches!(p(&["tui"]).unwrap().cmd, Some(Cmd::Tui { thread: None })));
    for bad in [&["open"][..], &["open", "abc"], &["open", "0"], &["open", "-1"], &["tui", "--thread"], &["tui", "--thread", "x"]] {
        assert!(p(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_saved_vscode_theme_is_resolved_when_the_scan_lands_and_an_unknown_one_falls_back_silently() {
    use super::vscode_themes::set_scanned_for_test;
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_d, mut a) = app();
    let mut p = super::seg::palettes()[0].1.clone();
    p.label = "Fixture Night".into();
    set_scanned_for_test(None);

    a.theme = "vscode:acme.x/Night".into();
    a.theme_pending = true;
    a.tick();
    assert_eq!(a.theme, "vscode:acme.x/Night", "still waiting for the scan, so not judged");

    set_scanned_for_test(Some(vec![("vscode:acme.x/Night".into(), p)]));
    a.tick();
    assert_eq!(a.palette().label, "Fixture Night");
    assert_eq!(super::seg::palettes().len(), 5);

    a.theme = "vscode:gone.ext/Missing".into();
    a.theme_pending = true;
    a.tick();
    assert_eq!(a.theme, "monokai-pro");
    assert_eq!(a.palette().label, "Monokai Pro");
    set_scanned_for_test(None);
}
