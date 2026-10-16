//! The folder step of New agent, New goal and New job: the folders John works in (`adopt::folders`, ranked by how recently and how
//! often they were used), filtered as he types (or dictates). Up/Down picks, Tab copies the pick into the box to edit, Enter takes a
//! typed path that exists, else the pick, and a lone digit takes that row. The list is the old app's `FolderPicker`.

use serde_json::Value;

/// How many rows the list shows at once.
pub const ROWS: usize = 9;

#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub path: String,
    pub who: Vec<String>,
    pub uses: i64,
    pub last: String,
}

pub fn from_json(v: &Value) -> Vec<Pick> {
    v["folders"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(Pick {
                        path: f["path"].as_str()?.to_string(),
                        who: f["who"].as_array().map(|w| w.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
                        uses: f["uses"].as_i64().unwrap_or(0),
                        last: f["last"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// The picks whose path or people contain what was typed, ignoring case, spaces and punctuation ("agent desk" finds AgentDesk). A lone
/// digit picks a row, it does not filter.
pub fn filter<'a>(picks: &'a [Pick], query: &str) -> Vec<&'a Pick> {
    let q = norm(query);
    let digit = query.trim().len() == 1 && query.trim().chars().all(|c| c.is_ascii_digit());
    if q.is_empty() || digit {
        return picks.iter().collect();
    }
    picks.iter().filter(|p| norm(&format!("{}{}", p.path, p.who.concat())).contains(&q)).collect()
}

/// What Enter means: nothing typed takes the highlighted pick; a path that exists is taken as typed (`~` is home); a lone digit takes
/// that row; anything else takes the highlighted match. None when there is nothing to take.
pub fn resolve(text: &str, matches: &[&Pick], selected: usize, home: &str, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let text = text.trim();
    let pick = (!matches.is_empty()).then(|| matches[selected.min(matches.len() - 1)].path.clone());
    if text.is_empty() {
        return pick;
    }
    let typed = if text == "~" || text.starts_with("~/") || text.starts_with("~\\") { format!("{home}{}", &text[1..]) } else { text.to_string() };
    if exists(&typed) {
        return Some(typed);
    }
    if text.len() == 1 {
        if let Some(d) = text.chars().next().and_then(|c| c.to_digit(10)).filter(|d| *d >= 1) {
            if let Some(m) = matches.get(d as usize - 1).filter(|_| (d as usize) <= ROWS) {
                return Some(m.path.clone());
            }
        }
    }
    pick
}

/// "3d ago" style age of an RFC 3339 time.
pub fn age(last: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(last) else { return String::new() };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds().max(0);
    match secs {
        0..=3599 => format!("{}m ago", (secs / 60).max(1)),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// `path` cut from the left to fit `n` columns, padded to `n`: the end of a path (the folder's own name) is the part that tells folders apart.
pub fn shorten(path: &str, n: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    let t = if chars.len() > n { format!("…{}", chars[chars.len() - (n - 1)..].iter().collect::<String>()) } else { path.to_string() };
    format!("{t:<n$}")
}

/// The path with the home folder shown as `~`.
pub fn tilde(path: &str, home: &str) -> String {
    let (p, h) = (path.replace('\\', "/"), home.replace('\\', "/"));
    match p.to_lowercase().strip_prefix(&h.to_lowercase()) {
        Some(rest) if !h.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{}", &p[h.len()..]),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picks() -> Vec<Pick> {
        ["C:\\src\\AgentDesk", "C:\\src\\GoCDTool", "C:\\work\\Fusion Suite"]
            .iter()
            .enumerate()
            .map(|(i, p)| Pick { path: p.to_string(), who: vec![format!("agent{i}")], uses: 3, last: String::new() })
            .collect()
    }

    #[test]
    fn typing_filters_ignoring_case_spaces_and_punctuation_and_a_digit_does_not() {
        let p = picks();
        assert_eq!(filter(&p, "agent desk").len(), 1);
        assert_eq!(filter(&p, "FUSION").len(), 1);
        assert_eq!(filter(&p, "agent2").len(), 1, "the people who used it count");
        assert_eq!(filter(&p, "zzz").len(), 0);
        assert_eq!(filter(&p, "").len(), 3);
        assert_eq!(filter(&p, "2").len(), 3, "a lone digit picks a row");
    }

    #[test]
    fn enter_takes_what_was_typed_when_it_exists_else_a_digit_row_else_the_highlight() {
        let p = picks();
        let all = filter(&p, "");
        let exists = |s: &str| s == "C:\\real" || s == "/home/jo/code";
        assert_eq!(resolve("", &all, 1, "/home/jo", &exists).as_deref(), Some("C:\\src\\GoCDTool"), "nothing typed: the highlight");
        assert_eq!(resolve("C:\\real", &all, 1, "/home/jo", &exists).as_deref(), Some("C:\\real"), "a path that exists as typed");
        assert_eq!(resolve("~/code", &all, 0, "/home/jo", &exists).as_deref(), Some("/home/jo/code"), "~ is home");
        assert_eq!(resolve("3", &all, 0, "/home/jo", &exists).as_deref(), Some("C:\\work\\Fusion Suite"), "a digit takes that row");
        assert_eq!(resolve("nope", &all, 2, "/home/jo", &exists).as_deref(), Some("C:\\work\\Fusion Suite"), "else the highlight");
        assert_eq!(resolve("nope", &[], 0, "/home/jo", &exists), None, "nothing to take");
    }

    #[test]
    fn json_becomes_picks_and_paths_show_a_tilde() {
        let v = serde_json::json!({"folders": [{"path": "C:\\Users\\jo\\src", "who": ["a", "b"], "uses": 5, "last": "2026-10-01T00:00:00Z"}]});
        let p = from_json(&v);
        assert_eq!((p.len(), p[0].uses, p[0].who.len()), (1, 5, 2));
        assert_eq!(tilde("C:\\Users\\jo\\src", "C:\\Users\\jo"), "~/src");
        assert_eq!(tilde("D:\\x", "C:\\Users\\jo"), "D:\\x");
        assert!(age("2020-01-01T00:00:00Z").ends_with("d ago"));
        assert_eq!(shorten("~/a/very/long/path/to/repo-name", 12), "…o/repo-name");
        assert_eq!(shorten("~/x", 6), "~/x   ");
    }
}
