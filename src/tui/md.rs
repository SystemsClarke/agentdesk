//! A light Markdown renderer for message bodies: headings, bold/italic/code, bullets, quotes, rules, fenced
//! code, pipe tables as aligned grids, and callouts. Returns wrapped, styled lines.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let (mut i, mut buf) = (0, String::new());
    let flush = |buf: &mut String, out: &mut Vec<Span<'static>>, st: Style| {
        if !buf.is_empty() {
            out.push(Span::styled(std::mem::take(buf), st));
        }
    };
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        if rest.starts_with("**") || rest.starts_with("~~") {
            let m = &rest[..2];
            if let Some(end) = rest[2..].find(m) {
                flush(&mut buf, &mut out, base);
                let st = if m == "**" { base.add_modifier(Modifier::BOLD) } else { base.add_modifier(Modifier::CROSSED_OUT) };
                out.push(Span::styled(rest[2..2 + end].to_string(), st));
                i += rest[..2 + end + 2].chars().count();
                continue;
            }
        }
        if chars[i] == '`' {
            if let Some(end) = rest[1..].find('`') {
                flush(&mut buf, &mut out, base);
                out.push(Span::styled(rest[1..1 + end].to_string(), Style::default().fg(Color::Yellow)));
                i += rest[..1 + end + 1].chars().count();
                continue;
            }
        }
        if chars[i] == '*' && chars.get(i + 1).map(|c| !c.is_whitespace() && *c != '*').unwrap_or(false) {
            if let Some(end) = rest[1..].find('*') {
                flush(&mut buf, &mut out, base);
                out.push(Span::styled(rest[1..1 + end].to_string(), base.add_modifier(Modifier::ITALIC)));
                i += rest[..1 + end + 1].chars().count();
                continue;
            }
        }
        if chars[i] == '[' {
            if let (Some(close), true) = (rest.find("]("), true) {
                if let Some(paren) = rest[close..].find(')') {
                    flush(&mut buf, &mut out, base);
                    out.push(Span::styled(rest[1..close].to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)));
                    i += rest[..close + paren + 1].chars().count();
                    continue;
                }
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut out, base);
    out
}

/// Wraps one styled line to `width` columns, breaking at spaces where it can.
pub fn wrap(spans: Vec<Span<'static>>, width: usize, indent: &str) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut lines: Vec<Vec<Span<'static>>> = vec![vec![]];
    let mut col = 0usize;
    for sp in spans {
        let st = sp.style;
        let mut word = String::new();
        let push_word = |word: &mut String, lines: &mut Vec<Vec<Span<'static>>>, col: &mut usize| {
            if word.is_empty() {
                return;
            }
            let w = word.width();
            if *col + w > width && *col > 0 {
                lines.push(vec![Span::raw(indent.to_string())]);
                *col = indent.width();
                *word = word.trim_start().to_string();
            }
            let mut w = word.width();
            // a word longer than the line is cut
            while w > width.saturating_sub(*col).max(1) && *col == indent.width().min(*col) && w > width {
                let mut head = String::new();
                let mut hw = 0;
                for ch in word.chars() {
                    let cw = ch.to_string().width();
                    if hw + cw > width - *col {
                        break;
                    }
                    head.push(ch);
                    hw += cw;
                }
                let rest: String = word.chars().skip(head.chars().count()).collect();
                lines.last_mut().unwrap().push(Span::styled(head, st));
                lines.push(vec![Span::raw(indent.to_string())]);
                *col = indent.width();
                *word = rest;
                w = word.width();
            }
            *col += w;
            lines.last_mut().unwrap().push(Span::styled(std::mem::take(word), st));
        };
        for ch in sp.content.chars() {
            word.push(ch);
            if ch == ' ' {
                push_word(&mut word, &mut lines, &mut col);
            }
        }
        push_word(&mut word, &mut lines, &mut col);
    }
    lines.into_iter().map(Line::from).collect()
}

fn table(rows: &[Vec<String>], width: usize) -> Vec<Line<'static>> {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut w = vec![1usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.width());
        }
    }
    let total: usize = w.iter().sum::<usize>() + 3 * cols.saturating_sub(1);
    if total > width && cols > 0 {
        let cap = ((width.saturating_sub(3 * (cols - 1))) / cols).max(4);
        for x in w.iter_mut() {
            *x = (*x).min(cap);
        }
    }
    let mut out = Vec::new();
    for (ri, r) in rows.iter().enumerate() {
        let mut spans = Vec::new();
        for (i, wi) in w.iter().enumerate() {
            let mut c = r.get(i).cloned().unwrap_or_default();
            while c.width() > *wi {
                c.pop();
            }
            let pad = wi.saturating_sub(c.width());
            let st = if ri == 0 { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
            spans.push(Span::styled(format!("{c}{}", " ".repeat(pad)), st));
            if i + 1 < w.len() {
                spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            }
        }
        out.push(Line::from(spans));
        if ri == 0 {
            out.push(Line::styled(w.iter().map(|x| "─".repeat(*x)).collect::<Vec<_>>().join("─┼─"), Style::default().fg(Color::DarkGray)));
        }
    }
    out
}

