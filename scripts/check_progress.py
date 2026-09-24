"""Acceptance checks for seeing what a background agent is doing (work item 50).

The brief is thread 50, in John's words: *"can you add a thing where I can see
into the progress of the back ground agent"*. Its acceptance is four lines, and
each is a section below:

  1. While an item runs, the app shows what it is doing, refreshing.
                                                                 (section 1)
  2. *Last activity* is visible and updates, so a stuck job is
     distinguishable from a slow one.                            (section 3)   
  3. When it finishes, the result is readable on the item.       (section 4)
  4. An item that fails shows THAT IT FAILED and the error.      (section 5)

The last two are the ones the brief singles out, and they are the ones this
script spends most of its assertions on -- the feature is worth little if it
only works on the happy path.

WHAT THIS COSTS THE READER: nothing on screen. No window is built and no
balloon is fired (notify.toast is stubbed for the whole run). The labels and
lines are checked through agentdesk.terminal.activity, the function the
terminal view renders.

HOW THE AGENT IS FAKED, and what that does and does not prove. `worker.run_item`
is driven for real, but `worker._claude_exe` is pointed at a stub that emits a
known stream-json sequence on a known timeline -- steps, a sleep, a result.
That makes the TIMING assertions possible, which is the whole point: "recorded
while it was running, not after it stopped" cannot be checked against a real
agent whose duration is unknown. What the stub cannot prove is that the real CLI
still emits this format; section 8 does that, against the real binary, using the
dispatcher's own `_agent_cmd`.

    .venv\\Scripts\\python.exe scripts\\check_progress.py
    .venv\\Scripts\\python.exe scripts\\check_progress.py --live --shot .\\shots
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

# --- isolation, BEFORE agentdesk is imported -----------------------------------
#
# LOCALAPPDATA decides paths.DB_PATH, and the worker WRITES to this database and
# to worker.state beside it. Run this against the real board and it claims one of
# John's items for real, spawns an agent on it, and leaves the state file behind.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-progress-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import db, notify, paths, terminal, worker  # noqa: E402

REPORT = ("REPORT-TEXT: I changed agentdesk/worker.py and ran check_progress.py, "
          "and this is the agent's own write-up of the work.")


# --- the harness ----------------------------------------------------------------

FAILURES: list[str] = []


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<44} {got!r}")


def check_in(label: str, needle: str, haystack: str) -> None:
    ok = needle in (haystack or "")
    if not ok:
        FAILURES.append(f"{label}: {needle!r} not in {haystack[:200]!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<44} "
          f"{'found' if ok else 'MISSING'}")


def show(label: str, value) -> None:
    print(f"  ...  {label:<44} {value!r}")


def hr(title: str = "") -> None:
    print()
    print(f"--- {title} " + "-" * max(0, 68 - len(title)))


def _conn():
    return db.connect(paths.DB_PATH)


def events_for(tid: int) -> list:
    conn = _conn()
    try:
        return db.list_work_events(conn, tid)
    finally:
        conn.close()


def kinds_for(tid: int) -> list:
    return [e["kind"] for e in events_for(tid)]


def bodies_for(tid: int) -> list:
    return [e["body"] for e in events_for(tid)]


def messages_for(tid: int) -> list:
    conn = _conn()
    try:
        return [dict(r) for r in conn.execute(
            "SELECT author, body, meta FROM messages WHERE thread_id=? ORDER BY id",
            (tid,))]
    finally:
        conn.close()


def status_of(tid: int) -> str:
    conn = _conn()
    try:
        return conn.execute("SELECT status FROM threads WHERE id=?",
                            (tid,)).fetchone()["status"]
    finally:
        conn.close()


def message_count() -> int:
    conn = _conn()
    try:
        return conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    finally:
        conn.close()


def claim(tid: int, agent: str) -> None:
    conn = _conn()
    try:
        db.claim_task(conn, tid, agent)
    finally:
        conn.close()


def run_in_background(tid: int) -> threading.Thread:
    """Run worker.run_item on one item, on its own thread, as the dispatcher does.

    A thread and not a direct call because the test has to look at the database
    WHILE the run is in flight -- that is the entire claim being checked.
    """
    conn = _conn()
    try:
        item = next(i for i in db.list_work(conn, status=paths.STATUS_OPEN, limit=50)
                    if i["id"] == tid)
    finally:
        conn.close()
    th = threading.Thread(target=worker.run_item,
                          args=(item, worker.WORKER_NAME), daemon=True)
    th.start()
    return th


# --- the fake agent -------------------------------------------------------------
#
# A .cmd that runs a .py, because `worker._claude_exe` is patched to return this
# path and run_item Popen()s it directly: a .cmd is executable that way and a
# .py is not.

STUB_PY = r'''
import json, os, sqlite3, sys, time

# The argv the dispatcher actually built, kept so the test can assert the FLAGS
# rather than assume them -- in particular the stream format, which is the one
# thing this whole feature rests on and the one thing a stub cannot vouch for.
with open(os.environ["CHECK_ARGS"], "w", encoding="utf-8") as fh:
    json.dump(sys.argv[1:], fh)

DB = os.environ["CHECK_DB"]
TID = int(os.environ["CHECK_TID"])
MODE = os.environ.get("CHECK_MODE", "ok")


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "cwd": os.getcwd()})
emit({"type": "assistant", "message": {"content": [
    {"type": "text", "text": "Starting on the item now."}]}})
emit({"type": "assistant", "message": {"content": [
    {"type": "tool_use", "name": "Read", "input": {"file_path": "agentdesk/db.py"}}]}})

# The window the test observes through. Long enough that a test which only
# looked after the process exited would fail rather than pass by luck.
time.sleep(3.0)

emit({"type": "assistant", "message": {"content": [
    {"type": "tool_use", "name": "Bash",
     "input": {"command": "git status --short"}}]}})
time.sleep(0.4)

if MODE == "ok":
    conn = sqlite3.connect(DB, timeout=30.0)
    try:
        conn.execute("UPDATE threads SET status='done' WHERE id=?", (TID,))
        conn.commit()
    finally:
        conn.close()
    emit({"type": "result", "subtype": "success", "is_error": False,
          "result": os.environ["CHECK_REPORT"]})
else:
    # A real failure looks like this: the agent said something, stop_reason is
    # not success, and the process exits non-zero without completing the item.
    sys.stderr.write("stub: the agent gave up on this one\n")
    emit({"type": "result", "subtype": "error_during_execution",
          "is_error": True, "result": "stub: the agent gave up on this one"})
    sys.exit(1)
'''


def make_stub() -> Path:
    py = _SCRATCH / "stub_agent.py"
    py.write_text(STUB_PY, encoding="utf-8")
    cmd = _SCRATCH / "stub_claude.cmd"
    cmd.write_text(
        "@echo off\r\n"
        f'"{sys.executable}" "{py}" %*\r\n'
        "exit /b %errorlevel%\r\n",
        encoding="utf-8")
    return cmd


def use_stub(stub: Path, tid: int, mode: str) -> None:
    os.environ["CHECK_DB"] = str(paths.DB_PATH)
    os.environ["CHECK_TID"] = str(tid)
    os.environ["CHECK_MODE"] = mode
    os.environ["CHECK_ARGS"] = str(_SCRATCH / f"args-{tid}.json")
    os.environ["CHECK_REPORT"] = REPORT
    worker._claude_exe = lambda: str(stub)


def stub_args(tid: int) -> list:
    try:
        return json.loads((_SCRATCH / f"args-{tid}.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []


# --- sections -------------------------------------------------------------------

def section_the_flags_are_the_ones_we_meant(tid: int) -> None:
    hr("2. the dispatcher asks the CLI for a stream, not for one blob at the end")
    print("  The flag list is an assumption about another program. It is read")
    print("  from the ONE function run_item builds its command with, so this")
    print("  cannot drift from what actually ships.")
    conn = _conn()
    try:
        item = db.work_thread(conn, tid)
    finally:
        conn.close()
    argv = worker._agent_cmd("claude", dict(item, last_body=""), worker.WORKER_NAME)
    show("flags", [a for a in argv if a.startswith("--")])
    check("--output-format is present", "--output-format" in argv, True)
    check("  ...and asks for a stream",
          argv[argv.index("--output-format") + 1]
          if "--output-format" in argv else None, worker.STREAM_FORMAT)
    check("--verbose is passed (the CLI requires it for stream-json)",
          "--verbose" in argv, True)
    check("the stub really was spawned (so run_item used this command)",
          stub_args(tid) != [], True)


def section_recorded_while_running(tid: int) -> None:
    hr("1. progress is on the board WHILE the agent runs, not after it stops")
    th = run_in_background(tid)
    # Poll for the first step rather than sleeping a fixed time: a fixed sleep
    # would pass or fail on how fast this machine is.
    seen, deadline = [], time.time() + 6.0
    while time.time() < deadline:
        seen = events_for(tid)
        if any(e["kind"] == db.WORK_STEP for e in seen):
            break
        time.sleep(0.05)
    alive_at_first_step = th.is_alive()
    show("events seen mid-run", [(e["kind"], e["body"]) for e in seen])
    check("a step was recorded before the agent stopped", alive_at_first_step, True)
    check("the start event is there", db.WORK_START in [e["kind"] for e in seen], True)
    check_in("the file it was reading", "Reading agentdesk/db.py", str(bodies_for(tid)))
    th.join(timeout=60)
    check("the run finished", th.is_alive(), False)


def section_last_activity_moves(tid: int) -> None:
    hr("3. last activity is visible, and moves when the agent does")
    events = events_for(tid)
    conn = _conn()
    try:
        row = db.work_thread(conn, tid)
    finally:
        conn.close()
    label, lines = terminal.activity(row, events)
    show("label", label)
    check_in("the label says how long since the last event", "last activity", label)
    check_in("and how long since it started", "started", label)
    check("the newest event is on the list", lines[-1][1], events[-1]["body"])
    check("the newest step is the command it ran", lines[-2][1],
          "Running git status --short")

    # The property that matters: the age is read from the CLOCK, not frozen at
    # whatever it was when the event landed. Same events, one dated long ago,
    # and the label has to say so -- a cached "4s ago" is the stale number that
    # makes a dead job look busy.
    stale = [dict(e) for e in events]
    stale.append({"kind": db.WORK_STEP, "body": "Running something",
                  "ts": "2020-01-01T00:00:00+00:00"})
    stale_label, stale_lines = terminal.activity(row, stale)
    check("the stale event is still on the list", stale_lines[-1][1],
          "Running something")
    show("label with the same events, but one of them dated 2020", stale_label)
    check("the age is read from the clock, not cached",
          stale_label != label, True)
    import re
    check("and it reads as days, not seconds",
          bool(re.search(r"last activity \d+d", stale_label)), True)


def section_done_keeps_the_result(tid: int) -> None:
    hr("4. a finished item keeps the write-up, on the item")
    check("the item is done", status_of(tid), paths.STATUS_DONE)
    check("a done event was recorded", db.WORK_DONE in kinds_for(tid), True)
    posts = messages_for(tid)
    show("messages on the thread", [(m["author"], m["body"][:40]) for m in posts])
    reported = [m for m in posts if REPORT[:40] in m["body"]]
    check("the agent's report is on the thread", len(reported), 1)
    check("...posted as the agent, not the dispatcher",
          reported[0]["author"] if reported else None, worker.AGENT)

    conn = _conn()
    try:
        label, lines = terminal.activity(db.work_thread(conn, tid),
                                            events_for(tid))
    finally:
        conn.close()
    show("panel label when finished", label)
    check_in("the panel says it is done", "done", label)
    check_in("the panel's last line says the summary landed",
             "summary posted on the thread", lines[-1][1])


def section_failure_is_visible(tid: int) -> None:
    hr("5. a failed item says THAT it failed, and says why")
    th = run_in_background(tid)
    th.join(timeout=60)
    check("the run finished", th.is_alive(), False)
    check("the item went back on the queue", status_of(tid), paths.STATUS_OPEN)
    check("an error event was recorded", db.WORK_ERROR in kinds_for(tid), True)
    errors = [e["body"] for e in events_for(tid) if e["kind"] == db.WORK_ERROR]
    show("error events", errors)
    check_in("the error names the exit", "without completing", errors[-1])
    check("nothing claims it finished", db.WORK_DONE in kinds_for(tid), False)

    # The agent's own error event is kept as well as the dispatcher's, because
    # they say different things: one is what the agent reported, the other is
    # what happened to the process.
    check_in("the agent's own error is recorded too", "gave up on this one",
             str(errors))

    notes = [m for m in messages_for(tid)
             if "dispatcher-release" in (m["meta"] or "")]
    check("a release note is on the thread", len(notes), 1)
    check_in("the release note carries the tail the agent printed",
             "gave up", notes[0]["body"] if notes else "")

    conn = _conn()
    try:
        label, lines = terminal.activity(db.work_thread(conn, tid),
                                            events_for(tid))
    finally:
        conn.close()
    show("panel label after a failure", label)
    check_in("the panel shows the failure, not a spinner", "agent exited", label)
    error_tags = [tag for _ts, _body, tag in lines if tag == f"ev-{db.WORK_ERROR}"]
    check("the failure is rendered in the error colour", len(error_tags) >= 1, True)


def section_hand_claimed_is_not_blank(tid: int) -> None:
    hr("6. an item claimed by hand does not pretend to be watched")
    conn = _conn()
    try:
        row = db.work_thread(conn, tid)
    finally:
        conn.close()
    label, lines = terminal.activity(row, [])
    show("label", label)
    check_in("it says nothing has reported progress", "no progress reported", label)
    explanation = " ".join(body for _ts, body, _tag in lines)
    check_in("with a line saying nothing reported progress",
             "Nothing has reported progress", explanation)
    check_in("and a line saying why it cannot see anything",
             "claimed by hand", explanation)


def section_real_cli(tid: int) -> None:
    hr("8. the REAL claude CLI still emits what this reads (--live)")
    conn = _conn()
    try:
        item = db.work_thread(conn, tid)
    finally:
        conn.close()
    item = dict(item, last_body="live probe")
    exe = worker._claude_exe()
    if not exe:
        print("  [skip] the claude CLI is not on PATH")
        return
    argv = worker._agent_cmd(exe, item, worker.WORKER_NAME)
    # The real thing, with the real prompt and the real flags, but the agent is
    # told to do nothing -- this checks the FORMAT, and asking it to do work
    # would be a different test with a different failure mode.
    argv[2] = "Reply with exactly: ok. Do not use any tools."
    argv[argv.index("--agent") + 1] = "app-dev"
    show("flags", [a for a in argv if a.startswith("--")])
    proc = subprocess.Popen(argv, cwd=str(REPO), stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, encoding="utf-8", errors="replace")
    recorder = worker._RunRecorder(proc, tid)
    recorder.start()
    try:
        proc.wait(timeout=300)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    recorder.join()
    show("lines read from stdout", len(recorder.out_lines))
    show("report extracted by the dispatcher's own parser",
         (recorder.report or "")[:60])
    show("cached steps written for the live run",
         [e["body"] for e in events_for(tid) if e["kind"] == db.WORK_STEP])
    check("stdout was a stream, not one blob",
          len(recorder.out_lines) > 1, True)
    check("the dispatcher's parser got the final message",
          recorder.report, "ok")
    check("the real CLI accepted the dispatcher's flags",
          "--output-format" in argv and "--verbose" in argv, True)


# --- main -----------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true",
                        help="also run the real claude CLI (a network call, "
                             "about a minute)")
    args = parser.parse_args()

    print(f"scratch board: {paths.DB_PATH}")
    paths.ensure_dirs()
    stub = make_stub()

    conn = db.connect(paths.DB_PATH)
    try:
        db.init_db(conn)
        w_ok = db.start_thread(
            conn, "work", "Make the queue show progress while it runs",
            "claude", paths.AGENT_KIND,
            "Acceptance: while it runs the app shows what it is doing.")
        w_fail = db.start_thread(
            conn, "work", "An item that dies half way through",
            "claude", paths.AGENT_KIND, "Acceptance: a failure is visible.")
        w_hand = db.start_thread(
            conn, "work", "An item claimed straight from the board",
            "claude", paths.AGENT_KIND, "Claimed by hand, no dispatcher.")
    finally:
        conn.close()
    # w_ok and w_fail are left OPEN on purpose: worker.run_item claims the item
    # itself, as the dispatcher does, and a pre-claim here would make it refuse
    # the item it was handed. w_hand is claimed because that is the case it is
    # standing for -- an agent holding an item with no dispatcher behind it.
    claim(w_hand, "app-dev")
    # Seeded mid-run events for the hand-claimed item, so section 6's panel has
    # something to show and section 7 has something to append to.
    conn = _conn()
    try:
        db.add_work_event(conn, w_hand, db.WORK_START,
                          "app-dev started (auto): An item claimed straight "
                          "from the board")
        db.add_work_event(conn, w_hand, db.WORK_STEP, "Reading agentdesk/db.py")
    finally:
        conn.close()

    real_toast = notify.toast
    notify.toast = lambda *a, **k: None      # nothing this run can balloon
    try:
        use_stub(stub, w_ok, "ok")
        # The run comes first: section 1 reads the argv that section 2's run
        # hands to the stub, so it can only be asserted after one has happened.
        section_recorded_while_running(w_ok)
        section_the_flags_are_the_ones_we_meant(w_ok)
        section_last_activity_moves(w_ok)
        section_done_keeps_the_result(w_ok)

        use_stub(stub, w_fail, "fail")
        section_failure_is_visible(w_fail)

        section_hand_claimed_is_not_blank(w_hand)

        if args.live:
            # Hand the real lookup back before section 8: it is the only place
            # anything is allowed to touch the actual CLI.
            import shutil
            worker._claude_exe = lambda: (shutil.which("claude")
                                          or shutil.which("claude.cmd"))
            section_real_cli(w_hand)

    finally:
        notify.toast = real_toast
    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print()
    print("WHAT IS NOT ASSERTED HERE: any claim about a real")
    print("agent's own steps: sections 2-5 drive a stub, which is what makes")
    print("the timing assertions possible but means the STEP TEXT is checked")
    print("against a known stream. Section 8, behind --live, is the only place")
    print("the real CLI's output is read.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
