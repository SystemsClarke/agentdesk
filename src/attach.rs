//! The attach endpoint: lets another process (`agentdesk attach <name>`, the Agents screen) look into and type into a session the
//! supervisor owns. The supervisor listens on 127.0.0.1 at a random port and writes `supervisor.addr` in the data folder:
//! one line, `<port> <key>`.
//!
//! Protocol (all lines end in `\n`). The client opens a TCP connection and sends
//! `ATTACH <key> <name> <cols> <rows>`. The server answers `OK` or `ERR <why>` and closes on an error. After `OK` the server
//! sends raw terminal output (the recent output first, then everything new) until the session ends or the client leaves.
//! The client sends lines: `INPUT <base64>` (bytes for the terminal), `RESIZE <cols> <rows>`, `DETACH`.
//! A connection may instead open with `LIST <key>`: the server answers one session name per line, then `END`, and closes.
//! This mirrors `ui:attach` / `ui:input` / `ui:resize` of the .NET build's UI API.

use crate::paths;
use crate::sessions::Registry;
use anyhow::Result;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

pub fn addr_file() -> PathBuf {
    paths::data_dir().join("supervisor.addr")
}

/// `(port, key)` of the running supervisor's attach endpoint, if its file is there (it may be stale: connecting tells).
#[allow(dead_code)] // the client (`agentdesk attach`) reads it
pub fn read_addr() -> Option<(u16, String)> {
    let text = std::fs::read_to_string(addr_file()).ok()?;
    let mut it = text.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.to_string()))
}

/// Listens for attach clients in the background; the address file is removed when the returned guard is dropped.
pub struct Server;

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(addr_file());
    }
}

pub fn start(registry: Registry) -> Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let key = random_key();
    let file = addr_file();
    std::fs::create_dir_all(paths::data_dir())?;
    std::fs::write(&file, format!("{port} {key}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (registry, key) = (registry.clone(), key.clone());
            std::thread::spawn(move || {
                let _ = serve(conn, &registry, &key);
            });
        }
    });
    Ok(Server)
}

fn random_key() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the operating system's randomness; four of them make 256 bits.
    (0..4)
        .map(|i| {
            let mut h = RandomState::new().build_hasher();
            h.write_u32(i);
            format!("{:016x}", h.finish())
        })
        .collect()
}

fn serve(conn: TcpStream, registry: &Registry, key: &str) -> Result<()> {
    conn.set_nodelay(true).ok();
    let mut out = conn.try_clone()?;
    let mut input = BufReader::new(conn.try_clone()?);
    let mut first = String::new();
    // A client that does not speak within 5 seconds is dropped.
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    input.read_line(&mut first)?;
    conn.set_read_timeout(None)?;
    let parts: Vec<&str> = first.split_whitespace().collect();
    if let ["LIST", k] = parts.as_slice() {
        if !constant_eq(k, key) {
            return deny(&mut out, "bad key");
        }
        let mut names: Vec<String> = registry.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
        names.sort_by_key(|n| (n.starts_with("plain:"), n.to_lowercase()));
        for n in names {
            writeln!(out, "{n}")?;
        }
        writeln!(out, "END")?;
        return Ok(());
    }
    let (name, cols, rows) = match parts.as_slice() {
        ["ATTACH", k, name, cols, rows] if constant_eq(k, key) => (name.to_string(), cols.parse().unwrap_or(160u16), rows.parse().unwrap_or(40u16)),
        ["ATTACH", ..] => return deny(&mut out, "bad key or arguments (ATTACH <key> <name> <cols> <rows>)"),
        _ => return deny(&mut out, "expected ATTACH <key> <name> <cols> <rows>"),
    };
    let shared = registry.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)).map(|(_, s)| s.clone());
    let Some(shared) = shared else { return deny(&mut out, &format!("{name} is not running")) };
    shared.resize(cols, rows);
    let (tail, rx) = shared.subscribe();
    out.write_all(b"OK\n")?;
    out.write_all(&tail)?;
    out.flush()?;
    // Output down, on its own thread: it ends when the session does (the sender is dropped) or the client goes.
    let mut down = out.try_clone()?;
    std::thread::spawn(move || {
        for chunk in rx {
            if down.write_all(&chunk).and_then(|_| down.flush()).is_err() {
                break;
            }
        }
        let _ = down.shutdown(std::net::Shutdown::Both);
    });
    for line in input.lines() {
        let line = line?;
        let mut it = line.split_whitespace();
        match it.next() {
            Some("INPUT") => {
                if let Some(bytes) = it.next().and_then(base64_decode) {
                    shared.write(&bytes)?;
                }
            }
            Some("RESIZE") => {
                if let (Some(c), Some(r)) = (it.next().and_then(|v| v.parse().ok()), it.next().and_then(|v| v.parse().ok())) {
                    shared.resize(c, r);
                }
            }
            Some("DETACH") => break,
            _ => {}
        }
    }
    let _ = out.shutdown(std::net::Shutdown::Both);
    Ok(())
}

