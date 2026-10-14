//! Where things live. The board's data is kept outside any repo and outside synced folders on purpose:
//! a file-sync client copying a WAL out from under a writer is how SQLite databases get corrupted.

use std::path::PathBuf;

/// `AGENTDESK_DATA`, else the per-user local data directory's `AgentDesk` folder.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AGENTDESK_DATA").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("AgentDesk")
}

pub fn db_path() -> PathBuf {
    data_dir().join("agentdesk.db")
}

/// The memory vault: a folder of Markdown notes (`AGENTDESK_VAULT` overrides it).
pub fn vault_dir() -> PathBuf {
    std::env::var_os("AGENTDESK_VAULT").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| data_dir().join("vault"))
}

pub fn backup_dir() -> PathBuf {
    data_dir().join("backups")
}

pub fn usage_file() -> PathBuf {
    data_dir().join("claude_usage.json")
}

pub fn log_file() -> PathBuf {
    data_dir().join("core.log")
}

/// Claude Code's user settings (permission rules are written here on approval).
pub fn claude_settings() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

pub fn log(level: &str, msg: &str) {
    if cfg!(test) {
        return;
    }
    use std::io::Write;
    let _ = std::fs::create_dir_all(data_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_file()) {
        let _ = writeln!(f, "{} {level} {msg}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
    }
}

/// Finds `program` the way a shell would. On Windows that includes the PATHEXT extensions, and a `.cmd`/`.bat` script (an npm-installed
/// `claude`, a stand-in) is run through `cmd /d /c`, which `CreateProcess` will not do on its own. Returns the program and the
/// arguments to put in front of the caller's. A name that is not found is returned as it is, so the spawn error names it.
pub fn resolve(program: &str) -> (String, Vec<String>) {
    if !cfg!(windows) {
        return (program.to_string(), vec![]);
    }
    let exts: Vec<String> = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into()).split(';').map(|e| e.to_lowercase()).collect();
    let has_ext = std::path::Path::new(program).extension().is_some();
    let mut candidates: Vec<PathBuf> = vec![];
    let dirs: Vec<PathBuf> =
        if program.contains(std::path::is_separator) { vec![PathBuf::new()] } else { std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect() };
    for d in dirs {
        let base = d.join(program);
        if has_ext {
            candidates.push(base.clone());
        }
        for e in &exts {
            candidates.push(PathBuf::from(format!("{}{e}", base.display())));
        }
    }
    for c in candidates {
        if c.is_file() {
            let ext = c.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            return if ext == "cmd" || ext == "bat" { ("cmd".into(), vec!["/d".into(), "/c".into(), c.display().to_string()]) } else { (c.display().to_string(), vec![]) };
        }
    }
    (program.to_string(), vec![])
}

/// Stops a child process from opening a console window of its own (Windows): `gh`, `git`, `powershell` and the rest would otherwise
/// flash one each time the core or the tray runs them. A no-op elsewhere.
pub fn hide(c: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// `std::process::Command::new(program)` that opens no window.
pub fn new_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    hide(&mut c);
    c
}

/// A `std::process::Command` for `program` resolved by `resolve`, opening no window.
pub fn command(program: &str) -> std::process::Command {
    let (p, pre) = resolve(program);
    let mut c = new_command(p);
    c.args(pre);
    c
}

/// The user's home folder. `USERPROFILE` (Windows) or `HOME` win over the system's answer, so a scratch environment can move it.
pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(dirs::home_dir).unwrap_or_default()
}
