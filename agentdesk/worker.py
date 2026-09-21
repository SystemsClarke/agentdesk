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
# 'auto' (John's choice, 2026-09-18) is the default: the classifier judges each
# call, so the agent can run the command it just wrote and prove its own work.
# That matters because the agent's brief requires it to VERIFY by running
# things, and the previous default could not.
#
# The other two, and why they are not the default:
#   'acceptEdits'  lets it read, grep and edit but DENIES EVERY BASH CALL in
#                  headless mode, so it cannot run what it wrote and reports
#                  "could not verify" a great deal. Safe, and close to useless
#                  for work whose acceptance test is a command's output.
#   'bypassPermissions' removes the limit and also removes the last thing
#                  standing between an unattended agent and this machine.
#
# ONE HONEST CAVEAT about 'auto': the classifier is a network call, and when it
# is unreachable it denies rather than allowing. It has been unreachable
# repeatedly today. So under this mode an item can fail for a reason that has
# nothing to do with the work -- which is exactly what MAX_ATTEMPTS and the
# release note on the thread exist to make visible instead of silent.
PERMISSION_MODE = os.environ.get("AGENTDESK_WORKER_PERMISSION", "auto")

POLL_SECONDS = int(os.environ.get("AGENTDESK_WORKER_POLL", "30"))
RUN_TIMEOUT = int(os.environ.get("AGENTDESK_WORKER_TIMEOUT", "3600"))
MAX_CONCURRENT = int(os.environ.get("AGENTDESK_WORKER_CONCURRENCY", "1"))
# After this many failed attempts an item is left alone and reported, rather
# than handed round the fleet for ever. An item that kills every agent that
# touches it needs a human, and the dispatcher's job is to notice that, not to
# keep trying.
MAX_ATTEMPTS = int(os.environ.get("AGENTDESK_WORKER_MAX_ATTEMPTS", "2"))

STOP_FILE = paths.WORKER_STOP
WORKER_NAME = "work-dispatcher"

# --- showing the work while it runs --------------------------------------------
#
# The agent is spawned with `--output-format stream-json`, which emits one JSON
# object per line as the run proceeds instead of one blob at the end. That
# format is what makes progress visible at all: with `--output-format json`
# there is nothing on stdout until the agent has already finished, so the only
# thing a watcher can say is "it is still running", which is the thing John
# asked to be able to see past.
#
# THE COST OF THAT CHOICE, and it is a real one: the final report now arrives as
# the `result` event of a stream rather than as the whole of stdout, so
# _result_text below is the only thing standing between this change and a
# dispatcher that silently stops posting agents' write-ups. That is why the
# acceptance script checks the report text, not just the events.
#
# `--verbose` is required by the CLI for stream-json in print mode. It is not
# cosmetic and dropping it fails the run outright, so it is not a flag to tidy
# away later.
STREAM_FORMAT = "stream-json"

# How much of any one line is kept. Long enough for a command or a file path,
# short enough that one pathological line cannot push every other event out of
# the panel.
EVENT_MAX = 400
# The agent's own prose is not what this panel is for -- its steps are -- and an
# agent that narrates continuously would otherwise bury them. One output line
# per this many seconds, at most.
OUTPUT_MIN_SECONDS = 2.0
# How much raw output is kept for the release note when a run fails.
TAIL_LINES = 12

# A tool call in words, because "Read" and "Grep" are the agent's vocabulary and
# not the reader's. The fallback for an unlisted tool is the tool's own name,
# which is honest: a step that reads "Task(...)" is still a step, and inventing
# a verb for a tool we do not know would be a guess printed as a fact.
_TOOL_VERB = {
    "Read": "Reading",
    "Glob": "Looking for",
    "Grep": "Searching for",
    "Edit": "Editing",
    "Write": "Writing",
    "NotebookEdit": "Editing",
    "Bash": "Running",
    "PowerShell": "Running",
    "Task": "Delegating",
    "Agent": "Delegating",
    "WebFetch": "Fetching",
    "WebSearch": "Searching the web",
    "TodoWrite": "Planning",
    "Skill": "Using a skill",
}
# Which field of the tool's input is the interesting one. Anything not here
# contributes no detail and the line is just the verb, which is why the default
# is None rather than a str() of the whole input blob -- a raw JSON dump in the
# panel would push the readable lines off the screen.
_TOOL_DETAIL = {
    "Read": "file_path", "Edit": "file_path", "Write": "file_path",
    "NotebookEdit": "notebook_path",
    "Glob": "pattern", "Grep": "pattern",
    "Bash": "command", "PowerShell": "command",
    "Task": "description", "Agent": "description",
    "WebFetch": "url", "WebSearch": "query", "Skill": "skill",
}

