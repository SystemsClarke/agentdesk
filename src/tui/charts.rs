//! ```chart and ```mermaid blocks, drawn as text. The original draws them as images (and in Slack as pictures); a terminal
//! gets box-and-bar art with the same information: line/bar/progress/burndown/timeline/sparkline/stat/table charts, and
//! graph/flowchart, stateDiagram-v2, sequenceDiagram and pie diagrams.

use super::seg::{fit, rep, s, L};
use serde_json::Value;
use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn nums(v: &Value) -> Vec<f64> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).collect()).unwrap_or_default()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect()).unwrap_or_default()
}

fn fmt(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn spark(vals: &[f64]) -> String {
    let (lo, hi) = vals.iter().fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
    vals.iter().map(|v| if hi > lo { BLOCKS[(((v - lo) / (hi - lo)) * 7.0).round() as usize] } else { BLOCKS[3] }).collect()
}

fn bar(frac: f64, width: usize) -> String {
    let f = frac.clamp(0.0, 1.0);
    let full = (f * width as f64).floor() as usize;
    let part = ((f * width as f64 - full as f64) * 8.0) as usize;
    let mut out = rep('█', full);
    if full < width {
        if part > 0 {
            out.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉'][part.min(7) - 1]);
        }
        let used = out.chars().count();
        out.push_str(&rep('░', width.saturating_sub(used)));
    }
    out
}

fn title(spec: &Value) -> Option<L> {
    spec["title"].as_str().map(|t| vec![s(t.to_string(), "ye b")])
}

/// One ```chart block. A spec that does not parse is shown with its error, never as a failure of the message.
pub fn chart(src: &str, width: usize) -> Vec<L> {
    let spec: Value = match serde_json::from_str(src) {
        Ok(v) => v,
        Err(e) => return vec![vec![s(format!("chart error: {e}"), "pk")], vec![s(src.lines().next().unwrap_or("").to_string(), "fa")]],
    };
    let mut out: Vec<L> = title(&spec).into_iter().collect();
    let unit = spec["unit"].as_str().unwrap_or("");
    let lower_better = spec["lower_is_better"].as_bool().unwrap_or(false);
    let bw = width.saturating_sub(34).clamp(10, 40);
    match spec["type"].as_str().unwrap_or("") {
        "line" => {
            let x = strs(&spec["x"]);
            if let Some(series) = spec["series"].as_object() {
                for (name, v) in series {
                    let vals = nums(v);
                    let (first, last) = (vals.first().copied().unwrap_or(0.0), vals.last().copied().unwrap_or(0.0));
                    out.push(vec![
                        s(fit(name, 14), "cy"),
                        s(spark(&vals), "fg"),
                        s(format!("  {} → {}{}{unit}", fmt(first), fmt(last), if unit.is_empty() { "" } else { " " }), "mu"),
                    ]);
                }
            }
            if let (Some(a), Some(z)) = (x.first(), x.last()) {
                out.push(vec![s(format!("{}{a} … {z}", rep(' ', 14)), "fa")]);
            }
            if let Some(g) = spec["goal"].as_f64() {
                out.push(vec![s(format!("{}goal {}{unit}", rep(' ', 14), fmt(g)), "gr")]);
            }
        }
        "bar" => {
            let (labels, vals) = (strs(&spec["labels"]), nums(&spec["values"]));
            let max = vals.iter().cloned().fold(0.0, f64::max).max(f64::MIN_POSITIVE);
            for (l, v) in labels.iter().zip(vals.iter()) {
                out.push(vec![s(fit(l, 14), "cy"), s(bar(v / max, bw), "ye"), s(format!(" {}{unit}", fmt(*v)), "fg")]);
            }
        }
        "progress" => {
            for it in spec["items"].as_array().cloned().unwrap_or_default() {
                let (done, total) = (it["done"].as_f64().unwrap_or(0.0), it["total"].as_f64().unwrap_or(0.0));
                let frac = if total > 0.0 { done / total } else { 0.0 };
                out.push(vec![
                    s(fit(it["label"].as_str().unwrap_or(""), 14), "cy"),
                    s(bar(frac, bw), if frac >= 1.0 { "gr" } else { "ye" }),
                    s(format!(" {}/{} ({:.0}%)", fmt(done), fmt(total), frac * 100.0), "fg"),
                ]);
            }
        }
        "burndown" => {
            let (x, rem) = (strs(&spec["x"]), nums(&spec["remaining"]));
            out.push(vec![
                s(fit("remaining", 14), "cy"),
                s(spark(&rem), "fg"),
                s(format!("  {} → {}", fmt(rem.first().copied().unwrap_or(0.0)), fmt(rem.last().copied().unwrap_or(0.0))), "mu"),
            ]);
            if let (Some(a), Some(z)) = (x.first(), x.last()) {
                out.push(vec![s(format!("{}{a} … {z}", rep(' ', 14)), "fa")]);
            }
        }
        "timeline" => {
            for e in spec["events"].as_array().cloned().unwrap_or_default() {
                out.push(vec![s("● ", "ye"), s(fit(e["when"].as_str().unwrap_or(""), 12), "mu"), s(e["label"].as_str().unwrap_or("").to_string(), "fg")]);
            }
        }
        "sparkline" => {
            let vals = nums(&spec["values"]);
            let (first, last) = (vals.first().copied().unwrap_or(0.0), vals.last().copied().unwrap_or(0.0));
            let good = if lower_better { last <= first } else { last >= first };
            out.push(vec![s(spark(&vals), "cy"), s(format!("  {}{unit}", fmt(last)), if good { "gr" } else { "or" })]);
        }
        "stat" => {
            let tiles = spec["tiles"].as_array().cloned().unwrap_or_default();
            let cell = (width.saturating_sub(2) / tiles.len().max(1)).clamp(12, 26);
            let (mut top, mut mid, mut bot): (L, L, L) = (vec![], vec![], vec![]);
            for t in &tiles {
                let delta = t["delta"].as_str().unwrap_or("");
                let up = delta.starts_with('+');
                let good = if lower_better || t["lower_is_better"].as_bool().unwrap_or(false) { !up } else { up };
                top.push(s(fit(t["label"].as_str().unwrap_or(""), cell), "mu"));
                mid.push(s(fit(t["value"].as_str().map(String::from).unwrap_or_else(|| t["value"].to_string()).as_str(), cell), "fg b"));
                bot.push(s(
                    fit(delta, cell),
                    if delta.is_empty() {
                        "mu"
                    } else if good {
                        "gr"
                    } else {
                        "pk"
                    },
                ));
            }
            out.extend([top, mid, bot]);
        }
        "table" => {
            let cols = strs(&spec["columns"]);
            let mut rows: Vec<Vec<String>> = vec![cols];
            for r in spec["rows"].as_array().cloned().unwrap_or_default() {
                rows.push(strs(&r));
            }
            let n = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            let mut w = vec![1usize; n];
            for r in &rows {
                for (i, c) in r.iter().enumerate() {
                    w[i] = w[i].max(c.width());
                }
            }
            for (ri, r) in rows.iter().enumerate() {
                let mut line: L = vec![];
                for (i, wi) in w.iter().enumerate() {
                    line.push(s(fit(r.get(i).map(|x| x.as_str()).unwrap_or(""), *wi), if ri == 0 { "b" } else { "" }));
                    if i + 1 < w.len() {
                        line.push(s(" │ ", "rule"));
                    }
                }
                out.push(line);
                if ri == 0 {
                    out.push(vec![s(w.iter().map(|x| rep('─', *x)).collect::<Vec<_>>().join("─┼─"), "rule")]);
                }
            }
        }
        other => out.push(vec![s(format!("chart error: unknown type {other:?}"), "pk")]),
    }
    out
}

// ---- mermaid

#[derive(Clone)]
struct Node {
    id: String,
    label: String,
    shape: char, // [ box, ( round, { decision
}

fn node(tok: &str) -> Node {
    let t = tok.trim();
    let open = t.find(['[', '(', '{']);
    match open {
        Some(i) => {
            let id = t[..i].trim().to_string();
            let inner: String = t[i + 1..].trim_end_matches([']', ')', '}']).trim_start_matches(['(', '[', '{']).trim().trim_matches('"').to_string();
            Node { id, label: inner, shape: t.chars().nth(i).unwrap_or('[') }
        }
        None => Node { id: t.to_string(), label: t.to_string(), shape: '[' },
    }
}

fn boxed(n: &Node) -> Vec<String> {
    let w = n.label.width() + 2;
    let (tl, tr, bl, br) = match n.shape {
        '(' => ('╭', '╮', '╰', '╯'),
        '{' => ('◇', '◇', '◇', '◇'),
        _ => ('┌', '┐', '└', '┘'),
    };
    vec![format!("{tl}{}{tr}", rep('─', w)), format!("│ {} │", n.label), format!("{bl}{}{br}", rep('─', w))]
}

/// A flowchart or state diagram: nodes in layers by their distance from the roots, boxes side by side, arrows between layers.
fn flow(lines: &[&str], horizontal: bool, state: bool) -> Vec<L> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<(String, String, String)> = Vec::new();
    let add = |n: Node, nodes: &mut Vec<Node>| -> String {
        if let Some(e) = nodes.iter_mut().find(|x| x.id == n.id) {
            if n.label != n.id {
                e.label = n.label.clone();
                e.shape = n.shape;
            }
            return e.id.clone();
        }
        let id = n.id.clone();
        nodes.push(n);
        id
    };
    for raw in lines {
        let l = raw.trim();
        if l.is_empty() || l.starts_with("%%") || l.starts_with("style ") || l.starts_with("classDef") || l == "end" {
            continue;
        }
        let arrow = ["-->", "---", "-.->", "==>"].iter().find_map(|a| l.find(a).map(|i| (i, *a)));
        if let Some((i, a)) = arrow {
            let (lhs, rest) = (&l[..i], &l[i + a.len()..]);
            let mut label = String::new();
            let mut rhs = rest.trim();
            if let Some(r) = rhs.strip_prefix('|') {
                if let Some(e) = r.find('|') {
                    label = r[..e].trim().to_string();
                    rhs = r[e + 1..].trim();
                }
            }
            if state {
                if let Some((n, lab)) = rhs.split_once(':') {
                    label = lab.trim().to_string();
                    rhs = n.trim();
                }
            }
            let mut mk = |t: &str| {
                let t = t.trim();
                let n = if state && t == "[*]" { Node { id: "[*]".into(), label: "●".into(), shape: '(' } } else { node(t) };
                add(n, &mut nodes)
            };
            let (a_id, b_id) = (mk(lhs), mk(rhs));
            edges.push((a_id, b_id, label));
        } else if !l.starts_with("subgraph") && !l.starts_with("direction") {
            add(node(l), &mut nodes);
        }
    }
    if nodes.is_empty() {
        return vec![vec![s("diagram error: no nodes found", "pk")]];
    }
    // layers: longest path from a root, cycles cut
    let idx: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    let mut depth = vec![0usize; nodes.len()];
    for _ in 0..nodes.len() {
        for (a, b, _) in &edges {
            let (ia, ib) = (idx[a.as_str()], idx[b.as_str()]);
            if depth[ib] < depth[ia] + 1 && depth[ia] + 1 < nodes.len() {
                depth[ib] = depth[ia] + 1;
            }
        }
    }
    let max_d = depth.iter().copied().max().unwrap_or(0);
    let mut out: Vec<L> = Vec::new();
    if horizontal {
        // left to right: one row per layer chain is unreadable for wide fan-out, so a layer is a column of boxes
        let cols: Vec<Vec<Vec<String>>> = (0..=max_d).map(|d| nodes.iter().enumerate().filter(|(i, _)| depth[*i] == d).map(|(_, n)| boxed(n)).collect()).collect();
        let height = cols.iter().map(|c| c.len() * 4).max().unwrap_or(3);
        let widths: Vec<usize> = cols.iter().map(|c| c.iter().map(|b| b[0].width()).max().unwrap_or(0)).collect();
        for row in 0..height {
            let mut line = String::new();
            for (ci, c) in cols.iter().enumerate() {
                let (bi, br) = (row / 4, row % 4);
                let cell = c.get(bi).and_then(|b| b.get(br)).cloned().unwrap_or_default();
                line.push_str(&format!("{cell}{}", rep(' ', widths[ci].saturating_sub(cell.width()))));
                if ci + 1 < cols.len() {
                    line.push_str(if br == 1 && c.get(bi).is_some() { " ──▶ " } else { "     " });
                }
            }
            out.push(vec![s(line.trim_end().to_string(), "fg")]);
        }
    } else {
        for d in 0..=max_d {
            let layer: Vec<&Node> = nodes.iter().enumerate().filter(|(i, _)| depth[*i] == d).map(|(_, n)| n).collect();
            let boxes: Vec<Vec<String>> = layer.iter().map(|n| boxed(n)).collect();
            for r in 0..3 {
                out.push(vec![s(boxes.iter().map(|b| b[r].clone()).collect::<Vec<_>>().join("   "), "fg")]);
            }
            if d < max_d {
                let centers: Vec<usize> = {
                    let mut x = 0;
                    boxes
                        .iter()
                        .map(|b| {
                            let c = x + b[0].width() / 2;
                            x += b[0].width() + 3;
                            c
                        })
                        .collect()
                };
                let mut row = String::new();
                for (k, n) in layer.iter().enumerate() {
                    let has_out = edges.iter().any(|(a, _, _)| *a == n.id);
                    if has_out {
                        row.push_str(&rep(' ', centers[k].saturating_sub(row.width())));
                        row.push('│');
                    }
                }
                out.push(vec![s(row.clone(), "rule")]);
                out.push(vec![s(row.replace('│', "▼"), "rule")]);
            }
        }
    }
    let labelled: Vec<String> = edges
        .iter()
        .map(|(a, b, l)| {
            let (la, lb) = (nodes[idx[a.as_str()]].label.clone(), nodes[idx[b.as_str()]].label.clone());
            if l.is_empty() {
                format!("{la} → {lb}")
            } else {
                format!("{la} ─{l}→ {lb}")
            }
        })
        .collect();
    if !labelled.is_empty() && (edges.iter().any(|e| !e.2.is_empty()) || nodes.len() > 4) {
        out.push(vec![]);
        for e in labelled {
            out.push(vec![s(format!("  {e}"), "mu")]);
        }
    }
    out
}

