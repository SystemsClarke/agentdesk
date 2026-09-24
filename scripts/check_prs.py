"""Acceptance checks for the merge list (work item 38).

What John asked for, in his words: "a list of PRs that I can click on, like
links to PRs that I can click on to go merge them on the web", "some sort of
background daemon or service ... that keeps using the GitHub CLI like GH command
to check if that pull request has been merged", "and if it has, then it can
close it and send the notification back to the agent", "all from one of the tabs
inside of this tool".

Four properties, then the arms that try to break them:

  1. a registered PR appears on the tab, clickable, opening it on GitHub;
  2. the background checker asks `gh`, and when GitHub says MERGED the row is
     taken off the list;
  3. the agent that opened it gets a notice on the thread it came from, once;
  4. THE ARM THAT MATTERS: a check that FAILS must never take a row off the
     list. A list that loses an entry because this machine could not reach
     GitHub is worse than no list, because John would go and look for a PR that
     is still waiting.

HOW `gh` IS FAKED, AND WHY THAT STILL TESTS THE REAL CODE. A real `gh.bat` is
put on PATH, so `prs._gh_pr_view` runs its real subprocess, its real timeout,
its real stdout decoding and its real JSON parse; only the answer comes from a
file this script writes. Nothing is monkeypatched. The fake reports exactly the
shape real `gh` reported when this was written:
    {"mergedAt":"...","state":"MERGED","title":"..."}
and reproduces real gh's failure mode too -- a non-zero exit with the message on
stderr. A separate arm below runs the module's own command line as a
subprocess, so the CLI path is exercised by a real process rather than by an
import.

What this does NOT show: that `gh` is installed and authenticated on this
machine, or that these URLs exist. That is checked by talking to the real
GitHub, and the output of that is pasted into the report rather than asserted
here, because a test that fails when the network is down is a test nobody runs.

    python scripts/check_prs.py [--shot DIR]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
import textwrap
import time
import webbrowser
from pathlib import Path

# --- isolation, BEFORE agentdesk is imported -----------------------------------
#
# LOCALAPPDATA first: paths.py reads it at import time, and db.DB_PATH with it.
# A scratch value here is what keeps this from touching John's real board.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-prs-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from agentdesk import app as appmod          # noqa: E402
from agentdesk import db, mcp_server, notify, paths, prs   # noqa: E402

# The vault is redirected as well, even though nothing here archives a question:
# the App's poll loop runs the archive pass on every tick, and a board that
# happened to hold a settled question would otherwise write into the real vault.
_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = paths.VAULT_AGENTDESK / "questions"

# --- the fake gh ---------------------------------------------------------------

_FAKE_DIR = _SCRATCH / "fakebin"
_ANSWERS = _SCRATCH / "gh-answers.json"
_CALLS = _SCRATCH / "gh-calls.log"


def install_fake_gh() -> None:
    """Put a `gh` on PATH that answers from a file this script controls."""
    _FAKE_DIR.mkdir(parents=True, exist_ok=True)
    script = _SCRATCH / "fakegh.py"
    script.write_text(textwrap.dedent(f'''
        import json, sys, pathlib
        ANSWERS = pathlib.Path({str(_ANSWERS)!r})
        CALLS = pathlib.Path({str(_CALLS)!r})
        # argv is [script, "pr", "view", <url>, "--json", ...]
        url = sys.argv[3] if len(sys.argv) > 3 else ""
        with CALLS.open("a", encoding="utf-8") as fh:
            fh.write(url + "\\n")
        try:
            answers = json.loads(ANSWERS.read_text(encoding="utf-8"))
        except Exception:
            answers = {{}}
        a = answers.get(url)
        if a is None:
            # What real gh does for a PR that is not there.
            sys.stderr.write(
                "GraphQL: Could not resolve to a PullRequest with the URL of "
                + url + ". (repository.pullRequest)\\n")
            raise SystemExit(1)
        if isinstance(a, str) and a.startswith("ERROR:"):
            sys.stderr.write(a[6:] + "\\n")
            raise SystemExit(1)
        print(json.dumps({{"state": a.get("state"), "title": a.get("title", ""),
                           "mergedAt": a.get("mergedAt")}}))
    '''), encoding="utf-8")
    bat = _FAKE_DIR / "gh.bat"
    bat.write_text(f'@echo off\r\n"{sys.executable}" "{script}" %*\r\n',
                   encoding="utf-8")
    os.environ["PATH"] = str(_FAKE_DIR) + os.pathsep + os.environ["PATH"]


def gh_answers(mapping: dict) -> None:
    _ANSWERS.write_text(json.dumps(mapping, indent=2), encoding="utf-8")


def gh_call_count() -> int:
    if not _CALLS.exists():
        return 0
    # Only `gh pr view <url>` calls. The watcher pass also runs pr_scan, which
    # calls `gh search prs ...` (logged as its flags, not a URL); those are the
    # scan's business, not the per-PR status check these counts are about.
    return len([ln for ln in _CALLS.read_text(encoding="utf-8").splitlines()
                if ln.startswith("https://")])


# --- reporting ------------------------------------------------------------------

FAILURES: list[str] = []


def hr(title: str) -> None:
    print()
    print("-" * 78)
    print(title)
    print("-" * 78)


def show(label: str, value) -> None:
    print(f"  {label:<44} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<40} {got!r}")
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")


def note(text: str) -> None:
    print(f"  ... {text}")


def conn_status(url: str) -> dict:
    conn = db.connect(paths.DB_PATH)
    try:
        return db.get_pr(conn, url) or {}
    finally:
        conn.close()


def pr_id_of(url: str) -> int:
    """The row id for a URL, read from the database.

    Read rather than assumed. The first draft of this script hardcoded "the
    first PR is row 2" and it was wrong, because the ids depend on registration
    order -- and a check that breaks when a section is reordered or one more PR
    is seeded is a check that gets deleted rather than fixed.
    """
    row = conn_status(url)
    if not row:
        raise AssertionError(f"no pull request registered for {url}")
    return int(row["id"])


def pr_row_in(app, pr_id: int) -> bool:
    return pr_id in [int(r["id"]) for r in app.view.prs]


def view_open_prs(app) -> int:
    """The open-PR count the terminal view shows (main menu and sysop)."""
    return sum(1 for r in app.view.prs if r["state"] == paths.PR_OPEN)


def body_text(app) -> str:
    return app.view.body.get("1.0", "end")


def select_pr(app, url: str) -> None:
    """Go to the merge desk and move the cursor onto the row for url."""
    app.view.goto("prs")
    idx = [r["url"] for r in app.view.prs].index(url)
    app.view.sel_prs = idx
    app.view.render()
    app.root.update()


def open_pr_count() -> int:
    conn = db.connect(paths.DB_PATH)
    try:
        return len(db.list_prs(conn))
    finally:
        conn.close()


# --- the sections ---------------------------------------------------------------

def seed() -> dict:
    """A thread for a PR to belong to, plus the PRs themselves."""
    conn = db.connect(paths.DB_PATH)
    try:
        db.init_db(conn)
        tid = db.start_thread(conn, "work", "the job that opened a PR",
                              "builder", paths.AGENT_KIND, "SEEDJOB doing the work")
    finally:
        conn.close()
    return {"thread": tid}


def register(url: str, thread_id: int, note_text: str = "") -> dict:
    """Register through the REAL MCP tool, and parse what it returns."""
    raw = mcp_server.request_merge(pr_url=url, thread_id=thread_id,
                                   note=note_text or None, author="builder")
    return json.loads(raw)


def check_register_and_tab(ids: dict, app) -> None:
    hr("1. an agent registers a PR; it lands on the tab")
    conn = db.connect(paths.DB_PATH)
    try:
        # Seeded directly rather than through the tool only so that the tab has
        # more than one row to draw; the tool is exercised in the next step.
        db.register_pr(conn, "https://github.com/Vispero/Fusion/pull/80",
                       "Vispero/Fusion", 80, "Point FusionHelp at the submodule",
                       "builder", thread_id=ids["thread"])
    finally:
        conn.close()
    app.refresh_now()
    app.root.update()

    show("rows on the merge list", [r["id"] for r in app.view.prs])
    check("the seeded PR is on the tab",
          pr_row_in(app, pr_id_of("https://github.com/Vispero/Fusion/pull/80")),
          True)

    hr("2. an agent asks through the real MCP tool")
    out = register("https://github.com/Vispero/SetupSDK/pull/21", ids["thread"],
                   "one-line fast-forward, needs your sign-off")
    print(f"  request_merge returned {json.dumps(out)}")
    check("ok", out.get("ok"), True)
    check("created", out.get("created"), True)
    check("the URL is stored canonically", out.get("url"),
          "https://github.com/Vispero/SetupSDK/pull/21")

    again = register("https://github.com/Vispero/SetupSDK/pull/21", ids["thread"])
    check("registering the same URL again does not create a row",
          again.get("created"), False)
    check("...it hands back the same row", again.get("pr_id"), out.get("pr_id"))
    conn = db.connect(paths.DB_PATH)
    try:
        check("...and there is exactly one row for that URL",
              len([r for r in db.list_prs(conn, include_settled=True)
                   if r["url"] == out["url"]]), 1)
        msgs = [dict(m) for m in conn.execute(
            "SELECT author, author_kind, body, meta FROM messages"
            " WHERE thread_id=? ORDER BY id", (ids["thread"],))]
    finally:
        conn.close()
    asking = [m for m in msgs if m["meta"] and "pr-request" in m["meta"]]
    check("a message on the thread asked John to merge it", len(asking), 1)
    if asking:
        show("  it says", repr(asking[0]["body"].splitlines()[0][:60]))
        check("...and it carries the agent's name, not the board's",
              asking[0]["author"], "builder")

    app.refresh_now()
    app.root.update()
    check("both PRs are now on the tab",
          len(app.view.prs), open_pr_count())
    check("the view counts them", view_open_prs(app), open_pr_count())


def check_clickable(ids: dict, app) -> None:
    hr("3. the list is clickable and opens the PR on the web")
    opened: list = []
    real_open = webbrowser.open

    def spy(url, *a, **k):
        opened.append(url)
        return True

    webbrowser.open = spy
    try:
        target = "https://github.com/Vispero/SetupSDK/pull/21"
        row = [r for r in app.view.prs if r["url"] == target]
        check("the PR is listed", len(row), 1)
        select_pr(app, target)
        check("...and selecting it shows the URL in the detail",
              target in body_text(app), True)
        app.view._activate()
        check("selecting and activating it opened it", opened[-1:], [target])
        opened.clear()
        app.view._on_double(None)
        check("...and so did double-clicking the row", opened[-1:], [target])
    finally:
        webbrowser.open = real_open


def check_merge_clears(ids: dict, app) -> None:
    hr("4. GitHub says MERGED; the row clears itself and the agent is told")
    url = "https://github.com/Vispero/SetupSDK/pull/21"
    gh_answers({url: {"state": "MERGED", "title": "SetupSDK pin to 1252909b"},
                "https://github.com/Vispero/Fusion/pull/80":
                    {"state": "OPEN", "title": "still open"}})
    before_msgs = count_thread_messages(ids["thread"])
    before_calls = gh_call_count()
    ran = run_check(app)
    check("the watcher thread ran a check on its own", ran, True)
    check("...it invoked gh once per PR", gh_call_count() - before_calls, 2)

    app.refresh_now()
    app.root.update()
    check("the merged PR left the merge list",
          url in [r["url"] for r in app.view.prs], False)
    check("...the still-open one did not",
          "https://github.com/Vispero/Fusion/pull/80"
          in [r["url"] for r in app.view.prs], True)
    # One of the two open PRs settled, so the count falls by exactly one. The
    # expected label is built from the board rather than written as a literal:
    # the value being asserted is "the tab agrees with the list", and a literal
    # would silently stop testing that the moment a section adds a PR.
    want_open = open_pr_count()
    check("the view's count dropped", view_open_prs(app), want_open)
    check("...and it counts only the unsettled ones", want_open, 1)

    stored = conn_status(url)
    check("its state is stored as merged", stored.get("state"), "merged")
    check("...with a settled timestamp", bool(stored.get("settled_ts")), True)
    check("...and no error left on it", stored.get("last_error"), None)

    msgs = thread_messages(ids["thread"])
    new = msgs[before_msgs:]
    check("exactly one new message was posted back", len(new), 1)
    if new:
        m = new[0]
        print()
        print("  the notice, as posted:")
        for line in m["body"].splitlines()[:6]:
            print(f"      | {line[:66]}")
        print()
        check("...it is from the board, not from an agent", m["author"], "agentdesk")
        check("...its kind marks it as a merge notice",
              json.loads(m["meta"] or "{}").get("kind"), "pr-merged")
        check("...and it names the PR", "SetupSDK#21" in m["body"], True)


def check_notify_once(ids: dict, app) -> None:
    hr("5. the notice is sent once, not on every pass")
    url = "https://github.com/Vispero/SetupSDK/pull/21"
    before = count_thread_messages(ids["thread"])
    conn = db.connect(paths.DB_PATH)
    try:
        row = db.get_pr(conn, url)
        # Calling the notifier directly is the strong form: it bypasses "the
        # checker would not ask again" -- which is true, because the row is
        # terminal and prs_due_for_check no longer selects it -- and proves the
        # once-guard holds even for a caller that does ask.
        again = prs.notify_settled(conn, row, paths.PR_MERGED, row["title"])
    finally:
        conn.close()
    check("a second notification is refused", again, False)
    check("...and nothing new was posted",
          count_thread_messages(ids["thread"]) - before, 0)
    conn = db.connect(paths.DB_PATH)
    try:
        check("...and a settled PR is not offered to the checker again",
              url in [r["url"] for r in db.prs_due_for_check(conn)], False)
    finally:
        conn.close()


def check_failure_never_clears(ids: dict, app) -> None:
    hr("6. THE ARM THAT MATTERS: a failed check never clears a row")
    url = "https://github.com/Vispero/Fusion/pull/80"
    gh_answers({url: "ERROR:HTTP 403: rate limit exceeded"})
    # Scoped to what this section causes. An earlier section legitimately posts
    # a merge notice for a different PR, so "there is no merge notice on this
    # thread" is a claim that was false before this section ran -- it would fail
    # on a correct board.
    before_msgs = count_thread_messages(ids["thread"])
    ran = run_check(app)
    check("the pass ran", ran, True)
    app.refresh_now()
    app.root.update()

    pid = pr_id_of(url)
    check("the PR is STILL on the list", pr_row_in(app, pid), True)
    stored = conn_status(url)
    check("...its state is still open", stored.get("state"), "open")
    check("...no settled timestamp was written", stored.get("settled_ts"), None)
    show("the error it recorded", repr(stored.get("last_error")))
    check("...the failure was recorded, not swallowed",
          bool(stored.get("last_error")), True)
    new = thread_messages(ids["thread"])[before_msgs:]
    check("...and nothing was posted about a merge that did not happen",
          [m for m in new
           if json.loads(m["meta"] or "{}").get("kind") in
           ("pr-merged", "pr-closed")], [])
    check("...in fact this section posted nothing at all", len(new), 0)

    # And it is visible, not merely stored.
    select_pr(app, url)
    body = body_text(app)
    row_line = [ln for ln in body.splitlines() if "Fusion#80" in ln]
    check("the list says the check failed",
          bool(row_line) and "failed" in row_line[0], True)
    check("...the detail explains it and says the row is still waiting",
          "rate limit exceeded" in body and "stays on the list" in body, True)

    hr("6b. and when GitHub answers again, the error clears")
    gh_answers({url: {"state": "OPEN", "title": "still open"}})
    ran = run_check(app)
    check("the pass ran", ran, True)
    stored = conn_status(url)
    check("the stale error is gone", stored.get("last_error"), None)
    check("...and the PR is still open, because that is what GitHub said",
          stored.get("state"), "open")


def check_closed_unmerged(ids: dict, app) -> None:
    hr("7. a PR closed WITHOUT merging is settled too, and says so")
    url = "https://github.com/Vispero/RemoteSupport/pull/38"
    out = register(url, ids["thread"])
    check("registered", out.get("created"), True)
    before = count_thread_messages(ids["thread"])
    # Every open PR is answered, not just this one: an unanswered URL is a gh
    # error, and leaving one would put an error on Fusion#80 that the next
    # section then has to reason around.
    gh_answers({url: {"state": "CLOSED", "title": "abandoned in favour of 39"},
                "https://github.com/Vispero/Fusion/pull/80":
                    {"state": "OPEN", "title": "still open"}})
    ran = run_check(app)
    check("the pass ran", ran, True)
    app.refresh_now()
    app.root.update()

    stored = conn_status(out["url"])
    check("its state is closed", stored.get("state"), "closed")
    check("...it left the list", out["url"] in [r["url"] for r in app.view.prs],
          False)
    new = thread_messages(ids["thread"])[before:]
    check("...a notice was posted", len(new), 1)
    if new:
        check("...and it does NOT claim a merge", "Merged:" in new[0]["body"], False)
        check("...it says closed without merging",
              new[0]["body"].startswith("Closed without merging:"), True)


def check_resurrection(ids: dict) -> None:
    hr("8. re-registering a settled PR must not put it back on the list")
    url = "https://github.com/Vispero/RemoteSupport/pull/38"
    before = gh_call_count()
    out = register(url, ids["thread"])
    check("the tool reports the row already exists", out.get("created"), False)
    stored = conn_status(url)
    check("...and it is still closed", stored.get("state"), "closed")
    conn = db.connect(paths.DB_PATH)
    try:
        check("...so it is not back on the open list",
              url in [r["url"] for r in db.list_prs(conn)], False)
    finally:
        conn.close()
    check("...and re-registering did not even ask GitHub", gh_call_count(), before)


def check_unknown_state(ids: dict) -> None:
    hr("9. a GitHub state this does not understand is treated as an error")
    url = "https://github.com/Vispero/Authorization/pull/36"
    register(url, ids["thread"])
    gh_answers({url: {"state": "DRAFT_MERGE_QUEUE_SOMETHING_NEW", "title": "?"},
                "https://github.com/Vispero/Fusion/pull/80":
                    {"state": "OPEN", "title": "still open"}})
    conn = db.connect(paths.DB_PATH)
    try:
        results = [r for r in prs.check_due(conn) if r["url"] == url]
    finally:
        conn.close()
    check("it is reported as an error, not as settled",
          results[0]["status"] if results else None, "error")
    stored = conn_status(url)
    check("...the row is still open", stored.get("state"), "open")
    check("...and the odd state is recorded so somebody can see it",
          "DRAFT_MERGE_QUEUE" in (stored.get("last_error") or ""), True)


def _old_digest(conn) -> tuple:
    """The redraw digest as it was first written, kept to prove what it missed."""
    row = conn.execute(
        "SELECT COUNT(*) AS n, COALESCE(MAX(settled_ts),'') AS settled,"
        " COALESCE(MAX(checked_ts),'') AS checked FROM pull_requests"
    ).fetchone()
    return (row["n"], row["settled"], row["checked"])


def check_signature_catches_same_second_error(app) -> None:
    hr("9b. the redraw digest notices an error written in the same second")
    conn = db.connect(paths.DB_PATH)
    original = None
    pid = None
    try:
        pid = int(db.list_prs(conn, include_settled=True)[0]["id"])
        # Saved and put back by hand: db.connect is autocommit, so the writes
        # below are durable the moment they run and rollback() would not undo
        # them. This section must leave the board exactly as it found it -- the
        # sections after it read these rows.
        row = conn.execute("SELECT checked_ts, last_error FROM pull_requests"
                           " WHERE id=?", (pid,)).fetchone()
        original = (row["checked_ts"], row["last_error"])
        stamp = "2026-01-01T00:00:00+00:00"
        # Both writes pin checked_ts to the same value, so the only column that
        # differs between them is last_error. Not a contrived case: it is what
        # happens whenever the same PR is checked twice inside one second, which
        # is what pressing "Check now" does.
        conn.execute("UPDATE pull_requests SET checked_ts=?, last_error=NULL"
                     " WHERE id=?", (stamp, pid))
        old_before, before = _old_digest(conn), app._prs_signature(conn)
        conn.execute("UPDATE pull_requests SET checked_ts=?, last_error=?"
                     " WHERE id=?", (stamp, "boom", pid))
        old_after, after = _old_digest(conn), app._prs_signature(conn)
        check("the digest first used cannot see it", old_after, old_before)
        check("...so it would have skipped the redraw that shows the failure",
              old_after == old_before, True)
        check("the digest now used does see it", after != before, True)
    finally:
        # Guarded, so a failure above surfaces as itself rather than as a
        # NameError from in here.
        if original is not None:
            conn.execute("UPDATE pull_requests SET checked_ts=?, last_error=?"
                         " WHERE id=?", (original[0], original[1], pid))
        conn.close()
    if original is None:
        return
    conn = db.connect(paths.DB_PATH)
    try:
        back = conn.execute("SELECT checked_ts, last_error FROM pull_requests"
                            " WHERE id=?", (pid,)).fetchone()
        check("...and the board is left as it was found",
              (back["checked_ts"], back["last_error"]), original)
    finally:
        conn.close()


def check_rejections(ids: dict) -> None:
    hr("10. what must NOT get onto the list")
    conn = db.connect(paths.DB_PATH)
    try:
        before = len(db.list_prs(conn, include_settled=True))
    finally:
        conn.close()
    for bad, why in (
        ("https://gitlab.com/foo/bar/pull/1", "not GitHub"),
        ("https://evil.example.com/a/b/pull/1", "not GitHub"),
        ("https://github.com/Vispero/Fusion/issues/80", "an issue, not a PR"),
        ("not a url at all", "not a URL"),
    ):
        out = register(bad, ids["thread"])
        check(f"rejected ({why})", bool(out.get("error")), True)
    conn = db.connect(paths.DB_PATH)
    try:
        # Counted against the board's own state rather than a literal, for the
        # reason pr_id_of gives: the claim is "none of these added a row", and
        # that stays true however many PRs the sections above registered.
        check("...and none of them created a row",
              len(db.list_prs(conn, include_settled=True)), before)
    finally:
        conn.close()


def check_cli() -> None:
    hr("11. the command line, as a real subprocess")
    scratch = Path(tempfile.mkdtemp(prefix="agentdesk-prs-cli-"))
    db_path = scratch / "AgentDesk" / "agentdesk.db"
    db_path.parent.mkdir(parents=True, exist_ok=True)
    url = "https://github.com/Vispero/GetVoices/pull/23"
    url2 = "https://github.com/Vispero/Fusion/pull/999999"

    # The CLI talks to its own board so that this arm is independent of the
    # app's, and to the same fake gh, because PATH is inherited.
    conn = db.connect(db_path)
    try:
        db.init_db(conn)
        db.register_pr(conn, url, "Vispero/GetVoices", 23, "pin", "builder")
        db.register_pr(conn, url2, "Vispero/Fusion", 999999, "gone", "builder")
    finally:
        conn.close()
    gh_answers({url: {"state": "MERGED", "title": "GetVoices pin"},
                url2: "ERROR:Could not resolve to a PullRequest"})

    import subprocess
    env = dict(os.environ)
    env["LOCALAPPDATA"] = str(scratch)
    env["PATH"] = str(_FAKE_DIR) + os.pathsep + os.environ["PATH"]
    repo_root = str(Path(__file__).resolve().parent.parent)

    def run(*argv):
        return subprocess.run([sys.executable, "-m", "agentdesk.prs", *argv],
                              capture_output=True, text=True, timeout=120,
                              cwd=repo_root, env=env, encoding="utf-8")

    before = run("--list")
    print("  $ python -m agentdesk.prs --list")
    for line in before.stdout.strip().splitlines():
        print(f"      {line}")
    check("--list exits 0", before.returncode, 0)
    check("...and lists both PRs as open", before.stdout.count("[open"), 2)

    checked = run("--check-once")
    print("  $ python -m agentdesk.prs --check-once")
    for line in checked.stdout.strip().splitlines():
        print(f"      {line}")
    check("--check-once exits 0", checked.returncode, 0)
    check("...it merged the one GitHub says is merged",
          f"GetVoices#23: merged" in checked.stdout, True)
    check("...and it did NOT settle the one that errored",
          "Fusion#999999: error" in checked.stdout, True)

    after = run("--list")
    check("...so the merged PR is off the list", url in after.stdout, False)
    check("...and the failed one is still on it", url2 in after.stdout, True)
    check("...with its error shown, so it is not a silent failure",
          "last check failed" in after.stdout, True)
    settled = run("--list", "--include-settled")
    check("--include-settled shows the merged one", url in settled.stdout, True)


def check_look(app, shot: Path | None) -> None:
    hr("12. the tab, as John would see it")
    app.refresh_now()
    app.root.update()
    app.view.goto("prs")
    app.root.update()
    print("  the merge list, row by row:")
    for r in app.view.prs:
        chk = "failed" if r["last_error"] else (r["checked_ts"] or "never")
        print(f"      #{r['id']}  {r['state']:<7} {r['repo']}#{r['number']:<10} "
              f"{r['title'][:30]:<32} {chk}")
    failed = [r["id"] for r in app.view.prs if r["last_error"]]
    show("rows whose last check failed", failed)
    body = body_text(app)
    for pid in failed:
        r = next(x for x in app.view.prs if x["id"] == pid)
        ref = f"{r['repo']}#{r['number']}"[:20]   # the column is 22 wide, fit() clips
        line = [ln for ln in body.splitlines() if ref in ln]
        check(f"...#{pid} renders as failed on screen",
              bool(line) and "failed" in line[0], True)
    show("the title line", repr(app.view.top.get("1.0", "end-1c")[:80]))
    check("the header says the list clears itself",
          "clears itself" in body, True)

    if shot is not None:
        from check_look import screenshot
        app.root.geometry("1000x640")
        app.root.update()
        for _ in range(6):
            app.root.update()
            time.sleep(0.05)
        try:
            path = screenshot(app.root, shot / "merge-list.png")
            show("screenshot", f"saved {path}")
        except Exception as exc:
            show("screenshot", f"FAILED: {exc!r}")


# --- helpers --------------------------------------------------------------------

def thread_messages(thread_id: int) -> list:
    conn = db.connect(paths.DB_PATH)
    try:
        return [dict(m) for m in conn.execute(
            "SELECT author, author_kind, body, meta FROM messages"
            " WHERE thread_id=? ORDER BY id", (thread_id,))]
    finally:
        conn.close()


def count_thread_messages(thread_id: int) -> int:
    return len(thread_messages(thread_id))


def wait_for(predicate, seconds: float) -> bool:
    """Poll until predicate() is true. Returns whether it became true.

    A real wait and not a sleep: the thing being waited for is a background
    thread deciding to do something, and a fixed sleep is either too short (a
    flaky failure) or wasted time on every run.
    """
    deadline = time.time() + seconds
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


def run_check(app, seconds: float = 30) -> bool:
    """Wake the watcher and wait for the PASS TO FINISH. True if one did.

    Not "wait for the first gh call", which is what this script originally did
    and which is a different, weaker fact. A pass checks every open PR in
    sequence, so returning on the first call means every assertion after it
    runs while the rest of that same pass is still in flight -- the first
    version of this harness reported a merge that had not happened yet, and the
    section after it then raced in and posted a notice for a PR that was still
    open, turning one timing bug into nineteen failed checks.

    app.pr_passes is the counter the window keeps of finished passes, including
    passes that died, so this cannot hang on the interesting case.
    """
    before = app.pr_passes
    app.check_prs_now()
    return wait_for(lambda: app.pr_passes > before, seconds)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--shot", type=Path, default=None,
                    help="directory to write a window screenshot into")
    args = ap.parse_args()

    install_fake_gh()
    print(f"scratch board and vault: {_SCRATCH.name}")
    print(f"fake gh on PATH:         {_FAKE_DIR}")

    # The watcher waits this long between passes. Long, so that every section
    # except the ones that deliberately provoke it is the only thing running a
    # check -- a background pass landing mid-section would make the gh call
    # counts meaningless.
    paths.PR_FIRST_CHECK_SECONDS = 3600
    paths.PR_CHECK_SECONDS = 3600

    ids = seed()
    app = appmod.App(paths.DB_PATH)
    app.root.withdraw()
    app.root.update()
    time.sleep(0.3)
    app.root.deiconify()
    app.root.update()
    try:
        gh_answers({})
        check_register_and_tab(ids, app)
        check_clickable(ids, app)
        check_merge_clears(ids, app)
        check_notify_once(ids, app)
        check_failure_never_clears(ids, app)
        check_closed_unmerged(ids, app)
        check_resurrection(ids)
        check_unknown_state(ids)
        check_signature_catches_same_second_error(app)
        check_rejections(ids)
        check_cli()
        check_look(app, args.shot)
    finally:
        app._stop_pr_watch()
        if app.icon is not None:
            try:
                app.icon.stop()
            except Exception:
                pass
        app.root.destroy()

    print()
    print("-" * 78)
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print()
        print(f"scratch left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print()
    print("WHAT IS NOT ASSERTED HERE: that `gh` is installed and authenticated")
    print("on this machine, or that any of these URLs exist. The subprocess is")
    print("real but its answer is a file. That check is the live query pasted")
    print("into the report, not an assertion in this script.")
    print()
    print(f"scratch left at {_SCRATCH}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
