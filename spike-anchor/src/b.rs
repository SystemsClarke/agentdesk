//! Strategy B: Loro text document + stable cursors. The edit is applied to the
//! Loro text with `LoroText::update` (Loro's own diff -> ops), cursors are
//! resolved afterwards with `LoroDoc::get_cursor_pos`.

use crate::model::{Anchor, Range};
use loro::cursor::Side;
use loro::LoroDoc;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// cursors at the line's first / last character (indent included)
    Line,
    /// cursors at first / last non-whitespace character
    Token,
}

fn char_starts(lines: &[String]) -> Vec<usize> {
    let mut v = vec![];
    let mut p = 0;
    for l in lines {
        v.push(p);
        p += l.chars().count() + 1;
    }
    v
}

fn line_of(starts: &[usize], pos: usize) -> usize {
    starts.partition_point(|&s| s <= pos).saturating_sub(1)
}

/// How the edit reaches the Loro document.
pub enum Edit<'a> {
    /// Hand Loro the new text; Loro diffs it character by character.
    Diff,
    /// Line-granular splices (from git hunks), as an editor/bridge that
    /// observes real edit operations would emit them.
    Ops(&'a [(usize, usize, usize, usize)]),
}

pub fn run(old: &[String], new: &[String], anchors: &[Anchor], mode: Mode) -> Vec<Option<Range>> {
    run_with(old, new, anchors, mode, Edit::Diff)
}

pub fn run_with(
    old: &[String],
    new: &[String],
    anchors: &[Anchor],
    mode: Mode,
    edit: Edit,
) -> Vec<Option<Range>> {
    let doc = LoroDoc::new();
    let text = doc.get_text("t");
    let join = |v: &[String]| v.iter().map(|l| format!("{l}\n")).collect::<String>();
    text.insert(0, &join(old)).unwrap();
    let st = char_starts(old);
    let curs: Vec<_> = anchors
        .iter()
        .map(|a| {
            let first = &old[a.start];
            let last = &old[a.end];
            let lead = if mode == Mode::Token {
                first.chars().count() - first.trim_start().chars().count()
            } else {
                0
            };
            let tail = if mode == Mode::Token {
                last.trim_end().chars().count()
            } else {
                last.chars().count()
            };
            let s = text.get_cursor(st[a.start] + lead, Side::Right).unwrap();
            let e = text.get_cursor(st[a.end] + tail - 1, Side::Left).unwrap();
            (s, e)
        })
        .collect();
    match edit {
        Edit::Diff => text.update(&join(new), Default::default()).unwrap(),
        Edit::Ops(hunks) => {
            for &(os, ol, ns, nl) in hunks.iter().rev() {
                let first = if ol == 0 { os } else { os - 1 };
                let from = if first < st.len() { st[first] } else { text.len_unicode() };
                if ol > 0 {
                    let to = if first + ol < st.len() { st[first + ol] } else { text.len_unicode() };
                    text.delete(from, to - from).unwrap();
                }
                if nl > 0 {
                    text.insert(from, &join(&new[ns - 1..ns - 1 + nl])).unwrap();
                }
            }
        }
    }
    let nst = char_starts(new);
    curs.iter()
        .map(|(s, e)| {
            let (rs, re) = (doc.get_cursor_pos(s).ok()?, doc.get_cursor_pos(e).ok()?);
            // `update` is Some when the anchored character itself was deleted:
            // that is Loro's own "I cannot place this exactly" signal.
            if rs.update.is_some() || re.update.is_some() {
                return None;
            }
            let (ps, pe) = (rs.current.pos, re.current.pos);
            if pe < ps {
                return None;
            }
            Some((line_of(&nst, ps), line_of(&nst, pe)))
        })
        .collect()
}
