//! `agentdesk migrate`: moves a machine from the .NET AgentDesk to this one without losing the board.
//!
//! The board file is the same SQLite database, so nothing is converted: the migration backs it up, brings its schema
//! up to date, and points Claude Code at this binary (the MCP server entry in `~/.claude.json` and the hooks in
//! `~/.claude/settings.json`). It does nothing without `--apply`, backs up every file it edits, and only touches
//! entries that mention agentdesk. It never stops or uninstalls the old app: it says how.

use crate::{db::Db, paths};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn exe() -> String {
    std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "agentdesk".into())
}

fn backup_file(path: &Path, tag: &str) -> Result<PathBuf> {
    let dir = paths::data_dir().join("migrate_backups");
    std::fs::create_dir_all(&dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let to = dir.join(format!("{}-{tag}-{name}", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
    std::fs::copy(path, &to)?;
    Ok(to)
}

/// Rewrites the `agentdesk` MCP server entries of a `.claude.json` (top level and per project). Returns what changed.
pub fn patch_mcp(root: &mut Value, exe: &str) -> Vec<String> {
    let mut changes = Vec::new();
    let mut fix = |servers: &mut Value, where_: &str| {
        let Some(map) = servers.as_object_mut() else { return };
        for (name, entry) in map.iter_mut() {
            if !(name.eq_ignore_ascii_case("agentdesk") || (entry["command"].as_str().map(|c| c.to_lowercase().contains("agentdesk")).unwrap_or(false))) {
                continue;
            }
            let (old_cmd, old_args) = (entry["command"].clone(), entry["args"].clone());
            if entry["command"].as_str() == Some(exe) && entry["args"] == json!(["mcp"]) {
                continue;
            }
            entry["command"] = json!(exe);
            entry["args"] = json!(["mcp"]);
            if entry.get("type").is_none() {
                entry["type"] = json!("stdio");
            }
            changes.push(format!("{where_}: MCP server `{name}` was {old_cmd} {old_args}, now \"{exe}\" mcp"));
        }
    };
    if let Some(s) = root.get_mut("mcpServers") {
        fix(s, "~/.claude.json");
    }
    if let Some(projects) = root.get_mut("projects").and_then(|p| p.as_object_mut()) {
        for (dir, p) in projects.iter_mut() {
            if let Some(s) = p.get_mut("mcpServers") {
                fix(s, &format!("project {dir}"));
            }
        }
    }
    changes
}

/// Rewrites hooks that run the old `agentdesk hook <event>` so they run this binary.
pub fn patch_hooks(root: &mut Value, exe: &str) -> Vec<String> {
    let mut changes = Vec::new();
    let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else { return changes };
    for (event, groups) in hooks.iter_mut() {
        for group in groups.as_array_mut().into_iter().flatten() {
            for h in group["hooks"].as_array_mut().into_iter().flatten() {
                let Some(cmd) = h["command"].as_str().map(String::from) else { continue };
                if !cmd.to_lowercase().contains("agentdesk") {
                    continue;
                }
                let Some(pos) = cmd.find(" hook ") else { continue };
                let ev = cmd[pos + 6..].split_whitespace().next().unwrap_or("");
                if ev.is_empty() {
                    continue;
                }
                let new = format!("\"{exe}\" hook {ev}");
                if cmd != new {
                    h["command"] = json!(new);
                    changes.push(format!("{event} hook: `{cmd}` -> `{new}`"));
                }
            }
        }
    }
    changes
}

fn edit_json(path: &Path, apply: bool, tag: &str, patch: impl Fn(&mut Value) -> Vec<String>, report: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        report.push(format!("  {} not found: nothing to change there", path.display()));
        return;
    };
    let mut v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            report.push(format!("  ! {} is not valid JSON ({e}); not touched", path.display()));
            return;
        }
    };
    let changes = patch(&mut v);
    if changes.is_empty() {
        report.push(format!("  {}: already points here (or has no agentdesk entries)", path.display()));
        return;
    }
    for c in &changes {
        report.push(format!("  {}{c}", if apply { "changed " } else { "would change " }));
    }
    if apply {
        match backup_file(path, tag).and_then(|b| {
            let tmp = path.with_extension("agentdesk-tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&v)? + "\n")?;
            std::fs::rename(&tmp, path)?;
            Ok(b)
        }) {
            Ok(b) => report.push(format!("  backup: {}", b.display())),
            Err(e) => report.push(format!("  ! could not write {}: {e}", path.display())),
        }
    }
}

fn old_core_running() -> bool {
    let out = if cfg!(windows) {
        crate::paths::new_command("tasklist").args(["/FI", "IMAGENAME eq AgentDesk.Core.exe", "/NH"]).output()
    } else {
        crate::paths::new_command("pgrep").args(["-f", "AgentDesk.Core"]).output()
    };
    out.map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("agentdesk.core") || (!cfg!(windows) && o.status.success())).unwrap_or(false)
}

