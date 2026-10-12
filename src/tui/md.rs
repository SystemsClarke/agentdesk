//! A light Markdown renderer for message bodies: headings, bold/italic/code/links, bullets, task lists, quotes, callouts,
//! rules, fenced code and pipe tables as aligned grids. Returns wrapped lines of tagged segments.

use super::seg::{rep, s, L};
use unicode_width::UnicodeWidthStr;

fn with(base: &str, extra: &str) -> String {
    format!("{base} {extra}").trim().to_string()
}

fn inline(text: &str, base: &str) -> L {
    let chars: Vec<char> = text.chars().collect();
    let mut out: L = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, out: &mut L| {
        if !buf.is_empty() {
            out.push(s(std::mem::take(buf), base));
        }
    };
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let mut handled = false;
        for (m, tag) in [("**", "b"), ("~~", "s")] {
            if rest.starts_with(m) {
                if let Some(end) = rest[2..].find(m) {
                    flush(&mut buf, &mut out);
                    out.push(s(rest[2..2 + end].to_string(), &with(base, tag)));
                    i += rest[..2 + end + 2].chars().count();
                    handled = true;
                    break;
                }
            }
        }
        if handled {
            continue;
        }
        if chars[i] == '`' {
            if let Some(end) = rest[1..].find('`') {
                flush(&mut buf, &mut out);
                out.push(s(rest[1..1 + end].to_string(), "cy"));
                i += rest[..1 + end + 1].chars().count();
                continue;
            }
        }
        if chars[i] == '*' && chars.get(i + 1).map(|c| !c.is_whitespace() && *c != '*').unwrap_or(false) {
            if let Some(end) = rest[1..].find('*') {
                flush(&mut buf, &mut out);
                out.push(s(rest[1..1 + end].to_string(), &with(base, "i")));
                i += rest[..1 + end + 1].chars().count();
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some(close) = rest.find("](") {
                if let Some(paren) = rest[close..].find(')') {
                    flush(&mut buf, &mut out);
                    out.push(s(rest[1..close].to_string(), "cy u"));
                    i += rest[..close + paren + 1].chars().count();
                    continue;
                }
            }
        }
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let url: String = rest.chars().take_while(|c| !c.is_whitespace() && !matches!(c, ')' | '>' | '"')).collect();
            let url = url.trim_end_matches(['.', ',', ';']).to_string();
            flush(&mut buf, &mut out);
            i += url.chars().count();
            out.push(s(url, "cy u"));
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut out);
    out
}

/// Wraps one line of segments to `w` columns at spaces (a word longer than the line is cut), continuing under `indent`.
pub fn wrap(segs: L, w: usize, indent: &str) -> Vec<L> {
    let w = w.max(8);
    let mut lines: Vec<L> = vec![vec![]];
    let mut col = 0usize;
    for sg in segs {
        let mut word = String::new();
        let mut words: Vec<String> = Vec::new();
        for ch in sg.text.chars() {
            word.push(ch);
            if ch == ' ' {
                words.push(std::mem::take(&mut word));
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
        for wd in words {
            let mut wd = wd;
            let ww = wd.width();
            if col + ww > w && col > indent.width() {
                lines.push(vec![s(indent, "")]);
                col = indent.width();
                wd = wd.trim_start().to_string();
            }
            while wd.width() > w.saturating_sub(col).max(1) && wd.width() > w {
                let mut head = String::new();
                for ch in wd.chars() {
                    if head.width() + ch.to_string().width() > w.saturating_sub(col) {
                        break;
                    }
                    head.push(ch);
                }
                if head.is_empty() {
                    break;
                }
                let rest: String = wd.chars().skip(head.chars().count()).collect();
                lines.last_mut().unwrap().push(s(head, &sg.tags));
                lines.push(vec![s(indent, "")]);
                col = indent.width();
                wd = rest;
            }
            col += wd.width();
            lines.last_mut().unwrap().push(s(wd, &sg.tags));
        }
    }
    lines
}

fn table(rows: &[Vec<String>], width: usize) -> Vec<L> {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut w = vec![1usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.width());
        }
    }
    let total: usize = w.iter().sum::<usize>() + 3 * cols.saturating_sub(1);
    if total > width && cols > 0 {
        let cap = (width.saturating_sub(3 * (cols - 1)) / cols).max(4);
        for x in w.iter_mut() {
            *x = (*x).min(cap);
        }
    }
    let mut out = Vec::new();
    for (ri, r) in rows.iter().enumerate() {
        let mut line: L = Vec::new();
        for (i, wi) in w.iter().enumerate() {
            let mut c = r.get(i).cloned().unwrap_or_default();
            while c.width() > *wi {
                c.pop();
            }
            let padn = wi.saturating_sub(c.width());
            line.push(s(format!("{c}{}", rep(' ', padn)), if ri == 0 { "b" } else { "" }));
            if i + 1 < w.len() {
                line.push(s(" │ ", "rule"));
            }
        }
        out.push(line);
        if ri == 0 {
            out.push(vec![s(w.iter().map(|x| rep('─', *x)).collect::<Vec<_>>().join("─┼─"), "rule")]);
        }
    }
    out
}

