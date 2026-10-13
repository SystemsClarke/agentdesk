//! `agentdesk ui-bench` (hidden): times the window on a board, as JSON. Point `AGENTDESK_DATA` at a COPY of a board (it
//! refuses to run without it: the bench opens threads, saves drafts and settings). See `docs/ui-bench.md`.
//!
//! What it measures, in milliseconds unless it says otherwise:
//! - `reload`: `App::reload`, and every step of `State::load` one by one (median over the runs).
//! - `draw`: `App::draw` into a TestBackend, per screen.
//! - `render`: the Markdown / mermaid / chart renderer on the longest thread.
//! - `key_to_frame`: a key, then the frame it causes, on the screens people live on.
//! - `terminal`: what one frame sends to the console: bytes and write calls, with stdout's own buffering (a 1 KiB
//!   LineWriter) against the window's, and for a full, an unchanged and a one-row-changed frame.

use super::*;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::Terminal;
use std::io::Write;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn summary(mut v: Vec<f64>) -> Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len().max(1);
    json!({"n": v.len(), "min": round(v.first().copied().unwrap_or(0.0)), "median": round(v.get(n / 2).copied().unwrap_or(0.0)), "p95": round(v.get((n * 95 / 100).min(n - 1)).copied().unwrap_or(0.0)), "max": round(v.last().copied().unwrap_or(0.0))})
}

/// Runs `f` `n` times (after one warm-up) and summarises how long each took.
fn time(n: usize, mut f: impl FnMut()) -> Value {
    f();
    summary(
        (0..n)
            .map(|_| {
                let t = Instant::now();
                f();
                ms(t)
            })
            .collect(),
    )
}

fn median_of(v: &Value) -> f64 {
    v["median"].as_f64().unwrap_or(0.0)
}

/// Counts what reaches the console: bytes, and how many separate `write` calls (each one a console write on Windows).
#[derive(Default, Clone)]
pub struct Counting(pub std::rc::Rc<std::cell::Cell<(usize, usize)>>);

impl Counting {
    /// (bytes, write calls) since the last call.
    pub fn take(&self) -> (usize, usize) {
        self.0.replace((0, 0))
    }
}

