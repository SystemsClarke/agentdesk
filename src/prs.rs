//! The merge list: pull requests agents asked John to merge. A row leaves the list only when GitHub (through
//! `gh pr view`) says it merged or closed; a failed check leaves it open with `last_error`.

use crate::db::{now_iso, Db, AGENT};
use serde_json::{json, Value};

/// Checks every open PR once. Returns how many settled.
pub fn check(db: &Db) -> usize {
    let open = db.rows("SELECT * FROM pull_requests WHERE state='open' ORDER BY id", &[]).unwrap_or_default();
    let mut settled = 0;
    for pr in open {
        let (id, url) = (pr["id"].as_i64().unwrap_or(0), pr["url"].as_str().unwrap_or("").to_string());
        let out = crate::paths::command("gh").args(["pr", "view", &url, "--json", "state,title"]).output();
        let ts = now_iso();
        let parsed = match out {
            Ok(o) if o.status.success() => serde_json::from_slice::<Value>(&o.stdout).map_err(|e| e.to_string()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().chars().take(300).collect()),
            Err(e) => Err(format!("gh not available: {e}")),
        };
        match parsed {
            Err(e) => {
                let _ = db.exec("UPDATE pull_requests SET checked_ts=:ts, last_error=:e WHERE id=:id", &[(":ts", &ts), (":e", &e), (":id", &id)]);
            }
            Ok(v) => {
                let state = v["state"].as_str().unwrap_or("OPEN").to_lowercase();
                let title = v["title"].as_str().unwrap_or("").to_string();
                if !title.is_empty() {
                    let _ = db.exec("UPDATE pull_requests SET title=:t WHERE id=:id", &[(":t", &title), (":id", &id)]);
                }
                if state == "merged" || state == "closed" {
                    let _ =
                        db.exec("UPDATE pull_requests SET state=:s, checked_ts=:ts, settled_ts=:ts, last_error=NULL WHERE id=:id", &[(":s", &state), (":ts", &ts), (":id", &id)]);
                    if let Some(tid) = pr["thread_id"].as_i64() {
                        let kind = if state == "merged" { "pr-merged" } else { "pr-closed" };
                        let body = format!(
                            "{} {}#{} was {state}.",
                            if state == "merged" { "Merged:" } else { "Closed without merging:" },
                            pr["repo"].as_str().unwrap_or(""),
                            pr["number"]
                        );
                        let _ = db.reply(tid, "agentdesk", AGENT, &body, None, Some(&json!({"kind": kind})));
                    }
                    settled += 1;
                } else {
                    let _ = db.exec("UPDATE pull_requests SET checked_ts=:ts, last_error=NULL WHERE id=:id", &[(":ts", &ts), (":id", &id)]);
                }
            }
        }
    }
    settled
}

/// Why this PR must not be merged right now, or `Ok(())` when everything GitHub reports says it may be. `pr` is
/// `gh pr view --json state,isDraft,mergeable,mergeStateStatus,baseRefName,statusCheckRollup`; `default_branch` is the repo's.
fn verdict(pr: &Value, default_branch: &str) -> Result<(), String> {
    let state = pr["state"].as_str().unwrap_or("");
    if state != "OPEN" {
        return Err(format!("it is {}, not open", state.to_lowercase()));
    }
    if pr["isDraft"].as_bool().unwrap_or(true) {
        return Err("it is a draft".into());
    }
    let base = pr["baseRefName"].as_str().unwrap_or("");
    if base != default_branch {
        return Err(format!("it targets '{base}', not the default branch '{default_branch}'"));
    }
    for c in pr["statusCheckRollup"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().or_else(|| c["context"].as_str()).unwrap_or("a check");
        // A check run has status + conclusion; a commit status has only a state.
        let result = c["conclusion"].as_str().filter(|s| !s.is_empty()).or_else(|| c["state"].as_str()).unwrap_or("");
        match result {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "" | "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" => return Err(format!("{name} has not finished")),
            other => return Err(format!("{name} is {}", other.to_lowercase())),
        }
    }
    if pr["mergeable"].as_str() != Some("MERGEABLE") || pr["mergeStateStatus"].as_str() != Some("CLEAN") {
        return Err(format!(
            "GitHub says mergeable={} mergeStateStatus={}, not MERGEABLE and CLEAN",
            pr["mergeable"].as_str().unwrap_or("?"),
            pr["mergeStateStatus"].as_str().unwrap_or("?")
        ));
    }
    Ok(())
}

