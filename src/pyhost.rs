//! The Python half of AgentDesk, as in the original: the Slack bridge, the vault plugin (mirroring wiki posts into the vault, searching
//! it) and dictation. The sources are embedded in this binary (`python/`) and extracted to the data folder, so an updated binary brings
//! its Python with it. An interpreter is looked for in this order: `AGENTDESK_PYTHON_BIN`, `<data>/venv`, then `python3`, `python`, `py`
//! on the PATH. Without one, or without a module's packages, the Rust implementation answers where it has one.

use crate::paths;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

include!(concat!(env!("OUT_DIR"), "/python_files.rs"));

/// The extracted `python/` folder (made on first use, and again when the embedded files change).
pub fn dir() -> Result<PathBuf> {
    let root = paths::data_dir().join("python").join(HASH);
    if root.join(".complete").exists() {
        return Ok(root);
    }
    for (rel, bytes) in FILES {
        let to = root.join(rel);
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&to, bytes)?;
    }
    std::fs::write(root.join(".complete"), HASH)?;
    // older extractions are not needed any more
    for old in std::fs::read_dir(paths::data_dir().join("python")).into_iter().flatten().flatten() {
        if old.file_name().to_string_lossy() != HASH {
            let _ = std::fs::remove_dir_all(old.path());
        }
    }
    Ok(root)
}

/// A Python 3 interpreter, if there is one.
pub fn interpreter() -> Option<String> {
    if let Some(p) = std::env::var("AGENTDESK_PYTHON_BIN").ok().filter(|p| !p.is_empty()) {
        return Some(p);
    }
    let venv = paths::data_dir().join("venv");
    for rel in ["bin/python3", "bin/python", "Scripts/python.exe"] {
        let p = venv.join(rel);
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    for c in ["python3", "python", "py"] {
        if Command::new(c).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false) {
            return Some(c.to_string());
        }
    }
    None
}

/// A `python` command with the extracted package importable and the board's folders passed through.
pub fn command() -> Result<Command> {
    let py = interpreter().ok_or_else(|| anyhow!("no Python 3 interpreter found (set AGENTDESK_PYTHON_BIN)"))?;
    let root = dir()?;
    let mut c = Command::new(py);
    c.current_dir(&root).env("PYTHONPATH", &root).env("PYTHONIOENCODING", "utf-8").env("AGENTDESK_DATA", paths::data_dir()).env("AGENTDESK_VAULT", paths::vault_dir());
    if let Ok(exe) = std::env::current_exe() {
        c.env("AGENTDESK_EXE", exe);
    }
    Ok(c)
}

struct Plugin {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
}

static HOST: std::sync::Mutex<Option<Plugin>> = std::sync::Mutex::new(None);

fn spawn_plugin() -> Result<Plugin> {
    let mut child = command()?.args(["-m", "agentdesk.plugin"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    Ok(Plugin { child, stdin, lines })
}

/// One request to the plugin (`python -m agentdesk.plugin`, JSON-RPC 2.0 on stdio, one long-lived process per agentdesk process, like the
/// original's plugin host, because dictation holds a microphone between calls): the reply's `result`, or an error saying what broke.
pub fn call(method: &str, params: Value) -> Result<Value> {
    let mut guard = HOST.lock().unwrap_or_else(|e| e.into_inner());
    for attempt in 0..2 {
        if guard.as_mut().map(|p| matches!(p.child.try_wait(), Ok(Some(_)))).unwrap_or(true) {
            *guard = Some(spawn_plugin()?);
        }
        let p = guard.as_mut().unwrap();
        while p.lines.try_recv().is_ok() {} // a stale reply from an abandoned call
        if writeln!(p.stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).and_then(|_| p.stdin.flush()).is_err() {
            *guard = None;
            if attempt == 0 {
                continue;
            }
            return Err(anyhow!("the Python plugin is not running"));
        }
        return match p.lines.recv_timeout(std::time::Duration::from_secs(120)) {
            Ok(line) => {
                let reply: Value = serde_json::from_str(line.trim()).map_err(|_| anyhow!("the Python plugin answered with something that is not JSON"))?;
                match reply["error"]["message"].as_str() {
                    Some(e) => Err(anyhow!("{e}")),
                    None => Ok(reply["result"].clone()),
                }
            }
            Err(_) => {
                if let Some(mut p) = guard.take() {
                    let _ = p.child.kill();
                }
                Err(anyhow!("the Python plugin did not answer (are its packages installed? see python/requirements.txt)"))
            }
        };
    }
    Err(anyhow!("the Python plugin is not running"))
}

/// Ends the plugin (its microphone, its model) when this process is done.
pub fn shutdown() {
    if let Some(mut p) = HOST.lock().unwrap_or_else(|e| e.into_inner()).take() {
        drop(p.stdin);
        let _ = p.child.kill();
        let _ = p.child.wait();
    }
}