impl Write for Counting {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let (bytes, writes) = self.0.get();
        self.0.set((bytes + b.len(), writes + 1));
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn draw_to_test(app: &mut App, t: &mut Terminal<TestBackend>) {
    t.draw(|f| app.draw(f)).unwrap();
}

fn go(app: &mut App, screen: Screen, channel: &'static str) {
    app.channel = channel;
    app.screen = screen;
    app.read_tid = None;
    app.detail = None;
    app.reload();
}

fn screens(app: &mut App, w: u16, h: u16, runs: usize, long_thread: Option<i64>) -> Value {
    let mut out = serde_json::Map::new();
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    let mut list: Vec<(String, Screen, &'static str)> = vec![("main".into(), Screen::Main, "question")];
    for ch in state::CHANNELS {
        list.push((format!("list-{ch}"), Screen::List, ch));
    }
    for (n, s) in [
        ("prs", Screen::Prs),
        ("jobs", Screen::Jobs),
        ("rules", Screen::Rules),
        ("sysop", Screen::Sysop),
        ("who", Screen::Who),
        ("options", Screen::Options),
        ("agents", Screen::Agents),
    ] {
        list.push((n.into(), s, "question"));
    }
    for (name, screen, ch) in list {
        go(app, screen, ch);
        out.insert(name, time(runs, || draw_to_test(app, &mut t)));
    }
    if let Some(tid) = long_thread {
        let t0 = Instant::now();
        app.open_thread(tid, Screen::List);
        out.insert(
            "open longest thread (load + first frame, cold)".into(),
            json!(round({
                draw_to_test(app, &mut t);
                ms(t0)
            })),
        );
        out.insert("read-longest-thread".into(), time(runs, || draw_to_test(app, &mut t)));
    }
    Value::Object(out)
}

fn reload_steps(app: &mut App, runs: usize) -> Value {
    let (db_archived, db_decided) = (app.show_archived, app.show_decided);
    let mut by_name: Vec<(&'static str, Vec<f64>)> = Vec::new();
    let mut totals = Vec::new();
    for i in 0..=runs {
        state::probe_start();
        let t = Instant::now();
        let _ = State::load(&app.db, db_archived, db_decided);
        let total = ms(t);
        let steps = state::probe_take();
        if i == 0 {
            continue; // warm-up: statement cache, page cache
        }
        totals.push(total);
        for (n, v) in steps {
            match by_name.iter_mut().find(|(k, _)| *k == n) {
                Some((_, a)) => a.push(v),
                None => by_name.push((n, vec![v])),
            }
        }
    }
    let mut steps: Vec<(&str, f64)> = by_name.into_iter().map(|(n, v)| (n, median_of(&summary(v)))).collect();
    steps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    json!({
        "State::load": summary(totals),
        "steps_median_ms_slowest_first": steps.iter().map(|(n, v)| json!({"step": n, "ms": round(*v)})).collect::<Vec<_>>(),
    })
}

/// The longest thread by body bytes, and the one with the most mermaid/chart fences.
fn pick_threads(app: &App) -> (Option<i64>, Option<i64>) {
    let big = app.db.row("SELECT thread_id AS id FROM messages GROUP BY thread_id ORDER BY SUM(LENGTH(body)) DESC LIMIT 1", &[]).ok().flatten().and_then(|r| r["id"].as_i64());
    let fenced = app
        .db
        .row("SELECT thread_id AS id FROM messages WHERE body LIKE '%```mermaid%' OR body LIKE '%```chart%' GROUP BY thread_id ORDER BY COUNT(*) DESC LIMIT 1", &[])
        .ok()
        .flatten()
        .and_then(|r| r["id"].as_i64());
    (big, fenced)
}

fn render_bench(app: &App, tid: Option<i64>, runs: usize) -> Value {
    let Some(d) = tid.and_then(|t| Detail::load(&app.db, t)) else { return json!({"thread": null}) };
    let bytes: usize = d.messages.iter().map(|m| m.body.len()).sum();
    let fences = d.messages.iter().map(|m| m.body.matches("```mermaid").count() + m.body.matches("```chart").count()).sum::<usize>();
    let longest = d.messages.iter().max_by_key(|m| m.body.len()).map(|m| m.body.clone()).unwrap_or_default();
    json!({
        "thread": d.thread.id,
        "messages": d.messages.len(),
        "body_bytes": bytes,
        "mermaid_or_chart_fences": fences,
        "md_render_whole_thread_w100": time(runs, || {
            for m in &d.messages {
                std::hint::black_box(md::render(&m.body, 100));
            }
        }),
        "md_render_longest_message_w100": time(runs, || {
            std::hint::black_box(md::render(&longest, 100));
        }),
        "longest_message_bytes": longest.len(),
    })
}

fn key_to_frame(app: &mut App, w: u16, h: u16, runs: usize, long_thread: Option<i64>) -> Value {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    let mut out = serde_json::Map::new();
    let k = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    go(app, Screen::List, "discussion");
    draw_to_test(app, &mut t);
    let mut flip = false;
    out.insert(
        "list: down/up key + frame".into(),
        time(runs, || {
            flip = !flip;
            app.on_key(k(if flip { KeyCode::Down } else { KeyCode::Up }));
            draw_to_test(app, &mut t);
        }),
    );
    if let Some(tid) = long_thread {
        app.open_thread(tid, Screen::List);
        draw_to_test(app, &mut t);
        out.insert(
            "reader: type a letter + frame".into(),
            time(runs, || {
                app.on_key(k(KeyCode::Char('x')));
                draw_to_test(app, &mut t);
            }),
        );
        out.insert(
            "reader: scroll a page + frame".into(),
            time(runs, || {
                app.on_key(k(KeyCode::PageDown));
                draw_to_test(app, &mut t);
                app.on_key(k(KeyCode::PageUp));
                draw_to_test(app, &mut t);
            }),
        );
    }
    go(app, Screen::Main, "question");
    for (label, keys) in [("main -> list -> main: 2 screen switches + frames", ['q', 'm']), ("main -> agents -> main: 2 screen switches + frames", ['a', 'm'])] {
        out.insert(
            label.into(),
            time(runs, || {
                for c in keys {
                    app.on_key(k(KeyCode::Char(c)));
                    draw_to_test(app, &mut t);
                }
            }),
        );
    }
    out.insert("reload (exact, after a write)".into(), time(runs, || app.reload()));
    out.insert("reload_nav (switching screen, budget reused)".into(), time(runs, || app.reload_nav(None)));
    out.insert(
        "main: theme key (t) + frame".into(),
        time(runs.min(8), || {
            app.on_key(k(KeyCode::Char('t')));
            draw_to_test(app, &mut t);
        }),
    );
    Value::Object(out)
}

/// One frame through the backend the window uses, into a counting sink, for a few typical changes.
fn terminal_path(app: &mut App, w: u16, h: u16) -> Value {
    let mut rows = Vec::new();
    let k = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    let steps =
        [("first (full screen)", None), ("unchanged", None), ("one row changed (down key)", Some(k(KeyCode::Down))), ("theme change (every cell)", Some(k(KeyCode::Char('t'))))];
    // what `ratatui::init()` gave the window: crossterm over `std::io::stdout()`, which is a LineWriter with a 1 KiB buffer
    let tally = Counting::default();
    let sink: Box<dyn Write> = Box::new(std::io::LineWriter::with_capacity(1024, tally.clone()));
    let mut term =
        Terminal::with_options(CrosstermBackend::new(sink), ratatui::TerminalOptions { viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, w, h)) }).unwrap();
    go(app, Screen::List, "discussion");
    for (name, key) in steps {
        if let Some(key) = key {
            app.on_key(key);
        }
        let t0 = Instant::now();
        term.draw(|f| app.draw(f)).unwrap();
        term.backend_mut().flush().unwrap();
        let (bytes, writes) = tally.take();
        rows.push(json!({"writer": "before: stdout LineWriter(1 KiB)", "frame": name, "ms": round(ms(t0)), "bytes": bytes, "write_calls": writes}));
    }
    // what the window does now: the frame in memory, synchronized-update markers around it, one write
    let tally = Counting::default();
    let mut console = super::term::Console::with_size(tally.clone(), w, h).unwrap();
    go(app, Screen::List, "discussion");
    for (name, key) in steps {
        if let Some(key) = key {
            app.on_key(key);
        }
        let t0 = Instant::now();
        console.draw(app).unwrap();
        let (bytes, writes) = tally.take();
        rows.push(json!({"writer": "after: one write per frame", "frame": name, "ms": round(ms(t0)), "bytes": bytes, "write_calls": writes}));
    }
    json!(rows)
}

/// The loop's idle cost and the stall a board change causes on the window's thread: a worker is started, a message is posted to the
/// board from another connection every 100 ms for three seconds, and every `tick` (what the loop runs between events) is timed.
fn live_loop(app: &mut App, unit_ms: u64) -> Value {
    go(app, Screen::List, "discussion");
    let version = app.db.data_version();
    let db_version = time(200, || {
        std::hint::black_box(app.db.data_version());
    });
    app.dirty = false;
    app.start_refresher(paths::db_path());
    // let the worker's first answer land, then check that nothing is drawn while nothing changes
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_millis(unit_ms) {
        app.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
    app.tick();
    let mut idle_redraws = 0;
    let idle = Instant::now();
    let mut idle_ticks = Vec::new();
    while idle.elapsed() < Duration::from_millis(unit_ms) {
        let t0 = Instant::now();
        let redraw = app.tick();
        idle_ticks.push(ms(t0));
        idle_redraws += redraw as usize;
        std::thread::sleep(Duration::from_millis(20));
    }
    let writer = Db::open(&paths::db_path()).unwrap();
    let mut ticks = Vec::new();
    let mut redraws = 0;
    let mut posts = 0;
    let t0 = Instant::now();
    let mut next_post = Instant::now();
    while t0.elapsed() < Duration::from_millis(unit_ms * 2) {
        if Instant::now() >= next_post {
            let _ = writer.start_thread("discussion", &format!("bench post {posts}"), "bench-agent", "agent", "a line", None, None);
            posts += 1;
            next_post += Duration::from_millis(100);
        }
        let t = Instant::now();
        let redraw = app.tick();
        ticks.push(ms(t));
        redraws += redraw as usize;
        std::thread::sleep(Duration::from_millis(5));
    }
    json!({
        "PRAGMA data_version (window thread)": db_version,
        "data_version_at_start": version,
        "idle: tick (what the loop does between events)": summary(idle_ticks),
        "idle: redraws requested": idle_redraws,
        "while a post lands every 100 ms: posts": posts,
        "while a post lands every 100 ms: redraws requested": redraws,
        "while a post lands every 100 ms: tick on the window thread": summary(ticks),
    })
}

pub fn run(runs: usize, quick: bool) -> Result<String> {
    if std::env::var_os("AGENTDESK_DATA").filter(|d| !d.is_empty()).is_none() {
        anyhow::bail!("ui-bench opens threads and saves drafts and settings: point AGENTDESK_DATA at a copy of a board first (never the live one)");
    }
    // A machine with the Slack bridge up: its heartbeat names a live pid (this one), which the window must check on every frame.
    let beat = json!({"ts": chrono::Utc::now().to_rfc3339(), "pid": std::process::id(), "poll_s": 5});
    std::fs::write(paths::data_dir().join("slack_bridge.state"), beat.to_string())?;
    let bridge_running = time(if quick { 1 } else { 8 }, || {
        std::hint::black_box(crate::bridge::running());
    });
    let db = Db::open(&paths::db_path())?;
    db.init()?;
    let count = |t: &str| db.scalar_i64(&format!("SELECT COUNT(*) FROM {t}"), &[]).ok().flatten().unwrap_or(0);
    let board =
        json!({"threads": count("threads"), "messages": count("messages"), "identities": count("identities"), "goals": count("goals"), "usage_samples": count("usage_samples")});
    let mut app = App::with_db(db);
    let (w, h) = (120u16, 40u16);
    let (big, fenced) = pick_threads(&app);
    let t0 = Instant::now();
    app.reload();
    let first_reload = ms(t0);
    let reload = json!({"App::reload": time(runs, || app.reload()), "first_reload_ms": round(first_reload), "State::load": reload_steps(&mut app, runs)});
    let draw = screens(&mut app, w, h, runs, big);
    let render = json!({"longest thread": render_bench(&app, big, runs), "thread with mermaid/chart": render_bench(&app, fenced, runs)});
    let keys = key_to_frame(&mut app, w, h, runs, big);
    let terminal = terminal_path(&mut app, w, h);
    let beat_text = beat.to_string();
    let slack_up = time(50, || {
        std::hint::black_box(super::live::slack_up(&beat_text));
    });
    let live = live_loop(&mut app, if quick { 300 } else { 1500 });
    let doc = json!({
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "os": std::env::consts::OS,
        "board": board,
        "bridge::running() with a live heartbeat (what every frame used to call)": bridge_running,
        "tui::live::slack_up with a live heartbeat (what a refresh calls now)": slack_up,
        "event_loop": live,
        "size": format!("{w}x{h}"),
        "reload_ms": reload,
        "draw_ms": draw,
        "render_ms": render,
        "key_to_frame_ms": keys,
        "terminal_frame": terminal,
    });

    Ok(serde_json::to_string_pretty(&doc)?)
}
