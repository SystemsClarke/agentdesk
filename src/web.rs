//! The ops console: one page (`web.html`, the original's) and `POST /api/<op>`, served on `http://127.0.0.1:<port>` and nowhere else. The
//! port is last start's (`web.port` in the data folder) while it is free, else any free one. Every request needs the key, persistent in
//! `web.key` so a bookmark keeps working: `?k=<key>` once sets an HttpOnly SameSite=Strict cookie and redirects the key out of the address
//! bar. A Host header other than `127.0.0.1:<port>`, or a foreign Origin, is refused (DNS rebinding, cross-site posts); there is no CORS.
//! Plain HTTP/1.1 over a `TcpListener`, one request per connection; `/events/stream` (the event log as Server-Sent Events) keeps its
//! connection open.

use crate::{db::Db, identity::Caller, kinds, packet_page, paths, ui_api, update};
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

/// `http://127.0.0.1:<port>/packet/<id>?k=<key>`: a packet's page, or None when no console is serving.
pub fn packet_url(id: i64) -> Option<String> {
    let u = url()?;
    let (base, key) = u.split_once("/?k=")?;
    Some(format!("{base}/packet/{id}?k={key}"))
}

fn b64url(bytes: &[u8]) -> String {
    crate::attach::base64_encode(bytes).trim_end_matches('=').replace('+', "-").replace('/', "_")
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
            let (key, started, active, stop) = (key.clone(), started.clone(), active.clone(), flag.clone());
            active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::thread::spawn(move || {
                let _ = serve(conn, port, &key, &started, &stop);
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

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// Live event streams open now: each holds a connection and a thread, so there are at most `MAX_STREAMS`.
static STREAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_STREAMS: usize = 8;

/// `?name=<integer>` from the query string.
fn query_i64(r: &Request, name: &str) -> Option<i64> {
    r.query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == name).and_then(|(_, v)| v.parse::<i64>().ok())
}

fn serve(mut conn: TcpStream, port: u16, key: &str, started: &str, stop: &std::sync::atomic::AtomicBool) -> std::io::Result<()> {
    let (status, headers, body) = match read_request(&mut conn) {
        None => (400, vec![], vec![]),
        Some(r) if r.path == "/events/stream" && r.method == "GET" => match gate(&r, port, key) {
            Some(refused) => refused,
            None if STREAMS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_STREAMS => {
                STREAMS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                json_reply(503, json!({"error": "too many live streams"}))
            }
            None => {
                let done = stream(&mut conn, query_i64(&r, "after"), stop);
                STREAMS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return done;
            }
        },
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

/// `GET /events/stream?after=<seq>` as Server-Sent Events: every event past `after` (default: the newest now), one `id:`/`data:` pair
/// each, found by polling the log every 250 ms, and a `: ping` comment every 15 s. It ends when a write fails (the client left) or the
/// console stops.
fn stream(conn: &mut TcpStream, after: Option<i64>, stop: &std::sync::atomic::AtomicBool) -> std::io::Result<()> {
    let db = match Db::open(&paths::db_path()).and_then(|d| d.init().map(|_| d)) {
        Ok(d) => d,
        Err(_) => return conn.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    };
    conn.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    conn.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\nretry: 3000\n\n",
    )?;
    let mut last = after.unwrap_or_else(|| db.max_event_seq());
    let mut pinged = std::time::Instant::now();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        if db.max_event_seq() > last {
            for ev in db.events_after(last, 1000).unwrap_or_default() {
                last = ev["seq"].as_i64().unwrap_or(last);
                conn.write_all(format!("id: {last}\ndata: {ev}\n\n").as_bytes())?;
            }
        }
        if pinged.elapsed() >= std::time::Duration::from_secs(15) {
            conn.write_all(b": ping\n\n")?;
            pinged = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Ok(())
}

type Reply = (u16, Vec<(String, String)>, Vec<u8>);

/// The door every request goes through: the Host and Origin checks, then the key (`?k=` sets the cookie and redirects). None: let it in.
fn gate(r: &Request, port: u16, key: &str) -> Option<Reply> {
    let host = format!("127.0.0.1:{port}");
    let origin = r.header("Origin").unwrap_or("");
    if r.header("Host") != Some(host.as_str()) || (!origin.is_empty() && origin != format!("http://{host}")) {
        return Some((400, vec![], vec![]));
    }
    let query_k = r.query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| crate::links::percent_decode(k) == "k").map(|(_, v)| crate::links::percent_decode(v));
    if query_k.as_deref().map(|k| same(k, key)).unwrap_or(false) {
        return Some((
            302,
            vec![("Location".into(), r.path.clone()), ("Set-Cookie".into(), format!("k={key}; max-age={}; path=/; samesite=strict; httponly", 400 * 86400))],
            vec![],
        ));
    }
    let cookie = r.header("Cookie").unwrap_or("").split(';').filter_map(|p| p.trim().split_once('=')).find(|(k, _)| *k == "k").map(|(_, v)| v.to_string());
    if !cookie.as_deref().map(|c| same(c, key)).unwrap_or(false) {
        return Some((401, vec![], vec![]));
    }
    None
}

fn route(r: &Request, port: u16, key: &str, started: &str) -> Reply {
    if let Some(refused) = gate(r, port, key) {
        return refused;
    }
    if r.path == "/events" {
        if r.method != "GET" {
            return (405, vec![("Allow".into(), "GET".into())], vec![]);
        }
        let (after, limit) = (query_i64(r, "after").unwrap_or(0), query_i64(r, "limit").unwrap_or(200).clamp(1, 1000));
        return match Db::open(&paths::db_path()).and_then(|d| d.init().map(|_| d)).and_then(|d| d.events_after(after, limit)) {
            Ok(evs) => json_reply(200, Value::Array(evs)),
            Err(e) => json_reply(500, json!({"error": crate::db::anyhow_msg(&e)})),
        };
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
    if let Some(action) = r.path.strip_prefix("/dictate/") {
        return dictate_route(r, action);
    }
    if let Some(rest) = r.path.strip_prefix("/packet/") {
        return packet_route(r, rest);
    }
    if let Some(op) = r.path.strip_prefix("/api/").filter(|o| !o.is_empty() && !o.contains('/')) {
        if r.method != "POST" {
            return (405, vec![("Allow".into(), "POST".into())], vec![]);
        }
        let args: Value = if r.body.is_empty() { json!({}) } else { serde_json::from_slice(&r.body).unwrap_or_else(|_| json!({})) };
        let text = call(&crate::links::percent_decode(op), &args, started);
        return (200, vec![("Cache-Control".into(), "no-store".into()), ("Content-Type".into(), "application/json; charset=utf-8".into())], text.into_bytes());
    }
    (404, vec![], vec![])
}

/// `POST /dictate/start|poll|stop`: the window's local speech-to-text (`dictation.rs`, which this process owns), for a page that wants Ctrl+D.
/// Answered in the plugin's shape (state, text, error, device). JSON only, like every post.
fn dictate_route(r: &Request, action: &str) -> Reply {
    if r.method != "POST" {
        return (405, vec![("Allow".into(), "POST".into())], vec![]);
    }
    if !r.header("Content-Type").unwrap_or("").starts_with("application/json") {
        return json_reply(400, json!({"error": "a post must be JSON"}));
    }
    if !matches!(action, "start" | "poll" | "stop") {
        return (404, vec![], vec![]);
    }
    match crate::dictation::call(action) {
        Ok(v) => json_reply(200, v),
        Err(e) => json_reply(400, json!({"error": crate::db::anyhow_msg(&e)})),
    }
}

/// `?block=<16 hex digits>`: which command of a packet a poll is about.
fn block_of(r: &Request) -> Option<String> {
    let v = r.query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == "block").map(|(_, v)| v.to_string())?;
    (v.len() == 16 && v.bytes().all(|c| c.is_ascii_hexdigit())).then_some(v)
}

fn json_reply(status: u16, v: Value) -> Reply {
    (status, vec![("Cache-Control".into(), "no-store".into()), ("Content-Type".into(), "application/json; charset=utf-8".into())], v.to_string().into_bytes())
}

/// `/packet/<id>` (the page), `/packet/<id>/version` (what the page polls) and `POST /packet/<id>/step|note|finish` (what John does on it).
/// A post must be JSON: a plain form post from another page is refused (the cookie is SameSite=Strict as well).
fn packet_route(r: &Request, rest: &str) -> Reply {
    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
    let Ok(id) = id.parse::<i64>() else { return (404, vec![], vec![]) };
    let db = match Db::open(&paths::db_path()).and_then(|d| d.init().map(|_| d)) {
        Ok(d) => d,
        Err(e) => return json_reply(500, json!({"error": crate::db::anyhow_msg(&e)})),
    };
    let is_packet = |db: &Db| kinds::kind_of(db, id).map(|k| k == kinds::PACKET).unwrap_or(false);
    if !is_packet(&db) {
        return (404, vec![], b"not a packet".to_vec());
    }
    match (r.method.as_str(), action) {
        ("GET", "") => match packet_page::page(&db, id) {
            Ok(html) => (
                200,
                vec![
                    ("Cache-Control".into(), "no-store".into()),
                    ("Content-Type".into(), "text/html; charset=utf-8".into()),
                    (
                        "Content-Security-Policy".into(),
                        "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".into(),
                    ),
                    ("X-Content-Type-Options".into(), "nosniff".into()),
                    ("Referrer-Policy".into(), "no-referrer".into()),
                ],
                html.into_bytes(),
            ),
            Err(e) => json_reply(500, json!({"error": crate::db::anyhow_msg(&e)})),
        },
        ("GET", "run") => match block_of(r) {
            Some(h) => json_reply(200, packet_page::run_poll(id, &h)),
            None => json_reply(400, json!({"error": "block is required"})),
        },
        ("GET", "version") => match packet_page::version(&db, id) {
            Ok(v) => json_reply(200, json!({"v": v})),
            Err(e) => json_reply(500, json!({"error": crate::db::anyhow_msg(&e)})),
        },
        ("POST", "step" | "note" | "finish" | "run" | "stop" | "share") => {
            if !r.header("Content-Type").unwrap_or("").starts_with("application/json") {
                return json_reply(400, json!({"error": "a post must be JSON"}));
            }
            let a: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            let text = |k: &str| a[k].as_str().map(str::to_string);
            let done = match action {
                "step" => match text("step") {
                    Some(step) => kinds::step(&db, id, &step, a["done"].as_bool().unwrap_or(true), text("value").as_deref(), text("note").as_deref()).map(|_| ()),
                    None => Err(anyhow::anyhow!("step is required")),
                },
                "note" => match text("body").filter(|b| !b.trim().is_empty()) {
                    Some(b) => kinds::note(&db, id, &b).map(|_| ()),
                    None => Err(anyhow::anyhow!("body is required")),
                },
                "run" | "stop" | "share" => match text("block").filter(|b| b.len() == 16 && b.bytes().all(|c| c.is_ascii_hexdigit())) {
                    Some(b) => match action {
                        "run" => packet_page::run_start(&db, id, &b),
                        "stop" => packet_page::run_stop(id, &b),
                        _ => packet_page::run_share(&db, id, &b),
                    },
                    None => Err(anyhow::anyhow!("block is required")),
                },
                _ => kinds::finish(&db, id),
            };
            match done {
                Ok(()) => json_reply(200, json!({"ok": true})),
                Err(e) => json_reply(400, json!({"error": crate::db::anyhow_msg(&e)})),
            }
        }
        (_, "" | "version") => (405, vec![("Allow".into(), "GET".into())], vec![]),
        _ => (404, vec![], vec![]),
    }
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
                "theme": paths::setting("theme").as_str()}),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_identities::Env;

    const KEY: &str = "k3y";

    fn req(method: &str, path: &str, ctype: Option<&str>, body: Value, cookie: bool) -> Request {
        let mut headers = vec![("Host".to_string(), "127.0.0.1:4000".to_string())];
        if cookie {
            headers.push(("Cookie".into(), format!("k={KEY}")));
        }
        if let Some(c) = ctype {
            headers.push(("Content-Type".into(), c.into()));
        }
        Request { method: method.into(), path: path.into(), query: String::new(), headers, body: body.to_string().into_bytes() }
    }

    fn go(r: &Request) -> (u16, String) {
        let (status, _, body) = route(r, 4000, KEY, "now");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[test]
    fn a_packet_is_a_page_john_can_tick_and_finish_and_the_door_is_locked() {
        let e = Env::new();
        let db = Db::open(&paths::db_path()).unwrap();
        db.init().unwrap();
        let c = Caller { session_id: Some("s".into()), env_author: None, cwd: None, harness: Some("claude-code".into()), pid: 1 };
        let made = crate::tests::call(
            &db,
            &c,
            "ask_human",
            json!({"subject": "Two runs", "body": "Waiting.", "author": "builder", "meta": {"type": "packet", "packet": crate::tests_kinds::spec()}}),
        );
        let id = made["thread_id"].as_i64().unwrap();
        let plain = db.start_thread("question", "Plain?", "builder", "agent", "Ship it?", None, None).unwrap();
        let page = format!("/packet/{id}");

        assert_eq!(go(&req("GET", &page, None, json!(null), false)).0, 401, "no key, no page");
        let (s, html) = go(&req("GET", &page, None, json!(null), true));
        assert_eq!(s, 200);
        assert!(html.contains("data-step=\"run-1\"") && html.contains("data-finish") && html.contains("Two runs"), "{html}");
        assert_eq!(go(&req("GET", &format!("/packet/{plain}"), None, json!(null), true)).0, 404, "a plain question has no packet page");
        assert_eq!(go(&req("GET", "/packet/abc", None, json!(null), true)).0, 404);

        let (_, v1) = go(&req("GET", &format!("{page}/version"), None, json!(null), true));
        let step = json!({"step": "run-1", "done": true, "note": "started 14:05"});
        assert_eq!(go(&req("POST", &format!("{page}/step"), Some("text/plain"), step.clone(), true)).0, 400, "a plain form post is refused");
        assert_eq!(go(&req("POST", &format!("{page}/step"), Some("application/json"), step.clone(), false)).0, 401);
        assert_eq!(go(&req("POST", &format!("{page}/step"), Some("application/json"), step, true)).0, 200);
        let (_, v2) = go(&req("GET", &format!("{page}/version"), None, json!(null), true));
        assert_ne!(v1, v2, "the page notices a change");
        let (s, bad) = go(&req("POST", &format!("{page}/step"), Some("application/json"), json!({"step": "which", "done": true}), true));
        assert_eq!(s, 400);
        assert!(bad.contains("pick one of"), "{bad}");
        assert_eq!(go(&req("POST", &format!("{page}/note"), Some("application/json"), json!({"body": "  "}), true)).0, 400);
        assert_eq!(go(&req("POST", &format!("{page}/note"), Some("application/json"), json!({"body": "the second host is down"}), true)).0, 200);
        let (_, html) = go(&req("GET", &page, None, json!(null), true));
        assert!(html.contains("step ticked") && html.contains("started 14:05") && html.contains("the second host is down"), "{html}");

        assert_eq!(go(&req("POST", &format!("{page}/finish"), Some("application/json"), json!({}), true)).0, 200);
        let (_, html) = go(&req("GET", &page, None, json!(null), true));
        assert!(html.contains("class=\"closed\"") && html.contains("data-finish data-left=\"2\" disabled"), "{html}");
        assert_eq!(go(&req("POST", &format!("{page}/step"), Some("application/json"), json!({"step": "run-1", "done": false}), true)).0, 400, "a finished packet is not changed");
        drop(e);
    }
    #[test]
    fn a_command_in_a_packet_runs_from_the_page_shows_its_output_and_only_the_packets_own_commands_run() {
        let e = Env::new();
        let db = Db::open(&paths::db_path()).unwrap();
        db.init().unwrap();
        let c = Caller { session_id: Some("s".into()), env_author: None, cwd: None, harness: Some("claude-code".into()), pid: 1 };
        let lang = if cfg!(windows) { "cmd" } else { "sh" };
        let mut spec = crate::tests_kinds::spec();
        spec["steps"][0]["detail"] = json!(format!("Run it:\n```{lang}\necho hello-from-packet\n```"));
        let made = crate::tests::call(&db, &c, "ask_human", json!({"subject": "Runs", "body": "Waiting.", "author": "builder", "meta": {"type": "packet", "packet": spec}}));
        let id = made["thread_id"].as_i64().unwrap();
        let page = format!("/packet/{id}");
        let (_, html) = go(&req("GET", &page, None, json!(null), true));
        let at = html.find("data-block=\"").expect("a run panel") + "data-block=\"".len();
        let hash = html[at..at + 16].to_string();
        let post = |action: &str, body: Value| go(&req("POST", &format!("{page}/{action}"), Some("application/json"), body, true));

        assert_eq!(post("run", json!({"block": "0000000000000000"})).0, 400, "a block that is not in the packet does not run");
        assert_eq!(post("run", json!({"block": "echo hi"})).0, 400, "the browser names a block; it cannot send a script");
        assert_eq!(post("run", json!({"block": hash})).0, 200);
        let mut out = Value::Null;
        for _ in 0..200 {
            let r = req("GET", &format!("{page}/run"), None, json!(null), true);
            let mut r = r;
            r.query = format!("block={hash}");
            out = serde_json::from_str(&go(&r).1).unwrap();
            if out["state"] == "done" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(out["state"], "done", "{out}");
        assert_eq!(out["code"], 0);
        assert!(out["lines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("hello-from-packet")), "{out}");
        let (_, html) = go(&req("GET", &page, None, json!(null), true));
        assert!(html.contains("data-state=\"done\"") && html.contains("hello-from-packet"), "a reload keeps the output");

        assert_eq!(post("share", json!({"block": hash})).0, 200);
        let msgs = db.get_thread(id).unwrap();
        let last = msgs["messages"].as_array().unwrap().last().unwrap();
        assert!(last["author"] == "john" && last["body"].as_str().unwrap().contains("hello-from-packet"), "{last}");

        assert_eq!(post("finish", json!({})).0, 200);
        assert_eq!(post("run", json!({"block": hash})).0, 400, "a finished packet runs nothing");
        drop(e);
    }

    #[test]
    fn events_after_a_seq_honour_the_limit_behind_the_door() {
        let e = Env::new();
        let db = Db::open(&paths::db_path()).unwrap();
        db.init().unwrap();
        let tid = db.start_thread("discussion", "s", "alpha", "agent", "one", None, None).unwrap();
        let after = db.max_event_seq();
        for b in ["two", "three", "four"] {
            db.reply(tid, "alpha", "agent", b, None, None).unwrap();
        }
        let mut r = req("GET", "/events", None, json!(null), true);
        r.query = format!("after={after}&limit=2");
        let (s, body) = go(&r);
        assert_eq!(s, 200);
        let evs: Vec<Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(evs.iter().map(|e| e["body"].as_str().unwrap()).collect::<Vec<_>>(), ["two", "three"]);
        assert!(evs[0]["seq"].as_i64().unwrap() > after && evs[0]["data"]["message_id"].is_i64());
        r.query = String::new();
        assert_eq!(serde_json::from_str::<Vec<Value>>(&go(&r).1).unwrap().len() as i64, db.max_event_seq(), "after defaults to the start");
        assert_eq!(go(&req("GET", "/events", None, json!(null), false)).0, 401, "no key, no log");
        assert_eq!(go(&req("POST", "/events", Some("application/json"), json!({}), true)).0, 405);
        drop(e);
    }

    /// Opens `/events/stream` on the running console; returns the socket and the status line.
    fn open_stream(port: u16, key: Option<&str>) -> (TcpStream, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let cookie = key.map(|k| format!("Cookie: k={k}\r\n")).unwrap_or_default();
        s.write_all(format!("GET /events/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}\r\n").as_bytes()).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let mut got = Vec::new();
        let mut b = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") && s.read(&mut b).map(|n| n == 1).unwrap_or(false) {
            got.push(b[0]);
        }
        let head = String::from_utf8_lossy(&got).into_owned();
        (s, head.lines().next().unwrap_or("").to_string())
    }

    #[test]
    fn the_event_stream_delivers_a_new_post_within_a_second_needs_the_key_and_takes_eight() {
        let e = Env::new();
        let db = Db::open(&paths::db_path()).unwrap();
        db.init().unwrap();
        let server = start().expect("the console starts");
        let port: u16 = std::fs::read_to_string(port_file()).unwrap().trim().parse().unwrap();
        let k = key();

        assert!(open_stream(port, None).1.contains(" 401 "), "no key, no stream");
        let (mut s, status) = open_stream(port, Some(&k));
        assert!(status.contains(" 200 "), "{status}");
        let tid = db.start_thread("discussion", "live", "alpha", "agent", "streamed-now", None, None).unwrap();
        let posted = std::time::Instant::now();
        s.set_read_timeout(Some(std::time::Duration::from_millis(100))).unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        while !seen.contains("streamed-now") && posted.elapsed() < std::time::Duration::from_secs(1) {
            if let Ok(n) = s.read(&mut buf) {
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }
        assert!(seen.contains("streamed-now"), "an event posted after the stream opened arrives within a second: {seen:?}");
        let data = seen.lines().find(|l| l.starts_with("data: ") && l.contains("streamed-now")).unwrap();
        let ev: Value = serde_json::from_str(&data["data: ".len()..]).unwrap();
        assert_eq!(ev["thread_id"], tid);
        assert!(seen.contains(&format!("id: {}\n", ev["seq"])), "{seen}");

        let mut more: Vec<TcpStream> = (0..7)
            .map(|_| open_stream(port, Some(&k)))
            .map(|(s, st)| {
                assert!(st.contains(" 200 "), "{st}");
                s
            })
            .collect();
        let (_ninth, st) = open_stream(port, Some(&k));
        assert!(st.contains(" 503 "), "the ninth stream is refused: {st}");
        more.clear();
        drop(s);
        drop(server);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while STREAMS.load(std::sync::atomic::Ordering::SeqCst) > 0 && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(STREAMS.load(std::sync::atomic::Ordering::SeqCst), 0, "the streams end with the console");
        drop(e);
    }

    #[test]
    fn the_packet_page_listens_to_the_event_stream_and_keeps_polling_as_the_fallback() {
        let e = Env::new();
        let db = Db::open(&paths::db_path()).unwrap();
        db.init().unwrap();
        let c = Caller { session_id: Some("s".into()), env_author: None, cwd: None, harness: Some("claude-code".into()), pid: 1 };
        let made = crate::tests::call(
            &db,
            &c,
            "ask_human",
            json!({"subject": "P", "body": "Waiting.", "author": "builder", "meta": {"type": "packet", "packet": crate::tests_kinds::spec()}}),
        );
        let (s, html) = go(&req("GET", &format!("/packet/{}", made["thread_id"]), None, json!(null), true));
        assert_eq!(s, 200);
        assert!(html.contains("new EventSource('/events/stream')") && html.contains("es.onerror") && html.contains("setInterval(check,2500)"), "{html}");
        drop(e);
    }

    #[test]
    fn dictation_is_behind_the_same_door_and_only_starts_on_a_json_post() {
        let e = Env::new();
        assert_eq!(go(&req("POST", "/dictate/poll", Some("application/json"), json!({}), false)).0, 401, "no key, no microphone");
        assert_eq!(go(&req("GET", "/dictate/start", None, json!(null), true)).0, 405);
        assert_eq!(go(&req("POST", "/dictate/start", Some("text/plain"), json!({}), true)).0, 400, "a plain form post from another page is refused");
        assert_eq!(go(&req("POST", "/dictate/format", Some("application/json"), json!({}), true)).0, 404);
        drop(e);
    }
}