fn deny(out: &mut TcpStream, why: &str) -> Result<()> {
    out.write_all(format!("ERR {why}\n").as_bytes())?;
    Ok(())
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[allow(dead_code)] // the client encodes keystrokes with it
pub fn base64_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0u32);
    for b in text.bytes().filter(|b| *b != b'=') {
        let v = B64.iter().position(|c| *c == b)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The bytes a key press sends to a terminal.
fn key_bytes(k: &ratatui::crossterm::event::KeyEvent) -> Vec<u8> {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => vec![(c.to_ascii_lowercase() as u8) - b'a' + 1],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => vec![],
    }
}

/// The running sessions the supervisor hosts, in the order the switcher walks them (identities first, then plain sessions).
pub fn list_sessions() -> Vec<String> {
    let Some((port, key)) = read_addr() else { return vec![] };
    let Ok(mut conn) = TcpStream::connect(("127.0.0.1", port)) else { return vec![] };
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    if writeln!(conn, "LIST {key}").is_err() {
        return vec![];
    }
    BufReader::new(conn).lines().map_while(|l| l.ok()).take_while(|l| l != "END").filter(|l| !l.starts_with("ERR")).collect()
}

/// Mouse reporting modes a session may switch on. While one is on, the terminal hands every click and drag to the session instead of
/// selecting text, which is why copying from an attached session fails; the attach client drops these so the terminal keeps selecting.
const MOUSE_MODES: [&[u8]; 6] = [b"1000", b"1002", b"1003", b"1005", b"1006", b"1015"];

/// Removes "turn mouse reporting on" from terminal output (`ESC [ ? 1000 h` and the like, alone or in a list). `carry` holds the start of
/// a sequence that was cut by the end of the last chunk. Everything else passes through untouched.
pub fn strip_mouse_on(carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<u8> {
    let mut data = std::mem::take(carry);
    data.extend_from_slice(chunk);
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            out.push(data[i]);
            i += 1;
            continue;
        }
        // a private-mode sequence: ESC [ ? <params> (h|l)
        let rest = &data[i..];
        if rest.len() < 3 {
            *carry = rest.to_vec();
            return out;
        }
        if rest[1] != b'[' || rest[2] != b'?' {
            out.push(data[i]);
            i += 1;
            continue;
        }
        let end = rest[3..].iter().position(|b| !(b.is_ascii_digit() || *b == b';'));
        let Some(end) = end else {
            if rest.len() < 24 {
                *carry = rest.to_vec(); // may still be growing
                return out;
            }
            out.push(data[i]);
            i += 1;
            continue;
        };
        let (params, fin) = (&rest[3..3 + end], rest[3 + end]);
        let total = 3 + end + 1;
        if fin == b'h' {
            let keep: Vec<&[u8]> = params.split(|b| *b == b';').filter(|p| !MOUSE_MODES.contains(p)).collect();
            if keep.len() == params.split(|b| *b == b';').count() {
                out.extend_from_slice(&rest[..total]);
            } else if !keep.is_empty() {
                out.extend_from_slice(b"\x1b[?");
                out.extend_from_slice(&keep.join(&b';'));
                out.push(b'h');
            }
        } else {
            out.extend_from_slice(&rest[..total]);
        }
        i += total;
    }
    out
}