pub fn render(body: &str, width: usize, dim: bool) -> Vec<Line<'static>> {
    let base = if dim { Style::default().fg(Color::DarkGray) } else { Style::default() };
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let mut pending_table: Vec<Vec<String>> = Vec::new();
    let flush_table = |pt: &mut Vec<Vec<String>>, out: &mut Vec<Line<'static>>| {
        if !pt.is_empty() {
            out.extend(table(pt, width));
            pt.clear();
        }
    };
    for raw in body.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            flush_table(&mut pending_table, &mut out);
            in_code = !in_code;
            let lang = line.trim_start().trim_start_matches('`');
            if in_code && !lang.is_empty() {
                out.push(Line::styled(format!("┌ {lang}"), Style::default().fg(Color::DarkGray)));
            }
            continue;
        }
        if in_code {
            out.push(Line::styled(format!("│ {line}"), Style::default().fg(Color::Green)));
            continue;
        }
        let t = line.trim_start();
        if t.starts_with('|') && t.ends_with('|') && t.len() > 1 {
            let cells: Vec<String> = t.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect();
            if cells.iter().all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' '))) {
                continue; // delimiter row
            }
            pending_table.push(cells);
            continue;
        }
        flush_table(&mut pending_table, &mut out);
        if t.is_empty() {
            out.push(Line::raw(""));
        } else if let Some(h) = t.strip_prefix("### ").or_else(|| t.strip_prefix("## ")).or_else(|| t.strip_prefix("# ")) {
            out.extend(wrap(inline(h, base.fg(Color::Cyan).add_modifier(Modifier::BOLD)), width, ""));
        } else if t == "---" || t == "***" {
            out.push(Line::styled("─".repeat(width.min(60)), Style::default().fg(Color::DarkGray)));
        } else if let Some(q) = t.strip_prefix('>') {
            let q = q.trim_start();
            let (label, color) = match q {
                s if s.starts_with("[!NOTE]") => ("Note", Color::Blue),
                s if s.starts_with("[!TIP]") => ("Tip", Color::Green),
                s if s.starts_with("[!IMPORTANT]") => ("Important", Color::Magenta),
                s if s.starts_with("[!WARNING]") => ("Warning", Color::Yellow),
                s if s.starts_with("[!CAUTION]") => ("Caution", Color::Red),
                _ => ("", Color::DarkGray),
            };
            if !label.is_empty() {
                out.push(Line::styled(format!("▌ {label}"), Style::default().fg(color).add_modifier(Modifier::BOLD)));
            } else {
                let mut spans = vec![Span::styled("▌ ", Style::default().fg(color))];
                spans.extend(inline(q, base.add_modifier(Modifier::ITALIC)));
                out.extend(wrap(spans, width, "▌ "));
            }
        } else if let Some(item) = t.strip_prefix("- [ ] ").or_else(|| t.strip_prefix("* [ ] ")) {
            let lead = line.len() - t.len();
            let mut spans = vec![Span::raw(format!("{}☐ ", " ".repeat(lead)))];
            spans.extend(inline(item, base));
            out.extend(wrap(spans, width, &" ".repeat(lead + 2)));
        } else if let Some(item) = t.strip_prefix("- [x] ").or_else(|| t.strip_prefix("- [X] ")) {
            let lead = line.len() - t.len();
            let mut spans = vec![Span::raw(format!("{}☑ ", " ".repeat(lead)))];
            spans.extend(inline(item, base));
            out.extend(wrap(spans, width, &" ".repeat(lead + 2)));
        } else if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            let lead = line.len() - t.len();
            let mut spans = vec![Span::raw(format!("{}• ", " ".repeat(lead)))];
            spans.extend(inline(item, base));
            out.extend(wrap(spans, width, &" ".repeat(lead + 2)));
        } else {
            out.extend(wrap(inline(line, base), width, ""));
        }
    }
    flush_table(&mut pending_table, &mut out);
    out
}
