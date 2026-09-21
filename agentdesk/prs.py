"""The merge list: pull requests waiting on John, and the thing that watches them.

An agent opens a PR, registers it here, and it appears on John's Pull Requests
tab as a link he can click to go and merge it. This module is the other half:
it asks GitHub, through the `gh` CLI, whether that PR has been merged, and when
it has, it takes the row off the list and posts a notice back onto the board
thread the work came from.

The one rule that shapes everything below: THIS MODULE MAY ONLY EVER REMOVE A
ROW BECAUSE GITHUB SAID SO. A check that fails -- no `gh`, no network, an
expired token, a repo John cannot see, a PR that no longer exists -- leaves the
row exactly as open as it was and records why. It never guesses, and it never
treats an error as a terminal state. The failure that would destroy the feature
is not a missing row; it is a row that vanished while the PR was still waiting,
because then John stops trusting the list and goes back to scrolling chat.

`gh` is a subprocess and costs about a second, so this is deliberately NOT run
from the window's 3-second poll. See paths.PR_CHECK_SECONDS.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
from typing import Optional

from . import db, identity, notify, paths


def _no_window() -> dict:
    """Extra Popen/run kwargs that stop a spawned `gh` flashing a console.

    Same fix as `crew._no_window()`, for the same reason: the app normally
    runs under `pythonw.exe` (no console), and a console child spawned from
    it with no flag gets a brand-new window that steals focus. This module
    shells out to `gh` on the watcher's own interval, so without this it
    would throw a window in John's face every `PR_CHECK_SECONDS`.
    """
    if os.name != "nt":
        return {}
    si = subprocess.STARTUPINFO()
    si.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    si.wShowWindow = subprocess.SW_HIDE
    return {"creationflags": subprocess.CREATE_NO_WINDOW, "startupinfo": si}

# `gh` reports these. Anything else is treated as an error and leaves the row
# open -- a GitHub that grows a new state must not thereby settle John's list.
_OPEN = "OPEN"
_MERGED = "MERGED"
_CLOSED = "CLOSED"

# Only the shape we can act on, and only the parts we re-build the URL from.
# Deliberately not "store the URL the caller passed": the stored value is what
# the tab turns into a clickable link and hands to `gh`, so it is reconstructed
# from validated pieces rather than trusted. A trailing /files, /commits or
# ?diff=split is tolerated and dropped, because people paste those.
_PR_URL = re.compile(
    r"^https?://(?:www\.)?github\.com/"
    r"(?P<owner>[A-Za-z0-9._-]+)/(?P<repo>[A-Za-z0-9._-]+)/pull/(?P<number>\d+)"
    r"(?:[/?#].*)?$",
    re.IGNORECASE,
)


class GhError(RuntimeError):
    """A check could not be completed. Never means, and never implies, merged."""


def parse_pr_url(url: str) -> tuple:
    """(repo, number, canonical_url) for a GitHub PR URL, or raise ValueError.

    The canonical URL is rebuilt from the parsed pieces rather than echoed
    back, so what gets stored and later rendered as a link is a string this
    module constructed. That is the difference between "we display what an
    agent gave us" and "we display a URL we know the shape of".
    """
    m = _PR_URL.match((url or "").strip())
    if not m:
        raise ValueError(
            f"not a GitHub pull request URL: {url!r} "
            "(expected https://github.com/<owner>/<repo>/pull/<number>)")
    repo = f"{m.group('owner')}/{m.group('repo')}"
    number = int(m.group("number"))
    return repo, number, f"https://github.com/{repo}/pull/{number}"


def gh_available() -> bool:
    return shutil.which("gh") is not None


def _gh_pr_view(url: str, timeout: int = paths.PR_GH_TIMEOUT_SECONDS) -> dict:
    """Ask `gh` about one PR. Raises GhError, never returns a guess."""
    exe = shutil.which("gh")
    if exe is None:
        raise GhError("the GitHub CLI (gh) is not on PATH")
    try:
        proc = subprocess.run(
            [exe, "pr", "view", url, "--json", "state,title,mergedAt"],
            capture_output=True, text=True, timeout=timeout,
            # gh emits UTF-8 whatever the console codepage is. Decoding with
            # the locale would mangle a non-ASCII title rather than fail, and
            # the title is stored and shown.
            encoding="utf-8", errors="replace",
            **_no_window(),
        )
    except subprocess.TimeoutExpired:
        raise GhError(f"gh did not answer within {timeout}s")
    except OSError as exc:
        raise GhError(f"gh could not be run: {exc}")
    if proc.returncode != 0:
        # gh's first stderr line is the useful one ("Could not resolve to a
        # PullRequest...", "gh auth login", ...). The rest is a stack of advice
        # that would be stored per PR and shown on every redraw.
        lines = [ln for ln in (proc.stderr or proc.stdout or "").splitlines() if ln.strip()]
        raise GhError(lines[0] if lines else f"gh exited {proc.returncode}")
    try:
        return json.loads(proc.stdout)
    except ValueError:
        raise GhError("gh returned something that is not JSON")


def _notice(row: dict, settled: str, title: str) -> str:
    """The board message for a settled PR, in the board's own voice."""
    where = f"{row['repo']}#{row['number']}"
    who = identity.describe(row["requested_by"])
    if settled == paths.PR_MERGED:
        return (f"Merged: {title}\n\n{where} - {row['url']}\n\n"
                f"This was the pull request {who} asked you to merge. "
                f"It is off your merge list.")
    return (f"Closed without merging: {title}\n\n{where} - {row['url']}\n\n"
            f"{who} asked you to merge this one. It was closed rather than "
            f"merged, so it is off your merge list -- reopen it if that was "
            f"not the intent.")


