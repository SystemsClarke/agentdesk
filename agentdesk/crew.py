"""The crew: one coordinator and three long-lived agents.

Replaces the spawn-per-item loop in `worker.py`. John's ask, 2026-09-18:
"I don't want one work agent I want 3 that all talk and communicate", "think
of this kinda like ultracode in claude code but cap it at 3 long lived agents",
and "one agent that goes through the board and spins up agent to do work, you
know act as coordinator". Design: `crew-design.md`.

WHAT CHANGED, AND WHY IT HAD TO

`worker.py` spawns `claude -p` once per item and throws the process away. Three
consequences killed it:

1. NO MEMORY. Item N+1 knows nothing item N learned.

2. FAILURES WERE INVISIBLE. `worker.py` read `tail = (err or out or "")`, and
   stderr always carries the harmless `[claude-code:unrecognized_model]`
   warning -- so `out` was never read, and the agent's real error (which lives
   in the stdout JSON's `result`) was discarded on EVERY failure. That is why
   every release note on the board was byte-identical and why the cause stayed
   invisible all day. THIS FILE NEVER DOES THAT: `failure_detail` reads stdout
   first and treats stderr as supporting detail.

3. THE CAP FROZE THE QUEUE SILENTLY. At `attempts >= MAX_ATTEMPTS` the old
   `_pick` skipped an item with no log line and nothing on the thread. After the
   2026-09-18 restart all twelve open items sat at the cap and the dispatcher
   ran, heartbeating, claiming nothing. A blocked item is now LOUD.

LOAD-BEARING DECISIONS

- THE COORDINATOR ROUTES AND NEVER WORKS. It assigns a role and stops. If it
  started editing, the board would have two accounts of every task and nobody
  could tell which one was acting.
- LONG-LIVED MEANS A RESUMED SESSION. `claude -p` is one-shot by design, so
  continuity comes from `--resume <session_id>`: the worker stores the id and
  the next item resumes the same conversation. A worker that resumes forever
  will eventually blow its window, so the role keeps a small memory file and
  starts a fresh session past RESET_AFTER items -- losing cost, not knowledge.
- THE BOARD IS THE MESSAGE BUS. The agents already hold the agentdesk MCP
  tools. No second transport, no second schema.

Run it in the foreground: `python -m agentdesk.crew`. The window's Start/Stop
button drives this the same way it drove the dispatcher -- the heartbeat file
and the stop file are the same ones, deliberately, so replacing the engine did
not require replacing the button.
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

import re

try:
    from agentdesk import db, paths, providers, roles
except ImportError:  # pragma: no cover - depends on how it was launched
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from agentdesk import db, paths, providers, roles

REPO = Path(__file__).resolve().parent.parent

APP_NAME = "agentdesk-crew"

STOP_FILE = paths.WORKER_STOP
STATE_FILE = paths.WORKER_STATE

PERMISSION_MODE = os.environ.get("AGENTDESK_CREW_PERMISSION", "auto")
POLL_SECONDS = int(os.environ.get("AGENTDESK_CREW_POLL", "30"))
RUN_TIMEOUT = int(os.environ.get("AGENTDESK_CREW_TIMEOUT", "3600"))
MAX_CONCURRENT = int(os.environ.get("AGENTDESK_CREW_CONCURRENCY", "3"))
MAX_ATTEMPTS = int(os.environ.get("AGENTDESK_CREW_MAX_ATTEMPTS", "3"))
#: Items a role may run before its session is replaced by a fresh one.
RESET_AFTER = int(os.environ.get("AGENTDESK_CREW_RESET_AFTER", "10"))

#: Which provider profile every crew agent runs on. None = the profile default,
#: which is the Claude subscription. Set AGENTDESK_CREW_PROVIDER=local to run
#: the crew on Ollama, or =deepseek for Fireworks; the names come from
#: ~/.claude/providers.json. Read agentdesk/providers.py before changing this:
#: the crew used to inherit the parent session's provider silently, which is
#: what made every crew run fail with unrecognized_model.
CREW_PROVIDER = os.environ.get("AGENTDESK_CREW_PROVIDER", "").strip() or None

# --- one provider for the whole crew -------------------------------------------
#
# The router decides, the workers follow. Without this the two would drift: the
# router would fail over to Fireworks or Ollama and keep routing there while
# every worker independently paid a dead first attempt against a subscription
# that is down. Worse, a crew could end up with its coordinator on one backend
# and its agents on another, which makes any question about "what model wrote
# this" unanswerable.
#
# So the choice is made ONCE, by observation rather than by assumption, and
# every later spawn puts it first. AGENTDESK_CREW_PROVIDER, when set, still
# wins: an explicit instruction is not something to discover.
_ACTIVE_PROVIDER: Optional[str] = None
_ACTIVE_LOCK = threading.Lock()


def _note_working(name: str) -> None:
    """Record that `name` actually answered. Ordering for every later spawn."""
    global _ACTIVE_PROVIDER
    with _ACTIVE_LOCK:
        if _ACTIVE_PROVIDER != name:
            log(f"provider: this crew is running on {name}")
            _ACTIVE_PROVIDER = name


def _active_provider() -> Optional[str]:
    """The provider observed working, or None before anything has answered."""
    with _ACTIVE_LOCK:
        return _ACTIVE_PROVIDER


def _provider_envs() -> list:
    """(name, env) pairs in try order, with the observed provider first."""
    return providers.env_chain(CREW_PROVIDER or _active_provider())

SESSIONS_DIR = paths.DATA_DIR / "sessions"
MEMORY_DIR = paths.DATA_DIR / "crew-memory"

_STARTED_TS = ""

#: role name -> the live `claude` process for that role. Stop has to be able to
#: reach these: without it, pressing Stop only sets a flag, the main loop then
#: waits out an in-flight run (up to RUN_TIMEOUT, an hour), and the button looks
#: broken and refuses to restart for as long as that lasts. Killing the child is
#: clean rather than brutal -- communicate() returns on the kill, the run is
#: treated as a failure, and the worker releases the item back to the queue.
_CHILDREN: dict = {}
_CHILDREN_LOCK = threading.Lock()


def _kill_children() -> int:
    """Kill every in-flight agent run. Returns how many were killed."""
    with _CHILDREN_LOCK:
        procs = list(_CHILDREN.values())
    killed = 0
    for proc in procs:
        try:
            proc.kill()
            killed += 1
        except OSError:
            pass
    return killed



def log(msg: str) -> None:
    """One line per event, to stdout and the app's log. Never raises.

    BOTH halves are guarded, and the stdout half is not a formality. Agent
    output routinely carries characters a Windows console cannot encode -- the
    harmless `unrecognized_model` warning alone begins with U+26A0, and it is
    present on successful runs too. An unguarded `print` raises
    UnicodeEncodeError on those, which turns "log that a run failed" into "die
    while logging that a run failed": the same undebuggable shape this rewrite
    exists to remove. Found the hard way, by doing exactly that.
    """
    line = f"{db.now_iso()} crew: {msg}"
    try:
        print(line, flush=True)
    except (UnicodeEncodeError, OSError, ValueError):
        # A console that cannot take the character still gets the line, with
        # the offending characters replaced rather than the whole fact lost.
        try:
            sys.stdout.write(line.encode("ascii", "replace").decode("ascii") + "\n")
            sys.stdout.flush()
        except Exception:
            pass
    try:
        with open(paths.LOG_PATH, "a", encoding="utf-8") as fh:
            fh.write(line + "\n")
    except OSError:
        pass


# --- heartbeat -----------------------------------------------------------------


def write_state(active: dict) -> None:
    """Tell the window what the crew is doing. Best-effort, never raises.

    `active` maps role name -> work item id currently held, so the window can
    show three agents and what each is on rather than one opaque "running".
    """
    try:
        paths.DATA_DIR.mkdir(parents=True, exist_ok=True)
        STATE_FILE.write_text(json.dumps({
            "pid": os.getpid(),
            "started_ts": _STARTED_TS,
            "item": next(iter(active.values()), None),
            "agent": APP_NAME,
            "mode": PERMISSION_MODE,
            "roles": active,
        }), encoding="utf-8")
    except OSError:
        pass


def clear_state() -> None:
    try:
        STATE_FILE.unlink()
    except OSError:
        pass


# --- sessions and memory -------------------------------------------------------


def _claude_exe() -> Optional[str]:
    return shutil.which("claude") or shutil.which("claude.cmd")


def _no_window() -> dict:
    """Extra Popen kwargs that stop a spawned child flashing a console window.

    WHY THIS EXISTS, and why it is not paranoia. The crew normally runs under
    `pythonw.exe`, which has NO console. When such a process spawns a console
    application without CREATE_NO_WINDOW, Windows does not share a console --
    it allocates the child a brand new one, which is a real window that appears
    and takes focus. Every agent run and every router call is a spawn, and the
    router runs once per item, so without this the crew throws a window in the
    user's face several times a minute and interrupts whatever they are typing.

    STARTUPINFO/SW_HIDE is belt-and-braces on top of the flag: CREATE_NO_WINDOW
    prevents the console being created, and SW_HIDE covers the case where a
    wrapper (a .cmd shim, a node launcher) creates its own anyway.
    """
    if os.name != "nt":
        return {}
    si = subprocess.STARTUPINFO()
    si.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    si.wShowWindow = subprocess.SW_HIDE
    return {"creationflags": subprocess.CREATE_NO_WINDOW, "startupinfo": si}


def _session_path(role: str) -> Path:
    return SESSIONS_DIR / f"{role}.json"


def load_session(role: str) -> dict:
    """{'id': str|None, 'items': int} for this role. A missing file is normal."""
    try:
        data = json.loads(_session_path(role).read_text(encoding="utf-8"))
        if isinstance(data, dict):
            return {"id": data.get("id"), "items": int(data.get("items") or 0)}
    except (OSError, ValueError, TypeError):
        pass
    return {"id": None, "items": 0}


def save_session(role: str, sid: Optional[str], items: int) -> None:
    try:
        SESSIONS_DIR.mkdir(parents=True, exist_ok=True)
        _session_path(role).write_text(
            json.dumps({"id": sid, "items": items}), encoding="utf-8")
    except OSError as exc:
        log(f"{role}: could not save the session ({exc!r}); the next item will "
            f"start a fresh conversation")


def memory_path(role: str) -> Path:
    return MEMORY_DIR / f"{role}.md"


def _memory_excerpt(role: str, limit: int = 4000) -> str:
    """What this role carries across a session reset. Empty when there is none."""
    try:
        text = memory_path(role).read_text(encoding="utf-8").strip()
    except OSError:
        return ""
    if not text:
        return ""
    # The tail, not the head: the most recent note is the most likely relevant.
    return text[-limit:]


# --- Phoenix: the explicit handoff -----------------------------------------
#
# `memory.md` (above) is the running notes file: appended to, spliced in as a
# tail excerpt, meant to be read by the NEXT SESSION OF THE SAME ROLE. It was
# never meant to answer "what was this role mid-doing" for a human or a
# stranger session skimming it cold -- that answer is buried wherever the
# last append happened to stop.
#
# handoff.md is the artifact John asked Phoenix to produce: explicit and
# discoverable, REWRITTEN (not appended) each reset, so it always holds
# exactly one thing -- the current state of the world for this role, written
# to be read standalone. memory.md keeps doing its job for the role itself;
# this is the one for everyone else.


def handoff_path(role: str) -> Path:
    return MEMORY_DIR / f"{role}-handoff.md"


def _vault_thread_path(role: str) -> Path:
    """Where this role's vault-mirror wiki thread id is remembered.

    One thread per role, reused across every reset -- see db.set_opening_body
    and vault.mirror_thread's docstring on why reuse (not a fresh thread per
    reset) is what keeps the note singular instead of colliding on filename.
    """
    return MEMORY_DIR / f"{role}-vault-thread.json"


def _load_vault_thread(role: str) -> Optional[int]:
    try:
        data = json.loads(_vault_thread_path(role).read_text(encoding="utf-8"))
        tid = data.get("thread_id")
        return int(tid) if tid is not None else None
    except (OSError, ValueError, TypeError, KeyError):
        return None


def _save_vault_thread(role: str, thread_id: int) -> None:
    try:
        MEMORY_DIR.mkdir(parents=True, exist_ok=True)
        _vault_thread_path(role).write_text(
            json.dumps({"thread_id": thread_id}), encoding="utf-8")
    except OSError as exc:
        log(f"{role}: could not remember the vault thread id ({exc!r}); the "
            f"next sync will start a new thread instead of updating this one")


_VAULT_TAG = "tooling"
_VAULT_MOC = "Workstation MOC"


def sync_handoff_to_vault(role: str, handoff_text: str) -> Optional[dict]:
    """Write this role's handoff into the memory vault, via the board.

    Deliberately does NOT touch vault files directly. `agentdesk/vault.py`
    already has a tested, protocol-compliant path from a board post to a vault
    note (frontmatter shape, word caps, wikilink count, MOC registration, the
    secret-shape check) -- writing a second implementation of that protocol
    here would be exactly the drift vault.py's own docstring warns about
    ("two-way sync... rejected"). So this POSTS a wiki entry shaped to pass
    that gate, then calls vault.mirror_thread on it. If it fails the gate, it
    is PARKED under vault/agentdesk/parked/ with the reasons -- same as any
    other wiki post that does not qualify -- and this function reports that
    rather than raising: a vault sync failing must never take down the role's
    actual work.

    The SAME thread is reused every reset (see _load_vault_thread /
    db.set_opening_body): a fresh thread every reset would give the note a
    colliding filename with the one from last time, which vault.py refuses to
    overwrite (existing_owner != thread_id) rather than silently orphan.
    """
    try:
        from . import vault  # local import: only the vault-sync path needs it
        today = __import__("datetime").datetime.now().astimezone().date()
        subject = f"AgentDesk crew handoff {role}"
        summary = f"The {role} role's latest Phoenix handoff, synced on reset."
        # Body must satisfy vault.py's protocol: >=20 words, <=420 (type=project
        # is accretive so the cap is generous), >=2 wikilinks, a Related: line,
        # no H1. The handoff text itself is free-form prose from the agent, so
        # it is wrapped rather than trusted to satisfy the shape on its own.
        body = (
            f"---\ntype: project\ntags: [{_VAULT_TAG}]\nsummary: {summary}\n"
            f"updated: {today.isoformat()}\n---\n"
            f"Phoenix handoff for the AgentDesk crew's **{role}** role, "
            f"synced automatically on session reset (crew.py RESET_AFTER).\n\n"
            f"{handoff_text.strip()}\n\n"
            f"Related: [[AgentDesk is the agent message board]] · "
            f"[[Reaching the board without the agentdesk tools]]\n"
        )

        conn = db.connect()
        try:
            tid = _load_vault_thread(role)
            if tid is not None:
                # Confirm the thread still exists before trusting it -- a
                # hand-pruned board should not crash the sync, just start a
                # fresh thread the way a first-ever sync would.
                row = conn.execute(
                    "SELECT id FROM threads WHERE id=?", (tid,)).fetchone()
                if row is None:
                    tid = None
            if tid is None:
                tid = db.start_thread(conn, "wiki", subject, APP_NAME,
                                      paths.AGENT_KIND, body)
                _save_vault_thread(role, tid)
            else:
                db.set_opening_body(conn, tid, body)
            result = vault.mirror_thread(conn, tid)
        finally:
            conn.close()
        if result.get("status") == "mirrored":
            log(f"{role}: handoff synced to the vault -> {result.get('note')}")
        else:
            log(f"{role}: handoff NOT mirrored to the vault "
                f"({result.get('status')}): {result.get('reasons') or result.get('reason')}")
        return result
    except Exception as exc:
        # Never let a vault problem take the role down; this is best-effort
        # continuity, not the item's own completion.
        log(f"{role}: vault sync failed: {exc!r}")
        return None


def _prompt(role: roles.Role, item: dict, memory: str) -> str:
    carried = ""
    if memory:
        carried = (
            "\n--- your own notes from earlier work, carried across sessions ---\n"
            f"{memory}\n--- end of your notes ---\n")
    return f"""{role.brief}
{carried}
You are working as `{role.name}` on the AgentDesk board, holding work item
#{item['id']}.

