use super::md;
use super::{App, Compose, Mode, Tab, TABS};
use crate::db::RECEIPT_KINDS;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;
use serde_json::Value;

const ACCENT: Color = Color::Cyan;

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn ago(ts: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else { return ts.to_string() };
    let d = chrono::Utc::now().signed_duration_since(t);
    match d.num_seconds() {
        s if s < 0 => "soon".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(2), Constraint::Min(3), Constraint::Length(1)]).split(area);
    header(f, app, chunks[0]);
    match &mut app.mode {
        Mode::Reader { .. } => reader(f, app, chunks[1]),
        Mode::Compose { .. } => compose(f, app, chunks[1]),
        Mode::Search { .. } => search(f, app, chunks[1]),
        Mode::Permissions { .. } => permissions(f, app, chunks[1]),
        Mode::Help => help(f, chunks[1]),
        _ => {
            body(f, app, chunks[1]);
            if let Mode::Prompt { label, input, .. } = &app.mode {
                popup(f, chunks[1], label, &format!("{input}▏"));
            }
            if let Mode::Confirm { text, .. } = &app.mode {
                popup(f, chunks[1], "Confirm", text);
            }
        }
    }
    footer(f, app, chunks[2]);
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let mut title = vec![Span::styled(" AgentDesk ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD))];
    if app.waiting > 0 {
        title.push(Span::styled(format!("  ● {} waiting on you", app.waiting), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)));
    } else {
        title.push(Span::styled("  all quiet", dim()));
    }
    if let Some(v) = &app.update {
        title.push(Span::styled(format!("   ↑ v{v} available (U to update)"), Style::default().fg(Color::Yellow)));
    }
    if !app.usage.is_empty() {
        title.push(Span::styled(format!("   {}", app.usage), dim()));
    }
    let tabs: Vec<Span> = TABS
        .iter()
        .enumerate()
        .flat_map(|(i, t)| {
            let n = app.waiting_by_tab[i];
            let label = if n > 0 { format!(" {} {} ({n}) ", i + 1, t.title()) } else { format!(" {} {} ", i + 1, t.title()) };
            let st = if *t == app.tab {
                Style::default().fg(Color::Black).bg(Color::White)
            } else if n > 0 && *t == Tab::Questions {
                Style::default().fg(Color::Red)
            } else {
                dim()
            };
            [Span::styled(label, st), Span::raw(" ")]
        })
        .collect();
    f.render_widget(Paragraph::new(vec![Line::from(title), Line::from(tabs)]), area);
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    let text = if let Some((m, _)) = &app.flash {
        Span::styled(format!(" {m}"), Style::default().fg(if m.starts_with('!') { Color::Red } else { Color::Green }))
    } else {
        let keys = match &app.mode {
            Mode::List => match app.tab {
                Tab::Merge => "Enter open PR  t thread  c check now  Tab next  / search  X rules  ? help  q quit",
                Tab::Jobs => "n new  r run now  e on/off  x delete  Tab next  / search  X rules  ? help  q quit",
                Tab::Questions => "Enter read  r reply  n new  c close  u unarchive  a archived  / search  X rules  ? help  q quit",
                _ => "Enter read  r reply  n new  Tab next  / search  X rules  ? help  q quit",
            },
            Mode::Reader { .. } => "j/k scroll  Space page  r reply  c close  u unarchive  Esc back",
            Mode::Compose { .. } => "Ctrl+S send  Esc cancel  Ctrl+U clear line",
            Mode::Prompt { .. } => "Enter accept  Esc cancel",
            Mode::Confirm { .. } => "y yes  n no",
            Mode::Search { .. } => "type to search  ↑/↓ choose  Enter open  Esc back",
            Mode::Permissions { .. } => "a approve  x reject  v revert  j/k move  Esc back",
            Mode::Help => "any key to close",
        };
        Span::styled(format!(" {keys}"), dim())
    };
    f.render_widget(Paragraph::new(Line::from(text)), area);
}

fn popup(f: &mut Frame, area: Rect, title: &str, text: &str) {
    let w = area.width.saturating_sub(8).clamp(20, 90);
    let r = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + area.height / 3, width: w, height: 3 };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text.to_string()).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(ACCENT)).title(format!(" {title} "))), r);
}

fn body(f: &mut Frame, app: &mut App, area: Rect) {
    match app.tab {
        Tab::Merge => merge(f, app, area),
        Tab::Jobs => jobs(f, app, area),
        _ => threads(f, app, area),
    }
}

