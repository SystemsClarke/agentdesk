//! Strategy A: line number + content hash, relocated by fuzzy search of the
//! anchored lines and the 3 surrounding lines (git-blame-like). std only.

use crate::model::{Anchor, Range};
use std::collections::HashMap;

pub struct A {
    start: usize,
    body: Vec<String>,
    before: Vec<String>,
    after: Vec<String>,
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bigrams(s: &str) -> HashMap<(char, char), u32> {
    let c: Vec<char> = s.chars().collect();
    let mut m = HashMap::new();
    for w in c.windows(2) {
        *m.entry((w[0], w[1])).or_insert(0) += 1;
    }
    m
}

/// Dice coefficient on character bigrams of whitespace-normalised lines.
fn sim(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let (x, y) = (bigrams(a), bigrams(b));
    let (nx, ny): (u32, u32) = (x.values().sum(), y.values().sum());
    if nx == 0 || ny == 0 {
        return 0.0;
    }
    let common: u32 = x.iter().map(|(k, v)| (*v).min(*y.get(k).unwrap_or(&0))).sum();
    2.0 * common as f64 / (nx + ny) as f64
}

impl A {
    pub fn new(old: &[String], a: &Anchor) -> A {
        let g = |i: isize| -> String {
            if i < 0 || i as usize >= old.len() {
                String::new()
            } else {
                norm(&old[i as usize])
            }
        };
        let s = a.start as isize;
        let e = a.end as isize;
        A {
            start: a.start,
            body: (s..=e).map(g).collect(),
            before: (s - 3..s).map(g).collect(),
            after: (e + 1..e + 4).map(g).collect(),
        }
    }

    /// Returns None when it declines to place the anchor.
    pub fn locate(&self, new_raw: &[String], thresh: f64) -> Option<Range> {
        let new: Vec<String> = new_raw.iter().map(|s| norm(s)).collect();
        let n = self.body.len();
        // 1. unchanged at the same line numbers
        if self.start + n <= new.len() && new[self.start..self.start + n] == self.body[..] {
            return Some((self.start, self.start + n - 1));
        }
        let ctx = |i: usize, m: usize, mat: &[Vec<f64>]| -> f64 {
            // mat rows: 0..n body, n..n+3 before, n+3..n+6 after
            let mut tot = 0.0;
            for k in 0..3 {
                let b = if i + k >= 3 && i + k - 3 < new.len() {
                    mat[n + k][i + k - 3]
                } else if self.before[k].is_empty() {
                    1.0
                } else {
                    0.0
                };
                let a_idx = i + m + k;
                let a = if a_idx < new.len() {
                    mat[n + 3 + k][a_idx]
                } else if self.after[k].is_empty() {
                    1.0
                } else {
                    0.0
                };
                tot += a + b;
            }
            tot / 6.0
        };
        // similarity matrix: anchor body + context lines vs every new line
        let rows: Vec<&String> = self
            .body
            .iter()
            .chain(self.before.iter())
            .chain(self.after.iter())
            .collect();
        let mat: Vec<Vec<f64>> = rows
            .iter()
            .map(|r| new.iter().map(|l| sim(r, l)).collect())
            .collect();

        // 2. exact (normalised) block match elsewhere
        let exact: Vec<usize> = (0..new.len().saturating_sub(n - 1))
            .filter(|&i| new[i..i + n] == self.body[..])
            .collect();
        if exact.len() == 1 {
            return Some((exact[0], exact[0] + n - 1));
        }
        if exact.len() > 1 {
            let mut sc: Vec<(f64, usize)> = exact.iter().map(|&i| (ctx(i, n, &mat), i)).collect();
            sc.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            return if sc[0].0 - sc[1].0 > 1e-9 {
                Some((sc[0].1, sc[0].1 + n - 1))
            } else {
                None // ambiguous duplicates: honest refusal
            };
        }

        // 3. fuzzy: windows of length n-2 ..= n+4, monotone alignment score
        let mut cands: Vec<(f64, usize, usize)> = vec![]; // (score, start, len)
        for m in n.saturating_sub(2).max(1)..=n + 4 {
            if m > new.len() {
                break;
            }
            for i in 0..=new.len() - m {
                // dp over (anchor line, window line)
                let mut dp = vec![vec![0.0f64; m + 1]; n + 1];
                for x in 1..=n {
                    for y in 1..=m {
                        let s = mat[x - 1][i + y - 1];
                        dp[x][y] = dp[x - 1][y].max(dp[x][y - 1]).max(dp[x - 1][y - 1] + s);
                    }
                }
                let body = dp[n][m] / n.max(m) as f64;
                cands.push((0.75 * body + 0.25 * ctx(i, m, &mat), i, m));
            }
        }
        cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        let best = *cands.first()?;
        let second = cands
            .iter()
            .find(|c| c.1 + c.2 <= best.1 || c.1 >= best.1 + best.2)
            .map(|c| c.0)
            .unwrap_or(0.0);
        if best.0 >= thresh && best.0 - second >= 0.05 {
            Some((best.1, best.1 + best.2 - 1))
        } else {
            None
        }
    }
}
