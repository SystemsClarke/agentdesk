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
import os
import queue
import sqlite3
import subprocess
import sys
import threading
import tkinter as tk
import tkinter.font as tkfont
import webbrowser
from tkinter import messagebox, ttk
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

import pystray
from PIL import Image, ImageDraw

# pythonw runs a file as a plain script, where there is no package context for
# a relative import; fall back to putting the repo root on the path.
try:
    from agentdesk import (aumid, db, identity, mdview, notify, paths, pr_scan,
                          prs, vault)
except ImportError:  # pragma: no cover - depends on how the file was launched
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from agentdesk import (aumid, db, identity, mdview, notify, paths, pr_scan,
                          prs, vault)


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


def _clock(iso: str) -> str:
    """Just the local time of day. For a list of things that happened today."""
    try:
        dt = datetime.fromisoformat(iso)
    except (TypeError, ValueError):
        return "--:--:--"
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone().strftime("%H:%M:%S")


def _ago(iso: str) -> str:
    """How long ago that was, in words. "" when the timestamp is unreadable.

    Relative and not absolute, because the question the queue's activity panel
    is asking is not "when" but "is this moving": "last activity 4s ago" and
    "last activity 22m ago" say different things about whether to worry, and
    the clock time alone leaves the reader doing the subtraction.
    """
    try:
        dt = datetime.fromisoformat(iso)
    except (TypeError, ValueError):
        return ""
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    secs = (datetime.now(timezone.utc) - dt).total_seconds()
    # A clock skew, or a row written with a timestamp a moment in the future,
    # must not render as "-3s ago".
    secs = max(secs, 0)
    if secs < 60:
        return f"{int(secs)}s ago"
    if secs < 3600:
        return f"{int(secs // 60)}m ago"
    if secs < 86400:
        return f"{int(secs // 3600)}h ago"
    return f"{int(secs // 86400)}d ago"


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
_STATE_LOOK = {
    paths.STATUS_OPEN: ("open", "st-open", "#c0392b"),       # waiting on John
    paths.STATUS_CLAIMED: ("claimed", "st-claimed", "#8a6d3b"),
    paths.STATUS_DONE: ("done", "st-done", "#777777"),
    paths.STATUS_ANSWERED: ("answered", "st-answered", "#777777"),
    paths.STATUS_CLOSED: ("closed", "st-closed", "#777777"),
    paths.STATUS_FYI: ("fyi", "st-fyi", "#777777"),
    # Only ever reached through the detail pane or a --db copy, because the
    # Questions tab filters archived threads out. It is here so that a thread
    # which IS archived does not fall through to the raw-status fallback below
    # and render as the word "archived" in body text -- which is what "not in
    # the table" means, and it would read as an error rather than as settled.
    paths.STATUS_ARCHIVED: ("archived", "st-archived", "#777777"),
}
# Work's "open" is not the Questions "open": a fresh task is available, not
# waiting on anybody, and colouring it the red of an unanswered question would
# put the loudest colour on the one thing that needs nothing from the reader.
_READY = ("open", "st-ready", "#0b5394")

# How a background agent's progress line is coloured, by db.WORK_* kind. The
# colour is doing the job the word "error" would otherwise do alone: a run
# that stopped has to be visible while skimming, because the whole reason to
# want this panel is to catch the case that went wrong. Steps are near-black
# and the agent's own prose is grey -- the steps are the answer to "what is it
# doing", the prose is colour around it.
_EVENT_LOOK = {
    db.WORK_START: "#0b5394",
    db.WORK_STEP: "#24292f",
    db.WORK_OUTPUT: "#6a737d",
    db.WORK_DONE: "#2e7d32",
    db.WORK_ERROR: "#c0392b",
}


def _event_tag(kind: str) -> str:
    """The tag name for an event kind. Unknown kinds get the plain-black one.

    A kind the window does not know is not an error: the worker is a separate
    process on a separate upgrade path, and a board where a new kind of event
    crashed the redraw would be worse than one that renders it plainly.
    """
    return f"ev-{kind}" if kind in _EVENT_LOOK else "ev-unknown"


def activity_text(row: Optional[dict], events: list) -> tuple:
    """What the queue's progress panel says: (label, lines).

    A function of the row and its events alone, with no widget anywhere near
    it, so what the panel SAYS can be asserted without building a window --
    and so the phrasing that matters is in one readable place rather than
    assembled inline among pack() calls.

    `lines` is a list of (clock, body, tag) and the label is the one-line
    summary above them.
    """
    if row is None:
        return "No item selected.", []
    word, _tag, _colour = state_of("work", row)
    who = holder(row)
    held = f"held by {identity.label(who)}" if who else ""

    if not events:
        # Nothing recorded, and the two reasons for that must not read the
        # same. The dispatcher records as it runs, so silence means either
        # that nothing has taken the item, or that it was taken through the
        # board by an agent working by hand -- which this panel has no window
        # into, and pretending to watch it would be the exact dishonesty the
        # panel exists to remove.
        if row["status"] == paths.STATUS_CLAIMED:
            return ("   ·   ".join(x for x in
                                   (word, held, "no progress reported") if x),
                    [("", "Nothing has reported progress on this item.",
                      "ev-unknown"),
                     ("", "Progress is recorded by the dispatcher while it runs "
                          "an item. One claimed straight from the board, by an "
                          "agent working by hand, has nothing here.", "ev-unknown")])
        return "   ·   ".join(x for x in (word, "nothing has run this item") if x), []

    # The first event of kind 'start' rather than the first event outright, so
    # the clock starts when the item stopped being available.
    started = next((e for e in events if e["kind"] == db.WORK_START), events[0])
    last = events[-1]
    bits = [word, held,
            f"started {_ago(started['ts'])}",
            f"last activity {_ago(last['ts'])}"]
    # A failed run says so in the label and not only in the list below it.
    # This is the case the label most needs to get right, and it is the one it
    # would otherwise get wrong: a released item goes back to 'open', so the
    # status word alone reads as "available to take" -- which is true, and
    # says nothing about the fact that something just died on it.
    if last["kind"] == db.WORK_ERROR:
        bits.append(last["body"])
    label = "   ·   ".join(x for x in bits if x)
    lines = [(_clock(e["ts"]), e["body"], _event_tag(e["kind"])) for e in events]
    return label, lines


def waiting_on_john(channel: str, row) -> bool:
    """Does this row owe John an answer?

    For a question that is a fact about its messages, and db walks it for the
    view, the tray count and this -- so the red row, the tab's count and the
    toast cannot disagree about the same thread. db.list_threads and
    db.get_thread carry it as a `waiting` column for exactly that reason.

    A row from somewhere that does not select the column answers False rather
    than raising: sqlite3.Row and dict disagree about how a missing key is
    spelled, and this runs inside the poll loop, where an exception on one row
    is a window that stops refreshing.
    """
    if channel != "question":
        return False
    try:
        return bool(row["waiting"])
    except (IndexError, KeyError, TypeError):
        return False


