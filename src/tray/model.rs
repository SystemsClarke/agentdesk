//! The tray's logic with no window in it: what the menu says for a given board and core state, which icon goes with it, how a menu click
//! is read back, how the terminal window is launched on each OS, and the icon bitmaps themselves. All pure, so every OS tests it.

use serde_json::{json, Value};

/// Open questions listed in the menu (the rest are one click away in the window).
pub const MAX_QUESTIONS: usize = 5;
/// Icon edge in pixels. Windows and Linux scale it; macOS draws it at menu-bar height.
pub const ICON_SIZE: u32 = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub id: i64,
    pub subject: String,
}

/// Everything the menu depends on. Two equal snapshots draw the same menu, so the loop only rebuilds when this changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub core_up: bool,
    /// All open questions, newest first (only the first few are listed, the count covers all).
    pub questions: Vec<Question>,
    pub concierge_on: bool,
    /// The ops console answers on its port (it is served by the core or the window, so a down core usually means no console).
    pub console_up: bool,
    /// The version (no leading `v`) that the last update check found newer than this build.
    pub update: Option<String>,
    /// An install-and-restart is under way: the menu shows it and refuses a second click.
    pub updating: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Idle,
    Attention,
    Down,
}

impl IconState {
    pub fn name(self) -> &'static str {
        match self {
            IconState::Idle => "idle",
            IconState::Attention => "attention",
            IconState::Down => "down",
        }
    }
}

/// A down core wins over questions: nothing will act on them until it is back.
pub fn icon_state(s: &Snapshot) -> IconState {
    if !s.core_up {
        IconState::Down
    } else if s.questions.is_empty() {
        IconState::Idle
    } else {
        IconState::Attention
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
}

fn item(id: &str, label: String, enabled: bool) -> Entry {
    Entry { id: id.to_string(), label, enabled, separator: false }
}

fn sep() -> Entry {
    Entry { id: String::new(), label: String::new(), enabled: false, separator: true }
}

pub fn status_line(s: &Snapshot) -> String {
    let core = if s.core_up { "Core running" } else { "Core not running" };
    let n = s.questions.len();
    let q = match n {
        0 => "no open questions".to_string(),
        1 => "1 open question".to_string(),
        n => format!("{n} open questions"),
    };
    format!("{core} | {q} | Concierge {}", if s.concierge_on { "on" } else { "off" })
}

pub fn tooltip(s: &Snapshot) -> String {
    format!("AgentDesk: {}", status_line(s))
}

/// A subject on one line, cut to fit a menu.
fn shorten(subject: &str) -> String {
    let one: String = subject.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 56 {
        format!("{}...", one.chars().take(53).collect::<String>())
    } else {
        one
    }
}

/// The menu for a snapshot. With the core down only the actions that still make sense stay enabled.
pub fn menu(s: &Snapshot) -> Vec<Entry> {
    let mut m = vec![item("status", status_line(s), false), sep(), item("open", "Open AgentDesk".into(), true)];
    m.push(item("console", if s.console_up { "Open ops console".into() } else { "Open ops console (not running)".into() }, s.console_up));
    if !s.questions.is_empty() {
        m.push(sep());
        for q in s.questions.iter().take(MAX_QUESTIONS) {
            m.push(item(&format!("q:{}", q.id), format!("#{} {}", q.id, shorten(&q.subject)), true));
        }
        if s.questions.len() > MAX_QUESTIONS {
            m.push(item("open", format!("{} more in the window...", s.questions.len() - MAX_QUESTIONS), true));
        }
    }
    m.push(sep());
    m.push(item("concierge", format!("Concierge: {}", if s.concierge_on { "on (click to turn off)" } else { "off (click to turn on)" }), true));
    if let Some(v) = s.update.as_deref() {
        let v = v.trim_start_matches('v');
        m.push(if s.updating {
            item("update", format!("Updating to v{v}... (the core restarts when done)"), false)
        } else {
            item("update", format!("Update available: v{v} - install and restart"), true)
        });
    }
    m.push(item("folder", "Open the data and logs folder".into(), true));
    m.push(item("stop", "Stop the core".into(), s.core_up));
    m.push(sep());
    m.push(item("quit", "Quit tray".into(), true));
    m
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Open,
    OpenQuestion(i64),
    ToggleConcierge,
    OpenConsole,
    OpenFolder,
    InstallUpdate,
    StopCore,
    Quit,
    None,
}

/// A clicked menu id back to what to do (the status line and anything unknown do nothing).
pub fn action(id: &str) -> Action {
    match id {
        "open" => Action::Open,
        "concierge" => Action::ToggleConcierge,
        "console" => Action::OpenConsole,
        "folder" => Action::OpenFolder,
        "update" => Action::InstallUpdate,
        "stop" => Action::StopCore,
        "quit" => Action::Quit,
        _ => match id.strip_prefix("q:").and_then(|n| n.parse().ok()) {
            Some(n) => Action::OpenQuestion(n),
            None => Action::None,
        },
    }
}

/// The snapshot's questions from `Db::open_questions` rows (`thread_id`, `subject`), newest first as the query returns them.
pub fn questions_from_rows(rows: &[Value]) -> Vec<Question> {
    rows.iter().filter_map(|r| Some(Question { id: r["thread_id"].as_i64().or_else(|| r["id"].as_i64())?, subject: r["subject"].as_str().unwrap_or("").to_string() })).collect()
}

// ---- terminal launch

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }
}

