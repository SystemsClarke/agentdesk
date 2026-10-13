//! Is the Slack bridge up? The window asks on every refresh, so the answer must not cost a process.
//!
//! `bridge::running()` checks the heartbeat's pid with `tasklist` on Windows: a whole process spawn, measured at about 300 ms
//! here, and the window used to ask it on every frame (twice on the SysOp screen). This does the same check (a fresh heartbeat
//! whose pid is alive) with one `OpenProcess` call on Windows. Elsewhere it keeps `bridge::pid_alive`, which is a cheap `kill -0`
//! there, and the refresh runs on a worker thread, never in a frame.

use chrono::{DateTime, Utc};
use serde_json::Value;

/// A heartbeat older than this is a bridge that stopped (the same bound as `bridge::FRESH`).
const FRESH_SECS: i64 = 90;

#[cfg(windows)]
fn pid_alive(pid: i64) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    const STILL_ACTIVE: u32 = 259;
    let Ok(pid) = u32::try_from(pid) else { return false };
    if pid == 0 {
        return false;
    }
    // SAFETY: plain FFI; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // an elevated process we may not open still exists
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: i64) -> bool {
    crate::bridge::pid_alive(pid)
}

/// `bridge::running()` for the window: the heartbeat file (`slack_bridge.state`) is fresh and names a live pid.
pub fn slack_up(heartbeat: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(heartbeat) else { return false };
    let Some(ts) = v["ts"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()) else { return false };
    let fresh = (Utc::now() - ts.with_timezone(&Utc)).num_seconds() < FRESH_SECS;
    fresh && v["pid"].as_i64().map(pid_alive).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_a_made_up_pid_is_not() {
        assert!(pid_alive(i64::from(std::process::id())));
        assert!(!pid_alive(0));
        assert!(!pid_alive(-5));
        assert!(!pid_alive(4_000_000_000));
    }

    #[test]
    fn a_fresh_heartbeat_with_a_live_pid_is_up_and_a_stale_one_is_not() {
        let now = Utc::now();
        let beat = |t: DateTime<Utc>, pid: u32| format!(r#"{{"ts":"{}","pid":{pid}}}"#, t.to_rfc3339());
        assert!(slack_up(&beat(now, std::process::id())));
        assert!(!slack_up(&beat(now - chrono::Duration::seconds(300), std::process::id())));
        assert!(!slack_up("not json"));
        assert!(!slack_up(r#"{"pid":1}"#));
    }
}
