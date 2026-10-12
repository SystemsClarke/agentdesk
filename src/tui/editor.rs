//! A small multi-line text editor for replies and new posts.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Default, Clone)]
pub struct Editor {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize, // in chars
}

impl Editor {
    pub fn new() -> Editor {
        Editor { lines: vec![String::new()], row: 0, col: 0 }
    }
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
    pub fn is_blank(&self) -> bool {
        self.text().trim().is_empty()
    }
    fn byte(&self, row: usize, col: usize) -> usize {
        self.lines[row].char_indices().nth(col).map(|(i, _)| i).unwrap_or(self.lines[row].len())
    }
    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            if ch == '\n' || ch == '\r' {
                self.newline();
            } else {
                let b = self.byte(self.row, self.col);
                self.lines[self.row].insert(b, ch);
                self.col += 1;
            }
        }
    }
    fn newline(&mut self) {
        let b = self.byte(self.row, self.col);
        let rest = self.lines[self.row].split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }
    pub fn key(&mut self, k: KeyEvent) {
        let len = |l: &String| l.chars().count();
        match k.code {
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => self.insert_str(&c.to_string()),
            KeyCode::Enter => self.newline(),
            KeyCode::Backspace => {
                if self.col > 0 {
                    let b = self.byte(self.row, self.col - 1);
                    self.lines[self.row].remove(b);
                    self.col -= 1;
                } else if self.row > 0 {
                    let cur = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = len(&self.lines[self.row]);
                    self.lines[self.row].push_str(&cur);
                }
            }
            KeyCode::Delete => {
                if self.col < len(&self.lines[self.row]) {
                    let b = self.byte(self.row, self.col);
                    self.lines[self.row].remove(b);
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&next);
                }
            }
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = len(&self.lines[self.row]);
                }
            }
            KeyCode::Right => {
                if self.col < len(&self.lines[self.row]) {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.col = self.col.min(len(&self.lines[self.row]));
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = self.col.min(len(&self.lines[self.row]));
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = len(&self.lines[self.row]),
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.lines[self.row].clear();
                self.col = 0;
            }
            _ => {}
        }
    }
}
