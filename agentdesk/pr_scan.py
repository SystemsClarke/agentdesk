"""GitHub-wide PR discovery and Ladder triage for the merge list (item #115).

Two independent, both best-effort passes, meant to share the pull-request
watcher's thread and interval rather than get a UI of their own:

`scan_github()` asks `gh` what John is assigned to, mentioned on, or has been
asked to review, across every repo he can see -- not just the ones an agent
registered with request_merge. Anything new is added to the same
pull_requests table used by request_merge, tagged source=github-scan, so nothing
that lands on his account silently misses the tab just because no agent
called the tool. Reconciled by URL: prs.parse_pr_url's canonical form is the
key, exactly like request_merge, so a PR an agent DID register never
duplicates when the scan also finds it.

`triage_open()` asks Ladder to classify each open PR's real state (title,
body, review/CI state, who is waiting on whom) into a small set of buckets a
human can scan in one glance. It is deliberately NOT a hard dependency: any
failure here -- Ladder not running, the local model not warm, a malformed
reply -- must leave the PR showing with no label, never break the pass that
found it. See `_triage_one`'s except clause for where that promise lives.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Optional

from . import db, paths, prs

_GH_TIMEOUT = 30

# Buckets triage_open() is allowed to write. Anything else Ladder returns is
# treated the same as "no answer" -- a free-text label that doesn't match one
# of these would render as an unlabelled row rather than as a lie the tab
# just repeats.
TRIAGE_LABELS = ("needs-merge-now", "fyi", "already-handled", "stale")

# Where the ladder package lives on this machine. Not pip-installed, so it is
# reached the same way agentdesk/mcp/ladder_mcp.py reaches it: put its parent
# on sys.path and import it directly, in-process, no HTTP server required.
_LADDER_PARENT = Path(r"C:\Users\palencharj\NoOneDrive\LocalBuildFastCode")

# Off: triage runs on Ladder rung 0, which is Ollama, and John does not want
# Ollama used (2026-09-22). PRs are still discovered and tracked; they just
# carry no triage label until this runs on something else.
TRIAGE_ENABLED = False


def _gh_json(args: list, timeout: int = _GH_TIMEOUT):
    """Run a `gh` command that prints JSON. None on ANY failure -- never raises.

    This module's failures must never look like GitHub's answers: a `gh` that
    is missing, times out, or returns non-zero all read here as "found
    nothing this pass", not as "found zero PRs", so the caller must not use
    None to settle anything -- only to skip adding rows this time.
    """
    exe = shutil.which("gh")
    if exe is None:
        return None
    try:
        proc = subprocess.run(
            [exe, *args], capture_output=True, text=True, timeout=timeout,
            encoding="utf-8", errors="replace", **prs._no_window())
    except (subprocess.TimeoutExpired, OSError):
        return None
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout)
    except ValueError:
        return None


def _from_notifications() -> list[dict]:
    """PRs from `gh api notifications`: assigned, mentioned, review-requested,
    or anything else GitHub decided to tell this account about.

    `subject.url` is the REST API url (.../pulls/<n>), not the html one, so
    the PR page url is rebuilt from repository.full_name + that number --
    the same "reconstruct, don't trust" rule prs.parse_pr_url follows for the
    URL an agent hands request_merge.
    """
    data = _gh_json(["api", "notifications", "--paginate"])
    if not isinstance(data, list):
        return []
    out = []
    for n in data:
        subject = n.get("subject") or {}
        if subject.get("type") != "PullRequest":
            continue
        repo_full = ((n.get("repository") or {}).get("full_name") or "").strip()
        subj_url = subject.get("url") or ""
        try:
            number = int(subj_url.rstrip("/").rsplit("/", 1)[-1])
        except ValueError:
            continue
        if not repo_full or not number:
            continue
        out.append({
            "url": f"https://github.com/{repo_full}/pull/{number}",
            "title": subject.get("title") or "",
            "reason": n.get("reason") or "notification",
        })
    return out


def _from_search(flag: str) -> list[dict]:
    """PRs from `gh search prs --state=open <flag>=@me`, across every repo
    the account can see -- gh search is not scoped to one owner or org."""
    data = _gh_json([
        "search", "prs", "--state=open", flag, "@me",
        "--limit", "100",
        "--json", "url,title,repository",
    ])
    if not isinstance(data, list):
        return []
    out = []
    for r in data:
        url = r.get("url") or ""
        if url:
            out.append({"url": url, "title": r.get("title") or "",
                        "reason": flag.strip("-")})
    return out


def discover() -> list[dict]:
    """Every PR `gh` says this account is on right now, deduped by URL.

    Merges three independent queries because they overlap but none subsumes
    the others: notifications includes things already acted on and drops off
    the list once read, `--assignee` catches PRs nobody notified about, and
    `--review-requested` is the one John most needs to see and is not
    guaranteed to raise a notification at all.
    """
    found: dict = {}
    for item in (_from_notifications()
                 + _from_search("--assignee")
                 + _from_search("--review-requested")):
        try:
            _repo, _number, canon = prs.parse_pr_url(item["url"])
        except ValueError:
            continue
        if canon not in found:
            found[canon] = item
    return list(found.values())


def scan_github(conn) -> dict:
    """Register every PR `gh` finds that isn't already on the list.

    Returns {"found": N, "added": N, "gh_available": bool} so the caller can
    log something useful without this module knowing about notify.log_line.
    Never raises: a `gh` failure means found=0, not an exception the watcher
    loop has to catch on this module's behalf too.
    """
    if not prs.gh_available():
        return {"found": 0, "added": 0, "gh_available": False}
    items = discover()
    added = 0
    for item in items:
        try:
            repo, number, canon = prs.parse_pr_url(item["url"])
        except ValueError:
            continue
        if db.get_pr(conn, canon) is not None:
            continue  # already on the list, agent-registered or scanned before
        title = (item.get("title") or canon)[:200]
        _pr_id, created = db.register_pr(
            conn, canon, repo, number, title,
            requested_by=paths.PR_NOTIFIER, thread_id=None,
            source=paths.PR_SOURCE_SCAN)
        if created:
            added += 1
    return {"found": len(items), "added": added, "gh_available": True}


# --- Ladder triage --------------------------------------------------------------

_SYSTEM = (
    "You triage pull requests for a busy engineer. Classify the ONE pull "
    "request described below into exactly one label, and reply with that "
    "label alone, nothing else:\n"
    "needs-merge-now - it is approved/green and is only waiting on him to "
    "click merge\n"
    "fyi - informational, a CI update, or not something needing his action\n"
    "already-handled - someone else has already merged, closed, or taken "
    "the action needed\n"
    "stale - old, inactive, or likely dead\n"
    "Reply with exactly one of: needs-merge-now, fyi, already-handled, stale"
)


def _pr_context(url: str) -> Optional[str]:
    """The fields item #115 asks the classifier see, as one text block."""
    data = _gh_json([
        "pr", "view", url, "--json",
        "title,body,state,reviewDecision,statusCheckRollup,isDraft,"
        "mergeable,updatedAt,author,reviewRequests",
    ])
    if not isinstance(data, dict):
        return None
    checks = data.get("statusCheckRollup") or []
    check_states = [c.get("conclusion") or c.get("state") or "?" for c in checks]
    reviewers = [r.get("login") or r.get("name") or "?"
                for r in (data.get("reviewRequests") or [])]
    body = (data.get("body") or "")[:800]
    return (
        f"Title: {data.get('title')}\n"
        f"State: {data.get('state')}  Draft: {data.get('isDraft')}\n"
        f"Review decision: {data.get('reviewDecision')}\n"
        f"CI checks: {', '.join(check_states) or 'none reported'}\n"
        f"Mergeable: {data.get('mergeable')}\n"
        f"Waiting on review from: {', '.join(reviewers) or 'nobody'}\n"
        f"Last updated: {data.get('updatedAt')}\n"
        f"Description:\n{body}"
    )


