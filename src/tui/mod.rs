//! The terminal window: a BBS-style board for John. Tabs for the channels, the merge list and the recurring jobs;
//! a thread reader with a reply box; search; permission rules. State lives here, drawing in `draw.rs`.

mod draw;
mod editor;
mod md;

use crate::db::{anyhow_msg, Db};
use crate::{jobs, paths, permissions, prs, service, usage};
use anyhow::Result;
use editor::Editor;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Questions,
    Discussion,
    Wiki,
    Work,
    Merge,
    Jobs,
}
pub const TABS: [Tab; 6] = [Tab::Questions, Tab::Discussion, Tab::Wiki, Tab::Work, Tab::Merge, Tab::Jobs];

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Questions => "Questions",
            Tab::Discussion => "Discussion",
            Tab::Wiki => "Wiki",
            Tab::Work => "Work to Hire",
            Tab::Merge => "Merge list",
            Tab::Jobs => "Recurring jobs",
        }
    }
    fn channel(self) -> Option<&'static str> {
        match self {
            Tab::Questions => Some("question"),
            Tab::Discussion => Some("discussion"),
            Tab::Wiki => Some("wiki"),
            Tab::Work => Some("work"),
            _ => None,
        }
    }
}

pub enum Step {
    PostSubject(&'static str),
    JobName,
    JobFolder(String),
    JobSchedule(String, String),
    JobModel(String, String, String),
}

pub enum Compose {
    Reply(i64),
    Post(&'static str, String),
    JobPrompt { name: String, folder: String, schedule: String, model: String },
}

pub enum Confirm {
    CloseQuestion(i64),
    DeleteJob(String),
}

pub enum Mode {
    List,
    Reader { id: i64, scroll: usize, doc: Value },
    Compose { what: Compose, editor: Editor, back: Box<Mode> },
    Prompt { label: String, input: String, step: Step },
    Confirm { text: String, action: Confirm },
    Search { query: String, results: Vec<Value>, selected: usize },
    Permissions { rows: Vec<Value>, selected: usize, settings: String },
    Help,
}

pub struct App {
    pub db: Db,
    pub tab: Tab,
    pub mode: Mode,
    pub threads: Vec<Value>,
    pub prs: Vec<Value>,
    pub jobs: Vec<Value>,
    pub selected: usize,
    pub waiting: i64,
    pub waiting_by_tab: [i64; 6],
    pub show_archived: bool,
    pub usage: String,
    pub update: Option<String>,
    pub flash: Option<(String, Instant)>,
    pub quit: bool,
    data_version: i64,
    last_load: Instant,
}

impl App {
    fn new() -> Result<App> {
        let db = Db::open(&paths::db_path())?;
        db.init()?;
        Ok(App::with_db(db))
    }

    fn with_db(db: Db) -> App {
        let mut app = App {
            db,
            tab: Tab::Questions,
            mode: Mode::List,
            threads: vec![],
            prs: vec![],
            jobs: vec![],
            selected: 0,
            waiting: 0,
            waiting_by_tab: [0; 6],
            show_archived: false,
            usage: String::new(),
            update: None,
            flash: None,
            quit: false,
            data_version: -1,
            last_load: Instant::now() - Duration::from_secs(60),
        };
        app.reload();
        app
    }