SUBJECT: {item['subject']}

--- the item body, which contains its acceptance test ---
{item.get('last_body') or ''}
--- end of item body ---

You ALREADY HOLD this item: the coordinator claimed it for you before starting
you. Do not call claim_work -- a second claim returns claimed:false and that is
expected, not a failure.

Before you finish, write anything worth carrying into your next session to:

    {memory_path(role.name)}

Append; do not rewrite. That file is what survives a session reset.

Also keep this file current -- REWRITE it whole, do not append:

    {handoff_path(role.name)}

That is your handoff: a snapshot a fresh session (or a human) can read
standalone and understand what you, {role.name}, are mid-doing right now --
what you own, what this item changed, what is still open, what the next
session should do first. Unlike the notes file above, it is not a log; it is
always "where things stand as of right now," so rewrite the whole file rather
than adding to it. It becomes stale the moment you finish this item, so update
it every time, not just near a reset.

Post to the board through the agentdesk tools you already have, as
`{role.name}`. Use them: if you need something from another agent, ask on the
board rather than assuming. The other two of builder / verifier / researcher are
working this same board right now.

Your final message IS the report. It is posted on the work item where the next
agent reads it, so write it for that reader:
  - what you changed and where
  - the exact command you ran and its printed output, as evidence
  - what you could NOT do or verify, said plainly
  - anything you noticed that the item did not ask about
