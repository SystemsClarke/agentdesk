//! The terminal window: a port of the original's BBS screens (main menu, message bases, reader, merge desk, SysOp
//! console, Who's on, options, recurring jobs, permission rules, agents) with the same letter keys and wording.
//! `screens.rs` builds each screen as lines of tagged segments; this file is state, keys and drawing.

mod bench;
pub(crate) mod charts; // the Slack bridge draws charts and diagrams with it
mod editor;
mod live;
mod md;
mod runner;
mod screens;
pub(crate) mod seg;
mod state;
mod term;

use crate::db::{anyhow_msg, Db};
use crate::{jobs, paths, permissions, prs, update};
use anyhow::Result;
use editor::Editor;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use seg::{palettes, Palette};
use serde_json::{json, Value};
use state::{Detail, Fresh, State, Want};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Main,
    List,
    Read,
    Prs,
    Jobs,
    Rules,
    Sysop,
    Who,
    Options,
    Compose,
    Agents,
    Adopt,
    Goal,
    Ask,
}

#[derive(Clone)]
pub enum AskDone {
    NewJob,
    NewAgent,
    NewGoal,
    Adopt(String),
}

#[derive(Clone)]
pub struct AskSpec {
    pub title: String,
    pub banner: String,
    pub back: Screen,
    pub fields: Vec<(String, String, bool)>,
    pub done: AskDone,
}

pub enum Confirm {
    RunJob(String),
    DeleteJob(String),
    Rule(i64, char),
    CloseThread(i64),
    Forget(String),
    StopGoal(String),
    ConciergeOff,
}

pub struct App {
    pub db: Db,
    pub st: State,
    pub screen: Screen,
    pub channel: &'static str,
    pub read_tid: Option<i64>,
    pub read_back: Screen,
    pub detail: Option<Detail>,
    pub sel: HashMap<String, usize>,
    pub top_row: HashMap<String, usize>,
    pub window: Option<(usize, usize, usize, usize)>,
    pub show_archived: bool,
    pub show_settled: bool,
    pub show_decided: bool,
    pub confirm: Option<(String, Confirm)>,
    pub flash: Option<(String, String, Instant)>,
    pub scroll: usize,
    pub reply: Editor,
    pub subject: String,
    pub subject_focus: bool,
    pub drafts: HashMap<i64, String>,
    pub compose_draft: Option<(String, String)>,
    pub discard_armed: Option<(String, String)>,
    pub compose_back: Screen,
    pub ask: Option<AskSpec>,
    pub answers: Vec<String>,
    pub theme: String,
    pub prefs: serde_json::Map<String, Value>,
    pub update: Option<String>,
    pub quit_at: Option<Instant>,
    pub quit: bool,
    pub body_h: usize,
    pub dictating: bool,
    pub goal_name: Option<String>,
    pub goal_doc: Option<Value>,
    pub adoptables: Vec<crate::adopt::Convo>,
    pub adopt_note: Option<String>,
    pub attach_request: Option<String>,
    /// The script runner pane under the reader (see `runner`).
    pub runner: Option<runner::Runner>,
    data_version: i64,
    last_load: Instant,
    reader_key: Option<(i64, usize)>,
    scroll_end: bool,
    /// The screen has changed since it was last drawn. The loop draws only when this is set (see `tick`).
    dirty: bool,
    pal: Palette,
    /// What the sync `reload` may reuse from the last one (the slow-moving budget).
    memo: state::Memo,
    /// The worker that loads refreshes off the window's thread; `None` for the headless snapshot and the tests, which reload in place.
    refresher: Option<Refresher>,
    /// The reader's rendered Markdown, per message (thread id, index): `(width, message rev, lines)`. Rendering is the slow part
    /// of a long thread, and nothing about a message changes between frames.
    md_cache: RefCell<MdCache>,
    /// Bio text by thread id for the Who's on screen; emptied when a refresh brings new data.
    bio_cache: RefCell<HashMap<i64, Option<String>>>,
    /// What `tick` last saw of the clock: the minute (relative times), and on the main menu the six-second phrase.
    time_key: (i64, i64),
    last_save: Instant,
    last_dictation: Instant,
    /// Counts the reloads done in place: a worker answer asked for before one is older than what the window shows.
    epoch: u64,
    /// A test's stopped clock, so a minute rolling over mid-test cannot cause a frame.
    #[cfg(test)]
    fixed_clock: Option<(i64, i64)>,
}

#[derive(Default)]
pub struct MdCache {
    thread: i64,
    lines: HashMap<usize, (usize, u64, Rc<Vec<seg::L>>)>,
}

/// The refresh worker and the bookkeeping around it. A refresh is asked for when the board changed (`PRAGMA data_version`), at most
/// every `MIN_GAP`, and at least every `MAX_AGE`; while one is out another is not started.
struct Refresher {
    ask: mpsc::Sender<Want>,
    answer: mpsc::Receiver<Fresh>,
    out: Option<u64>,
    started: Instant,
    /// The last result was for a screen the window has left: ask again at once.
    stale: bool,
    /// Hash of the last result applied; an identical one changes nothing on screen.
    sig: u64,
}

const MIN_GAP: Duration = Duration::from_millis(250);
const MAX_AGE: Duration = Duration::from_secs(5);
/// How long a flash message stays on the bottom line.
const FLASH: Duration = Duration::from_millis(3200);

fn john() -> crate::identity::Caller {
    crate::identity::Caller::default()
}

fn concierge_folder() -> String {
    std::env::var("AGENTDESK_CONCIERGE_FOLDER")
        .ok()
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())
}

fn n_word_(n: i64) -> String {
    format!("{n} member{}", if n == 1 { "" } else { "s" })
}

