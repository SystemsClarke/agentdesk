"""The terminal view, driven for real against a scratch copy of the board.

Every screen is reached by keypress, every keypress is timed from handler entry to
the paint landing (update_idletasks), and the budget is one 60 Hz frame. Replying
and composing write to the copy and are read back. Screenshots land in the scratch
folder for a human to look at.

Run: .venv\\Scripts\\python.exe scripts\\check_terminal.py [--shots DIR]
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

REAL_DATA = Path(os.environ.get("LOCALAPPDATA", Path.home())) / "AgentDesk"
SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-terminal-"))
os.environ["LOCALAPPDATA"] = str(SCRATCH)
REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO))

from agentdesk import app as appmod  # noqa: E402
from agentdesk import db, paths  # noqa: E402

FRAME_MS = 16.7
failures: list = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + (f"  ({detail})" if detail else ""))
    if not ok:
        failures.append(name)


def key(view, keysym, char=""):
    return SimpleNamespace(keysym=keysym, char=char, state=0)


def timed(app, fn, *args):
    t0 = time.perf_counter()
    fn(*args)
    app.root.update_idletasks()
    return (time.perf_counter() - t0) * 1000


consistency_errors: list = []


def consistent(app, label):
    v = app.view
    top = v.top.get("1.0", "end-1c")
    if top.count("AgentDesk") != 1:
        consistency_errors.append(f"{label}: title bar drawn {top.count('AgentDesk')}x")
    if v.screen not in ("read", "compose"):
        on_screen = int(v.lines_view.index("end-1c").split(".")[0])
        if on_screen != max(1, len(v._painted)):
            consistency_errors.append(f"{label}: {on_screen} lines on screen, painter drew {len(v._painted)}")


def press(app, keysym, char=""):
    ms = timed(app, app.view._on_key, key(app.view, keysym, char))
    consistent(app, f"{app.view.screen} after {keysym}")
    return ms


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--shots", type=Path, default=SCRATCH / "shots")
    args = ap.parse_args()
    paths.ensure_dirs()
    src = sqlite3.connect(str(REAL_DATA / "agentdesk.db"))
    dst = sqlite3.connect(str(paths.DB_PATH))
    src.backup(dst)
    src.close()
    dst.close()
    for name in ("claude_usage.json", "slack_bridge.state"):
        if (REAL_DATA / name).exists():
            shutil.copy(REAL_DATA / name, paths.DATA_DIR / name)
    (paths.DATA_DIR / "settings.json").write_text(json.dumps({"screech": False}), encoding="utf-8")
    from PIL import Image
    (paths.DATA_DIR / "attachments").mkdir(exist_ok=True)
    Image.new("RGB", (300, 200), "#78dce8").save(paths.DATA_DIR / "attachments" / "photo.png")
    conn = db.connect(paths.DB_PATH)
    try:
        for subj in ("Stagger the JAWS MAIN nightly timers, or add SYMTOOLS agents?",
                     "OK to land the testhost flake fix on all 20 J27 branches?"):
            db.start_thread(conn, "question", subj, "builder", paths.AGENT_KIND,
                            "Planted by scripts/check_terminal.py in a scratch copy.")
    finally:
        conn.close()

    appmod._enable_dpi_awareness()
    app = appmod.App(paths.DB_PATH)
    app._stop_pr_watch()
    v = app.view
    app.root.deiconify()
    app.root.update()
    app.refresh_now()
    app.root.update()

    shots = args.shots
    shots.mkdir(parents=True, exist_ok=True)

    def shot(name):
        try:
            from PIL import ImageGrab
            app.root.update()
            x, y = app.root.winfo_rootx(), app.root.winfo_rooty()
            w, h = app.root.winfo_width(), app.root.winfo_height()
            ImageGrab.grab(bbox=(x, y, x + w, y + h)).save(shots / f"{name}.png")
        except Exception as exc:
            print(f"  (screenshot {name} skipped: {exc})")

    timings: dict = {}

    def rec(label, ms):
        timings.setdefault(label, []).append(ms)

    check("starts on the main menu", v.screen == "main")
    main_text = v.body.get("1.0", "end")
    check("main menu shows the CONNECT line", "CONNECT" in main_text)
    check("main menu shows the usage line", "time left" in main_text or "monthly spend" in main_text,
          main_text.splitlines()[-2:])
    shot("01-main")

    rec("open Questions", press(app, "q", "q"))
    check("Q opens Questions", v.screen == "list" and v.channel == "question")
    shot("02-questions")
    for _ in range(15):
        rec("move selection", press(app, "Down"))
    for _ in range(15):
        rec("move selection", press(app, "Up"))
    rows = v.rows["question"]
    check("Questions list has rows from the real board", len(rows) > 0, f"{len(rows)} rows")

    rec("open a message", press(app, "Return"))
    check("Enter opens the reader", v.screen == "read" and v.read_tid == rows[0]["id"])
    shot("03-reader")
    rec("next message", press(app, "n", "n"))
    rec("previous message", press(app, "p", "p"))
    rec("back to list", press(app, "Escape"))
    check("Esc returns to the list", v.screen == "list")

    tid = rows[0]["id"]
    v.open_thread(tid)
    app.root.update()
    v.reply.insert("1.0", "terminal check: this reply was written by scripts/check_terminal.py")
    v._send()
    app.root.update()
    conn = db.connect(paths.DB_PATH)
    try:
        msgs = db.get_thread(conn, tid)["messages"]
    finally:
        conn.close()
    check("a reply from the reader lands in the board as john",
          msgs[-1]["author"] == paths.HUMAN and "check_terminal" in msgs[-1]["body"])
    check("the reader repaints with the new reply",
          "check_terminal" in v.body.get("1.0", "end"))

    press(app, "Escape")
    press(app, "d", "d")
    press(app, "n", "n")
    check("N opens compose", v.screen == "compose")
    v.subject_ent.insert(0, "terminal check thread")
    v.reply.insert("1.0", "posted by scripts/check_terminal.py")
    v._send()
    app.root.update()
    check("compose posts a thread and opens it", v.screen == "read"
          and v._thread(v.read_tid)["thread"]["subject"] == "terminal check thread")

    for label, k in (("open SysOp", "s"), ("open Who's on", "b"), ("open PRs", "p"),
                     ("open Options", "o"), ("open Work to Hire", "j"), ("open Wiki", "w"),
                     ("back to main", "m")):
        press(app, "Escape")
        rec(label, press(app, k, k))
        if k in "sbpojw":
            shot({"s": "04-sysop", "b": "05-whos-on", "p": "06-prs", "o": "07-options",
                  "j": "08-work", "w": "09-wiki"}[k])
    check("SysOp shows the Claude plan row", "Claude plan" in (press(app, "s", "s") and v.body.get("1.0", "end")))
    toggled = []
    app._toggle_worker = lambda: toggled.append(True)
    v._on_key(SimpleNamespace(keysym="w", char="\x17", state=0x4))
    app.root.update_idletasks()
    check("Ctrl+W toggles the worker and stays on the screen", v.screen == "sysop" and toggled == [True])
    press(app, "w", "w")
    check("plain W is still global navigation to the Wiki", v.screen == "list" and v.channel == "wiki")
    press(app, "s", "s")
    rec("switch theme", press(app, "t", "t"))
    press(app, "m", "m")
    shot("10-main-light")
    rec("switch theme", press(app, "t", "t"))

    showcase = "\n".join([
        "# Build speed report",
        "> [!WARNING]",
        "> JAWS MAIN symbol upload is the long pole.",
        "",
        "| lane | before | after | change |",
        "|:-----|-------:|------:|:------:|",
        "| compile | 41m | 29m | **-29%** |",
        "| sign | 12m | 7m | -42% |",
        "",
        "```mermaid",
        "graph LR",
        "  A[Queue] --> B{Worker free?}",
        "  B -->|yes| C[Compile]",
        "  B -->|no| D[Wait]",
        "  C --> E[Sign]",
        "```",
        "",
        "```mermaid",
        "sequenceDiagram",
        "  builder->>john: OK to land?",
        "  john-->>builder: yes, all branches",
        "```",
        "",
        "```mermaid",
        "pie title Where the time goes",
        '  "compile" : 29',
        '  "sign" : 7',
        '  "symbols" : 14',
        "```",
        "",
        "```python",
        "def stagger(timers, gap=15):  # minutes",
        "    return [t + i * gap for i, t in enumerate(timers)]",
        "```",
        "",
        "```chart",
        '{"type":"line","title":"Compile time","unit":"min","x":["09-01","09-08","09-15","09-22"],'
        '"series":{"JAWS":[41,38,33,29]},"goal":25}',
        "```",
        "",
        f"![photo]({(paths.DATA_DIR / "attachments" / "photo.png").as_uri()})",
        "",
        "- [x] stagger the nightly timers",
        "- [ ] add SYMTOOLS to more agents",
        "  - survey the fleet first",
        "~~old plan~~ and https://github.com/ogden-marrow/agentdesk",
    ])
    conn = db.connect(paths.DB_PATH)
    try:
        md_tid = db.start_thread(conn, "discussion", "markdown showcase", "builder", paths.AGENT_KIND, showcase)
    finally:
        conn.close()
    app.refresh_now()
    v.open_thread(md_tid)
    app.root.update()
    shown = v.read_view.get("1.0", "end")
    images = len(v.read_view.image_names())
    check("the table, the ```chart and a Slack photo render as images", images >= 3, f"{images} images")
    check("no raw table or chart JSON is left in the text", "| lane |" not in shown and '"type":"line"' not in shown)
    check("a mermaid flowchart is drawn, not shown as source", "►" in shown and "graph LR" not in shown)
    check("a mermaid sequence diagram is drawn", "OK to land?" in shown and "->>" not in shown)
    check("a mermaid pie renders as bars", "█" in shown and "Where the time goes" in shown)
    check("code gets a language label", "╭─ python" in shown)
    check("a GitHub callout gets its label", "WARNING" in shown and "[!WARNING]" not in shown)
    check("task list items render as boxes", "☑" in shown and "☐" in shown)
    shot("11-markdown")
    v.read_view.yview_moveto(1.0)
    shot("12-markdown-bottom")

    broken = []
    for key in list(v.theme_order):
        try:
            v.set_theme(key)
            app.root.update_idletasks()
        except Exception as exc:
            broken.append(f"{key}: {exc}")
    check("every installed theme applies cleanly", not broken, f"{len(v.theme_order)} themes; {broken[:2]}")
    v.set_theme("monokai-pro")

    v.zoom(1)
    app.root.update()
    consistent(app, "after zoom in")
    v.zoom(-1)
    app.root.update()
    consistent(app, "after zoom out")
    log_text = (paths.DATA_DIR / "terminal.log").read_text(encoding="utf-8", errors="replace")
    errors = [l for l in log_text.splitlines() if " ERROR " in l]
    check("no screen raised an error while drawing (terminal.log)", not errors, "; ".join(errors[:2]))
    check("no screen is ever drawn twice or left with stale lines", not consistency_errors,
          "; ".join(consistency_errors[:4]))
    print("\nkeypress to paint, ms (median / worst):")
    for label, ms in sorted(timings.items()):
        s = sorted(ms)
        print(f"  {label:<20} {s[len(s) // 2]:6.2f} / {s[-1]:6.2f}")
    in_screen = ("move selection", "next message", "previous message", "back to list", "back to main")
    moves = [m for l in in_screen for m in timings.get(l, [])]
    switches = [m for l, ms in timings.items() if l not in in_screen and l != "switch theme" for m in ms]
    check("in-screen keypresses paint within one frame (16.7 ms)", max(moves) <= FRAME_MS,
          f"worst {max(moves):.2f} ms")
    check("full screen switches paint within two frames (33 ms)", max(switches) <= 2 * FRAME_MS,
          f"worst {max(switches):.2f} ms")
    check("a theme change paints within 50 ms", max(timings["switch theme"]) <= 50,
          f"{max(timings['switch theme']):.2f} ms")

    try:
        if app.icon is not None:
            app.icon.stop()
    except Exception:
        pass
    app.root.destroy()
    print(f"\nscreenshots: {shots}")
    print(f"{'ALL PASS' if not failures else str(len(failures)) + ' FAILED'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
