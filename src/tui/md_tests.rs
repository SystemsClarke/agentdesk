use super::*;

/// The renderer as it was before it walked bytes: the new one must give the same segments for any line.
fn inline_old(text: &str, base: &str) -> L {
    let chars: Vec<char> = text.chars().collect();
    let mut out: L = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, out: &mut L| {
        if !buf.is_empty() {
            out.push(s(std::mem::take(buf), base));
        }
    };
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let mut handled = false;
        for (m, tag) in [("**", "b"), ("~~", "s")] {
            if rest.starts_with(m) {
                if let Some(end) = rest[2..].find(m) {
                    flush(&mut buf, &mut out);
                    out.push(s(rest[2..2 + end].to_string(), &with(base, tag)));
                    i += rest[..2 + end + 2].chars().count();
                    handled = true;
                    break;
                }
            }
        }
        if handled {
            continue;
        }
        if chars[i] == '`' {
            if let Some(end) = rest[1..].find('`') {
                flush(&mut buf, &mut out);
                out.push(s(rest[1..1 + end].to_string(), "cy"));
                i += rest[..1 + end + 1].chars().count();
                continue;
            }
        }
        if chars[i] == '*' && chars.get(i + 1).map(|c| !c.is_whitespace() && *c != '*').unwrap_or(false) {
            if let Some(end) = rest[1..].find('*') {
                flush(&mut buf, &mut out);
                out.push(s(rest[1..1 + end].to_string(), &with(base, "i")));
                i += rest[..1 + end + 1].chars().count();
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some(close) = rest.find("](") {
                if let Some(paren) = rest[close..].find(')') {
                    flush(&mut buf, &mut out);
                    out.push(s(rest[1..close].to_string(), "cy u"));
                    i += rest[..close + paren + 1].chars().count();
                    continue;
                }
            }
        }
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let url: String = rest.chars().take_while(|c| !c.is_whitespace() && !matches!(c, ')' | '>' | '"')).collect();
            let url = url.trim_end_matches(['.', ',', ';']).to_string();
            flush(&mut buf, &mut out);
            i += url.chars().count();
            out.push(s(url, "cy u"));
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut out);
    out
}

#[test]
fn inline_gives_what_it_always_did() {
    let pieces = ["a", " ", "**", "*", "`", "~~", "[", "](", ")", "x y", "é", "日本", "https://a.b/c", "http://", ".", ",", "\"", ">", "_", "#", "go to ", "done"];
    let mut seed: u64 = 0x2545F4914F6CDD1D;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..4000 {
        let mut line = String::new();
        for _ in 0..(next() % 14 + 1) {
            line.push_str(pieces[(next() % pieces.len() as u64) as usize]);
        }
        assert_eq!(inline(&line, "fg"), inline_old(&line, "fg"), "line: {line:?}");
    }
}

#[test]
fn a_long_paragraph_renders_in_linear_time() {
    let para = "word **bold** and `code` and https://example.com/x, [link](http://a.b) ".repeat(400);
    let t = std::time::Instant::now();
    let lines = render(&para, 100);
    assert!(!lines.is_empty());
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "took {:?}", t.elapsed());
}