pub fn open_url(url: &str) {
    let (prog, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/c", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = crate::paths::new_command(prog).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

/// `08:00`, `08:00 weekdays`, `08:00 mon,thu`, `every 120`.
pub fn parse_schedule(s: &str) -> (Option<String>, Option<String>, Option<f64>) {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("every") {
        return (None, None, rest.trim().parse().ok());
    }
    let mut parts = s.splitn(2, char::is_whitespace);
    let at = parts.next().filter(|a| !a.is_empty()).map(String::from);
    (at, parts.next().map(|d| d.trim().to_string()).filter(|d| !d.is_empty()), None)
}

impl App {
    pub fn with_db(db: Db) -> App {
        let prefs = std::fs::read_to_string(paths::data_dir().join("settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let theme = prefs.get("theme").and_then(|t| t.as_str()).filter(|t| palettes().iter().any(|(k, _)| k == t)).unwrap_or("monokai-pro").to_string();
        let mut app = App {
            st: State::load(&db, false, false),
            db,
            screen: Screen::Main,
            channel: "question",
            read_tid: None,
            read_back: Screen::List,
            detail: None,
            sel: HashMap::new(),
            top_row: HashMap::new(),
            window: None,
            show_archived: false,
            show_settled: false,
            show_decided: false,
            confirm: None,
            flash: None,
            scroll: 0,
            reply: Editor::new(),
            subject: String::new(),
            subject_focus: true,
            drafts: HashMap::new(),
            compose_draft: None,
            discard_armed: None,
            compose_back: Screen::List,
            ask: None,
            answers: vec![],
            theme,
            prefs,
            update: update::pending(),
            quit_at: None,
            quit: false,
            body_h: 20,
            dictating: false,
            goal_name: None,
            goal_doc: None,
            adoptables: vec![],
            adopt_note: None,
            attach_request: None,
            runner: None,
            data_version: -1,
            last_load: Instant::now(),
            reader_key: None,
            scroll_end: false,
            dirty: true,
            pal: palettes().remove(0).1,
            memo: state::Memo::default(),
            refresher: None,
            md_cache: RefCell::new(MdCache::default()),
            bio_cache: RefCell::new(HashMap::new()),
            time_key: (0, 0),
            last_save: Instant::now(),
            last_dictation: Instant::now(),
            epoch: 0,
            #[cfg(test)]
            fixed_clock: None,
        };
        app.pal = app.palette_for(&app.theme);
        app.time_key = app.clock();
        app.load_drafts();
        app.data_version = app.db.data_version();
        app
    }

    fn palette_for(&self, theme: &str) -> Palette {
        palettes().into_iter().find(|(k, _)| *k == theme).map(|(_, p)| p).unwrap_or_else(|| palettes().remove(0).1)
    }

    /// The current theme's palette, built when the theme was chosen rather than on every frame.
    pub fn palette(&self) -> Palette {
        self.pal.clone()
    }

    pub fn theme_index(&self) -> usize {
        palettes().iter().position(|(k, _)| *k == self.theme).unwrap_or(0)
    }

    pub fn settings_path(&self) -> std::path::PathBuf {
        paths::data_dir().join("settings.json")
    }

    pub fn pref_i(&self, k: &str, d: i64) -> i64 {
        self.prefs.get(k).and_then(|v| v.as_i64()).unwrap_or(d)
    }

    pub fn pref_b(&self, k: &str, d: bool) -> bool {
        self.prefs.get(k).and_then(|v| v.as_bool()).unwrap_or(d)
    }

    /// Changes this one key in the file as it is now, not as this window read it at launch (other processes write keys of
    /// their own), written beside it and moved in: never half a file.
    pub fn set_pref(&mut self, key: &str, value: Value) {
        self.prefs.insert(key.into(), value.clone());
        if cfg!(test) {
            return;
        }
        let path = self.settings_path();
        let mut on_disk = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        on_disk.insert(key.into(), value);
        let tmp = path.with_extension("json.tmp");
        let r = std::fs::create_dir_all(paths::data_dir())
            .and_then(|_| std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(on_disk)).unwrap_or_default()))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = r {
            self.say(format!("Couldn't save settings: {e}"), "pk");
        }
    }

    pub fn say(&mut self, text: impl Into<String>, tags: &str) {
        self.flash = Some((text.into(), tags.to_string(), Instant::now()));
        self.dirty = true;
    }

    fn fail(&mut self, prefix: &str, e: anyhow::Error) {
        self.say(format!("{prefix}{}", anyhow_msg(&e)), "pk b");
    }

    pub fn sel_key(&self) -> String {
        match self.screen {
            Screen::List => self.channel.to_string(),
            s => format!("{s:?}").to_lowercase(),
        }
    }

    /// What the current screen needs from the board. The top line shows the Concierge's held item from the work list on every
    /// screen, so that list is always loaded; the other three only where something reads them.
    fn want(&self) -> Want {
        let mut rows = [false, false, false, true];
        match self.screen {
            Screen::Main => rows = [true; 4],
            Screen::List | Screen::Read => {
                if let Some(i) = state::CHANNELS.iter().position(|c| *c == self.channel) {
                    rows[i] = true;
                }
            }
            _ => {}
        }
        Want {
            archived: self.show_archived,
            decided: self.show_decided,
            rows,
            read: self.read_tid.filter(|_| self.screen == Screen::Read),
            goal: self.goal_name.clone().filter(|_| self.screen == Screen::Goal),
        }
    }

    /// Reads the board again, here and now, exactly (no reused budget): for after something was written.
    pub fn reload(&mut self) {
        self.data_version = self.db.data_version();
        self.last_load = Instant::now();
        self.epoch += 1;
        let fresh = Fresh::load(&self.db, self.want(), None, None);
        self.apply(fresh);
    }

    /// The same for moving to another screen: the slow-moving budget may be up to ten seconds old. `known` is the open thread when
    /// the caller has just loaded it.
    fn reload_nav(&mut self, known: Option<Detail>) {
        self.data_version = self.db.data_version();
        self.last_load = Instant::now();
        self.epoch += 1;
        let fresh = Fresh::load(&self.db, self.want(), Some(&mut self.memo), known);
        self.apply(fresh);
    }

    fn apply(&mut self, mut f: Fresh) {
        for ch in state::CHANNELS {
            if !f.st.rows.contains_key(ch) {
                if let Some(old) = self.st.rows.remove(ch) {
                    f.st.rows.insert(ch.into(), old);
                }
            }
        }
        self.st = f.st;
        self.update = f.update;
        self.bio_cache.borrow_mut().clear();
        if let (Some(id), Some(detail)) = (f.want.read, f.detail) {
            self.detail = detail;
            let count = self.detail.as_ref().map(|d| d.messages.len()).unwrap_or(0);
            if self.reader_key.map(|(t, c)| t != id || c < count).unwrap_or(true) {
                self.scroll_end = true;
            }
            self.reader_key = Some((id, count));
        }
        if let Some(doc) = f.goal_doc {
            self.goal_doc = doc;
        }
        if self.scroll_end {
            self.scroll = usize::MAX / 2;
            self.scroll_end = false;
        }
        self.dirty = true;
    }

    // ---- refreshing in the background

    /// Starts the worker that loads refreshes on its own connection, so a slow query never holds up a key.
    fn start_refresher(&mut self, db_path: std::path::PathBuf) {
        let (ask, asked) = mpsc::channel::<Want>();
        let (reply, answer) = mpsc::channel::<Fresh>();
        let spawned = std::thread::Builder::new().name("window-refresh".into()).spawn(move || {
            let Ok(db) = Db::open(&db_path) else { return };
            let mut memo = state::Memo::default();
            while let Ok(mut want) = asked.recv() {
                while let Ok(newer) = asked.try_recv() {
                    want = newer; // only the latest ask matters
                }
                let mut fresh = Fresh::load(&db, want, Some(&mut memo), None);
                fresh.sig = fresh.signature();
                if reply.send(fresh).is_err() {
                    return;
                }
            }
        });
        if spawned.is_ok() {
            self.refresher = Some(Refresher { ask, answer, out: None, started: Instant::now() - MIN_GAP, stale: false, sig: 0 });
        }
    }

    /// Takes in a finished refresh and, if the board changed or it is time, asks for the next. Never waits.
    fn pump_refresh(&mut self) {
        let Some(mut r) = self.refresher.take() else { return };
        match r.answer.try_recv() {
            Ok(f) => {
                let asked_at = r.out.take();
                if f.want != self.want() || asked_at != Some(self.epoch) {
                    r.stale = true; // the screen moved on, or the window reloaded in place (after a write), while it was loading
                } else if f.sig != r.sig {
                    r.sig = f.sig;
                    self.apply(f);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return, // the worker is gone: reload in place from now on
        }
        let editing = matches!(self.screen, Screen::Compose | Screen::Ask);
        if r.out.is_none() && !editing && r.started.elapsed() >= MIN_GAP {
            let version = self.db.data_version();
            if version != self.data_version || r.stale || self.last_load.elapsed() > MAX_AGE {
                let want = self.want();
                self.data_version = version;
                self.last_load = Instant::now();
                r.started = Instant::now();
                r.stale = false;
                r.out = Some(self.epoch);
                if r.ask.send(want).is_err() {
                    return;
                }
            }
        }
        self.refresher = Some(r);
    }

    /// Without a worker (the headless snapshot, the tests) a changed board is reloaded in place, as before.
    fn pump_refresh_inline(&mut self) {
        let editing = matches!(self.screen, Screen::Compose | Screen::Ask);
        if !editing && (self.db.data_version() != self.data_version || self.last_load.elapsed() > MAX_AGE) {
            self.reload_nav(None);
        }
    }

    fn clock(&self) -> (i64, i64) {
        #[cfg(test)]
        if let Some(c) = self.fixed_clock {
            return c;
        }
        let now = chrono::Utc::now().timestamp();
        (now / 60, if self.screen == Screen::Main { now / 6 } else { 0 })
    }

    /// One pass of the loop's housekeeping, after the events were handled: dictation, the quit timer, draft saving, the board,
    /// and the things that change the screen by themselves (a flash message ending, the clock moving on a minute). Returns
    /// whether the screen needs drawing; an unchanged screen costs no frame at all.
    pub fn tick(&mut self) -> bool {
        if self.dictating && self.last_dictation.elapsed() >= Duration::from_millis(250) {
            self.last_dictation = Instant::now();
            self.dictation_tick();
        }
        if self.quit_at.map(|t| Instant::now() >= t).unwrap_or(false) {
            self.quit = true;
        }
        if self.runner.as_mut().is_some_and(|r| r.pump()) {
            self.dirty = true;
        }
        if self.last_save.elapsed() > Duration::from_secs(5) {
            self.save_drafts();
            self.last_save = Instant::now();
        }
        if self.refresher.is_some() {
            self.pump_refresh();
        } else {
            self.pump_refresh_inline();
        }
        if self.flash.as_ref().is_some_and(|(_, _, t)| t.elapsed() > FLASH) {
            self.flash = None;
            self.dirty = true;
        }
        let clock = self.clock();
        if clock != self.time_key {
            self.time_key = clock;
            self.dirty = true;
        }
        std::mem::take(&mut self.dirty)
    }

    /// How long the loop may sleep before something needs its attention.
    pub fn next_wake(&self) -> Duration {
        let mut d = Duration::from_millis(if self.refresher.is_some() { 100 } else { 250 });
        if self.dictating {
            d = d.min(Duration::from_millis(250));
        }
        if let Some((_, _, t)) = &self.flash {
            d = d.min(FLASH.saturating_sub(t.elapsed()) + Duration::from_millis(5));
        }
        if let Some(t) = self.quit_at {
            d = d.min(t.saturating_duration_since(Instant::now()));
        }
        d.max(Duration::from_millis(1))
    }

    fn item_count(&self) -> usize {
        match self.screen {
            Screen::List => self.rows_of(self.channel).len(),
            Screen::Prs => self.prs_shown().len(),
            Screen::Jobs => self.st.jobs.len(),
            Screen::Rules => self.rules_shown().len(),
            Screen::Who => self.callers().len(),
            Screen::Options => self.option_items().len(),
            Screen::Agents => self.entries().len(),
            Screen::Adopt => self.adoptables.len(),
            _ => 0,
        }
    }

    fn sel_now(&self) -> usize {
        self.sel.get(&self.sel_key()).copied().unwrap_or(0).min(self.item_count().saturating_sub(1))
    }

    fn goto(&mut self, to: Screen, ch: Option<&'static str>) {
        if to != Screen::Read {
            self.set_draft_aside();
        }
        if let Some(c) = ch {
            self.channel = c;
        }
        self.screen = to;
        self.confirm = None;
        self.discard_armed = None;
        self.scroll = 0;
        if to == Screen::Adopt {
            self.adoptables = crate::adopt::adoptable();
        }
        if to != Screen::Agents && to != Screen::Adopt {
            self.adopt_note = None;
        }
        if to != Screen::Read {
            self.read_tid = None;
            self.detail = None;
        }
        if to == Screen::Compose {
            if let Some((s, b)) = self.compose_draft.take() {
                self.subject = s;
                self.reply = Editor::from(&b);
            }
            self.subject_focus = self.subject.is_empty();
        }
        self.reload_nav(None);
    }

    fn open_thread(&mut self, tid: i64, back: Screen) {
        self.set_draft_aside();
        let Some(d) = Detail::load(&self.db, tid) else {
            self.say(format!("#{tid} is no longer on the board."), "mu");
            return;
        };
        if let Some(c) = state::CHANNELS.iter().find(|c| **c == d.thread.channel) {
            self.channel = c;
        }
        self.read_tid = Some(tid);
        self.read_back = back;
        self.reply = Editor::from(self.drafts.get(&tid).map(|s| s.as_str()).unwrap_or(""));
        self.reader_key = None;
        self.screen = Screen::Read;
        self.scroll = usize::MAX / 2;
        self.reload_nav(Some(d));
    }

    /// The reply being typed belongs to the thread it was typed on, and an unsent new post to its screen: put each where it
    /// can come back, and empty the boxes they shared.
    fn set_draft_aside(&mut self) {
        match (self.screen, self.read_tid) {
            (Screen::Read, Some(tid)) => {
                if self.reply.is_blank() {
                    self.drafts.remove(&tid);
                } else {
                    self.drafts.insert(tid, self.reply.text());
                }
                self.reply = Editor::new();
            }
            (Screen::Compose, _) => {
                self.compose_draft = if self.subject.is_empty() && self.reply.is_blank() { None } else { Some((self.subject.clone(), self.reply.text())) };
                self.subject.clear();
                self.reply = Editor::new();
            }
            _ => {}
        }
        self.save_drafts();
    }

    fn drafts_file(&self) -> std::path::PathBuf {
        paths::data_dir().join("drafts.json")
    }

    fn load_drafts(&mut self) {
        let Some(v) = std::fs::read_to_string(self.drafts_file()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
        if let Some(o) = v["replies"].as_object() {
            self.drafts = o.iter().filter_map(|(k, t)| Some((k.parse().ok()?, t.as_str()?.to_string()))).collect();
        }
        if let (Some(s), Some(b)) = (v["compose"]["subject"].as_str(), v["compose"]["body"].as_str()) {
            self.compose_draft = Some((s.into(), b.into()));
        }
    }

    pub fn save_drafts(&self) {
        if cfg!(test) {
            return;
        }
        let mut replies: HashMap<String, String> = self.drafts.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        if let (Screen::Read, Some(t)) = (self.screen, self.read_tid) {
            if !self.reply.is_blank() {
                replies.insert(t.to_string(), self.reply.text());
            }
        }
        let compose = if self.screen == Screen::Compose && !(self.subject.is_empty() && self.reply.is_blank()) {
            Some((self.subject.clone(), self.reply.text()))
        } else {
            self.compose_draft.clone()
        };
        let doc = json!({"replies": replies, "compose": compose.map(|(s, b)| json!({"subject": s, "body": b}))});
        let _ = std::fs::create_dir_all(paths::data_dir());
        let _ = std::fs::write(self.drafts_file(), doc.to_string());
    }

    fn go_back(&mut self) {
        if self.screen == Screen::Compose && !(self.subject.is_empty() && self.reply.is_blank()) {
            let now = (self.subject.clone(), self.reply.text());
            if self.discard_armed.as_ref() != Some(&now) {
                self.discard_armed = Some(now);
                self.say("Esc again to throw this post away.", "ye");
                return;
            }
            self.subject.clear();
            self.reply = Editor::new();
        }
        if self.screen == Screen::Ask {
            self.subject.clear();
        }
        let to = match self.screen {
            Screen::Read => self.read_back,
            Screen::Compose => self.compose_back,
            Screen::Ask => self.ask.as_ref().map(|a| a.back).unwrap_or(Screen::Main),
            Screen::Adopt | Screen::Goal => Screen::Agents,
            _ => Screen::Main,
        };
        self.goto(to, None);
    }

    // ---- keys

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some((_, action)) = self.confirm.take() {
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.run_confirm(action);
            } else if !matches!(k.code, KeyCode::Modifier(_)) {
                self.say("Never mind, then.", "mu");
            }
            return;
        }
        if ctrl {
            match k.code {
                KeyCode::Char('s') | KeyCode::Enter | KeyCode::Char('j') if matches!(self.screen, Screen::Read | Screen::Compose) => self.send(),
                KeyCode::Char('w') if self.screen != Screen::Read && self.screen != Screen::Compose => self.toggle_concierge(),
                KeyCode::Char('r') => self.wake_selected(),
                KeyCode::Char('d') => self.toggle_dictation(),
                _ => {}
            }
            if !(ctrl && matches!(k.code, KeyCode::Char('u'))) || !matches!(self.screen, Screen::Read | Screen::Compose) {
                return;
            }
        }
        if self.dictating && matches!(k.code, KeyCode::Esc | KeyCode::Enter) {
            if k.code == KeyCode::Esc {
                let _ = crate::pyhost::call("dictate.stop", json!({}));
                self.dictating = false;
                self.say("Dictation cancelled.", "mu");
            } else {
                self.say("Ctrl+D stops dictating first.", "ye");
            }
            return;
        }
        match self.screen {
            Screen::Read => return self.read_key(k, alt),
            Screen::Compose => return self.compose_key(k),
            Screen::Ask => return self.ask_key(k),
            _ => {}
        }
        if k.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        let n = self.item_count();
        match k.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::PageUp if n > 0 => self.step(-(self.body_h.saturating_sub(7).max(5) as i64)),
            KeyCode::PageDown if n > 0 => self.step(self.body_h.saturating_sub(7).max(5) as i64),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.body_h.saturating_sub(2).max(3)),
            KeyCode::PageDown => self.scroll += self.body_h.saturating_sub(2).max(3),
            KeyCode::Home if n > 0 => self.step(-10_000),
            KeyCode::End if n > 0 => self.step(10_000),
            KeyCode::Left | KeyCode::Right if self.screen == Screen::Options => {
                self.change_option(if k.code == KeyCode::Left { -1 } else { 1 }, k.modifiers.contains(KeyModifiers::SHIFT))
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.screen != Screen::Main => self.activate(),
            KeyCode::Char(c) => self.letter(c.to_ascii_lowercase()),
            _ => {}
        }
    }

    fn step(&mut self, delta: i64) {
        if self.item_count() == 0 {
            if delta < 0 {
                self.scroll = self.scroll.saturating_sub(1);
            } else {
                self.scroll += 1;
            }
            return;
        }
        let key = self.sel_key();
        let max = self.item_count() as i64 - 1;
        let now = self.sel_now() as i64;
        self.sel.insert(key, (now + delta).clamp(0, max) as usize);
    }

    fn letter(&mut self, ch: char) {
        let s = self.screen;
        match (s, ch) {
            (Screen::Jobs, 'r' | 'e' | 'x' | 'n') => return self.job_key(ch),
            (Screen::Rules, 'a' | 'r' | 'v' | 'h') => return self.rule_key(ch),
            _ => {}
        }
        if let Some(c) = match ch {
            'q' => Some("question"),
            'd' => Some("discussion"),
            'w' => Some("wiki"),
            'j' => Some("work"),
            _ => None,
        } {
            return self.goto(Screen::List, Some(c));
        }
        match (s, ch) {
            (Screen::List, 'n') => {
                self.compose_back = Screen::List;
                self.goto(Screen::Compose, None);
                self.subject_focus = true;
            }
            (Screen::List, 'h') if self.channel == "question" => {
                self.show_archived = !self.show_archived;
                self.sel.insert("question".into(), 0);
                self.reload();
            }
            (Screen::List, 'u') if self.channel == "question" && self.show_archived => {
                if let Some(r) = self.rows_of("question").get(self.sel_now()).map(|r| r.id) {
                    self.unarchive(r);
                }
            }
            (Screen::Prs, 'c') => {
                self.say("Checking GitHub for merges...", "cy");
                let n = prs::check(&self.db);
                self.say(format!("Checked: {n} settled."), "gr");
                self.reload();
            }
            (Screen::Prs, 'h') => {
                self.show_settled = !self.show_settled;
                self.sel.insert("prs".into(), 0);
            }
            (Screen::Prs, 'o') => self.activate(),
            (Screen::Who, 'p') => self.page(),
            (Screen::Agents | Screen::Goal, '+' | '=') => self.members_delta(1),
            (Screen::Agents | Screen::Goal, '-' | '_') => self.members_delta(-1),
            (Screen::Agents | Screen::Goal, ']') => self.members_delta(10),
            (Screen::Agents | Screen::Goal, '[') => self.members_delta(-10),
            (Screen::Agents, 'n') => self.new_agent(),
            (Screen::Agents, 'g') => self.new_goal(),
            (Screen::Agents, 'f') => self.forget_selected(),
            (Screen::Agents, 'a') if !self.sel_goal().is_some_and(|g| !g.standing && !matches!(g.state.as_str(), "running" | "succeeded" | "stopped")) => {
                self.goto(Screen::Adopt, None)
            }
            (Screen::Agents | Screen::Goal, 'a') => self.approve_selected(),
            (Screen::Agents | Screen::Goal, 'x') => self.stop_goal_selected(),
            (Screen::Agents | Screen::Goal, 's') => self.start_stop_selected(),
            (Screen::Agents | Screen::Goal, 'l') => self.attach_lead(),
            (Screen::Options, 'c') => self.open_console(),
            (Screen::Sysop, 'l') => match self.held_row() {
                Some(h) => self.open_thread(h.id, Screen::Sysop),
                None => self.say("The Concierge holds nothing right now.", "mu"),
            },
            (_, 'a') => self.goto(Screen::Agents, None),
            (_, 'p') => self.goto(Screen::Prs, None),
            (_, 's') => self.goto(Screen::Sysop, None),
            (_, 'b') => self.goto(Screen::Who, None),
            (_, 'o') => self.goto(Screen::Options, None),
            (_, 'r') => self.goto(Screen::Jobs, None),
            (_, 'x') => self.goto(Screen::Rules, None),
            (_, 'm') => self.goto(Screen::Main, None),
            (_, 't') => self.next_theme(1),
            (Screen::Main, 'g') => {
                self.say("+++ATH0 · NO CARRIER", "or b");
                self.quit_at = Some(Instant::now() + Duration::from_millis(350));
            }
            _ => {}
        }
    }

    fn next_theme(&mut self, step: i64) {
        let all = palettes();
        let i = (self.theme_index() as i64 + step).rem_euclid(all.len() as i64) as usize;
        self.theme = all[i].0.to_string();
        self.pal = self.palette_for(&self.theme);
        self.dirty = true;
        let t = self.theme.clone();
        self.set_pref("theme", json!(t));
    }

    fn activate(&mut self) {
        let sel = self.sel_now();
        match self.screen {
            Screen::List => {
                if let Some(id) = self.rows_of(self.channel).get(sel).map(|r| r.id) {
                    self.open_thread(id, Screen::List);
                }
            }
            Screen::Prs => {
                if let Some(url) = self.prs_shown().get(sel).map(|p| p.url.clone()) {
                    open_url(&url);
                    self.say("Opened on GitHub. The list clears itself once it's merged.", "cy");
                }
            }
            Screen::Who => {
                let name = self.callers().get(sel).map(|c| c.author.clone());
                if let Some(name) = name {
                    match self.st.bios.get(&name.to_lowercase()).copied() {
                        Some(t) => self.open_thread(t, Screen::Who),
                        None => self.say(format!("{name} hasn't posted a bio."), "mu"),
                    }
                }
            }
            Screen::Options => self.change_option(0, false),
            Screen::Rules => {
                let tid = self.rules_shown().get(sel).map(|r| r.thread_id);
                match tid {
                    Some(Some(t)) => self.open_thread(t, Screen::Rules),
                    Some(None) => self.say("That proposal has no thread.", "mu"),
                    None => {}
                }
            }
            Screen::Agents => {
                if let Some(g) = self.sel_goal() {
                    self.goal_name = Some(g.name);
                    self.goal_doc = None;
                    self.goto(Screen::Goal, None);
                } else if let Some(a) = self.sel_agent() {
                    self.attach_agent(&a);
                }
            }
            Screen::Adopt => self.adopt_pick(),
            _ => {}
        }
    }

    fn change_option(&mut self, delta: i64, shift: bool) {
        let items = self.option_items();
        let Some((_, _, key)) = items.get(self.sel_now()) else { return };
        let step = if delta == 0 { 1 } else { delta };
        match *key {
            "max_sessions" => {
                let n = (self.pref_i("max_sessions", 3) + step * if shift { 10 } else { 1 }).max(1);
                self.set_pref("max_sessions", json!(n));
                self.say(format!("Up to {n} agent session(s) at once. Takes effect when the session host starts them."), "ye");
            }
            "concierge" => {
                if delta == 0 {
                    self.toggle_concierge();
                }
            }
            "preroll" => {
                let on = !self.pref_b("preroll", true);
                self.set_pref("preroll", json!(on));
                self.say(
                    if on { "Pre-roll on: the mic keeps a 2-second rolling buffer while you're in a box." } else { "Pre-roll off: the mic only opens when you press Ctrl+D." },
                    "ye",
                );
            }
            "web" => self.open_console(),
            "theme" => self.next_theme(step),
            "toasts" => {
                let on = !self.pref_b("toasts", true);
                self.set_pref("toasts", json!(on));
                self.say(if on { "Toasts on." } else { "Toasts off." }, "ye");
            }
            "update" if delta == 0 => {
                self.say("Checking GitHub...", "cy");
                match update::check() {
                    Ok((latest, true)) => match update::apply() {
                        Ok(v) => self.say(format!("Updated to {v} (was {latest} available). Restart agentdesk to use it."), "gr"),
                        Err(e) => self.fail("Not updated: ", e),
                    },
                    Ok((latest, false)) => self.say(format!("v{} is current (latest release {latest}).", update::CURRENT), "gr"),
                    Err(e) => self.fail("Couldn't check: ", e),
                }
                self.update = update::pending();
            }
            _ => {}
        }
    }

    fn unarchive(&mut self, tid: i64) {
        match self.db.unarchive(tid) {
            Ok(true) => {
                self.show_archived = false;
                self.reload();
                self.say(format!("#{tid} is back on the desk."), "gr");
            }
            Ok(false) => self.say(format!("Nothing to bring back: #{tid} is not archived."), "ye"),
            Err(e) => self.fail("Not unarchived: ", e),
        }
    }

    fn page(&mut self) {
        let Some(name) = self.callers().get(self.sel_now()).map(|c| c.author.clone()) else { return };
        self.compose_back = Screen::Who;
        self.channel = "discussion";
        self.goto(Screen::Compose, Some("discussion"));
        if self.subject.is_empty() && self.reply.is_blank() {
            self.subject = format!("page: {name}");
            self.reply = Editor::from(&format!("@{name} "));
            self.subject_focus = false;
        } else {
            self.say("Your unsent post is still here. Post it, or Esc twice throws it away, then P again.", "ye");
        }
    }

    fn job_key(&mut self, ch: char) {
        if ch == 'n' {
            return self.new_job();
        }
        let Some(j) = self.st.jobs.get(self.sel_now()).cloned() else { return };
        match ch {
            'e' => match jobs::enable(&self.db, &j.name, !j.enabled) {
                Ok(_) => {
                    self.reload();
                    self.say(if j.enabled { format!("{} is off.", j.name) } else { format!("{} is on, as scheduled.", j.name) }, "gr");
                }
                Err(e) => self.fail("Not done: ", e),
            },
            'r' => self.confirm = Some((format!("Run {} now? It starts a session at once; the schedule is unchanged.", j.name), Confirm::RunJob(j.name))),
            _ => self.confirm = Some((format!("Delete {}? The schedule goes.", j.name), Confirm::DeleteJob(j.name))),
        }
    }

    fn rule_key(&mut self, ch: char) {
        if ch == 'h' {
            self.show_decided = !self.show_decided;
            self.sel.insert("rules".into(), 0);
            return self.reload();
        }
        let Some(p) = self.rules_shown().get(self.sel_now()).map(|r| (*r).clone()) else { return };
        if (matches!(ch, 'a' | 'r') && p.status != "pending") || (ch == 'v' && p.status != "approved") {
            return self.say(if ch == 'v' { "V reverts an approved rule." } else { "That one is already decided." }, "mu");
        }
        let text = match ch {
            'a' => format!(
                "Allow {} in {}?{}",
                p.rule,
                if p.scope == "user" { "your Claude settings" } else { "the project's settings" },
                if p.broad { " It is BROAD: every use of the tool." } else { "" }
            ),
            'r' => format!("Reject {}? {} is told not to retry.", p.rule, p.requested_by),
            _ => format!("Take {} back out of settings.json?", p.rule),
        };
        self.confirm = Some((text, Confirm::Rule(p.id, ch)));
    }

    fn run_confirm(&mut self, action: Confirm) {
        match action {
            Confirm::RunJob(name) => match jobs::run_now(&self.db, &name) {
                Ok(j) => self.say(format!("{name}: {}", j["last_status"].as_str().unwrap_or("started")), "gr"),
                Err(e) => self.fail("Not done: ", e),
            },
            Confirm::DeleteJob(name) => match jobs::delete(&self.db, &name) {
                Ok(_) => self.say(format!("{name} deleted."), "gr"),
                Err(e) => self.fail("Not done: ", e),
            },
            Confirm::Rule(id, ch) => {
                let r = if ch == 'v' { permissions::revert(&self.db, id) } else { permissions::decide(&self.db, id, ch == 'a') };
                match r {
                    Ok(p) => {
                        let who = p["requested_by"].as_str().unwrap_or("the agent").to_string();
                        self.say(
                            match ch {
                                'a' => format!("Allowed: {}. {who} is told on the thread.", p["rule"].as_str().unwrap_or("")),
                                'r' => "Rejected.".into(),
                                _ => "Reverted.".into(),
                            },
                            "gr",
                        )
                    }
                    Err(e) => self.fail("Not done: ", e),
                }
            }
            Confirm::CloseThread(tid) => match self.db.close_question(tid) {
                Ok(true) => self.say(format!("#{tid} closed. The sweep files it."), "gr"),
                Ok(false) => self.say(format!("Nothing to close: #{tid} is not an open question."), "ye"),
                Err(e) => self.fail("Not closed: ", e),
            },
            Confirm::Forget(name) => match crate::identities::forget(&self.db, &name) {
                Ok(_) => self.say(format!("{name} forgotten."), "gr"),
                Err(e) => self.fail("Not forgotten: ", e),
            },
            Confirm::StopGoal(name) => match crate::goals::stop(&self.db, &john(), &name) {
                Ok(_) => self.say(format!("{name} stopped. A starts it again."), "gr"),
                Err(e) => self.fail("Not stopped: ", e),
            },
            Confirm::ConciergeOff => match crate::concierge::toggle(&self.db, &john(), &concierge_folder(), Some(false)) {
                Ok(_) => self.say("Concierge off: its swarm is stopped and the items it held go back on the queue.", "ye"),
                Err(e) => self.fail("", e),
            },
        }
        self.reload();
    }

    fn send(&mut self) {
        let body = self.reply.text().trim().to_string();
        if body.is_empty() {
            return self.say("Nothing to send. The line stays quiet.", "mu");
        }
        match self.screen {
            Screen::Compose => {
                let subject = if self.subject.trim().is_empty() { "(no subject)".to_string() } else { self.subject.trim().to_string() };
                match self.db.start_thread(self.channel, &subject, crate::db::JOHN, crate::db::HUMAN, &body, None, None) {
                    Ok(tid) => {
                        self.subject.clear();
                        self.reply = Editor::new();
                        self.compose_draft = None;
                        self.screen = Screen::List;
                        self.open_thread(tid, Screen::List);
                        self.say(format!("Posted #{tid}."), "gr");
                    }
                    Err(e) => self.fail("Not sent: ", e),
                }
            }
            Screen::Read => {
                let Some(tid) = self.read_tid else { return };
                match self.db.john_replies(tid, &body) {
                    Ok(_) => {
                        self.drafts.remove(&tid);
                        self.reply = Editor::new();
                        self.scroll_end = true;
                        self.reload();
                        self.say(format!("Sent to #{tid}."), "gr");
                    }
                    Err(e) => self.fail("Not sent: ", e),
                }
            }
            _ => {}
        }
    }

    fn read_key(&mut self, k: KeyEvent, alt: bool) {
        match k.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.body_h.saturating_sub(2).max(3)),
            KeyCode::PageDown => self.scroll += self.body_h.saturating_sub(2).max(3),
            KeyCode::Up if k.modifiers.contains(KeyModifiers::CONTROL) => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down if k.modifiers.contains(KeyModifiers::CONTROL) => self.scroll += 1,
            KeyCode::Char('n') | KeyCode::Char('p') if alt => self.reader_step(if k.code == KeyCode::Char('n') { 1 } else { -1 }),
            KeyCode::Char('c') if alt => self.reader_close(),
            KeyCode::Char('r') if alt => self.runner_key_r(),
            KeyCode::Char('k') if alt => self.runner_kill(),
            KeyCode::Char('y') if alt => self.runner_yank(),
            KeyCode::Char('x') if alt => self.runner_close(),
            KeyCode::Char(d @ '1'..='9') if alt && self.runner.is_some() => {
                let ok = self.runner.as_mut().is_some_and(|r| r.pick(d as usize - '1' as usize));
                if !ok {
                    self.say("No such script in this thread (or one is running).", "mu");
                }
            }
            KeyCode::Char('u') if alt => {
                if let (Some(t), "question") = (self.read_tid, self.channel) {
                    self.unarchive(t);
                    if let Some(id) = self.read_tid {
                        self.detail = Detail::load(&self.db, id);
                    }
                }
            }
            _ if alt => {}
            _ => self.reply.key(k),
        }
    }

    /// Alt+R: the first press opens the runner pane on the thread's scripts and shows the picked one; the next runs it.
    fn runner_key_r(&mut self) {
        if self.runner.is_none() {
            let msgs: Vec<(String, String)> = self.detail.as_ref().map(|d| d.messages.iter().map(|m| (m.author.clone(), m.body.clone())).collect()).unwrap_or_default();
            let blocks = runner::extract_blocks(&msgs);
            if blocks.is_empty() {
                return self.say("No script in this thread: it needs a fenced ```powershell, ```bash, ```sh or ```cmd block.", "mu");
            }
            self.runner = Some(runner::Runner::new(blocks));
            return self.say("The script is shown, not run. Alt+R again runs it; Alt+1..9 picks another; Alt+X closes.", "cy");
        }
        let started = self.runner.as_mut().is_some_and(|r| r.run());
        if !started {
            self.say("Already running (Alt+K stops it).", "mu");
        }
        self.dirty = true;
    }

    fn runner_kill(&mut self) {
        if self.runner.as_mut().is_some_and(|r| r.kill()) {
            self.say("Stopped.", "ye");
        }
    }

    fn runner_yank(&mut self) {
        let Some(r) = &self.runner else { return };
        let text = r.summary(30);
        self.reply.insert_str(&text);
        self.say("The output is in your reply: edit it, then Ctrl+S sends.", "gr");
    }

    fn runner_close(&mut self) {
        if let Some(mut r) = self.runner.take() {
            r.kill();
        }
        self.dirty = true;
    }

    fn reader_step(&mut self, delta: i64) {
        let rs = self.rows_of(self.channel);
        let Some(idx) = rs.iter().position(|r| Some(r.id) == self.read_tid) else { return };
        let j = (idx as i64 + delta).clamp(0, rs.len() as i64 - 1) as usize;
        if j == idx {
            return self.say(if delta > 0 { "That's the last one." } else { "That's the first one." }, "mu");
        }
        let (id, back) = (rs[j].id, self.read_back);
        self.sel.insert(self.channel.to_string(), j);
        self.open_thread(id, back);
    }

    fn reader_close(&mut self) {
        match (self.channel, self.read_tid) {
            ("question", Some(tid)) => self.confirm = Some((format!("Close & archive #{tid}? It goes to the vault on the next sweep."), Confirm::CloseThread(tid))),
            _ => self.say("Only questions can be closed & archived.", "mu"),
        }
    }

    fn compose_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Tab | KeyCode::BackTab => self.subject_focus = !self.subject_focus,
            _ if self.subject_focus => match k.code {
                KeyCode::Enter => self.subject_focus = false,
                KeyCode::Backspace => {
                    self.subject.pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => self.subject.push(c),
                _ => {}
            },
            KeyCode::PageUp | KeyCode::PageDown => {}
            _ => self.reply.key(k),
        }
    }

    // ---- dictation: local speech-to-text through the Python plugin

    fn insert_dictated(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match self.screen {
            Screen::Read => self.reply.insert_str(&format!("{text} ")),
            Screen::Compose if !self.subject_focus => self.reply.insert_str(&format!("{text} ")),
            Screen::Compose | Screen::Ask => {
                self.subject.push_str(text);
            }
            _ => {}
        }
    }

    pub fn toggle_dictation(&mut self) {
        if !matches!(self.screen, Screen::Read | Screen::Compose | Screen::Ask) {
            return self.say("Ctrl+D dictates into a text box: open a reply or a new post first.", "ye");
        }
        if self.dictating {
            let _ = crate::pyhost::call("dictate.stop", json!({}));
            let end = std::time::Instant::now() + Duration::from_secs(8);
            let mut said = String::new();
            while std::time::Instant::now() < end {
                match crate::pyhost::call("dictate.poll", json!({})) {
                    Ok(v) => {
                        said = v["text"].as_str().unwrap_or("").to_string();
                        if v["state"].as_str() == Some("done") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            self.dictating = false;
            self.insert_dictated(&said);
            return self.say(if said.trim().is_empty() { "Dictation: nothing heard.".to_string() } else { "Dictation: done. Ctrl+D again to add more.".to_string() }, "gr");
        }
        match crate::pyhost::call("dictate.start", json!({})) {
            Ok(v) if v["state"].as_str() != Some("done") => {
                self.dictating = true;
                let what = match v["state"].as_str() {
                    Some("downloading") => "downloading the speech model (first use)",
                    Some("loading") => "loading the speech model",
                    _ => "listening",
                };
                let mic = v["device"].as_str().map(|d| format!(" on {d}")).unwrap_or_default();
                self.say(format!("Dictation: {what}{mic}... Ctrl+D stops, Esc cancels."), "cy");
            }
            Ok(v) => self.say(format!("Dictation: {}", v["error"].as_str().unwrap_or("not available")), "pk b"),
            Err(e) => self.say(format!("Dictation: {e}"), "pk b"),
        }
    }

    /// While dictating: the words so far, shown as they come.
    pub fn dictation_tick(&mut self) {
        if !self.dictating {
            return;
        }
        match crate::pyhost::call("dictate.poll", json!({})) {
            Ok(v) => {
                if v["state"].as_str() == Some("done") {
                    self.dictating = false;
                    let said = v["text"].as_str().unwrap_or("").to_string();
                    self.insert_dictated(&said);
                    let msg = v["error"].as_str().map(|e| format!("Dictation: {e}")).unwrap_or_else(|| "Dictation: done.".to_string());
                    self.say(msg, "ye");
                } else if let Some(t) = v["text"].as_str().filter(|t| !t.is_empty()) {
                    let tail: String = t.chars().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect();
                    self.say(format!("Dictation: …{tail}"), "cy");
                }
            }
            Err(_) => self.dictating = false,
        }
    }

    // ---- the swarm: agents, goals, the Concierge

    fn toggle_concierge(&mut self) {
        if self.st.concierge_on {
            self.confirm = Some(("Turn the Concierge off? Its swarm is stopped and the items it held go back on the queue.".into(), Confirm::ConciergeOff));
            return;
        }
        match crate::concierge::toggle(&self.db, &john(), &concierge_folder(), Some(true)) {
            Ok(_) => {
                self.reload();
                self.say("Turning the Concierge on...", "ye");
            }
            Err(e) => self.fail("", e),
        }
        self.reload();
    }

    fn wake_selected(&mut self) {
        let tid = match self.screen {
            Screen::Read => self.read_tid,
            Screen::List if self.channel == "question" => self.rows_of("question").get(self.sel_now()).map(|r| r.id),
            _ => None,
        };
        let Some(tid) = tid else { return self.say("Ctrl+R wakes the agent on a question: pick one first.", "ye") };
        match crate::ui_api::wake(&self.db, tid) {
            Ok(d) => {
                let said = d["said"].as_str().unwrap_or("").to_string();
                self.say(said.clone(), if said.starts_with("woke") { "gr" } else { "ye" });
            }
            Err(e) => self.fail("Not woken: ", e),
        }
        self.reload();
    }

    fn start_stop_selected(&mut self) {
        let agent = self.sel_agent().or_else(|| self.sel_goal().and_then(|g| self.st.agents.iter().find(|a| a.name.eq_ignore_ascii_case(&g.lead)).cloned()));
        let Some(a) = agent else { return self.say("S starts or stops an agent, or a goal's lead. A goal starts with A and stops with X.", "ye") };
        let r = if a.state != "stopped" { crate::identities::stop(&self.db, &a.name) } else { crate::identities::start(&self.db, &a.name) };
        match r {
            Ok(mut row) => {
                if row["state"].as_str() == Some("queued") && crate::supervisor::running() {
                    for _ in 0..20 {
                        std::thread::sleep(Duration::from_millis(150));
                        if let Ok(Some(r)) = crate::identities::get(&self.db, &a.name) {
                            row = r;
                            if row["state"].as_str() != Some("queued") {
                                break;
                            }
                        }
                    }
                }
                let now = row["state"].as_str().unwrap_or("").to_string();
                self.say(
                    match now.as_str() {
                        "stopped" => format!("{} stopped. Its conversation is kept; S resumes it.", a.name),
                        "running" => format!("{} is running. ↵ attaches.", a.name),
                        _ => format!("{} is queued: it starts when the pool has a slot ({} at once).", a.name, self.pref_i("max_sessions", self.st.max_sessions)),
                    },
                    "ye",
                );
            }
            Err(e) => self.fail("Not done: ", e),
        }
        self.reload();
    }

    fn approve_selected(&mut self) {
        let Some(g) = self.sel_goal() else { return };
        if !g.standing && matches!(g.state.as_str(), "running" | "succeeded") {
            return self.say(format!("{} is already {}. X stops it.", g.name, g.state), "mu");
        }
        let r = if g.standing {
            crate::concierge::toggle(&self.db, &john(), &concierge_folder(), Some(true))
        } else {
            crate::goals::approve(&self.db, &john(), &g.name, None, None, None)
        };
        match r {
            Ok(_) => self.say(format!("{} is running: until its line, its budget, or X.", g.name), "gr"),
            Err(e) => self.fail("Not approved: ", e),
        }
        self.reload();
    }

    fn members_delta(&mut self, delta: i64) {
        let Some(g) = self.sel_goal() else { return self.say("+ and - set a goal's members: pick a goal's row.", "mu") };
        if g.standing {
            return self.say("The Concierge sizes its own swarm.", "mu");
        }
        let n = (g.max_members + delta).max(1);
        match crate::goals::set_budget(&self.db, &john(), &g.name, Some(n), None, None) {
            Ok(_) => self.say(format!("{} may now run {} at once. The total is still capped by Sessions at once (Options).", g.name, n_word_(n)), "gr"),
            Err(e) => self.fail("Not changed: ", e),
        }
        self.reload();
    }

    fn stop_goal_selected(&mut self) {
        let Some(g) = self.sel_goal() else { return self.say("X stops a goal: pick a goal's row. S stops an agent.", "mu") };
        if g.standing {
            return self.toggle_concierge();
        }
        self.confirm = Some((format!("Stop {}? Its members are forgotten and its lead stops; the log stays.", g.name), Confirm::StopGoal(g.name)));
    }

    fn forget_selected(&mut self) {
        let Some(a) = self.sel_agent() else { return self.say("F forgets an agent: pick an agent's row. A goal stops with X.", "mu") };
        self.confirm = Some((format!("Forget {}? It stops and leaves the list; its Claude conversation stays on disk.", a.name), Confirm::Forget(a.name)));
    }

    fn attach_lead(&mut self) {
        let Some(g) = self.sel_goal() else { return self.say("L attaches to a goal's lead: pick a goal's row.", "mu") };
        match self.st.agents.iter().find(|a| a.name.eq_ignore_ascii_case(&g.lead)).cloned() {
            Some(a) => self.attach_agent(&a),
            None => self.say(format!("{} isn't an agent now. A starts the goal and its lead.", g.lead), "ye"),
        }
    }

    fn attach_agent(&mut self, a: &state::Agent) {
        match a.state.as_str() {
            "running" => self.attach_request = Some(a.name.clone()),
            "queued" => self.say(format!("{} is waiting for a free slot.", a.name), "ye"),
            _ => self.say(format!("{} isn't running. S starts it.", a.name), "ye"),
        }
    }

    fn adopt_pick(&mut self) {
        let Some(c) = self.adoptables.get(self.sel_now()) else { return };
        let (sid, folder) = (c.session_id.clone(), c.folder.clone());
        let name = std::path::Path::new(&folder).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        self.ask_for(
            AskSpec {
                title: "Adopt a session".into(),
                banner: format!("ADOPT  ·  {folder}"),
                back: Screen::Adopt,
                fields: vec![("name".into(), "What to call it from now on: agentdesk attach <name> and its board posts use this.".into(), false)],
                done: AskDone::Adopt(sid),
            },
            &name,
        );
    }

    fn new_goal(&mut self) {
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        self.ask_for(
            AskSpec {
                title: "New goal".into(),
                banner: "NEW GOAL  ·  start a swarm  ·  its lead proposes a hypothesis, a measure and a line, then you approve".into(),
                back: Screen::Agents,
                fields: vec![
                    ("name".into(), "A short name: its lead is <name>-lead, and Slack's swarm slot uses it too.".into(), false),
                    ("folder".into(), format!("The folder the lead works and measures in. Enter for {cwd}."), true),
                    ("objective".into(), "What it should achieve, in a sentence. The lead turns it into a hypothesis.".into(), false),
                ],
                done: AskDone::NewGoal,
            },
            "",
        );
    }

    fn open_console(&mut self) {
        match crate::web::url() {
            Some(u) => {
                open_url(&u);
                self.say("Opened the ops console in your browser.", "cy");
            }
            None => self.say("The ops console isn't running: start `agentdesk daemon` (or the window).", "pk b"),
        }
    }

    // ---- ask: a few one-line questions, one at a time

    fn ask_for(&mut self, spec: AskSpec, prefill: &str) {
        self.ask = Some(spec);
        self.answers.clear();
        self.goto(Screen::Ask, None);
        self.subject = prefill.to_string();
    }

    fn new_job(&mut self) {
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        self.ask_for(
            AskSpec {
                title: "New job".into(),
                banner: "NEW RECURRING JOB  ·  a prompt that runs on a schedule, as a fresh headless claude session each time".into(),
                back: Screen::Jobs,
                fields: vec![
                    ("name".into(), "What to call it: letters, digits, - _ . only.".into(), false),
                    ("folder".into(), format!("The folder each run works in. Enter for {cwd}."), true),
                    ("prompt".into(), "What each run is told to do, in one line.".into(), false),
                    ("when".into(), "08:00 (daily), 08:00 weekdays, 09:30 mon,thu, or every 120 (minutes).".into(), false),
                    ("model".into(), "haiku, sonnet or opus. Enter for sonnet.".into(), true),
                ],
                done: AskDone::NewJob,
            },
            "",
        );
    }

    fn new_agent(&mut self) {
        self.ask_for(
            AskSpec {
                title: "New agent".into(),
                banner: "NEW AGENT  ·  sign up a long-lived agent  ·  it keeps one Claude conversation and hands off at 60%".into(),
                back: Screen::Agents,
                fields: vec![
                    ("name".into(), "What to call it: agentdesk attach <name> and its board posts use this.".into(), false),
                    ("folder".into(), "The folder it works in, e.g. ~/src/repo.".into(), false),
                    ("charter".into(), "Optional: what it is for, appended to its system prompt. Enter skips it.".into(), true),
                ],
                done: AskDone::NewAgent,
            },
            "",
        );
    }

    fn ask_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Backspace => {
                self.subject.pop();
            }
            KeyCode::Enter => self.ask_next(),
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => self.subject.push(c),
            _ => {}
        }
    }

    fn ask_next(&mut self) {
        let Some(spec) = self.ask.clone() else { return };
        let field = &spec.fields[self.answers.len()];
        let mut text = self.subject.trim().to_string();
        if text.is_empty() && field.0 == "folder" {
            text = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        }
        if text.is_empty() && !field.2 {
            return self.say(format!("It needs a {}. Esc cancels.", field.0), "ye");
        }
        self.answers.push(text);
        self.subject.clear();
        if self.answers.len() < spec.fields.len() {
            return;
        }
        let a = std::mem::take(&mut self.answers);
        self.go_back();
        match spec.done {
            AskDone::NewJob => {
                let (at, days, every) = parse_schedule(&a[3]);
                let folder = if a[1].is_empty() { std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default() } else { a[1].clone() };
                let model = if a[4].is_empty() { None } else { Some(a[4].as_str()) };
                match jobs::create(
                    &self.db,
                    jobs::NewJob { name: &a[0], folder: &folder, prompt: &a[2], at: at.as_deref(), days: days.as_deref(), every_minutes: every, model, catch_up_minutes: None },
                ) {
                    Ok(_) => {
                        self.reload();
                        self.say(format!("{} is scheduled.", a[0]), "gr");
                    }
                    Err(e) => self.fail("Not done: ", e),
                }
            }
            AskDone::NewGoal => {
                let name = crate::goals::slug(&a[0]);
                let folder = std::fs::canonicalize(&a[1]).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| a[1].clone());
                match crate::goals::create(&self.db, &name, &a[2], &folder) {
                    Ok(_) => {
                        self.reload();
                        self.say(format!("{name} is a draft. Its lead is proposing a hypothesis; A approves it."), "gr");
                    }
                    Err(e) => self.fail("Not done: ", e),
                }
            }
            AskDone::Adopt(sid) => match crate::adopt::adopt(&self.db, &sid, &a[0]) {
                Ok(row) => {
                    self.adopt_note = row["note"].as_str().map(String::from);
                    self.goto(Screen::Agents, None);
                    self.say(format!("{} adopted. Close that conversation in the Claude desktop app.", a[0]), "gr");
                }
                Err(e) => self.fail("Not adopted: ", e),
            },
            AskDone::NewAgent => {
                let ts = crate::db::now_iso();
                match self.db.exec(
                    "INSERT INTO identities (name, folder, charter, host, state, autostart, created_ts, updated_ts, generation) VALUES (:n,:f,:c,'local','stopped',0,:ts,:ts,1)",
                    &[(":n", &a[0]), (":f", &a[1]), (":c", &(if a[2].is_empty() { None } else { Some(a[2].clone()) })), (":ts", &ts)],
                ) {
                    Ok(_) => {
                        self.reload();
                        self.say(format!("{} is signed up. Starting it needs the session host.", a[0]), "gr");
                    }
                    Err(e) => self.fail("Not signed up: ", e),
                }
            }
        }
    }

    // ---- drawing

    pub fn draw(&mut self, f: &mut Frame) {
        let pal = self.palette();
        let area = f.area();
        if self.screen != Screen::Read && self.runner.is_some() {
            self.runner_close(); // the pane belongs to the reader
        }
        f.render_widget(Block::default().style(Style::default().bg(pal.color("bg")).fg(pal.color("fg"))), area);
        let input_h: u16 = match self.screen {
            Screen::Read => 5,
            Screen::Compose => 3 + 9,
            Screen::Ask => 3,
            _ => 0,
        }
        .min(area.height.saturating_sub(4));
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(input_h), Constraint::Length(1)])
            .split(area);
        // the runner pane takes the lower part of the body while it is open
        let (body, pane) = if self.screen == Screen::Read && self.runner.is_some() && parts[1].height >= 14 {
            let ph = (parts[1].height * 45 / 100).max(8);
            let split = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(4), Constraint::Length(ph)]).split(parts[1]);
            (split[0], Some(split[1]))
        } else {
            (parts[1], None)
        };
        let (w, h) = (body.width as usize, body.height as usize);
        self.body_h = h;
        f.render_widget(Paragraph::new(pal.line(&self.top_line(parts[0].width as usize))), parts[0]);
        let mut lines = self.build(w, h);
        let max_scroll = lines.len().saturating_sub(h);
        if self.item_count() == 0 || matches!(self.screen, Screen::Read) {
            self.scroll = self.scroll.min(max_scroll);
        } else {
            self.scroll = 0;
        }
        let shown: Vec<Line> = lines.drain(..).skip(self.scroll).take(h).map(|l| pal.line(&l)).collect();
        f.render_widget(Paragraph::new(shown), body);
        if let Some(pane) = pane {
            self.draw_runner(f, pane, &pal);
        }
        f.render_widget(Paragraph::new(pal.line(&self.bar_line(parts[3].width as usize))), parts[3]);
        self.draw_inputs(f, parts[2], &pal);
        if let Some((_, _, t)) = &self.flash {
            if t.elapsed() > FLASH {
                self.flash = None;
            }
        }
    }

    /// The runner pane: the picked script on top, its output (the newest lines) below, the state in the title.
    fn draw_runner(&self, f: &mut Frame, area: Rect, pal: &Palette) {
        let Some(r) = &self.runner else { return };
        let inner_h = area.height.saturating_sub(2) as usize;
        let (n, from, lang) = (r.picked + 1, r.block().map(|b| b.from.clone()).unwrap_or_default(), r.block().map(|b| b.lang.clone()).unwrap_or_default());
        let title = format!(" Script {n}/{} ({lang}, from {from}): {} ", r.blocks.len(), r.status());
        let script_h = r.block().map(|b| b.script.lines().count()).unwrap_or(0).min(inner_h / 2).max(1);
        let mut lines: Vec<Line> =
            r.block().map(|b| b.script.lines().take(script_h).map(|l| Line::styled(format!("> {l}"), Style::default().fg(pal.color("mu")))).collect()).unwrap_or_default();
        let out_h = inner_h.saturating_sub(lines.len());
        let skip = r.out.len().saturating_sub(out_h);
        lines.extend(r.out.iter().skip(skip).map(|l| Line::raw(l.chars().take(area.width.saturating_sub(2) as usize).collect::<String>())));
        let border = match r.state {
            runner::State::Running => pal.color("cy"),
            runner::State::Done(0) => pal.color("gr"),
            runner::State::Done(_) | runner::State::Failed(_) => pal.color("pk"),
            runner::State::Ready => pal.color("ye"),
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default().borders(Borders::ALL).border_style(Style::default().fg(border)).title(title).style(Style::default().bg(pal.color("bg")).fg(pal.color("fg"))),
            ),
            area,
        );
    }

    fn boxed(&self, f: &mut Frame, area: Rect, title: &str, text: &str, pal: &Palette, focus: bool) {
        let border = if focus { pal.color("ye") } else { pal.color("rule") };
        let inner = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(title.to_string())
            .style(Style::default().bg(pal.color("bg")).fg(pal.color("fg")));
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(text.to_string()).block(inner), area);
    }

    fn draw_inputs(&self, f: &mut Frame, area: Rect, pal: &Palette) {
        if area.height < 3 {
            return;
        }
        match self.screen {
            Screen::Read => {
                self.editor_box(f, area, " Reply (Ctrl+S sends, Esc leaves the draft) ", &self.reply, pal, true);
            }
            Screen::Compose => {
                let parts = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(3)]).split(area);
                self.boxed(f, parts[0], " Subject ", &self.subject, pal, self.subject_focus);
                if self.subject_focus {
                    f.set_cursor_position((parts[0].x + 1 + self.subject.chars().count().min(parts[0].width.saturating_sub(3) as usize) as u16, parts[0].y + 1));
                }
                self.editor_box(f, parts[1], " Message (Ctrl+S posts) ", &self.reply, pal, !self.subject_focus);
            }
            Screen::Ask => {
                self.boxed(f, area, " Answer ", &self.subject, pal, true);
                f.set_cursor_position((area.x + 1 + self.subject.chars().count().min(area.width.saturating_sub(3) as usize) as u16, area.y + 1));
            }
            _ => {}
        }
    }

    fn editor_box(&self, f: &mut Frame, area: Rect, title: &str, ed: &Editor, pal: &Palette, focus: bool) {
        let inner_h = area.height.saturating_sub(2) as usize;
        let inner_w = area.width.saturating_sub(2) as usize;
        let top = (ed.row + 1).saturating_sub(inner_h);
        let left = (ed.col + 1).saturating_sub(inner_w.max(1));
        let text: Vec<Line> = ed.lines.iter().skip(top).take(inner_h).map(|l| Line::raw(l.chars().skip(left).take(inner_w).collect::<String>())).collect();
        let border = if focus { pal.color("ye") } else { pal.color("rule") };
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border))
                    .title(title.to_string())
                    .style(Style::default().bg(pal.color("bg")).fg(pal.color("fg"))),
            ),
            area,
        );
        if focus {
            let cx = ed.lines[ed.row].chars().skip(left).take(ed.col - left).map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1)).sum::<usize>() as u16;
            f.set_cursor_position((area.x + 1 + cx, area.y + 1 + (ed.row - top) as u16));
        }
    }
}

