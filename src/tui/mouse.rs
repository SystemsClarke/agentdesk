//! The mouse: what a click lands on, and whether two clicks are a double click.
//!
//! The window records a hit map while it draws (the rectangle each list row, main-menu entry and footer hint occupies on the
//! screen), so a click is looked up in what was drawn rather than recomputed from layout arithmetic that could drift from it.

use super::seg::Seg;
use super::{App, Screen};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// Two clicks on the same target this close together are a double click.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// What a rectangle on the screen stands for.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// Row `i` of the current screen's list.
    Row(usize),
    /// A main-menu entry, by its key letter (same as pressing it).
    Menu(char),
    /// A footer hint, by the key it names (same as pressing it).
    Hint(ratatui::crossterm::event::KeyEvent),
}

#[derive(Clone, Debug, Default)]
pub struct HitMap {
    hits: Vec<(Rect, Target)>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.hits.clear();
    }

    pub fn add(&mut self, r: Rect, t: Target) {
        if r.width > 0 && r.height > 0 {
            self.hits.push((r, t));
        }
    }

    /// The target under cell (`col`, `row`); the last one drawn wins where rectangles overlap.
    pub fn at(&self, col: u16, row: u16) -> Option<&Target> {
        self.hits.iter().rev().find(|(r, _)| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height).map(|(_, t)| t)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.hits.len()
    }
}

/// Remembers the last click so the next one can be told to be its second half.
#[derive(Debug, Default)]
pub struct Clicks {
    last: Option<(Instant, String)>,
}

impl Clicks {
    /// Registers a click on `id` at `now`; true when it completes a double click. A completed double click is forgotten, so a third
    /// click starts over instead of firing again.
    pub fn click(&mut self, now: Instant, id: &str) -> bool {
        let double = matches!(&self.last, Some((t, last)) if last == id && now.saturating_duration_since(*t) <= DOUBLE_CLICK);
        self.last = if double { None } else { Some((now, id.to_string())) };
        double
    }

    pub fn forget(&mut self) {
        self.last = None;
    }
}

/// Whether `AGENTDESK_MOUSE`'s value turns the mouse off.
fn off_value(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "0" | "off" | "false" | "no")
}

/// `AGENTDESK_MOUSE=0` (or `off`/`false`/`no`) turns the mouse off whatever the Options screen says.
pub fn env_off() -> bool {
    std::env::var("AGENTDESK_MOUSE").is_ok_and(|v| off_value(&v))
}

/// The footer hint's key as a key press: `Esc`, `↵`, `Ctrl+R` or one letter; `None` for the hints that name several keys or none.
fn hint_key(text: &str) -> Option<KeyEvent> {
    let t = text.trim();
    let mut parts = t.split_whitespace();
    let first = parts.next()?;
    let rest_is_one_key = parts.next().is_none_or(|o| o.chars().count() == 1 && parts.next().is_none());
    match first {
        "Esc" if t == "Esc" => return Some(KeyEvent::from(KeyCode::Esc)),
        "↵" if rest_is_one_key => return Some(KeyEvent::from(KeyCode::Enter)),
        _ => {}
    }
    if let Some(c) = t.strip_prefix("Ctrl+") {
        let mut cs = c.chars();
        return match (cs.next(), cs.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => Some(KeyEvent::new(KeyCode::Char(c.to_ascii_lowercase()), KeyModifiers::CONTROL)),
            _ => None,
        };
    }
    let mut cs = t.chars();
    match (cs.next(), cs.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(KeyEvent::from(KeyCode::Char(c.to_ascii_lowercase()))),
        _ => None,
    }
}

/// Where each clickable footer hint sits: (column, width, key). A hint is its key and its label together.
fn hint_cells(hints: &[Seg]) -> Vec<(u16, u16, KeyEvent)> {
    let (mut x, mut i, mut out) = (0usize, 0, vec![]);
    while i < hints.len() {
        let w = hints[i].text.width();
        if hints[i].tags == "ye inv" {
            let end = x + w + hints.get(i + 1).map_or(0, |l| l.text.width());
            if let Some(k) = hint_key(&hints[i].text) {
                out.push((x as u16, (end - x) as u16, k));
            }
            x = end;
            i += 2;
        } else {
            x += w;
            i += 1;
        }
    }
    out
}

impl App {
    /// Whether the window should be asking the terminal for the mouse now.
    pub fn mouse_wanted(&self) -> bool {
        self.pref_b("mouse", true) && !env_off()
    }

    /// Notes where the frame just drawn can be clicked: the list rows, the main-menu cells and the footer hints.
    pub(super) fn record_hits(&mut self, body: Rect, bar: Rect) {
        self.hits.clear();
        self.hits_screen = self.screen;
        let at = |line: usize, scroll: usize| -> Option<u16> { line.checked_sub(scroll).filter(|l| *l < body.height as usize).map(|l| body.y + l as u16) };
        if self.screen != Screen::Read {
            let n = self.item_count();
            for &(line, item) in &self.row_lines {
                if let (Some(y), true) = (at(line, 0), item < n) {
                    self.hits.add(Rect::new(body.x, y, body.width, 1), Target::Row(item));
                }
            }
        }
        if self.screen == Screen::Main {
            for &(line, x, w, key) in &self.menu_cells {
                if let Some(y) = at(line, self.scroll) {
                    let x = (x as u16).min(body.width);
                    self.hits.add(Rect::new(body.x + x, y, (w as u16).min(body.width - x), 1), Target::Menu(key));
                }
            }
        }
        if self.confirm.is_some() || self.flash.is_none() {
            for (x, w, k) in hint_cells(&self.fitted_hints(bar.width as usize)) {
                self.hits.add(Rect::new(bar.x + x, bar.y, w.min(bar.width.saturating_sub(x)), 1), Target::Hint(k));
            }
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(m.column, m.row, Instant::now()),
            MouseEventKind::ScrollUp => self.wheel(-1),
            MouseEventKind::ScrollDown => self.wheel(1),
            _ => {}
        }
    }

