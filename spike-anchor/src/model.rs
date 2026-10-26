//! Corpus model: a file is a list of lines, each carrying a stable identity
//! (`id`) that the *scenario author* tracks. Ground truth is computed from ids,
//! never from any strategy's output.

#[derive(Clone, Debug)]
pub struct Line {
    pub id: Option<usize>,
    pub text: String,
}

pub fn load(path: &str) -> Vec<Line> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    // Corpus files are CRLF; normalise to LF (stated limit of the spike).
    raw.lines()
        .enumerate()
        .map(|(i, t)| Line {
            id: Some(i),
            text: t.to_string(),
        })
        .collect()
}

pub fn join(lines: &[Line]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(&l.text);
        s.push('\n');
    }
    s
}

pub fn texts(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|l| l.text.clone()).collect()
}

/// Inclusive line range, 0-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub start: usize,
    pub end: usize,
}

pub type Range = (usize, usize);

/// Rule (fixed before looking at results): 3 anchors per file at ~15%, 45%,
/// 75% of the file; each is the first 4 consecutive non-blank, non-trivial
/// lines (trimmed length > 3) at or after that point.
pub fn pick_anchors(lines: &[Line]) -> Vec<Anchor> {
    let mut out = vec![];
    for frac in [0.15, 0.45, 0.75] {
        let mut i = (lines.len() as f64 * frac) as usize;
        loop {
            let ok = (i..i + 4).all(|k| k < lines.len() && lines[k].text.trim().len() > 3);
            if ok {
                out.push(Anchor {
                    start: i,
                    end: i + 3,
                });
                break;
            }
            i += 1;
        }
    }
    out
}

/// Ground truth: where did the anchored lines go? `None` = the anchored text
/// no longer exists (any line deleted/rewritten, or reordered).
pub fn truth(a: &Anchor, new: &[Line]) -> Option<Range> {
    let mut pos = vec![];
    for id in a.start..=a.end {
        let p = new.iter().position(|l| l.id == Some(id))?;
        pos.push(p);
    }
    if pos.windows(2).all(|w| w[0] < w[1]) {
        Some((pos[0], *pos.last().unwrap()))
    } else {
        None
    }
}

pub struct Scenario {
    pub name: String,
    pub kind: &'static str,
    pub file: usize,
    pub new: Vec<Line>,
}

fn fresh(t: &str) -> Line {
    Line {
        id: None,
        text: t.to_string(),
    }
}

// ---------------------------------------------------------------- procedural

pub fn insert_above(old: &[Line], at: usize, n: usize) -> Vec<Line> {
    let mut v = old.to_vec();
    for k in 0..n {
        v.insert(at + k, fresh(&format!("// inserted line {k}")));
    }
    v
}

pub fn delete_range(old: &[Line], from: usize, n: usize) -> Vec<Line> {
    let mut v = old.to_vec();
    v.drain(from..(from + n).min(old.len()));
    v
}

pub fn halve_indent(old: &[Line]) -> Vec<Line> {
    old.iter()
        .map(|l| {
            let lead = l.text.len() - l.text.trim_start_matches(' ').len();
            Line {
                id: l.id,
                text: format!("{}{}", " ".repeat(lead / 2), l.text.trim_start_matches(' ')),
            }
        })
        .collect()
}

pub fn move_block(old: &[Line], from: usize, to_excl: usize) -> Vec<Line> {
    let mut v = old.to_vec();
    let block: Vec<Line> = v.drain(from..to_excl.min(old.len())).collect();
    v.extend(block);
    v
}

pub fn rewrite_reverse_words(old: &[Line], a: &Anchor) -> Vec<Line> {
    let mut v = old.to_vec();
    for l in v.iter_mut().take(a.end + 1).skip(a.start) {
        let lead = l.text.len() - l.text.trim_start().len();
        let words: Vec<&str> = l.text.split_whitespace().rev().collect();
        *l = Line {
            id: None,
            text: format!("{}{}", " ".repeat(lead), words.join(" ")),
        };
    }
    v
}

// ------------------------------------------------- hand-authored agent style

/// Index range [start, end] of a top-level fn including preceding doc/attr lines.
fn fn_block(lines: &[Line], sig_prefix: &str) -> (usize, usize, usize) {
    let sig = lines
        .iter()
        .position(|l| l.text.starts_with(sig_prefix))
        .unwrap_or_else(|| panic!("no fn {sig_prefix}"));
    let mut top = sig;
    while top > 0 {
        let t = lines[top - 1].text.trim_start();
        if t.starts_with("///") || t.starts_with("#[") {
            top -= 1;
        } else {
            break;
        }
    }
    let close = (sig..lines.len())
        .find(|&i| lines[i].text == "}")
        .expect("no close");
    (top, sig, close)
}

/// kinds.rs: global rename `kind` -> `category` (agent "rename" refactor).
pub fn agent_rename(old: &[Line]) -> Vec<Line> {
    old.iter()
        .map(|l| Line {
            id: l.id,
            text: l.text.replace("kind", "category"),
        })
        .collect()
}

/// governor.rs: rewrite the body of `spendable`, keep signature and closing.
pub fn agent_rewrite_body(old: &[Line], sig: &str, body: &[&str]) -> Vec<Line> {
    let (_, s, c) = fn_block(old, sig);
    let mut v: Vec<Line> = old[..=s].to_vec();
    v.extend(body.iter().map(|t| fresh(t)));
    v.extend_from_slice(&old[c..]);
    v
}

/// vault_search.rs: add a doc comment above every `pub fn` and rename a helper.
pub fn agent_add_docs(old: &[Line]) -> Vec<Line> {
    let mut v = vec![];
    for l in old {
        if l.text.starts_with("pub fn ") {
            v.push(fresh("/// Documented by an agent pass."));
        }
        v.push(l.clone());
    }
    v
}

/// slackfmt.rs: swap two adjacent top-level fns (`width` and `grid`).
pub fn agent_swap_fns(old: &[Line], a: &str, b: &str) -> Vec<Line> {
    let (at, _, ac) = fn_block(old, a);
    let (bt, _, bc) = fn_block(old, b);
    assert!(ac < bt, "expected {a} before {b}");
    let mut v: Vec<Line> = old[..at].to_vec();
    v.extend_from_slice(&old[bt..=bc]);
    v.extend_from_slice(&old[ac + 1..bt]);
    v.extend_from_slice(&old[at..=ac]);
    v.extend_from_slice(&old[bc + 1..]);
    v
}

/// bridge.rs: rewrite one fn body AND rename a helper everywhere.
pub fn agent_rewrite_and_rename(old: &[Line]) -> Vec<Line> {
    let v = agent_rewrite_body(
        old,
        "pub fn heartbeat()",
        &[
            "    let path = heartbeat_file();",
            "    let Ok(raw) = std::fs::read_to_string(&path) else {",
            "        return (None, None);",
            "    };",
            "    heartbeat_of(&serde_json::from_str(&raw).unwrap_or_default())",
        ],
    );
    v.into_iter()
        .map(|l| Line {
            id: l.id,
            text: l.text.replace("status_file", "state_path"),
        })
        .collect()
}