fn gh(args: &[&str]) -> Result<String, String> {
    let o = crate::paths::command("gh").args(args).output().map_err(|e| format!("gh not available: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().chars().take(400).collect())
    }
}

/// `agentdesk merge <pr-url>`: merges the PR through `gh` only if it is open, not a draft, aimed at the repo's default branch,
/// has every check green and GitHub calls it MERGEABLE and CLEAN. Never `--admin`, so branch protection still applies.
/// This is the command a permission rule (`Bash(agentdesk merge:*)`) authorizes; the checks are what keep that rule narrow.
pub fn merge(url: &str, squash: bool, dry_run: bool) -> Result<String, String> {
    let re = regex::Regex::new(r"(?i)^https?://(?:www\.)?github\.com/([A-Za-z0-9._-]+)/([A-Za-z0-9._-]+)/pull/(\d+)(?:[/?#].*)?$").unwrap();
    let m = re.captures(url.trim()).ok_or_else(|| format!("not a GitHub pull request URL: '{url}'"))?;
    let (repo, number) = (format!("{}/{}", &m[1], &m[2]), m[3].to_string());
    let json = gh(&["pr", "view", &number, "--repo", &repo, "--json", "state,isDraft,mergeable,mergeStateStatus,baseRefName,statusCheckRollup"])?;
    let pr: Value = serde_json::from_str(&json).map_err(|e| format!("unreadable gh output: {e}"))?;
    let default_branch = gh(&["repo", "view", &repo, "--json", "defaultBranchRef", "--jq", ".defaultBranchRef.name"])?;
    verdict(&pr, &default_branch).map_err(|why| format!("not merging {repo}#{number}: {why}"))?;
    if dry_run {
        return Ok(format!("{repo}#{number} would be merged ({}): open, not a draft, every check green, MERGEABLE and CLEAN", if squash { "squash" } else { "merge commit" }));
    }
    gh(&["pr", "merge", &number, "--repo", &repo, if squash { "--squash" } else { "--merge" }])?;
    Ok(format!("merged {repo}#{number} ({})", if squash { "squash" } else { "merge commit" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> Value {
        json!({"state": "OPEN", "isDraft": false, "baseRefName": "main", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
               "statusCheckRollup": [{"name": "test (ubuntu-latest)", "status": "COMPLETED", "conclusion": "SUCCESS"}, {"context": "ci/other", "state": "SUCCESS"}]})
    }

    #[test]
    fn a_green_clean_pr_may_merge() {
        assert_eq!(verdict(&clean(), "main"), Ok(()));
    }

    #[test]
    fn every_reason_to_wait_is_refused() {
        let mut cases: Vec<(Value, &str)> = vec![];
        let mut p = clean();
        p["state"] = json!("MERGED");
        cases.push((p, "not open"));
        let mut p = clean();
        p["isDraft"] = json!(true);
        cases.push((p, "draft"));
        let mut p = clean();
        p["baseRefName"] = json!("release");
        cases.push((p, "not the default branch"));
        let mut p = clean();
        p["statusCheckRollup"][0]["conclusion"] = json!("FAILURE");
        cases.push((p, "test (ubuntu-latest) is failure"));
        let mut p = clean();
        p["statusCheckRollup"][0] = json!({"name": "test (windows-latest)", "status": "IN_PROGRESS", "conclusion": ""});
        cases.push((p, "has not finished"));
        let mut p = clean();
        p["mergeable"] = json!("CONFLICTING");
        p["mergeStateStatus"] = json!("DIRTY");
        cases.push((p, "CONFLICTING"));
        let mut p = clean();
        p["mergeStateStatus"] = json!("BLOCKED");
        cases.push((p, "BLOCKED"));
        for (pr, want) in cases {
            let got = verdict(&pr, "main").unwrap_err();
            assert!(got.contains(want), "wanted '{want}' in '{got}'");
        }
    }

    #[test]
    fn a_missing_field_is_never_read_as_permission() {
        assert!(verdict(&json!({}), "main").is_err());
        assert!(verdict(&json!({"state": "OPEN"}), "main").is_err(), "no isDraft means refuse");
    }

    #[test]
    fn only_github_pull_urls_are_accepted() {
        assert!(merge("https://example.com/o/r/pull/1", false, true).unwrap_err().contains("not a GitHub pull request URL"));
        assert!(merge("https://github.com/o/r/issues/1", false, true).is_err());
    }
}
