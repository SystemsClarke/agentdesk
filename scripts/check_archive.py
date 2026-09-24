"""Acceptance checks for archiving a settled question (work item 48).

The brief is thread 48 message 88, in John's words: *"when a question is closed
it archives the chat and I don't see it anymore"*. Its acceptance is four
lines, and each is a section below:

  1. Post a question, reply as a human, and the thread leaves the default list
     with no other action taken.                                  (section 3)
  2. `open_questions` no longer lists it, and the toast stops.    (section 4)
  3. It is still findable by `search_messages`, and still readable via
     `read_thread`.                                               (section 5)
  4. It can be brought back.                                      (section 6)

Sections 2 and 7 pin the parts that are easy to get wrong and hard to see: the
`include_archived` flags, and the fact that bringing a question back must
SURVIVE the sweep. That last one is the whole reason an archive hold exists --
without it the restored row flashes on the tab for one poll tick and vanishes,
which reads as the button being broken rather than as the archive being eager.

WHAT THIS COSTS THE READER: nothing on screen. No tray icon is built, the seeds
all go in before the window starts (so the window's first poll absorbs them and
announces nothing), and the one call that could have fired a toast is checked
against a spy around `notify.toast` instead. So this runs without a single
balloon -- which is why "the toast stops" is checked at the layer that decides
it rather than by watching for a banner.

    .venv\\Scripts\\python.exe scripts\\check_archive.py
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
import time
from pathlib import Path

# --- isolation, BEFORE agentdesk is imported -----------------------------------
#
# LOCALAPPDATA decides paths.DB_PATH, and the vault paths are rewritten below
# because the archive WRITES to the vault -- run this against the real board and
# it files one of John's questions for real.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-archive-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import app as appmod               # noqa: E402
from agentdesk import db, mcp_server, notify, paths, vault  # noqa: E402

db.ANSWERED_GRACE_HOURS = 0  # these checks test filing itself, not the grace period

_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = _VAULT / "questions"

WIDTH = 78
FAILURES: list[str] = []


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<44} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<42} {got!r}")


def note(text: str) -> None:
    for line in text.strip().splitlines():
        print(f"  {line}")


# --- helpers --------------------------------------------------------------------

def status_of(thread_id: int) -> str:
    conn = db.connect(paths.DB_PATH)
    try:
        return db.get_thread(conn, thread_id)["thread"]["status"]
    finally:
        conn.close()


def meta_of(thread_id: int) -> dict:
    conn = db.connect(paths.DB_PATH)
    try:
        row = db.get_thread(conn, thread_id)["thread"]
    finally:
        conn.close()
    try:
        return json.loads(row["meta"] or "{}") or {}
    except (TypeError, ValueError):
        return {}


def open_ids() -> list:
    conn = db.connect(paths.DB_PATH)
    try:
        return sorted(q["thread_id"] for q in db.open_questions(conn))
    finally:
        conn.close()


def listed_ids(app) -> list:
    """The ids the Questions list is showing right now, from the terminal view."""
    return [int(r["id"]) for r in app.view.rows["question"]]


def goto_questions(app) -> None:
    """Show the Questions list, as selecting the old tab did.

    Recorded as a check rather than allowed to raise: if the list screen cannot
    render, that is a failure to report, and the backend checks after it still
    deserve to run. On failure the view is put back on the main screen so later
    redraws do not keep re-raising the same error.
    """
    try:
        app.view.goto("list", "question")
        app.root.update()
        err = None
    except Exception as e:  # noqa: BLE001
        err = f"{type(e).__name__}: {e}"
        app.view.screen = "main"
    check("the Questions list screen renders", err, None)


TOASTS: list = []


def spy_toast():
    """Record what notify_open_questions WOULD have shown.

    A spy and not a call: the real toast() with no icon goes out through
    PowerShell, which pops a real banner on whoever's screen this is, and the
    question here is what the toast was told to say rather than whether a
    banner appeared. Recorded rather than suppressed, so the assertion is about
    the argument and not about the call having happened.
    """
    real = notify.toast

    def spy(title, message, **kw):
        TOASTS.append({"title": title, "message": message})
        return True

    notify.toast = spy
    return real


# --- sections -------------------------------------------------------------------

def section_flags() -> None:
    hr("2. the flags the brief asks for, and what they default to")
    import inspect

    oq = inspect.signature(db.open_questions).parameters
    lt = inspect.signature(db.list_threads).parameters
    check("db.open_questions takes include_archived", "include_archived" in oq, True)
    check("...defaulting to False (unchanged)", oq["include_archived"].default, False)
    check("db.list_threads takes include_archived", "include_archived" in lt, True)
    check("...defaulting to True (unchanged)", lt["include_archived"].default, True)
    check("and the old exclude_archived is gone",
          "exclude_archived" in lt, False)

    mq = inspect.signature(mcp_server.open_questions).parameters
    ml = inspect.signature(mcp_server.list_threads).parameters
    check("the MCP open_questions tool takes it", "include_archived" in mq, True)
    check("the MCP list_threads tool takes it", "include_archived" in ml, True)

    print()
    note("The bodies are wrapped by @server.tool(), so the signatures above are")
    note("the declared ones. What they actually DO is sections 4 and 6.")


def section_settle_and_file(app, ids) -> None:
    hr("3. a human reply files the question, with no other action taken")
    tid = ids["q1"]
    show("the question is, before", status_of(tid))
    print()
    note("The ONLY call below is app.post_reply -- the same method the Reply")
    note("box calls. Nothing calls vault.archive_question, and nothing touches")
    note("the status: the window's own poll sweep is what files it.")
    print()

    before = listed_ids(app)

    app.post_reply("question", tid, "Keep them; the mirror is the point.")
    app.root.update()

    show("its status is now", status_of(tid))
    show("its meta is now", meta_of(tid))
    check("the reply settled it", status_of(tid), paths.STATUS_ARCHIVED)

    files = sorted(p.name for p in paths.VAULT_QUESTIONS.glob("*.md"))
    show("vault files", files)
    check("the transcript is in the vault", f"question-{tid}.md" in files, True)

    now = listed_ids(app)
    show("the tab showed, before", before)
    show("the tab shows, after", now)
    check("it left the default list", tid in now, False)
    check("...and left no hole (the others are still there)",
          [i for i in before if i != tid], now)


def section_toast_stops(app, ids) -> None:
    hr("4. open_questions no longer lists it, and the toast stops")
    tid = ids["q1"]
    check("open_questions does not list it", tid in open_ids(), False)
    check("...but include_archived=True does",
          tid in [q["thread_id"] for q in _open_archived()], True)

    # The title counts the open questions. The archived one must be out of that
    # count -- checked as a PROPERTY of the live count rather than against a
    # hard-coded "0 open questions", because a second question is deliberately
    # left open here (sections 5 and 6 need a live row to prove the archive did
    # not take the whole tab with it).
    n_open = len(open_ids())
    title = app.root.title()
    show("the window title", title)
    show("questions still open", n_open)
    want = f"{n_open} open question" + ("" if n_open == 1 else "s")
    check("the title counts exactly the open ones", want in title, True)
    top = app.view.top.get("1.0", "end-1c")
    show("the terminal top line", top.strip())
    check("the view rings for exactly the open ones", len(app.view.open_qs), n_open)
    check("...and says so on the top line",
          f"{n_open} ringing for you" in top if n_open else "nobody's calling" in top, True)

    print()
    note("and the hourly toast. notify_open_questions is called for real -- it")
    note("is the function that decides what to say -- with toast() spied so no")
    note("banner appears. The first call announces, the second must stay quiet:")
    note("that dedupe is what 'the toast stops' means in practice.")
    state = _SCRATCH / "notify_state.json"
    real = spy_toast()
    try:
        TOASTS.clear()
        first = notify.notify_open_questions(_conn(), state_path=state)
        second = notify.notify_open_questions(_conn(), state_path=state)
    finally:
        notify.toast = real
    show("announced, first call", first)
    show("announced, second call", second)
    show("what it would have shown", TOASTS[-1] if TOASTS else None)
    check("it announced exactly what is still open", first, n_open)
    check("the second call stayed quiet", second, 0)
    check("...and showed nothing", len(TOASTS), 1)
    for t in TOASTS:
        check("the toast does not name the archived question",
              "submodule" in json.dumps(t).lower(), False)


def section_still_findable(ids) -> None:
    hr("5. still findable by search_messages, still readable by read_thread")
    tid = ids["q1"]
    note("Through the MCP tools themselves, not through db.py -- the acceptance")
    note("names the tools, and a tool that returns the wrong envelope is a")
    note("failure this would otherwise miss.")
    print()

    found = json.loads(mcp_server.search_messages("submodule", limit=20))
    hits = found.get("results", [])
    show("search_messages('submodule') hit ids",
         sorted({m["thread_id"] for m in hits}))
    check("search finds the archived question",
          tid in [m["thread_id"] for m in hits], True)
    check("...by its body, which only the archived transcript holds",
          any("drops them" in m["body"] for m in hits), True)

    got = json.loads(mcp_server.read_thread(tid, author="builder"))
    check("read_thread returns the thread", got.get("thread", {}).get("id"), tid)
    check("...with every message", len(got.get("messages", [])), 2)
    shown = [m["body"] for m in got.get("messages", [])]
    check("...including the human's answer",
          any("Keep them" in b for b in shown), True)
    print()
    note("a receipt is posted by that read; that is read_thread's documented")
    note("behaviour and it is on the scratch board, not John's.")


def section_filter_and_bring_back(app, ids) -> None:
    hr("6. the Archived filter, and bringing it back")
    view = app.view
    tid = ids["q1"]

    show("filter starts on", "Archived" if view.show_archived else "Active")
    check("the tab opens on Active", view.show_archived, False)
    check("...and the archived one is not in it", tid in listed_ids(app), False)

    # Close the poll's gate first. It only redraws when its change signature
    # moves, and section 5's read_thread posted a receipt, so without this the
    # flip below would be redrawn by that unrelated message and would pass
    # whether or not switching the filter can redraw on its own. The empty list
    # asserted here is what "nothing changed underneath" looks like.
    app.refresh_now()
    app.root.update()
    check("the tab is showing exactly the Active list before the flip",
          listed_ids(app), [ids["q2"]])

    view.show_archived = True
    app.redraw()
    app.root.update()
    show("after switching to Archived, the tab shows", listed_ids(app))
    check("the archived question is listed", tid in listed_ids(app), True)
    check("...and it is the ONLY thing listed", listed_ids(app), [tid])

    print()
    note("now Bring back, through app.unarchive_question (what 'u' calls).")
    if tid not in listed_ids(app):
        # Selecting a row that is not there raises TclError, which would abort
        # the run and hide every check after it behind a traceback. On a
        # working board this is unreachable; on broken code it is the whole
        # point of running this, so it reports instead of crashing.
        check("the archived row is on the tab, to press the button on", False, True)
        note("the rest of this section needs a row to press the button on.")
        return
    app.unarchive_question(tid)
    app.root.update()

    show("its status is now", status_of(tid))
    show("its meta is now", meta_of(tid))
    show("the filter is now", "Archived" if view.show_archived else "Active")
    check("it is settled again, not reopened", status_of(tid), paths.STATUS_ANSWERED)
    check("...and it carries the hold", meta_of(tid).get("archive_hold"), True)
    check("the filter flipped back to Active", view.show_archived, False)
    check("it is back on the tab", tid in listed_ids(app), True)
    check("open_questions still does not list it", tid in open_ids(), False)


def section_traps(app, ids) -> None:
    hr("7. the traps: re-filing, double presses, and the queue")
    tid = ids["q1"]

    note("A hold must survive the sweep. This is the whole reason it exists:")
    note("restoring a question puts it back to 'answered', which is exactly")
    note("what the sweep looks for, so without the hold the row would flash")
    note("and vanish within one poll.")
    for _ in range(3):
        app.refresh_now()
        app.root.update()
    show("after three more polls, its status is", status_of(tid))
    check("the sweep left the restored question alone",
          status_of(tid), paths.STATUS_ANSWERED)
    check("...and it is still on the tab", tid in listed_ids(app), True)

    print()
    check("a second Bring back changes nothing",
          db.unarchive_thread(*_conn_status(tid)), False)

    print()
    note("Close & archive must still be able to re-file it -- that is the one")
    note("gesture that clears the hold.")
    app.close_question(tid)
    app.root.update()
    show("its status is now", status_of(tid))
    show("its meta is now", meta_of(tid))
    check("it is archived again", status_of(tid), paths.STATUS_ARCHIVED)
    check("the hold is gone", meta_of(tid).get("archive_hold"), None)
    check("...and it recorded what it was settled as",
          meta_of(tid).get("archived_from"), paths.STATUS_CLOSED)
    check("it left the tab", tid in listed_ids(app), False)

    print()
    note("and the other channels are untouched by any of this. A discussion is")
    note("born 'fyi', not 'open' -- this asserts only that the sweep never")
    note("reaches it, not what its own default is.")
    conn = db.connect(paths.DB_PATH)
    try:
        d = [r["id"] for r in db.list_threads(conn, channel="discussion")]
    finally:
        conn.close()
    check("the discussion thread is still listed", ids["disc"] in d, True)
    check("a discussion is never archived",
          status_of(ids["disc"]) != paths.STATUS_ARCHIVED, True)

    print()
    note("finally: settle the last question and the toast goes quiet entirely.")
    q2 = ids["q2"]
    app.close_question(q2)
    app.root.update()
    check("it is archived", status_of(q2), paths.STATUS_ARCHIVED)
    check("nothing is open any more", open_ids(), [])

    state = _SCRATCH / "notify_state.json"
    real = spy_toast()
    try:
        TOASTS.clear()
        announce = notify.notify_open_questions(_conn(), state_path=state)
        shown = TOASTS[-1] if TOASTS else None
    finally:
        notify.toast = real
    show("the last toast would have said", shown)
    show("and announced this many", announce)
    check("it is an all-clear", "all clear" in shown["title"] if shown else False, True)
    check("...with an empty body", shown["message"] if shown else None,
          "No open questions.")

    # The announce count is 0 here -- and so is "stayed quiet", deliberately:
    # notify_open_questions cannot tell those apart through its return value,
    # which is why the assertion above reads the toast text instead.
    show("the title now", app.root.title())
    check("the title carries no count", "open question" in app.root.title(), False)


# --- small connection shims ------------------------------------------------------

def _conn():
    return db.connect(paths.DB_PATH)


def _open_archived():
    conn = db.connect(paths.DB_PATH)
    try:
        return db.open_questions(conn, include_archived=True)
    finally:
        conn.close()


def _conn_status(thread_id):
    """(conn, thread_id) for a direct db call -- caller closes."""
    return db.connect(paths.DB_PATH), thread_id


# --- main -----------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shot", type=Path, default=None,
                        help="directory to write a PNG of the window into")
    args = parser.parse_args()

    print(f"scratch board: {paths.DB_PATH}")
    print(f"scratch vault: {paths.VAULT_QUESTIONS}")
    paths.ensure_dirs()

    conn = db.connect(paths.DB_PATH)
    try:
        db.init_db(conn)
        ids = {
            "q1": db.start_thread(
                conn, "question",
                "Should the mirror keep the submodules it clones?",
                "claude-code", paths.AGENT_KIND,
                "The mirror pass currently drops them and the runs then fetch "
                "them one commit at a time."),
            "q2": db.start_thread(
                conn, "question", "Which vault does the wiki mirror write into?",
                "researcher", paths.AGENT_KIND, "It is hard-coded today."),
            "disc": db.start_thread(
                conn, "discussion", "The dispatcher releases on an unknown model",
                "builder", paths.AGENT_KIND, "Two releases tonight, same cause."),
        }
    finally:
        conn.close()

    app = None
    try:
        # Seeded BEFORE the window: the first poll absorbs what already exists
        # and announces none of it, so this run shows John nothing.
        app = appmod.App(paths.DB_PATH)
        app.root.withdraw()
        app.root.update()
        time.sleep(0.3)
        for _ in range(3):
            app.refresh_now()
            app.root.update()
        goto_questions(app)
        app.root.update()

        show("open questions at the start", open_ids())
        section_flags()
        section_settle_and_file(app, ids)
        section_toast_stops(app, ids)
        section_still_findable(ids)
        section_filter_and_bring_back(app, ids)
        section_traps(app, ids)

        if args.shot is not None:
            args.shot.mkdir(parents=True, exist_ok=True)
            from check_look import screenshot
            app.root.deiconify()
            app.root.update()
            time.sleep(0.3)
            show("screenshot (active)", screenshot(
                app.root, (args.shot / "questions-active.png").resolve()))
            app.view.show_archived = True
            app.redraw()
            for _ in range(5):
                app.root.update()
                time.sleep(0.15)
            # Printed so the picture can be trusted: an empty tree in the PNG
            # is a rendering race unless these ids are also empty.
            show("the archived list actually holds",
                 listed_ids(app))
            show("screenshot (archived)", screenshot(
                app.root, (args.shot / "questions-archived.png").resolve()))
            app.root.withdraw()
    finally:
        notify.toast = getattr(notify, "toast", notify.toast)
        if app is not None:
            if app.icon is not None:
                try:
                    app.icon.stop()
                except Exception:
                    pass
            app.root.destroy()

    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print()
    print("WHAT IS NOT ASSERTED HERE: that the archived row LOOKS settled on")
    print("the tab -- the word and the grey are read by a person, and --shot is")
    print("the evidence for that rather than an assertion. And no banner was")
    print("watched for: section 4 reads what the toast would have said, through")
    print("a spy, because a real one would pop on John's screen to prove a")
    print("point about a function's arguments.")
    print(f"\nscratch board and vault left at {_SCRATCH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