/// `claude_home` and `claude_json` are parameters so the tests can use a scratch home.
pub fn run(apply: bool, claude_json: &Path, claude_settings: &Path) -> Result<String> {
    let mut r: Vec<String> = Vec::new();
    let exe = exe();
    r.push(format!("agentdesk migrate{}\n", if apply { "" } else { " (dry run: pass --apply to make the changes)" }));

    r.push("1. The board".into());
    let db_path = paths::db_path();
    if db_path.exists() {
        let db = Db::open(&db_path)?;
        let (t, m) =
            (db.scalar_i64("SELECT COUNT(*) FROM threads", &[]).ok().flatten().unwrap_or(0), db.scalar_i64("SELECT COUNT(*) FROM messages", &[]).ok().flatten().unwrap_or(0));
        r.push(format!("  found {} with {t} threads and {m} messages: the same database, nothing is converted", db_path.display()));
        if apply {
            let dir = paths::data_dir().join("migrate_backups");
            std::fs::create_dir_all(&dir)?;
            let b = dir.join(format!("{}-pre-rust-agentdesk.db", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
            db.exec(&format!("VACUUM INTO '{}'", b.to_string_lossy().replace('\'', "''")), &[])?;
            r.push(format!("  backup: {}", b.display()));
            db.init()?;
            r.push("  schema brought up to date (additive only: the .NET app can still open it)".into());
        }
    } else {
        r.push(format!("  no board at {} yet: a fresh one is made on first use (set AGENTDESK_DATA if the old one lives elsewhere)", db_path.display()));
    }

    r.push("\n2. Claude Code: MCP server".into());
    edit_json(claude_json, apply, "mcp", |v| patch_mcp(v, &exe), &mut r);
    r.push("\n3. Claude Code: hooks".into());
    edit_json(claude_settings, apply, "hooks", |v| patch_hooks(v, &exe), &mut r);

    r.push("\n4. The old app (not touched by this command)".into());
    if old_core_running() {
        r.push("  ! the .NET core appears to be running: quit it from its tray icon before you rely on this one (both would run toasts, jobs and the merge list)".into());
    }
    if cfg!(windows) {
        let old = dirs::data_local_dir().unwrap_or_default().join("AgentDeskApp");
        if old.exists() {
            r.push(format!("  the .NET install is at {}", old.display()));
            r.push(format!(
                "  to remove it later: \"{}\" --uninstall  (this also takes its `agentdesk` off the PATH, so put this binary's folder on the PATH first)",
                old.join("Update.exe").display()
            ));
        }
    }
    r.push("  What the old app kept (Slack credentials, goals, identities, the usage governor, swarm slots) is read as it is: this build runs all of it. Start `agentdesk daemon` (or `agentdesk tui`) once the old core is quit.".into());
    r.push(format!("\nThis binary: {exe}\nAfter --apply, restart Claude Code sessions so they reconnect to the new MCP server."));
    Ok(r.join("\n"))
}
