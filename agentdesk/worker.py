"""The dispatcher: watch the Work to Hire queue and start an agent on each item.

John's ask, verbatim: "a background task that will look at the Work to Hire
board and spin up subagents to do that work with a good sr dev system prompt to
get the work done. this is so that you all can get work done with out me having
to keep an agent work on that work."

Three decisions here are load-bearing, and they are the reason this file has
comments at all.

1. THE DISPATCHER CLAIMS, THE AGENT DOES NOT. `db.claim_task` checks and writes
   in a single statement, so exactly one claimant can win. If the spawned agent
   claimed for itself, there would be a gap between the dispatcher deciding to
   start it and that agent getting as far as claiming -- and a second dispatcher,
   or a person working the queue by hand, could take the same item inside that
   gap and both would run. Claiming first makes "decided to do it" and "has the
   right to do it" the same event.

2. EVERY FAILED RUN RELEASES THE ITEM. A claim is a lock and a lock with no
   release leaks. If the agent dies, times out, or is killed, the item sits in
   'claimed' for ever -- not open, so no other agent will pick it up; not done,
   so nobody notices it stopped. The queue silently loses the work, which is
   worse than either succeeding or failing loudly. So every exit path below
   either completes the item or releases it.

3. IT DOES NOT SECOND-GUESS THE WORK. The dispatcher never judges whether an
   item is worth doing, whether the result is any good, or whether the agent
   chose the right approach. It hands the item over and records what happened.
   The moment it starts having opinions, the board has two accounts of every
   task and nobody can tell which one is acting.

Run it in the foreground. A service that starts editing a shared repository at
boot should be something John switched on deliberately, not something he
discovers later -- so there is no install-on-startup here on purpose. See
`--once` for a single pass, which is also how it is tested.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Optional

try:
    from agentdesk import db, paths
except ImportError:  # pragma: no cover - depends on how it was launched
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from agentdesk import db, paths

REPO = Path(__file__).resolve().parent.parent

# The agent the work is handed to. It carries its own standing brief, which is
# where the "senior dev system prompt" John asked for actually lives -- see
# C:\Users\palencharj\.claude\agents\app-dev\CLAUDE.md. Putting a second copy of
# it here would guarantee the two drift.
AGENT = "app-dev"

# How the spawned agent may act without a human in the loop.
#
# 'acceptEdits' lets it read, grep and edit files unattended but NOT run
# commands -- every Bash call is denied in headless mode. That is a real limit
# and it matters: the agent's own brief requires it to VERIFY by running things,
# so under this mode it will report more often that it could not prove a result.
# Raising this to 'bypassPermissions' removes that limit and also removes the
# last thing standing between an unattended agent and this machine.
#
# Default stays 'acceptEdits'. A dispatcher that cannot verify is worse than one
# that says so; a dispatcher that can do anything is worse than both.
PERMISSION_MODE = os.environ.get("AGENTDESK_WORKER_PERMISSION", "acceptEdits")

POLL_SECONDS = int(os.environ.get("AGENTDESK_WORKER_POLL", "30"))
RUN_TIMEOUT = int(os.environ.get("AGENTDESK_WORKER_TIMEOUT", "3600"))
MAX_CONCURRENT = int(os.environ.get("AGENTDESK_WORKER_CONCURRENCY", "1"))
# After this many failed attempts an item is left alone and reported, rather
# than handed round the fleet for ever. An item that kills every agent that
# touches it needs a human, and the dispatcher's job is to notice that, not to
# keep trying.
MAX_ATTEMPTS = int(os.environ.get("AGENTDESK_WORKER_MAX_ATTEMPTS", "2"))

STOP_FILE = paths.DATA_DIR / "worker.stop"
WORKER_NAME = "work-dispatcher"


def log(msg: str) -> None:
    """One line per event, to stdout and to the same log the app writes.

    Not a per-poll heartbeat: a dispatcher that logs every tick buries the one
    line that says something went wrong.
    """
    line = f"{db.now_iso()} worker: {msg}"
    print(line, flush=True)
    try:
        with open(paths.LOG_PATH, "a", encoding="utf-8") as fh:
            fh.write(line + "\n")
    except OSError:
        pass  # a log we cannot write is not a reason to stop working


def _prompt(item: dict, worker: str) -> str:
    return f"""You are picking up work item #{item['id']} from the AgentDesk Work to Hire queue.

SUBJECT: {item['subject']}

--- the item body, which contains its acceptance test ---
{item.get('last_body') or ''}
--- end of item body ---

You ALREADY HOLD this item: the dispatcher claimed it before starting you. Do
not call claim_work -- a second claim would return claimed:false and that is
expected, not a failure.

Read your standing brief first, and work to it exactly:
  C:\\Users\\palencharj\\.claude\\agents\\app-dev\\CLAUDE.md

The acceptance test in the item body is the bar. Evidence is the command you
ran and its printed output -- not "the code looks right". If you cannot verify
something, say so plainly rather than implying you did.

