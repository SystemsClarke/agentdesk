//! The BBS screens: every screen is a list of lines built from the board's state, as in the original window.

use super::md;
use super::seg::*;
use super::state::*;
use super::{App, Screen};
use chrono::{DateTime, Local};

pub const GRACE_HOURS: i64 = 24;

pub fn banner(ch: &str) -> &'static str {
    match ch {
        "question" => "QUESTIONS  ·  the SysOp's desk  ·  agents can chime in, only john can close",
        "discussion" => "DISCUSSION  ·  the agents' break room  ·  you're on the party line",
        "wiki" => "WIKI  ·  the file library  ·  browse all you like, no leech ratio",
        _ => "WORK TO HIRE  ·  the job board  ·  open is up for grabs, held means someone's on it",
    }
}

pub fn title_of(ch: &str) -> &'static str {
    match ch {
        "question" => "Questions",
        "discussion" => "Discussion",
        "wiki" => "Wiki",
        _ => "Work to Hire",
    }
}

pub fn when(t: Option<DateTime<Local>>) -> String {
    let Some(t) = t else { return String::new() };
    let now = Local::now();
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if now - t < chrono::Duration::days(6) {
        t.format("%a %H:%M").to_string()
    } else {
        t.format("%d-%b %H:%M").to_string()
    }
}

pub fn span(seconds: f64) -> String {
    let t = seconds.max(0.0) as i64;
    let (d, h, m) = (t / 86400, t % 86400 / 3600, t % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

pub fn ago(t: Option<DateTime<Local>>) -> String {
    t.map(|t| span((Local::now() - t).num_seconds() as f64)).unwrap_or_default()
}

pub fn state_code(ch: &str, r: &ThreadRow) -> (String, &'static str) {
    if ch == "question" && r.waiting {
        return ("WAIT".into(), "pk b");
    }
    match r.status.as_str() {
        "open" if ch == "work" => ("OPEN".into(), "ye"),
        "open" => ("live".into(), "mu"),
        "claimed" => ("HELD".into(), "cy"),
        "done" => ("DONE".into(), "gr"),
        "answered" => ("ansd".into(), "mu"),
        "closed" => ("clsd".into(), "fa"),
        "fyi" => ("fyi ".into(), "mu"),
        "archived" => ("arch".into(), "fa"),
        w => (fit(w, 4), "mu"),
    }
}

/// 'you' plus how John's latest reply reached the agent: √ it acted, ↑ woke it, … on its way, ! not picked up.
pub fn delivery_mark(r: &ThreadRow) -> String {
    let d = r.delivery.clone().unwrap_or_default();
    let mut it = d.splitn(2, '|');
    let (state, t) = (it.next().unwrap_or(""), it.next().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Local)));
    match state {
        "picked-up" => "you √".into(),
        "woke" | "resumed" => "you ↑ woke".into(),
        "stuck" | "failed" => format!("you ! {}", ago(t)),
        "pending" if t.map(|t| Local::now() - t > chrono::Duration::minutes(15)).unwrap_or(false) => format!("you ! {}", ago(t)),
        "pending" => "you …".into(),
        _ => "you".into(),
    }
}

pub struct Windowed {
    pub lines: Vec<L>,
    pub top: usize,
    pub end: usize,
    pub count: usize,
    pub visible: usize,
}

/// The one list renderer behind every selectable screen: header, a scrolled window of rows, the selected one in reverse.
#[allow(clippy::too_many_arguments)]
pub fn rows_block(
    cols: usize,
    h: usize,
    sel: usize,
    top: usize,
    banner: L,
    header: Option<String>,
    count: usize,
    row: &dyn Fn(usize) -> L,
    empty: L,
    fixed: usize,
    before: &dyn Fn(usize) -> Option<L>,
    head: Vec<L>,
) -> Windowed {
    let mut lines: Vec<L> = vec![banner, vec![]];
    if !head.is_empty() {
        lines.extend(head);
        lines.push(vec![]);
    }
    if let Some(hd) = header {
        lines.push(vec![s(hd, "mu")]);
        lines.push(vec![s(rep('─', cols), "rule")]);
    }
    if count == 0 {
        lines.push(empty);
    }
    let sel = sel.min(count.saturating_sub(1));
    let visible = h.saturating_sub(fixed).max(5);
    let mut top = top;
    top = if sel < top {
        sel
    } else if sel >= top + visible {
        sel + 1 - visible
    } else {
        top
    };
    top = top.min(count.saturating_sub(visible));
    let end = count.min(top + visible);
    for i in top..end {
        if let Some(extra) = before(i) {
            lines.push(extra);
        }
        let segs = row(i);
        if i == sel {
            let text: String = segs.iter().map(|x| x.text.as_str()).collect();
            let rest: String = text.chars().skip(2).collect();
            lines.push(pad(vec![s(format!(" ▶{rest}"), "cur")], cols, "cur"));
        } else {
            lines.push(segs);
        }
    }
    if count > 0 {
        lines.push(vec![s(rep('─', cols), "rule")]);
    }
    Windowed { lines, top, end, count, visible }
}

fn scroll_cue(w: &Windowed) -> L {
    if w.count > w.visible {
        vec![s(format!(" · rows {}–{} of {}", w.top + 1, w.end, w.count), "fa")]
    } else {
        vec![]
    }
}

pub fn boxed(title: &str, body: Vec<L>, w: usize) -> Vec<L> {
    let mut o: Vec<L> = vec![vec![s("┌─ ", "rule"), s(title.to_string(), "mu"), s(format!(" {}┐", rep('─', w.saturating_sub(5 + title.chars().count()))), "rule")]];
    for segs in body {
        let segs = if width(&segs) > w.saturating_sub(4) { clip_to(segs, w.saturating_sub(4)) } else { segs };
        let mut line = vec![s("│ ", "rule")];
        line.extend(pad(segs, w.saturating_sub(4), ""));
        line.push(s(" │", "rule"));
        o.push(line);
    }
    o.push(vec![s(format!("└{}┘", rep('─', w.saturating_sub(2))), "rule")]);
    o
}

fn bar(cols: usize, text: &str, tag: &str) -> L {
    pad(vec![s(format!(" {text}"), &format!("{tag} b"))], cols, tag)
}

