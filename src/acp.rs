//! An Agent Client Protocol (ACP) client: JSON-RPC 2.0 over a child process's stdio, one JSON message per line.
//!
//! Implements protocol version 1 (https://agentclientprotocol.com, checked 2026-10-26): `initialize`, `session/new`, `session/prompt`
//! (with `session/update` notifications streaming until the response arrives), `session/cancel`, and the agent's inbound
//! `session/request_permission`. Inbound `fs/*` and `terminal/*` requests get a JSON-RPC method-not-found error, and `initialize`
//! advertises no such capability, so a well-behaved agent never asks. Not yet wired into the supervisor.
//!
//! Timeouts are idle timeouts: how long the agent may go without sending any message at all, not a cap on the whole call. Setup
//! calls (`initialize`, `session/new`) default to 60 s; a prompt turn defaults to 10 minutes, since a working agent can think or run a
//! long tool between updates. Both are settable (`set_setup_timeout`, `set_prompt_timeout`, or `set_timeout` for both).
//!
//! The client makes one request at a time, so a JSON-RPC error response whose `id` is null or names no request it is waiting for
//! (and none it already gave up on) is taken to be about the pending one and fails it at once rather than waiting out the timeout.
#![allow(dead_code)]

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The ACP major protocol version this client speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// Default idle timeout for `initialize` and `session/new`.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(60);
/// Default idle timeout for a `session/prompt` turn.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(600);

/// How the agent ended a prompt turn.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
}

impl StopReason {
    fn parse(s: &str) -> StopReason {
        match s {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "max_turn_requests" => StopReason::MaxTurnRequests,
            "refusal" => StopReason::Refusal,
            "cancelled" => StopReason::Cancelled,
            o => StopReason::Other(o.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermOption {
    pub id: String,
    pub name: String,
    /// allow_once | allow_always | reject_once | reject_always
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub session_id: String,
    pub tool_call: Value,
    pub options: Vec<PermOption>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PermissionDecision {
    Allow,
    Reject,
    Cancel,
}

/// A harness-neutral view of one `session/update` (or of a permission request the agent made during the turn).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Update {
    AgentMessageChunk {
        text: String,
    },
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
    },
    ToolCallUpdate {
        id: String,
        status: Option<String>,
        raw: Value,
    },
    Plan {
        entries: Vec<Value>,
    },
    PermissionRequest(PermissionRequest),
    /// Anything else (thought chunks, mode changes, available commands, ...): the raw `update` object.
    Other(Value),
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn convert_update(u: &Value) -> Update {
    match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("") {
        "agent_message_chunk" => match u.pointer("/content/text").and_then(Value::as_str) {
            Some(t) => Update::AgentMessageChunk { text: t.to_string() },
            None => Update::Other(u.clone()),
        },
        "tool_call" => Update::ToolCall { id: s(u, "toolCallId"), title: s(u, "title"), kind: s(u, "kind"), status: s(u, "status") },
        "tool_call_update" => Update::ToolCallUpdate { id: s(u, "toolCallId"), status: u.get("status").and_then(Value::as_str).map(String::from), raw: u.clone() },
        "plan" => Update::Plan { entries: u.get("entries").and_then(Value::as_array).cloned().unwrap_or_default() },
        _ => Update::Other(u.clone()),
    }
}

fn parse_permission(p: &Value) -> PermissionRequest {
    let options = p
        .get("options")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|o| PermOption { id: s(o, "optionId"), name: s(o, "name"), kind: s(o, "kind") }).collect())
        .unwrap_or_default();
    PermissionRequest { session_id: s(p, "sessionId"), tool_call: p.get("toolCall").cloned().unwrap_or(Value::Null), options }
}

fn permission_outcome(req: &PermissionRequest, d: PermissionDecision) -> Value {
    let want: &[&str] = match d {
        PermissionDecision::Allow => &["allow_once", "allow_always"],
        PermissionDecision::Reject => &["reject_once", "reject_always"],
        PermissionDecision::Cancel => &[],
    };
    for k in want {
        if let Some(o) = req.options.iter().find(|o| o.kind == *k) {
            return json!({"outcome": {"outcome": "selected", "optionId": o.id}});
        }
    }
    json!({"outcome": {"outcome": "cancelled"}})
}

enum Msg {
    Line(Value),
    Eof,
}

fn write_line(stdin: &Mutex<ChildStdin>, v: &Value) -> Result<()> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    let mut g = stdin.lock().map_err(|_| anyhow!("acp stdin lock poisoned"))?;
    g.write_all(line.as_bytes()).and_then(|_| g.flush()).map_err(|e| anyhow!("writing to the agent failed: {e}"))
}

/// A cloneable, thread-safe way to send `session/cancel` while `prompt` is blocking.
#[derive(Clone)]
pub struct CancelHandle {
    stdin: Arc<Mutex<ChildStdin>>,
}

impl CancelHandle {
    pub fn cancel(&self, session: &str) -> Result<()> {
        write_line(&self.stdin, &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}))
    }
}