def notify_settled(conn, row: dict, settled: str, title: str) -> bool:
    """Post the merge/close notice on the thread the PR came from. True if posted.

    Once ever per PR, and the once-mechanism is db.mark_pr_notified's
    conditional UPDATE, not a read here: the app's checker thread and a CLI run
    can both be looking at the same unsettled row.

    The claim is taken BEFORE the post, so that two posters cannot both write.
    If the post then fails the claim is handed back, because a notification
    that was claimed and never sent is worse than one sent twice -- the row is
    terminal, so nothing else will ever come along and retry it.
    """
    thread_id = row.get("thread_id")
    if thread_id is None:
        # Nobody to tell. The PR row's own settled_ts is the record of when it
        # happened, and the tab still shows it under "show merged".
        notify.log_line(f"pr {row['url']} settled {settled} with no thread to notify")
        return False
    if not db.mark_pr_notified(conn, row["id"]):
        return False
    try:
        db.reply(conn, thread_id, paths.PR_NOTIFIER, paths.AGENT_KIND,
                 _notice(row, settled, title),
                 meta={"kind": f"pr-{settled}"})
    except Exception:
        conn.execute("UPDATE pull_requests SET notified_ts=NULL WHERE id=?",
                     (int(row["id"]),))
        raise
    return True


def check_one(conn, row: dict) -> dict:
    """Check one PR and act on the answer. Never raises; returns what happened.

    status is one of 'still-open', 'merged', 'closed' or 'error'. Only the two
    terminal ones mean the row left the list.
    """
    url = row["url"]
    try:
        data = _gh_pr_view(url)
    except GhError as exc:
        db.mark_pr_checked(conn, row["id"], error=str(exc))
        return {"pr_id": row["id"], "url": url, "repo": row["repo"],
                "number": row["number"], "status": "error",
                "error": str(exc), "notified": False}

    state = str(data.get("state") or "").upper()
    title = (data.get("title") or row["title"] or "").strip()

    if state == _OPEN:
        # Clear any previous error: the row is now known-good, and a stale
        # "could not reach GitHub" left on a PR that is answering fine would
        # send John looking for a problem that has gone.
        db.mark_pr_checked(conn, row["id"], error=None)
        return {"pr_id": row["id"], "url": url, "repo": row["repo"],
                "number": row["number"], "status": "still-open",
                "error": None, "notified": False}

    if state in (_MERGED, _CLOSED):
        settled = paths.PR_MERGED if state == _MERGED else paths.PR_CLOSED
        db.settle_pr(conn, row["id"], settled)
        notified = notify_settled(conn, dict(row), settled, title)
        return {"pr_id": row["id"], "url": url, "repo": row["repo"],
                "number": row["number"], "status": settled,
                "error": None, "notified": notified}

    msg = f"gh reported a state this does not understand: {state!r}"
    db.mark_pr_checked(conn, row["id"], error=msg)
    return {"pr_id": row["id"], "url": url, "repo": row["repo"],
            "number": row["number"], "status": "error",
            "error": msg, "notified": False}