Not a summary of your process. What you did, and what is true now that was not
true before."""


# --- the agent process ---------------------------------------------------------


class RunResult:
    """What one `claude -p` run produced, with nothing thrown away."""

    __slots__ = ("code", "out", "err", "session_id", "result", "is_error",
                 "denials", "turns", "subtype")

    def __init__(self, code, out, err):
        self.code, self.out, self.err = code, out, err
        self.session_id = self.result = self.subtype = None
        self.is_error = None
        self.denials = []
        self.turns = None
        self._parse()

    def _parse(self) -> None:
        try:
            payload = json.loads(self.out or "")
        except (TypeError, ValueError):
            return
        if not isinstance(payload, dict):
            return
        sid = payload.get("session_id")
        self.session_id = sid if isinstance(sid, str) and sid else None
        text = payload.get("result")
        self.result = text.strip() if isinstance(text, str) and text.strip() else None
        self.is_error = payload.get("is_error")
        self.denials = payload.get("permission_denials") or []
        self.turns = payload.get("num_turns")
        self.subtype = payload.get("subtype")

    @property
    def ok(self) -> bool:
        return self.code == 0 and not self.is_error


def run_claude(role: roles.Role, prompt: str, resume: Optional[str],
               timeout: int = RUN_TIMEOUT) -> RunResult:
    """One agent run. Returns a RunResult; a mere failure does not raise."""
    exe = _claude_exe()
    if not exe:
        raise FileNotFoundError("claude CLI not on PATH")

    cmd = [exe, "-p", prompt,
           "--permission-mode", role.permission_mode or PERMISSION_MODE,
           "--output-format", "json",
           "--add-dir", str(REPO)]
    if role.model:
        cmd += ["--model", role.model]
    if resume:
        cmd += ["--resume", resume]

    # The role is stamped into the environment, not left to the prompt. The
    # session's board name is derived from what it inherits (identity.py), and
    # a role must keep that name across its session resets: complete_work
    # matches on it, so an item claimed as `builder` has to be finished by
    # something that still resolves to `builder`. A model that forgets the
    # `author` argument then still posts as its role, and a rotated session
    # still owns the work it was given.
    # NOT dict(os.environ). Inheriting the parent's environment is what pointed
    # every crew agent at the parent session's provider while still asking for
    # an Anthropic model, so the run died in seconds with zero tokens and read
    # as a bad answer. providers.env_for scrubs the override first.
    # FAILOVER. Try each configured provider in turn, ending at `local`, so a
    # dead subscription is a slow run rather than a lost one.
    #
    # The retry trigger is deliberately narrow: a run that produced NO usable
    # output at all. A run that produced an ANSWER is never retried, even an
    # unhappy one -- the agent answered, and putting the same question to a
    # different model would quietly replace its report with a second opinion
    # that the dispatcher then records as the result.
    res: Optional[RunResult] = None
    tried: list[str] = []
    for name, base_env in _provider_envs():
        child_env = dict(base_env)
        child_env["AGENTDESK_AUTHOR"] = role.name

        proc = subprocess.Popen(
            cmd, cwd=str(REPO), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace", env=child_env,
            **_no_window())
        # Registered so a Stop can reach it; see _CHILDREN.
        with _CHILDREN_LOCK:
            _CHILDREN[role.name] = proc
        try:
            out, err = proc.communicate(timeout=timeout)
            code = proc.returncode
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate()
            code = -1
            err = (err or "") + f"\n[crew] killed after {timeout}s"
        finally:
            with _CHILDREN_LOCK:
                _CHILDREN.pop(role.name, None)

        res = RunResult(code, out, err)
        if res.ok and res.result:
            _note_working(name)
            if tried:
                log(f"{role.name}: {name} answered after "
                    f"{', '.join(tried)} refused")
            return res
        tried.append(name)
        log(f"{role.name}: no usable output from {name}; trying the next "
            f"provider")

    return res if res is not None else RunResult(-1, "", "no provider configured")


def failure_detail(res: RunResult) -> str:
    """Why a run failed, quoting THE AGENT'S OWN WORDS first.

    This function is the whole lesson of 2026-09-18. The old dispatcher read
    `err or out`, and because stderr always carries a harmless model warning,
    the stdout JSON -- the only place the real error lives -- was read by
    nobody. Here `out` is consulted FIRST and stderr is supporting detail.

    Order: the JSON `result` (the agent explaining itself), then permission
    denials (a denied tool is the most common real cause), then raw stderr.
    """
    parts = []
    if res.result:
        parts.append(f"the agent said: {res.result[:1500]}")
    if res.denials:
        rows = []
        for d in res.denials[:5]:
            if isinstance(d, dict):
                tool = d.get("tool_name") or d.get("tool") or "?"
                reason = d.get("reason") or d.get("message") or ""
                rows.append(f"{tool}: {str(reason)[:200]}")
            else:
                rows.append(str(d)[:200])
        parts.append("permission denied for " + "; ".join(rows))
    if res.is_error and not res.result:
        parts.append(f"is_error=true (subtype={res.subtype!r})")
    if not parts:
        tail = (res.err or res.out or "").strip().splitlines()[-8:]
        if tail:
            parts.append("no usable stdout; stderr tail:\n" +
                         "\n".join("    " + ln[:300] for ln in tail))
    parts.append(f"exit={res.code} turns={res.turns}")
    return "\n".join(parts)


# --- board writes --------------------------------------------------------------


def _reply(tid: int, author: str, body: str, meta: Optional[dict] = None) -> bool:
    conn = db.connect()
    try:
        db.reply(conn, tid, author, paths.AGENT_KIND, body, meta=meta or {})
        return True
    except Exception as exc:
        log(f"#{tid} could not post as {author}: {exc!r}")
        return False
    finally:
        conn.close()


def _set_meta_flag(tid: int, key: str) -> None:
    """Set one flag in a thread's meta. Used to make a note fire exactly once."""
    conn = db.connect()
    try:
        row = conn.execute("SELECT meta FROM threads WHERE id=?", (tid,)).fetchone()
        try:
            meta = json.loads((row["meta"] if row else None) or "{}") or {}
        except (TypeError, ValueError):
            meta = {}
        meta[key] = True
        conn.execute("UPDATE threads SET meta=? WHERE id=?",
                     (json.dumps(meta), tid))
        conn.commit()
    except Exception as exc:
        log(f"#{tid} could not set meta {key}: {exc!r}")
    finally:
        conn.close()