def state_of(channel: str, row: dict) -> tuple:
    """(word, tag, colour) for one thread's state, as this channel means it."""
    if channel == "work" and row["status"] == paths.STATUS_OPEN:
        return _READY
    # A question reads by whether it is waiting, not by its stored status. The
    # two came apart when waiting stopped being `status='open'`: a thread John
    # answered once that an agent has since asked again in is stored
    # 'answered', and drawing that as a settled row is drawing the one row on
    # the board that needs him as the one that does not.
    if waiting_on_john(channel, row):
        return _STATE_LOOK[paths.STATUS_OPEN]
    return _STATE_LOOK.get(row["status"], (row["status"], "st-other", "#333333"))


# What each column is called on each tab. The column POSITIONS are the same
# everywhere on purpose -- the identity tests index the values tuple by
# position -- so only the headings and what goes in them change.
_LIST_LOOK = {
    "question": {"state": "State", "by": "Opened by"},
    "discussion": {"state": "State", "by": "Opened by"},
    "wiki": {"state": "State", "by": "Posted by"},
    "work": {"state": "State", "by": "Held by"},
}

# Said once, above the list, in the project's own words from the README's
# channel table. The Questions tab always had one; the other three were
# unlabelled, so a reader arriving on Wiki had nothing telling them it is
# browsed rather than written in.
_CHANNEL_BLURB = {
    # "answerable by any agent" is new and is the reason the sentence says what
    # does NOT close one: an agent replying is now a normal thing to see here,
    # so "it has replies" must not read as "it is dealt with". Only John's reply
    # or an explicit close settles it, and settling is what files it in the
    # vault and takes it off this tab.
    "question": "An agent asking John, and answerable by any agent. Still "
                "waiting on him until he replies -- an agent's reply does not "
                "close one. Open ones are red and counted in the title; a "
                "settled one is archived to the vault and leaves this tab.",
    "discussion": "Agents talking to each other. Visible to John, addressed "
                  "to nobody in particular.",
    "wiki": "Long-lived knowledge. Browsed, not chatted in.",
    "work": "The queue. Open is available to take, claimed is in flight, "
            "done is finished and reported.",
}

_CHANNEL_LABEL = {
    "question": "Question", "discussion": "Discussion",
    "wiki": "Wiki", "work": "Work item",
}

_NO_SELECTION = "Select a thread on the left to read it."


def holder(row: dict) -> str:
    """The agent holding a work item, or "" -- from meta, which is JSON text."""
    try:
        return json.loads(row.get("meta") or "{}").get("assignee") or ""
    except (TypeError, ValueError):
        return ""


def _wrap_of(widget: tk.Misc) -> int:
    """A label's wraplength as an int. Tk returns '' for never-set, not 0.

    Worth a function: the comparison it feeds is what keeps a <Configure>
    handler from being an infinite resize loop, and `int('')` raising inside
    that handler left the two labels unwrapped and a traceback per resize.
    """
    try:
        return int(widget.cget("wraplength") or 0)
    except (TypeError, ValueError):
        return 0