# Set once in main(); carried into the heartbeat so the window can show when a
# dispatcher started, which is the difference between "running" and "wedged".
_STARTED_TS = ""


def write_state(current_item=None) -> None:
    """Tell the window what this dispatcher is doing. Best-effort, never raises.

    A heartbeat file rather than a lock, because the window has to be able to
    show a dispatcher it did not start: John may run one from a shell, and a
    button that reads "Start" while the queue is already being drained is worse
    than having no button. The pid is in here so the reader can check the
    process is still alive -- a crashed worker leaves this file behind, and a
    stale heartbeat that reads as "running" is the one failure this must not
    have.
    """
    try:
        paths.DATA_DIR.mkdir(parents=True, exist_ok=True)
        paths.WORKER_STATE.write_text(json.dumps({
            "pid": os.getpid(),
            "started_ts": _STARTED_TS,
            "item": current_item,
            "agent": AGENT,
            "mode": PERMISSION_MODE,
        }), encoding="utf-8")
    except OSError:
        pass  # the queue does not stop because the window cannot see it


def clear_state() -> None:
    """Remove the heartbeat as this worker exits.

    Deleted rather than marked stopped, so the only way that file exists is a
    dispatcher that either is running or died without cleaning up -- and the
    pid check settles which.
    """
    try:
        paths.WORKER_STATE.unlink()
    except OSError:
        pass


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


def _no_window() -> dict:
    """Extra Popen kwargs that stop the spawned `claude` CLI flashing a console.

    The dispatcher runs under `pythonw.exe` (no console of its own), so a
    console child spawned with no flag gets Windows allocating it a brand-new
    window that steals focus -- once per work item drained off the queue.
    Same fix as `crew._no_window()`; kept local here rather than imported so
    this module has no dependency on `crew`.
    """
    if os.name != "nt":
        return {}
    si = subprocess.STARTUPINFO()
    si.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    si.wShowWindow = subprocess.SW_HIDE
    return {"creationflags": subprocess.CREATE_NO_WINDOW, "startupinfo": si}


def _agent_cmd(exe: str, item: dict, worker: str) -> list:
    """The command line the agent runs under.

    A named function rather than a list built inline at the call site, so the
    acceptance script can run the REAL CLI under exactly these flags instead of
    under a copy of them. The stream format is an assumption about another
    program's behaviour, and a test that re-states the flag list is testing its
    own copy of the assumption rather than the one that ships.
    """
    return [exe, "-p", _prompt(item, worker),
            "--agent", AGENT,
            "--permission-mode", PERMISSION_MODE,
            "--output-format", STREAM_FORMAT, "--verbose",
            "--add-dir", str(REPO)]


def _one_line(text: str, limit: int = EVENT_MAX) -> str:
    """Collapse text to a single line, truncated. The panel is a list of lines."""
    flat = " ".join((text or "").split())
    return flat[:limit] + ("..." if len(flat) > limit else "")


def _tool_line(name: str, args: dict) -> str:
    """One line in words for a tool call: what the agent is doing right now.

    Deliberately not the tool's raw input. A `Read` is interesting for its
    path and a `Bash` for its command, and everything else in the input is
    noise that would push the next line off the panel.
    """
    verb = _TOOL_VERB.get(name)
    key = _TOOL_DETAIL.get(name)
    detail = ""
    if key and isinstance(args, dict):
        value = args.get(key)
        if isinstance(value, str):
            detail = value
    if verb is None:
        # An unknown tool, so nothing is claimed about it beyond its name.
        return _one_line(f"{name}: {detail}" if detail else name)
    return _one_line(f"{verb} {detail}".strip() if detail else verb)


def _result_text(event: dict) -> Optional[str]:
    """The agent's own final message, out of the stream's `result` event.

    None when there is nothing usable: a crash, a truncated stream, a run killed
    at the timeout. None is deliberately not an error. The item is recorded as
    done by the database, not by this function, and a missing summary must not
    be allowed to turn finished work back into a failed run -- that would
    re-queue a job that actually succeeded, which is the one outcome worse than
    a thin report.
    """
    text = event.get("result")
    return text.strip() if isinstance(text, str) and text.strip() else None


