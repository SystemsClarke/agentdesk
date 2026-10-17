//! Markdown <-> Slack. Deterministic, no model involved. A port of `agentdesk/slackfmt.py`.
//!
//! `md_to_slack` turns a board message (Markdown) into Slack Block Kit: headers, dividers and mrkdwn sections, with tables as aligned
//! monospace grids. `slack_to_md` turns a reply typed in Slack back into Markdown, which is what every agent on the board reads and
//! writes. ```mermaid is drawn by the terminal's engine (`tui::charts`) as ASCII art in a code block, and sent as its source when the art
//! is too wide for a phone (the Python half then lists the edges as steps). A ```chart is drawn the same way, as text: the Python half
//! rasterises it to a PNG and uploads it into the thread, which needs an image renderer this build does not have. Both are on the board
//! in full.

use fancy_regex::Regex as Fancy;
use regex::{Captures, Regex};
use serde_json::{json, Value};
use std::sync::OnceLock;

const SECTION_MAX: usize = 2900; // Slack's hard limit is 3000 characters per section
const HEADER_MAX: usize = 150;
pub const BLOCKS_MAX: usize = 50;
/// Slack wraps code blocks at the screen's width, and a phone fits about this many columns.
const PHONE_COLS: usize = 44;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

macro_rules! rx {
    ($pat:expr) => {{
        static C: OnceLock<Regex> = OnceLock::new();
        re(&C, $pat)
    }};
}

macro_rules! fx {
    ($pat:expr) => {{
        static C: OnceLock<Fancy> = OnceLock::new();
        C.get_or_init(|| Fancy::new($pat).unwrap())
    }};
}

/// iOS Slack's code font draws box-drawing horizontals as blanks, so code blocks use ASCII.
fn ascii(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '─' | '┄' => '-',
            '│' => '|',
            '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' | '╭' | '╮' | '╰' | '╯' | '◆' | '╔' | '╗' | '╚' | '╝' => '+',
            '►' => '>',
            '◄' => '<',
            '▼' => 'v',
            '▲' => '^',
            '┆' => ':',
            '═' => '=',
            '║' => '|',
            '█' => '#',
            '░' => '.',
            '↺' => '@',
            '▌' => '|',
            c => c,
        })
        .collect()
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn inline_re() -> &'static Regex {
    // Code first so a backtick span is never further marked up, bold before italic so **x** is never two *x*, images before links,
    // explicit links before bare URLs.
    rx!(concat!(
        r"`(?P<code>[^`\n]+)`",
        r"|\*\*(?P<bold>[^*\n]+?)\*\*",
        r"|__(?P<bold2>[^_\n]+?)__",
        r"|~~(?P<strike>[^~\n]+?)~~",
        r"|\*(?P<ital>[^*\n]+)\*",
        r"|!\[(?P<ialt>[^\]\n]*)\]\((?P<iurl>[^)\s]+)\)",
        r"|\[(?P<ltxt>[^\]\n]+)\]\((?P<lurl>[^)\s]+)\)",
        r"|<(?P<aurl>https?://[^>\s]+)>",
        r"|(?P<burl>https?://[^\s)>\]]+)"
    ))
}

/// A line's inline Markdown as Slack mrkdwn.
pub fn inline(text: &str) -> String {
    let (mut out, mut pos) = (String::new(), 0);
    for m in inline_re().captures_iter(text) {
        let whole = m.get(0).unwrap();
        out.push_str(&esc(&text[pos..whole.start()]));
        let g = |n: &str| m.name(n).map(|x| x.as_str());
        if let Some(c) = g("code") {
            out.push_str(&format!("`{c}`"));
        } else if let Some(b) = g("bold").or(g("bold2")) {
            out.push_str(&format!("*{}*", esc(b)));
        } else if let Some(s) = g("strike") {
            out.push_str(&format!("~{}~", esc(s)));
        } else if let Some(i) = g("ital") {
            out.push_str(&format!("_{}_", esc(i)));
        } else if let Some(alt) = g("ialt") {
            let alt = if alt.is_empty() { "image" } else { alt };
            out.push_str(&format!("<{}|▣ {}>", g("iurl").unwrap_or(""), esc(alt)));
        } else if let Some(t) = g("ltxt") {
            out.push_str(&format!("<{}|{}>", g("lurl").unwrap_or(""), esc(t)));
        } else {
            out.push_str(&format!("<{}>", g("aurl").or(g("burl")).unwrap_or("")));
        }
        pos = whole.end();
    }
    out.push_str(&esc(&text[pos..]));
    out
}