pub struct AcpAgent {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    rx: Receiver<Msg>,
    next_id: u64,
    setup_timeout: Duration,
    prompt_timeout: Duration,
    /// Requests that timed out: a late response to one of these is stale and is ignored.
    abandoned: HashSet<u64>,
    handler: Box<dyn FnMut(&PermissionRequest) -> PermissionDecision + Send>,
}

impl AcpAgent {
    /// Starts the agent process. Its stderr is discarded (ACP says it is free-form logging). `program` is found the way a shell would
    /// (`paths::command`): on Windows that includes PATHEXT, and an npm `.cmd` shim (opencode, gemini) runs through `cmd /d /c`.
    pub fn spawn(program: &str, args: &[String], cwd: &Path, env: &[(String, String)]) -> Result<AcpAgent> {
        let mut cmd = crate::paths::command(program);
        cmd.args(args).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| anyhow!("cannot start {program}: {e}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let (tx, rx) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // A line that is not a JSON object is not ACP: ignore it rather than die.
                if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(line.trim()) {
                    if tx.send(Msg::Line(v)).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Msg::Eof);
        });
        Ok(AcpAgent {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            rx,
            next_id: 0,
            setup_timeout: SETUP_TIMEOUT,
            prompt_timeout: PROMPT_TIMEOUT,
            abandoned: HashSet::new(),
            handler: Box::new(|_| PermissionDecision::Reject),
        })
    }

    /// Sets both idle timeouts (setup and prompt) to `d`.
    pub fn set_timeout(&mut self, d: Duration) {
        self.setup_timeout = d;
        self.prompt_timeout = d;
    }

    /// How long the agent may stay silent during `initialize` or `session/new` before the call fails. Default 60 s.
    pub fn set_setup_timeout(&mut self, d: Duration) {
        self.setup_timeout = d;
    }

    /// How long the agent may stay silent (no update or request of any kind) during a prompt turn before it fails. Default 10 minutes.
    pub fn set_prompt_timeout(&mut self, d: Duration) {
        self.prompt_timeout = d;
    }

    /// Decides `session/request_permission`. The default rejects everything.
    pub fn set_permission_handler(&mut self, f: impl FnMut(&PermissionRequest) -> PermissionDecision + Send + 'static) {
        self.handler = Box::new(f);
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle { stdin: self.stdin.clone() }
    }

    /// Sends one request and waits for its response, serving the agent's notifications and inbound requests meanwhile.
    /// `idle` is how long the agent may go without sending anything.
    fn call(&mut self, method: &str, params: Value, idle: Duration, on_update: &mut dyn FnMut(Update)) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        write_line(&self.stdin, &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        loop {
            let msg = match self.rx.recv_timeout(idle) {
                Ok(Msg::Line(v)) => v,
                Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => bail!("the agent exited during {method}"),
                Err(RecvTimeoutError::Timeout) => {
                    self.abandoned.insert(id);
                    bail!("the agent was silent for {idle:?} during {method}")
                }
            };
            let m = msg.get("method").and_then(Value::as_str);
            match (m, msg.get("id")) {
                (Some(m), Some(rid)) => self.inbound_request(m, rid.clone(), msg.get("params").unwrap_or(&Value::Null), on_update)?,
                (Some("session/update"), None) => {
                    if let Some(u) = msg.pointer("/params/update") {
                        on_update(convert_update(u));
                    }
                }
                (Some(_), None) => {}
                (None, Some(rid)) if rid.as_u64() == Some(id) => {
                    if let Some(e) = msg.get("error") {
                        bail!("{method} failed: {}", e.get("message").and_then(Value::as_str).unwrap_or("agent error"));
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                // An error naming no request we are waiting on (a null id, a missing one, or one we never sent) can only be about the
                // one outstanding request: fail it now. A late answer to a request that already timed out is stale: drop it.
                (None, rid) if msg.get("error").is_some() && !rid.and_then(Value::as_u64).is_some_and(|r| self.abandoned.contains(&r)) => {
                    let e = &msg["error"];
                    bail!("{method} failed: {} (error response with id {})", e.get("message").and_then(Value::as_str).unwrap_or("agent error"), rid.unwrap_or(&Value::Null));
                }
                _ => {}
            }
        }
    }

    fn inbound_request(&mut self, method: &str, id: Value, params: &Value, on_update: &mut dyn FnMut(Update)) -> Result<()> {
        let reply = if method == "session/request_permission" {
            let req = parse_permission(params);
            on_update(Update::PermissionRequest(req.clone()));
            let d = (self.handler)(&req);
            json!({"jsonrpc": "2.0", "id": id, "result": permission_outcome(&req, d)})
        } else {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}})
        };
        write_line(&self.stdin, &reply)
    }

    /// Capabilities exchange. Returns the agent's `initialize` result (agentCapabilities, authMethods, ...).
    pub fn initialize(&mut self) -> Result<Value> {
        let p = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "agentdesk", "title": "AgentDesk", "version": env!("CARGO_PKG_VERSION")},
        });
        let r = self.call("initialize", p, self.setup_timeout, &mut |_| {})?;
        let v = r.get("protocolVersion").and_then(Value::as_u64).unwrap_or(0);
        if v != PROTOCOL_VERSION {
            bail!("the agent speaks ACP version {v}, this client speaks {PROTOCOL_VERSION}");
        }
        Ok(r)
    }

    pub fn new_session(&mut self, cwd: &Path) -> Result<String> {
        let r = self.call("session/new", json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}), self.setup_timeout, &mut |_| {})?;
        r.get("sessionId").and_then(Value::as_str).map(String::from).ok_or_else(|| anyhow!("session/new returned no sessionId"))
    }

    /// Sends a prompt and blocks until the agent's response, calling `on_update` for everything it streams.
    pub fn prompt(&mut self, session: &str, text: &str, on_update: &mut dyn FnMut(Update)) -> Result<StopReason> {
        let r = self.call("session/prompt", json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}), self.prompt_timeout, on_update)?;
        Ok(StopReason::parse(r.get("stopReason").and_then(Value::as_str).unwrap_or("")))
    }

    /// Asks the agent to stop the turn in progress (a notification; the pending `prompt` then returns `Cancelled`).
    pub fn cancel(&self, session: &str) -> Result<()> {
        self.cancel_handle().cancel(session)
    }
}