def _undo_attempt(conn, tid: int) -> None:
    """Give back the attempt that `release_task` just charged.

    A run killed because the operator pressed Stop is not the item's fault.
    `release_task` increments `attempts` unconditionally -- that is right for a
    genuine failure and wrong for a deliberate shutdown. Without this, every
    Stop burns one of the item's lives, so stopping the crew at the wrong moment
    a few times blocks a perfectly healthy item for good, which is the silent
    freeze this rewrite exists to remove, reintroduced by the Stop button.
    """
    try:
        row = conn.execute("SELECT meta FROM threads WHERE id=?", (tid,)).fetchone()
        meta = json.loads((row["meta"] if row else None) or "{}") or {}
        if isinstance(meta.get("attempts"), int) and meta["attempts"] > 0:
            meta["attempts"] -= 1
        meta["last_release_reason"] = "crew stopped mid-run; attempt not charged"
        conn.execute("UPDATE threads SET meta=? WHERE id=?",
                     (json.dumps(meta), tid))
        conn.commit()
    except Exception as exc:
        log(f"#{tid} could not undo the attempt charge: {exc!r}")


# --- the coordinator -----------------------------------------------------------


# Keyword shapes for the zero-cost prefilter. The model is asked only to
# CONFIRM or override, because a routing call that fails must not become an item
# that never runs -- that is how the old dispatcher lost twelve of them.
_RESEARCH_HINTS = (
    "what do we know", "where is", "which pipelines", "how do i", "how does",
    "find out", "look up", "research", "is there", "does anyone know",
    "what happened", "who owns", "remind me",
)
_VERIFY_HINTS = ("verify", "confirm that", "check that", "reproduce", "audit")