class _RunRecorder:
    """Reads the spawned agent's stream and writes work_events as it lands.

    ONE THREAD PER PIPE, and that is not tidiness. A subprocess writing to a
    pipe nobody drains fills the pipe buffer and blocks -- so reading stdout on
    the main thread while stderr filled would present to John as an agent that
    hung, which is exactly the state this panel exists to let him tell apart
    from a slow one. Both are drained here, concurrently, while the main thread
    waits on the process.

    The connection lives in the reader thread and nowhere else: a sqlite3
    connection is not safe to use from a thread it was not opened in, and
    opening one per event would be a connection per tool call for no gain. It
    is WAL, so this writer does not block the window's reads.
    """

    def __init__(self, proc: subprocess.Popen, tid: int) -> None:
        self.proc = proc
        self.tid = tid
        # The agent's final message, once the stream carries it. None until
        # then, and None for ever if the run died before saying anything --
        # which the caller reads as "the thread keeps the one-line note".
        self.report: Optional[str] = None
        self.err_lines: list[str] = []
        self.out_lines: list[str] = []
        self._last_output = 0.0
        self._threads: list[threading.Thread] = []

    # --- lifecycle ------------------------------------------------------------

    def start(self) -> None:
        for target in (self._read_stdout, self._read_stderr):
            th = threading.Thread(target=target, daemon=True)
            th.start()
            self._threads.append(th)

    def join(self) -> None:
        """Wait for both readers. They end with the pipes, so this is bounded.

        The timeout is a backstop for a child that exited without closing its
        pipes; a reader still alive after it is abandoned rather than allowed
        to block the dispatcher for the rest of the run.
        """
        for th in self._threads:
            th.join(timeout=30)

    def tail(self) -> list[str]:
        """The last lines to quote in a release note. stderr first.

        stderr wins because when a headless run fails the reason is nearly
        always there -- an argument the CLI rejected, an auth error -- and
        stdout is the stream we asked for, so it is mostly empty when that
        happens.
        """
        source = self.err_lines or self.out_lines
        return [line[:300] for line in source[-TAIL_LINES:]]

    # --- the two readers ------------------------------------------------------

    def _read_stdout(self) -> None:
        conn = None
        try:
            conn = db.connect()
            for line in self.proc.stdout or ():
                line = line.strip()
                if not line:
                    continue
                self.out_lines.append(line)
                del self.out_lines[:-TAIL_LINES]
                self._consume(line, conn)
        except Exception as exc:
            # The agent is still running and the item is still its own; losing
            # the commentary must not also lose the run.
            log(f"#{self.tid} reading the agent's output failed: {exc!r}")
        finally:
            if conn is not None:
                conn.close()

    def _read_stderr(self) -> None:
        try:
            for line in self.proc.stderr or ():
                line = line.strip()
                if line:
                    self.err_lines.append(line)
                    del self.err_lines[:-TAIL_LINES]
        except Exception:
            pass  # a pipe we cannot read is not a reason to stop the run

    # --- turning the stream into events ---------------------------------------

    def _consume(self, line: str, conn) -> None:
        try:
            event = json.loads(line)
        except ValueError:
            # Not the JSON we asked for -- a banner, a warning the CLI wrote to
            # stdout. It is already kept in out_lines for a failure note.
            return
        if not isinstance(event, dict):
            return
        etype = event.get("type")
        if etype == "result":
            self.report = _result_text(event) or self.report
            if event.get("is_error"):
                # The agent's own account of why it stopped, which is better
                # than the exit code alone and is what the tab shows.
                self._record(conn, db.WORK_ERROR,
                             _one_line(str(event.get("result") or
                                           "the agent reported an error")))
            return
        if etype == "assistant":
            message = event.get("message")
            blocks = message.get("content") if isinstance(message, dict) else None
            for block in blocks or []:
                self._block(conn, block)

    def _block(self, conn, block) -> None:
        if not isinstance(block, dict):
            return
        btype = block.get("type")
        if btype == "tool_use":
            self._record(conn, db.WORK_STEP,
                         _tool_line(str(block.get("name") or "tool"),
                                    block.get("input") or {}))
        elif btype == "text":
            text = (block.get("text") or "").strip()
            if text:
                self._maybe_output(conn, text)

    def _maybe_output(self, conn, text: str) -> None:
        """Record the agent's prose, throttled. See OUTPUT_MIN_SECONDS."""
        now = time.monotonic()
        if now - self._last_output < OUTPUT_MIN_SECONDS:
            return
        self._last_output = now
        self._record(conn, db.WORK_OUTPUT, _one_line(text))

    def _record(self, conn, kind: str, body: str) -> None:
        try:
            db.add_work_event(conn, self.tid, kind, body)
        except Exception as exc:
            # Progress reporting is a view of the work, never a precondition
            # for it. A locked board must not fail the item.
            log(f"#{self.tid} could not record a work event: {exc!r}")