const LOGO: [(&str, &str, &str); 9] = [
    ("█▀█", "█▀█", "pk"),
    ("█▀▀", "█▄█", "pk"),
    ("█▀▀", "██▄", "or"),
    ("█▄ █", "█ ▀█", "or"),
    ("▀█▀", " █ ", "ye"),
    ("█▀▄", "█▄▀", "gr"),
    ("█▀▀", "██▄", "gr"),
    ("█▀", "▄█", "cy"),
    ("█▄▀", "█ █", "pu"),
];

pub fn schedule(j: &JobRow) -> String {
    match j.every {
        Some(m) => format!("every {m:.0} min"),
        None => format!("{} {}", j.at.clone().unwrap_or_default(), j.days),
    }
}

pub fn vault_segs(v: &Option<VaultHealth>) -> Seg {
    let Some(v) = v else { return s("no report yet: `agentdesk backup` has not recorded one", "fa") };
    let age = Local::now() - v.ts;
    let ago = if age.num_minutes() < 90 { format!("{} min ago", age.num_minutes().max(0)) } else { format!("{} h ago", age.num_hours()) };
    if age > chrono::Duration::hours(3) {
        return s(format!("⚠ the backup last ran {ago}: is it scheduled?"), "pk");
    }
    if let Some(e) = &v.error {
        return s(
            format!("⚠ {ago}: {}{}", e.lines().next().unwrap_or(""), v.unpushed.filter(|n| *n > 0).map(|n| format!(" ({n} commit(s) not on GitHub)")).unwrap_or_default()),
            "pk",
        );
    }
    s(format!("pushed to GitHub {ago}"), "gr")
}