fn threads(f: &mut Frame, app: &mut App, area: Rect) {
    if app.threads.is_empty() {
        f.render_widget(Paragraph::new("  Nothing here yet.").style(dim()), area);
        return;
    }
    let w = area.width as usize;
    let subj_w = w.saturating_sub(4 + 6 + 20 + 6 + 5).max(10);
    let rows: Vec<Row> = app
        .threads
        .iter()
        .map(|t| {
            let waiting = t["waiting"].as_i64() == Some(1);
            let status = t["status"].as_str().unwrap_or("");
            let last = match t["follow_up"].as_str() {
                Some(fu) => fu.to_string(),
                None => t["last_author"].as_str().unwrap_or("").to_string(),
            };
            let delivery = t["delivery"].as_str().and_then(|d| d.split('|').next()).filter(|d| !d.is_empty()).map(|d| format!(" [{d}]")).unwrap_or_default();
            let mark = if waiting { "●" } else { " " };
            let style = if waiting {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else if status == "archived" || status == "closed" || status == "done" {
                dim()
            } else {
                Style::default()
            };
            Row::new(vec![
                format!("{mark}{}", t["id"]),
                cut(t["subject"].as_str().unwrap_or(""), subj_w),
                status.to_string(),
                cut(&format!("{last}{delivery}"), 20),
                format!("{}", t["message_count"]),
                ago(t["updated_ts"].as_str().unwrap_or("")),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(6), Constraint::Min(10), Constraint::Length(9), Constraint::Length(20), Constraint::Length(4), Constraint::Length(5)])
        .header(Row::new(vec!["  ID", "SUBJECT", "STATUS", "LAST WORD", "MSGS", "AGE"]).style(dim().add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1);
    let mut st = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(table, area, &mut st);
}

fn merge(f: &mut Frame, app: &mut App, area: Rect) {
    if app.prs.is_empty() {
        f.render_widget(Paragraph::new("  No pull requests waiting. Agents add them with request_merge.").style(dim()), area);
        return;
    }
    let rows: Vec<Row> = app
        .prs
        .iter()
        .map(|p| {
            let state = p["state"].as_str().unwrap_or("");
            let style = match state {
                "open" => Style::default().fg(Color::Yellow),
                "merged" => Style::default().fg(Color::Green),
                _ => dim(),
            };
            let note = p["last_error"].as_str().map(|e| format!("check failed: {e}")).unwrap_or_else(|| p["title"].as_str().unwrap_or("").to_string());
            Row::new(vec![
                format!("{}#{}", p["repo"].as_str().unwrap_or(""), p["number"]),
                state.to_string(),
                p["requested_by"].as_str().unwrap_or("").to_string(),
                ago(p["requested_ts"].as_str().unwrap_or("")),
                cut(&note, 80),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(34), Constraint::Length(8), Constraint::Length(20), Constraint::Length(5), Constraint::Min(10)])
        .header(Row::new(vec!["PULL REQUEST", "STATE", "ASKED BY", "AGE", "TITLE"]).style(dim().add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1);
    let mut st = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(table, area, &mut st);
}

fn jobs(f: &mut Frame, app: &mut App, area: Rect) {
    if app.jobs.is_empty() {
        f.render_widget(Paragraph::new("  No recurring jobs. Press n to make one.").style(dim()), area);
        return;
    }
    let rows: Vec<Row> = app
        .jobs
        .iter()
        .map(|j| {
            let on = j["enabled"].as_i64() == Some(1);
            let when = match (j["at"].as_str(), j["every_minutes"].as_f64()) {
                (Some(at), _) => format!("{at} {}", j["days"].as_str().unwrap_or("daily")),
                (None, Some(m)) => format!("every {m:.0} min"),
                _ => String::new(),
            };
            let next = if on { j["next_run_ts"].as_str().map(until).unwrap_or_default() } else { "off".into() };
            let running = crate::jobs::is_running(j["name"].as_str().unwrap_or(""));
            Row::new(vec![
                format!("{}{}", if running { "▶ " } else { "  " }, j["name"].as_str().unwrap_or("")),
                when,
                next,
                j["model"].as_str().unwrap_or("").to_string(),
                format!("{}", j["runs"]),
                cut(j["last_status"].as_str().unwrap_or(""), 60),
            ])
            .style(if on { Style::default() } else { dim() })
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(28), Constraint::Length(22), Constraint::Length(9), Constraint::Length(7), Constraint::Length(5), Constraint::Min(10)])
        .header(Row::new(vec!["JOB", "SCHEDULE", "NEXT", "MODEL", "RUNS", "LAST STATUS"]).style(dim().add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .column_spacing(1);
    let mut st = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(table, area, &mut st);
}

fn until(ts: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else { return ts.to_string() };
    let s = t.signed_duration_since(chrono::Utc::now()).num_seconds();
    match s {
        s if s <= 0 => "due".into(),
        s if s < 3600 => format!("{}m", s / 60 + 1),
        s if s < 86400 => format!("{}h{:02}", s / 3600, s % 3600 / 60),
        s => format!("{}d", s / 86400),
    }
}

fn message_kind(m: &Value) -> String {
    m["meta"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v["kind"].as_str().map(String::from)).unwrap_or_default()
}

fn reader(f: &mut Frame, app: &mut App, area: Rect) {
    let Mode::Reader { scroll, doc, id } = &mut app.mode else { return };
    let t = &doc["thread"];
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for m in doc["messages"].as_array().into_iter().flatten() {
        let kind = message_kind(m);
        let author = m["author"].as_str().unwrap_or("");
        if RECEIPT_KINDS.contains(&kind.as_str()) {
            lines.push(Line::styled(format!("  · {} — {}", m["body"].as_str().unwrap_or(""), ago(m["ts"].as_str().unwrap_or(""))), dim()));
            continue;
        }
        let color = if m["author_kind"].as_str() == Some("human") { Color::Green } else { ACCENT };
        lines.push(Line::from(vec![
            Span::styled(format!("▌ {author}"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}  #{}", m["ts"].as_str().unwrap_or(""), m["id"]), dim()),
        ]));
        lines.extend(md::render(m["body"].as_str().unwrap_or(""), width.saturating_sub(2), false).into_iter().map(|l| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(l.spans);
            Line::from(spans)
        }));
        lines.push(Line::raw(""));
    }
    let title = format!(" #{} {} · {} · {} ", id, cut(t["subject"].as_str().unwrap_or(""), 70), t["channel"].as_str().unwrap_or(""), t["status"].as_str().unwrap_or(""));
    let inner_h = area.height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(inner_h);
    *scroll = (*scroll).min(max);
    let shown = format!(" {}/{} ", (*scroll + inner_h).min(lines.len()), lines.len());
    f.render_widget(
        Paragraph::new(lines).scroll((*scroll as u16, 0)).block(
            Block::default().borders(Borders::ALL).border_style(Style::default().fg(ACCENT)).title(title).title_bottom(Line::styled(shown, dim()).alignment(Alignment::Right)),
        ),
        area,
    );
}

fn compose(f: &mut Frame, app: &mut App, area: Rect) {
    let Mode::Compose { what, editor, back } = &app.mode else { return };
    let (title, context): (String, Option<i64>) = match what {
        Compose::Reply(id) => (format!(" Reply to #{id} "), Some(*id)),
        Compose::Post(ch, subj) => (format!(" New {ch}: {subj} "), None),
        Compose::JobPrompt { name, schedule, .. } => (format!(" Job {name} ({schedule}): the prompt it runs "), None),
    };
    let _ = back;
    let parts = if context.is_some() {
        Layout::default().direction(Direction::Vertical).constraints([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area)
    } else {
        Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(3)]).split(area)
    };
    let (ctx_area, edit_area) = if context.is_some() { (Some(parts[0]), parts[1]) } else { (None, parts[0]) };
    if let (Some(id), Some(a)) = (context, ctx_area) {
        if let Ok(doc) = app.db.get_thread(id) {
            let width = a.width.saturating_sub(4) as usize;
            let mut lines = Vec::new();
            for m in doc["messages"].as_array().into_iter().flatten().rev().take(3).collect::<Vec<_>>().into_iter().rev() {
                if RECEIPT_KINDS.contains(&message_kind(m).as_str()) {
                    continue;
                }
                lines.push(Line::styled(format!("▌ {}", m["author"].as_str().unwrap_or("")), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
                lines.extend(md::render(m["body"].as_str().unwrap_or(""), width, false));
            }
            let skip = lines.len().saturating_sub(a.height.saturating_sub(2) as usize);
            f.render_widget(Paragraph::new(lines).scroll((skip as u16, 0)).block(Block::default().borders(Borders::ALL).border_style(dim()).title(" the thread ")), a);
        }
    }
    let text: Vec<Line> = editor.lines.iter().map(|l| Line::raw(l.clone())).collect();
    let inner_h = edit_area.height.saturating_sub(2) as usize;
    let skip = (editor.row + 1).saturating_sub(inner_h);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((skip as u16, 0))
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Green)).title(title)),
        edit_area,
    );
    let cx = edit_area.x + 1 + editor.lines[editor.row].chars().take(editor.col).map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1)).sum::<usize>() as u16;
    let cy = edit_area.y + 1 + (editor.row - skip) as u16;
    if cx < edit_area.x + edit_area.width.saturating_sub(1) && cy < edit_area.y + edit_area.height.saturating_sub(1) {
        f.set_cursor_position((cx, cy));
    }
}

fn search(f: &mut Frame, app: &mut App, area: Rect) {
    let Mode::Search { query, results, selected } = &app.mode else { return };
    let parts = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(3)]).split(area);
    f.render_widget(
        Paragraph::new(format!("{query}▏")).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(ACCENT)).title(" Search messages ")),
        parts[0],
    );
    let items: Vec<ListItem> = results
        .iter()
        .map(|r| {
            let body = r["body"].as_str().unwrap_or("").replace('\n', " ");
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("#{} ", r["thread_id"]), Style::default().fg(ACCENT)),
                    Span::styled(cut(r["subject"].as_str().unwrap_or(""), 60), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {} · {}", r["author"].as_str().unwrap_or(""), r["channel"].as_str().unwrap_or("")), dim()),
                ]),
                Line::styled(format!("   {}", cut(&body, parts[1].width.saturating_sub(6) as usize)), dim()),
            ])
        })
        .collect();
    let mut st = ListState::default().with_selected(if results.is_empty() { None } else { Some(*selected) });
    f.render_stateful_widget(List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED)), parts[1], &mut st);
}