/// The program and arguments that open a terminal window running `<exe> tui`. `over` is `AGENTDESK_TERMINAL` (a terminal command line,
/// split on spaces, that gets `<exe> tui` appended); `have` says whether a program is on the PATH. None when no terminal is found.
pub fn terminal_command(os: Os, exe: &str, over: Option<&str>, have: &dyn Fn(&str) -> bool) -> Option<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let tail = [exe.to_string(), "tui".to_string()];
    if let Some(o) = over.map(str::trim).filter(|o| !o.is_empty()) {
        return Some(o.split_whitespace().map(str::to_string).chain(tail).collect());
    }
    match os {
        Os::Windows => {
            if have("wt") {
                Some(s(&["wt", "new-tab", "--title", "AgentDesk"]).into_iter().chain(tail).collect())
            } else {
                // `start` is a cmd built-in; the empty-ish first quoted argument is the window title it requires
                Some(s(&["cmd", "/c", "start", "AgentDesk"]).into_iter().chain(tail).collect())
            }
        }
        Os::Mac => {
            let script =
                format!("tell application \"Terminal\"\nactivate\ndo script \"'{}' tui\"\nend tell", exe.replace('\'', "'\\''").replace('\\', "\\\\").replace('"', "\\\""));
            Some(vec!["osascript".into(), "-e".into(), script])
        }
        Os::Linux => {
            let (prog, flag) = [("x-terminal-emulator", "-e"), ("gnome-terminal", "--"), ("konsole", "-e"), ("xterm", "-e")].into_iter().find(|(p, _)| have(p))?;
            Some(vec![prog.to_string(), flag.to_string()].into_iter().chain(tail).collect())
        }
    }
}

/// The program and arguments that open `target` (an http URL in the default browser, anything else as a folder in the file manager).
/// Windows goes through `cmd /c start` for a URL (the empty first argument is the window title `start` insists on) and `explorer` for a
/// folder; the caller runs both hidden.
pub fn open_command(os: Os, target: &str) -> Vec<String> {
    let url = target.starts_with("http://") || target.starts_with("https://");
    let v = |a: &[&str]| a.iter().map(|x| x.to_string()).chain([target.to_string()]).collect::<Vec<String>>();
    match (os, url) {
        (Os::Windows, true) => v(&["cmd", "/c", "start", ""]),
        (Os::Windows, false) => v(&["explorer"]),
        (Os::Mac, _) => v(&["open"]),
        (Os::Linux, _) => v(&["xdg-open"]),
    }
}

// ---- Windows: promoting the icon out of the overflow