pub fn run() -> Result<()> {
    let db = Db::open(&paths::db_path())?;
    db.init()?;
    let mut app = App::with_db(db);
    // The window is a client: the core (`agentdesk daemon`) owns the sessions, jobs and the Slack bridge. Start it if it is not running.
    if let Err(e) = crate::daemon::ensure(Duration::from_secs(15)) {
        eprintln!("agentdesk: {e}. The window works on the board, but nothing will run agents or send toasts until the core is up (`agentdesk core start`).");
        std::thread::sleep(Duration::from_secs(2));
    }
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        prev_hook(info);
    }));
    let mut console = enter()?;
    app.start_refresher(paths::db_path());
    let result = (|| -> Result<()> {
        let mut redraw = true;
        while !app.quit {
            // a frame only when something changed; then sleep until a key, or until something needs attention
            if redraw {
                console.draw(&mut app)?;
            }
            if event::poll(app.next_wake())? {
                // take every event that is waiting before drawing: a held key or a paste is one frame, not one each
                loop {
                    handle(&mut app, event::read()?);
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            if let Some(name) = app.attach_request.take() {
                // leave the window, hand the console to the session, and come back when it is detached
                leave();
                if let Err(e) = crate::attach::client(&name) {
                    eprintln!("{}", anyhow_msg(&e));
                    std::thread::sleep(Duration::from_secs(2));
                }
                console = enter()?;
                app.reload();
            }
            redraw = app.tick();
        }
        Ok(())
    })();
    app.set_draft_aside();
    crate::pyhost::shutdown();
    leave();
    result
}

/// The window takes the console: raw mode, the alternate screen, bracketed paste.
fn enter() -> Result<term::Console<std::io::Stdout>> {
    use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
    enable_raw_mode()?;
    let entered = execute!(std::io::stdout(), EnterAlternateScreen, EnableBracketedPaste).and_then(|_| term::Console::new(std::io::stdout()));
    entered.map_err(|e| {
        leave();
        e.into()
    })
}

/// Gives the console back.
fn leave() {
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
}

fn handle(app: &mut App, ev: Event) {
    match ev {
        Event::Key(k) => {
            app.dirty |= k.kind != KeyEventKind::Release;
            app.on_key(k)
        }
        Event::Paste(s) => {
            match app.screen {
                Screen::Read => app.reply.insert_str(&s),
                Screen::Compose if !app.subject_focus => app.reply.insert_str(&s),
                Screen::Compose | Screen::Ask => app.subject.push_str(s.lines().next().unwrap_or("")),
                _ => {}
            }
            app.dirty = true;
        }
        Event::Resize(..) => app.dirty = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_perf;
#[cfg(test)]
mod tests_pty;

/// `agentdesk ui-bench`: see `bench.rs`.
pub fn bench(runs: usize, quick: bool) -> Result<String> {
    bench::run(runs, quick)
}

fn parse_key(tok: &str) -> Vec<KeyEvent> {
    let named = |c: KeyCode| vec![KeyEvent::new(c, KeyModifiers::NONE)];
    match tok.to_lowercase().as_str() {
        "enter" | "ret" => return named(KeyCode::Enter),
        "esc" => return named(KeyCode::Esc),
        "tab" => return named(KeyCode::Tab),
        "backtab" => return named(KeyCode::BackTab),
        "up" => return named(KeyCode::Up),
        "down" => return named(KeyCode::Down),
        "left" => return named(KeyCode::Left),
        "right" => return named(KeyCode::Right),
        "pgup" => return named(KeyCode::PageUp),
        "pgdn" => return named(KeyCode::PageDown),
        "home" => return named(KeyCode::Home),
        "end" => return named(KeyCode::End),
        "backspace" => return named(KeyCode::Backspace),
        "space" => return named(KeyCode::Char(' ')),
        _ => {}
    }
    if let Some(text) = tok.strip_prefix("t:") {
        return text.replace('~', " ").chars().map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)).collect();
    }
    for (prefix, m) in [("ctrl+", KeyModifiers::CONTROL), ("alt+", KeyModifiers::ALT), ("shift+", KeyModifiers::SHIFT)] {
        if let Some(rest) = tok.to_lowercase().strip_prefix(prefix) {
            let mut inner = parse_key(rest);
            for k in inner.iter_mut() {
                k.modifiers |= m;
            }
            return inner;
        }
    }
    tok.chars().next().map(|c| vec![KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)]).unwrap_or_default()
}