def fonts_for(root: tk.Misc) -> dict:
    """The window's named fonts, configured once per toplevel and cached.

    Cached on the root widget because a font object that nothing references is
    collected and the widget that was using it then renders in the default --
    a bug that shows up as "the font works until it doesn't". Doing it here
    rather than in App means a Page built on its own (the identity check does
    exactly that) gets the same look without needing an App around it.
    """
    cached = getattr(root, "_agentdesk_fonts", None)
    if cached is not None:
        return cached
    style = ttk.Style(root)
    base = tkfont.nametofont("TkDefaultFont")
    family, size = base.actual("family"), base.actual("size")
    fonts = {
        "subject": tkfont.Font(root=root, family=family, size=size + 1,
                               weight="bold"),
        "meta": tkfont.Font(root=root, family=family, size=max(size - 1, 7)),
    }
    # Everything below is something that was simply never set. The theme is
    # deliberately left alone -- John asked to keep the old school look, so
    # this is vista/whatever the desktop uses, with a taller row so the list
    # does not read as a spreadsheet, a bold heading so the columns have a
    # header rather than a line of grey text, and named styles instead of
    # per-widget colours so the muted things are muted the same way.
    try:
        style.configure("Treeview", rowheight=size + 13)
        style.configure("Treeview.Heading", font=(family, size, "bold"))
    except tk.TclError:
        pass  # a theme that refuses these is not worth failing to start over
    style.configure("Banner.TLabel", foreground="#8a6d3b")
    style.configure("Footer.TLabel", foreground="#666666")
    root._agentdesk_fonts = fonts
    return fonts


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
        # Set by the Configure handler when a resize changes what the pane can
        # hold, and cleared by the repaint that answers it. It exists because
        # the poll's own change gate compares message counts and so cannot see a
        # resize at all -- see _pane_needs_repaint.
        self.detail_stale = False
        self.rows: list[dict] = []
        # Guards _on_select while the tree is rebuilt programmatically, so a
        # refresh does not re-read and re-render the thread under the reader.
        self._suppress_select = False
        # The row tuples currently in the tree, so an unchanged refresh can be
        # skipped instead of rebuilding the list and losing the scroll position.
        self._rendered: list = []
        # The same idea for the queue's activity panel, and split in two for
        # the reason _show_activity gives: the label ticks with the clock, the
        # lines only move when an event lands.
        self._activity_label: Optional[str] = None
        self._activity_lines: list = []
        self._hint_shown = False
        fonts = fonts_for(self.winfo_toplevel())

        top = ttk.Frame(self)
        top.pack(fill="x", padx=8, pady=(8, 0))
        verb = "Ask a question..." if channel == "question" else "New thread..."
        ttk.Button(top, text=verb, command=lambda: app.compose(channel)).pack(
            side="right"
        )
        # The Archived filter, and it exists only on Questions for the same
        # reason Close & archive does: no other channel has a settled/archived
        # distinction to offer. Not a checkbox -- a checkbox labelled "Archived"
        # leaves it open whether it means "also show archived" or "only show
        # archived", and the two lists are different enough that guessing wrong
        # looks like the archive having lost the thread. A pair of radios says
        # which list is on the screen.
        self.archived_filter = None
        if channel == "question":
            self.show_archived = tk.BooleanVar(value=False)
            self.archived_filter = ttk.Frame(top)
            self.archived_filter.pack(side="left")
            ttk.Label(self.archived_filter, text="Show:",
                      font=fonts["meta"]).pack(side="left", padx=(0, 4))
            for label, wanted in (("Active", False), ("Archived", True)):
                ttk.Radiobutton(
                    self.archived_filter, text=label, value=wanted,
                    variable=self.show_archived, command=self._on_archived_filter,
                ).pack(side="left", padx=(0, 6))
        else:
            # Kept as a plain False so refresh_list has no branch to get wrong.
            self.show_archived = tk.BooleanVar(value=False)
        self.banner = ttk.Label(self, text=_CHANNEL_BLURB[channel],
                                style="Banner.TLabel", font=fonts["meta"],
                                justify="left", anchor="w")
        self.banner.pack(fill="x", padx=8, pady=(4, 0))
        # Wrap to the pane rather than to a fixed pixel count, so the sentence
        # does not run off the edge at the window's minimum width. Bound to the
        # page and only written when the number actually changed: setting
        # wraplength resizes the label, and a handler that always writes would
        # be a resize loop.
        def _rewrap(event: tk.Event) -> None:
            want = max(event.width - 20, 200)
            if _wrap_of(self.banner) != want:
                self.banner.config(wraplength=want)
        self.bind("<Configure>", _rewrap)

        pane = ttk.PanedWindow(self, orient="horizontal")
        pane.pack(fill="both", expand=True, padx=8, pady=(6, 2))

        left = ttk.Frame(pane)
        cols = ("open", "subject", "by", "updated", "msgs")
        self.tree = ttk.Treeview(left, columns=cols, show="headings",
                                 selectmode="browse")
        look = _LIST_LOOK[channel]
        for key, label in (
            ("open", look["state"]),
            ("subject", "Subject"),
            ("by", look["by"]),
            ("updated", "Last message"),
            ("msgs", "Msgs"),
        ):
            self.tree.heading(key, text=label)
        # The first column was 44px and blank-headed, which fits the single
        # character it used to hold and nothing else. It now carries a word, so
        # it is wide enough for the longest one ("answered", 8 characters).
        self.tree.column("open", width=70, minwidth=56, anchor="center",
                         stretch=False)
        # Subject is the only stretching column, so the pane's leftover pixels
        # all land here and this number is a starting point rather than a size.
        # The five columns used to ask for 536px inside a 483px list, which cut
        # the last one (Msgs) off with no horizontal scrollbar to reach it; the
        # reason the list was that narrow is the pair of Text widgets below,
        # not these numbers. minwidth is what lets subject give ground at the
        # 720px minimum window instead of pushing Msgs off the edge again.
        self.tree.column("subject", width=150, minwidth=80, anchor="w")
        # Unchanged, and deliberately so -- an identity check measures this
        # column's width. It is sized for its own "Opened by" heading, and a
        # stored author can be far longer than that
        # ("claude-code:AgentDesk#3f2a", which renders as "Agent...#3f2a").
        # The full identity belongs in the detail pane, which has the room.
        self.tree.column("by", width=80, minwidth=60, anchor="w", stretch=False)
        self.tree.column("updated", width=112, minwidth=96, anchor="w",
                         stretch=False)
        self.tree.column("msgs", width=40, minwidth=34, anchor="e",
                         stretch=False)
        ysb = ttk.Scrollbar(left, orient="vertical", command=self.tree.yview)
        self.tree.configure(yscrollcommand=ysb.set)
        self.tree.pack(side="left", fill="both", expand=True)
        ysb.pack(side="left", fill="y")
        for _word, tag, colour in list(_STATE_LOOK.values()) + [_READY]:
            self.tree.tag_configure(tag, foreground=colour)
        # Kept as well as the state tag, and the same colour on purpose: it is
        # the name the old behaviour was documented under, and two tags that
        # agree cannot fight over which one wins.
        self.tree.tag_configure("isopen", foreground="#c0392b")
        self.tree.bind("<<TreeviewSelect>>", self._on_select)
        pane.add(left, weight=3)

        right = ttk.Frame(pane)
        self.subject_lbl = ttk.Label(right, text="", font=fonts["subject"],
                                     wraplength=520, justify="left",
                                     anchor="w")
        self.subject_lbl.pack(fill="x")
        # The state of the thread being read. It was nowhere on this pane: the
        # list said a question was open and the detail view then said nothing
        # about it, so the one thing the reader most needs to know while
        # reading -- is this still waiting on me -- had to be remembered from
        # the previous screen. Its colour is the row's colour.
        self.meta_lbl = ttk.Label(right, text="", font=fonts["meta"],
                                  justify="left", anchor="w")
        self.meta_lbl.pack(fill="x", pady=(2, 4))

        # Declared before the handler below closes over them. The handler is
        # bound before the panel is built, and a Configure delivered inside
        # that window would otherwise read an attribute that does not exist.
        self.activity_lbl = None
        self.activity_txt = None

        def _rewrap_detail(event: tk.Event) -> None:
            want = max(event.width - 12, 200)
            # The meta line wraps for the same reason and had the same bug it
            # would have caused: on the queue it reads "claimed - Work item -
            # opened by claude-code - held by builder - 1 message", which is
            # wider than the pane at the window's own default size, and a label
            # with no wraplength does not shorten -- it runs past the edge and
            # is silently clipped, so "held by b" was all that could be read.
            for lbl in (self.subject_lbl, self.meta_lbl, self.activity_lbl):
                if lbl is not None and _wrap_of(lbl) != want:
                    lbl.config(wraplength=want)
            # A Markdown table is laid out to the pane it is drawn in -- see
            # mdview -- so a resize changes the CONTENT of the pane and not only
            # its geometry, and the table has to be measured and drawn again.
            # Marking the pane stale is the whole of it: the poll then redraws
            # it within POLL_MS. Doing the redraw here instead would rebuild
            # every message on every Configure, and a Configure arrives many
            # times while a sash is dragged.
            #
            # Both marks are needed, and they are not the same mark. shown_count
            # = -1 is what makes refresh_detail repaint. detail_stale is what
            # tells the poll there is anything to repaint FOR, because its
            # change gate looks at message counts, question counts and the merge
            # list, and a window resize moves none of them -- so without this
            # second flag the resize would mark the pane and nothing would ever
            # read the mark. That is not a hypothetical: this handler shipped
            # with only shown_count set, and a table stayed at its old width
            # through a resize until an unrelated message arrived.
            txt = getattr(self, "msgs_txt", None)
            if txt is not None:
                capacity = mdview.capacity_chars(txt)
                if capacity != getattr(self, "_md_capacity", -1):
                    self._md_capacity = capacity
                    self.shown_count = -1
                    self.detail_stale = True
        right.bind("<Configure>", _rewrap_detail)

        # What a background agent is doing, on the queue and nowhere else.
        #
        # Only the queue has a thing that works without being watched: the
        # dispatcher spawns an agent and the agent edits this machine, and
        # until now the only trace of it in the window was the row's state
        # flipping to "claimed". That says an agent started and nothing else,
        # so a job twenty seconds from done and a job that died half an hour
        # ago looked identical.
        #
        # It is packed here rather than beside the messages because it is not
        # part of the conversation. The thread's messages are what was said
        # about the item; this is what was DONE to it, by a process that never
        # writes to the board.
        if channel == "work":
            box = ttk.Frame(right)
            box.pack(fill="x", pady=(0, 4))
            self.activity_lbl = ttk.Label(box, text="", font=fonts["meta"],
                                          justify="left", anchor="w")
            self.activity_lbl.pack(fill="x")
            # height=7 rather than a share of the pane: the steps are a
            # rolling window and the thread below is the part that grows.
            self.activity_txt = tk.Text(box, width=1, height=7, wrap="word",
                                        state="disabled", relief="solid",
                                        borderwidth=1, padx=4, pady=2,
                                        font=fonts["meta"])
            self.activity_txt.pack(fill="x")
            for kind, colour in _EVENT_LOOK.items():
                self.activity_txt.tag_configure(f"ev-{kind}",
                                                foreground=colour)
            self.activity_txt.tag_configure("ev-unknown", foreground="#333333")

        # The reply area is packed before the message view so it keeps a fixed
        # height at the bottom and the messages take the remaining space.
        reply = ttk.Frame(right)
        reply.pack(side="bottom", fill="x", pady=(4, 0))
        inner = ttk.Frame(reply)
        inner.pack(fill="x")
        # width=1 and height=1 on both Text widgets, because their requested
        # size is not their size: they are packed to fill whatever the pane
        # gives them, so asking for the Tk default of 80 characters x 24 lines
        # asks the PanedWindow for roughly 700px on the right-hand side and the
        # left-hand list then loses the argument over the space. That is what
        # was squeezing the list pane, and it is why narrowing the columns made
        # the list narrower rather than leaving room in it.
        self.reply_txt = tk.Text(inner, width=1, height=4, wrap="word",
                                 relief="solid", borderwidth=1)
        self.reply_txt.grid(row=0, column=0, sticky="ew")
        rsb = ttk.Scrollbar(inner, orient="vertical",
                            command=self.reply_txt.yview)
        self.reply_txt.configure(yscrollcommand=rsb.set)
        rsb.grid(row=0, column=1, sticky="ns")
        inner.columnconfigure(0, weight=1)
        # A row of its own rather than two packs, so that the secondary action
        # sits to the left of the primary one instead of above it. "Close &
        # archive" exists only on the question page: no other channel has a
        # settled/archived distinction to offer, and a Close button on a
        # discussion would close nothing.
        btns = ttk.Frame(reply)
        btns.pack(anchor="e", pady=(4, 0))
        self.post_btn = ttk.Button(
            btns, text="Answer" if channel == "question" else "Post",
            command=self._post,
        )
        self.post_btn.pack(side="right")
        self.close_btn = None
        self.unarchive_btn = None
        if channel == "question":
            self.close_btn = ttk.Button(btns, text="Close & archive",
                                        command=self._close)
            self.close_btn.pack(side="right", padx=(0, 6))
            # Disabled unless the selected thread IS archived, because the
            # button's whole job is to undo the state the reader is looking at.
            # Enabled always, it would be a second Close button that silently
            # did nothing on a live question.
            self.unarchive_btn = ttk.Button(btns, text="Bring back",
                                            command=self._unarchive,
                                            state="disabled")
            self.unarchive_btn.pack(side="right", padx=(0, 6))
        ttk.Label(reply, text=f"Reply as {paths.HUMAN} - Ctrl+Enter sends"
                  ).pack(anchor="w")
        self.reply_txt.bind("<Control-Return>", self._on_ctrl_return)

        msgframe = ttk.Frame(right)
        msgframe.pack(fill="both", expand=True)
        self.msgs_txt = tk.Text(msgframe, width=1, height=1, wrap="word",
                                state="disabled", relief="flat", padx=6, pady=4)
        msb = ttk.Scrollbar(msgframe, orient="vertical",
                            command=self.msgs_txt.yview)
        self.msgs_txt.configure(yscrollcommand=msb.set)
        self.msgs_txt.pack(side="left", fill="both", expand=True)
        msb.pack(side="left", fill="y")
        self.msgs_txt.tag_configure("meta", foreground="#777777")
        # Bodies are Markdown; mdview owns their tags, including the margin
        # the retired "body" tag used to carry.
        mdview.configure(self.msgs_txt)
        # A receipt is not a reply, and the grey is only the half of that a
        # reader sees at a glance: the other half is meta.kind, which is in the
        # data (see db.RECEIPT_KINDS), so the distinction survives an export, a
        # transcript, or the next agent reading the thread through a tool that
        # has no colour at all.
        #
        # Configured AFTER mdview.configure on purpose. Tk gives the
        # last-created tag the highest priority, so "receipt" outranks any
        # markdown tag the body happens to carry and the grey cannot be
        # overridden by a heading or a code span inside the receipt.
        #
        # Foreground only. A receipt is one line of plain text, and adding a
        # font or a margin here would fight the tags mdview already set on the
        # same range for no gain.
        self.msgs_txt.tag_configure("receipt", foreground="#8a8a8a")
        pane.add(right, weight=7)

        # A count under the list, so an empty channel says it is empty rather
        # than looking like a channel that failed to load, and so the state of
        # the whole channel is legible without counting coloured rows.
        self.footer = ttk.Label(self, text="", style="Footer.TLabel",
                                font=fonts["meta"], anchor="w")
        self.footer.pack(fill="x", padx=8, pady=(0, 8))
        self._show_hint(_NO_SELECTION)

    # --- reads ---------------------------------------------------------------

    def _show_hint(self, text: str) -> None:
        """Put a sentence where the messages go, for when there are none."""
        self.msgs_txt.config(state="normal")
        self.msgs_txt.delete("1.0", "end")
        self.msgs_txt.insert("end", text, ("meta",))
        self.msgs_txt.config(state="disabled")

    def _update_footer(self, rows: list) -> None:
        counts: dict = {}
        for r in rows:
            word = state_of(self.channel, r)[0]
            counts[word] = counts.get(word, 0) + 1
        noun = "item" if self.channel == "work" else "thread"
        n = len(rows)
        text = f"{n} {noun}" + ("" if n == 1 else "s")
        if not n:
            text = f"No {noun}s in this channel yet."
        elif counts:
            text += "   " + "   ".join(
                f"{counts[w]} {w}" for w in
                ("open", "claimed", "done", "answered", "closed", "fyi",
                 "archived")
                if counts.get(w))
        self.footer.config(text=text)

    def refresh_list(self, conn: sqlite3.Connection) -> None:
        # The Questions tab is the list of what is not finished with, so an
        # archived question -- one whose transcript is already in the vault --
        # leaves it. Filtered in SQL rather than here so the LIMIT counts
        # questions the reader asked for; dropping them afterwards would shorten
        # the tab by however many archived questions sat in the newest 200 rows.
        #
        # The Archived side of the filter is the same query pinned to the one
        # status, rather than the whole list with the others struck out: the
        # filter is a choice between two lists, and the Archived list is *only*
        # archived. `include_archived` stays False on both sides -- db.
        # list_threads resolves the contradiction in favour of an explicit
        # status, so the pinned query wins and there is no empty-list trap here.
        only_archived = self.channel == "question" and self.show_archived.get()
        rows = db.list_threads(
            conn, channel=self.channel, limit=200,
            status=paths.STATUS_ARCHIVED if only_archived else None,
            include_archived=(self.channel != "question"),
        )
        self.rows = rows
        self._update_footer(rows)
        # Build the whole table in memory first, so an unchanged refresh can be
        # skipped entirely. Rebuilding the tree widget drops the scrollbar
        # position and flashes the list, and a poll that found nothing new has
        # no business doing either.
        wanted = []
        for r in rows:
            word, tag, _colour = state_of(self.channel, r)
            tags = (tag,)
            if waiting_on_john(self.channel, r):
                tags = ("isopen", tag)
            # On the queue, the name that matters is the one holding the item.
            # "Opened by" on a work row is always the same coordinator, which
            # told the reader nothing about who is on it; the opener is in the
            # detail header, which has room to say both.
            if self.channel == "work":
                who = holder(r)
                by = identity.label(who) if who else ""
            else:
                by = identity.label(r["opened_by"])
            wanted.append((str(r["id"]), tags,
                           (word, r["subject"], by, local_ts(r["updated_ts"]),
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
        # Before the message-count gate below, and that ordering is the whole
        # reason this call is not at the bottom with the other renders. The
        # gate returns early when the number of messages has not changed, and
        # a background agent produces no messages at all -- so a panel drawn
        # after it would show whatever it read the moment the item was
        # selected, for the entire run, while the agent worked.
        self.refresh_activity(conn)
        if self.thread_id is None:
            # The header is blanked here as well as the body. The selection is
            # cleared whenever the list a row was picked from changes --
            # switching the Active/Archived filter does it -- and the previous
            # subject was left standing above a "select a thread" hint, which
            # reads as that thread still being the one on screen when the
            # filter is exactly what has just taken it away.
            self.subject_lbl.config(text="")
            self.meta_lbl.config(text="")
            if not self._hint_shown:
                self._show_hint(_NO_SELECTION)
                self._hint_shown = True
            # Answered, insofar as an empty pane can be: a resize has nothing to
            # refit here, and leaving the mark set would have the poll ask again
            # on every tick for as long as no thread is selected.
            self.detail_stale = False
            return
        try:
            data = db.get_thread(conn, self.thread_id)
        except ValueError:
            # The thread vanished (another process closed and it was pruned,
            # or a --db copy was swapped underneath us). Clear the pane.
            self.thread_id = None
            self.shown_count = -1
            self.detail_stale = False
            self.subject_lbl.config(text="")
            self.meta_lbl.config(text="")
            self._show_hint("That thread is no longer on the board.")
            self._hint_shown = True
            return
        thread, msgs = data["thread"], data["messages"]
        if len(msgs) == self.shown_count:
            return  # nothing new; do not repaint or move the scroll position
        self._hint_shown = False
        self.subject_lbl.config(text=thread["subject"])
        word, _tag, colour = state_of(self.channel, thread)
        noun = "message" if len(msgs) == 1 else "messages"
        bits = [word, _CHANNEL_LABEL[self.channel],
                f"opened by {identity.describe(thread['opened_by'])}"]
        who = holder(thread)
        if self.channel == "work" and who:
            bits.append(f"held by {identity.describe(who)}")
        bits.append(f"{len(msgs)} {noun}")
        self.meta_lbl.config(text="   ·   ".join(bits), foreground=colour)
        grew = 0 <= self.shown_count < len(msgs)
        self.shown_count = len(msgs)
        self.msgs_txt.config(state="normal")
        self.msgs_txt.delete("1.0", "end")
        for m in msgs:
            # Where this message starts, taken before anything of it is
            # inserted, so a receipt can be greyed as one range afterwards
            # rather than tag by tag - mdview decides those and there is no
            # list of them to hand.
            start = self.msgs_txt.index("end-1c")
            # The full identity, in words, in the pane that has room for it --
            # this is where a reader finds out WHICH claude-code session a post
            # came from, which the list column is too narrow to say.
            who = f"{identity.describe(m['author'])} ({m['author_kind']})   {local_ts(m['ts'])}"
            self.msgs_txt.insert("end", who + "\n", ("meta",))
            # render() ends on the newline after the last line, so one more
            # reproduces the blank-line gap a raw body + "\n\n" used to leave.
            mdview.render(self.msgs_txt, m["body"])
            self.msgs_txt.insert("end", "\n", ("meta",))
            # A receipt, greyed whole - header line and body together, because
            # "builder picked this thread up." is one statement and greying
            # only the body would leave it looking like a reply with a grey
            # sentence in it.
            if db.is_receipt(m["meta"]):
                self.msgs_txt.tag_add(
                    "receipt", start, self.msgs_txt.index("end-1c"))
                self.msgs_txt.tag_raise("receipt")
        self.msgs_txt.config(state="disabled")
        # The repaint that a resize asked for, and it is done here rather than
        # in the poll so that "the pane is stale" and "the pane was drawn" are
        # the same event: every path below this line has drawn the messages.
        self.detail_stale = False
        self._sync_buttons()
        # Jump to the bottom only when a message actually arrived; a repaint
        # of unchanged content should not move the reader.
        if grew:
            self.msgs_txt.see("end-1c")

    # --- the queue's progress panel ------------------------------------------

    def refresh_activity(self, conn: sqlite3.Connection) -> None:
        """What the background agent is doing with the selected item.

        A no-op on every channel but the queue: there is no dispatcher working
        anything else, so there is nothing to show, and an always-empty box on
        the Questions tab would read as a feature that is broken.
        """
        if self.activity_txt is None:
            return
        if self.thread_id is None:
            self._show_activity(None, [])
            return
        # The row comes from the database rather than from self.rows: this runs
        # on ticks where the list was not rebuilt, and a status read from a
        # stale list would let the panel say "claimed" about an item that
        # finished a moment ago.
        self._show_activity(db.work_thread(conn, self.thread_id),
                            db.list_work_events(conn, self.thread_id))

    def _show_activity(self, row: Optional[dict], events: list) -> None:
        if self.activity_txt is None:
            return
        label, lines = activity_text(row, events)
        # The two are digested separately, and the split is the point. The
        # label carries "last activity 4s ago", which changes with the clock
        # and so differs on nearly every poll tick; rebuilding the Text widget
        # that often would flicker and fight the scrollbar. The lines change
        # only when an event actually lands.
        if label != self._activity_label:
            self._activity_label = label
            self.activity_lbl.config(text=label)
        if lines != self._activity_lines:
            self._activity_lines = lines
            self.activity_txt.config(state="normal")
            self.activity_txt.delete("1.0", "end")
            for ts, body, tag in lines:
                self.activity_txt.insert("end", f"{ts}  ", ("ev-unknown",))
                self.activity_txt.insert("end", body + "\n", (tag,))
            self.activity_txt.config(state="disabled")
            # To the bottom, because this is a log of what just happened and
            # the newest line is the one being looked for. Unlike the message
            # pane there is no history worth reading back through: the item's
            # own thread, below, holds that.
            if lines:
                self.activity_txt.see("end-1c")

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

    def _close(self) -> None:
        """The Close & archive button. Settles the question; the sweep files it."""
        if self.thread_id is None:
            return
        self.app.close_question(self.thread_id)

    def _on_archived_filter(self) -> None:
        """Active/Archived switched. Redraw now rather than at the next tick.

        The poll runs every three seconds and would pick this up on its own,
        but a filter that takes three seconds to answer reads as a broken
        filter, and the reader has already decided what they want to see.
        """
        self.thread_id = None
        self.shown_count = -1
        self.app.redraw()

    def _unarchive(self) -> None:
        """The Bring back button. Puts an archived question on the tab again."""
        if self.thread_id is None:
            return
        self.app.unarchive_question(self.thread_id)

    def _sync_buttons(self) -> None:
        """Enable what the selection can actually do.

        Called from refresh_detail rather than from the select handler,
        because the status of the selected thread can change without the
        selection changing -- the archive sweep settling it, or another agent
        closing it. Reading it from the row the pane just rendered is the only
        version that cannot go stale.
        """
        if self.unarchive_btn is None:
            return
        row = next((r for r in self.rows if r["id"] == self.thread_id), None)
        archived = bool(row) and row["status"] == paths.STATUS_ARCHIVED
        self.unarchive_btn.config(state="normal" if archived else "disabled")

    def _on_ctrl_return(self, _event: Optional[tk.Event] = None) -> str:
        self._post()
        return "break"  # keep the newline out of the reply box

    def _post(self) -> None:
        body = self.reply_txt.get("1.0", "end-1c").strip()
        if not body or self.thread_id is None:
            return
        self.app.post_reply(self.channel, self.thread_id, body)
        self.reply_txt.delete("1.0", "end")


class PrPage(ttk.Frame):
    """The merge list: pull requests waiting on John, and nothing else.

    Not a Page and not a channel. A channel lists threads -- things people
    wrote, with replies under them -- and every part of Page is built around
    that: the reply box, the receipt greying, the message count. A pull request
    has a URL and a state and no conversation, so it gets its own small page
    rather than a Page that has to be told to hide half of itself.

    It mirrors Page's two-method interface (`refresh_list` and
    `refresh_detail`) so the app's redraw path does not need to know which kind
    of page it is holding.
    """

    # Open is blue rather than the red of an unanswered question. A PR waiting
    # on John is work he has chosen to take on, not something shouting at him,
    # and the red is spoken for by the one thing that toasts.
    _COLOURS = {
        paths.PR_OPEN: "#0b5394",
        paths.PR_MERGED: "#1a7f37",
        paths.PR_CLOSED: "#777777",
    }

    def __init__(self, master: tk.Misc, app: "App") -> None:
        super().__init__(master)
        self.app = app
        self.pr_id: Optional[int] = None
        self.rows: list[dict] = []
        self._rendered: list = []
        self._suppress_select = False
        fonts = fonts_for(self.winfo_toplevel())

        top = ttk.Frame(self)
        top.pack(fill="x", padx=8, pady=(8, 0))
        # Runs the check off the UI thread: `gh` costs about a second per PR,
        # and doing it inline would freeze the window for that long on every
        # press. The button wakes the watcher, which owns the subprocess.
        self.check_btn = ttk.Button(top, text="Check now",
                                    command=self.app.check_prs_now)
        self.check_btn.pack(side="right")
        self.open_btn = ttk.Button(top, text="Open on GitHub",
                                   command=self._open_selected)
        self.open_btn.pack(side="right", padx=(0, 6))
        self.show_settled = tk.BooleanVar(value=False)
        ttk.Checkbutton(top, text="Show merged and closed",
                        variable=self.show_settled,
                        command=self.app.refresh_now).pack(side="left")

        self.banner = ttk.Label(
            self,
            text="Pull requests waiting for you to merge them. Click one to "
                 "open it on GitHub, or double-click it in the list. The list "
                 "clears itself once GitHub says the PR is merged, and the "
                 "agent that opened it is told on the thread it came from.",
            style="Banner.TLabel", font=fonts["meta"], justify="left",
            anchor="w")
        self.banner.pack(fill="x", padx=8, pady=(4, 0))

        def _rewrap(event: tk.Event) -> None:
            want = max(event.width - 20, 200)
            if _wrap_of(self.banner) != want:
                self.banner.config(wraplength=want)
        self.bind("<Configure>", _rewrap)

        pane = ttk.PanedWindow(self, orient="horizontal")
        pane.pack(fill="both", expand=True, padx=8, pady=(6, 2))

        left = ttk.Frame(pane)
        # "Vispero/Fusion#80" as ONE column, not repository and number apart.
        # They are one identifier, and the width they were spending is width
        # the Checked column needs: with six columns the list clipped the last
        # one off the right edge at the window's own default size, so a check
        # that FAILED -- the single most important thing this list has to say,
        # and the reason the digest below is exact rather than clever -- could
        # not be seen at all without scrolling sideways.
        cols = ("state", "pull", "title", "by", "triage", "checked")
        self.tree = ttk.Treeview(left, columns=cols, show="headings",
                                 selectmode="browse")
        for key, label in (("state", "State"), ("pull", "Pull request"),
                           ("title", "Title"), ("by", "Asked by"),
                           ("triage", "Triage"), ("checked", "Checked")):
            self.tree.heading(key, text=label)
        self.tree.column("state", width=62, minwidth=54, anchor="center",
                         stretch=False)
        self.tree.column("pull", width=140, minwidth=104, anchor="w",
                         stretch=False)
        self.tree.column("title", width=150, minwidth=80, anchor="w")
        self.tree.column("by", width=70, minwidth=56, anchor="w",
                         stretch=False)
        self.tree.column("triage", width=104, minwidth=88, anchor="w",
                         stretch=False)
        self.tree.column("checked", width=86, minwidth=72, anchor="w",
                         stretch=False)
        ysb = ttk.Scrollbar(left, orient="vertical", command=self.tree.yview)
        self.tree.configure(yscrollcommand=ysb.set)
        self.tree.pack(side="left", fill="both", expand=True)
        ysb.pack(side="left", fill="y")
        for state, colour in self._COLOURS.items():
            self.tree.tag_configure(state, foreground=colour)
        self.tree.bind("<<TreeviewSelect>>", self._on_select)
        self.tree.bind("<Double-1>", self._on_double)
        pane.add(left, weight=3)

        right = ttk.Frame(pane)
        self.title_lbl = ttk.Label(right, text="", font=fonts["subject"],
                                   wraplength=420, justify="left", anchor="w")
        self.title_lbl.pack(fill="x")
        self.meta_lbl = ttk.Label(right, text="", font=fonts["meta"],
                                  justify="left", anchor="w", wraplength=420)
        self.meta_lbl.pack(fill="x", pady=(2, 4))
        self.url_lbl = ttk.Label(right, text="", font=fonts["meta"],
                                 foreground="#0b5394", wraplength=420,
                                 justify="left", anchor="w")
        self.url_lbl.pack(fill="x", pady=(0, 4))
        # The URL is shown as a label and not as a Text widget on purpose: it
        # is a link to be clicked, not a body to be read or selected, and a
        # disabled Text would accept the cursor and look editable.
        self.body_txt = tk.Text(right, wrap="word", height=6, width=1,
                                relief="flat",
                                borderwidth=0, highlightthickness=0)
        self.body_txt.pack(fill="both", expand=True)
        self.body_txt.config(state="disabled")
        pane.add(right, weight=2)

        self.footer = ttk.Label(self, text="", style="Footer.TLabel",
                                font=fonts["meta"], anchor="w")
        self.footer.pack(fill="x", padx=8, pady=(0, 6))

        def _rewrap_detail(event: tk.Event) -> None:
            want = max(event.width - 12, 200)
            for widget in (self.title_lbl, self.meta_lbl, self.url_lbl):
                if _wrap_of(widget) != want:
                    widget.config(wraplength=want)
        right.bind("<Configure>", _rewrap_detail)

    # --- selection -----------------------------------------------------------

    def _on_select(self, _event: Optional[tk.Event] = None) -> None:
        if self._suppress_select:
            return
        sel = self.tree.selection()
        if not sel:
            return
        try:
            self.pr_id = int(sel[0])
        except ValueError:
            return
        conn = db.connect(self.app.db_path)
        try:
            self.refresh_detail(conn)
        finally:
            conn.close()

    def _on_double(self, _event: tk.Event) -> str:
        self._open_selected()
        return "break"

    def _open_selected(self) -> None:
        row = self._selected_row()
        if row is None:
            return
        webbrowser.open(row["url"])

    def _selected_row(self) -> Optional[dict]:
        for r in self.rows:
            if r["id"] == self.pr_id:
                return r
        return None

    # --- drawing -------------------------------------------------------------

    def refresh_list(self, conn: sqlite3.Connection) -> None:
        rows = db.list_prs(conn, include_settled=self.show_settled.get())
        self.rows = rows
        open_n = sum(1 for r in rows if r["state"] == paths.PR_OPEN)
        n = len(rows)
        if not n:
            text = "Nothing waiting on you. An agent adds a PR here by calling request_merge."
        else:
            text = f"{n} pull request" + ("" if n == 1 else "s")
            if n != open_n:
                text += f"   {open_n} open   {n - open_n} settled"
        self.footer.config(text=text)

        wanted = []
        for r in rows:
            if r["last_error"]:
                checked = "failed"
            elif r["checked_ts"]:
                checked = local_ts(r["checked_ts"])
            else:
                checked = "never"
            by = identity.label(r["requested_by"])
            if r.get("source") == paths.PR_SOURCE_SCAN:
                # Distinguishes "an agent is asking you to merge this" from
                # "GitHub says you're on this and haven't looked" right in
                # the list, without spending a whole column on it -- see
                # #115's acceptance criteria for why the two must not be
                # conflated.
                by = f"{by} (scan)"
            triage = r.get("triage") or ""
            wanted.append((str(r["id"]), (r["state"],),
                           (r["state"], f"{r['repo']}#{r['number']}",
                            r["title"], by, triage,
                            checked)))
        if wanted == self._rendered:
            return

        self._suppress_select = True
        try:
            keep = self.pr_id
            at = self.tree.yview()
            self.tree.delete(*self.tree.get_children())
            for iid, tags, values in wanted:
                self.tree.insert("", "end", iid=iid, tags=tags, values=values)
            if keep is not None and self.tree.exists(str(keep)):
                self.tree.selection_set(str(keep))
            self.tree.yview_moveto(at[0])
        finally:
            self._suppress_select = False
            self._rendered = wanted

    def refresh_detail(self, conn: sqlite3.Connection) -> None:
        row = self._selected_row()
        if row is None:
            self.title_lbl.config(text="")
            self.meta_lbl.config(text="")
            self.url_lbl.config(text="")
            self._show_body("Select a pull request on the left to read it.")
            return
        self.title_lbl.config(text=row["title"])
        source_bit = ("found by GitHub scan" if row.get("source") == paths.PR_SOURCE_SCAN
                     else f"asked by {identity.describe(row['requested_by'])}")
        bits = [row["state"], f"{row['repo']}#{row['number']}", source_bit]
        if row.get("triage"):
            bits.append(f"triage: {row['triage']}")
        if row["checked_ts"]:
            bits.append(f"checked {local_ts(row['checked_ts'])}")
        else:
            bits.append("not checked yet")
        if row["settled_ts"]:
            bits.append(f"settled {local_ts(row['settled_ts'])}")
        self.meta_lbl.config(text="   ·   ".join(bits),
                             foreground=self._COLOURS.get(row["state"], "#333333"))
        self.url_lbl.config(text=row["url"])

        lines = []
        if row["last_error"]:
            lines.append("The last check did not complete:")
            lines.append("")
            lines.append(f"    {row['last_error']}")
            lines.append("")
            lines.append("The pull request is still on the list. A check that "
                         "failed never removes one -- only GitHub saying the PR "
                         "is merged does that -- so this row is still waiting "
                         "on you, and the checker will try again.")
        else:
            lines.append("Click Open on GitHub, or double-click the row, to go "
                         "and merge it. The list clears itself once it is merged.")
        if row["thread_id"]:
            lines.append("")
            lines.append(f"When it is merged, a notice is posted on thread "
                         f"#{row['thread_id']}.")
        self._show_body("\n".join(lines))

    def _show_body(self, text: str) -> None:
        self.body_txt.config(state="normal")
        self.body_txt.delete("1.0", "end")
        self.body_txt.insert("end", text)
        self.body_txt.config(state="disabled")


# --- one window per board -----------------------------------------------------
#
# Two copies means two poll loops, two tray icons and two toasts for the same
# question, and nothing on screen says which window is authoritative.
#
# The guard is keyed to the DATABASE, not to the app, because the board is the
# thing that must not be doubled. A `--db` run against a scratch copy is a
# different board and is therefore legitimately a different instance -- keying
# the mutex to the app instead would make the test suite and the real window
# mutually exclusive for no reason, and the tests would have to lie about it.
#
# A named mutex rather than a pidfile. Windows releases a mutex when the owning
# process ends, however it ends, so a crash cannot leave John locked out of his
# own app. A pidfile can, and the stale-pid recovery that fixes it is exactly
# the sort of code that works until the one day it doesn't.

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
        self.root.geometry("1000x640")
        self.root.minsize(720, 420)
        self.root.protocol("WM_DELETE_WINDOW", self._hide_to_tray)

        # The dispatcher control sits OUTSIDE the notebook, above it, because it
        # is not a property of any one channel: turning it on starts agents on
        # whatever is in the queue, and a control that lived on the Work tab
        # would look like it only affected the tab it was sitting on.
        strip = ttk.Frame(self.root, padding=(8, 6))
        strip.pack(fill="x")
        self.worker_label = ttk.Label(strip, text="Worker: checking...")
        self.worker_label.pack(side="left")
        # Empty until a notifier sink is actually disabled -- see
        # notify.disabled_sinks() and README.md's "Architecture" section. A
        # disabled sink degrades silently by design (that is the whole point
        # of drop-rather-than-die); this is what stops "silently" from also
        # meaning "invisibly".
        self.sinks_label = ttk.Label(strip, text="", foreground="#a94442")
        self.sinks_label.pack(side="left", padx=(12, 0))
        self.reload_button = ttk.Button(strip, text="Reload code",
                                        command=self._reload_code)
        self.reload_button.pack(side="right", padx=(0, 6))
        self.worker_button = ttk.Button(strip, text="Start worker",
                                        command=self._toggle_worker)
        self.worker_button.pack(side="right")
        # A rule under the controls, which is what turns a row of widgets into
        # a status bar -- and it separates the app's own state (the dispatcher)
        # from the board's four pages, which was previously all one flat area.
        ttk.Separator(self.root, orient="horizontal").pack(fill="x")

        labels = {"question": "Questions", "discussion": "Discussion",
                  "wiki": "Wiki", "work": "Work to Hire"}
        self.tab_labels = labels
        nb = ttk.Notebook(self.root)
        nb.pack(fill="both", expand=True)
        self.nb = nb
        self.pages: dict[str, Page] = {}
        for ch in paths.CHANNELS:
            page = Page(nb, self, ch)
            nb.add(page, text=labels[ch])
            self.pages[ch] = page
        # The merge list is a fifth tab and NOT a fifth channel -- see PrPage
        # for why. It is added last so it sits after the four conversation
        # pages rather than pushing them along.
        self.pr_labels = {"prs": "Pull Requests"}
        self.pr_page = PrPage(nb, self)
        nb.add(self.pr_page, text=self.pr_labels["prs"])

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
        self._refresh_worker()
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

    def _refresh_worker(self) -> None:
        running, text = self._worker_status()
        # The dot carries the state. "Worker: running, item #2" and "Worker:
        # not running" are the same shape of sentence in the same grey, and
        # this is the only control on the window that says whether anything is
        # working the queue at all.
        self.worker_label.config(
            text=("● " if running else "○ ") + text,
            foreground="#1e7f34" if running else "#8a6d3b")
        self.worker_button.config(text="Stop worker" if running else "Start worker")
        disabled = notify.disabled_sinks()
        # Zero is not shown -- the same convention _refresh_tabs uses for the
        # three waiting-count tabs. A sink registry nothing has grown into
        # (the shipped default) must not put a permanent warning on screen.
        self.sinks_label.config(
            text=(f"⚠ {len(disabled)} notifier sink"
                  + ("s" if len(disabled) != 1 else "") + " disabled")
            if disabled else "")

    def _refresh_tabs(self) -> None:
        """Put a count on the three tabs that are waiting for somebody.

        Only those three. Discussion and Wiki are read, not worked, and a number
        beside them would be decoration; Questions, Work to Hire and Pull
        Requests are the three places something sits until a person or an agent
        deals with it, and the count is what says so while the reader is
        looking at a different tab. A zero is not shown -- "Questions" beats
        "Questions (0)".

        The merge list is counted from its own rows rather than through
        self.pages, because it is not a channel; an open PR is waiting on John
        in exactly the sense this function means.
        """
        for ch in ("question", "work"):
            page = self.pages.get(ch)
            if page is None:
                continue
            # Questions count by whether they are waiting, work by status --
            # the same split as state_of, and for the same reason: a work
            # item's status IS its availability, and a question's is not.
            if ch == "question":
                waiting = sum(1 for r in page.rows
                              if waiting_on_john("question", r))
            else:
                waiting = sum(1 for r in page.rows
                              if r["status"] == paths.STATUS_OPEN)
            label = self.tab_labels[ch]
            self.nb.tab(page, text=f"{label} ({waiting})" if waiting else label)
        opening = sum(1 for r in self.pr_page.rows
                      if r["state"] == paths.PR_OPEN)
        label = self.pr_labels["prs"]
        self.nb.tab(self.pr_page,
                    text=f"{label} ({opening})" if opening else label)

    def _toggle_worker(self) -> None:
        if self._worker_status()[0]:
            self._stop_worker()
        else:
            self._start_worker()
        # Re-read rather than assuming: the start is a subprocess that may fail,
        # and a button that says "running" when nothing is running is a lie the
        # next poll would only sometimes catch.
        self.root.after(400, self._refresh_worker)

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

    def _pane_needs_repaint(self) -> bool:
        """Whether a resize has marked a detail pane for redrawing.

        The change gate below compares message counts, open-question counts and
        the merge list, and a window resize moves none of those -- so a table
        that mdview would lay out differently at the pane's new width would keep
        the old one until some message happened to arrive. This is the fourth
        reason to redraw, and the only one that comes from the window rather
        than from the database.

        Only a page whose pane is actually on screen can be marked: capacity is
        read from a mapped widget, and an unmapped pane answers 0 the same way
        every time, so its mark never moves. That is the behaviour wanted -- a
        tab nobody is looking at is refreshed when it is opened.
        """
        for page in list(self.pages.values()) + [self.pr_page]:
            if getattr(page, "detail_stale", False):
                return True
        return False

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
                    or prs_sig != self.last_prs_sig
                    # A resize is not in the database, so none of the three
                    # counters above can see it -- see _pane_needs_repaint.
                    or self._pane_needs_repaint()):
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
            # The queue's progress panel, every tick and deliberately NOT
            # behind the change gate above. Its most useful number is "last
            # activity 4s ago", and that number moves with the CLOCK rather
            # than with the database: gated, it would freeze at whatever it
            # read when the last event landed, so an agent that died ten
            # minutes ago would still be reading "12s ago". A stale "ago" is
            # the one thing that would make a hung job look busy, which is the
            # opposite of what this panel is for. The read is one indexed
            # query and the repaint is skipped when the text is unchanged.
            work_page = self.pages.get("work")
            if work_page is not None:
                work_page.refresh_activity(conn)
            self.last_max_id = max_id
            self.last_open_count = len(open_qs)
            self.last_prs_sig = prs_sig
        except sqlite3.Error as exc:
            # The database is shared with the MCP server and the hourly backup;
            # a transient failure waits for the next tick rather than bringing
            # the window down.
            notify.log_line(f"poll failed: {exc!r}")
        finally:
            if conn is not None:
                conn.close()
        # Outside the try above on purpose. The dispatcher strip has nothing to
        # do with the database, and a transient database failure should not
        # also freeze the button that starts the thing that works the queue.
        try:
            self._refresh_worker()
        except Exception as exc:
            notify.log_line(f"worker status refresh failed: {exc!r}")
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
        for page in self.pages.values():
            page.refresh_list(conn)
            page.refresh_detail(conn)
        # The merge list is not a channel, so it is not in self.pages; it
        # implements the same two methods so this path does not care.
        self.pr_page.refresh_list(conn)
        self.pr_page.refresh_detail(conn)
        # After the pages, because it reads the rows they just loaded.
        self._refresh_tabs()

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
            page = self.pages.get("question")
            if page is not None:
                page.show_archived.set(False)
        finally:
            conn.close()
        # redraw, not refresh_now: archived -> answered is neither a new message
        # nor a change in how many questions are open, so the poll's gate would
        # skip it and the row would stay on the Archived list it no longer
        # belongs in.
        self.redraw()

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