/// `reg query "HKCU\Control Panel\NotifyIconSettings" /s /v ExecutablePath` output to the registry keys whose `ExecutablePath` is `exe`.
/// Windows lists one subkey per program that ever showed a notification icon; the path is stored literally for most programs but as
/// `{KNOWNFOLDER-GUID}\rest` under Program Files and WindowsApps, which cannot be compared to a real path, so a GUID-prefixed entry matches
/// on the file name alone. Keys with no `ExecutablePath` are skipped. Case, slash direction and a `\\?\` prefix do not matter.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows tray promotes its icon
pub fn promotable_keys(reg_query: &str, exe: &str) -> Vec<String> {
    let norm = |p: &str| p.trim().trim_start_matches(r"\\?\").replace('/', r"\").to_lowercase();
    let want = norm(exe);
    let leaf = |p: &str| p.rsplit('\\').next().unwrap_or("").to_string();
    let mut out: Vec<String> = Vec::new();
    let mut key: Option<&str> = None;
    for line in reg_query.lines() {
        let t = line.trim();
        if t.to_uppercase().starts_with("HKEY_") {
            key = Some(t);
        } else if let (Some(k), Some((name, rest))) = (key, t.split_once(char::is_whitespace)) {
            if !name.eq_ignore_ascii_case("ExecutablePath") {
                continue;
            }
            let Some((_, path)) = rest.trim_start().split_once(char::is_whitespace).filter(|(ty, _)| ty.eq_ignore_ascii_case("REG_SZ")) else { continue };
            let have = norm(path);
            if (have == want || (have.starts_with('{') && leaf(&have) == leaf(&want))) && !out.iter().any(|o| o == k) {
                out.push(k.to_string());
            }
        }
    }
    out
}

// ---- icons

fn blend(px: &mut [u8], rgb: [u8; 3], cover: f32) {
    let a = cover.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    let out = a + (px[3] as f32 / 255.0) * (1.0 - a);
    for i in 0..3 {
        px[i] = ((rgb[i] as f32 * a + px[i] as f32 * (px[3] as f32 / 255.0) * (1.0 - a)) / out) as u8;
    }
    px[3] = (out * 255.0) as u8;
}

/// A filled circle with a one-pixel soft edge.
fn disc(buf: &mut [u8], size: u32, cx: f32, cy: f32, r: f32, rgb: [u8; 3]) {
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = ((y * size + x) * 4) as usize;
            blend(&mut buf[i..i + 4], rgb, r + 0.5 - d);
        }
    }
}

/// RGBA, `size` x `size`. Idle is a calm blue disc, attention an amber disc with a red badge dot at the top right, down a grey disc with
/// a dark slash: the three differ in colour and shape, so they stay apart at 16 px and for colour-blind eyes.
pub fn icon_rgba(state: IconState, size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let c = size as f32 / 2.0;
    let r = size as f32 * 0.42;
    let body = match state {
        IconState::Idle => [0x2e, 0x7d, 0xd1],
        IconState::Attention => [0xf0, 0xa0, 0x20],
        IconState::Down => [0x80, 0x84, 0x8a],
    };
    disc(&mut buf, size, c, c, r, body);
    disc(&mut buf, size, c, c, r * 0.38, [0xff, 0xff, 0xff]); // the "desk" mark in the middle
    match state {
        IconState::Attention => {
            disc(&mut buf, size, size as f32 * 0.76, size as f32 * 0.24, size as f32 * 0.22, [0xff, 0xff, 0xff]);
            disc(&mut buf, size, size as f32 * 0.76, size as f32 * 0.24, size as f32 * 0.17, [0xd6, 0x28, 0x28]);
        }
        IconState::Down => {
            // a slash across the disc
            for t in 0..size * 2 {
                let (x, y) = (t as f32 * 0.5, size as f32 - t as f32 * 0.5);
                if x < size as f32 && y >= 0.0 && ((x - c).powi(2) + (y - c).powi(2)).sqrt() < r {
                    disc(&mut buf, size, x, y, size as f32 * 0.06, [0x30, 0x30, 0x34]);
                }
            }
        }
        IconState::Idle => {}
    }
    buf
}

