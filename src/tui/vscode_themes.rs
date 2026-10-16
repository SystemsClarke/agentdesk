//! Dark themes installed in VS Code, offered as terminal themes (the .NET Options screen's "from your VS Code themes").
//!
//! Discovery reads `~/.vscode/extensions/*/package.json` (and `~/.vscode-insiders`) for `contributes.themes` entries
//! whose `uiTheme` is `vs-dark` or `hc-black`, loads each theme file (JSON with comments, `include` chains) and maps
//! its workbench colours onto the app's fifteen palette keys. The scan runs once, on a background thread, and the
//! result is cached for the life of the process; until it lands the app simply has its built-in themes.

use super::seg::{Palette, ThemeList};
use ratatui::style::Color;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Rgb = (u8, u8, u8);

/// Stable ids look like `vscode:publisher.extension/Theme Label`, so they survive extension updates.
pub const ID_PREFIX: &str = "vscode:";

/// Scanned themes: `None` until the scan has finished, then fixed for the life of the process.
static SCANNED: Mutex<Option<ThemeList>> = Mutex::new(None);
static STARTED: Mutex<bool> = Mutex::new(false);

/// Starts the one background scan of the real home folder. Later calls do nothing. Tests never scan the real home.
pub fn start_scan() {
    if cfg!(test) {
        return;
    }
    let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if *started {
        return;
    }
    *started = true;
    std::thread::spawn(|| {
        let found = scan(&crate::paths::home_dir(), truecolor());
        *SCANNED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(found));
    });
}

