//! The core: the one `agentdesk daemon` per user that owns every hosted session, the jobs, the governor, the Concierge, the Slack bridge
//! and the merge-list check. The terminal window, the tray and the CLI are clients of the board (and of the attach socket); none of them
//! runs the service threads, so there is never a second owner of the sessions. A client that needs the core starts it (`ensure`).
//!
//! Single instance is a lock on `core.lock` held for the life of the process, so it is released by the OS however the daemon dies.
//! `core.json` says who holds it (pid, version, start time) and is only believed while the lock is held.

use crate::paths;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub pid: u32,
    pub version: String,
    pub started: String,
    pub exe: String,
}

fn lock_path() -> PathBuf {
    paths::data_dir().join("core.lock")
}

fn info_path() -> PathBuf {
    paths::data_dir().join("core.json")
}

/// The file whose presence asks the running core to stop (so a client needs no signals, and it works the same on every OS).
pub fn stop_path() -> PathBuf {
    paths::data_dir().join("core.stop")
}

fn open_lock() -> std::io::Result<File> {
    std::fs::create_dir_all(paths::data_dir())?;
    std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path())
}

/// Takes the single-instance lock for this process, or None when a core already runs. Keep the file alive as long as the core is.
pub fn claim() -> Result<Option<File>> {
    let f = open_lock()?;
    match f.try_lock() {
        Ok(()) => {
            let me = json!({
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"),
                "started": crate::db::now_iso(),
                "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            });
            let tmp = info_path().with_extension("tmp");
            std::fs::write(&tmp, me.to_string())?;
            std::fs::rename(&tmp, info_path())?;
            let _ = std::fs::remove_file(stop_path()); // a stop asked of the previous core is not for this one
            Ok(Some(f))
        }
        Err(_) => Ok(None),
    }
}

/// Whether some process holds the core lock.
pub fn running() -> bool {
    let Ok(f) = open_lock() else { return false };
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(_) => true,
    }
}

/// The running core, if there is one.
pub fn status() -> Option<Info> {
    if !running() {
        return None;
    }
    let v: Value = serde_json::from_str(&std::fs::read_to_string(info_path()).ok()?).ok()?;
    Some(Info {
        pid: v["pid"].as_u64()? as u32,
        version: v["version"].as_str().unwrap_or("").to_string(),
        started: v["started"].as_str().unwrap_or("").to_string(),
        exe: v["exe"].as_str().unwrap_or("").to_string(),
    })
}

pub fn status_json() -> Value {
    match status() {
        Some(i) => json!({"running": true, "pid": i.pid, "version": i.version, "started": i.started, "exe": i.exe}),
        None => json!({"running": false}),
    }
}

/// Starts `<exe> daemon` detached from this process and its terminal.
fn spawn_detached(exe: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    {
        spawn_detached_windows(exe)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = crate::paths::new_command(exe);
        cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        cmd.process_group(0); // its own group: closing the terminal does not take it down
        cmd.spawn().map_err(|e| anyhow!("could not start the core: {e}"))?;
        Ok(())
    }
}

/// `CreateProcessW` with no handle inheritance: a child started the usual way inherits every inheritable handle of its parent, so a caller
/// that reads our output through a pipe (a script, a test) would wait for the core to exit. It also leaves the caller's job object when it
/// is allowed to (a Claude Code tool call ends its children), so the core outlives whoever started it.
#[cfg(windows)]
fn spawn_detached_windows(exe: &std::path::Path) -> Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let mut cmdline: Vec<u16> = format!("\"{}\" daemon", exe.display()).encode_utf16().chain(Some(0)).collect();
    let try_flags = |flags: u32, cmdline: &mut Vec<u16>| -> bool {
        // SAFETY: plain FFI with zeroed, correctly sized structs; the handles it returns are closed at once.
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(std::ptr::null(), cmdline.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, flags, std::ptr::null(), std::ptr::null(), &si, &mut pi);
            if ok != 0 {
                CloseHandle(pi.hProcess);
                CloseHandle(pi.hThread);
            }
            ok != 0
        }
    };
    let base = DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    if try_flags(base | CREATE_BREAKAWAY_FROM_JOB, &mut cmdline) || try_flags(base, &mut cmdline) {
        Ok(())
    } else {
        Err(anyhow!("could not start the core: {}", std::io::Error::last_os_error()))
    }
}

/// The core, started if it is not running; waits up to `wait` for it to hold its lock.
pub fn ensure(wait: Duration) -> Result<Info> {
    let exe = std::env::var("AGENTDESK_CORE_EXE").ok().filter(|e| !e.is_empty()).map(PathBuf::from).map(Ok).unwrap_or_else(std::env::current_exe)?;
    ensure_at(&exe, wait)
}

/// Like `ensure`, starting the core from `exe`: after a self-update the running program's own path may name the parked old build.
pub fn ensure_at(exe: &std::path::Path, wait: Duration) -> Result<Info> {
    if let Some(i) = status() {
        return Ok(i);
    }
    spawn_detached(exe)?;
    let start = Instant::now();
    while start.elapsed() < wait {
        if let Some(i) = status() {
            return Ok(i);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow!("the core did not start within {} s (see {})", wait.as_secs(), paths::log_file().display()))
}

/// Asks the core to stop and waits for it; if it does not, ends the process. True if a core was running.
pub fn stop(wait: Duration) -> Result<bool> {
    let Some(info) = status() else { return Ok(false) };
    std::fs::write(stop_path(), b"stop")?;
    let start = Instant::now();
    while start.elapsed() < wait {
        if !running() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    kill(info.pid);
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_file(stop_path());
    Ok(true)
}

fn kill(pid: u32) {
    let _ = if cfg!(windows) {
        crate::paths::new_command("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output()
    } else {
        crate::paths::new_command("kill").args(["-9", &pid.to_string()]).output()
    };
}

/// `agentdesk daemon`: become the core, or say who already is.
pub fn run_daemon() -> Result<()> {
    let Some(_lock) = claim()? else {
        match status() {
            Some(i) => println!("agentdesk core already running (pid {}, {} since {})", i.pid, i.version, i.started),
            None => println!("agentdesk core already running"),
        }
        return Ok(());
    };
    paths::log("INFO", &format!("core starting (pid {}, agentdesk {})", std::process::id(), env!("CARGO_PKG_VERSION")));
    println!("agentdesk core running on {}", paths::db_path().display());
    let stop = Arc::new(AtomicBool::new(false));
    crate::service::run(stop);
    let _ = std::fs::remove_file(info_path());
    let _ = std::fs::remove_file(stop_path());
    paths::log("INFO", "core stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_identities::Env;

    #[test]
    fn one_core_per_data_folder_and_a_stop_is_a_file() {
        let _e = Env::new();
        assert!(status().is_none() && !running());
        let first = claim().unwrap().expect("the first claim wins");
        assert!(running());
        let me = status().expect("core.json is believed while the lock is held");
        assert_eq!((me.pid, me.version.as_str()), (std::process::id(), env!("CARGO_PKG_VERSION")));
        assert!(claim().unwrap().is_none(), "a second core is refused");
        std::fs::write(stop_path(), b"stop").unwrap();
        drop(first);
        assert!(!running() && status().is_none(), "the lock goes with the process");
        let again = claim().unwrap();
        assert!(again.is_some() && !stop_path().exists(), "a new core starts clean: the old stop request is gone");
    }
}