/// What to type into a session for pasted text: wrapped in bracketed-paste markers when the session turned that mode on (so a pasted
/// newline is text, not Enter), otherwise plain text with newlines as Enter.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\n");
    if bracketed {
        let mut v = b"\x1b[200~".to_vec();
        v.extend_from_slice(text.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        text.replace('\n', "\r").into_bytes()
    }
}

/// One attached session: the socket, and the thread copying its output to this console.
struct Link {
    name: String,
    conn: TcpStream,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The session turned bracketed paste on (see `paste_bytes`).
    bracketed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    out: Option<std::thread::JoinHandle<()>>,
}

impl Link {
    fn open(port: u16, key: &str, name: &str, cols: u16, rows: u16) -> Option<Link> {
        for target in [name.to_string(), format!("plain:{name}")] {
            let Ok(mut conn) = TcpStream::connect(("127.0.0.1", port)) else { continue };
            if writeln!(conn, "ATTACH {key} {target} {cols} {rows}").is_err() {
                continue;
            }
            let Ok(clone) = conn.try_clone() else { continue };
            let mut reader = BufReader::new(clone);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.trim() != "OK" {
                continue;
            }
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let d2 = done.clone();
            let bracketed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let b2 = bracketed.clone();
            let keep_mouse = std::env::var("AGENTDESK_ATTACH_MOUSE").is_ok_and(|v| v == "1");
            let out = std::thread::spawn(move || {
                let mut stdout = std::io::stdout();
                let mut buf = [0u8; 8192];
                let mut carry = Vec::new();
                while let Ok(n) = std::io::Read::read(&mut reader, &mut buf) {
                    if n == 0 {
                        break;
                    }
                    let chunk = &buf[..n];
                    if let Some(at) = chunk.windows(8).rposition(|w| w == b"\x1b[?2004h" || w == b"\x1b[?2004l") {
                        b2.store(chunk[at + 7] == b'h', std::sync::atomic::Ordering::Relaxed);
                    }
                    let shown = if keep_mouse { chunk.to_vec() } else { strip_mouse_on(&mut carry, chunk) };
                    let _ = stdout.write_all(&shown);
                    let _ = stdout.flush();
                }
                d2.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            return Some(Link { name: name.to_string(), conn, done, bracketed, out: Some(out) });
        }
        None
    }
    fn ended(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn close(mut self) {
        let _ = writeln!(self.conn, "DETACH");
        let _ = self.conn.shutdown(std::net::Shutdown::Both);
        if let Some(h) = self.out.take() {
            let _ = h.join();
        }
    }
}

/// What the switcher does with the key pressed after the prefix (Ctrl+]).
#[derive(Debug, PartialEq)]
pub enum Switch {
    Next,
    Prev,
    Jump(usize),
    Window,
    Cancel,
}

pub fn switch_key(code: ratatui::crossterm::event::KeyCode, ctrl: bool) -> Switch {
    use ratatui::crossterm::event::KeyCode::*;
    match code {
        Char('n') | Char('N') | Right | Down | Tab => Switch::Next,
        Char('p') | Char('P') | Left | Up | BackTab => Switch::Prev,
        Char(d) if d.is_ascii_digit() && d != '0' => Switch::Jump(d as usize - '1' as usize),
        Char(']') if ctrl => Switch::Window,
        Char('d') | Char('D') | Char('w') | Char('W') | Char(']') | Char('l') | Char('L') | Enter | Esc => Switch::Window,
        _ => Switch::Cancel,
    }
}

/// The session `step` places away from `current` in `names` (wrapping), or None when there is nowhere else to go.
pub fn neighbour(names: &[String], current: &str, step: isize) -> Option<String> {
    let others = names.iter().filter(|n| !n.starts_with("plain:") || names.len() < 40).count();
    if others < 2 {
        return None;
    }
    let at = names.iter().position(|n| n.eq_ignore_ascii_case(current) || n.eq_ignore_ascii_case(&format!("plain:{current}")))? as isize;
    let len = names.len() as isize;
    let n = &names[(((at + step) % len + len) % len) as usize];
    Some(n.trim_start_matches("plain:").to_string())
}

fn hint(rows: u16, cols: u16, current: &str, names: &[String]) {
    let at = names.iter().position(|n| n.trim_start_matches("plain:").eq_ignore_ascii_case(current)).map(|i| i + 1).unwrap_or(0);
    let mut text = format!(" agentdesk | {current} {at}/{} | n next  p prev  1-9 jump  d back to the window ", names.len());
    text.truncate(cols.saturating_sub(1) as usize);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b7\x1b[{rows};1H\x1b[7m{text}\x1b[0m\x1b8");
    let _ = out.flush();
}

/// A full repaint of whatever the session is showing: a resize that changes and changes back (the session redraws on each).
fn repaint(link: &mut Link, cols: u16, rows: u16) {
    let _ = writeln!(link.conn, "RESIZE {cols} {}", rows.saturating_sub(1).max(1));
    let _ = writeln!(link.conn, "RESIZE {cols} {rows}");
}

/// `agentdesk attach <name>`: uses a session the supervisor hosts from this console.
///
/// Ctrl+] is the prefix. After it: `n`/`p` (or the arrow keys, Tab) switch to the next or previous running session without going back to
/// the window, `1`-`9` jump, `d` (or Ctrl+] again, Enter, Esc, or waiting a moment) detaches to the window. The sessions keep running.
pub fn client(name: &str) -> Result<()> {
    use ratatui::crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal,
    };
    let Some((port, key)) = read_addr() else { anyhow::bail!("no supervisor is running: start `agentdesk daemon` (or the window), which owns the sessions") };
    let (mut cols, mut rows) = terminal::size().unwrap_or((120, 30));
    let Some(first) = Link::open(port, &key, name, cols, rows) else { anyhow::bail!("no such session: {name} (is it running? `agentdesk list`)") };
    let mut link = first;
    terminal::enable_raw_mode()?;
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::EnableBracketedPaste);
    let title = |n: &str| {
        let mut o = std::io::stdout();
        let _ = write!(o, "\x1b]0;agentdesk: {n}\x07");
        let _ = o.flush();
    };
    title(&link.name);
    let mut prefix: Option<std::time::Instant> = None;
    let mut ended_session = false;
    let result = (|| -> Result<()> {
        loop {
            if link.ended() {
                ended_session = true;
                break;
            }
            // the prefix was pressed and nothing followed: back to the window, as Ctrl+] always did
            if prefix.is_some_and(|t| t.elapsed() > std::time::Duration::from_millis(1500)) {
                break;
            }
            if !event::poll(std::time::Duration::from_millis(100))? {
                continue;
            }
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    if prefix.take().is_some() {
                        let names = list_sessions();
                        let target = match switch_key(k.code, ctrl) {
                            Switch::Window => break,
                            Switch::Cancel => None,
                            Switch::Next => neighbour(&names, &link.name, 1),
                            Switch::Prev => neighbour(&names, &link.name, -1),
                            Switch::Jump(i) => names.get(i).map(|n| n.trim_start_matches("plain:").to_string()),
                        };
                        match target {
                            Some(t) if !t.eq_ignore_ascii_case(&link.name) => {
                                if let Some(next) = Link::open(port, &key, &t, cols, rows) {
                                    let old = std::mem::replace(&mut link, next);
                                    old.close();
                                    let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
                                    title(&link.name);
                                    repaint(&mut link, cols, rows);
                                } else {
                                    repaint(&mut link, cols, rows);
                                }
                            }
                            _ => repaint(&mut link, cols, rows),
                        }
                        continue;
                    }
                    if k.code == KeyCode::Char(']') && ctrl {
                        prefix = Some(std::time::Instant::now());
                        hint(rows, cols, &link.name, &list_sessions());
                        continue;
                    }
                    let b = key_bytes(&k);
                    if !b.is_empty() {
                        writeln!(link.conn, "INPUT {}", b64(&b))?;
                    }
                }
                Event::Paste(text) => {
                    let b = paste_bytes(&text, link.bracketed.load(std::sync::atomic::Ordering::Relaxed));
                    writeln!(link.conn, "INPUT {}", b64(&b))?;
                }
                Event::Resize(c, r) => {
                    (cols, rows) = (c, r);
                    writeln!(link.conn, "RESIZE {c} {r}")?;
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableBracketedPaste);
    let _ = terminal::disable_raw_mode();
    println!();
    let name = link.name.clone();
    link.close();
    if ended_session {
        println!("[the session ended]");
    } else {
        println!("[detached; {name} keeps running]");
    }
    result
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn mouse_reporting_is_dropped_and_everything_else_kept() {
        let mut c = Vec::new();
        assert_eq!(strip_mouse_on(&mut c, b"a\x1b[?1000hb\x1b[?1006hc"), b"abc");
        assert_eq!(strip_mouse_on(&mut c, b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[31mred"), b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[31mred");
        assert_eq!(strip_mouse_on(&mut c, b"\x1b[?1049;1002;1006h"), b"\x1b[?1049h", "a list keeps the other modes");
        assert_eq!(strip_mouse_on(&mut c, b"\x1b[?1000l"), b"\x1b[?1000l", "turning it off passes");
        // a sequence cut between chunks
        let a = strip_mouse_on(&mut c, b"x\x1b[?10");
        let b = strip_mouse_on(&mut c, b"00hy");
        assert_eq!([a, b].concat(), b"xy");
    }

    #[test]
    fn pasted_text_is_text_not_enter() {
        assert_eq!(paste_bytes("a\r\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste_bytes("a\nb", false), b"a\rb");
    }

    #[test]
    fn the_key_after_the_prefix_switches_or_leaves() {
        assert_eq!(switch_key(KeyCode::Char('n'), false), Switch::Next);
        assert_eq!(switch_key(KeyCode::Left, false), Switch::Prev);
        assert_eq!(switch_key(KeyCode::Char('3'), false), Switch::Jump(2));
        assert_eq!(switch_key(KeyCode::Char(']'), true), Switch::Window);
        assert_eq!(switch_key(KeyCode::Char('d'), false), Switch::Window);
        assert_eq!(switch_key(KeyCode::Char('x'), false), Switch::Cancel);
    }

    #[test]
    fn the_server_lists_its_sessions_and_refuses_a_bad_key() {
        let _e = crate::tests_identities::Env::new();
        let registry: crate::sessions::Registry = Default::default();
        let _server = start(registry).unwrap();
        assert!(list_sessions().is_empty(), "no sessions: just END");
        let (port, _key) = read_addr().unwrap();
        let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
        writeln!(conn, "LIST wrong-key").unwrap();
        let mut line = String::new();
        BufReader::new(conn).read_line(&mut line).unwrap();
        assert!(line.starts_with("ERR"), "{line}");
    }

    #[test]
    fn the_switcher_wraps_and_has_nowhere_to_go_alone() {
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(neighbour(&names, "a", 1).as_deref(), Some("b"));
        assert_eq!(neighbour(&names, "a", -1).as_deref(), Some("c"));
        assert_eq!(neighbour(&names, "C", 1).as_deref(), Some("a"));
        assert_eq!(neighbour(&names[..1], "a", 1), None);
        assert_eq!(neighbour(&names, "zzz", 1), None);
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(super::b64(b"ping\r"), "cGluZw0=");
        assert_eq!(super::b64(b"a"), "YQ==");
        assert_eq!(super::b64(b"abc"), "YWJj");
    }
}