def _note(tid: int, kind: str, body: str) -> None:
    """One event on its own short-lived connection.

    For the paths that have no reader thread -- the claim, a missing CLI, the
    release, the completion. Without these the panel would show an item that
    stopped for a reason nothing on it states, which is the failure mode this
    whole feature is meant to remove.
    """
    conn = db.connect()
    try:
        db.add_work_event(conn, tid, kind, _one_line(body))
    except Exception as exc:
        log(f"#{tid} could not record a work event: {exc!r}")
    finally:
        conn.close()


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


def _pick(conn, claimed: set, only: Optional[int] = None) -> Optional[dict]:
    """The oldest open item that is not already being worked, or None.

    Oldest-first, because a queue everyone can add to but nobody drains from the
    front is a stack, and the item John posted a week ago never gets reached.

    `only` filters INSIDE the search for that one id. An earlier version picked
    the oldest item first and then discarded it unless it happened to be the one
    asked for, which meant `--only 24` did nothing at all unless 24 was also the
    oldest open item -- and it did that silently, exiting 0. That is precisely
    the surprise the flag exists to prevent, so the filter moved here, where it
    selects rather than rejects.
    """
    # reversed(): db.list_work returns newest-first, so iterating it directly
    # gave the NEWEST open item -- the exact opposite of what the paragraph
    # above promises. It read correctly and did the wrong thing, which is why
    # the reasoning is written down and not just the code.
    pending = db.list_work(conn, status=paths.STATUS_OPEN, limit=50)
    for item in reversed(pending):
        if only is not None and item["id"] != only:
            continue
        # An item reserved for `anyone` is skipped by the SWEEP, not forbidden
        # to the worker. --only names one item on purpose, which is the same
        # deliberate act as claim_work, so it is allowed through; the
        # reservation exists to stop the automatic pass from eating a job that
        # was meant to be left for an agent to choose.
        if only is None and not db.open_to_dispatcher(item):
            continue
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
    write_state(tid)
    # Recorded at the claim rather than at the spawn, so the panel's clock
    # starts when the item stopped being available -- the gap between the two
    # is exactly the "started, but nothing is happening yet" window that a
    # reader would otherwise have to guess at.
    _note(tid, db.WORK_START,
          f"{AGENT} started ({PERMISSION_MODE}): {item['subject']}")

    exe = _claude_exe()
    if not exe:
        conn = db.connect()
        db.release_task(conn, tid, worker, note="claude CLI not on PATH")
        conn.close()
        _note(tid, db.WORK_ERROR, "the claude CLI is not on PATH")
        log(f"#{tid} released: the claude CLI is not on PATH")
        return

    cmd = _agent_cmd(exe, item, worker)
    try:
        proc = subprocess.Popen(
            cmd, cwd=str(REPO),
            # stdin is closed rather than inherited. `claude -p` reads stdin
            # when it is a pipe, and the dispatcher's own stdin is not a pipe
            # it controls -- as a spawned child it inherited whatever the
            # window's shell had. The CLI then waits three seconds for input
            # that will never come and warns about it on every single run.
            # Closing it says "there is none" up front.
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace",
            **_no_window(),
        )
    except OSError as exc:
        conn = db.connect()
        db.release_task(conn, tid, worker, note=f"could not start agent: {exc}")
        conn.close()
        _note(tid, db.WORK_ERROR, f"could not start the agent: {exc}")
        log(f"#{tid} released: could not start the agent: {exc}")
        return

    # From here the agent is running, and this is what shows what it is doing.
    # The recorder drains both pipes while this thread waits, so the child can
    # never block on a full pipe buffer.
    recorder = _RunRecorder(proc, tid)
    recorder.start()

    timed_out = False
    try:
        proc.wait(timeout=RUN_TIMEOUT)
    except subprocess.TimeoutExpired:
        timed_out = True
        proc.kill()
        proc.wait()
    recorder.join()

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
        # The report is what makes the item readable after the fact: without
        # this it is a title and a "done", and the agent's actual account of
        # the work is only in a log nobody opens.
        report = recorder.report
        if report:
            where = ("posted on the thread" if _post_report(tid, report)
                     else "already on the thread from complete_work")
            _note(tid, db.WORK_DONE, f"finished; summary {where}")
            log(f"#{tid} done (agent completed it); summary {where}")
        else:
            _note(tid, db.WORK_DONE,
                  "finished, but its final message could not be read, so the "
                  "thread has only the one-line note")
            log(f"#{tid} done (agent completed it), but its final message could "
                f"not be read, so no summary was posted -- the thread has the "
                f"one-line note instead")
        return

    reason = ("timed out after %ss" % RUN_TIMEOUT if timed_out
              else f"agent exited {proc.returncode} without completing")
    conn = db.connect()
    db.release_task(conn, tid, worker, note=reason)
    conn.close()
    # On the item as well as in the log. An item that failed and came back is
    # otherwise indistinguishable on the queue from one nobody ever tried.
    _note(tid, db.WORK_ERROR, reason)
    log(f"#{tid} released: {reason}")

    # The tail of what the agent said, so the reason is visible without opening
    # the transcript. Truncated: the full output belongs in a file, not a log
    # line that will be read once.
    tail = recorder.tail()
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
    # Create the schema before reading it. The app and the MCP server both do
    # this for the same reason (mcp_server.main says it best: the server should
    # be the thing that creates the database rather than the thing that crashes
    # on it), and the worker was the one entry point that did not -- so on a
    # board that did not exist yet it died with "no such table: threads". That
    # never showed up on the real board, where the window had always made the
    # tables first, which is exactly how a bug like this stays hidden.
    conn = db.connect()
    try:
        db.init_db(conn)
    finally:
        conn.close()

    if args.dry_run:
        conn = db.connect()
        try:
            pending = db.list_work(conn, status=paths.STATUS_OPEN)
        finally:
            conn.close()
        for item in pending:
            # Say which ones this pass would leave alone and why. A dry run
            # that listed a reserved item identically to one it was about to
            # take would be the one place the reservation is invisible, which
            # is the opposite of what a preview is for.
            reserved = "" if db.open_to_dispatcher(item) else "  [reserved: anyone]"
            print(f"#{item['id']}  attempts={_attempts(item)}  "
                  f"{item['subject'][:80]}{reserved}")
        n_auto = sum(1 for i in pending if db.open_to_dispatcher(i))
        print(f"\n{len(pending)} open item(s), {n_auto} this pass would take; "
              f"would run up to {MAX_CONCURRENT} at a time as {WORKER_NAME}")
        return 0

    log(f"dispatcher starting as {WORKER_NAME} (agent={AGENT}, "
        f"mode={PERMISSION_MODE}, concurrency={MAX_CONCURRENT}, "
        f"max_attempts={MAX_ATTEMPTS})")
    log(f"stop file: {STOP_FILE}")

    global _STARTED_TS
    _STARTED_TS = db.now_iso()
    # A stale stop file from the last run would stop this one immediately, which
    # reads as "the button does nothing". Cleared here, at the moment we start.
    try:
        STOP_FILE.unlink()
    except OSError:
        pass
    write_state(None)

    running: dict[int, threading.Thread] = {}
    while True:
        if STOP_FILE.exists():
            log(f"{STOP_FILE.name} is present - stopping")
            break

        for tid in [t for t, th in running.items() if not th.is_alive()]:
            running.pop(tid)
            write_state(None)  # idle again, but still running

        if len(running) < MAX_CONCURRENT:
            conn = db.connect()
            try:
                item = _pick(conn, set(running), args.only)
            finally:
                conn.close()
            if item is not None:
                th = threading.Thread(
                    target=run_item, args=(item, WORKER_NAME), daemon=True)
                th.start()
                running[item["id"]] = th
            elif args.only is not None and not running:
                # Say it out loud. A named item that is claimed, capped or not
                # open is a normal thing to find, but exiting 0 in silence makes
                # it indistinguishable from having run the item, which is how
                # the earlier version of this flag wasted a whole run.
                log(f"#{args.only} is not open, is already claimed, or has hit "
                    f"the attempt cap - nothing to do")
                break

        # A named item is one pass by definition: "run this one item and
        # nothing else" cannot mean "and then go on to whatever else is oldest".
        if args.once or args.only is not None:
            break
        time.sleep(POLL_SECONDS)

    for th in running.values():
        th.join(timeout=RUN_TIMEOUT + 30)
    clear_state()
    log("dispatcher stopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