fn permissions(f: &mut Frame, app: &mut App, area: Rect) {
    let Mode::Permissions { rows, selected, settings } = &app.mode else { return };
    let parts = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(3)]).split(area);
    f.render_widget(Paragraph::new(format!(" Permission rules agents proposed. Approving writes to {settings}")).style(dim()), parts[0]);
    if rows.is_empty() {
        f.render_widget(Paragraph::new("  No proposals yet.").style(dim()), parts[1]);
        return;
    }
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let st = r["status"].as_str().unwrap_or("");
            let color = match st {
                "pending" => Color::Yellow,
                "approved" => Color::Green,
                "rejected" => Color::Red,
                _ => Color::DarkGray,
            };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("[{st}] "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(r["rule"].as_str().unwrap_or("").to_string(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(if r["broad"].as_bool() == Some(true) { "  BROAD" } else { "" }, Style::default().fg(Color::Red)),
                    Span::styled(format!("  ({}, by {})", r["scope"].as_str().unwrap_or(""), r["requested_by"].as_str().unwrap_or("")), dim()),
                ]),
                Line::styled(format!("    {}", cut(r["reason"].as_str().unwrap_or(""), parts[1].width.saturating_sub(8) as usize)), dim()),
            ])
        })
        .collect();
    let mut st = ListState::default().with_selected(Some(*selected));
    f.render_stateful_widget(List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED)), parts[1], &mut st);
}

fn help(f: &mut Frame, area: Rect) {
    let text = "\
AgentDesk: a message board between you and the agents on this machine.

  Tabs        Tab / Shift+Tab or 1-6      Questions, Discussion, Wiki, Work, Merge list, Jobs
  Move        j/k or arrows, g/G, PgUp/PgDn
  Read        Enter                       thread reader (j/k scroll, r reply, c close, Esc back)
  Reply       r                           Ctrl+S sends, Esc cancels. Your reply settles an open question.
  New post    n                           subject, then body (posts as john)
  Close       c                           close a question without answering
  Archived    a toggles, u brings one back to the Questions tab
  Search      /                           full-text and substring over every message
  Rules       X                           approve, reject or revert permission rules agents proposed
  Refresh     R
  Quit        q or Ctrl+C

Questions ring a desktop toast. Agents reach the board through `agentdesk` (the MCP server) and learn of your
answers through the hooks and `agentdesk wait`. Run `agentdesk setup` for the Claude Code configuration.";
    f.render_widget(Paragraph::new(text).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(ACCENT)).title(" Help ")).wrap(Wrap { trim: false }), area);
}
