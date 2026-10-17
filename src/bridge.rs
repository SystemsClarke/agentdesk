//! The Slack bridge (`python/scripts/slack_bridge.py`) under the daemon's supervision, as the original's core did. Its heartbeat is
//! `slack_bridge.state` (its pid and `ts`). A bridge this process did not start is left alone while it beats, and adopted once its pid
//! is gone. One that is alive but has not beaten for `hung` (180 s) is a stale orphan: logged and shown in `ui:status`, and no second
//! bridge is started (it would double-post): kill that pid and the daemon takes over. A bridge it supervises that stays alive without
//! beating for `hung` is killed and restarted. Restarts wait 5 s, doubling to 5 min, back to 5 s once a run lasted 10 min.
//!
//! The native bridge (`slack.rs`, no Python) is the same thing run as threads of this process instead of a child: it writes the same
//! heartbeat and the same `ui:status` section (state `supervised`, `pid` the core's own, `native: true`), and a run that returns an error
//! or panics is restarted on the same delays. `settings.json` `"slack_native"` picks it: `true` or `false` is taken as written; absent, it
//! is on when no Python interpreter is found (there is nothing else to run) and off otherwise, so the proven Python bridge stays the
//! default until the native one has run against real Slack. To switch an installed machine over, put `"slack_native": true` in
//! `settings.json` in the data folder and restart the core (`agentdesk core restart`); `false` switches back. A bridge started by hand
//! (`agentdesk slack-bridge`) is "external" to the core while it beats, like any other.

use crate::sessions::Registry;
use crate::{paths, pyhost, slack};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRESH: Duration = Duration::from_secs(90);

fn status_file() -> std::path::PathBuf {
    paths::data_dir().join("bridge.json")
}

fn heartbeat_file() -> std::path::PathBuf {
    paths::data_dir().join("slack_bridge.state")
}

fn secs_env(name: &str, default: u64) -> Duration {
    Duration::from_secs(std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
}

/// Whether a process with this pid is alive. On Windows this is a handle query, not a `tasklist` spawn: starting a process costs
/// 300 ms or more on a managed machine, and this runs on every window refresh and every daemon pass.
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        const STILL_ACTIVE: u32 = 259;
        // SAFETY: plain FFI; the handle is closed before returning.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE
        }
    }
    #[cfg(not(windows))]
    {
        crate::paths::new_command("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    }
}

/// The heartbeat: its pid if that process is alive, and its age; nones with no readable file.
pub fn heartbeat() -> (Option<i64>, Option<Duration>) {
    let mut doc: Option<Value> = None;
    for _ in 0..3 {
        doc = std::fs::read_to_string(heartbeat_file()).ok().and_then(|t| serde_json::from_str(&t).ok());
        if doc.is_some() || !heartbeat_file().exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50)); // caught mid-replace
    }
    let Some(d) = doc else { return (None, None) };
    let Some(ts) = d["ts"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()) else { return (None, None) };
    let age = (Utc::now() - ts.with_timezone(&Utc)).to_std().unwrap_or_default();
    (d["pid"].as_i64().filter(|p| pid_alive(*p)), Some(age))
}

/// True when a bridge is running: its heartbeat's pid is alive and its `ts` under 90 s old.
pub fn running() -> bool {
    matches!(heartbeat(), (Some(_), Some(age)) if age < FRESH)
}

#[derive(Default, Clone)]
struct Info {
    enabled: bool,
    state: String,
    supervised: bool,
    orphan_pid: Option<i64>,
    pid: Option<u32>,
    restarts: i64,
    hung_restarts: i64,
    last_exit: Option<i64>,
    last_exit_ts: Option<String>,
    /// Running inside this process (`slack.rs`) rather than as a Python child.
    native: bool,
}

impl Info {
    fn json(&self) -> Value {
        json!({"enabled": self.enabled, "state": self.state, "supervised": self.supervised, "orphan_pid": self.orphan_pid, "pid": self.pid,
               "restarts": self.restarts, "hung_restarts": self.hung_restarts, "last_exit": self.last_exit, "last_exit_ts": self.last_exit_ts, "native": self.native})
    }
}

/// What `ui:status` reports as `bridge`: the daemon's own account of it (it writes `bridge.json` every poll), or, with no daemon, what the
/// heartbeat alone says.
pub fn status() -> Value {
    let fresh = std::fs::metadata(status_file()).ok().and_then(|m| m.modified().ok()).and_then(|m| m.elapsed().ok()).map(|e| e < Duration::from_secs(180)).unwrap_or(false);
    if fresh {
        if let Some(v) = std::fs::read_to_string(status_file()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            return v;
        }
    }
    let enabled = settings_on();
    let state = match heartbeat() {
        (Some(_), Some(a)) if a < secs_env("AGENTDESK_BRIDGE_HUNG", 180) => "external",
        (Some(_), Some(_)) => "stale-orphan",
        _ => "starting",
    };
    json!({"enabled": enabled, "state": state, "supervised": false, "orphan_pid": heartbeat().0, "pid": null, "restarts": 0, "hung_restarts": 0, "last_exit": null, "last_exit_ts": null, "native": false})
}