/// FNV-1a over the pixels: a short fingerprint for the self-test to print.
fn fingerprint(b: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in b {
        h = (h ^ *x as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// `agentdesk tray --self-test`: the menu of each sample state, the icons, and the terminal command, with no window opened.
pub fn self_test() -> Value {
    let q = |id: i64, s: &str| Question { id, subject: s.to_string() };
    let samples = [
        ("idle", Snapshot { core_up: true, questions: vec![], concierge_on: false, console_up: true, update: None, updating: false }),
        (
            "attention",
            Snapshot { core_up: true, questions: vec![q(7, "Deploy to prod?"), q(5, "Which branch")], concierge_on: true, console_up: true, update: None, updating: false },
        ),
        ("down", Snapshot { core_up: false, questions: vec![q(7, "Deploy to prod?")], concierge_on: false, console_up: false, update: None, updating: false }),
        ("update-available", Snapshot { core_up: true, questions: vec![], concierge_on: false, console_up: true, update: Some("1.2.3".into()), updating: false }),
        ("updating", Snapshot { core_up: true, questions: vec![], concierge_on: false, console_up: true, update: Some("1.2.3".into()), updating: true }),
    ];
    let states: Vec<Value> = samples
        .iter()
        .map(|(name, s)| {
            let st = icon_state(s);
            let px = icon_rgba(st, ICON_SIZE);
            json!({
                "sample": name,
                "icon": st.name(),
                "status": status_line(s),
                "menu": menu(s).iter().map(|e| if e.separator { "---".to_string() } else { e.label.clone() }).collect::<Vec<_>>(),
                "icon_bytes": px.len(),
                "icon_fingerprint": fingerprint(&px),
            })
        })
        .collect();
    let cmd = terminal_command(Os::current(), "agentdesk", std::env::var("AGENTDESK_TERMINAL").ok().as_deref(), &which);
    let opens: Vec<Value> = [Os::Windows, Os::Mac, Os::Linux]
        .iter()
        .map(|os| json!({"os": format!("{os:?}"), "console": open_command(*os, "http://127.0.0.1:8123/?k=key"), "folder": open_command(*os, "/data")}))
        .collect();
    json!({"ok": true, "icon_size": ICON_SIZE, "states": states, "terminal": cmd, "open": opens})
}

/// Whether `program` is an executable file in a PATH folder.
pub fn which(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|d| {
        let p = d.join(program);
        p.is_file() || (cfg!(windows) && ["exe", "cmd", "bat"].iter().any(|e| p.with_extension(e).is_file()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(up: bool, n: i64, c: bool) -> Snapshot {
        Snapshot {
            core_up: up,
            questions: (1..=n).rev().map(|i| Question { id: i, subject: format!("question {i}") }).collect(),
            concierge_on: c,
            console_up: up,
            update: None,
            updating: false,
        }
    }

    #[test]
    fn icon_follows_core_and_questions() {
        assert_eq!(icon_state(&snap(true, 0, false)), IconState::Idle);
        assert_eq!(icon_state(&snap(true, 2, false)), IconState::Attention);
        assert_eq!(icon_state(&snap(false, 2, true)), IconState::Down, "a down core wins");
    }

    #[test]
    fn status_line_counts_and_says_concierge() {
        assert_eq!(status_line(&snap(true, 0, false)), "Core running | no open questions | Concierge off");
        assert_eq!(status_line(&snap(true, 1, true)), "Core running | 1 open question | Concierge on");
        assert_eq!(status_line(&snap(false, 3, false)), "Core not running | 3 open questions | Concierge off");
    }

    #[test]
    fn menu_lists_the_latest_questions_and_caps_them() {
        let labels = |s: &Snapshot| menu(s).into_iter().filter(|e| !e.separator).map(|e| e.label).collect::<Vec<_>>();
        let none = labels(&snap(true, 0, false));
        assert_eq!(none.len(), 7, "status, open, console, concierge, folder, stop, quit: {none:?}");
        let many = menu(&snap(true, 8, false));
        let qs: Vec<_> = many.iter().filter(|e| e.id.starts_with("q:")).collect();
        assert_eq!(qs.len(), MAX_QUESTIONS);
        assert_eq!(qs[0].label, "#8 question 8", "newest first");
        assert!(many.iter().any(|e| e.label == "3 more in the window..."));
    }

    #[test]
    fn stop_needs_a_core_and_status_is_never_clickable() {
        let find = |s: &Snapshot, id: &str| menu(s).into_iter().find(|e| e.id == id).unwrap();
        assert!(find(&snap(true, 0, false), "stop").enabled);
        assert!(!find(&snap(false, 0, false), "stop").enabled);
        assert!(!find(&snap(true, 0, false), "status").enabled);
        assert!(find(&snap(true, 0, true), "concierge").label.contains("on (click to turn off)"));
        assert!(find(&snap(true, 0, false), "concierge").label.contains("off (click to turn on)"));
    }

    #[test]
    fn long_subjects_are_one_short_line() {
        let s = Snapshot { questions: vec![Question { id: 1, subject: format!("a\nb {}", "x".repeat(100)) }], ..snap(true, 0, false) };
        let l = menu(&s).into_iter().find(|e| e.id == "q:1").unwrap().label;
        assert!(!l.contains('\n') && l.chars().count() <= 62 && l.ends_with("..."), "{l}");
    }

    #[test]
    fn clicks_map_to_actions() {
        assert_eq!(action("open"), Action::Open);
        assert_eq!(action("q:42"), Action::OpenQuestion(42));
        assert_eq!(action("concierge"), Action::ToggleConcierge);
        assert_eq!(action("stop"), Action::StopCore);
        assert_eq!(action("console"), Action::OpenConsole);
        assert_eq!(action("folder"), Action::OpenFolder);
        assert_eq!(action("update"), Action::InstallUpdate);
        assert_eq!(action("quit"), Action::Quit);
        assert_eq!(action("status"), Action::None);
        assert_eq!(action("q:x"), Action::None);
    }

    #[test]
    fn questions_come_from_board_rows() {
        let rows = vec![json!({"thread_id": 9, "subject": "s"}), json!({"id": 4, "subject": "t"}), json!({"subject": "no id"})];
        assert_eq!(questions_from_rows(&rows), vec![Question { id: 9, subject: "s".into() }, Question { id: 4, subject: "t".into() }]);
    }

    #[test]
    fn terminal_command_per_os() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        let w = terminal_command(Os::Windows, "C:\\ad\\agentdesk.exe", None, &all).unwrap();
        assert_eq!(w[0], "wt");
        assert_eq!(&w[w.len() - 2..], ["C:\\ad\\agentdesk.exe", "tui"]);
        let c = terminal_command(Os::Windows, "ad.exe", None, &none).unwrap();
        assert_eq!(&c[..4], ["cmd", "/c", "start", "AgentDesk"]);
        let m = terminal_command(Os::Mac, "/usr/local/bin/agentdesk", None, &none).unwrap();
        assert_eq!((m[0].as_str(), m[1].as_str()), ("osascript", "-e"));
        assert!(m[2].contains("do script \"'/usr/local/bin/agentdesk' tui\""), "{}", m[2]);
        let only = |p: &str| p == "konsole";
        assert_eq!(terminal_command(Os::Linux, "ad", None, &only).unwrap(), ["konsole", "-e", "ad", "tui"]);
        assert_eq!(terminal_command(Os::Linux, "ad", None, &all).unwrap()[0], "x-terminal-emulator", "the first preference wins");
        assert!(terminal_command(Os::Linux, "ad", None, &none).is_none(), "no terminal found");
    }

    #[test]
    fn terminal_override_wins_on_every_os() {
        for os in [Os::Windows, Os::Mac, Os::Linux] {
            let c = terminal_command(os, "ad", Some("  alacritty  -e "), &|_| false).unwrap();
            assert_eq!(c, ["alacritty", "-e", "ad", "tui"]);
            assert!(terminal_command(os, "ad", Some("   "), &|_| true).unwrap()[0] != "alacritty", "a blank override is no override");
        }
    }

    #[test]
    fn icons_have_the_right_size_and_are_distinct() {
        let px: Vec<Vec<u8>> = [IconState::Idle, IconState::Attention, IconState::Down].iter().map(|s| icon_rgba(*s, ICON_SIZE)).collect();
        for p in &px {
            assert_eq!(p.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            assert!(p.chunks(4).any(|c| c[3] == 255), "something is drawn");
            assert_eq!(p[3], 0, "the corner is transparent");
        }
        assert!(px[0] != px[1] && px[1] != px[2] && px[0] != px[2]);
        // attention has a red badge at the top right that the others lack
        let at = |p: &Vec<u8>| {
            let i = ((7 * ICON_SIZE + 24) * 4) as usize;
            [p[i], p[i + 1], p[i + 2]]
        };
        assert_eq!(at(&px[1]), [0xd6, 0x28, 0x28]);
        assert_ne!(at(&px[0]), [0xd6, 0x28, 0x28]);
    }

    #[test]
    fn self_test_covers_every_state() {
        let v = self_test();
        assert_eq!(v["ok"], true);
        let icons: Vec<&str> = v["states"].as_array().unwrap().iter().map(|s| s["icon"].as_str().unwrap()).collect();
        assert_eq!(icons, ["idle", "attention", "down", "idle", "idle"]);
        let samples: Vec<&str> = v["states"].as_array().unwrap().iter().map(|s| s["sample"].as_str().unwrap()).collect();
        assert_eq!(samples, ["idle", "attention", "down", "update-available", "updating"]);
        let menu_of = |n: usize| v["states"][n]["menu"].to_string();
        assert!(menu_of(3).contains("Update available: v1.2.3 - install and restart"));
        assert!(menu_of(4).contains("Updating to v1.2.3..."));
        assert!(menu_of(2).contains("Open ops console (not running)"));
        assert_eq!(v["open"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn console_item_needs_a_serving_console() {
        let find = |s: &Snapshot, id: &str| menu(s).into_iter().find(|e| e.id == id).unwrap();
        let up = snap(true, 0, false);
        assert!(find(&up, "console").enabled);
        assert_eq!(find(&up, "console").label, "Open ops console");
        let down = Snapshot { console_up: false, ..snap(true, 0, false) };
        assert!(!find(&down, "console").enabled, "a click with nothing serving would do nothing");
        assert!(find(&down, "console").label.contains("not running"));
    }

    #[test]
    fn update_item_follows_the_check_and_the_install() {
        let has = |s: &Snapshot| menu(s).into_iter().find(|e| e.id == "update");
        assert!(has(&snap(true, 0, false)).is_none(), "nothing newer: no item");
        let newer = Snapshot { update: Some("0.9.1".into()), ..snap(true, 0, false) };
        let e = has(&newer).unwrap();
        assert!(e.enabled);
        assert_eq!(e.label, "Update available: v0.9.1 - install and restart");
        let tagged = Snapshot { update: Some("v0.9.1".into()), ..snap(true, 0, false) };
        assert_eq!(has(&tagged).unwrap().label, e.label, "a stored leading v is not doubled");
        let busy = Snapshot { updating: true, ..newer.clone() };
        let e = has(&busy).unwrap();
        assert!(!e.enabled, "no second click while installing");
        assert!(e.label.starts_with("Updating to v0.9.1"));
        // offered with the core down too: the install starts it again
        assert!(has(&Snapshot { update: Some("0.9.1".into()), ..snap(false, 0, false) }).unwrap().enabled);
    }

    #[test]
    fn the_folder_item_is_always_there() {
        for s in [snap(true, 0, false), snap(false, 3, true)] {
            assert!(menu(&s).iter().any(|e| e.id == "folder" && e.enabled));
        }
    }

    #[test]
    fn open_command_per_os() {
        let url = "http://127.0.0.1:8123/?k=abc";
        assert_eq!(open_command(Os::Windows, url), ["cmd", "/c", "start", "", url]);
        assert_eq!(open_command(Os::Windows, r"C:\Users\x\AgentDesk"), ["explorer", r"C:\Users\x\AgentDesk"]);
        assert_eq!(open_command(Os::Mac, url), ["open", url]);
        assert_eq!(open_command(Os::Mac, "/data"), ["open", "/data"]);
        assert_eq!(open_command(Os::Linux, url), ["xdg-open", url]);
        assert_eq!(open_command(Os::Linux, "/data"), ["xdg-open", "/data"]);
    }

    // the shape `reg query` really prints (checked against this machine's registry): literal paths, {GUID}-prefixed ones, and keys with no path
    const REG: &str = r"
HKEY_CURRENT_USER\Control Panel\NotifyIconSettings\111
    ExecutablePath    REG_SZ    C:\Users\me\Whisper\Whisper.exe

HKEY_CURRENT_USER\Control Panel\NotifyIconSettings\222
    ExecutablePath    REG_SZ    C:\Users\me\AppData\Local\AgentDeskApp\agentdesk.exe

HKEY_CURRENT_USER\Control Panel\NotifyIconSettings\333

HKEY_CURRENT_USER\Control Panel\NotifyIconSettings\444
    ExecutablePath    REG_SZ    {6D809377-6AF0-444B-8957-A3773F02200E}\AgentDesk\agentdesk.exe

HKEY_CURRENT_USER\Control Panel\NotifyIconSettings\555
    ExecutablePath    REG_SZ    {6D809377-6AF0-444B-8957-A3773F02200E}\Other\other.exe
";

    #[test]
    fn promotion_finds_this_programs_registry_key() {
        let key = |n: &str| format!(r"HKEY_CURRENT_USER\Control Panel\NotifyIconSettings\{n}");
        let exe = r"C:\Users\me\AppData\Local\AgentDeskApp\agentdesk.exe";
        assert_eq!(promotable_keys(REG, exe), [key("222"), key("444")], "the literal path, and the Program Files entry by file name");
        assert_eq!(promotable_keys(REG, r"c:/users/ME/appdata/local/agentdeskapp/AgentDesk.exe")[0], key("222"), "case and slashes do not matter");
        assert_eq!(promotable_keys(REG, r"\\?\C:\Users\me\AppData\Local\AgentDeskApp\agentdesk.exe")[0], key("222"), "the verbatim prefix is ignored");
        assert!(promotable_keys(REG, r"C:\elsewhere\notme.exe").is_empty());
        assert!(promotable_keys("", exe).is_empty());
        assert!(promotable_keys("ERROR: The system was unable to find the specified registry key or value.", exe).is_empty());
    }
}