def _heuristic_route(subject: str, body: str) -> str:
    text = f"{subject}\n{body}".lower()
    if any(h in text for h in _VERIFY_HINTS):
        return roles.VERIFIER.name
    if any(h in text for h in _RESEARCH_HINTS):
        return roles.RESEARCHER.name
    return roles.BUILDER.name


# Item #111's "easy half": a crew role already polls the board on a loop
# (worker_loop / _claim_next), so unlike an ad-hoc Claude Code session it can
# genuinely be steered -- a message that names a role by @mention is telling
# the coordinator who this is for, and the router should not spend a keyword
# guess or a model call second-guessing that. Same mention syntax db.py
# records for everyone else (see db.extract_mentions); this just narrows the
# result to the three names that are also role names.
_MENTION_ROLE_RE = re.compile(r"(?<![\w.])@(builder|verifier|researcher)\b",
                              re.IGNORECASE)


def _mentioned_role(subject: str, body: str) -> Optional[str]:
    """The role @mentioned in this item's text, or None if none is."""
    m = _MENTION_ROLE_RE.search(f"{subject}\n{body}")
    return m.group(1).lower() if m else None


def _route(item: dict) -> tuple:
    """(role_name, why). An explicit @mention wins outright; otherwise the
    heuristic, then a model confirms. Never fails.

    The model call is deliberately tiny and bounded: three candidate names and
    nothing else. If it errors, times out, or answers with something that is not
    a role name, the heuristic stands.
    """
    subject = str(item.get("subject") or "")
    body = str(item.get("last_body") or "")

    mentioned = _mentioned_role(subject, body)
    if mentioned:
        return mentioned, f"explicit @{mentioned} mention"

    guess = _heuristic_route(subject, body)

    # Routing is a one-word classification over ~2KB of text -- exactly the
    # bulk-mechanical case Ladder's rung 0 exists for, and nothing here needs
    # the full agentic `claude -p` harness (tools, file access, multi-turn).
    # This used to spawn a whole `claude -p` process per item just to pick a
    # role, and that process inherited every fragility of the crew's own
    # failover chain (Fireworks billing, provider env bugs, `local`'s ~61s
    # cold start and empty answers under the full harness -- see
    # providers.json). A direct in-process Ladder call has none of that: it
    # is one HTTP round trip to a model that is either warm or isn't, and any
    # failure (Ladder not installed, no model warm, a bad response shape)
    # falls back to the keyword heuristic exactly as the old path did.
    word = _ladder_route(subject, body)
    if word is None:
        return guess, "heuristic (Ladder gave no usable answer)"
    if word == guess:
        return word, "the router agreed with the keyword read (ladder rung 0)"
    return word, f"the router overrode the keyword read ({guess}) (ladder rung 0)"


