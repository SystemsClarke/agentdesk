"""Acceptance checks for the reload-code affordance (thread 82 section 5,
built as work item 105): a relaunch that refuses outright while the
dispatcher holds a claimed item, and a real proof that the single-instance
mutex is actually released before the replacement process is spawned.

No real process is ever spawned here: subprocess.Popen is replaced with a
spy for the App-level checks, and no real dialog is ever shown: messagebox
is replaced with a spy too, since a real messagebox.showwarning would block
this script on a click that never comes.

    .venv\\Scripts\\python.exe scripts\\check_reload.py
"""

from __future__ import annotations

import ctypes
import json
import os
import sys
import tempfile
import tkinter as tk
from pathlib import Path

_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-reload-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import app as appmod  # noqa: E402
from agentdesk import db, paths  # noqa: E402

FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if not ok:
        FAILURES.append(label + (f" ({detail})" if detail else ""))
    print(f"  [{'ok' if ok else 'FAIL'}] {label}" + (f" -- {detail}" if detail else ""))


class _PopenSpy:
    calls: list = []

    def __init__(self, cmd, **kwargs):
        _PopenSpy.calls.append((cmd, kwargs))

    def poll(self):
        return None


class _MessageBoxSpy:
    warnings: list = []
    errors: list = []

    @staticmethod
    def showwarning(title, message):
        _MessageBoxSpy.warnings.append((title, message))

    @staticmethod
    def showerror(title, message):
        _MessageBoxSpy.errors.append((title, message))


class _GuardSpy:
    def __init__(self):
        self.released = 0

    def release(self):
        self.released += 1


def write_heartbeat(item) -> None:
    paths.DATA_DIR.mkdir(parents=True, exist_ok=True)
    paths.WORKER_STATE.write_text(json.dumps({
        "pid": os.getpid(), "item": item, "agent": "builder", "mode": "auto",
    }), encoding="utf-8")


def main() -> int:
    print("=== SingleInstance.release() actually frees the mutex ===")
    print("    (a real OS object, tested directly, no window involved)")
    db_path = _SCRATCH / "guard.db"
    first = appmod.SingleInstance(db_path)
    check("the first guard claims the mutex", not first.already_running)
    second_before = appmod.SingleInstance(db_path)
    check("a second guard on the same db sees the first as running",
          second_before.already_running)

    first.release()
    third = appmod.SingleInstance(db_path)
    check("after release(), a new guard can claim the SAME mutex",
          not third.already_running)
    check("release() is idempotent -- calling it again does not raise",
          _call_ok(first.release))

    print("\n=== the window: refuses to reload while an item is claimed ===")
    appmod.subprocess.Popen = _PopenSpy
    appmod.messagebox = _MessageBoxSpy
    _PopenSpy.calls.clear()
    _MessageBoxSpy.warnings.clear()

    window_db = _SCRATCH / "appdata" / "AgentDesk" / "agentdesk.db"
    window_db.parent.mkdir(parents=True, exist_ok=True)
    conn = db.connect(window_db)
    try:
        db.init_db(conn)
    finally:
        conn.close()

    root = tk.Tk()
    root.withdraw()
    window = None
    try:
        window = appmod.App(window_db)
        window.root.withdraw()
        window._instance_guard = _GuardSpy()

        write_heartbeat(item=42)
        window._reload_code()
        check("a mid-item reload does not spawn a replacement process",
              _PopenSpy.calls == [])
        check("a mid-item reload does not release the instance guard",
              window._instance_guard.released == 0)
        check("a mid-item reload warns instead of silently doing nothing",
              len(_MessageBoxSpy.warnings) == 1)
        check("the warning names the held item",
              "#42" in _MessageBoxSpy.warnings[0][1],
              _MessageBoxSpy.warnings[0][1] if _MessageBoxSpy.warnings else "")

        print("\n=== idle dispatcher: reload spawns the replacement and quits ===")
        write_heartbeat(item=None)
        _PopenSpy.calls.clear()
        window._reload_code()
        check("an idle-dispatcher reload spawns exactly one process",
              len(_PopenSpy.calls) == 1, str(_PopenSpy.calls))
        if _PopenSpy.calls:
            cmd = _PopenSpy.calls[0][0]
            check("the replacement command re-launches this module",
                  "agentdesk.app" in cmd, str(cmd))
            check("the replacement command carries this window's db path",
                  str(window_db) in cmd, str(cmd))
        check("the instance guard is released before spawning the replacement",
              window._instance_guard.released == 1)
        check("the old window is torn down (destroy calling this raises TclError)",
              _root_is_destroyed(window.root))
        window = None  # already torn down by _reload_code's own _quit()

        print("\n=== no worker running at all: reload is not blocked by that ===")
        try:
            paths.WORKER_STATE.unlink()
        except OSError:
            pass
        window2 = appmod.App(window_db)
        window2.root.withdraw()
        window2._instance_guard = _GuardSpy()
        _PopenSpy.calls.clear()
        window2._reload_code()
        check("with no worker heartbeat at all, reload still proceeds",
              len(_PopenSpy.calls) == 1)
        window = window2
    finally:
        if window is not None:
            try:
                if window.icon is not None:
                    window.icon.stop()
            except Exception:
                pass
            try:
                window.root.destroy()
            except tk.TclError:
                pass
        root.destroy()

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("all checks passed")
    return 0


def _call_ok(fn) -> bool:
    try:
        fn()
        return True
    except Exception:
        return False


def _root_is_destroyed(root: tk.Tk) -> bool:
    try:
        root.winfo_exists()
    except tk.TclError:
        return True
    return not bool(root.winfo_exists())


if __name__ == "__main__":
    sys.exit(main())
