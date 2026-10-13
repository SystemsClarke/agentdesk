//! Styled text the BBS screens are made of: a run of text and its space-separated tags (the original's vocabulary).
//! Colour tags: fg mu fa rule pk or ye gr cy pu. Modifiers: b i u s. Specials: bar barcy inv cur rcpt.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq)]
pub struct Seg {
    pub text: String,
    pub tags: String,
}

pub type L = Vec<Seg>;

pub fn s(text: impl Into<String>, tags: &str) -> Seg {
    Seg { text: text.into(), tags: tags.to_string() }
}

pub fn plain(text: impl Into<String>) -> Seg {
    s(text, "")
}

pub fn width(segs: &[Seg]) -> usize {
    segs.iter().map(|x| x.text.width()).sum()
}

pub fn rep(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

/// Cuts or pads to exactly `n` columns, collapsing whitespace like the original's Fit.
pub fn fit(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.width() > n {
        let mut out = String::new();
        for ch in t.chars() {
            if out.width() + ch.to_string().width() > n - 1 {
                break;
            }
            out.push(ch);
        }
        out.push('…');
        out
    } else {
        format!("{t}{}", rep(' ', n - t.width()))
    }
}

pub fn pad(mut segs: L, w: usize, tags: &str) -> L {
    let cur = width(&segs);
    if w > cur {
        segs.push(s(rep(' ', w - cur), tags));
    }
    segs
}

pub fn clip_to(segs: L, w: usize) -> L {
    let mut out: L = Vec::new();
    let mut used = 0;
    for sg in segs {
        let sw = sg.text.width();
        if used + sw > w.saturating_sub(1) {
            let keep: String = sg
                .text
                .chars()
                .scan(0, |acc, c| {
                    *acc += c.to_string().width();
                    Some((*acc, c))
                })
                .take_while(|(acc, _)| used + acc <= w.saturating_sub(2))
                .map(|(_, c)| c)
                .collect();
            out.push(Seg { text: format!("{keep}…"), tags: sg.tags });
            return out;
        }
        used += sw;
        out.push(sg);
    }
    out
}

pub fn n_word(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n != 1 { "s" } else { "" })
}

pub fn hue(author: &str) -> &'static str {
    const HUES: [&str; 4] = ["cy", "pu", "gr", "or"];
    if author == "john" {
        "ye"
    } else {
        HUES[(author.chars().map(|c| c as u32).sum::<u32>() % 4) as usize]
    }
}

pub struct Palette {
    pub label: &'static str,
    pub mono: bool,
    pub colors: [Color; 15],
}

fn hex(h: &str) -> Color {
    let v = u32::from_str_radix(h.trim_start_matches('#'), 16).unwrap_or(0);
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn pal(label: &'static str, mono: bool, colors: &str) -> Palette {
    let v: Vec<Color> = colors.split(' ').map(hex).collect();
    Palette { label, mono, colors: v.try_into().unwrap() }
}

/// bg panel line fg mu fa rule pk or ye gr cy pu on_bar textsel: the original's four themes.
pub fn palettes() -> Vec<(&'static str, Palette)> {
    vec![
        ("monokai-pro", pal("Monokai Pro", false, "#221f22 #2d2a2e #403e41 #fcfcfa #939293 #727072 #5b595c #ff6188 #fc9867 #ffd866 #a9dc76 #78dce8 #ab9df2 #221f22 #5b595c")),
        (
            "monokai-pro-light",
            pal("Monokai Pro Light", false, "#faf4f2 #ede7e5 #e0dad9 #29242a #706b6e #918c8e #d3cdcc #e14775 #e16032 #cc7a0a #269d69 #1c8ca8 #7058be #faf4f2 #d3cdcc"),
        ),
        ("grayscale", pal("Grayscale (paper)", true, "#ffffff #eeeeee #d4d4d4 #000000 #3a3a3a #595959 #8c8c8c #000000 #000000 #000000 #000000 #000000 #000000 #ffffff #b8b8b8")),
        ("grayscale-dark", pal("Grayscale (ink)", true, "#000000 #121212 #2e2e2e #ffffff #c4c4c4 #9e9e9e #6e6e6e #ffffff #ffffff #ffffff #ffffff #ffffff #ffffff #000000 #5c5c5c")),
    ]
}

const KEYS: [&str; 15] = ["bg", "panel", "line", "fg", "mu", "fa", "rule", "pk", "or", "ye", "gr", "cy", "pu", "on_bar", "textsel"];

impl Palette {
    pub fn color(&self, key: &str) -> Color {
        KEYS.iter().position(|k| *k == key).map(|i| self.colors[i]).unwrap_or(self.colors[3])
    }

    /// Tag priority follows the original: rcpt over cur over inv over the bars over a plain colour.
    pub fn style(&self, tags: &str) -> Style {
        let t: Vec<&str> = tags.split(' ').filter(|x| !x.is_empty()).collect();
        let has = |x: &str| t.contains(&x);
        let fg_key = if has("rcpt") {
            Some("fa")
        } else if has("cur") || has("bar") || has("barcy") {
            Some("on_bar")
        } else {
            t.iter().find(|x| x.len() == 2 || **x == "rule").copied().filter(|x| !matches!(*x, "on"))
        };
        let mut mods = Modifier::empty();
        let mut bold = has("b") || has("cur") || has("bar") || has("barcy");
        if self.mono {
            match fg_key {
                Some("pk") => {
                    bold = true;
                    mods |= Modifier::UNDERLINED;
                }
                Some("pu") => mods |= Modifier::ITALIC,
                Some("or") | Some("ye") => bold = true,
                _ => {}
            }
        }
        let bg = if has("cur") {
            Some("ye")
        } else if has("inv") {
            Some("line")
        } else if has("barcy") {
            Some("cy")
        } else if has("bar") {
            Some("ye")
        } else if has("pnl") {
            Some("panel")
        } else {
            None
        };
        if bold {
            mods |= Modifier::BOLD;
        }
        if has("i") {
            mods |= Modifier::ITALIC;
        }
        if has("u") {
            mods |= Modifier::UNDERLINED;
        }
        if has("s") {
            mods |= Modifier::CROSSED_OUT;
        }
        let mut st = Style::default().fg(self.color(fg_key.unwrap_or("fg"))).bg(bg.map(|b| self.color(b)).unwrap_or(self.color("bg")));
        st = st.add_modifier(mods);
        st
    }

    pub fn line(&self, segs: &[Seg]) -> Line<'static> {
        Line::from(segs.iter().map(|x| Span::styled(x.text.clone(), self.style(&x.tags))).collect::<Vec<_>>())
    }
}
