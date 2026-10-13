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

/// The `claude` to run: `AGENTDESK_CLAUDE`, else one on PATH, else the usual install spots, else the copy the Claude desktop app keeps
/// (`<config>/Claude/claude-code/<version>/<id>/claude`, newest version), which is on nobody's PATH. Falls back to the bare name.
pub fn claude_program() -> String {
    if let Some(c) = std::env::var("AGENTDESK_CLAUDE").ok().filter(|c| !c.is_empty()) {
        return c;
    }
    find_claude(&std::env::var_os("PATH").unwrap_or_default(), &home_dir(), dirs::config_dir())
}

fn find_claude(path: &std::ffi::OsStr, home: &std::path::Path, config: Option<PathBuf>) -> String {
    let exe = if cfg!(windows) { "claude.exe" } else { "claude" };
    let is_exe = |p: &std::path::Path| p.is_file();
    for d in std::env::split_paths(path) {
        for n in if cfg!(windows) { vec!["claude.exe", "claude.cmd"] } else { vec!["claude"] } {
            if is_exe(&d.join(n)) {
                return "claude".into();
            }
        }
    }
    let mut spots = vec![
        home.join(".local").join("bin").join(exe),
        home.join(".claude").join("local").join(exe),
        home.join(".claude").join("bin").join(exe),
        PathBuf::from("/usr/local/bin/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
    ];
    if cfg!(windows) {
        if let Some(a) = std::env::var_os("APPDATA") {
            spots.push(PathBuf::from(a).join("npm").join("claude.cmd"));
        }
    }
    if let Some(p) = spots.into_iter().find(|p| is_exe(p)) {
        return p.display().to_string();
    }
    let mut bases: Vec<PathBuf> = vec![];
    if let Some(c) = config {
        bases.push(c.join("Claude").join("claude-code"));
    }
    bases.push(home.join(".config").join("Claude").join("claude-code"));
    bases.push(home.join("Library").join("Application Support").join("Claude").join("claude-code"));
    let mut found: Vec<(Vec<u64>, PathBuf)> = vec![];
    for b in bases {
        for v in std::fs::read_dir(&b).into_iter().flatten().flatten() {
            let key: Vec<u64> = v.file_name().to_string_lossy().split('.').map(|n| n.parse().unwrap_or(0)).collect();
            for id in std::fs::read_dir(v.path()).into_iter().flatten().flatten() {
                let c = id.path().join(exe);
                if is_exe(&c) {
                    found.push((key.clone(), c));
                }
            }
        }
    }
    found.sort();
    found.pop().map(|(_, p)| p.display().to_string()).unwrap_or_else(|| "claude".into())
}

/// A `std::process::Command` for `program` resolved by `resolve`.
pub fn command(program: &str) -> std::process::Command {
    let (p, pre) = resolve(program);
    let mut c = std::process::Command::new(p);
    c.args(pre);
    c
}

/// The user's home folder. `USERPROFILE` (Windows) or `HOME` win over the system's answer, so a scratch environment can move it.
pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(dirs::home_dir).unwrap_or_default()
}

#[cfg(all(test, unix))]
mod claude_tests {
    #[test]
    fn finds_the_desktop_apps_copy_when_it_is_not_on_path() {
        let home = std::env::temp_dir().join(format!("ad-claude-{}", std::process::id()));
        let new = home.join(".config/Claude/claude-code/2.1.295/abc");
        let old = home.join(".config/Claude/claude-code/2.1.9/def");
        for d in [&new, &old] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("claude"), "").unwrap();
        }
        assert_eq!(super::find_claude(std::ffi::OsStr::new("/nonexistent"), &home, Some(home.join(".config"))), new.join("claude").display().to_string());
        std::fs::remove_dir_all(&home).ok();
    }
}