/// A cell's text as it will appear: markup markers and urls removed.
pub fn plain(text: &str) -> String {
    let (mut out, mut pos) = (String::new(), 0);
    for m in inline_re().captures_iter(text) {
        let whole = m.get(0).unwrap();
        out.push_str(&text[pos..whole.start()]);
        if let Some(alt) = m.name("ialt") {
            out.push_str(&format!("▣ {}", if alt.as_str().is_empty() { "image" } else { alt.as_str() }));
        } else {
            for g in ["code", "bold", "bold2", "strike", "ital", "ltxt", "aurl", "burl"] {
                if let Some(x) = m.name(g) {
                    out.push_str(x.as_str());
                    break;
                }
            }
        }
        pos = whole.end();
    }
    out.push_str(&text[pos..]);
    out
}

// ---- Markdown line structure

fn split_row(line: &str) -> Vec<String> {
    let mut s = line.trim().replace("\\|", "\0");
    if let Some(r) = s.strip_prefix('|') {
        s = r.to_string();
    }
    if let Some(r) = s.strip_suffix('|') {
        s = r.to_string();
    }
    s.split('|').map(|c| c.trim().replace('\0', "|")).collect()
}

fn is_delim(line: &str) -> bool {
    if !rx!(r"^[|\-:\s]+$").is_match(line) || !line.contains('-') {
        return false;
    }
    split_row(line).iter().all(|c| rx!(r"^\s*(:?)-+(:?)\s*$").is_match(c.trim()))
}

fn heading(line: &str) -> Option<Captures<'_>> {
    rx!(r"^(#{1,6})\s+(.+?)\s*#*\s*$").captures(line)
}

fn is_rule(line: &str) -> bool {
    rx!(r"^\s*(?:-{3,}|\*{3,}|_{3,})\s*$").is_match(line)
}

type Table = (Vec<String>, Vec<String>, Vec<Vec<String>>, usize);

fn table_at(lines: &[&str], i: usize) -> Option<Table> {
    if i + 1 >= lines.len() || !lines[i].contains('|') || !is_delim(lines[i + 1]) {
        return None;
    }
    let (header, delim) = (split_row(lines[i]), split_row(lines[i + 1]));
    if !lines[i + 1].contains('|') && delim.len() != header.len() {
        return None;
    }
    let mut rows = vec![];
    let mut j = i + 2;
    while j < lines.len() {
        let (line, stripped) = (lines[j], lines[j].trim());
        if stripped.is_empty() || !line.contains('|') || stripped.starts_with("```") || is_delim(line) || heading(line).is_some() || is_rule(line) {
            break;
        }
        rows.push(split_row(line));
        j += 1;
    }
    Some((header, delim, rows, j))
}

fn aligns(delim: &[String], ncols: usize) -> Vec<char> {
    (0..ncols)
        .map(|j| match delim.get(j).and_then(|d| rx!(r"^\s*(:?)-+(:?)\s*$").captures(d)) {
            Some(m) if &m[1] == ":" && &m[2] == ":" => 'c',
            Some(m) if &m[2] == ":" => 'r',
            _ => 'l',
        })
        .collect()
}

fn width(s: &str) -> usize {
    s.chars().count()
}

