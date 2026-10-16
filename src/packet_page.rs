//! A packet as a web page: server-rendered HTML for `GET /packet/<id>` on the ops console (`web.rs`), live from the board. John ticks steps,
//! chooses, adds notes, runs the commands the packet asks for, and finishes it here; each action is `kinds::step` / `note` / `finish` or a
//! run of the same script runner the window uses, so the agent that asked is woken exactly as before. The page polls
//! `/packet/<id>/version` and swaps itself when the agent revises the packet. Its colours are the window's: the theme saved in settings.
//!
//! Everything from the board is data: it is escaped, links are kept only when they are http(s), and the page runs no script from the board.
//! A command is never sent by the browser: it names a block (by a hash of its text) and the server finds that block in the packet.

use crate::db::Db;
use crate::kinds;
use crate::tui::runner::{self, Runner, State};
use crate::tui::seg::{palettes, Palette};
use anyhow::Result;
use ratatui::style::Color;
use serde_json::{json, Value};
use std::sync::Mutex;

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn safe_url(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    (l.starts_with("https://") || l.starts_with("http://")) && !u.contains(char::is_whitespace)
}

/// `**bold**`, `` `code` `` and `[text](http-url)`; everything else is text.
pub fn inline(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut o = String::new();
    let mut i = 0;
    let find = |from: usize, pat: &[char]| -> Option<usize> { (from..c.len().saturating_sub(pat.len() - 1)).find(|&j| c[j..j + pat.len()] == *pat) };
    while i < c.len() {
        if c[i] == '*' && c.get(i + 1) == Some(&'*') {
            if let Some(j) = find(i + 2, &['*', '*']).filter(|j| *j > i + 2) {
                o.push_str(&format!("<strong>{}</strong>", inline(&c[i + 2..j].iter().collect::<String>())));
                i = j + 2;
                continue;
            }
        } else if c[i] == '`' {
            if let Some(j) = find(i + 1, &['`']).filter(|j| *j > i + 1) {
                o.push_str(&format!("<code>{}</code>", esc(&c[i + 1..j].iter().collect::<String>())));
                i = j + 1;
                continue;
            }
        } else if c[i] == '[' {
            if let Some(m) = find(i + 1, &[']', '(']) {
                if let Some(e) = find(m + 2, &[')']) {
                    let (label, url) = (c[i + 1..m].iter().collect::<String>(), c[m + 2..e].iter().collect::<String>());
                    if safe_url(&url) {
                        o.push_str(&format!("<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>", esc(&url), esc(&label)));
                        i = e + 1;
                        continue;
                    }
                }
            }
        }
        o.push_str(&esc(&c[i].to_string()));
        i += 1;
    }
    o
}

