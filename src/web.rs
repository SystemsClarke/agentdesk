//! The ops console: one page (`web.html`, the original's) and `POST /api/<op>`, served on `http://127.0.0.1:<port>` and nowhere else. The
//! port is last start's (`web.port` in the data folder) while it is free, else any free one. Every request needs the key, persistent in
//! `web.key` so a bookmark keeps working: `?k=<key>` once sets an HttpOnly SameSite=Strict cookie and redirects the key out of the address
//! bar. A Host header other than `127.0.0.1:<port>`, or a foreign Origin, is refused (DNS rebinding, cross-site posts); there is no CORS.
//! Plain HTTP/1.1 over a `TcpListener`, one request per connection.

use crate::{db::Db, identity::Caller, paths, ui_api, update};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;

const PAGE: &str = include_str!("web.html");
const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;

fn port_file() -> PathBuf {
    paths::data_dir().join("web.port")
}

fn key_file() -> PathBuf {
    paths::data_dir().join("web.key")
}

/// `http://127.0.0.1:<port>/?k=<key>`, or None when no console is serving.
pub fn url() -> Option<String> {
    let port = std::fs::read_to_string(port_file()).ok()?.trim().parse::<u16>().ok()?;
    let key = std::fs::read_to_string(key_file()).ok()?.trim().to_string();
    if key.is_empty() || TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), std::time::Duration::from_millis(200)).is_err() {
        return None;
    }
    Some(format!("http://127.0.0.1:{port}/?k={key}"))
}