    /// A left click at (`col`, `row`): what is drawn there is looked up, and then it is the key press it stands for.
    pub(super) fn click(&mut self, col: u16, row: u16, now: Instant) {
        // the frame the hit map belongs to is not the screen any more (a key moved on and nothing has been drawn since)
        if self.hits_screen != self.screen {
            return;
        }
        let Some(t) = self.hits.at(col, row).cloned() else { return self.clicks.forget() };
        self.dirty = true;
        match t {
            Target::Hint(k) => {
                self.clicks.forget();
                self.on_key(k);
            }
            Target::Menu(c) => {
                self.clicks.forget();
                if self.confirm.is_none() {
                    self.letter(c);
                }
            }
            Target::Row(i) => {
                if self.confirm.is_some() || i >= self.item_count() {
                    return;
                }
                let id = format!("{}:{i}", self.sel_key());
                self.sel.insert(self.sel_key(), i);
                if self.clicks.click(now, &id) {
                    self.activate();
                }
            }
        }
    }

    /// The wheel: three rows (or lines) a notch. A list moves its selection, as the arrow keys do; the reader and the text screens scroll.
    pub(super) fn wheel(&mut self, dir: i64) {
        if self.confirm.is_some() || matches!(self.screen, Screen::Compose | Screen::Ask) {
            return;
        }
        if self.screen != Screen::Read && self.item_count() > 0 {
            self.step(dir * 3);
        } else if dir < 0 {
            self.scroll = self.scroll.saturating_sub(3);
        } else {
            self.scroll += 3;
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    #[test]
    fn hit_map_finds_the_cell_it_was_given_and_nothing_outside() {
        let mut m = HitMap::default();
        m.add(Rect::new(2, 5, 10, 1), Target::Row(3));
        m.add(Rect::new(0, 9, 4, 1), Target::Menu('q'));
        m.add(Rect::new(0, 0, 0, 1), Target::Menu('z')); // empty: dropped
        assert_eq!(m.len(), 2);
        assert_eq!(m.at(2, 5), Some(&Target::Row(3)));
        assert_eq!(m.at(11, 5), Some(&Target::Row(3)));
        assert_eq!(m.at(12, 5), None);
        assert_eq!(m.at(1, 5), None);
        assert_eq!(m.at(5, 6), None);
        assert_eq!(m.at(3, 9), Some(&Target::Menu('q')));
        m.add(Rect::new(0, 5, 20, 1), Target::Hint(KeyEvent::from(KeyCode::Esc)));
        assert!(matches!(m.at(5, 5), Some(Target::Hint(_))), "the last drawn wins");
        m.clear();
        assert_eq!(m.at(3, 9), None);
    }

    #[test]
    fn two_clicks_on_one_target_within_the_window_are_a_double_click() {
        let mut c = Clicks::default();
        let t = Instant::now();
        assert!(!c.click(t, "a"));
        assert!(c.click(t + Duration::from_millis(399), "a"));
        assert!(!c.click(t + Duration::from_millis(500), "a"), "a third click starts over");
    }

    #[test]
    fn hints_become_the_key_they_name_or_nothing() {
        let k = |t: &str| hint_key(t).map(|e| (e.code, e.modifiers));
        assert_eq!(k(" Esc"), Some((KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(k(" ↵"), Some((KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(k(" ↵ O"), Some((KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(k(" Ctrl+R"), Some((KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert_eq!(k(" Y"), Some((KeyCode::Char('y'), KeyModifiers::NONE)));
        for none in [" ↑↓", " Q D W J", " + -", " type", " PgUp/PgDn", " Alt+N/P", " ←→", ""] {
            assert_eq!(k(none), None, "{none:?}");
        }
    }

    #[test]
    fn hint_cells_cover_each_key_with_its_label() {
        let hints = vec![
            Seg { text: " ↑↓".into(), tags: "ye inv".into() },
            Seg { text: " move ".into(), tags: "mu inv".into() },
            Seg { text: " Esc".into(), tags: "ye inv".into() },
            Seg { text: " menu ".into(), tags: "mu inv".into() },
        ];
        let cells = hint_cells(&hints);
        assert_eq!(cells.len(), 1, "the arrows name no single key");
        assert_eq!((cells[0].0, cells[0].1), (9, 10));
    }

    #[test]
    fn the_env_switch_reads_off_values_only() {
        for v in ["0", "off", "OFF", " false ", "no"] {
            assert!(off_value(v), "{v}");
        }
        for v in ["1", "on", "", "yes"] {
            assert!(!off_value(v), "{v}");
        }
    }

    #[test]
    fn slow_or_elsewhere_is_not_a_double_click() {
        let mut c = Clicks::default();
        let t = Instant::now();
        assert!(!c.click(t, "a"));
        assert!(!c.click(t + Duration::from_millis(401), "a"), "too slow");
        assert!(!c.click(t + Duration::from_millis(500), "b"), "another row");
        assert!(!c.click(t + Duration::from_millis(600), "a"), "back on the first: the second was another row");
        c.forget();
        assert!(!c.click(t + Duration::from_millis(650), "a"));
    }
}
