"""The AgentDesk window: three channel pages, a tray icon, and a 3-second poll.

Run with pythonw.exe, so nothing here may require a console. The database is
shared with the MCP server and the hourly backup, so every read and write
below opens a short-lived connection and closes it again; no connection is
held open across the window's lifetime.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import logging
import os
import queue
import sqlite3
import subprocess
import sys
import threading
import tkinter as tk
from tkinter import messagebox, ttk
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

import pystray
from PIL import Image

# pythonw runs a file as a plain script, where there is no package context for
# a relative import; fall back to putting the repo root on the path.
try:
    from agentdesk import (aumid, db, dictate, icon, identity, notify,
                          paths, pr_scan, prs, settings, usage, vault, winedit)
except ImportError:  # pragma: no cover - depends on how the file was launched
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from agentdesk import (aumid, db, dictate, icon, identity, notify,
                          paths, pr_scan, prs, settings, usage, vault, winedit)

log =logging.getLogger("agentdesk.app")

def _self_argv(subcommand: str, extra: Optional[list[str]] = None) -> list[str]:
    """The argv to re-invoke this program as `subcommand`, from source or exe.

    Packaged (Nuitka-compiled) builds run as one exe, `AgentDesk.exe`, with
    the module name as a subcommand ("crew", "app", ...) -- see cli.py. Under
    plain source, sys.executable is the venv's python(w).exe and the old
    `-m agentdesk.<module>` form is what actually exists. Nuitka stamps a
    `__compiled__` global into every compiled module's namespace, which is
    the cheapest reliable way to tell the two apart at runtime.
    """
    extra = extra or []
    if "__compiled__" in globals():
        return [sys.executable, subcommand, *extra]
    exe = sys.executable
    if subcommand == "app" and exe.lower().endswith("python.exe"):
        exe = exe[: -len("python.exe")] + "pythonw.exe"
    return [exe, "-m", f"agentdesk.{subcommand}", *extra]


def local_ts(iso: str) -> str:
    """Render a UTC ISO timestamp as a short LOCAL-time string for display."""
    try:
        dt = datetime.fromisoformat(iso)
    except (TypeError, ValueError):
        return str(iso)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone().strftime("%Y-%m-%d %H:%M")


def _enable_dpi_awareness() -> None:
    # Must happen before the first window is drawn, otherwise Windows scales
    # the window as a bitmap and the text blurs. Niceties are not worth a
    # crash, so any failure here just means the old blurry behaviour.
    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(1)
    except Exception:
        try:
            ctypes.windll.user32.SetProcessDPIAware()
        except Exception:
            pass


def _flash_taskbar(root: tk.Tk) -> None:
    # FLASHW_ALL | FLASHW_TIMERNOFG: flash both the title bar and the taskbar
    # button until the window comes to the foreground.
    try:
        class FLASHWINFO(ctypes.Structure):
            _fields_ = [
                ("cbSize", ctypes.c_uint),
                ("hwnd", ctypes.c_void_p),
                ("dwFlags", ctypes.c_uint),
                ("uCount", ctypes.c_uint),
                ("dwTimeout", ctypes.c_uint),
            ]

        hwnd = ctypes.windll.user32.GetParent(root.winfo_id())
        info = FLASHWINFO(
            ctypes.sizeof(FLASHWINFO), hwnd, 0x00000003 | 0x0000000C, 0, 0
        )
        ctypes.windll.user32.FlashWindowEx(ctypes.byref(info))
    except Exception:
        pass


# --- how a row says what state it is in ---------------------------------------
#
# The board has two state machines that mean different things -- a question is
# open until John answers it, a work item is open until an agent takes it -- and
# they share the word "open". So the word and the colour a row shows are decided
# here, per channel, rather than by drawing paths.STATUS_* straight: "open" on
# the Work tab has to read as available and "open" on the Questions tab as
# waiting on a person, or the two tabs look like the same list.
#
# The colour is the point of the table. Before this, a finished item, a claimed
# one and an answered question were all drawn identically, and the only row that
# looked like anything was the open question -- so the list could not be skimmed
# for "what still needs doing". Loudest for what needs a person, quiet for
# what is settled.
def _tray_image() -> Image.Image:
    # Drawn from code (see icon.py), so the package never needs an image file on disk.
    return icon.render(64)


_MUTEX_ERROR_ALREADY_EXISTS = 183
_SW_RESTORE = 9


def window_title(db_path: Path, open_count: Optional[int] = None) -> str:
    """The title of the window showing this board.

    Only the real board gets the bare name; a copy says which copy it is, both
    so it is honest on screen and so `raise_existing` finds the right window
    when two boards really are open side by side.

    The open count is composed HERE and not by whoever is renaming the window.
    It used to be built at the call site, as "AgentDesk - N open questions",
    which threw the `[db]` marker away the moment the first poll found a
    question: a --db copy then wore the real board's title, and the launcher
    could no longer tell the two apart -- which is exactly what happened while
    checking this change, and what the screenshot had to work around.
    """
    base = (paths.APP_NAME if Path(db_path) == paths.DB_PATH
            else f"{paths.APP_NAME} [{Path(db_path).name}]")
    if open_count:
        noun = "question" if open_count == 1 else "questions"
        return f"{base} - {open_count} open {noun}"
    return base


class SingleInstance:
    """Hold the one-window-per-board mutex for as long as this object lives.

    The handle is deliberately never closed on the success path: it is released
    when the process exits, which is the whole point of using a mutex. Keep a
    reference to this object alive for the same reason.
    """

    def __init__(self, db_path: Path) -> None:
        self.title = window_title(db_path)
        self.already_running = False
        self._handle = None
        if os.name != "nt":
            return  # the tests run on Windows; other platforms just skip the guard
        digest = hashlib.sha1(
            str(Path(db_path).resolve()).lower().encode("utf-8")
        ).hexdigest()[:16]
        # use_last_error=True gives this DLL its own error slot, so the read
        # below cannot be clobbered by an unrelated ctypes call in between.
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        # A HANDLE is pointer-sized, and ctypes defaults restype to c_int -- so
        # without these, the handle is truncated to 32 bits on the way back.
        # A truncated handle can alias some OTHER handle in the process, and
        # CloseHandle on it would then close the wrong thing.
        kernel32.CreateMutexW.restype = ctypes.c_void_p
        kernel32.CreateMutexW.argtypes = [
            ctypes.c_void_p, ctypes.c_int, ctypes.c_wchar_p]
        kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
        handle = kernel32.CreateMutexW(None, True, f"Local\\AgentDesk.{digest}")
        # CreateMutex succeeds even when the name is taken -- it hands back a
        # handle to the EXISTING mutex -- so the existence check is this error,
        # read immediately, and not the return value.
        self.already_running = (
            ctypes.get_last_error() == _MUTEX_ERROR_ALREADY_EXISTS)
        if self.already_running:
            kernel32.CloseHandle(handle)  # somebody else's; we do not own it
        else:
            self._handle = handle         # ours, held until the process ends

    def _hwnd(self) -> int:
        """The handle of this board's window, or 0.

        By exact title first, then by prefix. The exact title stops matching as
        soon as the first poll puts the open count in it ("AgentDesk - 2 open
        questions"), so a search that only knew the base name reported "running
        but I could not raise it" whenever there was an open question -- which
        is most of the time. The prefix is anchored on the separator the title
        itself uses, so it cannot catch the OTHER board's window: that one
        reads "AgentDesk [agentdesk.db] - ...", which does not start with
        "AgentDesk - ".
        """
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        # Declared, not defaulted: without these ctypes returns an HWND as a
        # C int and silently truncates it on a 64-bit build, which is the same
        # mistake the mutex below goes out of its way to avoid.
        user32.FindWindowW.restype = ctypes.c_void_p
        user32.FindWindowW.argtypes = [ctypes.c_wchar_p, ctypes.c_wchar_p]
        user32.GetWindowTextW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p,
                                          ctypes.c_int]
        user32.EnumWindows.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        hwnd = user32.FindWindowW(None, self.title)
        if hwnd:
            return hwnd
        prefix = self.title + " - "
        found = ctypes.c_void_p(0)
        proc = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p,
                                  ctypes.c_void_p)

        def visit(handle, _param):
            text = ctypes.create_unicode_buffer(512)
            user32.GetWindowTextW(handle, text, 512)
            if text.value.startswith(prefix):
                found.value = handle
                return False  # stop: the first match is the window
            return True

        user32.EnumWindows(proc(visit), None)
        return found.value or 0

    def raise_existing(self) -> bool:
        """Bring the running window forward. False if that could not be done.

        Best-effort on purpose. Windows refuses a foreground steal from a
        process the user is not currently interacting with, so this genuinely
        fails sometimes -- so it reports what happened rather than assuming it
        worked, and the caller still exits cleanly either way.
        """
        if os.name != "nt":
            return False
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        user32.ShowWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
        user32.SetForegroundWindow.argtypes = [ctypes.c_void_p]
        user32.GetForegroundWindow.restype = ctypes.c_void_p
        hwnd = self._hwnd()
        if not hwnd:
            return False  # running, but with no window to raise: tray-only,
                          # or hidden -- both are legitimate states
        user32.ShowWindow(hwnd, _SW_RESTORE)  # in case it is minimised
        user32.SetForegroundWindow(hwnd)
        return user32.GetForegroundWindow() == hwnd

    def release(self) -> None:
        """Give up the mutex explicitly, ahead of process exit.

        The class docstring's "never closed on the success path" is still the
        right default: closing it early on a normal exit buys nothing, since
        the OS reclaims it at process teardown anyway. This exists for the one
        caller where that timing is not good enough -- a deliberate code
        reload that spawns a replacement process before this one has actually
        exited. Without releasing here first, the replacement's own
        SingleInstance sees this process as still alive (it has not exited
        yet -- it is still unwinding the call stack that led here) and quietly
        declines to open a window, which reads as "Reload code did nothing".
        """
        if self._handle is not None:
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
            kernel32.CloseHandle(self._handle)
            self._handle = None


# --- is that dispatcher still alive? -----------------------------------------
#
# The worker leaves a heartbeat file naming its pid, but a worker that CRASHED
# also leaves it behind -- so the file alone is not evidence of anything, and a
# stale heartbeat that reads as "running" is the one wrong answer here. This is
# what turns it into evidence: ask the OS, never trust a recorded number.
#
# It is the same lesson as the single-instance guard, which is why it uses the
# same call. OpenProcess on a pid that has exited and been recycled can look
# identical to one we are merely not allowed to open; returning False for a
# live-but-protected process only mislabels the button, which is the safe
# direction to be wrong in.
_SYNCHRONIZE = 0x00100000
_WAIT_TIMEOUT = 0x00000102


def pid_alive(pid) -> bool:
    """True if a process with this pid exists right now."""
    if os.name != "nt" or not pid:
        return False
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.restype = ctypes.c_void_p
    kernel32.OpenProcess.argtypes = [ctypes.c_uint, ctypes.c_int, ctypes.c_uint]
    kernel32.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint]
    kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
    handle = kernel32.OpenProcess(_SYNCHRONIZE, False, int(pid))
    if not handle:
        return False
    try:
        # A 0 timeout: signalled means it has already exited; WAIT_TIMEOUT
        # means the process is still running.
        return kernel32.WaitForSingleObject(handle, 0) == _WAIT_TIMEOUT
    finally:
        kernel32.CloseHandle(handle)


# --- dictation: Ctrl+D in any text field ------------------------------------
#
# Modeled on the same keystroke in Claude Code's own terminal UI: press it in
# a text field to start listening, press it again to stop. sherpa-onnx and
# the microphone callback run on a background thread (agentdesk.dictate's own
# rule -- see its module docstring); everything below only ever reaches a
# widget through `app.ui_queue`, the same hand-off pystray's tray callbacks
# already use to get onto the tk thread safely.
_DICTATE_INDICATOR_BG = "#fff3cd"
_DICTATE_INDICATOR_FG = "#856404"


class _LiveInsert:
    """Replaces the text typed since dictation started with each new partial.

    tk.Text and ttk.Entry/tk.Entry need different index arithmetic (a Text
    mark vs. a plain integer position), so this is the one place that knows
    which -- everything above it just calls update()/finish().
    """

    def __init__(self, widget: tk.Widget) -> None:
        self.widget = widget
        self._last = ""
        if isinstance(widget, tk.Text):
            widget.mark_set("dictate_start", "insert")
            # gravity "left": text inserted right at the mark does not push
            # it forward, so it keeps pointing at where dictation began
            # rather than chasing the text it is about to replace.
            widget.mark_gravity("dictate_start", "left")
        else:
            self._start = widget.index("insert")

    def update(self, text: str) -> None:
        widget = self.widget
        try:
            if isinstance(widget, tk.Text):
                widget.delete("dictate_start",
                             f"dictate_start + {len(self._last)}c")
                widget.insert("dictate_start", text)
                widget.mark_set("insert", f"dictate_start + {len(text)}c")
            else:
                widget.delete(self._start, self._start + len(self._last))
                widget.insert(self._start, text)
                widget.icursor(self._start + len(text))
        except tk.TclError:
            # The widget went away (dialog closed) or refused the edit (now
            # disabled) mid-dictation. Nothing left to update; not an error
            # the person dictating needs to see.
            pass
        self._last = text


class DictationController:
    def __init__(self, app: "App") -> None:
        self.app = app
        self.session: Optional[dictate.Session] = None
        self.insert: Optional[_LiveInsert] = None
        self.indicator: Optional[tk.Label] = None
        self._downloading = False

    def attach(self, root: tk.Misc) -> None:
        root.bind_all("<Control-d>", self._on_key)
        self.mic: Optional[dictate.Mic] = None
        self._disarm_job = None
        for cls in ("Text", "Entry", "TEntry"):
            root.bind_class(cls, "<FocusIn>", self._on_focus_in, add="+")
            root.bind_class(cls, "<FocusOut>", self._on_focus_out, add="+")
            # Replaces the class default (delete next char), which runs before bind_all.
            root.bind_class(cls, "<Control-d>", self._on_key)

    def enable_preroll(self, on: bool) -> None:
        """Keep the last ~2 s of mic audio in RAM while a text box has focus (real launches only)."""
        if on and self.mic is None and dictate.is_downloaded():
            self.mic = dictate.Mic(seconds=2.0)
        elif not on and self.mic is not None:
            self.mic.disarm()
            self.mic = None

    def _typing_widget(self, w) -> bool:
        if getattr(w, "_agentdesk_readonly", False):
            return False
        try:
            return str(w.cget("state")) == "normal"
        except tk.TclError:
            return False

    def _on_focus_in(self, event: tk.Event) -> None:
        if self.mic is None or not self._typing_widget(event.widget):
            return
        if self._disarm_job is not None:
            self.app.root.after_cancel(self._disarm_job)
            self._disarm_job = None
        self.mic.arm()

    def _on_focus_out(self, _event: tk.Event) -> None:
        if self.mic is None:
            return
        # A grace period, so tabbing between two boxes doesn't close and reopen the device.
        if self._disarm_job is not None:
            self.app.root.after_cancel(self._disarm_job)
        self._disarm_job = self.app.root.after(1500, self._disarm)

    def _disarm(self) -> None:
        self._disarm_job = None
        focused = self.app.root.focus_get()
        if self.mic is not None and not (focused is not None and self._typing_widget(focused)):
            self.mic.disarm()

    def _on_key(self, event: tk.Event) -> Optional[str]:
        if self.session is not None:
            self._stop()
            return "break"
        widget = event.widget
        if not isinstance(widget, (tk.Text, tk.Entry, ttk.Entry)):
            return None
        try:
            if str(widget.cget("state")) == "disabled" or getattr(widget, "_agentdesk_readonly", False):
                return None
        except tk.TclError:
            pass
        if self._downloading:
            return "break"
        if not dictate.is_downloaded():
            self._download_then(lambda: self._start(widget))
            return "break"
        self._start(widget)
        return "break"

    def _start(self, widget: tk.Widget) -> None:
        self.insert = _LiveInsert(widget)
        self._show_indicator(widget)
        self.session = dictate.Session(
            on_text=lambda text, final:
                self.app.ui_queue.put(lambda: self._on_text(text, final)),
            on_error=lambda exc:
                self.app.ui_queue.put(lambda: self._on_error(exc)),
            mic=self.mic,
        )
        self.session.start()

    def _stop(self) -> None:
        if self.session is not None:
            self.session.stop()
        # self.session itself is cleared in _on_text once the final=True
        # callback arrives, not here -- stop() only asks the mic to close;
        # the tail of the audio is still being decoded for a moment after.

    def _on_text(self, text: str, final: bool) -> None:
        if self.insert is not None:
            self.insert.update(text)
        if final:
            self._hide_indicator()
            self.session = None
            self.insert = None

    def _on_error(self, exc: Exception) -> None:
        self._hide_indicator()
        self.session = None
        self.insert = None
        messagebox.showerror(
            "AgentDesk - dictation",
            f"Dictation stopped because of an error:\n\n{exc}\n\n"
            "This is usually a missing or busy microphone -- Settings > "
            "Privacy > Microphone, or another app holding it exclusively.")

    _DOTS = ("●  ·  ·", "·  ●  ·", "·  ·  ●", "·  ●  ·")

    def _show_indicator(self, widget: tk.Widget) -> None:
        """Three pulsing dots tucked into the field's top-right corner, in the field's own colours."""
        try:
            bg = widget.cget("background")
            fg = widget.cget("insertbackground")
        except tk.TclError:
            bg, fg = _DICTATE_INDICATOR_BG, _DICTATE_INDICATOR_FG
        self.indicator = tk.Label(widget.winfo_toplevel(), text=self._DOTS[0], bg=bg, fg=fg,
                                  font=("Segoe UI", 9, "bold"), padx=6, pady=0, bd=0)
        # Anchored to the widget via `in_=`, so it tracks the field if the window moves.
        self.indicator.place(in_=widget, relx=1.0, x=-8, y=4, anchor="ne")
        self._dot_step = 0
        self._animate_dots()

    def _animate_dots(self) -> None:
        if self.indicator is None:
            return
        self._dot_step = (self._dot_step + 1) % len(self._DOTS)
        try:
            self.indicator.configure(text=self._DOTS[self._dot_step])
            self._dot_job = self.indicator.after(260, self._animate_dots)
        except tk.TclError:
            self.indicator = None

    def _hide_indicator(self) -> None:
        if self.indicator is not None:
            try:
                if getattr(self, "_dot_job", None):
                    self.indicator.after_cancel(self._dot_job)
                self.indicator.destroy()
            except tk.TclError:
                pass
            self.indicator = None

    def _download_then(self, then: Callable[[], None]) -> None:
        if not messagebox.askyesno(
            "AgentDesk - dictation",
            "Dictation needs a one-time download of the local speech "
            f"model (about {dictate.TOTAL_BYTES // (1024 * 1024)} MB, "
            "Nemotron-Speech-Streaming-EN-0.6B). Nothing is uploaded and "
            "no audio ever leaves this machine.\n\nDownload it now?"
        ):
            return
        self._downloading = True
        win = tk.Toplevel(self.app.root)
        win.title("AgentDesk - downloading speech model")
        win.resizable(False, False)
        ttk.Label(win, text="Downloading the dictation model...",
                 padding=(12, 12, 12, 4)).pack()
        bar = ttk.Progressbar(win, length=320, maximum=1000)
        bar.pack(padx=12, pady=(0, 12))
        pct_lbl = ttk.Label(win, text="0%")
        pct_lbl.pack(pady=(0, 12))

        last = [-1]

        def _progress(done: int, total: int) -> None:
            pct = 100 * done // total
            if pct == last[0]:
                return  # one update per percent, not one per 8 KB block
            last[0] = pct
            self.app.ui_queue.put(
                lambda: win.winfo_exists() and (bar.configure(value=int(1000 * done / total)),
                                                pct_lbl.configure(text=f"{pct}%")))

        def _worker() -> None:
            try:
                dictate.download_model(_progress)
                self.app.ui_queue.put(lambda: (_finish(), then()))
            except Exception as exc:
                # Bound now: Python deletes `exc` when the except block ends, so a
                # lambda that looked it up later raised NameError instead of reporting.
                self.app.ui_queue.put(lambda err=exc: _fail(err))

        def _finish() -> None:
            self._downloading = False
            win.destroy()

        def _fail(exc: Exception) -> None:
            self._downloading = False
            win.destroy()
            messagebox.showerror(
                "AgentDesk - dictation",
                f"Could not download the speech model:\n\n{exc}")

        threading.Thread(target=_worker, daemon=True).start()


class App:
    POLL_MS = 3000

    def __init__(self, db_path: Path) -> None:
        self.db_path = db_path
        self._worker_proc: Optional[subprocess.Popen] = None
        self.ui_queue: "queue.Queue[Callable[[], None]]" = queue.Queue()
        self.last_max_id = -1
        self.last_open_count = -1
        self.announced: set[int] = set()
        self._first_poll = True
        # A cheap digest of the merge list, so a redraw is asked for when a PR
        # changes state. The message-id counter cannot see that: a merge that
        # has no thread to notify changes no message at all.
        self.last_prs_sig = None
        # The pull-request watcher's two events, and they are separate on
        # purpose. _pr_stop ends the thread; _pr_wake is the Check now button
        # asking it to run a pass immediately instead of waiting out its
        # interval. One event could not mean both.
        self._pr_stop = threading.Event()
        self._pr_wake = threading.Event()
        self._pr_first = True
        # How many check passes have FINISHED. The wake event says a pass was
        # asked for; this says one is over, which is a different fact and the
        # one anything outside the thread needs. Without it the only observable
        # effect of waking the watcher is the first `gh` call it makes, and a
        # caller waiting on that proceeds while the remaining PRs are still
        # being checked -- which is exactly how the acceptance harness for this
        # feature first reported a merge that had not happened yet.
        self.pr_passes = 0

        # Idempotent, and needed for a --db test copy that starts empty.
        conn = db.connect(self.db_path)
        try:
            db.init_db(conn)
        finally:
            conn.close()

        self.root = tk.Tk()
        # Must match window_title(), which is what a second launch looks for
        # when it tries to raise this window.
        self.root.title(window_title(db_path))
        self.root.geometry("1120x760")
        self.root.minsize(720, 420)
        self.root.protocol("WM_DELETE_WINDOW", self._hide_to_tray)
        winedit.install(self.root)
        try:
            self._icon_images = icon.photo_images(self.root)
            self.root.iconphoto(True, *self._icon_images)
        except Exception:
            pass

        self.dictation = DictationController(self)
        self.dictation.attach(self.root)

        from agentdesk import terminal
        self.view = terminal.TerminalView(self, self.root)
        self._finish_startup()
        self.root.after(250, self.view.connected)

    def _finish_startup(self) -> None:
        # Claim the app's identity with the shell, once, where the app is
        # actually being run. It belongs here rather than in notify.py because
        # notify is linked into the MCP server and the hourly backup as well,
        # and neither of those should be installing anything: they only ask
        # whether the registration is there (aumid.registered). Doing it here
        # means a machine where the window has ever run has the shortcut, and
        # every other process finds it ready.
        try:
            notify.log_line(f"toast identity: {aumid.ensure_shortcut()}")
        except Exception:
            pass  # an identity failure must not stop the window from opening

        self._make_tray()
        self.root.after(200, self._drain)
        # Started LAST, after everything it needs exists. A daemon thread, so a
        # window closed mid-check still exits: the subprocess it is inside is
        # killed with the process rather than holding it open.
        self._pr_thread = threading.Thread(
            target=self._pr_watch_loop, name="agentdesk-prwatch", daemon=True)
        self._pr_thread.start()
        self._poll()

    # --- the dispatcher ------------------------------------------------------

    def _worker_status(self) -> tuple:
        """(running, one-line description). Reads the heartbeat, checks the pid.

        Reports a dispatcher started from a shell exactly the same as one this
        window started, because the heartbeat is the worker's own file and not
        something the window wrote about itself.
        """
        try:
            state = json.loads(paths.WORKER_STATE.read_text(encoding="utf-8"))
        except Exception:
            return False, "Worker: not running"
        if not pid_alive(state.get("pid")):
            # The crashed case, said out loud rather than hidden: the file is
            # real, the process is not, and the next Start clears it.
            return False, "Worker: not running (stale heartbeat on disk)"
        item = state.get("item")
        doing = f"item #{item}" if item else "idle"
        return True, (f"Worker: running, {doing} "
                      f"[{state.get('agent')}/{state.get('mode')}]")

    def _toggle_worker(self) -> None:
        if self._worker_status()[0]:
            self._stop_worker()
        else:
            self._start_worker()
        # Re-read rather than assuming: the start is a subprocess that may fail,
        # and a button that says "running" when nothing is running is a lie the
        # next poll would only sometimes catch.
        self.root.after(400, self.refresh_now)

    def _start_worker(self) -> None:
        if self._worker_proc is not None and self._worker_proc.poll() is None:
            return
        # A stop file left over from the last run would stop the new one before
        # it did anything, which reads as a dead button.
        try:
            paths.WORKER_STOP.unlink()
        except OSError:
            pass
        flags = 0x08000000 if os.name == "nt" else 0  # no console window
        repo = Path(__file__).resolve().parent.parent
        try:
            # The crew, not the old one-shot dispatcher: one coordinator and
            # three long-lived agents (builder / verifier / researcher). It
            # writes the same heartbeat file and watches the same stop file, so
            # the button below needs no other change.
            self._worker_proc = subprocess.Popen(
                _self_argv("crew"),
                cwd=str(repo), creationflags=flags,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError as exc:
            messagebox.showerror("AgentDesk",
                                 f"Could not start the worker:\n\n{exc}")

    def _stop_worker(self) -> None:
        """Ask the dispatcher to stop rather than killing it.

        It reads this flag between items, so the item being worked right now
        finishes and is recorded properly. Killing the process instead would
        abandon a claim mid-edit -- the exact leak release_task exists to
        prevent, and it would be silly to reintroduce it from the UI.

        So Stop is not instant, and it is not meant to be: the button will keep
        saying "Stop worker" until the item finishes and the heartbeat goes.
        """
        try:
            paths.WORKER_STOP.parent.mkdir(parents=True, exist_ok=True)
            paths.WORKER_STOP.write_text(db.now_iso(), encoding="utf-8")
        except OSError as exc:
            messagebox.showerror("AgentDesk",
                                 f"Could not write the stop flag:\n\n{exc}")

    # --- reload -----------------------------------------------------------------
    #
    # "Rebuild in real time" (thread 35 / design in thread 82, section 5) turns
    # out to be two different problems. The back end is nearly free: every
    # intra-package import here is module-object style and no process holds a
    # connection across its life (see README.md's Architecture section), so a
    # fresh interpreter picks up new db.py/notify.py/etc. code with nothing to
    # invalidate. The front end is NOT free -- tearing down and rebuilding a
    # live widget tree costs the same as a relaunch while failing worse
    # (destroyed-but-referenced widgets, doubled `after` jobs) -- so this does
    # not attempt in-place UI reload at all. One relaunch gets both halves
    # correctly: a fresh process re-imports everything, backend and frontend
    # alike, and there is no window state worth preserving that outlives a
    # closed reply box.
    #
    # Gated exactly like Stop: refuses outright while the dispatcher holds a
    # claimed item, because relaunching this window while a build is mid-edit
    # is precisely the class of "reload something while it is working" the
    # design says must never happen to worker.py/crew.py. It does not matter
    # that the dispatcher runs in its own process and would survive this
    # window closing -- the design asks for the same gate here regardless, and
    # it costs nothing to honour: Stop, then Reload.
    def _reload_code(self) -> None:
        running, text = self._worker_status()
        if running:
            try:
                state = json.loads(paths.WORKER_STATE.read_text(encoding="utf-8"))
            except Exception:
                state = {}
            if state.get("item"):
                messagebox.showwarning(
                    "AgentDesk",
                    "The dispatcher is holding item #" + str(state["item"]) +
                    ".\n\nPress Stop worker and wait for it to finish that "
                    "item before reloading -- relaunching this window mid-item "
                    "is the one thing the reload design rules out.")
                return

        guard = getattr(self, "_instance_guard", None)
        if guard is not None:
            # Must happen BEFORE spawning the replacement: the guard is a
            # named mutex held by THIS process, and it is only released for
            # certain at process exit -- which has not happened yet at this
            # point in the call stack. Without releasing it explicitly first,
            # the new process's own SingleInstance can see this one as still
            # alive and decline to open a window at all.
            guard.release()

        flags = 0x08000000 if os.name == "nt" else 0
        repo = Path(__file__).resolve().parent.parent
        try:
            subprocess.Popen(
                _self_argv("app", ["--db", str(self.db_path)]),
                cwd=str(repo), creationflags=flags,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError as exc:
            messagebox.showerror("AgentDesk",
                                 f"Could not relaunch:\n\n{exc}")
            return
        self._quit()

    # --- the toast identity --------------------------------------------------
    #
    # There was a Test toast button here and, behind it, a _test_toast handler
    # that fired one and showed the result in a dialogue. Both are gone. The
    # button asked John the one question nothing in this process can answer --
    # whether the banner on his screen names AgentDesk -- and he answered it, so
    # the button went. The handler went with it because once the button was
    # gone the dialogue it displayed was reachable from nowhere: code that only
    # a removed button could reach is not a diagnostic, it is a liability.
    #
    # Firing a diagnostic toast is still possible, and better, without a window:
    # `python -m agentdesk.notify "Title" "Body"` prints the same report the
    # dialogue used to show -- which id Windows was handed, whether it accepted
    # it, and the registry key -- and exits non-zero if the toast could not be
    # sent at all. It works with the app closed, which the button could not.
    # notify.diagnostic_toast is the function behind both.

    # --- tray ----------------------------------------------------------------

    def _make_tray(self) -> None:
        # pystray runs its own event loop on a thread that is NOT the tkinter
        # thread. NOTHING in a pystray callback may touch a tkinter widget
        # directly: calls from the wrong thread either silently do nothing or
        # crash the interpreter with no traceback. Every callback therefore
        # only puts a callable on ui_queue; _drain, running on the tk main
        # loop via root.after, is the only place that executes them. This is
        # the bug somebody will otherwise reintroduce.
        menu = pystray.Menu(
            pystray.MenuItem("Open AgentDesk", self._on_tray_open, default=True),
            pystray.MenuItem("Reload code", self._on_tray_reload),
            pystray.MenuItem("Quit", self._on_tray_quit),
        )
        self.icon: Optional[pystray.Icon] = pystray.Icon(
            paths.APP_NAME, _tray_image(),
            title="AgentDesk - closing the window only hides it; Quit is here",
            menu=menu,
        )
        try:
            self.icon.run_detached()
        except Exception:
            # No tray available on this session: keep the window usable and
            # let closing it really close it, since there is no way back in.
            self.icon = None

    def _on_tray_open(self, icon: object, item: object) -> None:
        self.ui_queue.put(self._show_window)

    def _on_tray_quit(self, icon: object, item: object) -> None:
        self.ui_queue.put(self._quit)

    def _on_tray_reload(self, icon: object, item: object) -> None:
        # See _make_tray: a pystray callback runs on its own thread and may
        # never touch tkinter directly, so this only queues the real work for
        # _drain to run on the tk thread -- same rule, same reason, as Open
        # and Quit above.
        self.ui_queue.put(self._reload_code)

    def _drain(self) -> None:
        # See _make_tray: the only place tray-initiated work touches tkinter.
        while True:
            try:
                fn = self.ui_queue.get_nowait()
            except queue.Empty:
                break
            try:
                fn()
            except Exception:
                log.exception("ui callback failed")  # one bad callback must not stop the tray
        self.root.after(200, self._drain)

    def _show_window(self) -> None:
        self.root.deiconify()
        self.root.lift()
        self.root.focus_force()
        _flash_taskbar(self.root)

    def _hide_to_tray(self) -> None:
        if self.icon is None:
            # Without a tray icon, hiding would strand the window with no way
            # to bring it back or exit; a real quit is the honest behaviour.
            self._quit()
            return
        self.root.withdraw()

    def _quit(self) -> None:
        # Asked to stop before the window goes, so the watcher is not left
        # mid-`gh` with a destroyed root behind it. It is a daemon thread, so
        # this is tidiness rather than a correctness requirement -- the
        # process is going either way -- but it also means a check in flight
        # gets to finish its database write instead of being killed during it.
        self._stop_pr_watch()
        if self.icon is not None:
            try:
                self.icon.stop()
            except Exception:
                pass
        self.root.destroy()

    # --- poll ----------------------------------------------------------------

    USAGE_REFRESH_S = 300
    _usage_at = 0.0
    _usage_busy = False

    def _maybe_refresh_usage(self) -> None:
        """Every 5 minutes, re-read plan usage from the claude CLI on a worker thread (it takes ~10 s)."""
        import time
        if self._usage_busy or time.monotonic() - self._usage_at < self.USAGE_REFRESH_S:
            return
        self._usage_busy, self._usage_at = True, time.monotonic()

        def work():
            try:
                usage.refresh()
            except Exception:
                log.exception("usage refresh failed")
            finally:
                self._usage_busy = False
        threading.Thread(target=work, daemon=True, name="usage-refresh").start()

    def _poll(self) -> None:
        self._do_poll(reschedule=True)

    def refresh_now(self) -> None:
        """Refresh immediately, without adding a second polling chain."""
        self._do_poll(reschedule=False)

    def redraw(self) -> None:
        """Re-render from the database, whether or not anything changed in it.

        `refresh_now` is not this. The poll only redraws when its change
        signature moves -- a new message, the number of OPEN questions, the
        merge list -- and the reader can change what is on screen without
        changing any of those: switching the Active/Archived filter changes
        which rows are shown and nothing about the rows themselves, and
        bringing a question back moves it from archived to answered, which is
        neither a new message nor a change in the open count. Routed through
        the poll, both left the pane showing the other list until some
        unrelated message arrived, which reads as the button not working.

        So this skips the gate deliberately and renders unconditionally. It is
        the same escape the archive pass uses, for the same reason.
        """
        conn = None
        try:
            conn = db.connect(self.db_path)
            self._apply_changes(conn, db.open_questions(conn))
        except sqlite3.Error as exc:
            notify.log_line(f"redraw failed: {exc!r}")
        finally:
            if conn is not None:
                conn.close()

    def _do_poll(self, reschedule: bool) -> None:
        conn = None
        self._maybe_refresh_usage()
        try:
            conn = db.connect(self.db_path)
            row = conn.execute("SELECT MAX(id) FROM messages").fetchone()
            max_id = int(row[0]) if row and row[0] is not None else 0
            open_qs = db.open_questions(conn)
            # Read BEFORE _apply_changes, which flips the flag: the first poll
            # must seed the counters without offering the board's entire
            # history to the ack queue, or every old John reply on every old
            # thread would queue a receipt the moment the window starts.
            first = self._first_poll
            # The merge list is in the condition for the archive pass's reason:
            # the checker runs in another thread and settles PRs without
            # touching a message, so neither counter above would notice.
            prs_sig = self._prs_signature(conn)
            if (max_id != self.last_max_id
                    or len(open_qs) != self.last_open_count
                    or prs_sig != self.last_prs_sig):
                # _apply_changes reads from this same connection, so the close
                # has to come after it, and the two counters have to move after
                # it as well. Advancing them first means a failed redraw leaves
                # the window believing it is up to date, and no later tick ever
                # retries -- the panes simply stay empty.
                self._apply_changes(conn, open_qs)
            # The ack pass runs EVERY tick, changed or not: an agent going
            # quiet is not a new message, but it is what turns a queued
            # acknowledgement into the board's "nobody has picked this up"
            # note. It also gates the counters now, so a failed pass leaves
            # them alone and the same messages are offered again next tick
            # rather than being skipped for ever.
            self._ack_pass(conn, since_id=(None if first
                                           else self.last_max_id))
            # Also every tick, and for the ack pass's reason: the state it acts
            # on is reached by other processes as well as this one. It is a
            # write, so it is last of the three.
            archived = self._archive_pass(conn)
            if archived:
                # Archiving moves a thread's status, and neither of the two
                # counters above notices that: it is not a new message, and it
                # does not change how many questions are OPEN. So the redraw is
                # asked for explicitly -- without this the archived row would
                # sit on the Questions tab until something unrelated arrived.
                self._apply_changes(conn, open_qs)
            # Every tick, not behind the change gate: "last activity 4s ago" moves with
            # the clock, and a frozen "ago" would make a hung job look busy.
            self.view.tick(conn)
            self.last_max_id = max_id
            self.last_open_count = len(open_qs)
            self.last_prs_sig = prs_sig
        except sqlite3.Error as exc:
            # The database is shared with the MCP server and the hourly backup;
            # a transient failure waits for the next tick rather than bringing
            # the window down.
            notify.log_line(f"poll failed: {exc!r}")
        except Exception:
            log.exception("poll failed")
        finally:
            if conn is not None:
                conn.close()
            if reschedule:
                self.root.after(self.POLL_MS, self._poll)

    def _apply_changes(self, conn: sqlite3.Connection,
                       open_qs: list[dict]) -> None:
        current_ids = {q["thread_id"] for q in open_qs}
        if self._first_poll:
            # Questions that already existed when the app started are not
            # news; only announce ones that appear while it is running.
            self.announced.update(current_ids)
            self._first_poll = False
            new_qs: list[dict] = []
        else:
            new_qs = [q for q in open_qs
                      if q["thread_id"] not in self.announced]
            self.announced.update(current_ids)
        for q in new_qs:
            # Route through notify.toast, never self.icon.notify. toast() tries
            # the tray icon first and falls back to the PowerShell WinRT route,
            # logging every failure; calling the tray icon directly here skipped
            # all of that, and when _make_tray had left icon None the old bare
            # except ate the AttributeError -- so from outside, "toasts do not
            # work" and "the tray icon was never built" looked identical.
            # toast() never raises (its own contract), so no try here.
            # The subject goes in the MESSAGE and the author in the title, and
            # that way round is deliberate. pystray maps the title onto a
            # 64-code-unit field and the message onto a 256-unit one, so a
            # subject in the title raised ValueError for every question whose
            # subject was longer than 50 characters -- 64 minus "New question:
            # " -- which is nearly every question on this board. The toast then
            # fell back to a PowerShell subprocess and the failure was only
            # visible in the log. Swapping which fact goes in which field fixes
            # it without shortening either: subjects do not reach 256.
            notify.toast(f"New question from {identity.label(q['opened_by'])}",
                         q["subject"], icon=self.icon)
        self.root.title(window_title(self.db_path, len(open_qs)))
        self.view.refresh(conn, open_qs)

    # --- acknowledgements ------------------------------------------------------

    def _ack_pass(self, conn: sqlite3.Connection, since_id: Optional[int]) -> None:
        """The watcher half of acknowledgements. Queue new ones, speak about
        the ones nobody has picked up.

        This is the app's only ack job, and it has two halves on purpose. It
        QUEUES (a row in db.acks keyed by the human message - the once
        mechanism, safe against any number of poll ticks) and it NOTES (one
        message under paths.WATCHER when the agent has gone quiet). It never
        posts as the agent: the ack itself is delivered by the agent's own MCP
        session on its next board write, and forging that receipt here would
        be exactly the pretending the feature exists to avoid.

        `since_id=None` (first poll after launch) queues nothing - messages
        that predate the window are not news, the same rule as the toast
        seeding in _apply_changes. The accepted limit that follows: a reply
        posted while NO window was running is never queued, so it gets neither
        a receipt nor a note. Silence, not a false receipt; the thread still
        reads as unanswered, which is the true state.
        """
        if since_id is not None:
            rows = conn.execute(
                "SELECT id FROM messages WHERE id > ? ORDER BY id",
                (since_id,)).fetchall()
            for r in rows:
                db.queue_ack_for_message(conn, r["id"])
        for row in db.due_ack_notes(conn):
            agent = row["agent"]
            if row["last_seen_ts"]:
                quiet = (f"{agent} last used the board at "
                         f"{local_ts(row['last_seen_ts'])}")
            else:
                quiet = f"{agent} has no board activity on record"
            body = (f"No receipt yet - {quiet}. John's reply is recorded "
                    f"above, but has not been acknowledged: nothing is "
                    f"picking it up right now. If {agent} writes to the "
                    f"board again, the acknowledgement will appear here "
                    f"automatically.")
            posted = db.post_ack_note(
                conn, row["message_id"], paths.WATCHER, body,
                meta={"kind": "ack-note", "ack_for": row["message_id"]})
            if posted:
                notify.log_line(
                    f"ack note on thread #{row['thread_id']}: {agent} quiet, "
                    f"reply unacknowledged")

    # --- archiving settled questions -------------------------------------------

    def _archive_pass(self, conn: sqlite3.Connection) -> int:
        """Move settled questions into the vault, and off the Questions tab.

        Returns how many were archived THIS tick, which is what tells the caller
        a redraw is needed.

        It runs the whole sweep rather than the one thread that just changed,
        because the window is not the only thing that settles a question: John
        can answer in another instance, and a question can be closed by the CLI.
        The sweep is idempotent ('already-archived' writes nothing), so the cost
        of asking about all of them every tick is a query against an indexed
        column and no writes at all in the ordinary case.

        Failure is survivable by design. A vault that is unreachable or
        unwritable leaves the question answered and still on the tab, because
        vault.archive_question flips the status only after the transcript has
        landed -- so the question is retried on the next tick and by
        `python -m agentdesk.vault --archive-due` after that, rather than
        disappearing with nothing behind it.
        """
        try:
            results = vault.archive_due(conn)
        except Exception as exc:
            notify.log_line(f"question archive pass failed: {exc!r}")
            return 0
        done = 0
        for r in results:
            if r["status"] == "archived":
                done += 1
                notify.log_line(
                    f"question #{r['thread_id']} archived to {r['file']} "
                    f"({r['messages']} messages)")
            elif r["status"] == "refused" and r.get("parked_new"):
                # Logged once, when the refusal first appears. The condition
                # that caused it does not go away by itself, so a line per tick
                # would be the same sentence in the log for ever.
                notify.log_line(
                    f"question #{r['thread_id']} NOT archived: "
                    + "; ".join(r["reasons"])
                    + f" (written up at {r['parked_at']})")
        return done

    # --- the pull-request watcher ---------------------------------------------

    def check_prs_now(self) -> None:
        """Ask the watcher to run a pass immediately. Returns at once.

        Called from the button, so it must not block: it sets an event the
        watcher is already waiting on rather than doing the work here, which
        would freeze the window for a second per pull request.
        """
        self._pr_wake.set()

    def _pr_watch_loop(self) -> None:
        """The background half: ask GitHub about open PRs, forever.

        A thread and not part of _do_poll, because each check is a `gh`
        subprocess costing about a second and the poll runs every three. Doing
        it on the poll would freeze the window for the length of the check and
        spend an API call every three seconds for an answer that changes on the
        order of hours.

        It touches NOTHING in Tk. It writes to SQLite and the window picks the
        result up on its next redraw, which is the same way every other process
        on this board communicates with the UI. Reaching into a widget from
        here would be a crash rather than a glitch, and the one place it is
        tempting -- refreshing the list the instant a check lands -- is what
        _do_poll's signature check already covers.

        It dies with the window. That is a real limit and not a design win: PRs
        are only watched while AgentDesk is running, and `python -m
        agentdesk.prs --check-once` is what covers the rest.
        """
        while not self._pr_stop.is_set():
            # The first pass is sooner than the rest: opening the window to see
            # whether a PR you just merged has cleared is the moment the wait
            # is most visible.
            wait = (paths.PR_FIRST_CHECK_SECONDS if self._pr_first
                    else paths.PR_CHECK_SECONDS)
            self._pr_first = False
            self._pr_wake.wait(wait)
            self._pr_wake.clear()
            if self._pr_stop.is_set():
                break
            try:
                results = prs.check_due_path(self.db_path)
                line = prs.summarise(results)
                if line:
                    notify.log_line(line)
            except Exception as exc:
                # Never allowed to end the loop: a database that is briefly
                # locked, or a `gh` that misbehaves, must cost one pass and not
                # the feature.
                notify.log_line(f"pr check pass failed: {exc!r}")
            self._pr_scan_and_triage_pass()
            # After the except as well as the try: a pass that died still
            # finished, and a counter that stopped moving on the interesting
            # case would be worse than no counter. Read from outside the
            # thread, so it only ever moves forward.
            self.pr_passes += 1

    def _pr_scan_and_triage_pass(self) -> None:
        """Item #115 parts 2 and 3: catch un-registered PRs, then label them.

        Runs less often than the merge check (every PR_SCAN_SECONDS worth of
        merge-check ticks, tracked in whole passes rather than wall clock so
        it never drifts from the loop that drives it) because it costs more
        `gh` calls and the thing it is looking for -- "did GitHub notice me
        on something new" -- does not change minute to minute.

        Both halves are independently best-effort: pr_scan.scan_github and
        pr_scan.triage_open already swallow their own exceptions and return a
        summary rather than raise, but this wraps them anyway, because a bug
        in the summary logging itself must still cost one pass and not the
        merge-check line above it.
        """
        if self.pr_passes % max(1, paths.PR_SCAN_SECONDS // paths.PR_CHECK_SECONDS):
            return
        conn = db.connect(self.db_path)
        try:
            scanned = pr_scan.scan_github(conn)
            if scanned["gh_available"] and scanned["added"]:
                notify.log_line(
                    f"pr scan: {scanned['found']} found on github, "
                    f"{scanned['added']} added")
            triaged = pr_scan.triage_open(conn)
            if triaged["ladder_available"] and triaged["attempted"]:
                notify.log_line(
                    f"pr triage: {triaged['labelled']}/{triaged['attempted']} "
                    f"labelled by ladder")
        except Exception as exc:
            notify.log_line(f"pr scan/triage pass failed: {exc!r}")
        finally:
            conn.close()

    def _stop_pr_watch(self) -> None:
        self._pr_stop.set()
        self._pr_wake.set()  # so a sleeping watcher notices immediately

    def _prs_signature(self, conn: sqlite3.Connection) -> tuple:
        """The mutable state of every merge-list row, as one comparable value.

        It exists because the message-id counter cannot see a PR settling: the
        checker writes to pull_requests, and a merge with no thread to notify
        does not touch messages at all -- so without this, a merged PR would sit
        on the tab until something unrelated arrived. That is the same failure
        the archive pass had.

        Every mutable column, and not a MAX of a couple of them, which is what
        this was first. "newest settled, newest checked, row count" looks
        sufficient and is not: re-checking the SAME row inside one second
        leaves MAX(checked_ts) identical, so a check that fails and writes
        last_error changes the digest by nothing at all. The window then never
        redraws, and the last check is reported to John as the timestamp of an
        earlier one that succeeded -- a failed check that looks like a good
        one, which is the exact failure prs.py is built to prevent. The cost of
        being exact is a query over a table that holds the handful of PRs
        waiting on one person; the thing worth avoiding is the redraw, not the
        read.
        """
        rows = conn.execute(
            "SELECT id, state, checked_ts, settled_ts, last_error"
            " FROM pull_requests ORDER BY id").fetchall()
        return tuple((r["id"], r["state"], r["checked_ts"], r["settled_ts"],
                      r["last_error"]) for r in rows)

    # --- shared actions ------------------------------------------------------

    def post_reply(self, channel: str, thread_id: int, body: str) -> None:
        conn = db.connect(self.db_path)
        try:
            thread = db.get_thread(conn, thread_id)["thread"]
            db.reply(conn, thread_id, paths.HUMAN, paths.HUMAN_KIND, body)
            # A human reply is what stops a question asking: moving it out of
            # open is what lets go of the toast and the title count.
            #
            # Deliberately NOT followed by an archive here. Settling a question
            # and recording it in the vault are two steps, and the sweep above
            # is the only thing that does the second -- so the rule is "a
            # settled question ends up in the vault" rather than "a question
            # John answered while this window happened to be open does". The
            # tick between them is also what makes the answered-but-not-yet-
            # archived state visible, which is the state the tab is for.
            if thread["channel"] == "question" \
                    and thread["status"] == paths.STATUS_OPEN:
                db.set_thread_status(conn, thread_id, paths.STATUS_ANSWERED)
        finally:
            conn.close()
        self.refresh_now()

    def close_question(self, thread_id: int) -> None:
        """Close a question without answering it, then archive it.

        The explicit close the brief asks for, and it is John's alone: an agent
        replying must never close a question, because the toast repeats until
        John has dealt with it and an agent silencing that is the one thing the
        channel's invariant forbids. There is therefore no MCP tool for this and
        no code path from an agent's write to this method -- only the button.

        The archive is not done here either. This sets the status the sweep
        looks for, and the sweep files it, so the vault write has exactly one
        caller however the question got settled.
        """
        conn = db.connect(self.db_path)
        try:
            thread = db.get_thread(conn, thread_id)["thread"]
            if thread["channel"] != "question" \
                    or thread["status"] == paths.STATUS_ARCHIVED:
                return
            # The hold is cleared here because this is the one gesture that can
            # re-file a question that was brought back, and the sweep honours a
            # hold. Without this the button would set the status, the sweep
            # would skip the thread for ever, and the question would sit on the
            # tab looking closed and never reach the vault -- a dead end with no
            # error anywhere.
            db.set_thread_status(conn, thread_id, paths.STATUS_CLOSED,
                                 meta_updates={"archive_hold": None})
        finally:
            conn.close()
        self.refresh_now()

    def unarchive_question(self, thread_id: int) -> None:
        """Bring an archived question back onto the Questions tab.

        Deliberately does NOT post anything, and deliberately does not open the
        question. Restoring happens in one column, which is what makes it
        reversible: the vault file stays where it is, the transcript stays
        whole, and a second press of Close & archive re-files it against that
        same file.

        What it does not do is put it back in front of John as unanswered. The
        status it restores is the one it was settled with, so a question he
        answered and brought back still says answered -- the board does not
        start claiming he owes an answer he has already given, which is the
        state a reopen would produce.

        There is no MCP tool for this, for close_question's reason: un-archiving
        is re-opening the question of whether a settled thing is settled, and
        the toast repeats until John has dealt with it. An agent doing that on
        its own is the silencing the channel forbids.
        """
        conn = db.connect(self.db_path)
        try:
            if not db.unarchive_thread(conn, thread_id):
                return
            # A question that is on the tab and held is exactly the state the
            # Active filter is for, so the reader is sent to where the thing
            # they just restored now lives. Leaving the Archived filter on
            # would make the button look like it had deleted the row.
            self.view.show_archived = False
        finally:
            conn.close()
        # redraw, not refresh_now: archived -> answered is neither a new message
        # nor a change in how many questions are open, so the poll's gate would
        # skip it and the row would stay on the Archived list it no longer
        # belongs in.
        self.redraw()

    def run(self) -> None:
        self.root.mainloop()


def main(argv: Optional[list[str]] = None) -> int:
    _enable_dpi_awareness()
    parser = argparse.ArgumentParser(
        prog=paths.APP_NAME,
        description="The AgentDesk window.",
    )
    parser.add_argument(
        "--db", type=Path, default=paths.DB_PATH,
        help="database file to use (lets a second instance point at a copy "
             "for testing)",
    )
    args = parser.parse_args(argv)
    paths.ensure_dirs()

    guard = SingleInstance(args.db)
    if guard.already_running:
        raised = guard.raise_existing()
        print("AgentDesk is already open"
              + (" - bringing that window forward." if raised else
                 "; its window could not be raised from here, so it is in the "
                 "taskbar or the notification area."), file=sys.stderr)
        # Zero, not one. The user asked for the window and the window is there;
        # the second copy declining to make a duplicate is this program
        # working, and a shortcut or launcher should not report a failure.
        return 0

    app = App(args.db)
    # Real launches only; check scripts build App directly and skip both.
    dictate.warm()
    app.dictation.enable_preroll(bool(settings.load().get("preroll", True)))
    # Attached so _reload_code can release it before spawning a replacement
    # process -- see SingleInstance.release(). A plain attribute, not a
    # constructor argument: App is also built directly by the check scripts,
    # none of which reload themselves, and giving them all a guard to thread
    # through for a feature they never use would be the wrong trade.
    app._instance_guard = guard
    app.run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