    pub fn say(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    fn fail(&mut self, e: anyhow::Error) {
        self.say(format!("! {}", anyhow_msg(&e)));
    }

    pub fn reload(&mut self) {
        self.data_version = self.db.data_version();
        self.last_load = Instant::now();
        self.waiting = self.db.threads_waiting();
        self.update = crate::update::pending();
        self.usage = usage::report()["summary"].as_str().unwrap_or("").to_string();
        match self.tab.channel() {
            Some(ch) => {
                let archived = self.show_archived || self.tab != Tab::Questions;
                self.threads = self.db.list_threads(Some(ch), None, 300, archived, false).unwrap_or_default();
            }
            None => self.threads.clear(),
        }
        self.prs = self.db.rows("SELECT * FROM pull_requests ORDER BY (state='open') DESC, id DESC LIMIT 200", &[]).unwrap_or_default();
        self.jobs = jobs::list(&self.db).unwrap_or_default();
        self.waiting_by_tab[0] = self.waiting;
        self.waiting_by_tab[3] = self.db.scalar_i64("SELECT COUNT(*) FROM threads WHERE channel='work' AND status='open'", &[]).ok().flatten().unwrap_or(0);
        self.waiting_by_tab[4] = self.prs.iter().filter(|p| p["state"].as_str() == Some("open")).count() as i64;
        let n = self.rows();
        if self.selected >= n {
            self.selected = n.saturating_sub(1);
        }
        if let Mode::Reader { id, doc, .. } = &mut self.mode {
            if let Ok(d) = self.db.get_thread(*id) {
                *doc = d;
            }
        }
    }

    pub fn rows(&self) -> usize {
        match self.tab {
            Tab::Merge => self.prs.len(),
            Tab::Jobs => self.jobs.len(),
            _ => self.threads.len(),
        }
    }

    fn switch(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
        self.reload();
    }

    fn open_thread(&mut self, id: i64) {
        match self.db.get_thread(id) {
            Ok(doc) => self.mode = Mode::Reader { id, scroll: 0, doc },
            Err(e) => self.fail(e),
        }
    }

    fn selected_thread_id(&self) -> Option<i64> {
        self.threads.get(self.selected).and_then(|t| t["id"].as_i64())
    }

    fn start_post(&mut self) {
        let Some(ch) = self.tab.channel() else { return };
        let ch: &'static str = match ch {
            "question" => "question",
            "discussion" => "discussion",
            "wiki" => "wiki",
            _ => "work",
        };
        self.mode = Mode::Prompt { label: format!("New {ch} post: subject"), input: String::new(), step: Step::PostSubject(ch) };
    }

    // ---- keys

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        let mode = std::mem::replace(&mut self.mode, Mode::List);
        self.mode = match mode {
            Mode::List => {
                self.list_key(k);
                std::mem::replace(&mut self.mode, Mode::List)
            }
            Mode::Reader { id, scroll, doc } => self.reader_key(k, id, scroll, doc),
            Mode::Compose { what, editor, back } => self.compose_key(k, what, editor, back),
            Mode::Prompt { label, input, step } => self.prompt_key(k, label, input, step),
            Mode::Confirm { text, action } => self.confirm_key(k, text, action),
            Mode::Search { query, results, selected } => self.search_key(k, query, results, selected),
            Mode::Permissions { rows, selected, settings } => self.permissions_key(k, rows, selected, settings),
            Mode::Help => Mode::List,
        };
    }

    fn list_key(&mut self, k: KeyEvent) {
        let n = self.rows();
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab | KeyCode::Right => self.switch(TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + 1) % TABS.len()]),
            KeyCode::BackTab | KeyCode::Left => self.switch(TABS[(TABS.iter().position(|t| *t == self.tab).unwrap() + TABS.len() - 1) % TABS.len()]),
            KeyCode::Char(c @ '1'..='6') => self.switch(TABS[c as usize - '1' as usize]),
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < n => self.selected += 1,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = n.saturating_sub(1),
            KeyCode::PageDown => self.selected = (self.selected + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::Char('R') => {
                self.reload();
                self.say("refreshed");
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/') => self.mode = Mode::Search { query: String::new(), results: vec![], selected: 0 },
            KeyCode::Char('X') => self.open_permissions(),
            KeyCode::Char('U') => match crate::update::check().and_then(|(latest, newer)| if newer { crate::update::apply() } else { Ok(format!("{latest} (already current)")) }) {
                Ok(v) => self.say(format!("now on {v}: restart the window to use it")),
                Err(e) => self.fail(e),
            },
            _ => match self.tab {
                Tab::Merge => self.merge_key(k),
                Tab::Jobs => self.jobs_key(k),
                _ => self.thread_list_key(k),
            },
        }
    }

    fn thread_list_key(&mut self, k: KeyEvent) {
        let Some(id) = self.selected_thread_id() else {
            if k.code == KeyCode::Char('n') {
                self.start_post();
            }
            return;
        };
        match k.code {
            KeyCode::Enter => self.open_thread(id),
            KeyCode::Char('r') => self.mode = Mode::Compose { what: Compose::Reply(id), editor: Editor::new(), back: Box::new(Mode::List) },
            KeyCode::Char('n') => self.start_post(),
            KeyCode::Char('c') if self.tab == Tab::Questions => {
                self.mode = Mode::Confirm { text: format!("Close question #{id} without answering? (y/n)"), action: Confirm::CloseQuestion(id) }
            }
            KeyCode::Char('u') if self.tab == Tab::Questions => match self.db.unarchive(id) {
                Ok(true) => {
                    self.say(format!("#{id} brought back"));
                    self.reload();
                }
                Ok(false) => self.say("not archived"),
                Err(e) => self.fail(e),
            },
            KeyCode::Char('a') if self.tab == Tab::Questions => {
                self.show_archived = !self.show_archived;
                self.say(if self.show_archived { "showing archived questions" } else { "hiding archived questions" });
                self.reload();
            }
            _ => {}
        }
    }