/// The themes found, or `None` while the scan is still running.
pub fn scanned() -> Option<ThemeList> {
    SCANNED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
pub fn set_scanned_for_test(v: Option<Vec<(String, Palette)>>) {
    *SCANNED.lock().unwrap_or_else(|e| e.into_inner()) = v.map(Arc::new);
}

/// True colour is used only when the terminal says it can (`COLORTERM`, or Windows Terminal, which does not set it).
pub fn truecolor() -> bool {
    std::env::var("COLORTERM").map(|v| matches!(v.to_ascii_lowercase().as_str(), "truecolor" | "24bit")).unwrap_or(false) || std::env::var_os("WT_SESSION").is_some()
}

/// Every dark theme installed under `home`, sorted by label. A missing folder is just an empty list.
pub fn scan(home: &Path, truecolor: bool) -> Vec<(String, Palette)> {
    let mut out: Vec<(String, Palette)> = Vec::new();
    for root in [".vscode", ".vscode-insiders"] {
        let mut dirs: Vec<PathBuf> = match std::fs::read_dir(home.join(root).join("extensions")) {
            Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
            Err(_) => continue,
        };
        // Newest-looking folder first, so an old copy of the same extension loses the duplicate-id race.
        dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        for dir in dirs {
            for (id, label, colors) in extension_themes(&dir) {
                if out.iter().any(|(k, _)| *k == id) {
                    continue;
                }
                out.push((id, to_palette(label, &colors, truecolor)));
            }
        }
    }
    out.sort_by_key(|(_, p)| p.label.to_lowercase());
    out
}

/// (id, label, merged colours) for each dark theme one extension contributes.
fn extension_themes(dir: &Path) -> Vec<(String, String, serde_json::Map<String, serde_json::Value>)> {
    let Some(pkg) = read_json(&dir.join("package.json")) else { return vec![] };
    let nls = read_json(&dir.join("package.nls.json"));
    let ext = match (pkg["publisher"].as_str(), pkg["name"].as_str()) {
        (Some(p), Some(n)) => format!("{p}.{n}"),
        _ => dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
    };
    let mut out = vec![];
    for t in pkg["contributes"]["themes"].as_array().into_iter().flatten() {
        if !matches!(t["uiTheme"].as_str(), Some("vs-dark") | Some("hc-black")) {
            continue;
        }
        let Some(path) = t["path"].as_str() else { continue };
        let mut label = t["label"].as_str().or(t["id"].as_str()).unwrap_or("").to_string();
        if let Some(key) = label.strip_prefix('%').and_then(|l| l.strip_suffix('%')) {
            let looked = nls.as_ref().and_then(|n| n[key].as_str().or(n[key]["message"].as_str()));
            label = looked.unwrap_or(key).to_string();
        }
        if label.is_empty() {
            continue;
        }
        let mut colors = serde_json::Map::new();
        load_colors(&dir.join(path), &mut colors, 0);
        if colors.is_empty() {
            continue;
        }
        if t["uiTheme"].as_str() == Some("hc-black") {
            colors.entry("__hc").or_insert(serde_json::Value::Bool(true));
        }
        out.push((format!("{ID_PREFIX}{ext}/{label}"), label, colors));
    }
    out
}

/// Reads a theme file's `colors`, following `include` first so the including file's own values win.
fn load_colors(path: &Path, into: &mut serde_json::Map<String, serde_json::Value>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Some(v) = read_json(path) else { return };
    if let Some(inc) = v["include"].as_str() {
        if let Some(parent) = path.parent() {
            load_colors(&parent.join(inc), into, depth + 1);
        }
    }
    if let Some(c) = v["colors"].as_object() {
        for (k, val) in c {
            into.insert(k.clone(), val.clone());
        }
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&strip_jsonc(&text)).ok()
}

/// Removes a BOM, `//` and `/* */` comments and trailing commas, leaving string contents alone.
pub fn strip_jsonc(src: &str) -> String {
    let src = src.trim_start_matches('\u{feff}');
    let c: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '"' => {
                out.push('"');
                i += 1;
                while i < c.len() {
                    out.push(c[i]);
                    if c[i] == '\\' && i + 1 < c.len() {
                        out.push(c[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if c[i - 1] == '"' {
                        break;
                    }
                }
            }
            '/' if c.get(i + 1) == Some(&'/') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                // A comma followed (past whitespace and comments) by a closer is a trailing comma: drop it.
                let mut j = i + 1;
                loop {
                    while j < c.len() && c[j].is_whitespace() {
                        j += 1;
                    }
                    if c.get(j) == Some(&'/') && c.get(j + 1) == Some(&'/') {
                        while j < c.len() && c[j] != '\n' {
                            j += 1;
                        }
                    } else if c.get(j) == Some(&'/') && c.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j < c.len() && !(c[j] == '*' && c.get(j + 1) == Some(&'/')) {
                            j += 1;
                        }
                        j += 2;
                    } else {
                        break;
                    }
                }
                if !matches!(c.get(j), Some('}') | Some(']')) {
                    out.push(',');
                }
                i += 1;
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

/// `#rgb`, `#rrggbb` or `#rrggbbaa` (alpha is blended over `over`).
fn parse_color(s: &str, over: Rgb) -> Option<Rgb> {
    let h = s.trim().strip_prefix('#')?;
    if !h.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    match h.len() {
        3 => {
            let n = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
            Some((n(0)?, n(1)?, n(2)?))
        }
        6 => Some((byte(0)?, byte(2)?, byte(4)?)),
        8 => {
            let a = byte(6)? as f32 / 255.0;
            Some(mix(over, (byte(0)?, byte(2)?, byte(4)?), a))
        }
        _ => None,
    }
}

/// `t` of the way from `a` to `b`.
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Maps a theme's workbench colours onto the app's keys: bg panel line fg mu fa rule pk or ye gr cy pu on_bar textsel.
fn to_palette(label: String, colors: &serde_json::Map<String, serde_json::Value>, truecolor: bool) -> Palette {
    let hc = colors.contains_key("__hc");
    let bg = colors.get("editor.background").and_then(|v| v.as_str()).and_then(|s| parse_color(s, (0, 0, 0))).unwrap_or(if hc { (0, 0, 0) } else { (0x1e, 0x1e, 0x1e) });
    // First key that parses wins; a fallback colour otherwise.
    let pick = |keys: &[&str], d: Rgb| keys.iter().find_map(|k| colors.get(*k).and_then(|v| v.as_str()).and_then(|s| parse_color(s, bg))).unwrap_or(d);
    let fg = pick(&["editor.foreground", "foreground"], (0xd4, 0xd4, 0xd4));
    let panel = pick(&["sideBar.background", "editorWidget.background", "panel.background"], mix(bg, fg, 0.06));
    let line = pick(&["list.inactiveSelectionBackground", "list.hoverBackground"], mix(bg, fg, 0.16));
    let mu = mix(fg, bg, 0.30);
    let fa = pick(&["descriptionForeground"], mix(fg, bg, 0.50));
    let rule = pick(&["editorGroup.border", "panel.border", "contrastBorder"], mix(bg, fg, 0.30));
    let pk = pick(&["terminal.ansiRed", "terminal.ansiBrightRed", "errorForeground"], (0xf1, 0x4c, 0x4c));
    let or = pick(&["editorWarning.foreground", "terminal.ansiBrightRed"], (0xcc, 0x7a, 0x0a));
    let ye = pick(&["terminal.ansiYellow", "terminal.ansiBrightYellow"], (0xe5, 0xe5, 0x10));
    let gr = pick(&["terminal.ansiGreen", "terminal.ansiBrightGreen"], (0x0d, 0xbc, 0x79));
    let cy = pick(&["terminal.ansiCyan", "textLink.foreground", "terminal.ansiBrightCyan"], (0x11, 0xa8, 0xcd));
    let pu = pick(&["terminal.ansiMagenta", "terminal.ansiBrightMagenta"], (0xbc, 0x3f, 0xbc));
    let textsel = pick(&["editor.selectionBackground", "list.activeSelectionBackground"], mix(bg, fg, 0.30));
    let rgb = [bg, panel, line, fg, mu, fa, rule, pk, or, ye, gr, cy, pu, bg, textsel];
    Palette { label, mono: false, colors: rgb.map(|c| if truecolor { Color::Rgb(c.0, c.1, c.2) } else { Color::Indexed(nearest_256(c)) }) }
}

/// The xterm-256 colour (cube or grey ramp) closest to `c`.
pub fn nearest_256(c: Rgb) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| (0..6).min_by_key(|&i| (STEPS[i] as i32 - v as i32).abs()).unwrap_or(0);
    let (r, g, b) = (near(c.0), near(c.1), near(c.2));
    let cube = (STEPS[r], STEPS[g], STEPS[b]);
    let cube_idx = 16 + 36 * r as u8 + 6 * g as u8 + b as u8;
    let avg = (c.0 as i32 + c.1 as i32 + c.2 as i32) / 3;
    let gi = ((avg - 8 + 5) / 10).clamp(0, 23);
    let gv = (8 + 10 * gi) as u8;
    let dist = |p: Rgb| (p.0 as i32 - c.0 as i32).pow(2) + (p.1 as i32 - c.1 as i32).pow(2) + (p.2 as i32 - c.2 as i32).pow(2);
    if dist((gv, gv, gv)) < dist(cube) {
        232 + gi as u8
    } else {
        cube_idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn fixture(home: &Path) {
        let ext = home.join(".vscode/extensions/acme.nightly-1.0.0");
        write(
            &ext.join("package.json"),
            r#"{ "name": "nightly", "publisher": "acme", "contributes": { "themes": [
                { "label": "Nightly Dark", "uiTheme": "vs-dark", "path": "./themes/dark.json" },
                { "label": "Nightly Day", "uiTheme": "vs", "path": "./themes/day.json" },
                { "label": "%contrast%", "uiTheme": "hc-black", "path": "./themes/hc.jsonc" } ] } }"#,
        );
        write(&ext.join("package.nls.json"), r##"{ "contrast": "Nightly Contrast" }"##);
        // Commented JSON with a trailing comma and an include chain: dark -> base.
        write(
            &ext.join("themes/dark.json"),
            "{\n // a comment\n \"include\": \"./base.json\",\n \"colors\": {\n  \"editor.background\": \"#101820\", /* block */\n  \"terminal.ansiRed\": \"#ff0000\",\n },\n}\n",
        );
        write(&ext.join("themes/base.json"), r##"{ "include": "./root.json", "colors": { "editor.background": "#000001", "terminal.ansiGreen": "#00ff00" } }"##);
        write(&ext.join("themes/root.json"), r##"{ "colors": { "editor.foreground": "#eeeeee", "terminal.ansiGreen": "#009900" } }"##);
        write(&ext.join("themes/day.json"), r##"{ "colors": { "editor.background": "#ffffff" } }"##);
        write(&ext.join("themes/hc.jsonc"), r##"{ "colors": { "contrastBorder": "#6fc3df" } }"##);
    }

    #[test]
    fn missing_extensions_folder_yields_nothing() {
        let d = tempfile::tempdir().unwrap();
        assert!(scan(d.path(), true).is_empty());
    }

    #[test]
    fn only_dark_themes_are_found_with_stable_ids_and_resolved_labels() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path());
        let found = scan(d.path(), true);
        let ids: Vec<&str> = found.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(ids, ["vscode:acme.nightly/Nightly Contrast", "vscode:acme.nightly/Nightly Dark"]);
    }

    #[test]
    fn comments_trailing_commas_and_include_chains_are_read_and_the_includer_wins() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path());
        let found = scan(d.path(), true);
        let dark = &found.iter().find(|(k, _)| k.ends_with("Nightly Dark")).unwrap().1;
        assert_eq!(dark.color("bg"), Color::Rgb(0x10, 0x18, 0x20));
        assert_eq!(dark.color("fg"), Color::Rgb(0xee, 0xee, 0xee), "from the end of the include chain");
        assert_eq!(dark.color("gr"), Color::Rgb(0, 0xff, 0), "base overrides root");
        assert_eq!(dark.color("pk"), Color::Rgb(0xff, 0, 0));
        assert_eq!(dark.color("on_bar"), dark.color("bg"));
        let hc = &found.iter().find(|(k, _)| k.ends_with("Contrast")).unwrap().1;
        assert_eq!(hc.color("bg"), Color::Rgb(0, 0, 0), "a high-contrast theme with no background is black");
        assert_eq!(hc.color("rule"), Color::Rgb(0x6f, 0xc3, 0xdf));
    }

    #[test]
    fn insiders_are_scanned_and_a_duplicate_id_is_listed_once() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path());
        let from = d.path().join(".vscode/extensions/acme.nightly-1.0.0");
        let to = d.path().join(".vscode-insiders/extensions/acme.nightly-1.0.0");
        for f in ["package.json", "package.nls.json", "themes/dark.json", "themes/base.json", "themes/root.json", "themes/hc.jsonc"] {
            write(&to.join(f), &std::fs::read_to_string(from.join(f)).unwrap());
        }
        write(
            &d.path().join(".vscode-insiders/extensions/zed.other-2.0.0/package.json"),
            r##"{ "name": "other", "publisher": "zed", "contributes": { "themes": [ { "label": "Zed", "uiTheme": "vs-dark", "path": "z.json" } ] } }"##,
        );
        write(&d.path().join(".vscode-insiders/extensions/zed.other-2.0.0/z.json"), r##"{ "colors": { "editor.background": "#222222" } }"##);
        assert_eq!(scan(d.path(), true).len(), 3);
    }

    #[test]
    fn broken_theme_files_are_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        write(
            &d.path().join(".vscode/extensions/a.b-1/package.json"),
            r#"{"name":"b","publisher":"a","contributes":{"themes":[{"label":"X","uiTheme":"vs-dark","path":"x.json"}]}}"#,
        );
        write(&d.path().join(".vscode/extensions/a.b-1/x.json"), "{ not json");
        write(&d.path().join(".vscode/extensions/c.d-1/package.json"), "garbage");
        assert!(scan(d.path(), true).is_empty());
    }

    #[test]
    fn without_true_colour_the_palette_is_256_colour() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path());
        let found = scan(d.path(), false);
        assert!(found.iter().all(|(_, p)| p.colors.iter().all(|c| matches!(c, Color::Indexed(_)))));
        assert_eq!(nearest_256((0, 0, 0)), 16);
        assert_eq!(nearest_256((255, 255, 255)), 231);
        assert_eq!(nearest_256((0xff, 0, 0)), 196);
        assert_eq!(nearest_256((0x80, 0x80, 0x80)), 244);
    }

    #[test]
    fn jsonc_stripper_leaves_strings_alone() {
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc("\u{feff}{\"a\": \"x // not a comment, ]\", // c\n \"b\": [1, 2,], }")).unwrap();
        assert_eq!(v["a"], "x // not a comment, ]");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn alpha_colours_are_blended_over_the_background() {
        assert_eq!(parse_color("#ffffff80", (0, 0, 0)), Some((128, 128, 128)));
        assert_eq!(parse_color("#fff", (0, 0, 0)), Some((255, 255, 255)));
        assert_eq!(parse_color("nope", (0, 0, 0)), None);
    }
}
