"""What does the window actually look like? Run this and read the numbers.

The item is "make it look good", which is not a thing that can be asserted, so
this does NOT assert. It launches the REAL window -- `agentdesk.app.App`, not a
stand-in -- against a scratch database seeded with one thread of every state the
board can be in, then prints what the window came up as: geometry, the tab
labels, every column and whether the columns fit the space they were given, the
rows with their status and colour, and the detail pane's own text.

Run it before and after a change and diff the output. That diff is the
before/after list the item asks for, and it is measured rather than asserted
because the thing being judged is appearance.

It also writes PNGs of the window (--shot DIR) so the picture can be looked at
rather than inferred from the geometry.

    .venv\\Scripts\\python.exe scripts\\check_look.py
    .venv\\Scripts\\python.exe scripts\\check_look.py --shot .\\look

Nothing here touches the real board: the App is pointed at a database under a
temp directory via its own --db mechanism, so the real window on the desktop is
left alone and the SingleInstance guard treats this as a different board.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PY = str(REPO / ".venv" / "Scripts" / "python.exe")
if not Path(PY).exists():
    PY = sys.executable
sys.path.insert(0, str(REPO))

from agentdesk import db, paths  # noqa: E402

WIDTH = 78


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<26} {value}")


# --- a board in every state it can be in --------------------------------------

def seed(conn, human="john") -> None:
    """One thread of every kind and state, so the window has something to show.

    Written through db.py, the same module the app and the MCP server write
    through, so what the window renders is a real board and not a fixture that
    happens to have the right columns.
    """
    q_open = db.start_thread(conn, "question", "Should the retry budget be per-host or per-job?",
                             "claude-code:AgentDesk#3f2a", paths.AGENT_KIND,
                             "Two hosts are eating the budget for everyone else.\n\n"
                             "**Per-job** is simpler to reason about; per-host stops one "
                             "bad host from starving the rest. I lean per-host.")
    q_answered = db.start_thread(conn, "question", "Which vault should the wiki mirror write into?",
                                 "builder", paths.AGENT_KIND, "It is currently hard-coded.")
    db.reply(conn, q_answered, human, paths.HUMAN_KIND,
             "The MainClaude one. Leave the path alone.")
    db.set_thread_status(conn, q_answered, paths.STATUS_ANSWERED)
    q_fyi = db.start_thread(conn, "question", "FYI: the GoCD agent pool is being rebuilt tonight",
                            "researcher", paths.AGENT_KIND, "No action needed.")
    db.set_thread_status(conn, q_fyi, paths.STATUS_FYI)

    d = db.start_thread(conn, "discussion", "The dispatcher creates the schema instead of dying",
                        "builder", paths.AGENT_KIND,
                        "A missing database used to be a crash at startup.\n\n"
                        "```\nOperationalError: no such table: threads\n```\n\n"
                        "It now calls `init_db` first, which is what the app already did.")
    db.reply(conn, d, "verifier", paths.AGENT_KIND,
             "Reproduced against a fresh LOCALAPPDATA. Confirmed.")
    db.reply(conn, d, "claude-code:AgentDesk#9c14", paths.AGENT_KIND, "Noted.")

    db.start_thread(conn, "wiki", "Registering an AUMID for a plain desktop app",
                    "builder", paths.AGENT_KIND,
                    "An unregistered AppUserModelID is not an error: "
                    "`CreateToastNotifier` succeeds and the toast is attributed "
                    "to somebody else. A Start Menu shortcut is what makes it real.")

    w_open = db.start_thread(conn, "work", "Every agent reads as 'claude' in the opened-by column",
                             "claude-code", paths.AGENT_KIND,
                             "Acceptance: two sessions posting produce two authors.")
    # Left unclaimed on purpose: the queue has three states and a run that
    # claims everything shows only two of them.
    db.start_thread(conn, "work", "The window forgets where I put it",
                    "claude-code", paths.AGENT_KIND,
                    "Acceptance: the geometry survives a restart.")
    w_done = db.start_thread(conn, "work", "The toast is attributed to PowerShell",
                             "claude-code", paths.AGENT_KIND,
                             "Give AgentDesk its own AppUserModelID.")
    w_claimed = db.start_thread(conn, "work", "Make the app actually look good",
                                "claude-code", paths.AGENT_KIND,
                                "Keep the ttk and the old school look.")
    db.claim_task(conn, w_claimed, "builder")
    db.claim_task(conn, w_done, "builder")
    db.reply(conn, w_done, "builder", paths.AGENT_KIND, "Done; report on the thread.")
    db.complete_task(conn, w_done, "builder")
    db.claim_task(conn, w_open, "verifier")
    db.complete_task(conn, w_open, "verifier")
    db.reply(conn, w_open, "verifier", paths.AGENT_KIND, "Verified.")
    conn.commit()


# --- measuring the real window -------------------------------------------------

def _columns(tree, where: str) -> None:
    """Every column's configured width, and the width it is actually given.

    The two are not the same for a stretching column, and the second one is the
    only one that says whether anything is unreachable. bbox on a real row is
    the only place Tk reports the laid-out column at all.
    """
    hr(f"columns, {where}")
    total = fixed = stretch_min = 0
    for col in tree.cget("columns"):
        w = int(tree.column(col, "width"))
        mn = int(tree.column(col, "minwidth"))
        stretching = bool(tree.column(col, "stretch"))
        total += w
        if stretching:
            stretch_min += mn
        else:
            fixed += w
        show(f"col {col}",
             f"width={w:4} minwidth={mn:4} stretch={stretching} "
             f"heading={tree.heading(col, 'text')!r}")
    kids = tree.get_children()
    reach = 0      # the right-hand edge of the last column that is drawn
    if kids:
        laid = []
        for col in tree.cget("columns"):
            box = tree.bbox(kids[0], col)
            if not box:
                continue
            laid.append(f"{col}: x={box[0]} w={box[2]}")
            reach = max(reach, box[0] + box[2])
        show("as laid out", "  ".join(laid))
        # bbox reports a column that is off the edge of the widget perfectly
        # happily -- its x is just past the right-hand side -- so a width
        # alone does not say the column is reachable. This does.
        show("right edge", f"{reach}px, tree is {tree.winfo_width()}px wide")
        show("every column reachable", "yes" if reach <= tree.winfo_width()
             else f"NO -- {reach - tree.winfo_width()}px past the edge, and "
                  f"there is no horizontal scrollbar")
    show("tree width", tree.winfo_width())
    show("columns total", f"{total} requested")
    show("floor", f"{fixed + stretch_min} (fixed columns + stretch minwidth)")
    show("fit?", "yes" if fixed + stretch_min <= tree.winfo_width() else
         f"NO -- needs {fixed + stretch_min}px, has {tree.winfo_width()}px")
    show("comfortable?", "yes" if total <= tree.winfo_width() else
         f"no -- {total - tree.winfo_width()}px short at this window size")


def measure(app, appmod) -> None:
    root = app.root
    root.update_idletasks()
    root.update()

    hr("window")
    show("title", root.title())
    show("geometry", root.winfo_geometry())
    show("min size", root.minsize())
    show("ttk theme", appmod.ttk.Style(root).theme_use())

    hr("toolbar strip")
    strip = app.worker_button.master
    show("strip padding", str(strip.cget("padding")))
    for w in strip.winfo_children():
        cls = w.winfo_class()
        try:
            text = w.cget("text")
        except Exception:
            text = ""
        try:
            fg = w.cget("foreground")
        except Exception:
            fg = ""
        show(f"{cls} {text[:24]!r}",
             f"x={w.winfo_x():4d} w={w.winfo_width():4d} fg={fg!r}")

    nb = None
    for w in root.winfo_children():
        if isinstance(w, appmod.ttk.Notebook):
            nb = w
    if nb is not None:
        hr("notebook")
        for tab in nb.tabs():
            show("tab", repr(nb.tab(tab, "text")))

    for ch in paths.CHANNELS:
        page = app.pages[ch]
        hr(f"page {ch!r}")
        # First at the window's own minimum, because that is where a list
        # that does not fit loses a column off the right edge and nobody
        # notices until they drag the window small. Reported here rather than
        # in a section of its own so the two widths sit next to each other.
        small = app.root.minsize()
        app.root.geometry(f"{small[0]}x{small[1]}")
        app.root.update_idletasks()
        app.root.update()
        if nb is not None:
            nb.select(page)
            app.root.update_idletasks()
            app.root.update()
        _columns(page.tree, f"at the {small[0]}px minimum")
        show("window now", app.root.winfo_geometry())
        show("panes", f"list={page.tree.winfo_width()} "
                      f"detail={page.msgs_txt.master.winfo_width()}")
        app.root.geometry("1000x640")
        app.root.update_idletasks()
        app.root.update()
        tree = page.tree
        # Select the tab first: an unmapped notebook page is never laid out and
        # every widget on it reports a width of 1, which reads as "the columns
        # do not fit" on three tabs for no reason.
        if nb is not None:
            nb.select(page)
            root.update_idletasks()
            root.update()
        try:
            show("banner", repr(page.banner.cget("text")[:60]))
        except Exception:
            pass
        try:
            show("footer", repr(page.footer.cget("text")))
        except Exception:
            pass
        _columns(tree, "the default size")
        show("row height", appmod.ttk.Style(root).lookup("Treeview", "rowheight"))
        show("rows", len(tree.get_children()))
        for iid in tree.get_children():
            vals = tree.item(iid, "values")
            tags = tree.item(iid, "tags")
            colour = ""
            for t in tags:
                fg = tree.tag_configure(t, "foreground")
                if fg:
                    colour = fg
            show(f"  #{iid}", f"{vals}  tags={tags} fg={colour!r}")

        # The detail pane, driven through the widget's own handler.
        kids = tree.get_children()
        if kids:
            tree.selection_set(kids[0])
            page._on_select()
            root.update_idletasks()
            detail = page.msgs_txt.get("1.0", "end-1c")
            show("detail subject", repr(page.subject_lbl.cget("text")))
            for name in getattr(page, "meta_lbl", None) and ("meta_lbl",) or ():
                show("detail meta", repr(page.meta_lbl.cget("text")))
            print("  detail body:")
            for line in detail.splitlines()[:6]:
                print(f"      | {line}")


def top_level_window(tk_window) -> int:
    """The HWND of the whole window, frame and title bar included.

    Tk's winfo_id() is the client-area window; the title bar and the border
    belong to the frame the window manager wrapped around it, which is the
    parent. Walking up matters because those pixels are half of "looks good"
    and a capture of the client area alone would miss them.
    """
    import ctypes

    user32 = ctypes.WinDLL("user32", use_last_error=True)
    user32.GetParent.restype = ctypes.c_void_p
    user32.GetParent.argtypes = [ctypes.c_void_p]
    hwnd = ctypes.c_void_p(tk_window.winfo_id())
    while True:
        parent = user32.GetParent(hwnd)
        if not parent:
            return hwnd.value
        hwnd = ctypes.c_void_p(parent)


def screenshot(tk_window, out: Path) -> str:
    """Capture the real window to a PNG, from inside the process that drew it.

    PrintWindow rather than a screen grab, and in-process rather than through
    PowerShell: a child process is not guaranteed the same desktop as the
    window it is being asked to photograph (this one is not, under the sandbox
    the shell runs in -- FindWindowW there finds nothing at all), and
    PrintWindow asks the window to draw itself rather than reading whatever
    happens to be on top of it.
    """
    import ctypes
    from ctypes import wintypes

    from PIL import Image  # pystray already depends on it

    user32 = ctypes.WinDLL("user32", use_last_error=True)
    gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)

    hwnd = top_level_window(tk_window)
    rect = wintypes.RECT()
    if not user32.GetWindowRect(ctypes.c_void_p(hwnd), ctypes.byref(rect)):
        return f"GetWindowRect failed on hwnd={hwnd}"
    w, h = rect.right - rect.left, rect.bottom - rect.top
    if w <= 0 or h <= 0:
        return f"window has no size ({w}x{h})"

    hdc = user32.GetWindowDC(ctypes.c_void_p(hwnd))
    memdc = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    old = gdi32.SelectObject(memdc, bmp)
    try:
        # PW_RENDERFULLCONTENT (2) is what makes this capture a composited
        # window rather than a blank rectangle on modern Windows.
        if not user32.PrintWindow(ctypes.c_void_p(hwnd), memdc, 2):
            return f"PrintWindow refused hwnd={hwnd}"

        class BMIH(ctypes.Structure):
            _fields_ = [("biSize", wintypes.DWORD), ("biWidth", wintypes.LONG),
                        ("biHeight", wintypes.LONG), ("biPlanes", wintypes.WORD),
                        ("biBitCount", wintypes.WORD),
                        ("biCompression", wintypes.DWORD),
                        ("biSizeImage", wintypes.DWORD),
                        ("biXPelsPerMeter", wintypes.LONG),
                        ("biYPelsPerMeter", wintypes.LONG),
                        ("biClrUsed", wintypes.DWORD),
                        ("biClrImportant", wintypes.DWORD)]

        info = BMIH()
        info.biSize = ctypes.sizeof(BMIH)
        info.biWidth = w
        info.biHeight = -h          # negative: rows top-down, as PIL wants
        info.biPlanes = 1
        info.biBitCount = 32
        info.biCompression = 0      # BI_RGB
        buf = ctypes.create_string_buffer(w * h * 4)
        got = gdi32.GetDIBits(memdc, bmp, 0, h, buf, ctypes.byref(info), 0)
        if got != h:
            return f"GetDIBits returned {got} of {h} rows"
        img = Image.frombuffer("RGB", (w, h), buf, "raw", "BGRX", 0, 1)
        out.parent.mkdir(parents=True, exist_ok=True)
        img.save(out)
        return f"saved {out} ({w}x{h})"
    finally:
        gdi32.SelectObject(memdc, old)
        gdi32.DeleteObject(bmp)
        gdi32.DeleteDC(memdc)
        user32.ReleaseDC(ctypes.c_void_p(hwnd), hdc)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--shot", type=Path, default=None,
                    help="directory to write window screenshots into")
    ap.add_argument("--keep", action="store_true",
                    help="leave the window open instead of closing it")
    ap.add_argument("--db", type=Path, default=None, help=(
        "measure an existing board instead of the seeded one. Pass a COPY: "
        "the window reads and the poll loop writes, and this is not a "
        "read-only view of it."))
    args = ap.parse_args()

    from agentdesk import app as appmod

    if args.db is not None:
        print(f"board under test: {args.db}  (a copy, not the live board)")
        return measure_board(appmod, args.db, args)

    with tempfile.TemporaryDirectory(prefix="agentdesk-look-") as tmp:
        db_path = Path(tmp) / "AgentDesk" / "agentdesk.db"
        db_path.parent.mkdir(parents=True, exist_ok=True)
        conn = db.connect(db_path)
        try:
            db.init_db(conn)
            seed(conn)
        finally:
            conn.close()

        print(f"scratch board: {db_path}")
        return measure_board(appmod, db_path, args)


def measure_board(appmod, db_path: Path, args) -> int:
    """Build the real App over that board, drive it, and report. Returns 0."""
    app = appmod.App(db_path)
    app.root.withdraw()              # off-screen while it settles
    app.root.update()
    time.sleep(0.3)
    app.root.deiconify()
    app.root.update()
    try:
        # The poll runs on a timer; drive a few ticks by hand so the pages are
        # populated exactly as they would be three seconds after launch.
        for _ in range(6):
            app.refresh_now()
            app.root.update()
            time.sleep(0.05)
        measure(app, appmod)
        if args.shot:
            args.shot.mkdir(parents=True, exist_ok=True)
            time.sleep(0.4)
            print("\n--- screenshots " + "-" * (WIDTH - 17))
            nb = getattr(app, "nb", None)
            for ch in paths.CHANNELS:
                if nb is not None:
                    nb.select(app.pages[ch])
                    app.root.update_idletasks()
                    app.root.update()
                    time.sleep(0.2)
                print("  " + screenshot(app.root,
                                        (args.shot / f"{ch}.png").resolve()))
            # ...and once at the minimum, where a list that does not fit
            # loses a column off the right edge.
            small = app.root.minsize()
            app.root.geometry(f"{small[0]}x{small[1]}")
            app.root.update_idletasks()
            app.root.update()
            time.sleep(0.3)
            print("  " + screenshot(app.root,
                                    (args.shot / "at-minimum.png").resolve()))
            if args.keep:
                print("  window left open; press Ctrl+C when done")
                while True:
                    app.root.update()
                    time.sleep(0.1)
    finally:
        if app.icon is not None:
            try:
                app.icon.stop()
            except Exception:
                pass
        app.root.destroy()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
