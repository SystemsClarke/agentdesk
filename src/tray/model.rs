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
    StopCore,
    Quit,
    None,
}

/// A clicked menu id back to what to do (the status line and anything unknown do nothing).
pub fn action(id: &str) -> Action {
    match id {
        "open" => Action::Open,
        "concierge" => Action::ToggleConcierge,
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
    terminal_command_args(os, exe, over, have, &["tui".to_string()])
}

/// Like `terminal_command`, but the terminal runs `<exe> <args...>` (`["open", "12"]` for a clicked toast).
pub fn terminal_command_args(os: Os, exe: &str, over: Option<&str>, have: &dyn Fn(&str) -> bool, args: &[String]) -> Option<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let tail: Vec<String> = std::iter::once(exe.to_string()).chain(args.iter().cloned()).collect();
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
            let esc = |t: &str| t.replace('\'', "'\\''").replace('\\', "\\\\").replace('"', "\\\"");
            let rest: String = args.iter().map(|a| format!(" {}", esc(a))).collect();
            let script = format!("tell application \"Terminal\"\nactivate\ndo script \"'{}'{rest}\"\nend tell", esc(exe));
            Some(vec!["osascript".into(), "-e".into(), script])
        }
        Os::Linux => {
            let (prog, flag) = [("x-terminal-emulator", "-e"), ("gnome-terminal", "--"), ("konsole", "-e"), ("xterm", "-e")].into_iter().find(|(p, _)| have(p))?;
            Some(vec![prog.to_string(), flag.to_string()].into_iter().chain(tail).collect())
        }
    }
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
        ("idle", Snapshot { core_up: true, questions: vec![], concierge_on: false }),
        ("attention", Snapshot { core_up: true, questions: vec![q(7, "Deploy to prod?"), q(5, "Which branch")], concierge_on: true }),
        ("down", Snapshot { core_up: false, questions: vec![q(7, "Deploy to prod?")], concierge_on: false }),
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
    json!({"ok": true, "icon_size": ICON_SIZE, "states": states, "terminal": cmd})
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
        Snapshot { core_up: up, questions: (1..=n).rev().map(|i| Question { id: i, subject: format!("question {i}") }).collect(), concierge_on: c }
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
        assert_eq!(none.len(), 5, "status, open, concierge, stop, quit: {none:?}");
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
        let s = Snapshot { core_up: true, questions: vec![Question { id: 1, subject: format!("a\nb {}", "x".repeat(100)) }], concierge_on: false };
        let l = menu(&s).into_iter().find(|e| e.id == "q:1").unwrap().label;
        assert!(!l.contains('\n') && l.chars().count() <= 62 && l.ends_with("..."), "{l}");
    }

    #[test]
    fn clicks_map_to_actions() {
        assert_eq!(action("open"), Action::Open);
        assert_eq!(action("q:42"), Action::OpenQuestion(42));
        assert_eq!(action("concierge"), Action::ToggleConcierge);
        assert_eq!(action("stop"), Action::StopCore);
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
    fn terminal_command_can_carry_other_arguments() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        let args = ["open".to_string(), "12".to_string()];
        let w = terminal_command_args(Os::Windows, "ad.exe", None, &all, &args).unwrap();
        assert_eq!(&w[w.len() - 3..], ["ad.exe", "open", "12"]);
        let o = terminal_command_args(Os::Linux, "ad", Some("alacritty -e"), &none, &args).unwrap();
        assert_eq!(o, ["alacritty", "-e", "ad", "open", "12"]);
        let m = terminal_command_args(Os::Mac, "/bin/ad", None, &none, &args).unwrap();
        assert!(m[2].contains("do script \"'/bin/ad' open 12\""), "{}", m[2]);
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
    fn self_test_covers_three_states() {
        let v = self_test();
        assert_eq!(v["ok"], true);
        let icons: Vec<&str> = v["states"].as_array().unwrap().iter().map(|s| s["icon"].as_str().unwrap()).collect();
        assert_eq!(icons, ["idle", "attention", "down"]);
    }
}