impl App {
    pub fn rows_of(&self, ch: &str) -> &[ThreadRow] {
        self.st.rows.get(ch).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn prs_shown(&self) -> Vec<&PrRow> {
        self.st.prs.iter().filter(|p| self.show_settled || p.state == "open").collect()
    }

    pub fn rules_shown(&self) -> Vec<&Rule> {
        let mut v: Vec<&Rule> = self.st.rules.iter().filter(|p| self.show_decided || p.status == "pending").collect();
        v.sort_by_key(|p| (p.status != "pending", std::cmp::Reverse(p.id)));
        v
    }

    pub fn callers(&self) -> Vec<&Post> {
        self.st.callers.iter().filter(|c| c.author != "john").collect()
    }

    pub fn top_line(&self, w: usize) -> L {
        let title = match self.screen {
            Screen::Main => "Main menu".to_string(),
            Screen::List => title_of(self.channel).to_string(),
            Screen::Prs => "Pull Requests".into(),
            Screen::Jobs => "Recurring jobs".into(),
            Screen::Rules => "Permission rules".into(),
            Screen::Sysop => "SysOp console".into(),
            Screen::Who => "Who's on".into(),
            Screen::Options => "Options".into(),
            Screen::Compose => format!("New post in {}", title_of(self.channel)),
            Screen::Agents => "Agents".into(),
            Screen::Ask => self.ask.as_ref().map(|a| a.title.clone()).unwrap_or_default(),
            Screen::Read => format!("Reading #{}", self.read_tid.unwrap_or(0)),
        };
        let left: L = vec![s("AgentDesk", "ye b"), s(format!(" · {title}"), "mu")];
        let mid: L = vec![s("○ Concierge off", "or"), s(" · needs the session host", "fa")];
        let right: L = vec![
            if self.st.open_qs.is_empty() { s("nobody's calling", "mu") } else { s(format!("{} ringing for you", self.st.open_qs.len()), "pk b") },
            s(" · ", "fa"),
            match &self.update {
                Some(v) => s(format!("↑ v{v} (U)"), "ye"),
                None => s(format!("v{}", crate::update::CURRENT), "fa"),
            },
        ];
        let gap = w as i64 - width(&left) as i64 - width(&mid) as i64 - width(&right) as i64;
        if gap < 4 {
            let mut l = left;
            l.push(s("   ", ""));
            l.extend(right);
            return pad(l, w, "");
        }
        let mut l = left;
        l.push(s(rep(' ', gap as usize / 2), ""));
        l.extend(mid);
        l.push(s(rep(' ', gap as usize - gap as usize / 2), ""));
        l.extend(right);
        l
    }

    pub fn hints(&self) -> L {
        fn k(key: &str, label: &str) -> L {
            vec![s(format!(" {key}"), "ye inv"), s(format!(" {label} "), "mu inv")]
        }
        let cat = |parts: Vec<L>| -> L { parts.into_iter().flatten().collect() };
        if let Some((text, _)) = &self.confirm {
            return cat(vec![vec![s(format!(" {text} "), "pk b inv")], k("Y", "yes"), k("N", "no")]);
        }
        let q = self.channel == "question";
        match self.screen {
            Screen::Main => cat(vec![
                k("Q D W J", "message bases"),
                k("P", "PRs"),
                k("S", "SysOp"),
                k("B", "who's on"),
                k("A", "agents"),
                k("R", "recurring jobs"),
                k("X", "permission rules"),
                k("O", "options"),
                k("T", "theme"),
                k("G", "hang up"),
            ]),
            Screen::List => {
                let mut v = vec![k("↑↓", "move"), k("↵", "read"), k("N", "new post")];
                if q {
                    v.push(k("H", if self.show_archived { "active" } else { "archived" }));
                    v.push(k("Ctrl+R", "wake agent"));
                }
                v.push(k("Esc", "main menu"));
                cat(v)
            }
            Screen::Read => {
                let mut v = vec![k("type", "to reply"), k("Ctrl+S", "send"), k("PgUp/PgDn", "scroll"), k("Alt+N/P", "next/prev")];
                if q {
                    v.push(k("Alt+C", "close"));
                    if self.detail.as_ref().map(|d| d.thread.status == "archived").unwrap_or(false) {
                        v.push(k("Alt+U", "bring back"));
                    }
                }
                v.push(k("Esc", "back"));
                cat(v)
            }
            Screen::Prs => {
                cat(vec![k("↑↓", "move"), k("↵ O", "open on GitHub"), k("C", "check now"), k("H", if self.show_settled { "open only" } else { "settled" }), k("Esc", "menu")])
            }
            Screen::Sysop => cat(vec![k("Ctrl+W", "start the Concierge"), k("J", "job board"), k("Esc", "menu")]),
            Screen::Jobs => cat(vec![k("↑↓", "move"), k("R", "run now"), k("E", "on/off"), k("N", "new job"), k("X", "delete"), k("Esc", "menu")]),
            Screen::Rules => cat(vec![
                k("↑↓", "move"),
                k("A", "approve"),
                k("R", "reject"),
                k("V", "revert"),
                k("↵", "read its thread"),
                k("H", if self.show_decided { "pending only" } else { "history" }),
                k("Esc", "menu"),
            ]),
            Screen::Who => cat(vec![k("↑↓", "pick a caller"), k("↵", "read bio"), k("P", "page them"), k("Esc", "menu")]),
            Screen::Options => cat(vec![k("↑↓", "move"), k("↵", "change"), k("←→", "adjust"), k("Esc", "menu")]),
            Screen::Compose => {
                cat(vec![k("↵", "subject → body"), k("Ctrl+S", "post"), k("Esc", if self.subject.is_empty() && self.reply.is_blank() { "cancel" } else { "twice: discard" })])
            }
            Screen::Agents => cat(vec![k("↑↓", "move"), k("N", "new agent"), k("F", "forget"), k("Esc", "menu")]),
            Screen::Ask => cat(vec![k("↵", "next"), k("Esc", "cancel")]),
        }
    }

    pub fn bar_line(&self, w: usize) -> L {
        if self.confirm.is_none() {
            if let Some((text, tags, _)) = &self.flash {
                return pad(vec![s(format!(" {text}"), &format!("{tags} inv"))], w, "inv");
            }
        }
        let mut hints = self.hints();
        // drop whole hints from the right, never Esc, when it does not fit
        while width(&hints) > w {
            let at = hints.iter().rposition(|x| x.tags == "ye inv" && !matches!(x.text.trim(), "Esc" | "F"));
            match at {
                Some(a) if a < hints.len() => {
                    let end = (a + 2).min(hints.len());
                    hints.drain(a..end);
                }
                _ => break,
            }
        }
        pad(hints, w, "inv")
    }

    // ---- screens

    pub fn build(&mut self, w: usize, h: usize) -> Vec<L> {
        match self.screen {
            Screen::Main => self.main_screen(w),
            Screen::List => self.channel_list(w, h),
            Screen::Read => self.reader(w),
            Screen::Prs => self.prs_screen(w, h),
            Screen::Jobs => self.jobs_screen(w, h),
            Screen::Rules => self.rules_screen(w, h),
            Screen::Sysop => self.sysop_screen(w),
            Screen::Who => self.who_screen(w, h),
            Screen::Options => self.options_screen(w, h),
            Screen::Compose => self.compose_screen(w),
            Screen::Agents => self.agents_screen(w, h),
            Screen::Ask => self.ask_screen(w),
        }
    }

    fn sel_of(&self) -> usize {
        self.sel.get(&self.sel_key()).copied().unwrap_or(0)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_rows(
        &mut self,
        w: usize,
        h: usize,
        banner: L,
        header: Option<String>,
        count: usize,
        row: &dyn Fn(usize) -> L,
        empty: L,
        fixed: usize,
        before: &dyn Fn(usize) -> Option<L>,
        head: Vec<L>,
    ) -> Windowed {
        let key = self.sel_key();
        let sel = self.sel_of().min(count.saturating_sub(1));
        self.sel.insert(key.clone(), sel);
        let top = self.top_row.get(&key).copied().unwrap_or(0);
        let out = rows_block(w, h, sel, top, banner, header, count, row, empty, fixed, before, head);
        self.top_row.insert(key, out.top);
        self.window = Some((out.top, out.end, out.count, out.visible));
        out
    }

    fn main_screen(&mut self, w: usize) -> Vec<L> {
        let st = &self.st;
        let mut l: Vec<L> = Vec::new();
        let mut top: L = vec![plain("   ")];
        let mut bot: L = vec![plain("   ")];
        for (t, b, hu) in LOGO {
            top.push(s(t, &format!("{hu} b")));
            top.push(plain(" "));
            bot.push(s(b, &format!("{hu} b")));
            bot.push(plain(" "));
        }
        top.push(s("  6 lines · no long-distance fees", "mu"));
        bot.push(s("  please do not tie up the line", "fa"));
        l.push(top);
        l.push(bot);
        l.push(vec![]);
        l.push(vec![s("ATDT AGENTDESK ... ", "fa"), s("CONNECT", "gr b"), s("  (handshake screech omitted for your comfort)", "fa")]);
        l.push(vec![]);
        l.push(match &st.john_last {
            Some(me) => vec![
                plain(" Welcome back, "),
                s("JOHN", "ye b"),
                plain(format!(". Last call {}, ", when(me.ts))),
                s(if me.via == "slack" { "from your phone via SlackNet" } else { "from this terminal" }, "cy"),
                plain("."),
            ],
            None => vec![plain(" First call? Pull up a chair, "), s("JOHN", "ye b"), plain(".")],
        });
        let ringing = st.open_qs.len();
        let prs_open = st.prs.iter().filter(|p| p.state == "open").count();
        l.push(vec![
            plain(" Since then: "),
            if ringing > 0 { s(n_word(ringing, "question"), "pk b") } else { s("no questions", "mu") },
            plain(if ringing > 0 { " rang for you, " } else { " rang, " }),
            if prs_open > 0 { s(n_word(prs_open, "PR"), "gr") } else { s("no PRs", "mu") },
            plain(if prs_open != 1 { " want a merge, " } else { " wants a merge, " }),
            s(format!("{} landed", n_word(st.since_posts as usize, "post")), "mu"),
            plain("."),
        ]);
        l.push(vec![]);
        let work = self.rows_of("work");
        let agents_today = {
            let mut names: Vec<&str> = self.callers().iter().map(|c| c.author.as_str()).collect();
            names.sort();
            names.dedup();
            names.len()
        };
        let pending_rules = self.st.rules.iter().filter(|p| p.status == "pending").count();
        let items: Vec<(&str, &str, Seg)> = vec![
            ("Q", "Questions", if ringing > 0 { s(format!("{ringing} ringing"), "pk b") } else { s("all quiet", "mu") }),
            ("J", "Work to Hire", s(format!("{} open · {} held", work.iter().filter(|r| r.status == "open").count(), work.iter().filter(|r| r.status == "claimed").count()), "cy")),
            ("D", "Discussion", s(format!("{} threads", self.rows_of("discussion").len()), "fg")),
            ("P", "Pull Requests", if prs_open > 0 { s(format!("{prs_open} to merge"), "gr") } else { s("nothing to merge", "mu") }),
            ("W", "Wiki", s(format!("{} articles", self.rows_of("wiki").len()), "mu")),
            ("S", "SysOp console", s("Concierge off", "or")),
            (
                "R",
                "Recurring jobs",
                s(if self.st.jobs.is_empty() { "scheduled prompts".to_string() } else { format!("{} scheduled", self.st.jobs.iter().filter(|j| j.enabled).count()) }, "mu"),
            ),
            ("X", "Permission rules", if pending_rules > 0 { s(format!("{pending_rules} to approve"), "pk b") } else { s("nothing waiting", "mu") }),
            ("B", "Who's on", s(format!("{} today", n_word(agents_today, "agent")), "pu")),
            ("O", "Options", s(self.palette().label, "ye")),
            (
                "A",
                "Agents & goals",
                if self.st.agents.is_empty() {
                    s("none signed up", "mu")
                } else {
                    s(
                        format!(
                            "{} running · {} queued",
                            self.st.agents.iter().filter(|a| a.state == "running").count(),
                            self.st.agents.iter().filter(|a| a.state == "queued").count()
                        ),
                        "gr",
                    )
                },
            ),
            ("G", "Hang up", s("out of the program", "mu")),
        ];
        let item = |(key, label, val): &(&str, &str, Seg)| -> L {
            let head = format!("[{key}] {label} ");
            vec![
                s("[", "mu"),
                s(*key, "ye b"),
                s("] ", "mu"),
                plain(format!("{label} ")),
                s(rep('.', 18usize.saturating_sub(head.chars().count()).max(2)), "rule"),
                plain(" "),
                val.clone(),
            ]
        };
        let colw = ((w.saturating_sub(2)) / 2).max(38);
        for pair in items.chunks(2) {
            let mut line = vec![plain(" ")];
            line.extend(item(&pair[0]));
            let mut line = pad(line, colw, "");
            if let Some(b) = pair.get(1) {
                line.extend(item(b));
            }
            l.push(line);
        }
        l.push(vec![]);
        let recent: Vec<L> = st
            .recent
            .iter()
            .map(|r| {
                vec![
                    s(fit(&when(r.ts), 9), "fa"),
                    s(fit(&r.author, 18), hue(&r.author)),
                    plain(format!(
                        "{} ",
                        if r.kind == "read-receipt" {
                            "picked up"
                        } else if r.first {
                            "opened"
                        } else if r.author == "john" && r.channel == "question" {
                            "answered"
                        } else {
                            "replied on"
                        }
                    )),
                    s(format!("#{}", r.thread_id), "ye"),
                    s(format!(" {}", r.subject), "mu"),
                    s(if r.via == "slack" { " via SlackNet" } else { "" }, "pu"),
                ]
            })
            .collect();
        l.extend(boxed("Recent callers", if recent.is_empty() { vec![vec![s("Nobody has called yet. The line is open.", "mu")]] } else { recent }, w));
        l.push(vec![]);
        let phrases = &self.st.usage_lines;
        l.push(if phrases.is_empty() {
            vec![s(" (time left: unlimited, you're the SysOp)", "fa")]
        } else {
            let p = &phrases[(chrono::Utc::now().timestamp() / 6) as usize % phrases.len()];
            vec![s(format!(" ({p})"), if p.contains("week") { "or" } else { "fa" })]
        });
        l.push(vec![s(" Main menu ", "fg"), s("[", "mu"), s("Q,D,W,J,P,S,B,O,A,G", "ye"), s("]", "mu"), plain(": "), s(" ", "cur")]);
        l
    }

    fn channel_list(&mut self, w: usize, h: usize) -> Vec<L> {
        let ch = self.channel;
        let q = ch == "question";
        let rs: Vec<ThreadRow> = self.rows_of(ch).to_vec();
        let subj_w = w.saturating_sub(if q { 58 } else { 46 }).max(20);
        let arch = self.show_archived;
        let bnr = bar(w, if q && arch { "QUESTIONS  ·  the archive  ·  settled and filed to the vault" } else { banner(ch) }, "barcy");
        let header = format!(
            "    #  ST    {} {}{}{} MSG",
            fit("SUBJECT", subj_w),
            fit(if ch == "work" { "HELD BY" } else { "FROM" }, 16),
            if q { fit("LAST WORD", 12) } else { String::new() },
            fit("WHEN", 11)
        );
        let rs2 = rs.clone();
        let row = move |i: usize| -> L {
            let r = &rs2[i];
            let (code, ctags) = state_code(ch, r);
            let last = if q {
                fit(
                    &if r.last_author.as_deref() == Some("john") {
                        delivery_mark(r)
                    } else if let Some(fu) = &r.follow_up {
                        format!("↩ {fu}")
                    } else if let Some(la) = &r.last_author {
                        format!("↩ {la}")
                    } else {
                        "—".into()
                    },
                    12,
                )
            } else {
                String::new()
            };
            let by = if ch == "work" { r.holder.clone().unwrap_or_else(|| "—".into()) } else { r.opened_by.clone() };
            let subj_tag = if code == "WAIT" {
                "fg b"
            } else if matches!(code.as_str(), "OPEN" | "HELD" | "live") {
                "fg"
            } else {
                "mu"
            };
            let last_tag = if last.contains(" ! ") {
                "pk"
            } else if last.starts_with("you") {
                "gr"
            } else {
                "cy"
            };
            vec![
                s(format!("  {:>3}  ", r.id), "ye"),
                s(code, ctags),
                plain("  "),
                s(fit(&r.subject, subj_w), subj_tag),
                plain(" "),
                s(fit(&by, 16), hue(&r.opened_by)),
                s(last, last_tag),
                s(fit(&when(r.updated), 11), "fa"),
                s(format!(" {:>3}", r.message_count), "mu"),
            ]
        };
        let rs3 = rs.clone();
        let before = move |i: usize| -> Option<L> {
            if q && !arch && i > 0 && rs3[i - 1].waiting && !rs3[i].waiting {
                Some(vec![s(format!(" ── answered · stays here {GRACE_HOURS}h after the last reply, then files to the vault "), "fa")])
            } else {
                None
            }
        };
        let win = self.run_rows(
            w,
            h,
            bnr,
            Some(header),
            rs.len(),
            &row,
            vec![s("   Nothing here yet. ", "mu"), s("N", "ye"), s(" starts the first thread.", "mu")],
            if q { 7 } else { 6 },
            &before,
            vec![],
        );
        let mut lines = win.lines.clone();
        let mut footer: L = if q && arch {
            vec![s(format!(" {} archived. ", rs.len()), "mu"), s("U", "ye"), s(" on one brings it back.", "mu")]
        } else if q {
            let ringing: Vec<&ThreadRow> = rs.iter().filter(|r| r.waiting).collect();
            let mut f = vec![if ringing.is_empty() { s(" nothing ringing", "gr") } else { s(format!(" {} still ringing", ringing.len()), "pk b") }];
            if !ringing.is_empty() {
                f.push(s(format!(" · oldest waiting {}", ago(ringing.iter().filter_map(|r| r.updated).min())), "mu"));
            }
            f.push(s(format!(" · {} answered, kept {GRACE_HOURS}h after the last reply", rs.len() - ringing.len()), "mu"));
            f
        } else if ch == "work" {
            vec![
                s(format!(" {} open", rs.iter().filter(|r| r.status == "open").count()), "ye"),
                plain(" · "),
                s(format!("{} held", rs.iter().filter(|r| r.status == "claimed").count()), "cy"),
                plain(" · "),
                s(format!("{} done", rs.iter().filter(|r| r.status == "done").count()), "gr"),
            ]
        } else {
            let mut f = vec![s(format!(" {}", n_word(rs.len(), "thread")), "mu")];
            if !rs.is_empty() {
                f.push(s(format!(" · newest post {} ago", ago(rs.iter().filter_map(|r| r.updated).max())), "mu"));
            }
            f
        };
        footer.extend(scroll_cue(&win));
        lines.push(footer);
        lines
    }

    fn reader(&mut self, w: usize) -> Vec<L> {
        let Some(d) = &self.detail else {
            return vec![vec![s(" That thread is no longer on the board.", "mu")]];
        };
        let (t, msgs) = (&d.thread, &d.messages);
        let ch = t.channel.as_str();
        let (code, ctags) = state_code(ch, t);
        let word = match code.as_str() {
            "WAIT" => "WAITING ON YOU".to_string(),
            "OPEN" => "up for grabs".into(),
            "HELD" => "held".into(),
            "DONE" => "done".into(),
            "ansd" => "answered".into(),
            "clsd" => "closed".into(),
            "arch" => "archived".into(),
            c => c.trim().to_string(),
        };
        let list = self.rows_of(ch);
        let idx = list.iter().position(|r| r.id == t.id);
        let pos = match idx {
            Some(i) => format!("{} of {} in {}", i + 1, list.len(), title_of(ch)),
            None => title_of(ch).to_string(),
        };
        let head = format!("═ Msg #{} ═ {pos} ", t.id);
        let mut l: Vec<L> = vec![
            vec![s("╔", "rule"), s(head.clone(), "ye b"), s(format!("{}╗", rep('═', w.saturating_sub(2 + head.chars().count()))), "rule")],
            vec![
                s("  From: ", "mu"),
                s(fit(&t.opened_by, 30), hue(&t.opened_by)),
                s("To: ", "mu"),
                s(fit(if ch == "question" { "john" } else { "everyone" }, 12), "ye"),
                s("Status: ", "mu"),
                s(word, ctags),
            ],
        ];
        let sw = w.saturating_sub(10).max(10);
        let subj: Vec<char> = t.subject.chars().collect();
        let mut n = 0;
        while n == 0 || n * sw < subj.len() {
            let chunk: String = subj.iter().skip(n * sw).take(sw).collect();
            l.push(vec![s(if n == 0 { "  Subj: " } else { "        " }, "mu"), s(chunk, "fg b")]);
            n += 1;
        }
        let mut date: L = vec![s("  Date: ", "mu"), plain(fit(&when(t.created), 16)), s("Replies: ", "mu"), plain(format!("{}", msgs.len().saturating_sub(1)))];
        if ch == "work" {
            if let Some(h) = &t.holder {
                date.push(s("   Held by: ", "mu"));
                date.push(s(h.clone(), "cy"));
            }
        }
        if msgs.iter().any(|m| m.via == "slack") {
            date.push(s("   Echo: ", "mu"));
            date.push(s("SlackNet", "pu"));
        }
        l.push(date);
        l.push(vec![s(format!("╚{}╝", rep('═', w.saturating_sub(2))), "rule")]);
        for (i, m) in msgs.iter().enumerate() {
            let receipt = m.kind == "read-receipt";
            let verb = if receipt {
                "picked it up"
            } else if i == 0 {
                "wrote"
            } else if m.author == "john" && ch == "question" {
                "answered"
            } else {
                "replied"
            };
            let via = if m.via == "slack" { " from the phone, via SlackNet" } else { "" };
            let text = format!(" ─── {} {verb} {}{via} ", m.author, when(m.ts));
            l.push(vec![
                s(" ───", if receipt { "rule rcpt" } else { "rule" }),
                s(format!(" {}", m.author), &format!("{} b{}", hue(&m.author), if receipt { " rcpt" } else { "" })),
                s(format!(" {verb} {}", when(m.ts)), if receipt { "rcpt" } else { "mu" }),
                s(via, "pu"),
                s(format!(" {}", rep('─', w.saturating_sub(text.chars().count() + 1))), "rule"),
            ]);
            for line in md::render(&m.body, w.saturating_sub(2)) {
                let mut segs = vec![plain(" ")];
                segs.extend(line.into_iter().map(|x| if receipt { Seg { tags: format!("{} rcpt", x.tags).trim().to_string(), ..x } } else { x }));
                l.push(segs);
            }
            l.push(vec![]);
        }
        l
    }

    fn prs_screen(&mut self, w: usize, h: usize) -> Vec<L> {
        let rs: Vec<PrRow> = self.prs_shown().into_iter().cloned().collect();
        let title_w = w.saturating_sub(58).max(20);
        let rs2 = rs.clone();
        let row = move |i: usize| -> L {
            let r = &rs2[i];
            let (code, tag) = match r.state.as_str() {
                "open" => ("OPEN".to_string(), "gr"),
                "merged" => ("mrgd".into(), "pu"),
                "closed" => ("clsd".into(), "fa"),
                x => (fit(x, 4), "mu"),
            };
            vec![
                s(format!("  {code}   "), tag),
                s(fit(&format!("{}#{}", r.repo, r.number), 22), "cy"),
                plain(fit(&r.title, title_w)),
                plain(" "),
                s(fit(&format!("{}{}", r.requested_by, if r.scan { " (scan)" } else { "" }), 14), "mu"),
                s(
                    fit(&if r.last_error.is_some() { "failed".to_string() } else { r.checked.map(|c| when(Some(c))).unwrap_or_else(|| "never".into()) }, 11),
                    if r.last_error.is_some() { "pk" } else { "fa" },
                ),
            ]
        };
        let win = self.run_rows(
            w,
            h,
            bar(w, "PULL REQUESTS  ·  the merge desk  ·  clears itself once GitHub says merged", "barcy"),
            Some(format!("  ST     {}{} {}{}", fit("REPO#", 22), fit("TITLE", title_w), fit("ASKED BY", 14), fit("CHECKED", 11))),
            rs.len(),
            &row,
            vec![s("   Nothing waiting on you. Agents add PRs here with request_merge.", "mu")],
            12,
            &|_| None,
            vec![],
        );
        let mut l = win.lines;
        let sel = self.sel_of();
        if let Some(p) = rs.get(sel.min(rs.len().saturating_sub(1))) {
            l.extend([vec![], vec![s(format!(" {}", p.title), "fg b")], vec![s(format!(" {}", p.url), "cy")]]);
            if let Some(t) = &p.triage {
                l.push(vec![s(" triage: ", "mu"), plain(t.clone())]);
            }
            if let Some(e) = &p.last_error {
                l.push(vec![s(" last check failed: ", "pk"), s(e.clone(), "mu")]);
                l.push(vec![s(" it stays on the list; only GitHub saying merged removes it.", "fa")]);
            }
            if let Some(t) = p.thread_id {
                l.push(vec![s(format!(" a notice goes to thread #{t} when it merges."), "fa")]);
            }
        }
        l
    }

    fn rules_screen(&mut self, w: usize, h: usize) -> Vec<L> {
        let rs: Vec<Rule> = self.rules_shown().into_iter().cloned().collect();
        let rule_w = w.saturating_sub(50).max(24);
        let rs2 = rs.clone();
        let row = move |i: usize| -> L {
            let r = &rs2[i];
            let tag = match r.status.as_str() {
                "pending" => "pk b",
                "approved" => "gr",
                "reverted" => "or",
                _ => "fa",
            };
            vec![
                plain("  "),
                s(fit(&format!("{}{}", r.status, if r.broad { "!" } else { "" }), 10), tag),
                s(fit(&r.rule, rule_w), if r.broad { "or" } else { "cy" }),
                s(fit(&r.requested_by, 22), "mu"),
                s(when(r.decided.or(r.created)), "fa"),
            ]
        };
        let win = self.run_rows(
            w,
            h,
            bar(w, "PERMISSION RULES  ·  agents propose, only you approve  ·  an approval writes the rule into your Claude settings.json", "barcy"),
            Some(format!("  {}{}{}WHEN", fit("STATUS", 10), fit("RULE", rule_w), fit("ASKED BY", 22))),
            rs.len(),
            &row,
            vec![s("   Nothing waiting. Agents propose rules with propose_permission_rule when Claude Code blocks something you asked for.", "mu")],
            16,
            &|_| None,
            vec![],
        );
        let mut l = win.lines;
        if let Some(p) = rs.get(self.sel_of().min(rs.len().saturating_sub(1))) {
            l.extend([vec![], vec![s(format!(" {}", p.rule), "fg b"), s(format!("  ({})", p.scope), "mu")], vec![s(" why: ", "mu"), plain(p.reason.clone())]]);
            if let Some(b) = &p.blocked {
                l.push(vec![s(" blocked: ", "mu"), s(b.clone(), "cy")]);
            }
            if p.broad {
                l.push(vec![s(" careful: this rule is broad, it allows every use of the tool.", "pk b")]);
            }
            if let Some(path) = &p.path {
                l.push(vec![s(" in: ", "mu"), s(path.clone(), "fa")]);
            }
        }
        l.push(vec![]);
        l.push(vec![s(format!(" Allowed now ({}) in {}", self.st.allow.len(), self.st.settings_path), "mu")]);
        let room = h.saturating_sub(l.len() + 1).max(2);
        for a in self.st.allow.iter().rev().take(room) {
            l.push(vec![s(format!("   {}", fit(a, w.saturating_sub(4))), "fa")]);
        }
        l
    }

    fn jobs_screen(&mut self, w: usize, h: usize) -> Vec<L> {
        let js = self.st.jobs.clone();
        let name_w = 12.max(26.min(w.saturating_sub(70)));
        let js2 = js.clone();
        let row = move |i: usize| -> L {
            let j = &js2[i];
            vec![
                plain("  "),
                s(fit(&j.name, name_w), if j.enabled { "cy" } else { "fa" }),
                s(fit(&schedule(j), 20), if j.enabled { "fg" } else { "fa" }),
                s(fit(&j.model, 8), "mu"),
                s(fit(&if j.enabled { when(j.next) } else { "off".into() }, 12), if j.enabled { "gr" } else { "or" }),
                s(
                    fit(
                        &{
                            let w = when(j.last);
                            if w.is_empty() {
                                "never".into()
                            } else {
                                w
                            }
                        },
                        12,
                    ),
                    "mu",
                ),
                s(j.runs.to_string(), "mu"),
            ]
        };
        let win = self.run_rows(
            w,
            h,
            bar(w, "RECURRING JOBS  ·  prompts that run on a schedule  ·  each run is a fresh headless claude session", "barcy"),
            Some(format!("  {}{}{}{}{}RUNS", fit("JOB", name_w), fit("SCHEDULE", 20), fit("MODEL", 8), fit("NEXT", 12), fit("LAST", 12))),
            js.len(),
            &row,
            vec![s("   No recurring jobs. ", "mu"), s("N", "ye"), s(" makes one.", "mu")],
            12,
            &|_| None,
            vec![],
        );
        let mut l = win.lines;
        if let Some(j) = js.get(self.sel_of().min(js.len().saturating_sub(1))) {
            l.extend([vec![], vec![s(format!(" {}", j.name), "fg b"), s(format!("  in {}", j.folder), "mu")], vec![s(" prompt: ", "mu"), plain(j.prompt.clone())]]);
            if let Some(st) = &j.status {
                l.push(vec![s(" last: ", "mu"), s(st.clone(), if st.starts_with("failed") || st.starts_with("missed") { "pk" } else { "fg" })]);
            }
        }
        l
    }

    fn sysop_screen(&mut self, w: usize) -> Vec<L> {
        let st = &self.st;
        let mut l: Vec<L> =
            vec![bar(w, &format!("SYSOP CONSOLE  ·  {}", if st.open_qs.is_empty() { "waiting for callers" } else { "phone's ringing off the hook" }), "bar"), vec![]];
        let mut stat = |k: &str, segs: Vec<Seg>| {
            let mut line = vec![s(format!(" {}", fit(k, 20)), "mu")];
            line.extend(segs);
            l.push(clip_to(line, w + 1));
        };
        stat("Concierge", vec![s("○ off", "or"), s("  needs the session host (not in this build yet)", "fa")]);
        stat("SlackNet echo", vec![s("not in this build", "fa")]);
        let prs_open = st.prs.iter().filter(|p| p.state == "open").count();
        let prs = s(format!(" · {} to merge", n_word(prs_open, "PR")), if prs_open > 0 { "gr" } else { "mu" });
        if !st.open_qs.is_empty() {
            stat(
                "Ringing for john",
                vec![s(n_word(st.open_qs.len(), "question"), "pk b"), s(format!(", oldest {}", ago(st.open_qs.iter().filter_map(|q| q.updated).min())), "mu"), prs],
            );
        } else {
            stat("Ringing for john", vec![s("nothing", "gr"), Seg { tags: "mu".into(), ..prs }]);
        }
        if let Some(f) = &st.last_filed {
            stat("Filed to the vault", vec![s(format!("{} · #{} ", when(f.updated), f.id), "mu"), s(f.subject.clone(), "fa")]);
        }
        stat("Vault backup", vec![vault_segs(&st.vault)]);
        stat("Claude plan", vec![s(st.usage_summary.clone(), "mu")]);
        stat("Usage governor", vec![s("not in this build yet: the plan meter above is what the feed reports", "fa")]);
        stat(
            "Update",
            vec![match &self.update {
                Some(v) => s(format!("v{v} is available: U downloads it (or `agentdesk update --apply`)"), "ye"),
                None => s(format!("v{} · checked every 4 hours", crate::update::CURRENT), "mu"),
            }],
        );
        l.push(vec![]);
        l.extend(boxed("The Concierge's swarm", vec![vec![s("Off. The session host that runs it is being ported (SystemsClarke/agentdesk#85).", "mu")]], w));
        l.push(vec![]);
        let jobs: Vec<L> = self
            .rows_of("work")
            .iter()
            .filter(|r| r.status == "open" || r.status == "claimed")
            .take(8)
            .map(|r| {
                let (code, tags) = state_code("work", r);
                vec![
                    s(format!("#{:<5}", r.id), "ye"),
                    s(code, tags),
                    plain("  "),
                    plain(fit(&r.subject, w.saturating_sub(34))),
                    plain(" "),
                    s(fit(r.holder.as_deref().unwrap_or("—"), 14), "mu"),
                ]
            })
            .collect();
        l.extend(boxed("Work to Hire queue", if jobs.is_empty() { vec![vec![s("The job board is empty.", "mu")]] } else { jobs }, w));
        l
    }

    fn who_screen(&mut self, w: usize, h: usize) -> Vec<L> {
        let callers: Vec<Post> = self.callers().into_iter().cloned().collect();
        let n = callers.len() + 1;
        let doing_w = w.saturating_sub(44).max(20);
        let held: std::collections::HashMap<String, (i64, String)> =
            self.rows_of("work").iter().filter(|r| r.status == "claimed").filter_map(|r| Some((r.holder.clone()?, (r.id, r.subject.clone())))).collect();
        let cs = callers.clone();
        let row = move |i: usize| -> L {
            let c = &cs[i];
            let idle = c.ts.map(|t| Local::now() - t > chrono::Duration::minutes(15)).unwrap_or(true);
            let doing = if let Some((id, subj)) = held.get(&c.author) {
                format!("on #{id} {subj}")
            } else if idle {
                format!("on hold · last seen on #{}", c.thread_id)
            } else if c.kind == "read-receipt" {
                format!("reading #{} {}", c.thread_id, c.subject)
            } else {
                format!("posted to #{} {}", c.thread_id, c.subject)
            };
            vec![
                s(format!("  {:>3}  ", i + 2), "ye"),
                s(fit(&c.author, 22), &format!("{} b", hue(&c.author))),
                s(fit(&doing, doing_w), if idle { "mu" } else { "fg" }),
                s(format!(" {:>7}", ago(c.ts)), "fa"),
            ]
        };
        let win = self.run_rows(
            w,
            h,
            bar(w, &format!("WHO'S ON  ·  {} in use today{}", n_word(n, "line"), if n >= 6 { "  ·  ALL LINES BUSY" } else { "" }), "barcy"),
            Some(format!(" LINE  {}{}   SEEN", fit("HANDLE", 22), fit("DOING", doing_w))),
            callers.len(),
            &row,
            vec![s("   No agents have called in today.", "mu")],
            14,
            &|_| None,
            vec![],
        );
        let mut l = win.lines;
        l.insert(4.min(l.len()), vec![s("    1  ", "ye"), s(fit("john (SysOp)", 22), "ye b"), plain(fit("reading the Who's On list", doing_w)), s("     now", "fa")]);
        l.push(vec![]);
        if !callers.is_empty() {
            let c = &callers[self.sel_of().min(callers.len() - 1)];
            let bio =
                self.st.bios.get(&c.author.to_lowercase()).and_then(|id| crate::tui::state::Detail::load(&self.db, *id)).and_then(|d| d.messages.first().map(|m| m.body.clone()));
            let body: Vec<L> = match bio {
                Some(b) => b.trim().lines().take(6).map(|x| vec![plain(x.to_string())]).collect(),
                None => vec![vec![s(format!("{} hasn't posted a bio yet.", c.author), "mu")]],
            };
            l.extend(boxed(&format!("bio: {}", c.author), body, w));
        }
        l.extend([
            vec![],
            vec![s(" P", "ye"), s(" pages the highlighted caller: a mention they read on their next poll.", "mu")],
            vec![s(" (It will not beep their pager. They do not have pagers. We checked.)", "fa")],
        ]);
        l
    }

    pub fn option_items(&self) -> Vec<(String, String, &'static str)> {
        vec![
            (
                "Sessions at once".into(),
                format!("{}   (←/→, Shift ±10)  ·  recorded for the session host; nothing starts sessions yet", self.pref_i("max_sessions", 3)),
                "max_sessions",
            ),
            ("Concierge".into(), "off   needs the session host (not in this build yet)".into(), "concierge"),
            ("Theme".into(), format!("{}   ({} of {}, ←/→ to browse)", self.palette().label, self.theme_index() + 1, super::seg::palettes().len()), "theme"),
            ("Toasts".into(), format!("{}   desktop notification for each new question", if self.pref_b("toasts", true) { "ON" } else { "off" }), "toasts"),
            (
                "Updates".into(),
                match &self.update {
                    Some(v) => format!("v{v} available   ↵ downloads and installs it"),
                    None => format!("v{} is current   ↵ checks GitHub now", crate::update::CURRENT),
                },
                "update",
            ),
            ("Board".into(), crate::paths::db_path().to_string_lossy().into_owned(), "board"),
        ]
    }

    fn options_screen(&mut self, w: usize, h: usize) -> Vec<L> {
        let items = self.option_items();
        let it2 = items.clone();
        let row = move |i: usize| -> L { vec![s(format!("   {}", fit(&it2[i].0, 28)), "fg"), s(format!(" {}", it2[i].1), if it2[i].1.starts_with("ON") { "ye" } else { "mu" })] };
        let win = self.run_rows(w, h, bar(w, "OPTIONS  ·  the SysOp's control panel", "bar"), None, items.len(), &row, vec![], 20, &|_| None, vec![]);
        let mut l = win.lines;
        l.extend([
            vec![],
            vec![s(" Agent sessions: ", "cy b"), s("hosting `claude` sessions (identities, the Concierge, goals) is being ported to this build;", "mu")],
            vec![s(" until then the board, hooks, recurring jobs, PRs and permission rules are what is live.", "mu")],
            vec![],
            vec![s(" Version ", "fa"), s(crate::update::CURRENT, "ye"), s("   ·   updates arrive from GitHub Releases: ↵ on Updates, or `agentdesk update --apply`", "fa")],
            vec![s(" Settings live in ", "fa"), s(self.settings_path().to_string_lossy().into_owned(), "mu")],
        ]);
        l
    }

    fn agents_screen(&mut self, w: usize, h: usize) -> Vec<L> {
        let ags = self.st.agents.clone();
        let folder_w = w.saturating_sub(57).max(20);
        let a2 = ags.clone();
        let row = move |i: usize| -> L {
            let a = &a2[i];
            vec![
                s(format!("  {}", fit(&a.name, 20)), &format!("{} b", hue(&a.name))),
                s(
                    fit(&a.state, 9),
                    match a.state.as_str() {
                        "running" => "gr",
                        "queued" => "ye",
                        _ => "fa",
                    },
                ),
                s(fit(&a.generation.to_string(), 5), "mu"),
                s(fit(&a.host, 13), "cy"),
                s(fit(if a.model.is_empty() { "—" } else { &a.model }, 8), "mu"),
                s(fit(&a.folder, folder_w), "fa"),
            ]
        };
        let win = self.run_rows(
            w,
            h,
            bar(w, "AGENTS & GOALS  ·  the switchboard  ·  long-lived agents, and the swarms working toward a line", "barcy"),
            Some(format!("  {}{}{}{}{}FOLDER", fit("AGENT", 20), fit("STATE", 9), fit("GEN", 5), fit("HOST", 13), fit("MODEL", 8))),
            ags.len(),
            &row,
            vec![
                s(if self.st.has_session_tables { "   Nothing yet. " } else { "   This board has no agents tables yet. " }, "mu"),
                s("N", "ye"),
                s(" signs up an agent (recorded; it cannot start yet).", "mu"),
            ],
            10,
            &|_| None,
            vec![],
        );
        let mut l = win.lines;
        l.push(vec![]);
        l.push(vec![s(" Starting, attaching, goals, adopting and the Concierge need the session host, which is not in this build yet.", "fa")]);
        l.push(vec![s(" Agents you signed up in the .NET build are listed here read-only; nothing here starts or stops them.", "fa")]);
        l
    }

    fn compose_screen(&mut self, w: usize) -> Vec<L> {
        vec![
            bar(w, &format!("NEW POST  ·  {}", title_of(self.channel)), "barcy"),
            vec![],
            vec![s(" Type a subject, press ", "mu"), s("Enter", "ye"), s(", write the message, then ", "mu"), s("Ctrl+S", "ye"), s(" to post it.", "mu")],
            vec![s(" Markdown works. So do @mentions: an agent named after the @ sees it on its next poll.", "fa")],
        ]
    }

    fn ask_screen(&mut self, w: usize) -> Vec<L> {
        let Some(a) = &self.ask else { return vec![] };
        let mut l: Vec<L> = vec![bar(w, &a.banner, "barcy"), vec![]];
        for (i, (label, help, _)) in a.fields.iter().enumerate() {
            l.push(if i < self.answers.len() {
                vec![s(format!("   {}", fit(label, 10)), "mu"), if self.answers[i].is_empty() { s("(none)", "fa") } else { s(self.answers[i].clone(), "fg b") }]
            } else if i == self.answers.len() {
                vec![s(format!(" ▶ {}", fit(label, 10)), "ye b"), s(help.clone(), "fg")]
            } else {
                vec![s(format!("   {}", fit(label, 10)), "fa"), s(help.clone(), "fa")]
            });
        }
        l.push(vec![]);
        l.push(vec![s(" Type in the box below and press ", "mu"), s("Enter", "ye"), s(" for the next one. ", "mu"), s("Esc", "ye"), s(" cancels.", "mu")]);
        l
    }
}