impl Drop for AcpAgent {
    fn drop(&mut self) {
        // A `.cmd` shim leaves the real agent as a grandchild of `cmd`: take the whole tree down, not just the shell.
        if cfg!(windows) {
            let _ = crate::paths::new_command("taskkill").args(["/PID", &self.child.id().to_string(), "/T", "/F"]).output();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `agentdesk acp-probe <program> [args...]`: initialize, open a session in the current folder, say hello, print what streams back.
pub fn probe(program: &str, args: &[String]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut a = AcpAgent::spawn(program, args, &cwd, &[])?;
    let init = a.initialize()?;
    println!("initialize: {}", init);
    let sid = a.new_session(&cwd)?;
    println!("session: {sid}");
    let stop = a.prompt(&sid, "say hello", &mut |u| println!("update: {u:?}"))?;
    println!("stop: {stop:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE: &str = r#"
import sys, json, os
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def chunk(sid, t):
    send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":t}}}})
def read():
    l = sys.stdin.readline()
    if not l: os._exit(0)
    return json.loads(l)
while True:
    m = read()
    meth = m.get("method"); i = m.get("id")
    if meth == "initialize":
        send({"jsonrpc":"2.0","id":i,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":False},"authMethods":[]}})
    elif meth == "session/new":
        send({"jsonrpc":"2.0","id":i,"result":{"sessionId":"s1"}})
    elif meth == "session/prompt":
        t = m["params"]["prompt"][0]["text"]
        if t == "stream":
            sys.stdout.write("this is not json\n{broken\n"); sys.stdout.flush()
            for c in ("a","b","c"): chunk("s1", c)
            send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Run","kind":"execute","status":"pending"}}})
            send({"jsonrpc":"2.0","id":i,"result":{"stopReason":"end_turn"}})
        elif t == "perm":
            send({"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"t1"},"options":[{"optionId":"yes","name":"Allow","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}})
            r = read()
            chunk("s1", r["result"]["outcome"].get("optionId", "cancelled"))
            send({"jsonrpc":"2.0","id":i,"result":{"stopReason":"end_turn"}})
        elif t == "fs":
            send({"jsonrpc":"2.0","id":901,"method":"fs/read_text_file","params":{"sessionId":"s1","path":"/x"}})
            r = read()
            chunk("s1", str(r["error"]["code"]))
            send({"jsonrpc":"2.0","id":i,"result":{"stopReason":"end_turn"}})
        elif t == "slow":
            chunk("s1", "start")
            while read().get("method") != "session/cancel": pass
            send({"jsonrpc":"2.0","id":i,"result":{"stopReason":"cancelled"}})
        elif t == "die":
            chunk("s1", "bye"); os._exit(0)
        elif t == "silent":
            while True: read()
        elif t == "nullerr":
            send({"jsonrpc":"2.0","id":None,"error":{"code":-32700,"message":"parse error"}})
        elif t == "wrongerr":
            send({"jsonrpc":"2.0","id":4242,"error":{"code":-32603,"message":"internal error"}})
"#;

    /// The python to run the fake agent with, or `None` (and a note on stderr) when there is none: the tests then skip rather than
    /// fail. Runs `--version` so the Windows Store's `python` stub, which exists but runs nothing, counts as absent.
    fn python() -> Option<String> {
        for p in ["python", "python3"] {
            if crate::paths::command(p).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
                return Some(p.into());
            }
        }
        eprintln!("skipping: the ACP tests need python on PATH to run the fake agent, and there is none");
        None
    }

    fn start(a: &mut AcpAgent) -> String {
        a.set_timeout(Duration::from_secs(20));
        a.initialize().unwrap();
        a.new_session(&std::env::current_dir().unwrap()).unwrap()
    }

    fn fake() -> Option<(AcpAgent, String)> {
        let py = python()?;
        let mut a = AcpAgent::spawn(&py, &["-u".into(), "-c".into(), FAKE.into()], &std::env::current_dir().unwrap(), &[]).unwrap();
        let sid = start(&mut a);
        Some((a, sid))
    }

    fn texts(v: &[Update]) -> String {
        v.iter().filter_map(|u| if let Update::AgentMessageChunk { text } = u { Some(text.as_str()) } else { None }).collect()
    }

    #[test]
    fn initialize_reports_the_version_and_session_id() {
        let Some((_a, sid)) = fake() else { return };
        assert_eq!(sid, "s1");
    }

    #[test]
    fn prompt_streams_three_chunks_ignoring_a_malformed_line() {
        let Some((mut a, sid)) = fake() else { return };
        let mut got = vec![];
        let stop = a.prompt(&sid, "stream", &mut |u| got.push(u)).unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(texts(&got), "abc");
        assert!(got.iter().any(|u| matches!(u, Update::ToolCall { id, kind, .. } if id == "t1" && kind == "execute")));
    }

    #[test]
    fn permission_is_routed_to_the_callback_and_defaults_to_reject() {
        let Some((mut a, sid)) = fake() else { return };
        let mut got = vec![];
        a.prompt(&sid, "perm", &mut |u| got.push(u)).unwrap();
        assert_eq!(texts(&got), "no");
        assert!(got.iter().any(|u| matches!(u, Update::PermissionRequest(r) if r.options.len() == 2)));
        a.set_permission_handler(|_| PermissionDecision::Allow);
        let mut got = vec![];
        a.prompt(&sid, "perm", &mut |u| got.push(u)).unwrap();
        assert_eq!(texts(&got), "yes");
        a.set_permission_handler(|_| PermissionDecision::Cancel);
        let mut got = vec![];
        a.prompt(&sid, "perm", &mut |u| got.push(u)).unwrap();
        assert_eq!(texts(&got), "cancelled");
    }

    #[test]
    fn inbound_fs_requests_get_method_not_found() {
        let Some((mut a, sid)) = fake() else { return };
        let mut got = vec![];
        a.prompt(&sid, "fs", &mut |u| got.push(u)).unwrap();
        assert_eq!(texts(&got), "-32601");
    }

    #[test]
    fn cancel_mid_prompt_ends_the_turn_as_cancelled() {
        let Some((mut a, sid)) = fake() else { return };
        let h = a.cancel_handle();
        let sid2 = sid.clone();
        let stop = a.prompt(&sid, "slow", &mut |u| {
            if matches!(u, Update::AgentMessageChunk { .. }) {
                h.cancel(&sid2).unwrap();
            }
        });
        assert_eq!(stop.unwrap(), StopReason::Cancelled);
    }

    #[test]
    fn the_child_exiting_mid_prompt_is_an_error_not_a_hang() {
        let Some((mut a, sid)) = fake() else { return };
        let mut got = vec![];
        let e = a.prompt(&sid, "die", &mut |u| got.push(u)).unwrap_err();
        assert!(e.to_string().contains("exited"), "{e}");
        assert_eq!(texts(&got), "bye");
    }

    #[test]
    fn a_silent_agent_times_out() {
        let Some((mut a, sid)) = fake() else { return };
        a.set_timeout(Duration::from_millis(400));
        let e = a.prompt(&sid, "silent", &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("silent"), "{e}");
    }

    #[test]
    fn an_error_with_a_null_or_unknown_id_fails_the_pending_prompt_at_once() {
        let Some((mut a, sid)) = fake() else { return };
        a.set_prompt_timeout(PROMPT_TIMEOUT);
        for (t, want) in [("nullerr", "parse error"), ("wrongerr", "internal error")] {
            let started = std::time::Instant::now();
            let e = a.prompt(&sid, t, &mut |_| {}).unwrap_err();
            assert!(e.to_string().contains(want), "{e}");
            assert!(started.elapsed() < Duration::from_secs(10), "{t} waited {:?}", started.elapsed());
        }
    }

    #[test]
    fn the_prompt_idle_timeout_defaults_longer_than_setup() {
        assert_eq!(SETUP_TIMEOUT, Duration::from_secs(60));
        assert_eq!(PROMPT_TIMEOUT, Duration::from_secs(600));
    }

    /// npm installs agents such as opencode and gemini as `.cmd` shims, which `CreateProcess` will not run by name: spawning one by its
    /// extensionless path proves PATHEXT resolution and the `cmd /d /c` wrapper.
    #[cfg(windows)]
    #[test]
    fn spawns_an_npm_style_cmd_shim() {
        let Some(py) = python() else { return };
        let dir = std::env::temp_dir().join(format!("ad-acp-shim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fake.py"), FAKE).unwrap();
        std::fs::write(dir.join("fakeagent.cmd"), format!("@\"{py}\" -u \"%~dp0fake.py\" %*\r\n")).unwrap();
        {
            let mut a = AcpAgent::spawn(&dir.join("fakeagent").display().to_string(), &[], &dir, &[]).unwrap();
            let sid = start(&mut a);
            assert_eq!(sid, "s1");
            let mut got = vec![];
            assert_eq!(a.prompt(&sid, "stream", &mut |u| got.push(u)).unwrap(), StopReason::EndTurn);
            assert_eq!(texts(&got), "abc");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn updates_convert_to_the_neutral_enum() {
        let p = convert_update(&json!({"sessionUpdate": "plan", "entries": [{"content": "x"}]}));
        assert!(matches!(p, Update::Plan { entries } if entries.len() == 1));
        let t = convert_update(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "c", "status": "completed"}));
        assert!(matches!(t, Update::ToolCallUpdate { id, status: Some(s), .. } if id == "c" && s == "completed"));
        assert!(matches!(convert_update(&json!({"sessionUpdate": "agent_thought_chunk"})), Update::Other(_)));
    }
}