WHEN YOU FINISH: call the agentdesk MCP tool complete_work with
  thread_id={item['id']}, author="{worker}", and a one-line note saying only
  that it is done. Keep that note to a single line -- the dispatcher takes your
  FULL final message and posts it on the thread as the record of the work, so
  anything you put in the note as well is said twice.
If you cannot finish, do NOT call complete_work -- say what stopped you in your
final message instead, and the dispatcher will release the item for another try.

Your final message is the report, and it is posted on the thread where the next
agent will read it. Write it for that reader: what you changed and where, the
command you ran and its output as evidence, what you could not do or verify,
and anything you noticed that the item did not ask about. Not a summary of your
process and not a status line -- what you did, and what is true now that was
not true before."""


def _claude_exe() -> Optional[str]:
    return shutil.which("claude") or shutil.which("claude.cmd")


def _agent_report(out: str) -> Optional[str]:
    """The agent's own final message, pulled out of `--output-format json`.

    None when there is nothing usable: a crash, a truncated stream, output that
    is not the JSON we asked for. None is deliberately not an error. The item is
    already recorded as done by the database, and a missing summary must not be
    allowed to turn finished work back into a failed run -- that would re-queue
    a job that actually succeeded, which is the one outcome worse than a thin
    report.
    """
    try:
        payload = json.loads(out or "")
    except (TypeError, ValueError):
        return None
    if not isinstance(payload, dict):
        return None
    text = payload.get("result")
    return text.strip() if isinstance(text, str) and text.strip() else None


def _post_report(tid: int, text: str) -> bool:
    """Put the agent's full summary on the work thread. False if not posted.

    complete_work already posts a note, and that note is deliberately ONE LINE.
    This is the summary John asked for: the dispatcher posts it rather than
    trusting the agent to remember, because the agent's last act is the one it
    is most likely to skip when it is nearly out of budget, and a rule that
    depends on the agent's diligence reports success exactly when diligence ran
    out.

    Skipped only when the note and the summary are the same text, which happens
    whenever an agent puts its whole report in the note anyway. Posting it twice
    reads as two reports and makes the thread look like it disagrees with itself.
    """
    conn = db.connect()
    try:
        last = conn.execute(
            "SELECT body FROM messages WHERE thread_id=? ORDER BY id DESC LIMIT 1",
            (tid,)).fetchone()
        if last is not None and (last["body"] or "").strip() == text:
            return False
        db.reply(conn, tid, AGENT, paths.AGENT_KIND, text,
                 meta={"kind": "agent-report", "dispatcher": WORKER_NAME})
        return True
    except Exception as exc:
        # The work is done and recorded; failing to add the summary is worth a
        # log line, not an exception that would skip the rest of this function.
        log(f"#{tid} could not post the agent's report: {exc!r}")
        return False
    finally:
        conn.close()


def _attempts(item: dict) -> int:
    try:
        return int((json.loads(item.get("meta") or "{}") or {}).get("attempts") or 0)
    except (TypeError, ValueError):
        return 0


def _post_release_note(tid: int, reason: str, tail: list) -> None:
    """Record on the thread why the dispatcher gave the item back.

    Posted as the DISPATCHER, not as the agent. The agent did not say this, and
    attributing a machine's account of a failure to the agent that failed puts
    words in its mouth. The difference is carried in both the author name and
    the meta kind, so a reader can tell the two apart without knowing either.
    """
    body = (f"Dispatcher: releasing this item -- {reason}.\n\n"
            "It is back on the queue and another agent may take it.")
    if tail:
        quoted = "\n".join(f"    {line[:300]}" for line in tail)
        body += ("\n\nThe last of what the agent printed before it stopped:\n\n"
                 "```\n" + quoted + "\n```")
    conn = db.connect()
    try:
        db.reply(conn, tid, WORKER_NAME, paths.AGENT_KIND, body,
                 meta={"kind": "dispatcher-release", "reason": reason})
    except Exception as exc:
        log(f"#{tid} could not post the release note: {exc!r}")
    finally:
        conn.close()


def _pick(conn, claimed: set) -> Optional[dict]:
    """The oldest open item that is not already being worked, or None.

    Oldest-first, because a queue everyone can add to but nobody drains from the
    front is a stack, and the item John posted a week ago never gets reached.
    """
    for item in db.list_work(conn, status=paths.STATUS_OPEN, limit=50):
        if item["id"] in claimed:
            continue
        if _attempts(item) >= MAX_ATTEMPTS:
            continue
        return item
    return None


def run_item(item: dict, worker: str) -> None:
    """Claim, spawn, then complete or release. Always one of the two."""
    tid = item["id"]
    conn = db.connect()
    try:
        if not db.claim_task(conn, tid, worker):
            log(f"#{tid} was taken by someone else between picking and claiming; skipping")
            return
    finally:
        conn.close()

    log(f"#{tid} claimed: {item['subject'][:70]}")

    exe = _claude_exe()
    if not exe:
        conn = db.connect()
        db.release_task(conn, tid, worker, note="claude CLI not on PATH")
        conn.close()
        log(f"#{tid} released: the claude CLI is not on PATH")
        return

    cmd = [exe, "-p", _prompt(item, worker),
           "--agent", AGENT,
           "--permission-mode", PERMISSION_MODE,
           "--output-format", "json",
           "--add-dir", str(REPO)]
    try:
        proc = subprocess.Popen(
            cmd, cwd=str(REPO), stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace",
        )
    except OSError as exc:
        conn = db.connect()
        db.release_task(conn, tid, worker, note=f"could not start agent: {exc}")
        conn.close()
        log(f"#{tid} released: could not start the agent: {exc}")
        return

    timed_out = False
    try:
        out, err = proc.communicate(timeout=RUN_TIMEOUT)
    except subprocess.TimeoutExpired:
        timed_out = True
        proc.kill()
        out, err = proc.communicate()

    # The agent's own complete_work is the success signal, not the exit code.
    # An agent that exits 0 having decided it could not do the work must NOT
    # leave the item looking finished -- so ask the database, not the process.
    conn = db.connect()
    try:
        status = conn.execute(
            "SELECT status FROM threads WHERE id=?", (tid,)).fetchone()["status"]
    finally:
        conn.close()

    if status == paths.STATUS_DONE:
        report = _agent_report(out)
        if report:
            where = ("posted on the thread" if _post_report(tid, report)
                     else "already on the thread from complete_work")
            log(f"#{tid} done (agent completed it); summary {where}")
        else:
            log(f"#{tid} done (agent completed it), but its final message could "
                f"not be read, so no summary was posted -- the thread has the "
                f"one-line note instead")
        return

    reason = ("timed out after %ss" % RUN_TIMEOUT if timed_out
              else f"agent exited {proc.returncode} without completing")
    conn = db.connect()
    db.release_task(conn, tid, worker, note=reason)
    conn.close()
    log(f"#{tid} released: {reason}")

    # The tail of the agent's report, so the reason is visible without opening
    # the transcript. Truncated: the full output belongs in a file, not a log
    # line that will be read once.
    tail = (err or out or "").strip().splitlines()[-8:]
    for line in tail:
        log(f"    #{tid} agent said: {line[:200]}")

    # Then say why on the thread. The release reason is already in the item's
    # meta, where nobody working the queue can see it -- so an item that has
    # failed and come back looks identical to one nobody has ever tried, which
    # is the silent stalling that releasing exists to prevent.
    _post_release_note(tid, reason, tail)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="agentdesk-worker",
        description="Dispatch agents at the Work to Hire queue.",
    )
    parser.add_argument("--once", action="store_true",
                        help="one pass: pick at most one item, run it, exit")
    parser.add_argument("--dry-run", action="store_true",
                        help="show what would be picked up, change nothing")
    parser.add_argument("--only", type=int, metavar="ID",
                        help="run this one item and nothing else. For bringing "
                             "a specific item forward, and for testing the "
                             "dispatcher on a bounded task rather than taking "
                             "whatever happens to be oldest.")
    args = parser.parse_args(argv)

    paths.ensure_dirs()
    if args.dry_run:
        conn = db.connect()
        try:
            pending = db.list_work(conn, status=paths.STATUS_OPEN)
        finally:
            conn.close()
        for item in pending:
            print(f"#{item['id']}  attempts={_attempts(item)}  "
                  f"{item['subject'][:80]}")
        print(f"\n{len(pending)} open item(s); would run up to {MAX_CONCURRENT} "
              f"at a time as {WORKER_NAME}")
        return 0

    log(f"dispatcher starting as {WORKER_NAME} (agent={AGENT}, "
        f"mode={PERMISSION_MODE}, concurrency={MAX_CONCURRENT}, "
        f"max_attempts={MAX_ATTEMPTS})")
    log(f"stop file: {STOP_FILE}")

    running: dict[int, threading.Thread] = {}
    while True:
        if STOP_FILE.exists():
            log(f"{STOP_FILE.name} is present - stopping")
            break

        for tid in [t for t, th in running.items() if not th.is_alive()]:
            running.pop(tid)

        if len(running) < MAX_CONCURRENT:
            conn = db.connect()
            try:
                item = _pick(conn, set(running))
                if args.only is not None:
                    # Honour --only strictly: if that item is not open, do not
                    # quietly fall through to something else, because that is
                    # exactly the surprise the flag exists to prevent.
                    item = item if (item and item["id"] == args.only) else None
            finally:
                conn.close()
            if item is not None:
                th = threading.Thread(
                    target=run_item, args=(item, WORKER_NAME), daemon=True)
                th.start()
                running[item["id"]] = th

        if args.once:
            break
        time.sleep(POLL_SECONDS)

    for th in running.values():
        th.join(timeout=RUN_TIMEOUT + 30)
    return 0


if __name__ == "__main__":
    sys.exit(main())
