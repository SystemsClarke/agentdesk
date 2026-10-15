//! Self-update from GitHub Releases (`SystemsClarke/agentdesk`): `agentdesk update [--apply]`, and a quiet check every
//! four hours while the window or the daemon runs. A newer release is only ever reported, never applied, without
//! `--apply` (or `U` in the window): swapping the binary under a running Claude Code session is the user's call.

use crate::paths;
use anyhow::Result;
use serde_json::{json, Value};

pub const OWNER: &str = "SystemsClarke";
pub const REPO: &str = "agentdesk";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn updater(tag: &str) -> Result<Box<dyn self_update::update::ReleaseUpdate>> {
    Ok(self_update::backends::github::Update::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name("agentdesk")
        .current_version(CURRENT)
        .target_version_tag(tag)
        .show_download_progress(false)
        .show_output(false)
        .no_confirm(true)
        .build()?)
}

/// The newest release that ships a build for this OS and CPU: (tag, version without the `v`). The repo's older
/// releases are the .NET app's installers, which have no such asset and are skipped.
fn newest() -> Result<(String, String)> {
    let target = self_update::get_target();
    let releases = self_update::backends::github::ReleaseList::configure().repo_owner(OWNER).repo_name(REPO).build()?.fetch()?;
    let mut best: Option<(String, String)> = None;
    for r in releases {
        if !r.assets.iter().any(|a| a.name.starts_with("agentdesk-") && a.name.contains(target)) {
            continue;
        }
        let v = r.version.trim_start_matches('v').to_string();
        if best.as_ref().map(|(_, b)| self_update::version::bump_is_greater(b, &v).unwrap_or(false)).unwrap_or(true) {
            best = Some((r.name.clone(), v));
        }
    }
    match best {
        Some((_, v)) => Ok((format!("v{v}"), v)),
        None => anyhow::bail!("no release with a build for {target} yet"),
    }
}

/// The newest release's version (without the `v`), and whether it is newer than this build.
pub fn check() -> Result<(String, bool)> {
    let (_, latest) = newest()?;
    let newer = self_update::version::bump_is_greater(CURRENT, &latest).unwrap_or(false);
    Ok((latest, newer))
}

/// Downloads the newest release for this OS and CPU and swaps it in. Returns the version now installed.
///
/// A failed swap must never leave the install without a working binary: Claude Code's MCP config and hooks name
/// this exact path, so a missing file takes the board off every session. The running build is copied aside first,
/// and if the swap errors or the file at this path does not run and report the new version, the copy goes back.
pub fn apply() -> Result<String> {
    let exe = std::env::current_exe()?;
    let keep = rollback_copy(&exe);
    std::fs::copy(&exe, &keep)?;
    let (tag, _) = newest()?;
    let swapped = updater(&tag)?.update();
    let want = swapped.as_ref().ok().map(|s| s.version().trim_start_matches('v').to_string());
    if let Some(v) = want.as_deref().filter(|v| works(&exe, v)) {
        return Ok(v.to_string());
    }
    if !works(&exe, CURRENT) {
        restore(&keep, &exe)?;
        paths::log("WARN", &format!("update to {tag} failed; put {CURRENT} back at {}", exe.display()));
    }
    match swapped {
        Err(e) => anyhow::bail!("update failed ({e}); kept {CURRENT}"),
        Ok(_) => anyhow::bail!("the new binary did not run as {tag}; kept {CURRENT}"),
    }
}

/// Where the running build is parked during a swap: next to it, so the copy is on the same volume.
fn rollback_copy(exe: &std::path::Path) -> std::path::PathBuf {
    exe.with_extension("rollback")
}

/// True when the file at `exe` exists and answers `--version` with `agentdesk <version>`.
fn works(exe: &std::path::Path, version: &str) -> bool {
    std::process::Command::new(exe)
        .arg("--version")
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == format!("agentdesk {version}"))
        .unwrap_or(false)
}

/// Puts the parked copy back at `exe`, replacing whatever is there (a half-written file, or nothing).
fn restore(keep: &std::path::Path, exe: &std::path::Path) -> Result<()> {
    if exe.exists() {
        let _ = std::fs::remove_file(exe);
    }
    std::fs::copy(keep, exe)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_brings_back_a_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("agentdesk.exe");
        let keep = rollback_copy(&exe);
        std::fs::write(&keep, b"old build").unwrap();
        assert!(!works(&exe, CURRENT));
        restore(&keep, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"old build");
    }

    #[test]
    fn restore_replaces_a_broken_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("agentdesk.exe");
        let keep = rollback_copy(&exe);
        std::fs::write(&keep, b"old build").unwrap();
        std::fs::write(&exe, b"half written").unwrap();
        assert!(!works(&exe, CURRENT));
        restore(&keep, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"old build");
    }
}

fn state_file() -> std::path::PathBuf {
    paths::data_dir().join("update.json")
}

/// Records what the last check found, for the window's header. Failures are logged, never raised.
pub fn check_and_record() {
    let doc = match check() {
        Ok((latest, newer)) => json!({"latest": latest, "newer": newer, "current": CURRENT, "checked_ts": crate::db::now_iso()}),
        Err(e) => {
            paths::log("WARN", &format!("update check failed: {e}"));
            return;
        }
    };
    let _ = std::fs::create_dir_all(paths::data_dir());
    let _ = std::fs::write(state_file(), doc.to_string());
}

/// The version waiting to be installed, if the last check found one newer than the running build.
pub fn pending() -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(state_file()).ok()?).ok()?;
    let latest = v["latest"].as_str()?;
    self_update::version::bump_is_greater(CURRENT, latest).ok()?.then(|| latest.to_string())
}
