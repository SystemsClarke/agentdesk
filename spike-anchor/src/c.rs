//! Strategy C: anchor = (commit, file, range), re-mapped through `git diff`
//! hunks (old commit -> working copy). std + the `git` binary.

use crate::model::{Anchor, Range};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Repo {
    dir: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "core.autocrlf=false", "-c", "commit.gpgsign=false"])
        .args(["-c", "user.name=spike", "-c", "user.email=spike@example.invalid"])
        .args(args)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Repo {
    /// Commit `old_text` as `f.txt` in a fresh scratch repo.
    pub fn new(dir: &Path, old_text: &str) -> Repo {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        std::fs::write(dir.join("f.txt"), old_text).unwrap();
        git(dir, &["add", "f.txt"]);
        git(dir, &["commit", "-q", "-m", "anchor commit"]);
        Repo { dir: dir.into() }
    }

    /// Hunks (old_start, old_len, new_start, new_len) of HEAD vs working copy.
    pub fn hunks(&self, new_text: &str, ignore_ws: bool) -> Vec<(usize, usize, usize, usize)> {
        std::fs::write(self.dir.join("f.txt"), new_text).unwrap();
        let mut args = vec!["diff", "-U0", "--no-color", "--no-ext-diff"];
        if ignore_ws {
            args.push("-w");
        }
        args.extend(["--", "f.txt"]);
        parse(&git(&self.dir, &args))
    }
}

pub fn parse(diff: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut v = vec![];
    for l in diff.lines().filter(|l| l.starts_with("@@ ")) {
        let mut it = l.split_whitespace().skip(1);
        let (o, n) = (it.next().unwrap(), it.next().unwrap());
        let p = |s: &str| -> (usize, usize) {
            let s = &s[1..];
            match s.split_once(',') {
                Some((a, b)) => (a.parse().unwrap(), b.parse().unwrap()),
                None => (s.parse().unwrap(), 1),
            }
        };
        let (os, ol) = p(o);
        let (ns, nl) = p(n);
        v.push((os, ol, ns, nl));
    }
    v
}

/// Map one 0-based old line through hunks; None when the line was touched.
fn map_line(l0: usize, hunks: &[(usize, usize, usize, usize)]) -> Option<usize> {
    let l = l0 + 1; // 1-based
    let mut delta: isize = 0;
    for &(os, ol, _ns, nl) in hunks {
        if ol == 0 {
            // pure insertion after old line `os`
            if os < l {
                delta += nl as isize;
            }
        } else if l >= os && l < os + ol {
            return None;
        } else if os + ol <= l {
            delta += nl as isize - ol as isize;
        }
    }
    Some((l as isize + delta - 1) as usize)
}

pub fn locate(hunks: &[(usize, usize, usize, usize)], a: &Anchor) -> Option<Range> {
    let mut mapped = vec![];
    for l in a.start..=a.end {
        mapped.push(map_line(l, hunks)?);
    }
    Some((mapped[0], *mapped.last().unwrap()))
}