/// `settings.json`'s `"slack_bridge": false` switches it off.
fn settings_on() -> bool {
    paths::setting("slack_bridge").as_bool().unwrap_or(true)
}

/// Whether the bridge runs natively (see the module comment): `slack_native` in settings.json, else when there is no Python to run.
/// `AGENTDESK_BRIDGE_CMD` (a stand-in command) is a Python-style child, so it keeps the supervised path unless the setting says otherwise.
pub fn use_native() -> bool {
    match paths::setting("slack_native").as_bool() {
        Some(b) => b,
        None => std::env::var("AGENTDESK_BRIDGE_CMD").map_or(true, |c| c.trim().is_empty()) && pyhost::interpreter().is_none(),
    }
}

/// The command to supervise, or None (with the reason) when this process should not start a bridge.
fn command() -> Result<Vec<String>, &'static str> {
    if let Ok(cmd) = std::env::var("AGENTDESK_BRIDGE_CMD") {
        if !cmd.trim().is_empty() {
            return Ok(split(&cmd));
        }
    }
    if std::env::var_os("AGENTDESK_DATA").is_some() {
        return Err("not supervised on a custom AGENTDESK_DATA without AGENTDESK_BRIDGE_CMD");
    }
    let creds = std::env::var("AGENTDESK_SLACK_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| crate::paths::home_dir().join(".claude").join("slack-notify"));
    if !creds.join("bot-token.txt").exists() && !creds.join("bots.json").exists() {
        return Err("not configured: no Slack credentials (set AGENTDESK_SLACK_DIR, or put bot-token.txt and app-token.txt in ~/.claude/slack-notify)");
    }
    let py = pyhost::interpreter().ok_or("no Python 3 interpreter")?;
    let dir = pyhost::dir().map_err(|_| "could not extract the Python half")?;
    Ok(vec![py, dir.join("scripts").join("slack_bridge.py").to_string_lossy().into_owned()])
}

fn split(command: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote) = (Vec::new(), String::new(), None::<char>);
    for c in command.trim().chars() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (q, Some(o)) if q == o => quote = None,
            (c, None) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (c, _) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn write_status(i: &Info) {
    let _ = std::fs::write(status_file(), i.json().to_string());
}

/// Keeps the bridge going until `stop` is set. Meant for the daemon's (or the window's) background thread; run it only in the process that
/// holds the supervisor lock, so there is one.
pub fn run(stop: Arc<std::sync::atomic::AtomicBool>, registry: Registry) {
    let (poll, hung) = (secs_env("AGENTDESK_BRIDGE_POLL", 30), secs_env("AGENTDESK_BRIDGE_HUNG", 180));
    let first = secs_env("AGENTDESK_BRIDGE_RESTART", 5);
    let mut info = Info { enabled: settings_on(), state: "starting".into(), ..Default::default() };
    if !info.enabled {
        paths::log("INFO", "slack bridge: off in settings.json");
        write_status(&info);
        return;
    }
    let native = use_native();
    let cmd = match if native { slack::unconfigured().map_or(Ok(vec![]), Err) } else { command() } {
        Ok(c) => c,
        Err(why) => {
            info.state = if why.starts_with("not configured") { "not-configured".into() } else { "not-supervised".into() };
            paths::log("INFO", &format!("slack bridge: {why}"));
            // someone else's bridge may still be beating: report that
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match heartbeat() {
                    (Some(_), Some(age)) if age < hung => (info.state, info.orphan_pid) = ("external".into(), None),
                    (Some(p), Some(_)) => (info.state, info.orphan_pid) = ("stale-orphan".into(), Some(p)),
                    _ => {}
                }
                write_status(&info);
                sleep_until(&stop, poll);
            }
            return;
        }
    };
    // 1. someone else's bridge: leave it while it beats
    let mut warned: Option<i64> = None;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match heartbeat() {
            (Some(p), Some(age)) => {
                if age < hung {
                    (info.state, info.orphan_pid) = ("external".into(), None);
                } else {
                    (info.state, info.orphan_pid) = ("stale-orphan".into(), Some(p));
                    if warned != Some(p) {
                        paths::log("WARN", &format!("slack bridge: pid {p} is alive but has not beaten for {}s. Not starting a second bridge, which would double-post: kill pid {p} and the daemon takes over.", age.as_secs()));
                        warned = Some(p);
                    }
                }
                write_status(&info);
                sleep_until(&stop, poll);
            }
            _ => break,
        }
    }
    // 2. supervise one
    info.state = "supervised".into();
    info.supervised = true;
    info.orphan_pid = None;
    let mut delay = first;
    let max = secs_env("AGENTDESK_BRIDGE_RESTART_MAX", 300);
    if native {
        paths::log("INFO", "slack bridge: native (threads of this process)");
        info.native = true;
        return run_native(&stop, &mut info, registry, (first, max), poll);
    }
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let began = Instant::now();
        let child = crate::paths::new_command(&cmd[0])
            .args(&cmd[1..])
            .current_dir(pyhost::dir().unwrap_or_else(|_| paths::data_dir()))
            .env("PYTHONIOENCODING", "utf-8")
            .env("AGENTDESK_DATA", paths::data_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let code = match child {
            Ok(mut c) => {
                info.pid = Some(c.id());
                paths::log("INFO", &format!("slack bridge: started pid {}: {}", c.id(), cmd.join(" ")));
                write_status(&info);
                watch(&mut c, &stop, &mut info, poll, hung, began)
            }
            Err(e) => {
                paths::log("WARN", &format!("slack bridge: could not start {}: {e}", cmd[0]));
                None
            }
        };
        info.pid = None;
        info.last_exit = code;
        info.last_exit_ts = Some(crate::db::iso(Utc::now()));
        if began.elapsed() >= Duration::from_secs(600) {
            delay = first;
        }
        paths::log("INFO", &format!("slack bridge: exited with code {code:?} after {}s; restarting in {}s", began.elapsed().as_secs(), delay.as_secs()));
        write_status(&info);
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        sleep_until(&stop, delay);
        delay = (delay * 2).min(max);
        info.restarts += 1;
    }
}