    fn merge_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(url) = self.prs.get(self.selected).and_then(|p| p["url"].as_str()) {
                    open_url(url);
                    self.say(format!("opened {url}"));
                }
            }
            KeyCode::Char('t') => {
                if let Some(tid) = self.prs.get(self.selected).and_then(|p| p["thread_id"].as_i64()) {
                    self.open_thread(tid);
                }
            }
            KeyCode::Char('c') => {
                let n = prs::check(&self.db);
                self.say(format!("checked the merge list: {n} settled"));
                self.reload();
            }
            _ => {}
        }
    }

    fn jobs_key(&mut self, k: KeyEvent) {
        let name = self.jobs.get(self.selected).and_then(|j| j["name"].as_str()).map(String::from);
        match (k.code, name) {
            (KeyCode::Char('n'), _) => self.mode = Mode::Prompt { label: "New job: name".into(), input: String::new(), step: Step::JobName },
            (KeyCode::Char('r'), Some(n)) => match jobs::run_now(&self.db, &n) {
                Ok(j) => {
                    self.say(format!("{n}: {}", j["last_status"].as_str().unwrap_or("")));
                    self.reload();
                }
                Err(e) => self.fail(e),
            },
            (KeyCode::Char('e'), Some(n)) => {
                let on = self.jobs[self.selected]["enabled"].as_i64() != Some(1);
                match jobs::enable(&self.db, &n, on) {
                    Ok(_) => {
                        self.say(format!("{n} {}", if on { "enabled" } else { "disabled" }));
                        self.reload();
                    }
                    Err(e) => self.fail(e),
                }
            }
            (KeyCode::Char('x'), Some(n)) => self.mode = Mode::Confirm { text: format!("Delete job {n}? (y/n)"), action: Confirm::DeleteJob(n) },
            _ => {}
        }
    }

    fn reader_key(&mut self, k: KeyEvent, id: i64, scroll: usize, doc: Value) -> Mode {
        let mut scroll = scroll;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => return Mode::List,
            KeyCode::Down | KeyCode::Char('j') => scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll += 15,
            KeyCode::PageUp => scroll = scroll.saturating_sub(15),
            KeyCode::Home | KeyCode::Char('g') => scroll = 0,
            KeyCode::End | KeyCode::Char('G') => scroll = usize::MAX / 2,
            KeyCode::Char('r') => {
                return Mode::Compose { what: Compose::Reply(id), editor: Editor::new(), back: Box::new(Mode::Reader { id, scroll, doc }) };
            }
            KeyCode::Char('c') if doc["thread"]["channel"].as_str() == Some("question") => {
                self.mode = Mode::Confirm { text: format!("Close question #{id} without answering? (y/n)"), action: Confirm::CloseQuestion(id) };
                return std::mem::replace(&mut self.mode, Mode::List);
            }
            KeyCode::Char('u') => match self.db.unarchive(id) {
                Ok(true) => {
                    self.say("brought back");
                    self.reload();
                }
                Ok(false) => self.say("not archived"),
                Err(e) => self.fail(e),
            },
            _ => {}
        }
        Mode::Reader { id, scroll, doc }
    }

    fn compose_key(&mut self, k: KeyEvent, what: Compose, mut editor: Editor, back: Box<Mode>) -> Mode {
        match k.code {
            KeyCode::Esc => return *back,
            KeyCode::Char('s') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                if editor.is_blank() {
                    self.say("nothing to send");
                    return Mode::Compose { what, editor, back };
                }
                let text = editor.text();
                let r: Result<String> = match &what {
                    Compose::Reply(id) => self.db.john_replies(*id, &text).map(|m| format!("replied on #{id} (message {m})")),
                    Compose::Post(ch, subject) => {
                        let subj = if subject.trim().is_empty() { "(no subject)" } else { subject };
                        self.db.start_thread(ch, subj, crate::db::JOHN, crate::db::HUMAN, &text, None, None).map(|t| format!("started thread #{t}"))
                    }
                    Compose::JobPrompt { name, folder, schedule, model } => {
                        let (at, days, every) = parse_schedule(schedule);
                        jobs::create(
                            &self.db,
                            jobs::NewJob {
                                name,
                                folder,
                                prompt: &text,
                                at: at.as_deref(),
                                days: days.as_deref(),
                                every_minutes: every,
                                model: Some(model),
                                catch_up_minutes: None,
                            },
                        )
                        .map(|_| format!("job {name} created"))
                    }
                };
                return match r {
                    Ok(msg) => {
                        self.say(msg);
                        self.reload();
                        match *back {
                            Mode::Reader { id, .. } => match self.db.get_thread(id) {
                                Ok(doc) => Mode::Reader { id, scroll: usize::MAX / 2, doc },
                                Err(_) => Mode::List,
                            },
                            other => other,
                        }
                    }
                    Err(e) => {
                        self.fail(e);
                        Mode::Compose { what, editor, back }
                    }
                };
            }
            _ => editor.key(k),
        }
        Mode::Compose { what, editor, back }
    }

    fn prompt_key(&mut self, k: KeyEvent, label: String, mut input: String, step: Step) -> Mode {
        match k.code {
            KeyCode::Esc => return Mode::List,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => input.push(c),
            KeyCode::Enter => return self.prompt_done(label, input, step),
            _ => {}
        }
        Mode::Prompt { label, input, step }
    }

    fn prompt_done(&mut self, label: String, input: String, step: Step) -> Mode {
        let again = |label: String, input: String, step: Step| Mode::Prompt { label, input, step };
        match step {
            Step::PostSubject(ch) => Mode::Compose { what: Compose::Post(ch, input), editor: Editor::new(), back: Box::new(Mode::List) },
            Step::JobName => {
                if input.trim().is_empty() {
                    return again(label, input, Step::JobName);
                }
                let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                Mode::Prompt { label: "New job: folder".into(), input: cwd, step: Step::JobFolder(input.trim().to_string()) }
            }
            Step::JobFolder(name) => Mode::Prompt {
                label: "New job: schedule (08:00 [weekdays|mon,thu] or every 120)".into(),
                input: String::new(),
                step: Step::JobSchedule(name, input.trim().to_string()),
            },
            Step::JobSchedule(name, folder) => {
                Mode::Prompt { label: "New job: model (haiku|sonnet|opus)".into(), input: "sonnet".into(), step: Step::JobModel(name, folder, input.trim().to_string()) }
            }
            Step::JobModel(name, folder, schedule) => {
                Mode::Compose { what: Compose::JobPrompt { name, folder, schedule, model: input.trim().to_string() }, editor: Editor::new(), back: Box::new(Mode::List) }
            }
        }
    }

    fn confirm_key(&mut self, k: KeyEvent, text: String, action: Confirm) -> Mode {
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let r = match &action {
                    Confirm::CloseQuestion(id) => self.db.close_question(*id).map(|c| if c { format!("#{id} closed") } else { "not a live question".to_string() }),
                    Confirm::DeleteJob(n) => jobs::delete(&self.db, n).map(|_| format!("job {n} deleted")),
                };
                match r {
                    Ok(m) => self.say(m),
                    Err(e) => self.fail(e),
                }
                self.reload();
                Mode::List
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Mode::List,
            _ => Mode::Confirm { text, action },
        }
    }

    fn search_key(&mut self, k: KeyEvent, mut query: String, mut results: Vec<Value>, mut selected: usize) -> Mode {
        match k.code {
            KeyCode::Esc => return Mode::List,
            KeyCode::Enter => {
                if let Some(id) = results.get(selected).and_then(|r| r["thread_id"].as_i64()) {
                    self.open_thread(id);
                    return std::mem::replace(&mut self.mode, Mode::List);
                }
            }
            KeyCode::Down if selected + 1 < results.len() => selected += 1,
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Backspace => {
                query.pop();
                results = self.db.search(&query, 50).unwrap_or_default();
                selected = 0;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                query.push(c);
                results = self.db.search(&query, 50).unwrap_or_default();
                selected = 0;
            }
            _ => {}
        }
        Mode::Search { query, results, selected }
    }

    fn open_permissions(&mut self) {
        match permissions::list(&self.db) {
            Ok(v) => {
                self.mode =
                    Mode::Permissions { rows: v["proposals"].as_array().cloned().unwrap_or_default(), selected: 0, settings: v["settings"].as_str().unwrap_or("").to_string() }
            }
            Err(e) => self.fail(e),
        }
    }

    fn permissions_key(&mut self, k: KeyEvent, rows: Vec<Value>, mut selected: usize, settings: String) -> Mode {
        let id = rows.get(selected).and_then(|r| r["id"].as_i64());
        let result = match (k.code, id) {
            (KeyCode::Esc | KeyCode::Char('q'), _) => return Mode::List,
            (KeyCode::Down | KeyCode::Char('j'), _) if selected + 1 < rows.len() => {
                selected += 1;
                None
            }
            (KeyCode::Up | KeyCode::Char('k'), _) => {
                selected = selected.saturating_sub(1);
                None
            }
            (KeyCode::Char('a'), Some(id)) => Some(permissions::decide(&self.db, id, true)),
            (KeyCode::Char('x'), Some(id)) => Some(permissions::decide(&self.db, id, false)),
            (KeyCode::Char('v'), Some(id)) => Some(permissions::revert(&self.db, id)),
            _ => None,
        };
        if let Some(r) = result {
            match r {
                Ok(p) => self.say(format!("rule {}: {}", p["rule"].as_str().unwrap_or(""), p["status"].as_str().unwrap_or(""))),
                Err(e) => self.fail(e),
            }
            self.reload();
            self.open_permissions();
            if let Mode::Permissions { selected: s, .. } = &mut self.mode {
                *s = selected;
            }
            return std::mem::replace(&mut self.mode, Mode::List);
        }
        Mode::Permissions { rows, selected, settings }
    }
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

