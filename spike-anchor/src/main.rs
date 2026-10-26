//! Anchor-survival spike. Run from spike-anchor/:  cargo run --release
mod a;
#[cfg(feature = "with-loro")]
mod b;
mod c;
mod model;

use model::*;
use std::collections::BTreeMap;

const FILES: [&str; 5] = [
    "../src/kinds.rs",
    "../src/governor.rs",
    "../src/vault_search.rs",
    "../src/slackfmt.rs",
    "../src/bridge.rs",
];

fn scenarios(files: &[Vec<Line>], anchors: &[Vec<Anchor>]) -> Vec<Scenario> {
    let mut out = vec![];
    for (f, old) in files.iter().enumerate() {
        let an = &anchors[f];
        let mut add = |kind: &'static str, new: Vec<Line>| {
            out.push(Scenario {
                name: format!("{}:{kind}", FILES[f].rsplit('/').next().unwrap()),
                kind,
                file: f,
                new,
            })
        };
        add("insert-above", insert_above(old, an[1].start, 3));
        add("insert-inside", insert_above(old, an[2].start + 2, 2));
        add("delete-nearby", delete_range(old, an[1].end + 2, 6));
        add("reformat-ws", halve_indent(old));
        add("move-block", move_block(old, an[1].start - 2, an[1].end + 3));
        add("delete-anchor", delete_range(old, an[0].start, 4));
        add("rewrite-anchor", rewrite_reverse_words(old, &an[2]));
        match f {
            0 => add("AGENT rename", agent_rename(old)),
            1 => add(
                "AGENT rewrite-body",
                agent_rewrite_body(
                    old,
                    "pub fn spendable(",
                    &[
                        "    let room = (remaining - baseline).max(0.0);",
                        "    let guard = sigma * k * hours.sqrt();",
                        "    (room - guard).max(0.0)",
                    ],
                ),
            ),
            2 => add("AGENT add-docs", agent_add_docs(old)),
            3 => add("AGENT swap-fns", agent_swap_fns(old, "fn width(", "fn grid(")),
            _ => add("AGENT rewrite+rename", agent_rewrite_and_rename(old)),
        }
    }
    out
}

#[derive(Default, Clone, Copy)]
struct Tally {
    ok: u32,
    wrong: u32,
    lost_ok: u32,   // truth: gone, strategy said so
    lost_miss: u32, // truth: still there, strategy declined (honest, but a miss)
}

impl Tally {
    fn add(&mut self, truth: Option<Range>, got: Option<Range>) {
        match (truth, got) {
            (Some(t), Some(g)) if t == g => self.ok += 1,
            (_, Some(_)) => self.wrong += 1,
            (None, None) => self.lost_ok += 1,
            (Some(_), None) => self.lost_miss += 1,
        }
    }
    fn fmt(&self) -> String {
        format!(
            "{:>3}/{:>3}/{:>3}/{:>3}",
            self.ok, self.wrong, self.lost_ok, self.lost_miss
        )
    }
}