fn sequence(lines: &[&str]) -> Vec<L> {
    let mut parts: Vec<String> = Vec::new();
    let mut msgs: Vec<(String, String, String, bool)> = Vec::new();
    for raw in lines {
        let l = raw.trim();
        if let Some(r) = l.strip_prefix("participant ").or_else(|| l.strip_prefix("actor ")) {
            let name = r.split(" as ").last().unwrap_or(r).trim().to_string();
            if !parts.contains(&name) {
                parts.push(name);
            }
            continue;
        }
        for (a, dashed) in [("-->>", true), ("->>", false), ("-->", true), ("->", false)] {
            if let Some(i) = l.find(a) {
                let from = l[..i].trim().to_string();
                let (to, text) =
                    l[i + a.len()..].split_once(':').map(|(t, x)| (t.trim().to_string(), x.trim().to_string())).unwrap_or((l[i + a.len()..].trim().to_string(), String::new()));
                for p in [&from, &to] {
                    if !parts.contains(p) {
                        parts.push(p.clone());
                    }
                }
                msgs.push((from, to, text, dashed));
                break;
            }
        }
    }
    if parts.is_empty() {
        return vec![vec![s("diagram error: no participants found", "pk")]];
    }
    let colw = parts.iter().map(|p| p.width()).max().unwrap_or(4).max(msgs.iter().map(|m| m.2.width() / 2).max().unwrap_or(0)).max(8) + 4;
    let center = |i: usize| i * colw + colw / 2;
    let mut out: Vec<L> = vec![vec![s(parts.iter().map(|p| format!("{:^colw$}", p)).collect::<String>().trim_end().to_string(), "cy b")]];
    let life = |out: &mut Vec<L>| {
        let mut row: Vec<char> = rep(' ', parts.len() * colw).chars().collect();
        for i in 0..parts.len() {
            row[center(i)] = '│';
        }
        out.push(vec![s(row.iter().collect::<String>().trim_end().to_string(), "rule")]);
    };
    life(&mut out);
    for (from, to, text, dashed) in msgs {
        let (a, b) = (parts.iter().position(|p| *p == from).unwrap(), parts.iter().position(|p| *p == to).unwrap());
        let mut row: Vec<char> = rep(' ', parts.len() * colw).chars().collect();
        for i in 0..parts.len() {
            row[center(i)] = '│';
        }
        let (lo, hi) = (center(a).min(center(b)), center(a).max(center(b)));
        for c in row.iter_mut().take(hi).skip(lo + 1) {
            *c = if dashed { '┄' } else { '─' };
        }
        if a != b {
            if center(b) > center(a) {
                row[hi] = '▶';
            } else {
                row[lo] = '◀';
            }
        }
        let label_row: String = {
            let mut r: Vec<char> = rep(' ', parts.len() * colw).chars().collect();
            for i in 0..parts.len() {
                r[center(i)] = '│';
            }
            for (k, ch) in text.chars().enumerate() {
                if lo + 2 + k < hi.max(lo + 2 + text.chars().count()) && lo + 2 + k < r.len() {
                    r[lo + 2 + k] = ch;
                }
            }
            r.iter().collect::<String>().trim_end().to_string()
        };
        out.push(vec![s(label_row, "fg")]);
        out.push(vec![s(row.iter().collect::<String>().trim_end().to_string(), "rule")]);
    }
    out
}