/// Renders the window headlessly: opens the board, presses `keys` (space-separated: `q`, `enter`, `esc`, `ctrl+s`,
/// `alt+c`, `t:text~with~spaces`, `wait:300` to let a started script print), and returns the screen as text (or JSON with the mode and the flash line).
pub fn snapshot(size: &str, keys: &str, as_json: bool) -> Result<String> {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let (w, h) = size.split_once('x').and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?))).unwrap_or((110, 36));
    let db = Db::open(&paths::db_path())?;
    db.init()?;
    let mut app = App::with_db(db);
    let mut t = Terminal::new(TestBackend::new(w, h))?;
    for tok in keys.split_whitespace() {
        // `wait:300` lets a script the runner started print for 300 ms, then takes what it printed
        if let Some(ms) = tok.strip_prefix("wait:").and_then(|n| n.parse::<u64>().ok()) {
            std::thread::sleep(Duration::from_millis(ms));
            if let Some(r) = app.runner.as_mut() {
                r.pump();
            }
            continue;
        }
        for k in parse_key(tok) {
            t.draw(|f| app.draw(f))?; // a key acts on the screen it was pressed on, as in a real session
            app.on_key(k);
        }
    }
    t.draw(|f| app.draw(f))?;
    let buf = t.backend().buffer().clone();
    let lines: Vec<String> = (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect();
    if !as_json {
        return Ok(lines.join("\n"));
    }
    Ok(serde_json::to_string_pretty(&json!({
        "screen": format!("{:?}", app.screen).to_lowercase(),
        "lines": lines,
        "flash": app.flash.as_ref().map(|f| f.0.clone()),
        "confirm": app.confirm.as_ref().map(|c| c.0.clone()),
        "quitting": app.quit || app.quit_at.is_some(),
    }))?)
}