_LADDER_PARENT = Path(r"C:\Users\palencharj\NoOneDrive\LocalBuildFastCode")

_ROUTE_SYSTEM = (
    "You are routing one work item to the single best-suited agent.\n"
    "Answer with EXACTLY ONE WORD, one of: builder, verifier, researcher.\n\n"
    "builder    - implements or edits something; the default.\n"
    "verifier   - independently reproduces a claim someone else made.\n"
    "researcher - answers a question from existing knowledge; no code change."
)


def _ladder_router():
    """The Router this routing call runs on. Raises if unavailable -- the
    caller is the one place that decides what "unavailable" degrades to."""
    if str(_LADDER_PARENT) not in sys.path:
        sys.path.insert(0, str(_LADDER_PARENT))
    from ladder.router import Router  # type: ignore
    return Router()


def _ladder_route(subject: str, body: str) -> Optional[str]:
    """A role name from Ladder's local rung, or None on ANY failure.

    Never raises and never falls through to a paid rung: `max_rung=0` pins
    this to the free local tier deliberately, because a wrong routing guess
    costs nothing (the heuristic is the fallback) and is not worth spending
    allowance to avoid.
    """
    try:
        router = _ladder_router()
        result = router.run_job(
            prompt=f"SUBJECT: {subject}\n\nITEM:\n{body[:2000]}\n\nONE WORD:",
            kind="classify",
            rung=0, max_rung=0,
            system_extra=_ROUTE_SYSTEM,
            max_tokens=16,
            title="route work item",
        )
        if not result.get("ok"):
            return None
        word = str(result.get("result") or "").strip().lower().strip(".\"'`* \n")
        return word if word in roles.BY_NAME else None
    except Exception:
        # Ladder not importable, no model warm, network hiccup, an
        # unexpected response shape -- all of it means "the heuristic
        # decides", never "crash the dispatcher's routing pass".
        return None


def post_blocked(tid: int, subject: str, attempts: int) -> None:
    """Say out loud that an item is stuck. The old dispatcher never did.

    An item at the attempt cap used to be skipped in silence, so a blocked item
    looked exactly like one nobody had tried. This is the note that difference
    deserves -- posted ONCE per item, keyed off meta, so it cannot itself become
    the noise it exists to prevent.
    """
    _reply(tid, APP_NAME,
           f"**Blocked - needs a human.**\n\n"
           f"This item has been attempted {attempts} times (the cap is "
           f"{MAX_ATTEMPTS}) and has not completed. The crew will not pick it up "
           f"again until the cap is raised or the item is fixed.\n\n"
           f"Subject: {subject}\n\n"
           f"To clear it: raise `AGENTDESK_CREW_MAX_ATTEMPTS`, or reset this "
           f"item's `attempts` in its meta. Until then it is deliberately asleep "
           f"rather than looping.",
           meta={"kind": "crew-blocked", "attempts": attempts})


