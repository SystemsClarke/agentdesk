"""The AgentDesk window: three channel pages, a tray icon, and a 3-second poll.

Run with pythonw.exe, so nothing here may require a console. The database is
shared with the MCP server and the hourly backup, so every read and write
below opens a short-lived connection and closes it again; no connection is
held open across the window's lifetime.
"""

from __future__ import annotations

import argparse
import ctypes
import queue
import sqlite3
import sys
import tkinter as tk
from tkinter import ttk
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

import pystray
from PIL import Image, ImageDraw

# pythonw runs a file as a plain script, where there is no package context for
# a relative import; fall back to putting the repo root on the path.
try:
    from agentdesk import db, notify, paths
except ImportError:  # pragma: no cover - depends on how the file was launched
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from agentdesk import db, notify, paths


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


def _tray_image() -> Image.Image:
    # A drawn icon, so the package never needs an image file on disk.
    img = Image.new("RGBA", (64, 64), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((4, 4, 60, 60), radius=12, fill=(40, 44, 52, 255))
    d.rounded_rectangle((14, 15, 50, 39), radius=6, fill=(235, 238, 242, 255))
    d.polygon([(20, 37), (30, 37), (17, 50)], fill=(235, 238, 242, 255))
    d.rounded_rectangle((20, 22, 44, 25), radius=1, fill=(40, 44, 52, 255))
    d.rounded_rectangle((20, 29, 38, 32), radius=1, fill=(40, 44, 52, 255))
    return img


class Page(ttk.Frame):
    """One channel: thread list on the left, selected thread on the right."""

    def __init__(self, master: tk.Misc, app: "App", channel: str) -> None:
        super().__init__(master)
        self.app = app
        self.channel = channel
        self.thread_id: Optional[int] = None
        self.shown_count = -1
        self.rows: list[dict] = []
        # Guards _on_select while the tree is rebuilt programmatically, so a
        # refresh does not re-read and re-render the thread under the reader.
        self._suppress_select = False
        # The row tuples currently in the tree, so an unchanged refresh can be
        # skipped instead of rebuilding the list and losing the scroll position.
        self._rendered: list = []

        top = ttk.Frame(self)
        top.pack(fill="x", padx=6, pady=(6, 0))
        verb = "Ask a question..." if channel == "question" else "New thread..."
        ttk.Button(top, text=verb, command=lambda: app.compose(channel)).pack(
            side="right"
        )
        if channel == "question":
            ttk.Label(
                top, text="Open questions show in red and count in the title.",
                foreground="#8a6d3b",
            ).pack(side="left")

        pane = ttk.PanedWindow(self, orient="horizontal")
        pane.pack(fill="both", expand=True, padx=6, pady=6)

        left = ttk.Frame(pane)
        cols = ("open", "subject", "by", "updated", "msgs")
        self.tree = ttk.Treeview(left, columns=cols, show="headings",
                                 selectmode="browse")
        for key, label in (
            ("open", ""),
            ("subject", "Subject"),
            ("by", "Opened by"),
            ("updated", "Last message"),
            ("msgs", "Msgs"),
        ):
            self.tree.heading(key, text=label)
        self.tree.column("open", width=44, anchor="center", stretch=False)
        self.tree.column("subject", width=260, anchor="w")
        self.tree.column("by", width=80, anchor="w", stretch=False)
        self.tree.column("updated", width=112, anchor="w", stretch=False)
        self.tree.column("msgs", width=40, anchor="e", stretch=False)
        ysb = ttk.Scrollbar(left, orient="vertical", command=self.tree.yview)
        self.tree.configure(yscrollcommand=ysb.set)
        self.tree.pack(side="left", fill="both", expand=True)
        ysb.pack(side="left", fill="y")
        self.tree.tag_configure("isopen", foreground="#c0392b")
        self.tree.bind("<<TreeviewSelect>>", self._on_select)
        pane.add(left, weight=3)

        right = ttk.Frame(pane)
        self.subject_lbl = ttk.Label(right, text="", font=("", 11, "bold"),
                                     wraplength=520, justify="left")
        self.subject_lbl.pack(fill="x", pady=(0, 4))

        # The reply area is packed before the message view so it keeps a fixed
        # height at the bottom and the messages take the remaining space.
        reply = ttk.Frame(right)
        reply.pack(side="bottom", fill="x", pady=(4, 0))
        inner = ttk.Frame(reply)
        inner.pack(fill="x")
        self.reply_txt = tk.Text(inner, height=4, wrap="word", relief="solid",
                                 borderwidth=1)
        self.reply_txt.grid(row=0, column=0, sticky="ew")
        rsb = ttk.Scrollbar(inner, orient="vertical",
                            command=self.reply_txt.yview)
        self.reply_txt.configure(yscrollcommand=rsb.set)
        rsb.grid(row=0, column=1, sticky="ns")
        inner.columnconfigure(0, weight=1)
        self.post_btn = ttk.Button(
            reply, text="Answer" if channel == "question" else "Post",
            command=self._post,
        )
        self.post_btn.pack(anchor="e", pady=(4, 0))
        ttk.Label(reply, text=f"Reply as {paths.HUMAN} - Ctrl+Enter sends"
                  ).pack(anchor="w")
        self.reply_txt.bind("<Control-Return>", self._on_ctrl_return)

        msgframe = ttk.Frame(right)
        msgframe.pack(fill="both", expand=True)
        self.msgs_txt = tk.Text(msgframe, wrap="word", state="disabled",
                                relief="flat", padx=6, pady=4)
        msb = ttk.Scrollbar(msgframe, orient="vertical",
                            command=self.msgs_txt.yview)
        self.msgs_txt.configure(yscrollcommand=msb.set)
        self.msgs_txt.pack(side="left", fill="both", expand=True)
        msb.pack(side="left", fill="y")
        self.msgs_txt.tag_configure("meta", foreground="#777777")
        self.msgs_txt.tag_configure("body", lmargin1=12, lmargin2=12)
        pane.add(right, weight=7)

    # --- reads ---------------------------------------------------------------

    def refresh_list(self, conn: sqlite3.Connection) -> None:
        rows = db.list_threads(conn, channel=self.channel, limit=200)
        self.rows = rows
        # Build the whole table in memory first, so an unchanged refresh can be
        # skipped entirely. Rebuilding the tree widget drops the scrollbar
        # position and flashes the list, and a poll that found nothing new has
        # no business doing either.
        wanted = []
        for r in rows:
            is_open = r["status"] == paths.STATUS_OPEN
            wanted.append((str(r["id"]), ("isopen",) if is_open else (),
                           ("open" if is_open else "", r["subject"],
                            r["opened_by"], local_ts(r["updated_ts"]),
                            r["message_count"])))
        if wanted == self._rendered:
            return

        self._suppress_select = True
        try:
            keep = self.thread_id
            at = self.tree.yview()
            self.tree.delete(*self.tree.get_children())
            for iid, tags, values in wanted:
                self.tree.insert("", "end", iid=iid, tags=tags, values=values)
            # Re-select the thread being read, or the view loses its place on
            # every refresh even though nothing about it changed.
            if keep is not None and self.tree.exists(str(keep)):
                self.tree.selection_set(str(keep))
            # Restore where the reader had scrolled to. see() would drag the
            # view to the selected row instead, which reads as the list
            # jumping under the cursor whenever any message anywhere lands.
            self.tree.yview_moveto(at[0])
        finally:
            self._suppress_select = False
            self._rendered = wanted

    def refresh_detail(self, conn: sqlite3.Connection) -> None:
        if self.thread_id is None:
            return
        try:
            data = db.get_thread(conn, self.thread_id)
        except ValueError:
            # The thread vanished (another process closed and it was pruned,
            # or a --db copy was swapped underneath us). Clear the pane.
            self.thread_id = None
            self.shown_count = -1
            self.subject_lbl.config(text="")
            return
        thread, msgs = data["thread"], data["messages"]
        if len(msgs) == self.shown_count:
            return  # nothing new; do not repaint or move the scroll position
        self.subject_lbl.config(text=thread["subject"])
        grew = 0 <= self.shown_count < len(msgs)
        self.shown_count = len(msgs)
        self.msgs_txt.config(state="normal")
        self.msgs_txt.delete("1.0", "end")
        for m in msgs:
            who = f"{m['author']} ({m['author_kind']})   {local_ts(m['ts'])}"
            self.msgs_txt.insert("end", who + "\n", ("meta",))
            self.msgs_txt.insert("end", m["body"] + "\n\n", ("body",))
        self.msgs_txt.config(state="disabled")
        # Jump to the bottom only when a message actually arrived; a repaint
        # of unchanged content should not move the reader.
        if grew:
            self.msgs_txt.see("end-1c")

    # --- writes --------------------------------------------------------------

    def _on_select(self, _event: Optional[tk.Event] = None) -> None:
        if self._suppress_select:
            return
        sel = self.tree.selection()
        if not sel:
            return
        self.thread_id = int(sel[0])
        self.shown_count = -1  # force a render even if the count matches
        conn = db.connect(self.app.db_path)
        try:
            self.refresh_detail(conn)
        finally:
            conn.close()

    def _on_ctrl_return(self, _event: Optional[tk.Event] = None) -> str:
        self._post()
        return "break"  # keep the newline out of the reply box

    def _post(self) -> None:
        body = self.reply_txt.get("1.0", "end-1c").strip()
        if not body or self.thread_id is None:
            return
        self.app.post_reply(self.channel, self.thread_id, body)
        self.reply_txt.delete("1.0", "end")


class App:
    POLL_MS = 3000

    def __init__(self, db_path: Path) -> None:
        self.db_path = db_path
        self.ui_queue: "queue.Queue[Callable[[], None]]" = queue.Queue()
        self.last_max_id = -1
        self.last_open_count = -1
        self.announced: set[int] = set()
        self._first_poll = True

        # Idempotent, and needed for a --db test copy that starts empty.
        conn = db.connect(self.db_path)
        try:
            db.init_db(conn)
        finally:
            conn.close()

        self.root = tk.Tk()
        self.root.title("AgentDesk")
        self.root.geometry("1000x640")
        self.root.minsize(720, 420)
        self.root.protocol("WM_DELETE_WINDOW", self._hide_to_tray)

        labels = {"question": "Questions", "discussion": "Discussion",
                  "wiki": "Wiki"}
        nb = ttk.Notebook(self.root)
        nb.pack(fill="both", expand=True)
        self.pages: dict[str, Page] = {}
        for ch in paths.CHANNELS:
            page = Page(nb, self, ch)
            nb.add(page, text=labels[ch])
            self.pages[ch] = page

        self._make_tray()
        self.root.after(200, self._drain)
        self._poll()

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

    def _drain(self) -> None:
        # See _make_tray: the only place tray-initiated work touches tkinter.
        while True:
            try:
                fn = self.ui_queue.get_nowait()
            except queue.Empty:
                break
            try:
                fn()
            except tk.TclError:
                break  # the window is gone; there is nothing left to update
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
        if self.icon is not None:
            try:
                self.icon.stop()
            except Exception:
                pass
        self.root.destroy()

    # --- poll ----------------------------------------------------------------

    def _poll(self) -> None:
        self._do_poll(reschedule=True)

    def refresh_now(self) -> None:
        """Refresh immediately, without adding a second polling chain."""
        self._do_poll(reschedule=False)

    def _do_poll(self, reschedule: bool) -> None:
        conn = None
        try:
            conn = db.connect(self.db_path)
            row = conn.execute("SELECT MAX(id) FROM messages").fetchone()
            max_id = int(row[0]) if row and row[0] is not None else 0
            open_qs = db.open_questions(conn)
            if (max_id != self.last_max_id
                    or len(open_qs) != self.last_open_count):
                # _apply_changes reads from this same connection, so the close
                # has to come after it, and the two counters have to move after
                # it as well. Advancing them first means a failed redraw leaves
                # the window believing it is up to date, and no later tick ever
                # retries -- the panes simply stay empty.
                self._apply_changes(conn, open_qs)
                self.last_max_id = max_id
                self.last_open_count = len(open_qs)
        except sqlite3.Error as exc:
            # The database is shared with the MCP server and the hourly backup;
            # a transient failure waits for the next tick rather than bringing
            # the window down.
            notify.log_line(f"poll failed: {exc!r}")
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
            try:
                self.icon.notify(f"from {q['opened_by']}",
                                 title=f"New question: {q['subject']}")
            except Exception:
                pass  # notify is best-effort; the title count still shows
        count = len(open_qs)
        noun = "question" if count == 1 else "questions"
        self.root.title(f"AgentDesk - {count} open {noun}" if count
                        else "AgentDesk")
        for page in self.pages.values():
            page.refresh_list(conn)
            page.refresh_detail(conn)

    # --- shared actions ------------------------------------------------------

    def post_reply(self, channel: str, thread_id: int, body: str) -> None:
        conn = db.connect(self.db_path)
        try:
            thread = db.get_thread(conn, thread_id)["thread"]
            db.reply(conn, thread_id, paths.HUMAN, paths.HUMAN_KIND, body)
            # A human reply is what stops a question asking: moving it out of
            # open is what lets go of the toast and the title count.
            if thread["channel"] == "question" \
                    and thread["status"] == paths.STATUS_OPEN:
                db.set_thread_status(conn, thread_id, paths.STATUS_ANSWERED)
        finally:
            conn.close()
        self.refresh_now()

    def compose(self, channel: str) -> None:
        win = tk.Toplevel(self.root)
        win.title(f"New {channel} thread")
        win.transient(self.root)

        ttk.Label(win, text="Subject").pack(fill="x", padx=8, pady=(8, 0))
        subject_ent = ttk.Entry(win)
        subject_ent.pack(fill="x", padx=8)
        ttk.Label(win, text="Message").pack(fill="x", padx=8, pady=(8, 0))
        body_txt = tk.Text(win, height=6, wrap="word", relief="solid",
                           borderwidth=1)
        body_txt.pack(fill="both", expand=True, padx=8)

        def submit() -> None:
            subject = subject_ent.get().strip() or "(no subject)"
            body = body_txt.get("1.0", "end-1c").strip()
            if not body:
                return
            conn = db.connect(self.db_path)
            try:
                db.start_thread(conn, channel, subject, paths.HUMAN,
                                paths.HUMAN_KIND, body)
            finally:
                conn.close()
            win.destroy()
            self.refresh_now()

        btns = ttk.Frame(win)
        btns.pack(fill="x", padx=8, pady=8)
        ttk.Button(btns, text="Cancel", command=win.destroy).pack(side="right")
        ttk.Button(btns, text="Post", command=submit).pack(side="right",
                                                           padx=(0, 8))
        body_txt.bind("<Control-Return>", lambda e: (submit(), "break")[1])
        subject_ent.focus_set()

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
    App(args.db).run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
