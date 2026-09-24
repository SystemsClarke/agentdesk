"""The AgentDesk window drawn as a dial-up bulletin board.

Speed is the design constraint: every screen is drawn from an in-memory copy of the
board that App's poll refreshes, never from the database on a keypress, and the
painter rewrites only the lines that differ from what is already on screen.
"""

from __future__ import annotations

import functools
import json
import logging
import logging.handlers
import sqlite3
import threading
import time
import tkinter as tk
import tkinter.font as tkfont
import webbrowser
from datetime import datetime, timedelta, timezone
from typing import Optional

from agentdesk import db, identity, mdview, notify, paths, screech, settings, usage, vscode_themes

PALETTES = {
    "monokai-pro": {
        "label": "Monokai Pro", "dark": True,
        "bg": "#221f22", "panel": "#2d2a2e", "line": "#403e41", "fg": "#fcfcfa",
        "mu": "#939293", "fa": "#727072", "rule": "#5b595c",
        "pk": "#ff6188", "or": "#fc9867", "ye": "#ffd866", "gr": "#a9dc76",
        "cy": "#78dce8", "pu": "#ab9df2", "on_bar": "#221f22", "textsel": "#5b595c",
    },
    "monokai-pro-light": {
        "label": "Monokai Pro Light", "dark": False,
        "bg": "#faf4f2", "panel": "#ede7e5", "line": "#e0dad9", "fg": "#29242a",
        "mu": "#706b6e", "fa": "#918c8e", "rule": "#d3cdcc",
        "pk": "#e14775", "or": "#e16032", "ye": "#cc7a0a", "gr": "#269d69",
        "cy": "#1c8ca8", "pu": "#7058be", "on_bar": "#faf4f2", "textsel": "#d3cdcc",
    },
}

LOGO = [("█▀█", "█▀█"), ("█▀▀", "█▄█"), ("█▀▀", "██▄"), ("█▄ █", "█ ▀█"), ("▀█▀", " █ "),
        ("█▀▄", "█▄▀"), ("█▀▀", "██▄"), ("█▀", "▄█"), ("█▄▀", "█ █")]
LOGO_HUES = ["pk", "pk", "or", "or", "ye", "gr", "gr", "cy", "pu"]

CHANNEL_KEYS = {"q": "question", "d": "discussion", "w": "wiki", "j": "work"}
BANNER = {
    "question": "QUESTIONS  ·  the SysOp's desk  ·  agents can chime in, only john can close",
    "discussion": "DISCUSSION  ·  the agents' break room  ·  you're on the party line",
    "wiki": "WIKI  ·  the file library  ·  browse all you like, no leech ratio",
    "work": "WORK TO HIRE  ·  the job board  ·  open is up for grabs, held means someone's on it",
}
TITLE = {"question": "Questions", "discussion": "Discussion", "wiki": "Wiki", "work": "Work to Hire"}
AUTHOR_HUES = ["cy", "pu", "gr", "or"]
RECEIPT_KINDS = set(db.RECEIPT_KINDS)
USAGE_ROTATE_S = 6
FLASH_MS = 3200
FRAME_MS = 16.7
WATCH_MS = 150

log = logging.getLogger("agentdesk.terminal")


def _setup_log() -> None:
    if log.handlers:
        return
    try:
        paths.ensure_dirs()
        h = logging.handlers.RotatingFileHandler(paths.DATA_DIR / "terminal.log", maxBytes=1_000_000,
                                                 backupCount=3, encoding="utf-8")
        h.setFormatter(logging.Formatter("%(asctime)s.%(msecs)03d %(levelname)-5s %(message)s", "%Y-%m-%d %H:%M:%S"))
        log.addHandler(h)
        log.setLevel(logging.DEBUG)
        log.propagate = False
    except OSError:
        pass


def guarded(fn):
    """Log any exception from a UI handler with its traceback instead of letting Tk swallow it."""
    @functools.wraps(fn)
    def wrap(self, *a, **kw):
        try:
            return fn(self, *a, **kw)
        except Exception:
            log.exception("%s failed on screen=%s channel=%s", fn.__name__, self.screen, self.channel)
            try:
                self.flash(f"Something broke in {fn.__name__}; details in terminal.log", "pk", "b")
            except Exception:
                pass
            return "break"
    return wrap


def S(text, *tags):
    return (text, tags)


def fit(s: str, n: int) -> str:
    s = " ".join(str(s).split())
    if n <= 0:
        return ""
    return s[: n - 1] + "…" if len(s) > n else s.ljust(n)


def seglen(segs) -> int:
    return sum(len(t) for t, _ in segs)


def clip(segs, width):
    """Cut segments to `width` columns, keeping colours and spacing, ending in an ellipsis."""
    out, used = [], 0
    for text, tags in segs:
        if used + len(text) <= width - 1:
            out.append((text, tags))
            used += len(text)
        else:
            out.append((text[: max(0, width - 1 - used)] + "…", tags))
            break
    return out


def pad(segs, width, *tags):
    n = width - seglen(segs)
    return list(segs) + ([S(" " * n, *tags)] if n > 0 else [])


def _parse(iso) -> Optional[datetime]:
    try:
        dt = datetime.fromisoformat(str(iso))
    except (TypeError, ValueError):
        return None
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


def when(iso) -> str:
    dt = _parse(iso)
    if dt is None:
        return ""
    local = dt.astimezone()
    now = datetime.now().astimezone()
    if local.date() == now.date():
        return local.strftime("%H:%M")
    if (now - local) < timedelta(days=6):
        return local.strftime("%a %H:%M")
    return local.strftime("%d-%b %H:%M")


def ago(iso) -> str:
    dt = _parse(iso)
    if dt is None:
        return ""
    return usage.span((datetime.now(timezone.utc) - dt).total_seconds())


def _meta(raw) -> dict:
    try:
        m = json.loads(raw or "{}")
        return m if isinstance(m, dict) else {}
    except (TypeError, ValueError):
        return {}


def author_hue(author: str) -> str:
    if author == paths.HUMAN:
        return "ye"
    return AUTHOR_HUES[sum(map(ord, author or "")) % len(AUTHOR_HUES)]


def state_code(channel: str, row) -> tuple:
    if waiting(channel, row):
        return ("WAIT", ("pk", "b"))
    word = row["status"]
    return {
        "open": ("OPEN", ("ye",)) if channel == "work" else ("live", ("mu",)),
        "claimed": ("HELD", ("cy",)),
        "done": ("DONE", ("gr",)),
        "answered": ("ansd", ("mu",)),
        "closed": ("clsd", ("fa",)),
        "fyi": ("fyi ", ("mu",)),
        "archived": ("arch", ("fa",)),
    }.get(word, (fit(word, 4), ("mu",)))


DELIVERY_LATE_S = 15 * 60


def delivery_mark(row) -> str:
    """'you' plus how your latest reply reached the agent: √ it acted · ↑ woke it · … on its way · ! not picked up.

    Only glyphs the terminal font has: a fallback-font lookup (✓, ⚡) cost ~100 ms on first paint."""
    try:
        state, _, ts = (row["delivery"] or "").partition("|")
    except (IndexError, KeyError, TypeError):
        return "you"
    if state == "picked-up":
        return "you √"
    if state in ("woke", "resumed"):
        return "you ↑ woke"
    if state in ("pending", "stuck", "failed"):
        try:
            age = (datetime.now(timezone.utc) - datetime.fromisoformat(ts)).total_seconds()
        except ValueError:
            age = 0
        return f"you ! {ago(ts)}" if state != "pending" or age > DELIVERY_LATE_S else "you …"
    return "you"


def waiting(channel: str, row) -> bool:
    """Does this question still owe John an answer? (db computes `waiting` from its messages.)"""
    try:
        return channel == "question" and bool(row["waiting"])
    except (IndexError, KeyError, TypeError):
        return False


def holder(row) -> str:
    """The agent holding a work item, from its meta JSON, or ""."""
    return _meta(row.get("meta") if isinstance(row, dict) else None).get("assignee") or ""


def clock(iso) -> str:
    dt = _parse(iso)
    return dt.astimezone().strftime("%H:%M:%S") if dt else ""


def activity(row, events: list) -> tuple:
    """(label, [(clock, body, tag)]) for the worker's progress on one work item."""
    if row is None:
        return "No item selected.", []
    word = state_code("work", row)[0].strip().lower()
    who = holder(row)
    held = f"held by {identity.label(who)}" if who else ""
    if not events:
        if row["status"] == paths.STATUS_CLAIMED:
            return " · ".join(x for x in (word, held, "no progress reported") if x), [
                ("", "Nothing has reported progress. An item claimed by hand, not by the worker, has none.",
                 "ev-unknown")]
        return f"{word} · nothing has run this item", []
    started = next((e for e in events if e["kind"] == db.WORK_START), events[0])
    last = events[-1]
    bits = [word, held, f"started {ago(started['ts'])} ago", f"last activity {ago(last['ts'])} ago"]
    if last["kind"] == db.WORK_ERROR:
        bits.append(last["body"])
    return " · ".join(b for b in bits if b), [(clock(e["ts"]), e["body"], f"ev-{e['kind']}") for e in events]


