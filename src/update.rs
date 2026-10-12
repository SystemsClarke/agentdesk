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
pub fn apply() -> Result<String> {
    let (tag, _) = newest()?;
    let status = updater(&tag)?.update()?;
    Ok(status.version().trim_start_matches('v').to_string())
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
