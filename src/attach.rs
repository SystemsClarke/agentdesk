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

/// Types `text` into a running session through the supervisor's attach endpoint, then presses Enter 300 ms later (text and Enter in one
/// write read as a paste). For other processes (the Slack bridge) that do not own the session host.
pub fn type_into(name: &str, text: &str) -> Result<()> {
    let (port, key) = read_addr().ok_or_else(|| anyhow::anyhow!("no supervisor is running"))?;
    let mut conn = TcpStream::connect(("127.0.0.1", port))?;
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    conn.write_all(format!("ATTACH {key} {name} 160 40\n").as_bytes())?;
    let mut reply = String::new();
    BufReader::new(conn.try_clone()?).read_line(&mut reply)?;
    if !reply.starts_with("OK") {
        anyhow::bail!("{}", reply.trim().trim_start_matches("ERR "));
    }
    conn.write_all(format!("INPUT {}\n", base64_encode(text.as_bytes())).as_bytes())?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    conn.write_all(format!("INPUT {}\nDETACH\n", base64_encode(b"\r")).as_bytes())?;
    Ok(())
}