fn grid(header: &[String], delim: &[String], rows: &[Vec<String>]) -> String {
    let ncols = rows.iter().map(Vec::len).chain([header.len(), delim.len()]).max().unwrap_or(1);
    let cells = |r: &[String]| (0..ncols).map(|j| r.get(j).map(|c| plain(c)).unwrap_or_default()).collect::<Vec<_>>();
    let head = cells(header);
    let body: Vec<Vec<String>> = rows.iter().map(|r| cells(r)).collect();
    let widths: Vec<usize> = (0..ncols).map(|j| std::iter::once(&head).chain(body.iter()).map(|c| width(&c[j])).max().unwrap_or(1).max(1)).collect();
    let al = aligns(delim, ncols);
    let row = |cs: &[String]| {
        let parts: Vec<String> = cs
            .iter()
            .enumerate()
            .map(|(j, c)| {
                let gap = widths[j] - width(c);
                let left = match al[j] {
                    'r' => gap,
                    'c' => gap / 2,
                    _ => 0,
                };
                format!("{}{c}{}", " ".repeat(left), " ".repeat(gap - left))
            })
            .collect();
        format!("│ {} │", parts.join(" │ "))
    };
    if widths.iter().sum::<usize>() + 3 * ncols + 1 > PHONE_COLS {
        // Too wide for a phone, where a code block wraps and the grid falls apart: one line per row instead, first column as the row's name.
        return body
            .iter()
            .map(|r| {
                let rest = (1..ncols).filter(|j| !r[*j].is_empty()).map(|j| format!("{} {}", head[j], r[j]).trim().to_string()).collect::<Vec<_>>().join(" · ");
                format!("• *{}*{}", esc(&r[0]), if rest.is_empty() { String::new() } else { format!(": {}", esc(&rest)) })
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let rule = |l: &str, m: &str, r: &str| format!("{l}{}{r}", widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join(m));
    let mut lines = vec![rule("┌", "┬", "┐"), row(&head), rule("├", "┼", "┤")];
    lines.extend(body.iter().map(|r| row(r)));
    lines.push(rule("└", "┴", "┘"));
    format!("```\n{}\n```", ascii(&lines.join("\n")))
}

/// The plain text of what the terminal's engine drew, in ASCII (a phone's code font has no box-drawing characters).
fn art_lines(art: &[Vec<crate::tui::seg::Seg>]) -> Vec<String> {
    art.iter().map(|l| ascii(&l.iter().map(|s| s.text.as_str()).collect::<String>())).collect()
}

fn fence(lines: &[String]) -> String {
    format!("```\n{}\n```", lines.join("\n"))
}

/// A ```chart block: its drawing, or the reason it could not be drawn.
fn chart_text(block: &[&str]) -> String {
    let lines = art_lines(&crate::tui::charts::chart(&block.join("\n"), PHONE_COLS));
    match lines.first().and_then(|l| l.strip_prefix("chart error: ")) {
        Some(why) => format!("_chart could not be drawn: {}_", esc(why)),
        None => fence(&lines),
    }
}

/// A ```mermaid block: art that fits a phone, else its source.
fn mermaid_text(block: &[&str]) -> String {
    let lines = art_lines(&crate::tui::charts::mermaid(&block.join("\n")));
    let drew = lines.first().is_some_and(|l| !l.starts_with("diagram error"));
    if drew && lines.iter().all(|l| width(l) <= PHONE_COLS) {
        fence(&lines)
    } else {
        fence(&block.iter().map(|l| l.to_string()).collect::<Vec<_>>())
    }
}

enum Piece {
    Text(String),
    Header(String),
    Divider,
}

fn pieces(md: &str) -> Vec<Piece> {
    let lines: Vec<&str> = md.split('\n').collect();
    let (mut out, mut i) = (vec![], 0);
    while i < lines.len() {
        let (line, stripped) = (lines[i], lines[i].trim());
        if let Some(lang) = stripped.strip_prefix("```") {
            let lang = lang.trim().to_lowercase();
            let mut block = vec![];
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                block.push(lines[i]);
                i += 1;
            }
            i += 1;
            out.push(Piece::Text(match lang.as_str() {
                "chart" => chart_text(&block),
                "mermaid" => mermaid_text(&block),
                _ => format!("```\n{}\n```", block.join("\n")),
            }));
            continue;
        }
        if let Some((header, delim, rows, next)) = table_at(&lines, i) {
            out.push(Piece::Text(grid(&header, &delim, &rows)));
            i = next;
            continue;
        }
        if is_rule(line) {
            out.push(Piece::Divider);
            i += 1;
            continue;
        }
        if let Some(m) = heading(line) {
            if m[1].len() == 1 {
                out.push(Piece::Header(plain(&m[2]).chars().take(HEADER_MAX).collect()));
            } else {
                out.push(Piece::Text(format!("*{}*", inline(&m[2]))));
            }
            i += 1;
            continue;
        }
        let quote = rx!(r"^\s*>\s?(.*)$");
        if quote.is_match(line) {
            let mut q = vec![];
            while i < lines.len() && quote.is_match(lines[i]) {
                q.push(quote.captures(lines[i]).unwrap()[1].to_string());
                i += 1;
            }
            let adm = rx!(r"(?i)^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s*(.*)$").captures(q[0].trim()).map(|m| (m[1].to_uppercase(), m[2].to_string()));
            let q = match adm {
                Some((kind, rest)) => {
                    let title = match kind.as_str() {
                        "NOTE" => "ⓘ NOTE",
                        "TIP" => "✓ TIP",
                        "IMPORTANT" => "★ IMPORTANT",
                        "WARNING" => "⚠ WARNING",
                        _ => "✖ CAUTION",
                    };
                    let mut v = vec![format!("*{title}*")];
                    if !rest.is_empty() {
                        v.push(rest);
                    }
                    v.extend(q[1..].iter().cloned());
                    v
                }
                None => q,
            };
            out.push(Piece::Text(
                q.iter().map(|l| format!("> {}", if l.starts_with('*') && l.ends_with('*') && l.len() > 1 { l.clone() } else { inline(l) })).collect::<Vec<_>>().join("\n"),
            ));
            continue;
        }
        if let Some(m) = rx!(r"^(\s*)[-*+]\s+(.*)$").captures(line) {
            let level = (m[1].replace('\t', "    ").len() / 2).min(3);
            let pad = "    ".repeat(level);
            if let Some(t) = rx!(r"^\[( |x|X)\]\s+(.*)$").captures(&m[2]) {
                out.push(Piece::Text(format!("{pad}{} {}", if t[1].eq_ignore_ascii_case("x") { "☑" } else { "☐" }, inline(&t[2]))));
            } else {
                out.push(Piece::Text(format!("{pad}{} {}", ["•", "◦", "▪", "·"][level], inline(&m[2]))));
            }
            i += 1;
            continue;
        }
        if let Some(m) = rx!(r"^(\s*)(\d+)[.)]\s+(.*)$").captures(line) {
            let level = (m[1].replace('\t', "    ").len() / 2).min(3);
            out.push(Piece::Text(format!("{}{}. {}", "    ".repeat(level), &m[2], inline(&m[3]))));
            i += 1;
            continue;
        }
        out.push(Piece::Text(inline(line)));
        i += 1;
    }
    out
}

/// Splits text into section-sized pieces, on line boundaries, never inside a ``` block.
fn chunks(s: &str) -> Vec<String> {
    if s.chars().count() <= SECTION_MAX {
        return vec![s.to_string()];
    }
    let (mut out, mut cur, mut fenced) = (vec![], String::new(), false);
    for line in s.split('\n') {
        let extra = format!("{}{line}", if cur.is_empty() { "" } else { "\n" });
        if width(&cur) + width(&extra) > SECTION_MAX && !cur.is_empty() {
            out.push(format!("{cur}{}", if fenced { "\n```" } else { "" }));
            cur = format!("{}{line}", if fenced { "```\n" } else { "" });
        } else {
            cur.push_str(&extra);
        }
        if line.trim().starts_with("```") {
            fenced = !fenced;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The Block Kit blocks for a Markdown body.
pub fn md_to_slack(md: &str) -> Vec<Value> {
    let (mut blocks, mut buf): (Vec<Value>, Vec<String>) = (vec![], vec![]);
    fn flush(blocks: &mut Vec<Value>, buf: &mut Vec<String>) {
        let joined = buf.join("\n").trim_matches('\n').to_string();
        if !joined.trim().is_empty() {
            for c in chunks(&joined) {
                blocks.push(json!({"type": "section", "text": {"type": "mrkdwn", "text": c}}));
            }
        }
        buf.clear();
    }
    for p in pieces(md) {
        match p {
            Piece::Text(t) => buf.push(t),
            Piece::Header(h) => {
                flush(&mut blocks, &mut buf);
                blocks.push(json!({"type": "header", "text": {"type": "plain_text", "text": if h.is_empty() { " " } else { &h }, "emoji": true}}));
            }
            Piece::Divider => {
                flush(&mut blocks, &mut buf);
                blocks.push(json!({"type": "divider"}));
            }
        }
    }
    flush(&mut blocks, &mut buf);
    if blocks.len() > BLOCKS_MAX {
        blocks.truncate(BLOCKS_MAX - 1);
        blocks.push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": "_(message truncated for Slack; the full text is on the board)_"}]}));
    }
    blocks
}

/// Plain text for the notification preview and for clients that can't show blocks.
pub fn fallback_text(md: &str, limit: usize) -> String {
    let (mut keep, mut fenced) = (vec![], false);
    for l in md.lines() {
        if l.trim().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !l.trim().is_empty() && !fenced && !is_delim(l) {
            keep.push(l);
        }
    }
    let text = keep
        .iter()
        .map(|l| {
            let l = l.trim().trim_start_matches(['#', '>', '-', '*', '+', ' ']).replace('|', " ");
            rx!(r"\[!\w+\]\s*").replace_all(&plain(&l), "").to_string()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let text = rx!(r"\s{2,}").replace_all(&text, " ").trim().to_string();
    if text.chars().count() > limit {
        format!("{}…", text.chars().take(limit).collect::<String>())
    } else {
        text
    }
}

/// Python's `html.unescape` for what Slack sends.
pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// A reply typed in Slack, as Markdown.
pub fn slack_to_md(text: &str) -> String {
    let (mut out, mut fenced) = (vec![], false);
    for line in text.split('\n') {
        if line.trim().starts_with("```") {
            fenced = !fenced;
            out.push(line.to_string());
            continue;
        }
        if fenced {
            out.push(unescape(line));
            continue;
        }
        let line = rx!(r"<(https?://[^|>]+)\|([^>]+)>").replace_all(line, "[$2]($1)").to_string();
        let line = rx!(r"<(https?://[^|>]+)>").replace_all(&line, "$1").to_string();
        let line = rx!(r"<[@#!]([^|>]+)(?:\|([^>]+))?>").replace_all(&line, |m: &Captures| format!("@{}", m.get(2).or(m.get(1)).map(|x| x.as_str()).unwrap_or(""))).to_string();
        // split into code spans (kept as they are) and the rest
        let mut rebuilt = String::new();
        let mut last = 0;
        let seg = |s: &str| -> String {
            let s = fx!(r"(?<![\w*])\*(?!\s)([^*\n]+?)(?<!\s)\*(?![\w*])").replace_all(s, "**$1**").to_string();
            let s = fx!(r"(?<![\w_])_(?!\s)([^_\n]+?)(?<!\s)_(?![\w_])").replace_all(&s, "*$1*").to_string();
            let s = fx!(r"(?<![\w~])~(?!\s)([^~\n]+?)(?<!\s)~(?![\w~])").replace_all(&s, "~~$1~~").to_string();
            unescape(&rx!(r"(?m)^(\s*)•\s").replace(&s, "$1- "))
        };
        for m in rx!(r"`[^`]*`").find_iter(&line) {
            rebuilt.push_str(&seg(&line[last..m.start()]));
            rebuilt.push_str(m.as_str());
            last = m.end();
        }
        rebuilt.push_str(&seg(&line[last..]));
        out.push(rebuilt);
    }
    out.join("\n")
}