def coordinator_loop(stop: threading.Event, active: dict, lock: threading.Lock,
                     once: bool = False, dry_run: bool = False) -> None:
    """Assign open items to roles. Route only -- never do the work."""
    log("coordinator up")
    while not stop.is_set():
        try:
            conn = db.connect()
            try:
                pending = db.list_work(conn, status=paths.STATUS_OPEN, limit=50)
            finally:
                conn.close()
        except Exception as exc:
            log(f"coordinator could not read the queue: {exc!r}")
            if once:
                return
            stop.wait(POLL_SECONDS)
            continue

        # Oldest first: a queue everyone can add to but nobody drains from the
        # front is a stack, and the oldest item never gets reached.
        for item in reversed(pending):
            if stop.is_set():
                break
            tid = item["id"]
            try:
                meta = json.loads(item.get("meta") or "{}") or {}
            except (TypeError, ValueError):
                meta = {}
            attempts = int(meta.get("attempts") or 0)

            if attempts >= MAX_ATTEMPTS:
                # Loud, exactly once. This is the fix for the silent freeze.
                if dry_run:
                    log(f"#{tid} BLOCKED at {attempts} attempts - a real run "
                        f"would flag this needs-human: {item['subject'][:60]}")
                    continue
                if not meta.get("blocked_noted"):
                    _set_meta_flag(tid, "blocked_noted")
                    post_blocked(tid, item["subject"], attempts)
                    log(f"#{tid} BLOCKED at {attempts} attempts - flagged "
                        f"needs-human, it will not be retried")
                continue

            if not db.open_to_dispatcher(item):
                # Reserved for an agent to claim deliberately (db.claim_policy).
                # This loop sweeps every open item within POLL_SECONDS of it
                # being posted, so without this the reservation would be
                # decorative -- the crew would win the race for a job that was
                # explicitly meant to be left for somebody to choose. Silent in
                # a real run, because "the crew correctly left this alone" is
                # not news; reported in a dry run, where the point is to see
                # what the crew would and would not touch.
                if dry_run:
                    log(f"#{tid} reserved for anyone to claim - not routed: "
                        f"{item['subject'][:60]}")
                continue

            if dry_run:
                # Heuristic (plus the free @mention check) only. A dry run
                # must not write, and it must not spend a model call per item
                # either -- otherwise a "free" preview costs twelve router
                # invocations. Checking the mention override here too is what
                # keeps this preview honest: without it, a dry run would claim
                # an item is heuristic-routed to X while a real run sends it
                # to whoever it @mentions instead.
                subject, body = str(item.get("subject") or ""), str(item.get("last_body") or "")
                mentioned = _mentioned_role(subject, body)
                guess = mentioned or _heuristic_route(subject, body)
                how = f"explicit @{mentioned} mention" if mentioned else "heuristic, no model call"
                log(f"#{tid} would go to {guess} ({how}): {item['subject'][:60]}")
                continue

            role, why = _route(item)

            conn = db.connect()
            try:
                won = db.claim_task(conn, tid, role)
            finally:
                conn.close()
            if won:
                log(f"#{tid} -> {role} ({why}): {item['subject'][:60]}")
                _reply(tid, APP_NAME,
                       f"Coordinator: routing this to **{role}** -- {why}.",
                       meta={"kind": "crew-route", "role": role})
            # A lost race is not an error: the other worker got there first.

        if once:
            return
        stop.wait(POLL_SECONDS)


# --- the workers ---------------------------------------------------------------


def _claim_next(role_name: str):
    """The oldest item assigned to this role and still waiting, or None."""
    conn = db.connect()
    try:
        claimed = db.list_work(conn, status=paths.STATUS_CLAIMED, limit=50)
    finally:
        conn.close()
    for item in reversed(claimed):
        try:
            meta = json.loads(item.get("meta") or "{}") or {}
        except (TypeError, ValueError):
            meta = {}
        if meta.get("assignee") == role_name:
            return item
    return None