pub fn render(body: &str, width: usize) -> Vec<L> {
    let mut out: Vec<L> = Vec::new();
    let mut in_code = false;
    let mut pending: Vec<Vec<String>> = Vec::new();
    for raw in body.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if t.starts_with("```") {
            if !pending.is_empty() {
                out.extend(table(&std::mem::take(&mut pending), width));
            }
            in_code = !in_code;
            let lang = t.trim_start_matches('`');
            if in_code && !lang.is_empty() {
                out.push(vec![s(format!("┌ {lang}"), "fa")]);
            }
            continue;
        }
        if in_code {
            out.push(vec![s("│ ", "fa"), s(line.to_string(), "gr")]);
            continue;
        }
        if t.starts_with('|') && t.ends_with('|') && t.len() > 1 {
            let cells: Vec<String> = t.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect();
            if cells.iter().all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' '))) {
                continue;
            }
            pending.push(cells);
            continue;
        }
        if !pending.is_empty() {
            out.extend(table(&std::mem::take(&mut pending), width));
        }
        if t.is_empty() {
            out.push(vec![]);
        } else if let Some(h) = t.strip_prefix("### ").or_else(|| t.strip_prefix("## ")).or_else(|| t.strip_prefix("# ")) {
            out.extend(wrap(inline(h, "ye b"), width, ""));
        } else if t == "---" || t == "***" {
            out.push(vec![s(rep('─', width.min(60)), "rule")]);
        } else if let Some(q) = t.strip_prefix('>') {
            let q = q.trim_start();
            let (label, tag) = match q {
                x if x.starts_with("[!NOTE]") => ("Note", "cy"),
                x if x.starts_with("[!TIP]") => ("Tip", "gr"),
                x if x.starts_with("[!IMPORTANT]") => ("Important", "pu"),
                x if x.starts_with("[!WARNING]") => ("Warning", "or"),
                x if x.starts_with("[!CAUTION]") => ("Caution", "pk"),
                _ => ("", "mu"),
            };
            if !label.is_empty() {
                out.push(vec![s(format!("▌ {label}"), &format!("{tag} b"))]);
            } else {
                let mut sp = vec![s("▌ ", "rule")];
                sp.extend(inline(q, "mu i"));
                out.extend(wrap(sp, width, "▌ "));
            }
        } else if let Some((mark, item)) = t
            .strip_prefix("- [ ] ")
            .map(|i| ("☐ ", i))
            .or_else(|| t.strip_prefix("- [x] ").or_else(|| t.strip_prefix("- [X] ")).map(|i| ("☑ ", i)))
            .or_else(|| t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).map(|i| ("• ", i)))
        {
            let lead = line.len() - t.len();
            let mut sp = vec![s(format!("{}{mark}", rep(' ', lead)), "mu")];
            sp.extend(inline(item, ""));
            out.extend(wrap(sp, width, &rep(' ', lead + 2)));
        } else {
            out.extend(wrap(inline(line, ""), width, ""));
        }
    }
    if !pending.is_empty() {
        out.extend(table(&pending, width));
    }
    out
}
