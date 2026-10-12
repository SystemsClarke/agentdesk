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
    dirs::home_dir().unwrap_or_default().join(".claude").join("settings.json")
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