class TerminalView:
    def __init__(self, app, root: tk.Tk) -> None:
        self.app = app
        self.root = root
        self.prefs = settings.load()
        self.palettes = dict(PALETTES)
        try:
            found = vscode_themes.cached()
            self.palettes.update({k: v for k, v in found.items() if v["label"] not in ("Monokai Pro",)})
        except Exception:
            pass  # no VS Code, or an unreadable theme: the two built-in palettes still work
        self.theme_order = list(self.palettes)
        if self.prefs.get("theme") not in self.palettes:
            self.prefs["theme"] = "monokai-pro"
        self.pal = self.palettes[self.prefs["theme"]]

        self.screen = "main"
        self.channel = "question"
        self.sel = {ch: 0 for ch in paths.CHANNELS}
        self.sel_prs = self.sel_who = self.sel_opt = 0
        self.show_archived = False
        self.show_settled = False
        self.read_tid: Optional[int] = None
        self.read_back = "list"
        self.confirm: Optional[tuple] = None
        self._flash: Optional[tuple] = None
        self._flash_job = None

        self.rows = {ch: [] for ch in paths.CHANNELS}
        self.prs: list = []
        self.open_qs: list = []
        self.recent: list = []
        self.callers: list = []
        self.bios: dict = {}
        self.john_last: Optional[dict] = None
        self.since_posts = 0
        self.last_filed: Optional[dict] = None
        self.worker = (False, "Worker: checking...")
        self.held_row: Optional[dict] = None
        self.held_events: list = []
        self.slack: Optional[dict] = None
        self.sinks: dict = {}
        self.usage_lines: list = usage.lines()
        self.usage_summary = usage.summary()
        self.top_row = {ch: 0 for ch in paths.CHANNELS}
        self._threads: dict = {}
        self._bio_bodies: dict = {}

        self._painted: list = []
        self._top_painted: list = []
        self._bar_painted: list = []
        self._reader_key = None
        self._click_map: dict = {}
        self.cols = 96
        self.stats = {"build_ms": 0.0, "paint_ms": 0.0, "changed": 0}

        _setup_log()
        self._build()
        self._apply_theme()
        self.render()
        log.info("terminal view up: font=%s %spt theme=%s screech=%s cols=%s",
                 self.font.actual("family"), self.font.actual("size"), self.prefs["theme"],
                 self.prefs.get("screech"), self.cols)
        self._watch_conn = None
        self.root.after(500, self._start_watch)

    # --- instant refresh ------------------------------------------------------
    # PRAGMA data_version (the trick honker is built on, minus the extension): a
    # counter that moves whenever ANOTHER connection commits, readable in microseconds.

    def _start_watch(self) -> None:
        try:
            self._watch_conn = sqlite3.connect(str(self.app.db_path), timeout=1.0, isolation_level=None)
            self._data_version = self._watch_conn.execute("PRAGMA data_version").fetchone()[0]
            log.info("watching %s via PRAGMA data_version every %dms", self.app.db_path, WATCH_MS)
        except sqlite3.Error as exc:
            log.warning("data_version watch unavailable, falling back to the 3s poll: %r", exc)
            self._watch_conn = None
            return
        self.root.after(WATCH_MS, self._watch)

    def _watch(self) -> None:
        try:
            v = self._watch_conn.execute("PRAGMA data_version").fetchone()[0]
            if v != self._data_version:
                t0 = time.perf_counter()
                self.app.refresh_now()
                # Absorb whatever the poll itself wrote (acks, archive sweep) so it can't re-trigger.
                self._data_version = self._watch_conn.execute("PRAGMA data_version").fetchone()[0]
                log.debug("board changed (data_version %s): refreshed in %.1fms", v,
                          (time.perf_counter() - t0) * 1000)
        except tk.TclError:
            return
        except Exception as exc:
            log.warning("board watch failed: %r", exc)
        self.root.after(WATCH_MS, self._watch)

    # --- widgets -------------------------------------------------------------

    def _pick_family(self) -> str:
        have = set(tkfont.families(self.root))
        for fam in ("Cascadia Mono", "Cascadia Code", "JetBrains Mono", "Consolas", "Courier New"):
            if fam in have:
                return fam
        return "TkFixedFont"

    def _build(self) -> None:
        fam = self._pick_family()
        size = int(self.prefs.get("font_size", 11))
        self.font = tkfont.Font(root=self.root, family=fam, size=size)
        self.bold = tkfont.Font(root=self.root, family=fam, size=size, weight="bold")
        self.frame = tk.Frame(self.root, highlightthickness=0, bd=0)
        self.frame.pack(fill="both", expand=True)
        common = dict(font=self.font, bd=0, highlightthickness=0, wrap="none", padx=10,
                      cursor="arrow", takefocus=0)
        self.top = tk.Text(self.frame, height=1, pady=5, **common)
        self.top.pack(fill="x")
        self.bar = tk.Text(self.frame, height=1, pady=5, **common)
        self.bar.pack(side="bottom", fill="x")
        # Two stacked pages flipped with tkraise: no wrap change, no re-pack, no relayout.
        self.stack = tk.Frame(self.frame, bd=0, highlightthickness=0)
        self.stack.pack(fill="both", expand=True)
        self.stack.grid_rowconfigure(0, weight=1)
        self.stack.grid_columnconfigure(0, weight=1)
        self.page_lines = tk.Frame(self.stack, bd=0, highlightthickness=0)
        self.page_read = tk.Frame(self.stack, bd=0, highlightthickness=0)
        for page in (self.page_lines, self.page_read):
            page.grid(row=0, column=0, sticky="nsew")
        self.lines_view = tk.Text(self.page_lines, **{**common, "padx": 12, "pady": 8})
        self.lines_view.pack(fill="both", expand=True)
        self.input = tk.Frame(self.page_read, bd=0, highlightthickness=0)
        self.subject_row = tk.Frame(self.input, bd=0, highlightthickness=0)
        self.subject_lbl = tk.Label(self.subject_row, text="subj>", font=self.bold, anchor="w")
        self.subject_lbl.pack(side="left", padx=(10, 6))
        self.subject_ent = tk.Entry(self.subject_row, font=self.font, bd=0, relief="flat",
                                    highlightthickness=1)
        self.subject_ent.pack(side="left", fill="x", expand=True, padx=(0, 10), ipady=3)
        self.reply_row = tk.Frame(self.input, bd=0, highlightthickness=0)
        self.reply_lbl = tk.Label(self.reply_row, text=f"{paths.HUMAN}>", font=self.bold, anchor="nw")
        self.reply_lbl.pack(side="left", anchor="n", padx=(10, 6), pady=4)
        self.reply = tk.Text(self.reply_row, height=3, font=self.font, bd=0, wrap="word",
                             highlightthickness=1, padx=6, pady=4, undo=True)
        self.reply.pack(side="left", fill="x", expand=True, padx=(0, 10), pady=(0, 6))
        self.reply_row.pack(fill="x", side="bottom")
        self.hint_lbl = tk.Label(
            self.input, anchor="w", font=self.font,
            text="Ctrl+Enter sends  ·  Ctrl+D dictates: local speech-to-text that runs on this PC "
                 "(nothing leaves the machine; the first use downloads the model once)")
        self.hint_lbl.pack(fill="x", side="bottom", before=self.reply_row, padx=(10, 10), pady=(0, 6))
        self.input.pack(side="bottom", fill="x", pady=(4, 0))
        self.read_view = tk.Text(self.page_read, **{**common, "padx": 12, "pady": 8, "wrap": "word"})
        self.read_view.pack(fill="both", expand=True)
        mdview.configure(self.read_view)
        self.body = self.lines_view
        self.page_lines.tkraise()
        # Screens stay state=normal: toggling state relayouts the whole widget (~2ms each),
        # so read-only is enforced by bindings instead. Top and bar take no input at all.
        for w in (self.top, self.bar, self.lines_view, self.read_view):
            w._agentdesk_readonly = True
        for w in (self.top, self.bar):
            w.bindtags((str(w), str(self.root), "all"))
        for view in (self.lines_view, self.read_view):
            for seq in ("<<Paste>>", "<<Cut>>", "<<Clear>>", "<<PasteSelection>>", "<<Undo>>",
                        "<<Redo>>", "<Button-2>"):
                view.bind(seq, lambda e: "break")
            view.bind("<Key>", self._on_key)
            view.bind("<Button-1>", self._on_click)
            view.bind("<Double-Button-1>", self._on_double)
        self.lines_view.bind("<Configure>", self._on_resize)
        self.reply.bind("<Control-Return>", self._send)
        # The reply box has focus whenever a message is open, so its keys are the reader's keys.
        self.reply.bind("<Escape>", lambda e: (self.go_back(), "break")[1])
        self.reply.bind("<Prior>", lambda e: (self.read_view.yview_scroll(-1, "pages"), "break")[1])
        self.reply.bind("<Next>", lambda e: (self.read_view.yview_scroll(1, "pages"), "break")[1])
        self.reply.bind("<Alt-n>", lambda e: (self.reader_step(1), "break")[1])
        self.reply.bind("<Alt-p>", lambda e: (self.reader_step(-1), "break")[1])
        self.reply.bind("<Alt-c>", lambda e: (self.reader_close(), "break")[1])
        self.reply.bind("<Alt-u>", lambda e: (self.reader_unarchive(), "break")[1])
        self.reply.bind("<Control-w>", lambda e: (self.toggle_worker(), "break")[1])
        self.reply.bind("<Control-r>", lambda e: (self.wake_selected(), "break")[1])
        self.subject_ent.bind("<Control-Return>", self._send)
        self.subject_ent.bind("<Escape>", lambda e: (self.go_back(), "break")[1])
        self.subject_ent.bind("<Return>", lambda e: (self.reply.focus_set(), "break")[1])
        self.root.bind("<Control-equal>", lambda e: self.zoom(1))
        self.root.bind("<Control-minus>", lambda e: self.zoom(-1))
        self._focus_body()

    def _apply_theme(self) -> None:
        p = self.pal
        for w in (self.frame, self.input, self.subject_row, self.reply_row, self.stack,
                  self.page_lines, self.page_read):
            w.configure(bg=p["bg"])
        for w in (self.subject_lbl, self.reply_lbl):
            w.configure(bg=p["bg"], fg=p["ye"])
        self.hint_lbl.configure(bg=p["bg"], fg=p["fa"])
        for w in (self.top, self.bar):
            w.configure(bg=p["panel"], fg=p["fg"], selectbackground=p["textsel"],
                        insertbackground=p["panel"])
        for view in (self.lines_view, self.read_view):
            view.configure(bg=p["bg"], fg=p["fg"], selectbackground=p["textsel"],
                           selectforeground=p["fg"], inactiveselectbackground=p["textsel"],
                           insertbackground=p["bg"])
        for w in (self.reply, self.subject_ent):
            w.configure(bg=p["panel"], fg=p["fg"], insertbackground=p["ye"],
                        selectbackground=p["textsel"], selectforeground=p["fg"],
                        highlightbackground=p["line"], highlightcolor=p["ye"])
        self.root.configure(bg=p["bg"])
        for t in (self.top, self.bar, self.lines_view, self.read_view):
            for name in ("fg", "mu", "fa", "rule", "pk", "or", "ye", "gr", "cy", "pu"):
                t.tag_configure(name, foreground=p[name])
            t.tag_configure("b", font=self.bold)
            t.tag_configure("bar", background=p["ye"], foreground=p["on_bar"])
            t.tag_configure("barcy", background=p["cy"], foreground=p["on_bar"])
            t.tag_configure("inv", background=p["line"])
            t.tag_configure("cur", background=p["ye"], foreground=p["on_bar"], font=self.bold)
            t.tag_configure("rcpt", foreground=p["fa"])
        mono = self.font.actual("family")
        size = self.font.actual("size")
        b = self.read_view
        for tag, extra in (("md-h1", 3), ("md-h2", 2), ("md-h3", 1), ("md-h456", 0)):
            b.tag_configure(tag, font=(mono, size + extra, "bold"), foreground=p["ye"])
        b.tag_configure("md-bold", font=(mono, size, "bold"))
        b.tag_configure("md-italic", font=(mono, size, "italic"))
        b.tag_configure("md-code", font=(mono, size), background=p["panel"], foreground=p["or"])
        b.tag_configure("md-codeblock", font=(mono, size), background=p["panel"])
        b.tag_configure("md-quote", foreground=p["mu"])
        b.tag_configure("md-rule", foreground=p["rule"])
        b.tag_configure("md-trule", foreground=p["rule"])
        b.tag_configure("md-table", font=(mono, size))
        b.tag_configure("md-th", font=(mono, size, "bold"))
        b.tag_configure("md-link", foreground=p["cy"])
        for name in ("fg", "mu", "fa", "rule", "pk", "or", "ye", "gr", "cy", "pu"):
            b.tag_configure(f"mdc-{name}", foreground=p[name])
        b.tag_configure("md-diagram", font=(mono, size))
        b.tag_configure("md-zebra", background=p["panel"])
        b._md_char_px = self.font.measure("0")
        from agentdesk import charts
        b._md_chart_theme = charts.theme_from_palette(p)
        b.tag_raise("rcpt")
        self.lines_view.tag_raise("cur")
        _titlebar(self.root, p)
        self._clear_screens()

    def _clear_screens(self) -> None:
        """Forget what's painted AND wipe it, so the next paint can't append a second copy."""
        for t in (self.top, self.bar, self.lines_view, self.read_view):
            t.delete("1.0", "end")
        self._painted, self._top_painted, self._bar_painted = [], [], []
        self._reader_key = None

    def _focus_body(self) -> None:
        self.body.focus_set()

    def _on_resize(self, _e=None) -> None:
        width = max(self.lines_view.winfo_width() - 24, 200)
        cols = max(64, width // max(1, self.bold.measure("M")) - 2)
        rows = self._visible_rows(fixed=0)
        if cols != self.cols or rows != getattr(self, "_rows_seen", None):
            log.debug("resize: %s -> %s columns, %s visible lines (%spx)", self.cols, cols, rows, width)
            self.cols = cols
            self._rows_seen = rows
            self._clear_screens()
            self.root.after_idle(self.render)

    # --- data ----------------------------------------------------------------

    @guarded
    def refresh(self, conn: sqlite3.Connection, open_qs: list) -> None:
        """Called by App when the database changed. Reloads the in-memory board."""
        t0 = time.perf_counter()
        self._refresh(conn, open_qs)
        log.info("refresh %.1fms: questions=%d discussion=%d wiki=%d work=%d prs=%d ringing=%d callers=%d cached=%d",
                 (time.perf_counter() - t0) * 1000, *(len(self.rows[c]) for c in ("question", "discussion", "wiki", "work")),
                 len(self.prs), len(self.open_qs), len(self.callers), len(self._threads))

    def _refresh(self, conn: sqlite3.Connection, open_qs: list) -> None:
        self.open_qs = open_qs
        for ch in paths.CHANNELS:
            only_archived = ch == "question" and self.show_archived
            self.rows[ch] = db.list_threads(
                conn, channel=ch, limit=200,
                status=paths.STATUS_ARCHIVED if only_archived else None,
                include_archived=(ch != "question"))
        if not self.show_archived:  # ringing first, then answered, each newest first
            self.rows["question"].sort(key=lambda r: not waiting("question", r))
        self.prs = db.list_prs(conn, include_settled=self.show_settled)
        stamps = {r["id"]: r["updated_ts"] for rows in self.rows.values() for r in rows}
        for tid in list(self._threads):
            if stamps.get(tid) != self._threads[tid]["thread"]["updated_ts"]:
                del self._threads[tid]
        self._load_recent(conn)
        self._load_callers(conn)
        row = conn.execute(
            "SELECT id, subject, updated_ts FROM threads WHERE channel='question'"
            " AND status=? ORDER BY updated_ts DESC LIMIT 1", (paths.STATUS_ARCHIVED,)).fetchone()
        self.last_filed = dict(row) if row else None
        self.tick(conn, render=False)
        self.render()
        if getattr(self, "_read_conn", None) is None:
            self.root.after_idle(self._reader)  # open it now, not on the first message you read
        self._prefetch()

    @guarded
    def tick(self, conn: sqlite3.Connection, render: bool = True) -> None:
        """Every poll: the clock-driven bits (worker, SlackNet, ages, usage)."""
        t0 = time.perf_counter()
        self._tick(conn, render)
        ms = (time.perf_counter() - t0) * 1000
        if ms > FRAME_MS:
            log.warning("SLOW tick %.1fms on %s (build %.1f paint %.1f, %d lines)", ms, self.screen,
                        self.stats["build_ms"], self.stats["paint_ms"], self.stats["changed"])

    def _tick(self, conn: sqlite3.Connection, render: bool = True) -> None:
        try:
            self.worker = self.app._worker_status()
        except Exception:
            self.worker = (False, "Worker: unknown")
        self.held_row, self.held_events = None, []
        item = None
        try:
            item = json.loads(paths.WORKER_STATE.read_text(encoding="utf-8")).get("item")
        except (OSError, ValueError, AttributeError):
            pass
        if not item:
            held = next((r for r in self.rows.get("work", []) if r["status"] == paths.STATUS_CLAIMED), None)
            item = held["id"] if held else None
        if item:
            try:
                self.held_row = db.work_thread(conn, int(item))
                self.held_events = db.list_work_events(conn, int(item))
            except (sqlite3.Error, ValueError):
                pass
        try:
            self.slack = json.loads((paths.DATA_DIR / "slack_bridge.state").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self.slack = None
        try:
            self.sinks = notify.disabled_sinks()
        except Exception:
            self.sinks = {}
        self.usage_lines = usage.lines()
        self.usage_summary = usage.summary()
        if render:
            self.render()

    def _load_recent(self, conn) -> None:
        rows = conn.execute(
            "SELECT m.id, m.ts, m.author, m.author_kind, m.meta, m.thread_id, t.subject, t.channel,"
            " (SELECT MIN(id) FROM messages WHERE thread_id=m.thread_id) = m.id AS first"
            " FROM messages m JOIN threads t ON t.id = m.thread_id ORDER BY m.id DESC LIMIT 40").fetchall()
        recent = []
        for r in rows:
            kind = _meta(r["meta"]).get("kind")
            if kind in ("ack", "ack-note"):
                continue
            recent.append(dict(r))
            if len(recent) >= 5:
                break
        self.recent = recent
        me = conn.execute("SELECT id, ts, meta FROM messages WHERE author=? ORDER BY id DESC LIMIT 1",
                          (paths.HUMAN,)).fetchone()
        self.john_last = dict(me) if me else None
        since = self.john_last["id"] if self.john_last else 0
        n = conn.execute(
            "SELECT COUNT(*) FROM messages WHERE id > ? AND author != ? AND COALESCE("
            "CASE WHEN json_valid(meta) THEN json_extract(meta, '$.kind') END, '') NOT IN ('ack','ack-note','read-receipt')",
            (since, paths.HUMAN)).fetchone()
        self.since_posts = int(n[0]) if n else 0

    def _load_callers(self, conn) -> None:
        cutoff = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat(timespec="seconds")
        rows = conn.execute(
            "SELECT m.author, m.ts, m.meta, m.thread_id, t.subject, t.channel FROM messages m"
            " JOIN threads t ON t.id = m.thread_id"
            " JOIN (SELECT author, MAX(id) AS mid FROM messages WHERE ts >= ? AND author != ?"
            "       GROUP BY author) last ON last.mid = m.id"
            " ORDER BY m.id DESC LIMIT 24", (cutoff, paths.HUMAN)).fetchall()
        self.callers = [dict(r) for r in rows]
        self.bios = {r["subject"][5:].strip(): r["id"] for r in conn.execute(
            "SELECT id, subject FROM threads WHERE channel='discussion' AND subject LIKE 'bio: %'")}

    def _reader(self) -> sqlite3.Connection:
        # One long-lived read connection: opening a fresh one costs ~40ms on this machine.
        if getattr(self, "_read_conn", None) is None:
            self._read_conn = db.connect(self.app.db_path)
        return self._read_conn

    def _thread(self, tid: int) -> Optional[dict]:
        data = self._threads.get(tid)
        if data is None:
            t0 = time.perf_counter()
            try:
                data = db.get_thread(self._reader(), tid)
            except ValueError:
                return None
            except sqlite3.Error as exc:
                log.warning("thread #%s fetch failed, reopening the read connection: %r", tid, exc)
                self._read_conn = None
                data = db.get_thread(self._reader(), tid)
            self._threads[tid] = data
            log.debug("fetched thread #%s (%d messages) in %.1fms", tid, len(data["messages"]),
                      (time.perf_counter() - t0) * 1000)
        return data

    def _prefetch(self) -> None:
        # Debounced: a held arrow key must never pay for a database read between keys.
        if getattr(self, "_prefetch_job", None):
            self.root.after_cancel(self._prefetch_job)
        self._prefetch_job = self.root.after(180, self._prefetch_now)

    def _prefetch_now(self) -> None:
        self._prefetch_job = None
        rows = self.rows.get(self.channel, [])
        if not rows or self.screen not in ("list", "read"):
            return
        if self.screen == "read":
            at = next((i for i, r in enumerate(rows) if r["id"] == self.read_tid), 0)
        else:
            at = min(self.sel[self.channel], len(rows) - 1)
        for i in (at, at + 1, at - 1):
            if 0 <= i < len(rows) and rows[i]["id"] not in self._threads:
                self._thread(rows[i]["id"])

    def _bio(self, name: str) -> str:
        tid = self.bios.get(name)
        if not tid:
            return ""
        if tid not in self._bio_bodies:
            data = self._thread(tid)
            msgs = data["messages"] if data else []
            self._bio_bodies[tid] = msgs[0]["body"] if msgs else ""
        return self._bio_bodies[tid]

    # --- painting ------------------------------------------------------------

    def _paint_into(self, t: tk.Text, lines: list, painted: list) -> list:
        for i, segs in enumerate(lines):
            if i < len(painted) and painted[i] == segs:
                continue
            self.stats["changed"] += 1
            args = []
            for text, tags in segs:
                args += [text, tags]
            ln = i + 1
            if i < len(painted):
                t.delete(f"{ln}.0", f"{ln}.end")
                if args:
                    t.insert(f"{ln}.0", *args)
            else:
                if i > 0:
                    t.insert("end-1c", "\n")
                if args:
                    t.insert("end-1c", *args)
        if len(lines) < len(painted):
            t.delete(f"{max(len(lines), 1)}.end" if lines else "1.0", "end-1c")
        return [list(s) for s in lines]

    def _paint(self, lines: list) -> None:
        self._painted = self._paint_into(self.lines_view, lines, self._painted)

    def render(self) -> None:
        W = self.cols
        screen = self.screen
        self.stats["changed"] = 0
        t0 = time.perf_counter()
        reading = screen in ("read", "compose")
        want = self.read_view if reading else self.lines_view
        if self.body is not want:
            (self.page_read if reading else self.page_lines).tkraise()
            self.body = want
        if screen == "read":
            self._render_reader()
            t1 = time.perf_counter()
        elif screen == "compose":
            self._render_static(self._compose(W))
            t1 = time.perf_counter()
        else:
            self._click_map = {}
            lines = {"main": self._main, "list": self._list, "prs": self._prs,
                     "sysop": self._sysop, "who": self._who, "options": self._options}[screen](W)
            t1 = time.perf_counter()
            self._paint(lines)
            focus_line = getattr(self, "_see_line", None)
            if focus_line:
                self.lines_view.see(f"{focus_line}.0")
        self._top_painted = self._paint_into(self.top, [self._topline(W)], self._top_painted)
        self._bar_painted = self._paint_into(self.bar, [self._barline(W)], self._bar_painted)
        self._sync_input()
        t2 = time.perf_counter()
        self.stats["build_ms"] = (t1 - t0) * 1000
        self.stats["paint_ms"] = (t2 - t1) * 1000

    def _render_static(self, lines: list) -> None:
        t = self.read_view
        t.delete("1.0", "end")
        for segs in lines:
            args = []
            for text, tags in segs:
                args += [text, tags]
            t.insert("end-1c", *(args + ["\n", ()]))
        self._reader_key = None

    def _sync_input(self) -> None:
        if self.screen == "compose":
            if not self.subject_row.winfo_ismapped():
                self.subject_row.pack(fill="x", side="top", pady=(0, 6), before=self.reply_row)
        elif self.subject_row.winfo_ismapped():
            self.subject_row.pack_forget()

    # --- top and bottom lines ------------------------------------------------

    def _topline(self, W: int) -> list:
        title = {"main": "Main menu", "list": TITLE.get(self.channel, ""), "prs": "Pull Requests",
                 "sysop": "SysOp console", "who": "Who's on", "options": "Options",
                 "compose": f"New post in {TITLE.get(self.channel, '')}"}.get(self.screen, "")
        if self.screen == "read" and self.read_tid:
            title = f"Reading #{self.read_tid}"
        running, _text = self.worker
        left = [S("AgentDesk", "ye", "b"), S(f" · {title}", "mu")]
        if running:
            item = self.held_row["id"] if self.held_row else None
            who = holder(self.held_row) if self.held_row else ""
            mid = [S("● worker online", "gr")] + ([S(f" · #{item}", "ye")] if item else [S(" · idle", "mu")]) \
                + ([S(f" · {identity.label(who)}", "mu")] if who else [])
        else:
            mid = [S("○ worker offline", "or"), S(" · Ctrl+W starts it", "fa")]
        ringing = len(self.open_qs)
        right = [S(f"{ringing} ringing for you", "pk", "b") if ringing else S("nobody's calling", "mu"),
                 S(" · ", "fa")] + self._slack_seg()
        gap = W - seglen(left) - seglen(mid) - seglen(right)
        if gap < 4:
            return pad(left + [S("   ")] + right, W)
        a = gap // 2
        return left + [S(" " * a)] + mid + [S(" " * (gap - a))] + right

    def _slack_seg(self) -> list:
        s = self.slack
        fresh = False
        if s:
            ts = _parse(s.get("ts"))
            fresh = ts is not None and (datetime.now(timezone.utc) - ts).total_seconds() < 90
        return [S("SlackNet ● up", "cy")] if fresh else [S("SlackNet ○ down", "fa")]

    def _hints(self) -> list:
        k = lambda key, label: [S(f" {key}", "ye", "inv"), S(f" {label} ", "mu", "inv")]
        s = self.screen
        if self.confirm:
            return [S(f" {self.confirm[0]} ", "pk", "b", "inv")] + k("Y", "yes") + k("N", "no")
        if s == "main":
            return k("Q D W J", "message bases") + k("P", "PRs") + k("S", "SysOp") + k("B", "who's on") \
                + k("O", "options") + k("G", "hang up")
        if s == "list":
            h = k("↑↓", "move") + k("↵", "read") + k("N", "new post")
            if self.channel == "question":
                h += k("H", "archived" if not self.show_archived else "active") + k("Ctrl+R", "wake agent")
            return h + k("Esc", "main menu")
        if s == "read":
            h = k("type", "to reply") + k("Ctrl+↵", "send") + k("Ctrl+D", "dictate") \
                + k("PgUp/PgDn", "scroll") + k("Alt+N/P", "next/prev")
            if self.channel == "question":
                h += k("Alt+C", "close") + k("Alt+U", "bring back")
            return h + k("Esc", "back")
        if s == "prs":
            return k("↑↓", "move") + k("↵", "open on GitHub") + k("C", "check now") \
                + k("H", "settled" if not self.show_settled else "open only") + k("Esc", "menu")
        if s == "sysop":
            running = self.worker[0]
            return k("Ctrl+W", "stop worker (after this item)" if running else "start worker") \
                + k("R", "reload code") + k("J", "job board") + k("L", "read held item") + k("Esc", "menu")
        if s == "who":
            return k("↑↓", "pick a caller") + k("↵", "read bio") + k("P", "page them") + k("Esc", "menu")
        if s == "options":
            return k("↑↓", "move") + k("↵", "change") + k("←→", "adjust") + k("Esc", "menu")
        if s == "compose":
            return k("↵", "subject → body") + k("Ctrl+↵", "post") + k("Ctrl+D", "dictate") + k("Esc", "cancel")
        return []

    def _barline(self, W: int) -> list:
        if self._flash and self._flash[2] > time.monotonic():
            text, tags, _ = self._flash
            return pad([S(" " + text, *tags, "inv")], W, "inv")
        return pad(self._hints(), W, "inv")

    def flash(self, text: str, *tags) -> None:
        self._flash = (text, tags or ("fg",), time.monotonic() + FLASH_MS / 1000)
        self._bar_painted = self._paint_into(self.bar, [self._barline(self.cols)], self._bar_painted)
        if self._flash_job:
            self.root.after_cancel(self._flash_job)
        self._flash_job = self.root.after(FLASH_MS + 50, self._clear_flash)

    def _clear_flash(self) -> None:
        self._flash_job = None
        self._flash = None
        self._bar_painted = self._paint_into(self.bar, [self._barline(self.cols)], self._bar_painted)

    # --- screens -------------------------------------------------------------

    def _box(self, title: str, rows: list, W: int) -> list:
        inner = W - 4
        out = [[S("┌─ ", "rule"), S(title, "mu"), S(" " + "─" * max(0, W - 5 - len(title)) + "┐", "rule")]]
        for segs in rows:
            segs = list(segs)
            if seglen(segs) > inner:
                segs = clip(segs, inner)
            out.append([S("│ ", "rule")] + pad(segs, inner) + [S(" │", "rule")])
        out.append([S("└" + "─" * (W - 2) + "┘", "rule")])
        return out

    def _main(self, W: int) -> list:
        L = []
        top = []
        bot = []
        for i, (a, b) in enumerate(LOGO):
            top += [S(a, LOGO_HUES[i], "b"), S(" ")]
            bot += [S(b, LOGO_HUES[i], "b"), S(" ")]
        L.append([S("   ")] + top + [S("  6 lines · no long-distance fees", "mu")])
        L.append([S("   ")] + bot + [S("  please do not tie up the line", "fa")])
        L.append([])
        tail = ("  (you heard that. we all heard that.)" if self.prefs.get("screech")
                else "  (handshake screech omitted for your comfort)")
        L.append([S("ATDT AGENTDESK ... ", "fa"), S("CONNECT", "gr", "b"), S(tail, "fa")])
        L.append([])
        if self.john_last:
            via = _meta(self.john_last.get("meta")).get("via")
            where = "from your phone via SlackNet" if via == "slack" else "from this terminal"
            L.append([S(" Welcome back, "), S(paths.HUMAN.upper(), "ye", "b"),
                      S(f". Last call {when(self.john_last['ts'])}, "), S(where, "cy"), S(".")])
        else:
            L.append([S(" First call? Pull up a chair, "), S(paths.HUMAN.upper(), "ye", "b"), S(".")])
        ringing = len(self.open_qs)
        prs_open = sum(1 for r in self.prs if r["state"] == paths.PR_OPEN)
        line = [S(" Since then: ")]
        line.append(S(f"{ringing} question{'s' if ringing != 1 else ''}", "pk", "b") if ringing
                    else S("no questions", "mu"))
        line.append(S(" rang for you, " if ringing else " rang, "))
        line.append(S(f"{prs_open} PR{'s' if prs_open != 1 else ''}", "gr") if prs_open else S("no PRs", "mu"))
        line.append(S(" want a merge, " if prs_open != 1 else " wants a merge, "))
        line.append(S(f"{self.since_posts} post{'s' if self.since_posts != 1 else ''} landed", "mu"))
        line.append(S("."))
        L.append(line)
        L.append([])

        work = self.rows["work"]
        w_open = sum(1 for r in work if r["status"] == paths.STATUS_OPEN)
        w_held = sum(1 for r in work if r["status"] == paths.STATUS_CLAIMED)
        running = self.worker[0]
        if running and self.held_row:
            sysop = (f"worker on #{self.held_row['id']}", "gr")
        elif running:
            sysop = ("worker idle", "gr")
        else:
            sysop = ("offline · Ctrl+W starts it", "or")
        agents = len({c["author"] for c in self.callers if c["author"] != paths.HUMAN})
        theme = self.pal["label"] + (" · screech on" if self.prefs.get("screech") else "")
        items = [
            ("Q", "Questions", (f"{ringing} ringing", "pk", "b") if ringing else ("all quiet", "mu")),
            ("J", "Work to Hire", (f"{w_open} open · {w_held} held", "cy")),
            ("D", "Discussion", (f"{len(self.rows['discussion'])} threads", "fg")),
            ("P", "Pull Requests", (f"{prs_open} to merge", "gr") if prs_open else ("nothing to merge", "mu")),
            ("W", "Wiki", (f"{len(self.rows['wiki'])} articles", "mu")),
            ("S", "SysOp console", sysop),
            ("B", "Who's on", (f"{agents} agent{'s' if agents != 1 else ''} today", "pu")),
            ("O", "Options", (theme, "ye")),
            ("G", "Hang up", ("to the tray", "mu")),
        ]
        colw = max(38, (W - 2) // 2)

        def item(key, label, val):
            text, *tags = val
            left = f"[{key}] {label} "
            dots = "." * max(2, 18 - len(left))
            return [S("[", "mu"), S(key, "ye", "b"), S("] ", "mu"), S(label + " "), S(dots, "rule"),
                    S(" "), S(text, *tags)]

        for i in range(0, len(items), 2):
            left = pad([S(" ")] + item(*items[i]), colw)
            right = item(*items[i + 1]) if i + 1 < len(items) else []
            L.append(left + right)
        L.append([])

        rows = []
        for r in self.recent:
            kind = _meta(r["meta"]).get("kind")
            if kind == "read-receipt":
                verb = "picked up"
            elif r["first"]:
                verb = "opened"
            elif r["author"] == paths.HUMAN and r["channel"] == "question":
                verb = "answered"
            else:
                verb = "replied on"
            via = " via SlackNet" if _meta(r["meta"]).get("via") == "slack" else ""
            rows.append([S(fit(when(r["ts"]), 9), "fa"), S(fit(identity.label(r["author"]), 18), author_hue(r["author"])),
                         S(f"{verb} "), S(f"#{r['thread_id']}", "ye"), S(" " + r["subject"], "mu"),
                         S(via, "pu")])
        if not rows:
            rows.append([S("Nobody has called yet. The line is open.", "mu")])
        L += self._box("Recent callers", rows, W)
        L.append([])
        phrases = self.usage_lines
        if phrases:
            phrase = phrases[int(time.time() // USAGE_ROTATE_S) % len(phrases)]
            hot = "week" in phrase
            L.append([S(" (" + phrase + ")", "or" if hot else "fa")])
        else:
            L.append([S(" (time left: unlimited, you're the SysOp)", "fa")])
        L.append([S(" Main menu ", "fg"), S("[", "mu"), S("Q,D,W,J,P,S,B,O,G", "ye"), S("]", "mu"),
                  S(": "), S(" ", "cur")])
        self._see_line = None
        return L

    def _list(self, W: int) -> list:
        ch = self.channel
        rows = self.rows[ch]
        L = [pad([S(" " + BANNER[ch], "barcy", "b")], W, "barcy")]
        if ch == "question" and self.show_archived:
            L[0] = pad([S(" QUESTIONS  ·  the archive  ·  settled and filed to the vault", "barcy", "b")], W, "barcy")
        L.append([])
        q = ch == "question"
        subj_w = max(20, W - (56 if q else 44))
        by_label = "HELD BY" if ch == "work" else "FROM"
        L.append([S("    #  ST    " + fit("SUBJECT", subj_w) + " " + fit(by_label, 16) + (fit("LAST WORD", 12) if q else "")
                    + fit("WHEN", 11) + " MSG", "mu")])
        L.append([S("─" * W, "rule")])
        if not rows:
            L.append([S("   Nothing here yet. ", "mu"), S("N", "ye"), S(" starts the first thread.", "mu")])
        sel = min(self.sel[ch], max(0, len(rows) - 1))
        self.sel[ch] = sel
        first_row_line = len(L) + 1
        visible = self._visible_rows(fixed=7 if q else 6)
        top = self.top_row[ch]
        if sel < top:
            top = sel
        elif sel >= top + visible:
            top = sel - visible + 1
        top = max(0, min(top, max(0, len(rows) - visible)))
        self.top_row[ch] = top
        self._window = (top, min(len(rows), top + visible), len(rows))
        offset = 0
        for i in range(top, min(len(rows), top + visible)):
            r = rows[i]
            if q and not self.show_archived and i > 0 and waiting(ch, rows[i - 1]) and not waiting(ch, r):
                L.append([S(f" ── answered · stays here {db.ANSWERED_GRACE_HOURS}h after the last reply,"
                            " then files to the vault ", "fa")])
                offset = 1
            code, ctags = state_code(ch, r)
            last = ""
            if q:
                la = r["last_author"] if "last_author" in r.keys() else None
                last = fit(delivery_mark(r) if la == paths.HUMAN else ("↩ " + identity.label(la) if la else "—"), 12)
            if ch == "work":
                who = holder(r)
                by = identity.label(who) if who else "—"
            else:
                by = identity.label(r["opened_by"])
            subj_tags = ("fg", "b") if code == "WAIT" else (("fg",) if code in ("OPEN", "HELD", "live") else ("mu",))
            if i == sel:
                text = f" ▶{r['id']:>3}  {code}  {fit(r['subject'], subj_w)} {fit(by, 16)}{last}{fit(when(r['updated_ts']), 11)} {r['message_count']:>3}"
                L.append(pad([S(text, "cur")], W, "cur"))
            else:
                L.append([S(f"  {r['id']:>3}  ", "ye"), S(code, *ctags), S("  "),
                          S(fit(r["subject"], subj_w), *subj_tags), S(" "), S(fit(by, 16), author_hue(r["opened_by"])),
                          S(last, "pk" if " ! " in last else ("gr" if last.startswith("you") else "cy")),
                          S(fit(when(r["updated_ts"]), 11), "fa"), S(f" {r['message_count']:>3}", "mu")])
            self._click_map[first_row_line + i - top + offset] = i
        L.append([S("─" * W, "rule")])
        footer = self._list_footer(ch, rows)
        if len(rows) > visible:
            footer = footer + [S(f" · rows {top + 1}–{min(len(rows), top + visible)} of {len(rows)}", "fa")]
        L.append(footer)
        self._see_line = None
        return L

    def _visible_rows(self, fixed: int) -> int:
        lines = self.lines_view.winfo_height() // max(1, self.font.metrics("linespace"))
        return max(5, lines - fixed - 1)

    def _list_footer(self, ch: str, rows: list) -> list:
        if ch == "question":
            if self.show_archived:
                return [S(f" {len(rows)} archived. ", "mu"), S("U", "ye"), S(" on one brings it back.", "mu")]
            ringing = [r for r in rows if waiting("question", r)]
            settled = len(rows) - len(ringing)
            out = [S(f" {len(ringing)} still ringing", "pk", "b") if ringing else S(" nothing ringing", "gr")]
            if ringing:
                oldest = min(ringing, key=lambda r: r["updated_ts"])
                out.append(S(f" · oldest waiting {ago(oldest['updated_ts'])}", "mu"))
            out.append(S(f" · {settled} answered, kept {db.ANSWERED_GRACE_HOURS}h after the last reply", "mu"))
            return out
        if ch == "work":
            counts = {}
            for r in rows:
                counts[r["status"]] = counts.get(r["status"], 0) + 1
            return [S(f" {counts.get(paths.STATUS_OPEN, 0)} open", "ye"), S(" · "),
                    S(f"{counts.get(paths.STATUS_CLAIMED, 0)} held", "cy"), S(" · "),
                    S(f"{counts.get(paths.STATUS_DONE, 0)} done", "gr")]
        newest = rows[0]["updated_ts"] if rows else None
        return [S(f" {len(rows)} thread{'s' if len(rows) != 1 else ''}", "mu")] + \
            ([S(f" · newest post {ago(newest)} ago", "mu")] if newest else [])

    def _render_reader(self) -> None:
        data = self._thread(self.read_tid) if self.read_tid else None
        W = self.cols
        if data is None:
            self._render_static([[S(" That thread is no longer on the board.", "mu")]])
            return
        thread, msgs = data["thread"], data["messages"]
        key = (self.read_tid, len(msgs), thread["status"], thread["updated_ts"], W, self.prefs["theme"])
        if key == self._reader_key:
            return
        grew = self._reader_key is not None and self._reader_key[0] == self.read_tid \
            and len(msgs) > self._reader_key[1]
        opened = self._reader_key is None or self._reader_key[0] != self.read_tid
        self._reader_key = key
        t = self.read_view
        t.delete("1.0", "end")
        ch = thread["channel"]
        code, ctags = state_code(ch, thread)
        status_word = {"WAIT": "WAITING ON YOU", "OPEN": "up for grabs", "HELD": "held",
                       "DONE": "done", "ansd": "answered", "clsd": "closed", "arch": "archived"}.get(code, code.strip())
        slack = any(_meta(m["meta"]).get("via") == "slack" for m in msgs)
        idx = next((i for i, r in enumerate(self.rows.get(ch, [])) if r["id"] == self.read_tid), None)
        pos = f"{idx + 1} of {len(self.rows[ch])} in {TITLE.get(ch, ch)}" if idx is not None else TITLE.get(ch, ch)

        def put(segs):
            args = []
            for text, tags in segs:
                args += [text, tags]
            args += ["\n", ()]
            t.insert("end-1c", *args)

        head = f"═ Msg #{thread['id']} ═ {pos} "
        put([S("╔", "rule"), S(head, "ye", "b"), S("═" * max(0, W - 2 - len(head)) + "╗", "rule")])
        put([S("  From: ", "mu"), S(fit(identity.describe(thread["opened_by"]), 30), author_hue(thread["opened_by"])),
             S("To: ", "mu"), S(fit(paths.HUMAN if ch == "question" else "everyone", 12), "ye"),
             S("Status: ", "mu"), S(status_word, *ctags)])
        subj = thread["subject"]
        width = W - 10
        chunks = [subj[i:i + width] for i in range(0, len(subj), width)] or [""]
        for n, chunk in enumerate(chunks):
            put([S("  Subj: " if n == 0 else "        ", "mu"), S(chunk, "fg", "b")])
        extra = []
        if ch == "work" and holder(thread):
            extra = [S("   Held by: ", "mu"), S(identity.label(holder(thread)), "cy")]
        put([S("  Date: ", "mu"), S(fit(when(thread["created_ts"]), 16)),
             S("Replies: ", "mu"), S(str(max(0, len(msgs) - 1)))] + extra +
            ([S("   Echo: ", "mu"), S("SlackNet", "pu")] if slack else []))
        put([S("╚" + "═" * (W - 2) + "╝", "rule")])
        for i, m in enumerate(msgs):
            start = t.index("end-1c")
            meta = _meta(m["meta"])
            receipt = meta.get("kind") in RECEIPT_KINDS
            if receipt:
                verb = "picked it up"
            elif i == 0:
                verb = "wrote"
            elif m["author"] == paths.HUMAN and ch == "question":
                verb = "answered"
            else:
                verb = "replied"
            via = "from the phone, via SlackNet" if meta.get("via") == "slack" else ""
            label = identity.label(m["author"])
            text = f" ─── {label} {verb} {when(m['ts'])}{(' ' + via) if via else ''} "
            put([S(" ───", "rule"), S(" " + label, author_hue(m["author"]), "b"), S(f" {verb} {when(m['ts'])}", "mu"),
                 S((" " + via) if via else "", "pu"), S(" " + "─" * max(0, W - len(text) - 1), "rule")])
            mdview.render(t, m["body"])
            if receipt:
                t.tag_add("rcpt", start, t.index("end-1c"))
        if opened or grew:
            t.see("end-1c")

    def _prs(self, W: int) -> list:
        L = [pad([S(" PULL REQUESTS  ·  the merge desk  ·  clears itself once GitHub says merged", "barcy", "b")], W, "barcy"), []]
        title_w = max(20, W - 58)
        L.append([S("  ST     " + fit("REPO#", 22) + fit("TITLE", title_w) + " " + fit("ASKED BY", 14) + fit("CHECKED", 11), "mu")])
        L.append([S("─" * W, "rule")])
        rows = self.prs
        sel = min(self.sel_prs, max(0, len(rows) - 1))
        self.sel_prs = sel
        start = len(L) + 1
        for i, r in enumerate(rows):
            st = {"open": ("OPEN", "gr"), "merged": ("mrgd", "pu"), "closed": ("clsd", "fa")}.get(r["state"], (r["state"][:4], "mu"))
            checked = "failed" if r["last_error"] else (when(r["checked_ts"]) if r["checked_ts"] else "never")
            by = identity.label(r["requested_by"]) + (" (scan)" if r.get("source") == paths.PR_SOURCE_SCAN else "")
            ref = f"{r['repo']}#{r['number']}"
            if i == sel:
                L.append(pad([S(f" ▶{st[0]}   {fit(ref, 22)}{fit(r['title'], title_w)} {fit(by, 14)}{fit(checked, 11)}", "cur")], W, "cur"))
            else:
                L.append([S(f"  {st[0]}   ", st[1]), S(fit(ref, 22), "cy"), S(fit(r["title"], title_w)), S(" "),
                          S(fit(by, 14), "mu"), S(fit(checked, 11), "pk" if r["last_error"] else "fa")])
            self._click_map[start + i] = i
        if not rows:
            L.append([S("   Nothing waiting on you. Agents add PRs here with request_merge.", "mu")])
        L.append([S("─" * W, "rule")])
        if rows:
            r = rows[sel]
            L.append([])
            L.append([S(" " + r["title"], "fg", "b")])
            L.append([S(" " + r["url"], "cy")])
            if r.get("triage"):
                L.append([S(" triage: ", "mu"), S(r["triage"])])
            if r["last_error"]:
                L.append([S(" last check failed: ", "pk"), S(r["last_error"], "mu")])
                L.append([S(" it stays on the list; only GitHub saying merged removes it.", "fa")])
            if r["thread_id"]:
                L.append([S(f" a notice goes to thread #{r['thread_id']} when it merges.", "fa")])
        self._see_line = start + sel if rows else None
        return L

    def _sysop(self, W: int) -> list:
        L = [pad([S(" SYSOP CONSOLE  ·  " + ("phone's ringing off the hook" if self.open_qs else "waiting for callers"), "bar", "b")], W, "bar"), []]

        def stat(k, segs):
            L.append([S(" " + fit(k, 20), "mu")] + segs)

        running, text = self.worker
        if running and self.held_row:
            started = next((e for e in self.held_events if e["kind"] == db.WORK_START), None)
            dur = f" for {ago(started['ts'])}" if started else ""
            who = holder(self.held_row)
            stat("Worker", [S(f"● online · on #{self.held_row['id']}{dur}", "gr")] +
                 ([S(f" · {identity.label(who)}", "mu")] if who else []))
        elif running:
            stat("Worker", [S("● online · idle, the queue is empty", "gr")])
        else:
            stat("Worker", [S("○ offline", "or"), S("  Ctrl+W starts it", "fa")])
        s = self.slack
        if s and self._slack_seg()[0][1] == ("cy",):
            relay = s.get("last_relay")
            stat("SlackNet echo", [S("● up", "gr"), S(f" · polling every {s.get('poll_s', 15)}s", "mu")] +
                 ([S(f" · last relay {when(relay.get('ts'))} (#{relay.get('thread_id')} to your phone)", "mu")] if relay else []))
        else:
            seen = f", last heard {ago(s.get('ts'))} ago" if s else ""
            stat("SlackNet echo", [S(f"○ down{seen}", "or"), S("  replies from your phone won't arrive", "fa")])
        disabled = self.sinks
        stat("Notifier sinks", [S(f"⚠ {len(disabled)} disabled: " + ", ".join(disabled), "pk")] if disabled
             else [S("all delivering", "mu")])
        ringing = len(self.open_qs)
        prs_open = sum(1 for r in self.prs if r["state"] == paths.PR_OPEN)
        if ringing:
            oldest = min(self.open_qs, key=lambda q: q["updated_ts"])
            stat("Ringing for john", [S(f"{ringing} question{'s' if ringing != 1 else ''}", "pk", "b"),
                                      S(f", oldest {ago(oldest['updated_ts'])}", "mu"),
                                      S(f" · {prs_open} PR{'s' if prs_open != 1 else ''} to merge", "gr" if prs_open else "mu")])
        else:
            stat("Ringing for john", [S("nothing", "gr"), S(f" · {prs_open} PR{'s' if prs_open != 1 else ''} to merge", "mu")])
        if self.last_filed:
            stat("Filed to the vault", [S(f"{when(self.last_filed['updated_ts'])} · #{self.last_filed['id']} ", "mu"),
                                        S(self.last_filed["subject"], "fa")])
        stat("Claude plan", [S(self.usage_summary, "mu")])
        L.append([])
        if self.held_row:
            label, lines = activity(self.held_row, self.held_events)
            rows = [[S(label, "mu")]]
            hue = {"ev-start": "cy", "ev-step": "fg", "ev-output": "mu", "ev-done": "gr", "ev-error": "pk"}
            word = {"ev-start": "START ", "ev-step": "step  ", "ev-output": "said  ", "ev-done": "DONE  ",
                    "ev-error": "ERROR "}
            for clock, body, tag in lines[-10:]:
                h = hue.get(tag, "mu")
                rows.append([S(fit(clock, 9), "fa"), S(word.get(tag, "      "), h, "b"), S(body, h)])
            L += self._box(f"Activity · #{self.held_row['id']} {self.held_row['subject']}"[: W - 8], rows, W)
        else:
            L += self._box("Activity", [[S("Nothing is being worked right now.", "mu")]], W)
        L.append([])
        jobs = [r for r in self.rows["work"] if r["status"] in (paths.STATUS_OPEN, paths.STATUS_CLAIMED)][:8]
        jrows = []
        for r in jobs:
            code, ctags = state_code("work", r)
            who = holder(r)
            jrows.append([S(f"#{r['id']:<5}", "ye"), S(code, *ctags), S("  "), S(fit(r["subject"], W - 34)),
                          S(" "), S(fit(identity.label(who) if who else "—", 14), "mu")])
        if not jrows:
            jrows = [[S("The job board is empty.", "mu")]]
        L += self._box("Work to Hire queue", jrows, W)
        self._see_line = None
        return L

    def _who_rows(self) -> list:
        return [c for c in self.callers if c["author"] != paths.HUMAN]

    def _who(self, W: int) -> list:
        callers = self._who_rows()
        n = len(callers) + 1
        busy = "  ·  ALL LINES BUSY" if n >= 6 else ""
        L = [pad([S(f" WHO'S ON  ·  {n} line{'s' if n != 1 else ''} in use today{busy}", "barcy", "b")], W, "barcy"), []]
        doing_w = max(20, W - 44)
        L.append([S(" LINE  " + fit("HANDLE", 22) + fit("DOING", doing_w) + "   SEEN", "mu")])
        L.append([S("─" * W, "rule")])
        L.append([S("    1  ", "ye"), S(fit(f"{paths.HUMAN} (SysOp)", 22), "ye", "b"),
                  S(fit("reading the Who's On list", doing_w)), S("     now", "fa")])
        held_by = {}
        for r in self.rows["work"]:
            if r["status"] == paths.STATUS_CLAIMED and holder(r):
                held_by[holder(r)] = r
        sel = min(self.sel_who, max(0, len(callers) - 1))
        self.sel_who = sel
        start = len(L) + 1
        for i, c in enumerate(callers):
            name = c["author"]
            held = held_by.get(name)
            kind = _meta(c["meta"]).get("kind")
            idle_s = (datetime.now(timezone.utc) - (_parse(c["ts"]) or datetime.now(timezone.utc))).total_seconds()
            if held:
                doing = f"on #{held['id']} {held['subject']}"
            elif idle_s > 15 * 60:
                doing = f"on hold · last seen on #{c['thread_id']}"
            elif kind == "read-receipt":
                doing = f"reading #{c['thread_id']} {c['subject']}"
            else:
                doing = f"posted to #{c['thread_id']} {c['subject']}"
            seen = ago(c["ts"])
            if i == sel:
                L.append(pad([S(f" ▶{i + 2:>3}  {fit(identity.label(name), 22)}{fit(doing, doing_w)} {seen:>7}", "cur")], W, "cur"))
            else:
                L.append([S(f"  {i + 2:>3}  ", "ye"), S(fit(identity.label(name), 22), author_hue(name), "b"),
                          S(fit(doing, doing_w), "fg" if idle_s <= 15 * 60 else "mu"), S(f" {seen:>7}", "fa")])
            self._click_map[start + i] = i
        if not callers:
            L.append([S("   No agents have called in today.", "mu")])
        L.append([S("─" * W, "rule")])
        L.append([])
        if callers:
            name = callers[sel]["author"]
            bio = self._bio(name)
            key = next((k for k in self.bios if k.lower() == name.lower()), None)
            if key and not bio:
                bio = self._bio(key)
            rows = [[S(l)] for l in (bio.strip().splitlines()[:6] if bio else [])] or \
                [[S(f"{identity.label(name)} hasn't posted a bio yet.", "mu")]]
            L += self._box(f"bio: {identity.label(name)}", rows, W)
        L.append([])
        L.append([S(" P", "ye"), S(" pages the highlighted caller: a mention they read on their next poll.", "mu")])
        L.append([S(" (It will not beep their pager. They do not have pagers. We checked.)", "fa")])
        self._see_line = start + sel if callers else None
        return L

    def _option_items(self) -> list:
        return [
            ("Theme", f"{self.pal['label']}   ({self.theme_order.index(self.prefs['theme']) + 1} of "
                      f"{len(self.theme_order)}, ←/→ to browse, from your VS Code themes)", "theme"),
            ("Modem screech on connect", "ON" if self.prefs.get("screech") else "off", "screech"),
            ("Play the screech now", "↵", "play"),
            ("Font size", f"{self.prefs.get('font_size', 11)} pt   (←/→ or Ctrl +/-)", "font"),
            ("Dictation pre-roll", ("ON" if self.prefs.get("preroll", True) else "off")
             + "   keeps the last 2 s in RAM while a box has focus, so Ctrl+D catches what you just said",
             "preroll"),
        ]

    def _options(self, W: int) -> list:
        L = [pad([S(" OPTIONS  ·  the SysOp's control panel", "bar", "b")], W, "bar"), []]
        items = self._option_items()
        self.sel_opt = min(self.sel_opt, len(items) - 1)
        start = len(L) + 1
        for i, (label, value, _key) in enumerate(items):
            if i == self.sel_opt:
                L.append(pad([S(f" ▶ {fit(label, 28)} {value}", "cur")], W, "cur"))
            else:
                L.append([S("   " + fit(label, 28), "fg"), S(" " + value, "ye" if value in ("ON",) else "mu")])
            self._click_map[start + i] = i
        L.append([])
        L.append([S(" Settings live in ", "fa"), S(str(paths.DATA_DIR / "settings.json"), "mu")])
        L.append([S(" The screech is synthesized from its parts (dial tone, DTMF, 2100 Hz answer tone,", "fa")])
        L.append([S(" V.21 chirps, training noise). No 56k modems were harmed.", "fa")])
        self._see_line = None
        return L

    def _compose(self, W: int) -> list:
        return [pad([S(f" NEW POST  ·  {TITLE.get(self.channel, '')}", "barcy", "b")], W, "barcy"), [],
                [S(" Type a subject, press ", "mu"), S("Enter", "ye"), S(", write the message, then ", "mu"),
                 S("Ctrl+Enter", "ye"), S(" to post it.", "mu")],
                [S(" Markdown works. So do @mentions: an agent named after the @ sees it on its next poll.", "fa")],
                [S(" Rather talk? Put the cursor in a box and press ", "fa"), S("Ctrl+D", "ye"),
                 S(". Speech-to-text runs locally on this PC; nothing is sent anywhere.", "fa")]]

    # --- navigation ------------------------------------------------------------

    def goto(self, screen: str, channel: Optional[str] = None) -> None:
        if channel:
            self.channel = channel
        self.screen = screen
        self.confirm = None
        self.lines_view.yview_moveto(0)
        self.render()
        self._prefetch()
        if screen == "read":
            self.reply.focus_set()
            self.reply.mark_set("insert", "end-1c")
        elif screen != "compose":
            self._focus_body()

    def go_back(self) -> None:
        if self.screen == "read":
            self.goto(self.read_back)
        elif self.screen == "compose":
            self.subject_ent.delete(0, "end")
            self.reply.delete("1.0", "end")
            self.goto("list")
        else:
            self.goto("main")

    def open_thread(self, tid: int, back: str = "list") -> None:
        self.read_tid = tid
        self.read_back = back
        data = self._thread(tid)
        if data:
            self.channel = data["thread"]["channel"]
        self._reader_key = None
        self.goto("read")
        self._prefetch()

    def _move(self, delta: int, page: bool = False) -> None:
        step = delta * (max(5, self._visible_rows(fixed=6) - 1) if page else 1)
        if self.screen == "list":
            n = len(self.rows[self.channel])
            self.sel[self.channel] = max(0, min(n - 1, self.sel[self.channel] + step))
        elif self.screen == "prs":
            self.sel_prs = max(0, min(len(self.prs) - 1, self.sel_prs + step))
        elif self.screen == "who":
            self.sel_who = max(0, min(len(self._who_rows()) - 1, self.sel_who + step))
        elif self.screen == "options":
            self.sel_opt = max(0, min(len(self._option_items()) - 1, self.sel_opt + step))
        self.render()
        self._prefetch()

    def _activate(self) -> None:
        s = self.screen
        if s == "list":
            rows = self.rows[self.channel]
            if rows:
                self.open_thread(rows[self.sel[self.channel]]["id"], back="list")
        elif s == "prs" and self.prs:
            webbrowser.open(self.prs[self.sel_prs]["url"])
            self.flash("Opened on GitHub. The list clears itself once it's merged.", "cy")
        elif s == "who":
            callers = self._who_rows()
            if callers:
                name = callers[self.sel_who]["author"]
                tid = self.bios.get(name) or next((v for k, v in self.bios.items() if k.lower() == name.lower()), None)
                if tid:
                    self.open_thread(tid, back="who")
                else:
                    self.flash(f"{identity.label(name)} hasn't posted a bio.", "mu")
        elif s == "options":
            self._change_option(0)

    def _change_option(self, delta: int) -> None:
        key = self._option_items()[self.sel_opt][2]
        if key == "theme":
            i = self.theme_order.index(self.prefs["theme"])
            self.set_theme(self.theme_order[(i + (delta or 1)) % len(self.theme_order)])
        elif key == "screech":
            self.prefs["screech"] = not self.prefs.get("screech")
            settings.save(self.prefs)
            if not self.prefs["screech"]:
                screech.stop()
            self.flash("Screech on. Brace yourself." if self.prefs["screech"] else "Screech off. The neighbours thank you.", "ye")
        elif key == "play":
            threading.Thread(target=screech.play, daemon=True).start()
            self.flash("EEEEEEEE-KSSSHHH-BWONG-BWONG-KSSSHHHHH", "or", "b")
        elif key == "font":
            self.zoom(delta or 1)
            return
        elif key == "preroll":
            self.prefs["preroll"] = not self.prefs.get("preroll", True)
            settings.save(self.prefs)
            try:
                self.app.dictation.enable_preroll(self.prefs["preroll"])
            except Exception:
                log.exception("toggling dictation pre-roll failed")
            self.flash("Pre-roll on: the mic keeps a 2-second rolling buffer while you're in a box."
                       if self.prefs["preroll"] else "Pre-roll off: the mic only opens when you press Ctrl+D.", "ye")
        self.render()

    def set_theme(self, key: str) -> None:
        self.prefs["theme"] = key
        settings.save(self.prefs)
        self.pal = self.palettes[key]
        self._apply_theme()
        self.render()

    def zoom(self, delta: int) -> None:
        size = max(8, min(22, int(self.prefs.get("font_size", 11)) + delta))
        self.prefs["font_size"] = size
        settings.save(self.prefs)
        self.font.configure(size=size)
        self.bold.configure(size=size)
        self._apply_theme()
        self.cols = 0
        self._on_resize()

    def _send(self, _e=None) -> str:
        body = self.reply.get("1.0", "end-1c").strip()
        if not body:
            self.flash("Nothing to send. The line stays quiet.", "mu")
            return "break"
        if self.screen == "compose":
            subject = self.subject_ent.get().strip() or "(no subject)"
            conn = db.connect(self.app.db_path)
            try:
                tid = db.start_thread(conn, self.channel, subject, paths.HUMAN, paths.HUMAN_KIND, body)
            finally:
                conn.close()
            self.subject_ent.delete(0, "end")
            self.reply.delete("1.0", "end")
            self.app.refresh_now()
            self.open_thread(tid, back="list")
            self.flash(f"Posted #{tid}.", "gr")
        elif self.screen == "read" and self.read_tid:
            self.app.post_reply(self.channel, self.read_tid, body)
            self.reply.delete("1.0", "end")
            self._threads.pop(self.read_tid, None)
            self._reader_key = None
            self.render()
            self.flash(f"Sent to #{self.read_tid}.", "gr")
        return "break"

    def toggle_worker(self) -> None:
        running = self.worker[0]
        log.info("worker toggle requested (was %s)", "running" if running else "stopped")
        self.app._toggle_worker()
        self.flash("Stop requested. It finishes the item it holds first." if running
                   else "Starting the worker...", "ye")

    def wake_selected(self) -> None:
        """Ctrl+R: resume the asking agent's session with your reply (a human decides each wake)."""
        tid = self.read_tid if self.screen == "read" else None
        if tid is None and self.screen == "list" and self.channel == "question" and self.rows["question"]:
            tid = self.rows["question"][min(self.sel["question"], len(self.rows["question"]) - 1)]["id"]
        if tid is None:
            self.flash("Ctrl+R wakes the agent on a question: pick one first.", "ye")
            return
        from agentdesk import wake
        conn = db.connect(self.app.db_path)
        try:
            said = wake.wake(conn, tid)
        finally:
            conn.close()
        log.info("wake #%s: %s", tid, said)
        self.flash(said, "gr" if said.startswith("woke") else "ye")

    def reader_step(self, delta: int) -> None:
        rows = self.rows.get(self.channel, [])
        idx = next((i for i, r in enumerate(rows) if r["id"] == self.read_tid), None)
        if idx is None or not rows:
            return
        j = max(0, min(len(rows) - 1, idx + delta))
        if j == idx:
            self.flash("That's the " + ("last" if delta > 0 else "first") + " one.", "mu")
            return
        self.sel[self.channel] = j
        self.open_thread(rows[j]["id"], back=self.read_back)

    def reader_close(self) -> None:
        if self.channel != "question" or not self.read_tid:
            self.flash("Only questions can be closed & archived.", "mu")
            return
        tid = self.read_tid
        self.confirm = (f"Close & archive #{tid}? It goes to the vault on the next sweep.",
                        lambda: (self.app.close_question(tid), self.flash(f"#{tid} closed. The sweep files it.", "gr")))
        self._focus_body()
        self.render()

    def reader_unarchive(self) -> None:
        if self.channel == "question" and self.read_tid:
            self.app.unarchive_question(self.read_tid)
            self.flash(f"#{self.read_tid} is back on the desk.", "gr")

    def _page(self) -> None:
        callers = self._who_rows()
        if not callers:
            return
        name = callers[self.sel_who]["author"]
        self.channel = "discussion"
        self.goto("compose")
        self.subject_ent.insert(0, f"page: {identity.label(name)}")
        self.reply.insert("1.0", f"@{name} ")
        self.reply.focus_set()

    # --- input -----------------------------------------------------------------

    def _on_click(self, e) -> Optional[str]:
        self._focus_body()
        if self.screen == "read":
            return None
        line = int(self.body.index(f"@{e.x},{e.y}").split(".")[0])
        i = self._click_map.get(line)
        if i is None:
            return "break"
        cur = {"list": lambda: self.sel[self.channel], "prs": lambda: self.sel_prs,
               "who": lambda: self.sel_who, "options": lambda: self.sel_opt}.get(self.screen, lambda: -1)()
        if i == cur:
            self._activate()
            return "break"
        if self.screen == "list":
            self.sel[self.channel] = i
        elif self.screen == "prs":
            self.sel_prs = i
        elif self.screen == "who":
            self.sel_who = i
        elif self.screen == "options":
            self.sel_opt = i
        self.render()
        return "break"

    def _on_double(self, e) -> Optional[str]:
        if self.screen == "read":
            return None
        self._activate()
        return "break"

    @guarded
    def _on_key(self, e) -> Optional[str]:
        before = self.screen
        t0 = time.perf_counter()
        result = self._handle_key(e)
        ms = (time.perf_counter() - t0) * 1000
        level = logging.WARNING if ms > FRAME_MS else logging.DEBUG
        log.log(level, "%skey %-6s %s -> %s  %.1fms (build %.1f paint %.1f, %d lines)",
                "SLOW " if ms > FRAME_MS else "", e.keysym, before, self.screen, ms,
                self.stats["build_ms"], self.stats["paint_ms"], self.stats["changed"])
        return result

    def _handle_key(self, e) -> Optional[str]:
        keysym = e.keysym
        ch = (e.char or "").lower()
        ctrl = bool(e.state & 0x4)
        if ctrl:
            k = keysym.lower()
            if k in ("equal", "plus"):
                self.zoom(1)
            elif k == "minus":
                self.zoom(-1)
            elif k == "w":
                self.toggle_worker()
            elif k == "r":
                self.wake_selected()
            elif k in ("c", "a", "insert"):
                return None
            return "break"
        s = self.screen

        if self.confirm:
            _msg, action = self.confirm
            self.confirm = None
            if ch == "y":
                action()
            else:
                self.flash("Never mind, then.", "mu")
            self.render()
            if self.screen == "read":
                self.reply.focus_set()
            return "break"

        if keysym == "Escape":
            self.go_back()
            return "break"
        if keysym in ("Up", "Down", "Prior", "Next"):
            if s == "read":
                if keysym in ("Up", "Down"):
                    self.body.yview_scroll(-1 if keysym == "Up" else 1, "units")
                else:
                    self.body.yview_scroll(-1 if keysym == "Prior" else 1, "pages")
                return "break"
            self._move(-1 if keysym in ("Up", "Prior") else 1, page=keysym in ("Prior", "Next"))
            return "break"
        if keysym in ("Home", "End") and s != "read":
            self._move(-10_000 if keysym == "Home" else 10_000)
            return "break"
        if keysym in ("Left", "Right") and s == "options":
            self._change_option(-1 if keysym == "Left" else 1)
            return "break"
        if keysym in ("Return", "KP_Enter", "space") and s not in ("read", "main"):
            self._activate()
            return "break"

        if s == "read":
            if ch in ("r", "a") or keysym == "Tab":
                self.reply.focus_set()
                return "break"
            if ch in ("n", "p"):
                self.reader_step(1 if ch == "n" else -1)
                return "break"
            if ch == "c":
                self.reader_close()
                return "break"
            if ch == "u":
                self.reader_unarchive()
                return "break"
            if ch == "o":
                pr = next((r for r in self.prs if r.get("thread_id") == self.read_tid), None)
                if pr:
                    webbrowser.open(pr["url"])
                return "break"
            return "break"

        if ch in CHANNEL_KEYS:
            self.goto("list", CHANNEL_KEYS[ch])
            return "break"
        if s == "list" and ch == "n":
            self.goto("compose")
            self.subject_ent.focus_set()
            return "break"
        if s == "list" and ch == "h" and self.channel == "question":
            self.show_archived = not self.show_archived
            self.sel["question"] = 0
            self.app.redraw()
            return "break"
        if s == "list" and ch == "u" and self.channel == "question" and self.show_archived:
            rows = self.rows["question"]
            if rows:
                self.app.unarchive_question(rows[self.sel["question"]]["id"])
            return "break"
        if s == "prs":
            if ch == "c":
                self.app.check_prs_now()
                self.flash("Checking GitHub for merges...", "cy")
                return "break"
            if ch == "h":
                self.show_settled = not self.show_settled
                self.sel_prs = 0
                self.app.redraw()
                return "break"
            if ch == "o":
                self._activate()
                return "break"
        if s == "sysop":
            if ch == "r":
                self.app._reload_code()
                return "break"
            if ch == "l" and self.held_row:
                self.open_thread(self.held_row["id"], back="sysop")
                return "break"
        if s == "who" and ch == "p":
            self._page()
            return "break"
        if ch == "p":
            self.goto("prs")
            return "break"
        if ch == "s":
            self.goto("sysop")
            return "break"
        if ch == "b":
            self.goto("who")
            return "break"
        if ch == "o":
            self.goto("options")
            return "break"
        if ch == "m":
            self.goto("main")
            return "break"
        if ch == "t":
            i = self.theme_order.index(self.prefs["theme"])
            self.set_theme(self.theme_order[(i + 1) % len(self.theme_order)])
            return "break"
        if ch == "g":
            self.flash("+++ATH0 · NO CARRIER", "or", "b")
            self.root.after(350, self.app._hide_to_tray)
            return "break"
        return "break"

    def connected(self) -> None:
        """First paint is on screen: play the handshake if it's switched on."""
        if self.prefs.get("screech"):
            threading.Thread(target=screech.play, daemon=True).start()


def _titlebar(root: tk.Tk, pal: dict) -> None:
    """Colour the native Windows title bar to match (Windows 11 honours the exact colours)."""
    import sys
    if sys.platform != "win32":
        return
    try:
        import ctypes
        root.update_idletasks()
        hwnd = ctypes.windll.user32.GetParent(root.winfo_id())
        dwm = ctypes.windll.dwmapi

        def put(attr, value):
            v = ctypes.c_uint(value)
            dwm.DwmSetWindowAttribute(hwnd, attr, ctypes.byref(v), ctypes.sizeof(v))

        def ref(hexc):
            h = hexc.lstrip("#")
            return int(h[4:6], 16) << 16 | int(h[2:4], 16) << 8 | int(h[0:2], 16)

        put(20, 1 if pal["dark"] else 0)
        put(35, ref(pal["panel"]))
        put(36, ref(pal["fg"]))
        put(34, ref(pal["panel"]))
        ctypes.windll.user32.SetWindowPos(hwnd, 0, 0, 0, 0, 0, 0x0027)
    except Exception:
        pass