fn pie(lines: &[&str]) -> Vec<L> {
    let mut out: Vec<L> = Vec::new();
    let mut items: Vec<(String, f64)> = Vec::new();
    for raw in lines {
        let l = raw.trim();
        if let Some(t) = l.strip_prefix("title ") {
            out.push(vec![s(t.to_string(), "ye b")]);
        } else if let Some((k, v)) = l.rsplit_once(':') {
            if let Ok(n) = v.trim().parse::<f64>() {
                items.push((k.trim().trim_matches('"').to_string(), n));
            }
        }
    }
    let total: f64 = items.iter().map(|i| i.1).sum::<f64>().max(f64::MIN_POSITIVE);
    for (k, v) in items {
        out.push(vec![s(fit(&k, 16), "cy"), s(bar(v / total, 24), "ye"), s(format!(" {:.0}%  ({})", 100.0 * v / total, fmt(v)), "fg")]);
    }
    out
}

/// One ```mermaid block.
pub fn mermaid(src: &str) -> Vec<L> {
    let lines: Vec<&str> = src.lines().collect();
    let Some(first) = lines.iter().position(|l| !l.trim().is_empty() && !l.trim().starts_with("%%")) else { return vec![] };
    let head = lines[first].trim();
    let rest = &lines[first + 1..];
    let kind = head.split_whitespace().next().unwrap_or("");
    let body: Vec<L> = match kind {
        "graph" | "flowchart" => flow(rest, matches!(head.split_whitespace().nth(1), Some("LR") | Some("RL")), false),
        "stateDiagram-v2" | "stateDiagram" => flow(rest, false, true),
        "sequenceDiagram" => sequence(rest),
        "pie" => {
            let mut all: Vec<&str> = vec![];
            if let Some(t) = head.strip_prefix("pie") {
                if t.trim().starts_with("title") {
                    all.push(t.trim());
                }
            }
            all.extend(rest.iter().copied());
            pie(&all)
        }
        other => {
            let mut v: Vec<L> = vec![vec![s(format!("diagram error: {other:?} diagrams are not drawn; source follows"), "pk")]];
            v.extend(src.lines().map(|l| vec![s(format!("  {l}"), "fa")]));
            return v;
        }
    };
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[L]) -> String {
        lines.iter().map(|l| l.iter().map(|x| x.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn every_chart_type_draws() {
        let specs = [
            (
                r#"{"type":"line","title":"Compile time","unit":"min","x":["09-01","09-08"],"series":{"JAWS":[41,29]},"goal":25}"#,
                vec!["Compile time", "JAWS", "41 → 29 min", "goal 25min", "09-01 … 09-08"],
            ),
            (r#"{"type":"bar","title":"Builds per lane","labels":["compile","sign"],"values":[41,12]}"#, vec!["Builds per lane", "compile", "█", "41", "12"]),
            (r#"{"type":"progress","title":"Migration","items":[{"label":"groups","done":7,"total":20}]}"#, vec!["groups", "7/20 (35%)"]),
            (r#"{"type":"burndown","title":"Sprint","x":["Mon","Wed"],"remaining":[30,19]}"#, vec!["remaining", "30 → 19", "Mon … Wed"]),
            (r#"{"type":"timeline","title":"Cutover","events":[{"when":"09-01","label":"canary"}]}"#, vec!["● ", "09-01", "canary"]),
            (r#"{"type":"sparkline","title":"queue","values":[3,5,2,8,4]}"#, vec!["queue", "4"]),
            (r#"{"type":"stat","tiles":[{"label":"green builds","value":"94%","delta":"+6%"}]}"#, vec!["green builds", "94%", "+6%"]),
            (r##"{"type":"table","columns":["run","result"],"rows":[["#45","pass"],["#46","fail"]]}"##, vec!["run", "│", "#46", "fail"]),
        ];
        for (src, want) in specs {
            let t = text(&chart(src, 100));
            for w in want {
                assert!(t.contains(w), "{w} missing in:\n{t}");
            }
            assert!(!t.contains("\"type\""), "drawn, not echoed:\n{t}");
        }
    }

    #[test]
    fn a_bad_chart_says_why() {
        assert!(text(&chart("{not json", 80)).contains("chart error"));
        assert!(text(&chart(r#"{"type":"nope"}"#, 80)).contains("unknown type"));
    }

    #[test]
    fn diagrams_draw() {
        let t = text(&mermaid("graph TD\n  A[Start] --> B{Ok?}\n  B -->|yes| C[Ship]\n  B -->|no| D[Fix]"));
        for w in ["┌", "Start", "Ok?", "Ship", "Fix", "▼", "yes"] {
            assert!(t.contains(w), "{w} missing in:\n{t}");
        }
        let lr = text(&mermaid("flowchart LR\n  A[One] --> B[Two] --> C[Three]"));
        assert!(lr.contains("One") && lr.contains("──▶") && lr.contains("Three"), "{lr}");
        let st = text(&mermaid("stateDiagram-v2\n  [*] --> Idle\n  Idle --> Busy: start\n  Busy --> [*]"));
        assert!(st.contains("Idle") && st.contains("Busy") && st.contains("●"), "{st}");
        let sq = text(&mermaid("sequenceDiagram\n  participant A\n  participant B\n  A->>B: hello\n  B-->>A: hi"));
        assert!(sq.contains("hello") && sq.contains("▶") && sq.contains("◀"), "{sq}");
        let pie = text(&mermaid("pie title Pets\n  \"Dogs\" : 3\n  \"Cats\" : 1"));
        assert!(pie.contains("Pets") && pie.contains("75%") && pie.contains("25%"), "{pie}");
        assert!(text(&mermaid("gantt\n  title x")).contains("not drawn"));
    }
}