fn b64url(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if c.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if c.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

/// The key in `web.key`, made (32 random bytes, 43 characters) the first time.
pub fn key() -> String {
    if let Ok(k) = std::fs::read_to_string(key_file()) {
        if k.trim().len() == 43 {
            return k.trim().to_string();
        }
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = Vec::new();
    for i in 0..4u32 {
        let mut h = RandomState::new().build_hasher();
        h.write_u32(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        bytes.extend_from_slice(&h.finish().to_le_bytes());
    }
    let key = b64url(&bytes);
    let _ = std::fs::create_dir_all(paths::data_dir());
    let _ = std::fs::write(key_file(), &key);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_file(), std::fs::Permissions::from_mode(0o600));
    }
    key
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Stops listening when dropped and removes `web.port`.
pub struct Server {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // wakes the accept
        let _ = std::fs::remove_file(port_file());
    }
}

/// Starts serving in the background; the console lives as long as the returned guard.
pub fn start() -> Option<Server> {
    if url().is_some() {
        return None; // another process (the window, or the daemon) is already serving it
    }
    let key = key();
    let last = std::fs::read_to_string(port_file()).ok().and_then(|t| t.trim().parse::<u16>().ok()).unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", last)).or_else(|_| TcpListener::bind(("127.0.0.1", 0))).ok()?;
    let port = listener.local_addr().ok()?.port();
    let _ = std::fs::write(port_file(), port.to_string());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let started = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    std::thread::spawn(move || {
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for conn in listener.incoming().flatten() {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if active.load(std::sync::atomic::Ordering::Relaxed) >= 32 {
                continue; // a flood is refused, not queued
            }
            let (key, started, active) = (key.clone(), started.clone(), active.clone());
            active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::thread::spawn(move || {
                let _ = serve(conn, port, &key, &started);
                active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            });
        }
    });
    Some(Server { stop, port })
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn read_request(conn: &mut TcpStream) -> Option<Request> {
    conn.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEADER {
            return None;
        }
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target, version) = (first.next()?.to_string(), first.next()?.to_string(), first.next()?);
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let mut headers = Vec::new();
    for l in lines {
        let (k, v) = l.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")) {
        return None; // fixed-length bodies only
    }
    let length = match headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
        Some((_, v)) => v.parse::<usize>().ok().filter(|l| *l <= MAX_BODY)?,
        None => 0,
    };
    let mut body = buf[end + 4..].to_vec();
    while body.len() < length {
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let (path, query) = target.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((target, String::new()));
    Some(Request { method, path, query, headers, body })
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            b'%' if s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()).is_some() => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

fn serve(mut conn: TcpStream, port: u16, key: &str, started: &str) -> std::io::Result<()> {
    let (status, headers, body) = match read_request(&mut conn) {
        None => (400, vec![], vec![]),
        Some(r) => route(&r, port, key, started),
    };
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    conn.write_all(head.as_bytes())?;
    conn.write_all(&body)
}

type Reply = (u16, Vec<(String, String)>, Vec<u8>);

fn route(r: &Request, port: u16, key: &str, started: &str) -> Reply {
    let host = format!("127.0.0.1:{port}");
    let origin = r.header("Origin").unwrap_or("");
    if r.header("Host") != Some(host.as_str()) || (!origin.is_empty() && origin != format!("http://{host}")) {
        return (400, vec![], vec![]);
    }
    let query_k = r.query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| unescape(k) == "k").map(|(_, v)| unescape(v));
    if query_k.as_deref().map(|k| same(k, key)).unwrap_or(false) {
        return (302, vec![("Location".into(), r.path.clone()), ("Set-Cookie".into(), format!("k={key}; max-age={}; path=/; samesite=strict; httponly", 400 * 86400))], vec![]);
    }
    let cookie = r.header("Cookie").unwrap_or("").split(';').filter_map(|p| p.trim().split_once('=')).find(|(k, _)| *k == "k").map(|(_, v)| v.to_string());
    if !cookie.as_deref().map(|c| same(c, key)).unwrap_or(false) {
        return (401, vec![], vec![]);
    }
    if r.path == "/" {
        return if r.method == "GET" {
            (
                200,
                vec![
                    ("Cache-Control".into(), "no-store".into()),
                    ("Content-Type".into(), "text/html; charset=utf-8".into()),
                    (
                        "Content-Security-Policy".into(),
                        "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'".into(),
                    ),
                    ("X-Content-Type-Options".into(), "nosniff".into()),
                ],
                PAGE.as_bytes().to_vec(),
            )
        } else {
            (405, vec![("Allow".into(), "GET".into())], vec![])
        };
    }
    if let Some(op) = r.path.strip_prefix("/api/").filter(|o| !o.is_empty() && !o.contains('/')) {
        if r.method != "POST" {
            return (405, vec![("Allow".into(), "POST".into())], vec![]);
        }
        let args: Value = if r.body.is_empty() { json!({}) } else { serde_json::from_slice(&r.body).unwrap_or_else(|_| json!({})) };
        let text = call(&unescape(op), &args, started);
        return (200, vec![("Cache-Control".into(), "no-store".into()), ("Content-Type".into(), "application/json; charset=utf-8".into())], text.into_bytes());
    }
    (404, vec![], vec![])
}

/// The requests the console's page needs, answered by the same objects as the window's. It is `web`, not John: no session, no identity.
fn call(op: &str, args: &Value, started: &str) -> String {
    let doc = (|| -> anyhow::Result<Value> {
        let db = Db::open(&paths::db_path())?;
        db.init()?;
        let c = Caller { session_id: None, env_author: None, cwd: None, harness: Some("web".into()), pid: 0 };
        match op {
            "core" => Ok(
                json!({"pid": std::process::id(), "started": started, "update": {"installed": true, "current": update::CURRENT, "pending": update::pending(), "latest": update::pending()},
                "theme": std::fs::read_to_string(paths::data_dir().join("settings.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["theme"].as_str().map(String::from))}),
            ),
            "update" => ui_api::call(&db, &c, "ui:update", args),
            "open_questions" => ui_api::call(&db, &c, "open_questions", &json!({})),
            "status" | "job_list" | "concierge" | "governor" | "governor_enforce" | "session_list" | "log_tail" | "identity_list" | "identity_create" | "identity_start"
            | "identity_stop" | "identity_forget" => ui_api::call(&db, &c, op, args),
            _ => crate::db::err(format!("unknown request: {op}")),
        }
    })();
    serde_json::to_string_pretty(&doc.unwrap_or_else(|e| json!({"error": crate::db::anyhow_msg(&e)}))).unwrap_or_default()
}