def worker_loop(role_name: str, stop: threading.Event, active: dict,
                lock: threading.Lock, once: bool = False) -> None:
    """Take this role's items one at a time, for as long as the crew runs.

    Long-lived: the session id is kept between items, so the agent remembers
    what it did and what it learned.
    """
    role = roles.get(role_name)
    if role is None:
        raise ValueError(f"no such role: {role_name}")

    if not _claude_exe():
        log(f"{role_name}: the claude CLI is not on PATH - this worker cannot run")
        return

    sess = load_session(role_name)
    log(f"{role_name} up (session={sess['id'] or 'fresh'}, items={sess['items']})")

    while not stop.is_set():
        item = _claim_next(role_name)
        if item is None:
            if once:
                return
            stop.wait(POLL_SECONDS)
            continue

        tid = item["id"]
        with lock:
            active[role_name] = tid
            write_state(dict(active))

        # Replace the conversation once it has run long enough that its context
        # is more baggage than memory. The memory file carries the continuity.
        resume = sess["id"]
        if sess["items"] >= RESET_AFTER:
            log(f"{role_name}: {sess['items']} items on this session - starting a "
                f"fresh one (the memory file carries continuity)")
            # Phoenix, Part A: the deterministic trigger (RESET_AFTER, a plain
            # int compare -- unchanged) fires the same handoff a torch_due
            # flag would for an interactive session. Sync whatever handoff.md
            # holds into the vault and record it in the handoffs table (which
            # also clears torch_due, though nothing sets it for a crew role
            # today -- this just keeps the table honest if something ever
            # does) BEFORE the session is thrown away, because after this
            # point the only copy of "what this role was mid-doing" is
            # whatever made it into that file.
            try:
                handoff_text = handoff_path(role_name).read_text(
                    encoding="utf-8").strip()
            except OSError:
                handoff_text = ""
            if handoff_text:
                sync_handoff_to_vault(role_name, handoff_text)
                conn = db.connect()
                try:
                    db.pass_the_torch(conn, role_name, handoff_text,
                                      path=str(handoff_path(role_name)))
                finally:
                    conn.close()
            else:
                log(f"{role_name}: no handoff.md content at reset - nothing "
                    f"to sync (the role never wrote one)")
            resume = None
            sess = {"id": None, "items": 0}

        log(f"{role_name} starting #{tid}: {item['subject'][:60]}")
        prompt = _prompt(role, item, _memory_excerpt(role_name))

        try:
            res = run_claude(role, prompt, resume)
        except FileNotFoundError as exc:
            log(f"{role_name}: {exc}")
            return
        except Exception as exc:
            log(f"{role_name}: could not run #{tid}: {exc!r}")
            conn = db.connect()
            try:
                db.release_task(conn, tid, role_name,
                                note=f"spawn failed: {exc!r}")
            finally:
                conn.close()
            with lock:
                active.pop(role_name, None)
                write_state(dict(active))
            continue

        if res.session_id:
            sess = {"id": res.session_id, "items": sess["items"] + 1}
            save_session(role_name, sess["id"], sess["items"])

        conn = db.connect()
        try:
            status = conn.execute(
                "SELECT status FROM threads WHERE id=?", (tid,)).fetchone()["status"]
        finally:
            conn.close()

        if status == paths.STATUS_DONE or res.ok:
            conn = db.connect()
            try:
                done = db.complete_task(conn, tid, role_name)
            finally:
                conn.close()
            if res.result:
                _reply(tid, role_name, res.result,
                       meta={"kind": "crew-report", "role": role_name})
            log(f"{role_name} #{tid} done"
                + ("" if done else " (it had already been completed)"))
        elif stop.is_set():
            # Killed by a Stop, not by a real failure. Put the item back and
            # give back the attempt release_task would otherwise charge.
            conn = db.connect()
            try:
                db.release_task(conn, tid, role_name,
                                note="crew stopped mid-run")
                _undo_attempt(conn, tid)
            finally:
                conn.close()
            log(f"{role_name} #{tid} released because the crew is stopping "
                f"(attempt not charged)")
        else:
            detail = failure_detail(res)
            conn = db.connect()
            try:
                db.release_task(conn, tid, role_name, note=detail[:400])
            finally:
                conn.close()
            log(f"{role_name} #{tid} FAILED: {detail[:400]}")
            _reply(tid, role_name,
                   f"**{role_name} could not finish this item.**\n\n{detail}\n\n"
                   f"The item is back on the queue; the coordinator will route it "
                   f"again or, at the cap, flag it as needing a human.",
                   meta={"kind": "crew-failure", "role": role_name})

        with lock:
            active.pop(role_name, None)
            write_state(dict(active))

        if once:
            return


# --- main ----------------------------------------------------------------------


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="agentdesk-crew",
        description="One coordinator and three long-lived agents.")
    parser.add_argument("--once", action="store_true",
                        help="one pass in every thread, then exit")
    parser.add_argument("--dry-run", action="store_true",
                        help="show routing decisions; claim nothing, run nothing")
    parser.add_argument("--role", action="append", metavar="NAME",
                        help="run only this role's worker (repeatable). For "
                             "testing one agent in isolation.")
    parser.add_argument("--no-coordinator", action="store_true",
                        help="workers only: take items that are already routed")
    args = parser.parse_args(argv)

    paths.ensure_dirs()
    conn = db.connect()
    try:
        db.init_db(conn)  # create the schema rather than die on it
    finally:
        conn.close()

    chosen = args.role or roles.names()
    for name in chosen:
        if roles.get(name) is None:
            parser.error(
                f"unknown role {name!r}; known: {', '.join(roles.names())}")

    if args.dry_run:
        log("DRY RUN - nothing will be claimed or run")
        coordinator_loop(threading.Event(), {}, threading.Lock(),
                         once=True, dry_run=True)
        return 0

    global _STARTED_TS
    _STARTED_TS = db.now_iso()
    try:
        STOP_FILE.unlink()  # a stale stop file reads as "the button does nothing"
    except OSError:
        pass

    stop = threading.Event()
    active: dict = {}
    lock = threading.Lock()
    write_state(active)

    log(f"crew starting: roles={chosen} mode={PERMISSION_MODE} "
        f"claude={_claude_exe()}")
    if MAX_CONCURRENT < len(chosen):
        log(f"note: concurrency cap is {MAX_CONCURRENT} but {len(chosen)} workers "
            f"are starting; the cap is not enforced per-worker yet")

    threads = []
    if not args.no_coordinator:
        threads.append(threading.Thread(
            target=coordinator_loop, args=(stop, active, lock, args.once),
            daemon=True, name="coordinator"))
    for name in chosen:
        threads.append(threading.Thread(
            target=worker_loop, args=(name, stop, active, lock, args.once),
            daemon=True, name=f"worker-{name}"))

    for th in threads:
        th.start()

    try:
        while any(th.is_alive() for th in threads):
            if STOP_FILE.exists():
                log(f"{STOP_FILE.name} is present - stopping")
                stop.set()
                break
            time.sleep(1)
    except KeyboardInterrupt:
        log("interrupted - stopping")
        stop.set()

    stop.set()
    # Reach the children before joining: a worker blocked inside communicate()
    # would otherwise hold Stop up for the rest of the run's timeout.
    killed = _kill_children()
    if killed:
        log(f"killed {killed} in-flight agent run(s); the items they held are "
            f"released back to the queue")
    for th in threads:
        th.join(timeout=60)
    clear_state()
    log("crew stopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