pub fn open_url(url: &str) {
    let (prog, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/c", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

pub fn run() -> Result<()> {
    let mut app = App::new()?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let stop = stop.clone();
        std::thread::spawn(move || service::run(stop))
    };
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|f| draw::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(k) => app.on_key(k),
                    Event::Paste(s) => {
                        if let Mode::Compose { editor, .. } = &mut app.mode {
                            editor.insert_str(&s);
                        } else if let Mode::Prompt { input, .. } = &mut app.mode {
                            input.push_str(s.lines().next().unwrap_or(""));
                        }
                    }
                    _ => {}
                }
            }
            if matches!(app.mode, Mode::List | Mode::Reader { .. }) && (app.db.data_version() != app.data_version || app.last_load.elapsed() > Duration::from_secs(5)) {
                app.reload();
            }
            if let Some((_, t)) = &app.flash {
                if t.elapsed() > Duration::from_secs(5) {
                    app.flash = None;
                }
            }
        }
        Ok(())
    })();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn the_window_draws_and_replies() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.init().unwrap();
        let q = db
            .start_thread("question", "Deploy to prod?", "builder", "agent", "Ship **now**, or wait?\n\n| env | state |\n|---|---|\n| staging | ok |\n\n- option A", None, None)
            .unwrap();
        db.start_thread("discussion", "Refactor notes", "researcher", "agent", "Moved the parser.", None, None).unwrap();
        let mut app = App::with_db(db);
        let s = screen(&mut app, 100, 14);
        println!("{s}");
        assert!(s.contains("1 waiting on you") && s.contains("Deploy to prod?"));
        app.on_key(key(KeyCode::Enter));
        let s = screen(&mut app, 100, 20);
        println!("{s}");
        assert!(s.contains("staging") && s.contains("option A"));
        app.on_key(key(KeyCode::Char('r')));
        for c in "ship it".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let s = screen(&mut app, 100, 24);
        println!("{s}");
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(app.db.get_thread(q).unwrap()["thread"]["status"], "answered");
        assert!(matches!(app.mode, Mode::Reader { .. }));
        app.on_key(key(KeyCode::Esc));
        let s = screen(&mut app, 100, 10);
        println!("{s}");
        assert!(s.contains("all quiet"));
        app.on_key(key(KeyCode::Char('?')));
        assert!(screen(&mut app, 100, 24).contains("Help"));
    }
}