def _ladder_router():
    """The Router item #115's classification runs on, or None if unavailable.

    Imported lazily and inside the caller's try/except: this machine may not
    have the ladder package importable, may not have a model warm, or may
    have Ladder deliberately stopped, and none of that is this module's
    business to report -- only to fall back cleanly from.
    """
    if str(_LADDER_PARENT) not in sys.path:
        sys.path.insert(0, str(_LADDER_PARENT))
    from ladder.router import Router  # type: ignore
    return Router()


def _triage_one(router, url: str) -> Optional[str]:
    """One PR's label, or None on ANY failure. Never raises."""
    try:
        context = _pr_context(url)
        if context is None:
            return None
        result = router.run_job(
            prompt=f"Pull request: {url}\n\n{context}",
            kind="classify",
            rung=0, max_rung=0,  # bulk mechanical classification: local rung only
            system_extra=_SYSTEM,
            max_tokens=16,
            title=f"triage {url}",
        )
        if not result.get("ok"):
            return None
        text = str(result.get("result") or "").strip().lower()
        for label in TRIAGE_LABELS:
            if label in text:
                return label
        return None
    except Exception:
        # Ladder down, model not warm, network hiccup, unexpected response
        # shape -- all of it means "no label this pass", never "crash the
        # watcher" and never "guess a label anyway".
        return None


def triage_open(conn, limit: Optional[int] = None) -> dict:
    """Label every open, untriaged PR. Returns a summary, never raises.

    Only PRs without a label yet, so a warm run doesn't re-spend a rung-0
    call on a PR whose situation has not changed since the last pass -- the
    gh state check already re-runs every pass; the label does not need to.
    """
    rows = [r for r in db.list_prs(conn, include_settled=False)
            if not r.get("triage")]
    if limit is not None:
        rows = rows[:int(limit)]
    if not rows:
        return {"attempted": 0, "labelled": 0, "ladder_available": None}
    if not TRIAGE_ENABLED:
        return {"attempted": len(rows), "labelled": 0, "ladder_available": False}
    try:
        router = _ladder_router()
    except Exception:
        # Ladder is not available at all this pass -- report how many rows
        # are waiting for a label (so the log/caller can say "N unlabelled",
        # not "nothing happened") without ever raising past this point.
        return {"attempted": len(rows), "labelled": 0, "ladder_available": False}
    labelled = 0
    for row in rows:
        label = _triage_one(router, row["url"])
        if label:
            db.set_pr_triage(conn, row["id"], label)
            labelled += 1
    return {"attempted": len(rows), "labelled": labelled, "ladder_available": True}