/// The native bridge, kept going the way a child is: a run that ends (an error, a panic) is started again after the same delays.
fn run_native(stop: &Arc<std::sync::atomic::AtomicBool>, info: &mut Info, registry: Registry, (first, max): (Duration, Duration), poll: Duration) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut delay = first;
    while !stop.load(Relaxed) {
        let began = Instant::now();
        info.pid = Some(std::process::id());
        write_status(info);
        let (s, input) = (stop.clone(), slack::registry_input(registry.clone()));
        let run = std::thread::spawn(move || slack::run(s, input));
        let mut last_poll = Instant::now();
        while !run.is_finished() {
            if last_poll.elapsed() >= poll {
                last_poll = Instant::now();
                write_status(info);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let code = match run.join() {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => {
                paths::log("WARN", &format!("slack bridge: {}", crate::db::anyhow_msg(&e)));
                1
            }
            Err(_) => {
                paths::log("WARN", "slack bridge: the native bridge panicked");
                101
            }
        };
        info.pid = None;
        info.last_exit = Some(code);
        info.last_exit_ts = Some(crate::db::iso(Utc::now()));
        if stop.load(Relaxed) {
            write_status(info);
            break;
        }
        if began.elapsed() >= Duration::from_secs(600) {
            delay = first;
        }
        paths::log("INFO", &format!("slack bridge: native run ended with code {code} after {}s; restarting in {}s", began.elapsed().as_secs(), delay.as_secs()));
        write_status(info);
        sleep_until(stop, delay);
        delay = (delay * 2).min(max);
        info.restarts += 1;
    }
}

fn sleep_until(stop: &std::sync::atomic::AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Waits for the child, recording its status each poll; recycles it when it lives on without a heartbeat. Returns its exit code.
fn watch(child: &mut Child, stop: &std::sync::atomic::AtomicBool, info: &mut Info, poll: Duration, hung: Duration, began: Instant) -> Option<i64> {
    let mut silent = 0;
    let mut last_poll = Instant::now();
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
            return child.wait().ok().and_then(|s| s.code()).map(|c| c as i64);
        }
        match child.try_wait() {
            Ok(Some(st)) => return st.code().map(|c| c as i64),
            Ok(None) => {}
            Err(_) => return None,
        }
        if last_poll.elapsed() >= poll {
            last_poll = Instant::now();
            write_status(info);
            // not up long enough to have beaten: nothing to judge
            if began.elapsed() >= hung {
                // any pid's beat counts: a venv's launcher runs the real interpreter as its child, which writes the heartbeat
                if matches!(heartbeat().1, Some(age) if age < hung) {
                    silent = 0;
                } else {
                    silent += 1;
                    if silent >= 2 {
                        silent = 0;
                        info.hung_restarts += 1;
                        paths::log("WARN", &format!("slack bridge: alive but no heartbeat for {}s: killing it, to restart", hung.as_secs()));
                        let _ = child.kill();
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