fn main() {
    let files: Vec<Vec<Line>> = FILES.iter().map(|p| load(p)).collect();
    let anchors: Vec<Vec<Anchor>> = files.iter().map(|f| pick_anchors(f)).collect();
    let scs = scenarios(&files, &anchors);
    println!("scenarios: {}  anchors/file: 3  trials/strategy: {}", scs.len(), scs.len() * 3);

    let tmp = std::env::temp_dir().join("spike-anchor-repos");
    let repos: Vec<c::Repo> = files
        .iter()
        .enumerate()
        .map(|(i, f)| c::Repo::new(&tmp.join(format!("r{i}")), &join(f)))
        .collect();

    let names = ["A@0.6", "A@0.8", "Bd-line", "Bd-token", "Bo-line", "Bo-token", "C", "C-w"];
    // kind -> strategy -> tally
    let mut by_kind: BTreeMap<String, Vec<Tally>> = BTreeMap::new();
    let mut total = vec![Tally::default(); names.len()];
    let mut wrong_log: Vec<String> = vec![];

    for sc in &scs {
        let old = &files[sc.file];
        let an = &anchors[sc.file];
        let (old_t, new_t) = (texts(old), texts(&sc.new));
        let truths: Vec<Option<Range>> = an.iter().map(|a| truth(a, &sc.new)).collect();
        let h = repos[sc.file].hunks(&join(&sc.new), false);
        let hw = repos[sc.file].hunks(&join(&sc.new), true);
        #[cfg(feature = "with-loro")]
        let bl = b::run(&old_t, &new_t, an, b::Mode::Line);
        #[cfg(feature = "with-loro")]
        let bt = b::run(&old_t, &new_t, an, b::Mode::Token);
        #[cfg(feature = "with-loro")]
        let ol = b::run_with(&old_t, &new_t, an, b::Mode::Line, b::Edit::Ops(&h));
        #[cfg(feature = "with-loro")]
        let ot = b::run_with(&old_t, &new_t, an, b::Mode::Token, b::Edit::Ops(&h));

        for (i, a) in an.iter().enumerate() {
            let am = a::A::new(&old_t, a);
            let mut got: Vec<Option<Range>> = vec![
                am.locate(&new_t, 0.6),
                am.locate(&new_t, 0.8),
            ];
            #[cfg(feature = "with-loro")]
            {
                got.push(bl[i]);
                got.push(bt[i]);
                got.push(ol[i]);
                got.push(ot[i]);
            }
            #[cfg(not(feature = "with-loro"))]
            {
                got.extend([None; 4]);
            }
            got.push(c::locate(&h, a));
            got.push(c::locate(&hw, a));
            let row = by_kind
                .entry(sc.kind.to_string())
                .or_insert_with(|| vec![Tally::default(); names.len()]);
            for (s, g) in got.iter().enumerate() {
                row[s].add(truths[i], *g);
                total[s].add(truths[i], *g);
                if let (Some(_), true) = (g, truths[i] != *g) {
                    wrong_log.push(format!(
                        "  WRONG {:<8} {:<28} anchor{} truth={:?} got={:?}",
                        names[s], sc.name, i, truths[i], g
                    ));
                }
            }
        }
    }

    println!("\ncells: correct/wrong/lost-honest(truth gone)/lost-missed(truth present)");
    print!("{:<20}", "scenario kind");
    for n in names {
        print!(" {:>13}", n);
    }
    println!();
    for (k, row) in &by_kind {
        print!("{:<20}", k);
        for t in row {
            print!(" {:>13}", t.fmt());
        }
        println!();
    }
    print!("{:<20}", "TOTAL (120)");
    for t in &total {
        print!(" {:>13}", t.fmt());
    }
    println!();
    if std::env::var("SPIKE_VERBOSE").is_ok() {
        for l in &wrong_log {
            println!("{l}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn a_relocates_after_insert() {
        let old = lines(&["a one", "b two", "c three", "d four", "e five", "f six"]);
        let mut new = lines(&["x", "y"]);
        new.extend(old.clone());
        let am = a::A::new(&old, &Anchor { start: 1, end: 3 });
        assert_eq!(am.locate(&new, 0.8), Some((3, 5)));
    }

    #[test]
    fn c_maps_through_hunks() {
        // insert 2 lines after old line 1, delete old line 5
        let h = c::parse("@@ -1,0 +2,2 @@\n+x\n+y\n@@ -5 +6,0 @@\n-gone\n");
        assert_eq!(c::locate(&h, &Anchor { start: 1, end: 3 }), Some((3, 5)));
        assert_eq!(c::locate(&h, &Anchor { start: 3, end: 4 }), None);
    }

    #[cfg(feature = "with-loro")]
    #[test]
    fn loro_cursor_semantics() {
        let old = lines(&["alpha", "beta", "gamma", "delta"]);
        let an = [Anchor { start: 1, end: 2 }];
        // insert exactly at the anchor's start boundary and right after its end
        let new = lines(&["alpha", "NEW", "beta", "gamma", "NEW2", "delta"]);
        assert_eq!(b::run(&old, &new, &an, b::Mode::Line)[0], Some((2, 3)));
        // delete the anchored lines
        let new = lines(&["alpha", "delta"]);
        assert_eq!(b::run(&old, &new, &an, b::Mode::Line)[0], None);
    }
}
