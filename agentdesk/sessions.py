"""The one place AgentDesk starts a Claude Code session.

Every headless run goes through `run()`: the crew's agents, Wake, anything else.
That is what makes the two settings on the Options screen true everywhere:

  max_sessions  how many agent sessions may run at once, across every process
                (a slot is a locked file, so a crashed run frees its slot)
  provider      which backend they run on: a profile from ~/.claude/providers.json,
                "claude" (the subscription) by default. If it gives no usable
                answer, the next profile in the chain is tried.

It also remembers which backend actually answered, so the window can say what
the swarm is running on rather than what it was asked to run on.
"""

from __future__ import annotations

import json
import msvcrt
import os
import shutil
import subprocess
import threading
import time
from pathlib import Path
from typing import Callable, Optional

from agentdesk import paths, providers, settings

REPO = Path(__file__).resolve().parent.parent
SLOTS_DIR = paths.DATA_DIR / "slots"
STATUS_DIR = paths.DATA_DIR / "live-sessions"

_live: dict = {}          # name -> {"pid", "started", "provider", "resume"}
_live_lock = threading.Lock()
_answered: Optional[str] = None


def max_sessions() -> int:
    try:
        return max(1, int(os.environ.get("AGENTDESK_MAX_SESSIONS") or settings.load()["max_sessions"]))
    except (TypeError, ValueError, KeyError):
        return 3


def backend() -> str:
    return (os.environ.get("AGENTDESK_CREW_PROVIDER") or settings.load().get("provider") or "claude").strip()


def claude_exe() -> Optional[str]:
    return shutil.which("claude") or shutil.which("claude.cmd") or \
        (str(p) if (p := Path.home() / ".local" / "bin" / "claude.exe").exists() else None)


def no_window() -> dict:
    """Popen kwargs that keep a child from flashing a console (we run under pythonw)."""
    si = subprocess.STARTUPINFO()
    si.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    si.wShowWindow = subprocess.SW_HIDE
    return {"creationflags": subprocess.CREATE_NO_WINDOW, "startupinfo": si}


class Slot:
    """One of max_sessions() slots, held for the life of a run. Blocks until one is free."""

    def __init__(self, name: str, stop: Optional[threading.Event] = None):
        self.name, self.stop, self.fh = name, stop, None

    def __enter__(self):
        SLOTS_DIR.mkdir(parents=True, exist_ok=True)
        while True:
            for i in range(max_sessions()):  # re-read each pass: the cap is live
                fh = open(SLOTS_DIR / f"slot-{i}.lock", "a+")
                try:
                    msvcrt.locking(fh.fileno(), msvcrt.LK_NBLCK, 1)
                    self.fh = fh
                    return self
                except OSError:
                    fh.close()
            if self.stop is not None and self.stop.wait(2):
                raise InterruptedError("stopped while waiting for a session slot")
            time.sleep(0 if self.stop is not None else 2)

    def __exit__(self, *exc):
        try:
            msvcrt.locking(self.fh.fileno(), msvcrt.LK_UNLCK, 1)
        except OSError:
            pass
        self.fh.close()


def _publish() -> None:
    """This process's live runs, in its own file (the crew and the app both spawn)."""
    try:
        STATUS_DIR.mkdir(parents=True, exist_ok=True)
        path = STATUS_DIR / f"{os.getpid()}.json"
        with _live_lock:
            data = {"ts": time.time(), "answered": _answered, "live": dict(_live)}
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, path)
    except OSError:
        pass


def status() -> dict:
    """What the window shows: live runs across processes, the cap, the chosen and answering backend."""
    from agentdesk.wake import pid_alive
    live, answered, newest = {}, None, 0.0
    for f in STATUS_DIR.glob("*.json") if STATUS_DIR.exists() else []:
        try:
            data = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if not pid_alive(int(f.stem)):
            f.unlink(missing_ok=True)
            continue
        live.update({n: r for n, r in (data.get("live") or {}).items() if pid_alive(r.get("pid"))})
        if data.get("answered") and data.get("ts", 0) > newest:
            answered, newest = data["answered"], data["ts"]
    return {"live": live, "answered": answered, "max": max_sessions(), "backend": backend()}


class Result:
    """What one run produced. Parsed from `--output-format json`; a failure does not raise."""

    def __init__(self, code: int, out: str, err: str, provider: str = ""):
        self.code, self.out, self.err, self.provider = code, out or "", err or "", provider
        try:
            payload = json.loads(self.out.strip().splitlines()[-1]) if self.out.strip() else {}
        except (ValueError, IndexError):
            payload = {}
        payload = payload if isinstance(payload, dict) else {}
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


def run(name: str, prompt: str, *, resume: Optional[str] = None, cwd: Optional[str] = None,
        permission_mode: str = "auto", model: Optional[str] = None, timeout: int = 3600,
        stop: Optional[threading.Event] = None, on_spawn: Optional[Callable] = None) -> Result:
    """Run one headless session as `name` (its board identity). Waits for a slot first.

    Tries the chosen backend, then the rest of the provider chain, but only when a run
    produced no usable answer at all: an answer is never re-asked of a different model.
    """
    global _answered
    exe = claude_exe()
    if not exe:
        raise FileNotFoundError("claude CLI not on PATH")
    cmd = [exe, "-p", prompt, "--permission-mode", permission_mode, "--output-format", "json",
           "--add-dir", str(REPO)]
    if model:
        cmd += ["--model", model]
    if resume:
        cmd += ["--resume", resume]
    res: Optional[Result] = None
    with Slot(name, stop):
        for prov, env in providers.env_chain(backend()):
            env = dict(env, AGENTDESK_AUTHOR=name)
            effort = providers.effort_for(prov)
            proc = subprocess.Popen(cmd + (["--effort", effort] if effort else []), cwd=cwd or str(REPO),
                                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    text=True, encoding="utf-8", errors="replace", env=env, **no_window())
            with _live_lock:
                _live[name] = {"pid": proc.pid, "started": time.time(), "provider": prov, "resume": bool(resume)}
            _publish()
            if on_spawn:
                on_spawn(proc)
            try:
                out, err = proc.communicate(timeout=timeout)
                code = proc.returncode
            except subprocess.TimeoutExpired:
                proc.kill()
                out, err = proc.communicate()
                code, err = -1, (err or "") + f"\n[sessions] killed after {timeout}s"
            finally:
                with _live_lock:
                    _live.pop(name, None)
            res = Result(code, out, err, prov)
            if res.ok and res.result:
                _answered = prov
                _publish()
                return res
            _publish()
            if stop is not None and stop.is_set():
                break
    return res if res is not None else Result(-1, "", "no provider configured")


def run_detached(name: str, prompt: str, **kw) -> None:
    """run() on a background thread, for callers on the UI thread (Wake)."""
    def work():
        try:
            run(name, prompt, **kw)
        except Exception:
            pass
    threading.Thread(target=work, daemon=True, name=f"session-{name}").start()
