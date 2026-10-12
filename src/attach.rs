//! The attach endpoint: lets another process (`agentdesk attach <name>`, the Agents screen) look into and type into a session the
//! supervisor owns. The supervisor listens on 127.0.0.1 at a random port and writes `supervisor.addr` in the data folder:
//! one line, `<port> <key>`.
//!
//! Protocol (all lines end in `\n`). The client opens a TCP connection and sends
//! `ATTACH <key> <name> <cols> <rows>`. The server answers `OK` or `ERR <why>` and closes on an error. After `OK` the server
//! sends raw terminal output (the recent output first, then everything new) until the session ends or the client leaves.
//! The client sends lines: `INPUT <base64>` (bytes for the terminal), `RESIZE <cols> <rows>`, `DETACH`.
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

/// `agentdesk attach <name>`: uses a session the supervisor hosts from this console. Ctrl+] detaches; the session keeps running.
pub fn client(name: &str) -> Result<()> {
    use ratatui::crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal,
    };
    let Some((port, key)) = read_addr() else { anyhow::bail!("no supervisor is running: start `agentdesk daemon` (or the window), which owns the sessions") };
    let (cols, rows) = terminal::size().unwrap_or((120, 30));
    let mut chosen = None;
    for target in [name.to_string(), format!("plain:{name}")] {
        let mut conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
        writeln!(conn, "ATTACH {key} {target} {cols} {rows}")?;
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim() == "OK" {
            chosen = Some((conn, reader));
            break;
        }
    }
    let Some((mut conn, mut reader)) = chosen else { anyhow::bail!("no such session: {name} (is it running? `agentdesk list`)") };
    terminal::enable_raw_mode()?;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d2 = done.clone();
    let out = std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 8192];
        while let Ok(n) = std::io::Read::read(&mut reader, &mut buf) {
            if n == 0 {
                break;
            }
            let _ = stdout.write_all(&buf[..n]);
            let _ = stdout.flush();
        }
        d2.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let result = (|| -> Result<()> {
        while !done.load(std::sync::atomic::Ordering::Relaxed) {
            if event::poll(std::time::Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if k.code == KeyCode::Char(']') && k.modifiers.contains(KeyModifiers::CONTROL) {
                            writeln!(conn, "DETACH")?;
                            break;
                        }
                        let b = key_bytes(&k);
                        if !b.is_empty() {
                            writeln!(conn, "INPUT {}", b64(&b))?;
                        }
                    }
                    Event::Resize(c, r) => writeln!(conn, "RESIZE {c} {r}")?,
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    let _ = terminal::disable_raw_mode();
    println!();
    drop(conn);
    if done.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = out.join();
        println!("[the session ended]");
    } else {
        println!("[detached; {name} keeps running]");
    }
    result
}

#[cfg(test)]
mod client_tests {
    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(super::b64(b"ping\r"), "cGluZw0=");
        assert_eq!(super::b64(b"a"), "YQ==");
        assert_eq!(super::b64(b"abc"), "YWJj");
    }
}