def check_due(conn, limit: Optional[int] = None) -> list:
    """Check every open PR. Returns one result row per PR, in check order.

    Sequential on purpose. The number of PRs waiting on John is small, and
    running them at once would multiply the rate-limit cost for a saving nobody
    can perceive at this size.
    """
    rows = db.prs_due_for_check(conn)
    if limit is not None:
        rows = rows[:int(limit)]
    return [check_one(conn, row) for row in rows]


def check_due_path(db_path) -> list:
    """check_due against a database path, opening its own connection.

    This is what the window's background thread calls: it must own its
    connection, because the UI thread is holding one on the same file.
    """
    conn = db.connect(db_path)
    try:
        return check_due(conn)
    finally:
        conn.close()


def summarise(results: list) -> str:
    """One log line for a pass, or "" when nothing happened.

    Silence for the quiet case is the point: a pass every minute that logs
    "0 PRs, nothing changed" would bury the passes that matter.
    """
    if not results:
        return ""
    counted: dict = {}
    for r in results:
        counted[r["status"]] = counted.get(r["status"], 0) + 1
    moved = counted.get("merged", 0) + counted.get("closed", 0)
    errored = counted.get("error", 0)
    if not moved and not errored:
        return ""
    parts = [f"{n} {word}" for word, n in sorted(counted.items())]
    return f"pr check: {len(results)} checked, " + ", ".join(parts)


# --- the command line ----------------------------------------------------------
# For running the check with no window open: a scheduled task, or a shell. The
# app runs the same code in a thread, so this is not a second implementation,
# only a second way to start it.

def main(argv: Optional[list] = None) -> int:
    import argparse

    ap = argparse.ArgumentParser(
        prog="agentdesk.prs",
        description="Check the pull requests waiting on John, and settle the "
                    "ones GitHub says are done.")
    ap.add_argument("--check-once", action="store_true",
                    help="check every open PR once and exit")
    ap.add_argument("--list", action="store_true",
                    help="print the merge list and exit")
    ap.add_argument("--include-settled", action="store_true",
                    help="with --list, include merged and closed rows")
    ap.add_argument("--db", default=None, help="database path (default: the real board)")
    args = ap.parse_args(argv)

    conn = db.connect(args.db)
    try:
        # Before the first query, and not at import: this CLI is most likely to
        # be run by a scheduled task on a machine that has never opened the
        # window, where the database file does not exist yet and connect() has
        # just created it empty. Without this the command dies on "no such
        # table: pull_requests", which is precisely the case it exists to
        # serve. mcp_server.main() carries the same call for the same reason.
        db.init_db(conn)
        if args.list:
            rows = db.list_prs(conn, include_settled=args.include_settled)
            if not rows:
                print("nothing on the merge list")
            for r in rows:
                line = f"[{r['state']:<6}] {r['repo']}#{r['number']}  {r['url']}"
                if r["last_error"]:
                    line += f"  (last check failed: {r['last_error']})"
                print(line)
            return 0

        if args.check_once:
            results = check_due(conn)
            if not results:
                print("no open pull requests to check")
            for r in results:
                line = f"{r['repo']}#{r['number']}: {r['status']}"
                if r["error"]:
                    line += f" ({r['error']})"
                if r["notified"]:
                    line += " -- notified"
                print(line)
            line = summarise(results)
            if line:
                notify.log_line(line)
            return 0

        ap.print_help()
        return 2
    finally:
        conn.close()


if __name__ == "__main__":
    raise SystemExit(main())