/// FNV-1a of a script's text: what the page and the server both call a block, stable across restarts.
fn block_hash(script: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in script.trim().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Paragraphs, `-` lists and fenced code, from the Markdown an agent writes. Code gets a Copy button; with `packet` set, a block in a shell
/// the runner knows also gets a Run button and an output panel (what `run_panel` draws).
pub fn blocks(md: &str, packet: Option<i64>) -> String {
    let mut o = String::new();
    let (mut para, mut list): (Vec<String>, Vec<String>) = (vec![], vec![]);
    let (mut fence, mut code, mut indent): (Option<String>, Vec<String>, usize) = (None, vec![], 0);
    let flush = |o: &mut String, para: &mut Vec<String>, list: &mut Vec<String>| {
        if !para.is_empty() {
            o.push_str(&format!("<p>{}</p>", para.join("<br>")));
            para.clear();
        }
        if !list.is_empty() {
            o.push_str(&format!("<ul>{}</ul>", list.iter().map(|l| format!("<li>{l}</li>")).collect::<String>()));
            list.clear();
        }
    };
    let code_block = |lang: &str, text: &str, closed: bool| -> String {
        if closed && lang.eq_ignore_ascii_case("email") {
            return email_card(text);
        }
        let runnable = packet.is_some() && runner::shell_for(lang).is_some() && !text.trim().is_empty();
        let mut h = format!("<div class=\"code\"><div class=\"codebar\"><span>{}</span>", esc(lang));
        if closed {
            h.push_str("<button type=\"button\" data-copy>copy</button>");
        }
        h.push_str(&format!("</div><pre><code>{}</code></pre>", esc(text)));
        if let (true, Some(id)) = (runnable && closed, packet) {
            h.push_str(&run_panel(id, &block_hash(text)));
        }
        h.push_str("</div>");
        h
    };
    for line in md.lines() {
        let t = line.trim_start();
        if let Some(lang) = &fence {
            if t.starts_with("```") {
                o.push_str(&code_block(lang, &code.join("\n"), true));
                fence = None;
                code.clear();
            } else {
                let lead = line.len() - t.len();
                code.push(line[lead.min(indent)..].to_string());
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("```") {
            flush(&mut o, &mut para, &mut list);
            fence = Some(rest.trim().to_string());
            indent = line.len() - t.len();
            continue;
        }
        if t.is_empty() {
            flush(&mut o, &mut para, &mut list);
        } else if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            if !para.is_empty() {
                flush(&mut o, &mut para, &mut list);
            }
            list.push(inline(item));
        } else {
            if !list.is_empty() {
                flush(&mut o, &mut para, &mut list);
            }
            para.push(inline(t.trim_start_matches("> ")));
        }
    }
    if let Some(lang) = fence {
        o.push_str(&code_block(&lang, &code.join("\n"), false));
    }
    flush(&mut o, &mut para, &mut list);
    o
}

// ---- email drafts

/// Outlook on the web's compose address: opens a new message with the fields filled in, in the browser's own Outlook session.
const OUTLOOK_COMPOSE: &str = "https://outlook.office.com/mail/deeplink/compose";
/// A longer address than this is not reliably accepted, so a long body is copied instead of put in the address.
const MAX_COMPOSE_URL: usize = 5500;

fn pct(s: &str) -> String {
    let mut o = String::new();
    for b in s.replace("\r\n", "\n").replace('\n', "\r\n").bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// The addresses in a `To:` or `Cc:` line: `a@b.com`, `Name <a@b.com>`, separated by `,` or `;`. Anything that is not an address is dropped.
fn addresses(line: &str) -> Vec<String> {
    line.split([',', ';'])
        .filter_map(|p| {
            let p = p.trim();
            let a = match (p.rfind('<'), p.rfind('>')) {
                (Some(i), Some(j)) if i < j => p[i + 1..j].trim(),
                _ => p,
            };
            let ok = a.contains('@')
                && !a.contains(char::is_whitespace)
                && !a.starts_with('@')
                && !a.ends_with('@')
                && a.matches('@').count() == 1
                && !a.contains(['<', '>', '"', ',', ';']);
            ok.then(|| a.to_string())
        })
        .collect()
}

/// A fenced ```email block: `To:`, `Cc:` and `Subject:` lines, a blank line, then the message. Drawn as a draft with a button that opens it
/// in Outlook on the web (nothing is sent from here: John reads it there and sends it himself).
fn email_card(text: &str) -> String {
    let (mut to, mut cc, mut subject) = (vec![], vec![], String::new());
    let mut lines = text.lines().peekable();
    while let Some(l) = lines.peek() {
        let Some((k, v)) = l.split_once(':') else { break };
        match k.trim().to_ascii_lowercase().as_str() {
            "to" => to.extend(addresses(v)),
            "cc" => cc.extend(addresses(v)),
            "subject" => subject = v.trim().to_string(),
            _ => break,
        }
        lines.next();
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    let mut url = format!("{OUTLOOK_COMPOSE}?");
    let mut parts: Vec<String> = vec![];
    if !to.is_empty() {
        parts.push(format!("to={}", pct(&to.join(";"))));
    }
    if !cc.is_empty() {
        parts.push(format!("cc={}", pct(&cc.join(";"))));
    }
    if !subject.is_empty() {
        parts.push(format!("subject={}", pct(&subject)));
    }
    let with_body = format!("{}&body={}", parts.join("&"), pct(&body));
    let paste = url.len() + with_body.len() > MAX_COMPOSE_URL;
    url.push_str(&if paste || body.is_empty() { parts.join("&") } else { with_body.trim_start_matches('&').to_string() });
    let field = |k: &str, v: &str| if v.is_empty() { String::new() } else { format!("<dt>{k}</dt><dd>{}</dd>", esc(v)) };
    format!(
        "<div class=\"code mail\"><div class=\"codebar\"><span>draft email · not sent</span><span class=\"acts\"><a class=\"open\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\"{}>open in Outlook</a><button type=\"button\" data-copy>copy</button></span></div>\
<dl class=\"hdr\">{}{}{}</dl><pre class=\"mailbody\"><code>{}</code></pre></div>",
        esc(&url),
        if paste { " data-paste" } else { "" },
        field("To", &to.join(", ")),
        field("Cc", &cc.join(", ")),
        field("Subject", &subject),
        esc(&body)
    )
}

// ---- running a command from the page

/// The runs started from pages, keyed by (packet, block hash). A run outlives a reload of the page; it is replaced by the next run of the
/// same block. At most `MAX_RUNNING` run at once.
static RUNS: Mutex<Vec<((i64, String), Runner)>> = Mutex::new(Vec::new());
const MAX_RUNNING: usize = 4;

/// Every text of a packet that is drawn as Markdown with runnable blocks: where a command can be.
fn runnable_texts(meta: &Value) -> Vec<String> {
    let spec = &meta["packet"];
    let mut v: Vec<String> = vec![meta["intro"].as_str().unwrap_or("").into(), spec["why"].as_str().unwrap_or("").into(), spec["next"].as_str().unwrap_or("").into()];
    v.extend(spec["steps"].as_array().into_iter().flatten().filter_map(|s| s["detail"].as_str().map(String::from)));
    v
}

fn find_block(meta: &Value, hash: &str) -> Option<runner::Block> {
    runnable_texts(meta).into_iter().flat_map(|t| runner::extract_blocks(&[("packet".to_string(), t)])).find(|b| block_hash(&b.script) == hash)
}

fn packet_meta(db: &Db, id: i64) -> Result<(Value, bool)> {
    let doc = db.get_thread(id)?;
    if !is_packet(&doc) {
        anyhow::bail!("#{id} is not a packet");
    }
    Ok((meta_of(&doc), doc["thread"]["status"] == "open"))
}

fn state_json(r: &Runner) -> Value {
    let (state, code, error) = match &r.state {
        State::Ready => ("ready", None, None),
        State::Running => ("running", None, None),
        State::Done(c) => ("done", Some(*c), None),
        State::Failed(e) => ("failed", None, Some(e.clone())),
    };
    json!({"state": state, "code": code, "error": error, "lines": r.out})
}

/// Starts the command: the packet's own text is the only source of the script, the browser names the block.
pub fn run_start(db: &Db, id: i64, hash: &str) -> Result<()> {
    let (meta, open) = packet_meta(db, id)?;
    if !open {
        anyhow::bail!("this packet is finished: nothing runs from it");
    }
    let Some(block) = find_block(&meta, hash) else { anyhow::bail!("that command is not in this packet (it may have been revised): reload the page") };
    let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    for (_, r) in runs.iter_mut() {
        r.pump();
    }
    if runs.iter().any(|(k, r)| *k == (id, hash.to_string()) && r.state == State::Running) {
        anyhow::bail!("it is already running");
    }
    if runs.iter().filter(|(_, r)| r.state == State::Running).count() >= MAX_RUNNING {
        anyhow::bail!("{MAX_RUNNING} commands are already running: stop one first");
    }
    runs.retain(|(k, _)| *k != (id, hash.to_string()));
    let mut r = Runner::new(vec![block]);
    if !r.run() {
        anyhow::bail!("this command cannot be run here");
    }
    runs.push(((id, hash.to_string()), r));
    Ok(())
}

pub fn run_poll(id: i64, hash: &str) -> Value {
    let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    match runs.iter_mut().find(|(k, _)| *k == (id, hash.to_string())) {
        Some((_, r)) => {
            r.pump();
            state_json(r)
        }
        None => json!({"state": "ready", "code": null, "error": null, "lines": []}),
    }
}

pub fn run_stop(id: i64, hash: &str) -> Result<()> {
    let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    match runs.iter_mut().find(|(k, _)| *k == (id, hash.to_string())) {
        Some((_, r)) => {
            if r.kill() {
                Ok(())
            } else {
                anyhow::bail!("it is not running")
            }
        }
        None => anyhow::bail!("it is not running"),
    }
}

/// Puts what a finished run printed in front of the agent, as a note from John (which wakes it). Nothing is sent unless he asks.
pub fn run_share(db: &Db, id: i64, hash: &str) -> Result<()> {
    let text = {
        let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, r)) = runs.iter_mut().find(|(k, _)| *k == (id, hash.to_string())) else { anyhow::bail!("nothing has been run") };
        r.pump();
        if r.state == State::Running {
            anyhow::bail!("it is still running");
        }
        r.summary(40)
    };
    kinds::note(db, id, &format!("{text}\n(run from the packet page)")).map(|_| ())
}

/// The buttons and output of one command, as the page last saw it (a reload keeps a run's output).
fn run_panel(id: i64, hash: &str) -> String {
    let v = run_poll(id, hash);
    let state = v["state"].as_str().unwrap_or("ready");
    let lines: Vec<&str> = v["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str()).collect();
    let status = match state {
        "running" => "running".to_string(),
        "done" => format!("exit {}", v["code"]),
        "failed" => v["error"].as_str().unwrap_or("could not run").to_string(),
        _ => String::new(),
    };
    format!(
        "<div class=\"run\" data-block=\"{hash}\" data-state=\"{state}\"><div class=\"runbar\"><button type=\"button\" data-run>{}</button><button type=\"button\" data-stop>stop</button><button type=\"button\" data-share>send output to the agent</button><span class=\"status\">{}</span></div><pre class=\"out\"{}>{}</pre></div>",
        if state == "ready" { "run" } else { "run again" },
        esc(&status),
        if lines.is_empty() { " hidden" } else { "" },
        esc(&lines.join("\n"))
    )
}

// ---- colours: the window's theme

/// The colour as `#rrggbb`: the window's palettes hold true colours, or 256-colour indexes when the terminal is not true-colour.
pub(crate) fn rgb_of(c: Color) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i @ 0..=15) => BASIC[i as usize],
        Color::Indexed(i @ 16..=231) => {
            let level = |n: u8| [0, 95, 135, 175, 215, 255][n as usize];
            let n = i - 16;
            (level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        Color::Indexed(i) => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
        _ => (128, 128, 128),
    }
}

fn hex(c: Color) -> String {
    let (r, g, b) = rgb_of(c);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The window's saved theme id and palette (the default theme when none is saved or it is gone). A saved VS Code theme exists only once the
/// background scan has found it, so it is waited for briefly.
pub fn theme() -> (String, Palette) {
    let saved = std::fs::read_to_string(crate::paths::data_dir().join("settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["theme"].as_str().map(String::from));
    if saved.as_deref().is_some_and(|t| t.starts_with(crate::tui::vscode_themes::ID_PREFIX)) {
        crate::tui::vscode_themes::start_scan();
        for _ in 0..30 {
            if crate::tui::vscode_themes::scanned().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    let all = palettes();
    let pick = saved.as_deref().and_then(|s| all.iter().find(|(k, _)| k == s)).or_else(|| all.first());
    let (id, p) = pick.expect("there is always a built-in theme");
    (id.clone(), p.clone())
}

/// The theme as CSS variables (the window's own names), and the `color-scheme` the controls need.
fn theme_css(p: &Palette) -> String {
    let names = ["bg", "panel", "line", "fg", "mu", "fa", "rule", "pk", "or", "ye", "gr", "cy", "pu"];
    let vars: String = names.iter().map(|n| format!("--{n}:{};", hex(p.color(n)))).collect();
    let dark = match p.color("bg") {
        Color::Rgb(r, g, b) => (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000 < 128,
        _ => true,
    };
    format!(":root{{{vars}color-scheme:{}}}", if dark { "dark" } else { "light" })
}

// ---- the page

fn meta_of(doc: &Value) -> Value {
    doc["thread"]["meta"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null)
}

fn is_packet(doc: &Value) -> bool {
    doc["thread"]["channel"] == "question" && kinds::kind_in(Some(&meta_of(doc))) == kinds::PACKET
}

/// A token that changes when anything on the packet does (its spec, John's progress, its status, a new message, the theme): the page
/// compares it.
pub fn version(db: &Db, id: i64) -> Result<String> {
    use std::hash::{Hash, Hasher};
    let doc = db.get_thread(id)?;
    if !is_packet(&doc) {
        anyhow::bail!("#{id} is not a packet");
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        doc["thread"]["status"].as_str(),
        doc["thread"]["meta"].as_str(),
        doc["messages"].as_array().map(|m| m.len()),
        doc["messages"].as_array().and_then(|m| m.last()).map(|m| m["id"].as_i64()),
        theme().0,
    )
        .hash(&mut h);
    Ok(format!("{:x}", h.finish()))
}

fn when(ts: &str) -> String {
    ts.replace('T', " ").chars().take(16).collect()
}

fn section(title: &str, body: &str) -> String {
    format!("<section><h2>{title}</h2>{body}</section>")
}

/// The inside of the page: the status bar, the header, the packet's parts, the steps as controls, the conversation.
pub fn app(db: &Db, id: i64) -> Result<String> {
    let doc = db.get_thread(id)?;
    if !is_packet(&doc) {
        anyhow::bail!("#{id} is not a packet");
    }
    let meta = meta_of(&doc);
    let spec = &meta["packet"];
    let status = doc["thread"]["status"].as_str().unwrap_or("");
    let open = status == "open";
    let dis = if open { "" } else { " disabled" };
    let run = open.then_some(id);
    let (done, total) = kinds::progress(&meta);
    let asker = doc["thread"]["opened_by"].as_str().unwrap_or("an agent");
    let steps = spec["steps"].as_array().cloned().unwrap_or_default();
    let mut h = String::new();

    // status bar: one segment per step, the count, Finish
    h.push_str("<div class=\"bar\"><div class=\"segs\" aria-hidden=\"true\">");
    for st in &steps {
        let on = meta["state"][st["id"].as_str().unwrap_or("")]["done"].as_bool().unwrap_or(false);
        h.push_str(&format!("<i{}></i>", if on { " class=\"on\"" } else { "" }));
    }
    h.push_str(&format!(
        "</div><span class=\"count\"><b>{done}</b> of {total} steps</span><button type=\"button\" class=\"finish\" data-finish data-left=\"{}\"{dis}>Finish packet</button></div>",
        total - done
    ));

    h.push_str("<header>");
    let mut kicker = format!("PACKET #{id} · from {}", esc(asker));
    if let Some(rev) = meta["rev"].as_i64().filter(|r| *r > 1) {
        kicker.push_str(&format!(" · <span class=\"rev\">revised, version {rev}</span>"));
    }
    h.push_str(&format!("<div class=\"kicker\">{kicker}</div><h1>{}</h1><dl class=\"facts\">", esc(doc["thread"]["subject"].as_str().unwrap_or(""))));
    h.push_str(&format!("<dt>Result</dt><dd class=\"result\">{}</dd>", inline(spec["outcome"].as_str().unwrap_or(""))));
    if let Some(date) = spec["deadline"]["date"].as_str() {
        let today = chrono::Local::now().date_naive();
        let cls = match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            Ok(d) if d < today && open => "late",
            Ok(d) if d == today && open => "today",
            _ => "",
        };
        h.push_str(&format!("<dt>Due</dt><dd><span class=\"due {cls}\">{}</span> · {}</dd>", esc(date), inline(spec["deadline"]["reason"].as_str().unwrap_or(""))));
    }
    h.push_str(&format!("<dt>Status</dt><dd>{}</dd></dl></header>", if open { "open".to_string() } else { esc(status) }));
    if !open {
        h.push_str(&format!("<div class=\"closed\">This packet is {}. It is kept for the record; nothing here changes it.</div>", esc(status)));
    }

    if let Some(intro) = meta["intro"].as_str().filter(|i| !i.trim().is_empty()) {
        h.push_str(&section(&format!("Note from {}", esc(asker)), &blocks(intro, run)));
    }
    h.push_str(&section("Why it is yours", &blocks(spec["why"].as_str().unwrap_or(""), run)));

    let mut happened = String::from("<ul>");
    for f in spec["happened"].as_array().into_iter().flatten() {
        let link =
            f["link"].as_str().filter(|l| safe_url(l)).map(|l| format!(" <a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">source</a>", esc(l))).unwrap_or_default();
        happened.push_str(&format!("<li>{}{link}</li>", inline(f["fact"].as_str().unwrap_or(""))));
    }
    happened.push_str("</ul>");
    h.push_str(&section("What happened", &happened));

    let found = spec["found"].as_array().cloned().unwrap_or_default();
    let mut links = String::new();
    if found.is_empty() {
        links.push_str("<p class=\"mu\">Nothing to open: everything needed is on this page.</p>");
    } else {
        links.push_str("<ul class=\"links\">");
        for f in &found {
            let url = f["url"].as_str().unwrap_or("");
            let label = f["label"].as_str().filter(|l| !l.is_empty()).unwrap_or_else(|| f["domain"].as_str().unwrap_or(url));
            let mut extra = format!("<span class=\"mu\">{}</span>", esc(f["domain"].as_str().unwrap_or("")));
            if f["cited"].as_i64().unwrap_or(1) > 1 {
                extra.push_str(&format!(" <span class=\"mu\">cited {} times</span>", f["cited"]));
            }
            if f["unknown_domain"].as_bool().unwrap_or(false) {
                extra.push_str(" <span class=\"warn\">new domain: check before you sign in</span>");
            }
            if let Some(n) = f["note"].as_str().filter(|n| !n.is_empty()) {
                extra.push_str(&format!(" <span class=\"mu\">{}</span>", esc(n)));
            }
            if safe_url(url) {
                links.push_str(&format!("<li><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a> {extra}</li>", esc(url), esc(label)));
            }
        }
        links.push_str("</ul>");
    }
    h.push_str(&section("What the agent found", &links));

    let mut list = String::from("<ol class=\"steps\">");
    for st in &steps {
        let sid = st["id"].as_str().unwrap_or("");
        let state = &meta["state"][sid];
        let ticked = state["done"].as_bool().unwrap_or(false);
        let value = state["value"].as_str().unwrap_or("");
        list.push_str(&format!("<li class=\"step{}\" data-step=\"{}\">", if ticked { " ticked" } else { "" }, esc(sid)));
        list.push_str(&format!(
            "<label class=\"head\"><input type=\"checkbox\" data-tick{}{}><span class=\"t\">{}</span></label>",
            if ticked { " checked" } else { "" },
            dis,
            esc(st["title"].as_str().unwrap_or(""))
        ));
        if let Some(d) = st["detail"].as_str() {
            list.push_str(&format!("<div class=\"detail\">{}</div>", blocks(d, run)));
        }
        if st["input"] == "choice" {
            list.push_str("<div class=\"choices\">");
            for o in st["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str()) {
                list.push_str(&format!("<button type=\"button\" class=\"chip{}\" data-choose=\"{}\"{}>{}</button>", if o == value { " on" } else { "" }, esc(o), dis, esc(o)));
            }
            list.push_str("</div>");
        }
        list.push_str(&format!(
            "<textarea rows=\"1\" placeholder=\"Note for the agent with this step (optional)\" data-note{dis}>{}</textarea></li>",
            esc(state["note"].as_str().unwrap_or(""))
        ));
    }
    list.push_str("</ol>");
    h.push_str(&section("Your steps", &list));

    h.push_str(&section("What the agent does next", &blocks(spec["next"].as_str().unwrap_or(""), run)));
    if let Some(w) = spec["work"].as_object() {
        let mut work = String::new();
        for (k, label) in [
            ("recommendation", "Recommendation"),
            ("confidence", "Confidence"),
            ("ruled_out", "Ruled out"),
            ("would_change", "Would change my mind"),
            ("prepared_with", "Prepared with"),
        ] {
            if let Some(v) = w.get(k).and_then(|v| v.as_str()) {
                work.push_str(&format!("<dt>{label}</dt><dd>{}</dd>", inline(v)));
            }
        }
        h.push_str(&section("Agent's working", &format!("<dl class=\"facts\">{work}</dl>")));
    }

    let mut talk = String::new();
    for m in doc["messages"].as_array().into_iter().flatten().skip(1) {
        let kind = m["meta"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["kind"].as_str().map(String::from)).unwrap_or_default();
        if matches!(kind.as_str(), "read-receipt" | "ack" | "ack-note") {
            continue;
        }
        let who = m["author"].as_str().unwrap_or("");
        talk.push_str(&format!(
            "<div class=\"msg{}\"><div class=\"who\"><b>{}</b><span>{}</span></div><div class=\"body\">{}</div></div>",
            if who == "john" { " me" } else { "" },
            esc(who),
            esc(&when(m["ts"].as_str().unwrap_or(""))),
            blocks(m["body"].as_str().unwrap_or(""), None)
        ));
    }
    if talk.is_empty() {
        talk.push_str("<p class=\"mu\">Nothing yet. Ticking a step, or writing here, tells the agent.</p>");
    }
    talk.push_str(&format!(
        "<textarea rows=\"3\" id=\"say\" placeholder=\"Write to the agent\" data-say{dis}></textarea><div class=\"row\"><button type=\"button\" data-send{dis}>Send</button></div>"
    ));
    h.push_str(&section("Conversation", &talk));
    Ok(h)
}

pub fn page(db: &Db, id: i64) -> Result<String> {
    let app = app(db, id)?;
    let v = version(db, id)?;
    let (theme_id, palette) = theme();
    let title = db.get_thread(id)?["thread"]["subject"].as_str().unwrap_or("Packet").to_string();
    Ok(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{} · AgentDesk</title><style>{}{CSS}</style></head>\
<body><div id=\"banner\" hidden></div><div id=\"dict\" hidden></div><main id=\"app\" data-v=\"{v}\" data-id=\"{id}\" data-theme=\"{}\">{app}</main><div id=\"toast\" hidden></div><script>{JS}</script></body></html>",
        esc(&title),
        theme_css(&palette),
        esc(&theme_id)
    ))
}

const CSS: &str = r#"
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.6 ui-sans-serif,system-ui,"Segoe UI",Roboto,sans-serif}
code,pre,.kicker,h2,.count,.status,button,.who,.step::before,.codebar,dt,.segs{font-family:ui-monospace,"Cascadia Mono","SF Mono",Consolas,Menlo,monospace}
main{max-width:50rem;min-width:0;margin:0 auto;padding:0 1.25rem 5rem;overflow-wrap:anywhere}
pre{overflow-wrap:normal}
a{color:var(--cy);text-underline-offset:.15em}a:hover{color:var(--fg)}
b,strong{font-weight:600}
.mu{color:var(--mu)}.warn{color:var(--pk)}

/* status bar */
.bar{position:sticky;top:0;z-index:10;display:flex;align-items:center;gap:1rem;margin:0 -1.25rem;padding:.55rem 1.25rem;background:var(--bg);border-bottom:1px solid var(--rule)}
.segs{display:flex;gap:3px;flex:1;min-width:0}
.segs i{flex:0 1 1.6rem;height:.45rem;background:var(--line)}
.segs i.on{background:var(--gr)}
.count{font-size:.8rem;color:var(--mu);white-space:nowrap}.count b{color:var(--fg);font-weight:600}
button{font:inherit;font-size:.8rem;color:var(--fg);background:none;border:1px solid var(--rule);border-radius:2px;padding:.28rem .75rem;cursor:pointer}
button:hover:not(:disabled){border-color:var(--fg)}
button:focus-visible,textarea:focus-visible,a:focus-visible,input:focus-visible{outline:2px solid var(--cy);outline-offset:2px}
button:disabled,textarea:disabled,input:disabled{opacity:.45;cursor:not-allowed}
.finish{color:var(--bg);background:var(--gr);border-color:var(--gr);font-weight:600}
.finish:hover:not(:disabled){background:var(--gr);filter:brightness(1.12);border-color:var(--gr)}

/* header */
header{padding:2.2rem 0 1rem}
.kicker{font-size:.74rem;letter-spacing:.07em;color:var(--mu);text-transform:uppercase}
.rev{color:var(--or)}
h1{font-size:1.55rem;line-height:1.25;font-weight:600;margin:.45rem 0 1.1rem;letter-spacing:-.01em}
.facts{display:grid;grid-template-columns:7.5rem minmax(0,1fr);margin:0;border-top:1px solid var(--rule)}
.facts dt,.facts dd{margin:0;padding:.55rem 0;border-bottom:1px solid var(--rule);min-width:0}
.facts dt{font-size:.72rem;letter-spacing:.07em;text-transform:uppercase;color:var(--mu);padding-top:.7rem}
.result{font-weight:600}
.due.late{color:var(--pk);font-weight:600}.due.today{color:var(--or);font-weight:600}
.closed{margin:1rem 0;padding:.6rem .8rem;background:var(--panel);border:1px solid var(--rule);color:var(--mu)}

/* sections */
section{margin-top:2.4rem}
h2{font-size:.72rem;font-weight:600;letter-spacing:.09em;text-transform:uppercase;color:var(--mu);margin:0 0 .8rem;padding-bottom:.4rem;border-bottom:1px solid var(--rule)}
section p,section ul{margin:.45rem 0}ul{padding-left:1.25rem}li{margin:.2rem 0}
.links{list-style:none;padding:0}
code{background:var(--panel);border:1px solid var(--rule);padding:.02rem .3rem;font-size:.88em;border-radius:2px}
.code{min-width:0;max-width:100%;margin:.7rem 0;border:1px solid var(--rule);background:var(--panel)}
.codebar{display:flex;justify-content:space-between;align-items:center;padding:.2rem .5rem .2rem .7rem;border-bottom:1px solid var(--rule);font-size:.72rem;color:var(--mu)}
.codebar button{border:0;padding:.1rem .4rem;color:var(--cy)}
pre{margin:0;padding:.7rem .8rem;overflow-x:auto;font-size:.85rem;line-height:1.5;tab-size:4}pre code{background:none;border:0;padding:0;font-size:inherit}
.mail .acts{display:flex;gap:.8rem;align-items:center}.mail .open{color:var(--bg);background:var(--cy);padding:.15rem .6rem;text-decoration:none;font-weight:600}
.hdr{display:grid;grid-template-columns:4.5rem minmax(0,1fr);margin:0;padding:.5rem .8rem;border-bottom:1px solid var(--rule);font-size:.85rem}
.hdr dt{color:var(--mu);font-size:.72rem;letter-spacing:.07em;text-transform:uppercase;padding:.1rem 0}.hdr dd{margin:0;padding:.05rem 0}
.mailbody,.mailbody code{white-space:pre-wrap;overflow-wrap:anywhere;font-family:ui-sans-serif,system-ui,"Segoe UI",Roboto,sans-serif;font-size:.93rem}
.run{border-top:1px solid var(--rule)}
.runbar{display:flex;align-items:center;gap:.5rem;padding:.35rem .5rem .35rem .7rem}
.runbar [data-run]{color:var(--bg);background:var(--cy);border-color:var(--cy);font-weight:600}
.runbar [data-stop],.runbar [data-share]{display:none}
.run[data-state=running] [data-stop]{display:inline-block}
.run[data-state=done] [data-share],.run[data-state=failed] [data-share]{display:inline-block}
.run[data-state=running] [data-run]{display:none}
.status{font-size:.75rem;color:var(--mu);margin-left:auto}
.run[data-state=running] .status{color:var(--ye)}
.run[data-state=failed] .status{color:var(--pk)}
.out{border-top:1px solid var(--rule);background:var(--bg);max-height:22rem;overflow:auto;white-space:pre-wrap;word-break:break-word}

/* steps */
.steps{list-style:none;margin:0;padding:0;counter-reset:s}
.step{counter-increment:s;position:relative;padding:1rem 0 1rem 2.4rem;border-top:1px solid var(--rule)}
.step:last-child{border-bottom:1px solid var(--rule)}
.step::before{content:counter(s,decimal-leading-zero);position:absolute;left:0;top:1.12rem;font-size:.78rem;color:var(--fa)}
.head{display:flex;gap:.7rem;align-items:flex-start;cursor:pointer;font-weight:600}
.t{min-width:0}
.head input{appearance:none;-webkit-appearance:none;flex:none;width:1.15rem;height:1.15rem;margin:.22rem 0 0;border:1px solid var(--mu);border-radius:2px;background:none;display:grid;place-content:center;cursor:pointer}
.head input:checked{background:var(--gr);border-color:var(--gr)}
.head input:checked::after{content:"";width:.3rem;height:.6rem;border:solid var(--bg);border-width:0 2px 2px 0;transform:rotate(45deg) translate(-1px,-1px)}
.step.ticked .t{color:var(--mu);font-weight:500}
.detail{margin:.45rem 0 0 1.85rem;font-size:.93rem}
.choices{display:flex;flex-wrap:wrap;gap:.4rem;margin:.6rem 0 0 1.85rem}
.chip.on{color:var(--bg);background:var(--ye);border-color:var(--ye);font-weight:600}
.step textarea{display:block;width:calc(100% - 1.85rem);margin:.7rem 0 0 1.85rem;background:transparent;color:var(--fg);border:0;border-bottom:1px dashed var(--rule);padding:.3rem 0;font:inherit;font-size:.9rem;resize:vertical}
.step textarea:focus{border-bottom:1px solid var(--cy);outline:0}

/* conversation */
.msg{display:grid;grid-template-columns:8.5rem minmax(0,1fr);gap:0 1rem;padding:.7rem 0;border-top:1px solid var(--rule)}
.who{display:flex;flex-direction:column;font-size:.74rem;color:var(--mu);overflow:hidden}.who b{color:var(--cy);font-weight:600;overflow:hidden;text-overflow:ellipsis}.msg.me .who b{color:var(--ye)}
.body{min-width:0}.body p{margin:.1rem 0}
#say{display:block;width:100%;margin-top:.9rem;background:var(--panel);color:var(--fg);border:1px solid var(--rule);border-radius:2px;padding:.55rem .7rem;font:inherit;resize:vertical}
.row{margin-top:.5rem}[data-send]{color:var(--bg);background:var(--cy);border-color:var(--cy);font-weight:600}

#banner{position:sticky;top:0;z-index:11;padding:.5rem 1.25rem;background:var(--panel);color:var(--ye);border-bottom:1px solid var(--ye);cursor:pointer;font-size:.85rem}
#dict{position:fixed;left:1rem;bottom:1rem;z-index:12;max-width:min(32rem,90vw);padding:.5rem .8rem;background:var(--panel);border:1px solid var(--cy);color:var(--fg);font:.82rem ui-monospace,"Cascadia Mono",Consolas,monospace}
#dict b{color:var(--pk)}
#toast{position:fixed;bottom:1rem;left:50%;transform:translateX(-50%);background:var(--panel);color:var(--fg);border:1px solid var(--pk);padding:.55rem 1rem;max-width:90vw;font-size:.88rem}
@media(max-width:34rem){.facts{grid-template-columns:minmax(0,1fr)}.facts dt{border-bottom:0;padding-bottom:0}.msg{grid-template-columns:minmax(0,1fr)}.who{flex-direction:row;gap:.6rem}.segs i{flex-basis:.8rem}}
"#;

const JS: &str = r#"
const $=(s,r=document)=>r.querySelector(s),$$=(s,r=document)=>[...r.querySelectorAll(s)],app=()=>$('#app');
let dirty=false;
const url=p=>'/packet/'+app().dataset.id+'/'+p;
function toast(t){const e=$('#toast');e.textContent=t;e.hidden=false;clearTimeout(toast.t);toast.t=setTimeout(()=>e.hidden=true,5000)}
async function post(path,body){return rpc(url(path),body)}
async function rpc(to,body){
  const r=await fetch(to,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body||{})});
  const j=await r.json().catch(()=>({error:'no answer from AgentDesk'}));
  if(!r.ok||j.error){toast(j.error||'failed');return null}
  return j
}
async function act(path,body){const j=await post(path,body);if(j){dirty=false;await refresh()}return !!j}
async function refresh(){
  const r=await fetch(location.pathname,{cache:'no-store'});if(!r.ok)return;
  const d=new DOMParser().parseFromString(await r.text(),'text/html'),n=$('#app',d);
  if(!n)return;
  if(n.dataset.theme!==app().dataset.theme){location.reload();return}
  app().innerHTML=n.innerHTML;app().dataset.v=n.dataset.v;$('#banner').hidden=true
}
function paint(box,j){
  box.dataset.state=j.state;
  const out=$('.out',box),at=out.scrollTop+out.clientHeight>=out.scrollHeight-8;
  out.hidden=!j.lines.length;out.textContent=j.lines.join('\n');if(at)out.scrollTop=out.scrollHeight;
  $('.status',box).textContent=j.state==='running'?'running':j.state==='done'?'exit '+j.code:j.state==='failed'?(j.error||'could not run'):'';
  $('[data-run]',box).textContent='run again'
}
async function poll(box){
  const r=await fetch(url('run?block='+box.dataset.block),{cache:'no-store'});if(r.ok)paint(box,await r.json())
}
setInterval(()=>$$('.run[data-state=running]').forEach(poll),700);
setInterval(async()=>{
  try{
    const r=await fetch(url('version'),{cache:'no-store'});if(!r.ok)return;
    const v=(await r.json()).v;if(v===app().dataset.v)return;
    const a=document.activeElement,busy=dict||a&&a.tagName==='TEXTAREA'&&a.value!==a.defaultValue;
    if(busy||dirty){const b=$('#banner');b.textContent='The agent changed this packet. Click to load the changes (what you are typing stays until you do).';b.hidden=false;b.onclick=refresh}
    else refresh()
  }catch(e){}
},2500);
document.addEventListener('input',e=>{if(e.target.matches('textarea'))dirty=true});
/* dictation: Ctrl+D listens through the core (the same local speech-to-text as the window), Ctrl+D again puts the words in the text box, Esc drops them */
let dict=null;
const dBar=t=>{const e=$('#dict');e.hidden=!t;e.innerHTML=t||''};
const dTarget=()=>{const a=document.activeElement;if(a&&a.matches&&a.matches('textarea')){const s=a.closest('[data-step]');return s?{step:s.dataset.step}:{say:true}}return{say:true}};
const dEl=t=>t.step?$('[data-step="'+CSS.escape(t.step)+'"] [data-note]'):$('#say');
async function dPoll(){
  if(!dict||dict.stopping)return;
  try{const r=await fetch('/dictate/poll',{method:'POST',headers:{'Content-Type':'application/json'},body:'{}'});const j=await r.json();
    if(!dict||dict.stopping)return;
    if(j.state==='done'){return dEnd(j.text,j.error,false)}
    const t=(j.text||'');dBar('<b>●</b> '+(j.state==='listening'||!j.state?'listening':j.state)+(t?': '+esc(t.slice(-90)):'')+' · Ctrl+D inserts · Esc drops')
  }catch(e){}
}
const esc=s=>s.replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
function dEnd(text,error,drop){
  const t=dict&&dict.target;clearInterval(dict&&dict.timer);dict=null;dBar('');
  if(error)toast('Dictation: '+error);
  if(drop||!text||!text.trim()){if(!error)toast(drop?'Dictation dropped.':'Dictation: nothing heard.');return}
  const el=t&&dEl(t);if(!el){toast('Dictation: the text box is gone; the words were: '+text);return}
  el.focus();const a=el.selectionEnd??el.value.length;el.setRangeText(text.trim()+' ',a,a,'end');dirty=true;toast('Dictation: done.')
}
async function dictToggle(drop){
  if(dict){
    if(dict.stopping)return;dict.stopping=true;dBar('finishing…');
    try{await rpc('/dictate/stop')}catch(e){}
    let text='',error=null;
    for(let i=0;i<50;i++){
      try{const r=await fetch('/dictate/poll',{method:'POST',headers:{'Content-Type':'application/json'},body:'{}'});const j=await r.json();text=j.text||'';error=j.error||null;if(j.state==='done'||j.state==='idle')break}catch(e){break}
      await new Promise(s=>setTimeout(s,160))
    }
    return dEnd(text,error,drop)
  }
  if(drop)return;
  const target=dTarget();dBar('starting the microphone…');
  const j=await rpc('/dictate/start');
  if(!j||j.state==='done'){dBar('');if(j&&j.error)toast('Dictation: '+j.error);return}
  dict={target,timer:setInterval(dPoll,350),stopping:false};
  dBar('<b>●</b> '+(j.state==='listening'?'listening':esc(j.state||'listening'))+(j.device?' on '+esc(j.device):'')+' · Ctrl+D inserts · Esc drops')
}
document.addEventListener('keydown',e=>{
  if(e.ctrlKey&&!e.altKey&&!e.shiftKey&&!e.metaKey&&e.key.toLowerCase()==='d'){e.preventDefault();dictToggle(false)}
  else if(e.key==='Escape'&&dict){e.preventDefault();dictToggle(true)}
},true);
document.addEventListener('click',async e=>{
  const t=e.target,step=t.closest('[data-step]'),box=t.closest('.run');
  const open=t.closest&&t.closest('a[data-paste]');
  if(open){const code=$('code',open.closest('.code'));try{await navigator.clipboard.writeText(code.textContent);toast('The message is long, so it is copied: paste it into the Outlook message.')}catch(x){}}
  if(t.matches('[data-copy]')){const code=$('code',t.closest('.code'));try{await navigator.clipboard.writeText(code.textContent);t.textContent='copied'}catch(x){t.textContent='select it, then copy'}setTimeout(()=>t.textContent='copy',1500);return}
  if(box&&t.matches('[data-run]')){
    if(!confirm('Run this command on this computer, as you?\n\n'+$('code',t.closest('.code')).textContent.slice(0,600)))return;
    const j=await post('run',{block:box.dataset.block});if(j){paint(box,{state:'running',lines:[]});poll(box)}return}
  if(box&&t.matches('[data-stop]')){await post('stop',{block:box.dataset.block});poll(box);return}
  if(box&&t.matches('[data-share]')){if(await post('share',{block:box.dataset.block}))toast('Sent to the agent.');await refresh();return}
  if(t.matches('[data-choose]')&&step){await act('step',{step:step.dataset.step,done:true,value:t.dataset.choose,note:$('[data-note]',step).value});return}
  if(t.matches('[data-send]')){const s=$('#say');if(!s.value.trim())return;await act('note',{body:s.value});return}
  if(t.matches('[data-finish]')){const left=+t.dataset.left;if(confirm(left?left+' step(s) are not ticked. Finish anyway?':'Finish this packet?'))await act('finish');return}
});
document.addEventListener('change',async e=>{
  const t=e.target;if(!t.matches('[data-tick]'))return;
  const step=t.closest('[data-step]'),chosen=$('.chip.on',step);
  const ok=await act('step',{step:step.dataset.step,done:t.checked,value:chosen?chosen.dataset.choose:undefined,note:$('[data-note]',step).value});
  if(!ok)t.checked=!t.checked
});
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_from_the_board_cannot_become_markup() {
        assert_eq!(inline("<script>alert(1)</script>"), "&lt;script&gt;alert(1)&lt;/script&gt;");
        assert_eq!(inline("[x](javascript:alert(1))"), "[x](javascript:alert(1))");
        assert!(inline("[x](https://a.b/c?d=1&e=2)").contains("href=\"https://a.b/c?d=1&amp;e=2\""));
        assert!(inline("[x](\"onmouseover=\"y)").starts_with("[x]("));
        assert_eq!(inline("**a <b>**"), "<strong>a &lt;b&gt;</strong>");
        assert_eq!(inline("a `<i>` b"), "a <code>&lt;i&gt;</code> b");
    }

    #[test]
    fn blocks_make_paragraphs_lists_and_copyable_code() {
        let h = blocks("One\ntwo\n\n- a\n- b\n\n  ```powershell\n  gh pr view 3\n    nested\n  ```\nafter", None);
        assert!(h.contains("<p>One<br>two</p>"), "{h}");
        assert!(h.contains("<ul><li>a</li><li>b</li></ul>"), "{h}");
        assert!(h.contains("data-copy") && !h.contains("data-run"), "{h}");
        assert!(h.contains("<code>gh pr view 3\n  nested</code>"), "an indented fence loses its indent: {h}");
        assert!(h.ends_with("<p>after</p>"), "{h}");
        assert!(blocks("```sh\nunclosed <b>", None).contains("&lt;b&gt;"));
    }

    #[test]
    fn a_shell_block_in_a_packet_gets_a_run_button_named_by_its_hash_and_other_blocks_do_not() {
        let md = "Do:\n  ```powershell\n  gh pr view 3\n  ```\n```json\n{\"a\":1}\n```";
        let h = blocks(md, Some(7));
        let found = runner::extract_blocks(&[("a".into(), md.to_string())]);
        assert_eq!(found.len(), 1);
        assert!(h.contains(&format!("data-block=\"{}\"", block_hash(&found[0].script))), "the page and the server name a block the same way: {h}");
        assert_eq!(h.matches("data-run>").count(), 1, "only the shell block runs: {h}");
    }

    #[test]
    fn the_windows_colours_become_css_variables_for_true_colour_and_256_colour_palettes() {
        assert_eq!(hex(Color::Rgb(0x22, 0x1f, 0x22)), "#221f22");
        assert_eq!(hex(Color::Indexed(16)), "#000000");
        assert_eq!(hex(Color::Indexed(231)), "#ffffff");
        assert_eq!(hex(Color::Indexed(232)), "#080808");
        let (_, p) = palettes().first().cloned().unwrap();
        let css = theme_css(&p);
        assert!(css.starts_with(":root{--bg:#") && css.contains("--ye:#") && css.contains("--rule:#") && css.ends_with("color-scheme:dark}"), "{css}");
        let light = palettes().iter().find(|(k, _)| k == "monokai-pro-light").map(|(_, p)| theme_css(p)).unwrap();
        assert!(light.ends_with("color-scheme:light}"), "{light}");
    }

    #[test]
    fn an_email_block_is_a_draft_with_an_outlook_button_and_nothing_else_about_it_can_inject() {
        let md = "```email\nTo: Karl Scott <kscott@example.com>; bad address, a@b.org\nCc: <script>@x.com>, carl@example.com\nSubject: Re: build & more\n\nHi Karl,\n\nit's fine <b>.\n```";
        let h = blocks(md, Some(3));
        let href = h.split("href=\"").nth(1).unwrap().split('"').next().unwrap().replace("&amp;", "&");
        assert!(
            href.starts_with(
                "https://outlook.office.com/mail/deeplink/compose?to=kscott%40example.com%3Ba%40b.org&cc=carl%40example.com&subject=Re%3A%20build%20%26%20more&body=Hi%20Karl%2C"
            ),
            "{href}"
        );
        assert!(href.contains("%0D%0A%0D%0Ait%27s%20fine%20%3Cb%3E."), "{href}");
        assert!(h.contains("open in Outlook") && h.contains("draft email · not sent") && h.contains("data-copy"), "{h}");
        assert!(h.contains("<dt>To</dt><dd>kscott@example.com, a@b.org</dd>") && !h.contains("bad address") && !h.contains("<script>"), "{h}");
        assert!(h.contains("it&#39;s fine &lt;b&gt;."), "{h}");
        assert!(!h.contains("data-run>"), "a draft is not a command");
    }

    #[test]
    fn a_long_email_goes_to_outlook_without_its_body_and_is_copied_instead() {
        let body = "word ".repeat(2000);
        let h = blocks(&format!("```email\nTo: a@b.org\nSubject: Long\n\n{body}\n```"), None);
        assert!(h.contains("data-paste") && !h.contains("&amp;body=") && !h.contains("&body="), "{}", &h[..300.min(h.len())]);
        assert!(h.contains("compose?to=a%40b.org&amp;subject=Long\""), "{}", &h[..400.min(h.len())]);
    }
}
