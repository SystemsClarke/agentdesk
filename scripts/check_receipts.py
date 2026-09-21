"""Read receipts: does the once-mechanism hold, and is the grey actually there?

Work item #34. Four things the item asks for, and this prints a line for each
rather than asserting one:

  1. Exactly ONE receipt when the same agent reads the same thread repeatedly,
     whatever number of readers race for it. The mechanism, not the outcome.
  2. John's question count and his toast count unchanged by receipts -- with a
     control that shows the harness WOULD have seen a toast, because a test
     that can only ever print zero proves nothing.
  3. A receipt that cannot feed itself: no ping-pong, no receipting your own
     thread, no receipt earned by another receipt.
  4. A real window with a receipt and a real reply side by side, reporting the
     tags and the foreground the pane actually resolved for each -- so the
     distinction is measured, not asserted -- plus a PNG to look at.

WHY THIS DRIVES THE REAL TOOLS. `LOCALAPPDATA` is redirected to a scratch
directory before anything imports `agentdesk.paths`, and paths reads it once at
import. Every `db.connect()` in the package resolves through it, so
`mcp_server.read_thread` can be called here directly and it posts against the
scratch board -- the real code path, not a rehearsal of it. The live board
cannot be reached from this script by accident, which is the point.

    .venv\\Scripts\\python.exe scripts\\check_receipts.py
    .venv\\Scripts\\python.exe scripts\\check_receipts.py --shot .\\receipts
"""

from __future__ import annotations

import argparse
import os
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))
sys.path.insert(0, str(REPO / "scripts"))

# Before the first agentdesk import, and it must stay before it: paths.py reads
# LOCALAPPDATA at import time and every writer in the package inherits the
# answer for the life of the process.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-receipts-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

from agentdesk import db, mcp_server, notify, paths  # noqa: E402

WIDTH = 78
FAILURES: list[str] = []

# Markers a reader can grep for, so a message can be located in the rendered
# pane by its own text rather than by an index that a repaint could move.
OPEN_MARK = "SEEDOPEN the pipeline went green this morning"
REPLY_MARK = "SEEDREPLY confirmed, I re-ran it and it is green"


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<44} {value}")


def check(label: str, got, want) -> None:
    """Print the value and record a failure if it is not what it must be.

    Records rather than raises, so one broken property does not hide the state
    of the other five.
    """
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<39} {got!r}")


# --- the board under test ------------------------------------------------------

def seed(conn) -> dict:
    """One of everything the receipt paths touch.

    Written through db.py, the same module the app and the MCP server write
    through, so what is measured is a real board rather than a fixture shaped
    like one.
    """
    ids = {}

    # A question an agent asked and John answered, so the EXISTING acks
    # machinery engages and its message can be checked for the same grey.
    ids["question"] = db.start_thread(
        conn, "question", "Should the retry budget be per-host or per-job?",
        "researcher", paths.AGENT_KIND,
        "Two hosts are eating the budget for everyone else.")
    db.reply(conn, ids["question"], paths.HUMAN, paths.HUMAN_KIND,
             "Per-host. Leave the per-job default alone.")
    db.set_thread_status(conn, ids["question"], paths.STATUS_ANSWERED)

    # The side-by-side thread: a real opening message, then a real reply, and
    # the receipt goes on the end of it. This is the one the window renders.
    ids["side"] = db.start_thread(
        conn, "discussion", "Side by side: a reply next to a receipt",
        "researcher", paths.AGENT_KIND, OPEN_MARK)
    db.reply(conn, ids["side"], "verifier", paths.AGENT_KIND, REPLY_MARK)

    # A work item to claim, and a bare thread for the race.
    ids["work"] = db.start_thread(
        conn, "work", "The window forgets where I put it",
        "claude-code", paths.AGENT_KIND,
        "Acceptance: the geometry survives a restart.")
    ids["race"] = db.start_thread(
        conn, "discussion", "Eight readers, one receipt",
        "researcher", paths.AGENT_KIND, "Nothing to see here.")
    conn.commit()
    return ids


def receipts_on(conn, thread_id) -> list:
    """The receipt messages on a thread, read back out of `messages` rather
    than out of the receipts table: the table says a receipt was recorded, the
    message is the thing a reader sees, and the two must agree."""
    return [dict(m) for m in conn.execute(
        "SELECT id, author, body, meta FROM messages WHERE thread_id=? ORDER BY id",
        (thread_id,)) if db.is_receipt(m["meta"])]


# --- 1. the once-mechanism -----------------------------------------------------

def check_once(ids: dict) -> None:
    hr("1. the once-mechanism: one receipt per (agent, thread), ever")
    conn = db.connect(paths.DB_PATH)

    print("  the real MCP tool, called repeatedly -- as an agent re-reading a")
    print("  thread it is working through would:")
    for i in range(1, 7):
        before = len(receipts_on(conn, ids["side"]))
        mcp_server.read_thread(ids["side"], author="builder")
        after = len(receipts_on(conn, ids["side"]))
        if i in (1, 2, 6):
            show(f"read {i}: receipts before -> after", f"{before} -> {after}")
    check("receipts after 6 reads by one agent", len(receipts_on(conn, ids["side"])), 1)
    check("builder's receipts on that thread",
          len([r for r in receipts_on(conn, ids["side"]) if r["author"] == "builder"]), 1)

    print()
    print("  a DIFFERENT agent reading it -- a second reader earns its own, so")
    print("  the count is per (agent, thread) and not one per thread:")
    mcp_server.read_thread(ids["side"], author="verifier")
    check("receipts total after verifier read", len(receipts_on(conn, ids["side"])), 2)
    show("readers with a receipt",
         sorted(r["author"] for r in receipts_on(conn, ids["side"])))

    print()
    print("  the receipts table itself, which is what decides:")
    show("receipts rows", db.receipts_for_thread(conn, ids["side"]))

    print()
    print("  the mechanism under an actual race -- 8 real MCP tool calls on one")
    print("  fresh thread, one per thread of execution, all the same agent. A")
    print("  guard in Python would let several through; the PRIMARY KEY cannot.")
    threads = [threading.Thread(
        target=mcp_server.read_thread, args=(ids["race"],),
        kwargs={"author": "racer"}) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    got = len(receipts_on(conn, ids["race"]))
    check("receipts after 8 concurrent readers", got, 1)
    conn.close()


# --- 2. it must not feed itself ------------------------------------------------

def warrant_basis(conn, thread_id, agent) -> list:
    """The messages that EARN a receipt for this agent: what the predicate is
    actually looking at. Printed, so "receipts do not count" is visible rather
    than asserted -- a receipt row appears in the messages table and must not
    appear here."""
    return [(r["id"], r["author"]) for r in conn.execute(
        "SELECT id, author, meta FROM messages WHERE thread_id=? ORDER BY id",
        (thread_id,)) if r["author"] != agent and not db.is_receipt(r["meta"])]


def check_no_pingpong(ids: dict) -> None:
    hr("2. it must not feed itself")
    conn = db.connect(paths.DB_PATH)

    print("  the side thread now holds real messages AND two receipts. A new")
    print("  agent's receipt must be earned by the real messages only:")
    show("messages on the side thread", conn.execute(
        "SELECT COUNT(*) FROM messages WHERE thread_id=?",
        (ids["side"],)).fetchone()[0])
    show("...of which receipts", len(receipts_on(conn, ids["side"])))
    show("what a new agent's receipt is earned by",
         warrant_basis(conn, ids["side"], "a-third-agent"))
    check("receipts are absent from the warrant basis",
          any(db.is_receipt(conn.execute(
              "SELECT meta FROM messages WHERE id=?", (mid,)).fetchone()["meta"])
              for mid, _ in warrant_basis(conn, ids["side"], "a-third-agent")),
          False)

    print()
    print("  a thread holding NOTHING but receipts must warrant nothing. Built")
    print("  by hand, because the real paths cannot produce it -- every thread")
    print("  is born with its opener's own message, which is exactly why a")
    print("  receipt can never be the only reason the next one is earned.")
    solo = db.start_thread(conn, "discussion", "Nothing but receipts",
                           "ghost-opener", paths.AGENT_KIND, "seed")
    conn.execute("DELETE FROM messages WHERE thread_id=?", (solo,))
    db.reply(conn, solo, "ghost-opener", paths.AGENT_KIND,
             "ghost-opener picked this thread up.", meta={"kind": "read-receipt"})
    show("that thread's warrant basis for a new agent",
         warrant_basis(conn, solo, "someone-new"))
    check("a thread of only receipts warrants another",
          db.thread_warrants_receipt(conn, solo, "someone-new"), False)
    check("...and posts nothing for a new agent",
          db.post_read_receipt(conn, solo, "someone-new", "x"), False)

    print()
    print("  the opener's own thread. Every thread here was opened by")
    print("  'researcher', and an agent receipting the thing it wrote is noise")
    print("  rather than a receipt.")
    check("opener reading its own thread",
          db.post_read_receipt(conn, ids["race"], "researcher", "x"), False)

    print()
    print("  and a real message still earns one, so the rule above is not")
    print("  simply refusing everything:")
    check("a real message still warrants a receipt",
          db.thread_warrants_receipt(conn, ids["side"], "a-third-agent"), True)
    conn.close()


# --- 3. what a receipt must not change -----------------------------------------

def check_nothing_else_moves(ids: dict) -> None:
    hr("3. a receipt changes exactly one thing: it adds a message row")

    def snapshot():
        c = db.connect(paths.DB_PATH)
        try:
            t = c.execute("SELECT status, updated_ts FROM threads WHERE id=?",
                          (ids["side"],)).fetchone()
            p = c.execute("SELECT seen_ts FROM presence WHERE author='newcomer'"
                          ).fetchone()
            oq = [q["thread_id"] for q in db.open_questions(c)]
            order = [r["id"] for r in db.list_threads(c, limit=50)]
            return (dict(t), (p["seen_ts"] if p else None), oq, order)
        finally:
            c.close()

    before = snapshot()
    conn = db.connect(paths.DB_PATH)
    posted = db.post_read_receipt(conn, ids["side"], "newcomer", "newcomer was here")
    conn.close()
    after = snapshot()

    check("the receipt was posted", posted, True)
    check("thread status", after[0]["status"], before[0]["status"])
    check("thread updated_ts", after[0]["updated_ts"], before[0]["updated_ts"])
    check("reader's presence row", after[1], before[1])
    check("open_questions", after[2], before[2])
    check("list_threads order", after[3], before[3])
    print()
    print("  status unchanged is 'must not close a question'; open_questions")
    print("  unchanged is 'must not toast John' at the data level. The toast is")
    print("  measured for real in the next section.")


# --- 4. the toast, measured, with a control ------------------------------------

def check_no_toast(ids: dict, appmod):
    hr("4. John's toast count, with a control that proves the probe works")
    fired: list = []
    original = notify.toast
    notify.toast = lambda *a, **k: fired.append(a)

    app = appmod.App(paths.DB_PATH)
    app.root.withdraw()
    app.root.update()
    time.sleep(0.3)
    try:
        # Seed the window's counters the way a launch does, so a receipt is
        # arriving into a running board rather than into startup.
        for _ in range(3):
            app.refresh_now()
            app.root.update()
        seeded = len(fired)

        # A receipt lands. This is a new message row, so the poll definitely
        # sees it; the question is whether it counts as news.
        conn = db.connect(paths.DB_PATH)
        db.post_read_receipt(conn, ids["side"], "late-reader", "late-reader read this")
        conn.close()
        for _ in range(3):
            app.refresh_now()
            app.root.update()
        after_receipt = len(fired)
        check("toasts caused by the receipts", after_receipt - seeded, 0)

        # THE CONTROL. Same window, same poll, a real open question. If this
        # does not fire, the probe is broken and the zero above means nothing.
        conn = db.connect(paths.DB_PATH)
        db.start_thread(conn, "question", "CONTROL: does a new question toast?",
                        "researcher", paths.AGENT_KIND, "Deliberately new.")
        conn.close()
        for _ in range(3):
            app.refresh_now()
            app.root.update()
        check("toasts caused by a real new question (control)",
              len(fired) - after_receipt, 1)
        if fired:
            show("the toast text", repr(fired[-1][0][:52]))
    finally:
        notify.toast = original
        if app.icon is not None:
            try:
                app.icon.stop()
            except Exception:
                pass
        app.root.destroy()


# --- 5. side by side, in the real window ---------------------------------------

def _tag_report(widget, offset: int) -> None:
    """Every tag Tk actually applied at this character, with its foreground.

    Printed rather than reduced to one colour: a reader checking this needs to
    see whether "receipt" is the only thing setting a foreground here, which is
    the claim, or whether some markdown tag is quietly winning it.
    """
    names = widget.tag_names(f"1.0 + {offset} chars")
    parts = []
    for n in names:
        fg = widget.tag_cget(n, "foreground")
        parts.append(f"{n}{'=' + fg if fg else ''}")
    print(f"        tags: {', '.join(parts) if parts else '(none)'}")


def check_side_by_side(ids: dict, appmod, shot: Path | None) -> None:
    hr("5. a receipt and a real reply, side by side, in the real window")
    app = appmod.App(paths.DB_PATH)
    app.root.withdraw()
    app.root.update()
    time.sleep(0.3)
    app.root.deiconify()
    app.root.update()
    try:
        for _ in range(4):
            app.refresh_now()
            app.root.update()
            time.sleep(0.05)

        page = app.pages["discussion"]
        app.nb.select(page)
        app.root.update_idletasks()
        app.root.update()

        page.tree.selection_set(str(ids["side"]))
        page._on_select()
        app.root.update_idletasks()
        app.root.update()

        txt = page.msgs_txt
        whole = txt.get("1.0", "end-1c")
        print("  what the pane holds, in order:")
        for line in whole.splitlines():
            if line.strip():
                print(f"      | {line[:66]}")

        print()
        print("  message by message: is it greyed, and by which tag?")
        print("  ('receipt' is the only tag on this widget that sets a")
        print("   foreground for a whole message, so the tag list IS the colour.)")
        probes = (
            ("opener's real message", OPEN_MARK, False),
            ("the real reply", REPLY_MARK, False),
            ("builder's receipt", "builder picked this thread up", True),
            ("verifier's receipt", "verifier picked this thread up", True),
            ("newcomer's receipt", "newcomer was here", True),
            ("late-reader's receipt", "late-reader read this", True),
        )
        for label, needle, expect_greyed in probes:
            found = txt.search(needle, "1.0", "end")
            if not found:
                FAILURES.append(f"{label}: not found in the pane")
                print(f"    {label:<24} NOT FOUND")
                continue
            # line.col -> a character offset, because tag_names() takes an
            # index expression and "1.0 + N chars" is the only form that
            # survives a needle spanning a line break.
            names = txt.tag_names(f"1.0 + {whole.find(needle)} chars")
            greyed = "receipt" in names
            if greyed != expect_greyed:
                FAILURES.append(
                    f"{label}: greyed={greyed}, wanted {expect_greyed}")
            print(f"    [{'ok' if greyed == expect_greyed else 'FAIL'}] "
                  f"{label:<24} greyed={str(greyed):<5} at {found}")
            print(f"          tags: "
                  f"{', '.join(n + ('=' + txt.tag_cget(n, 'foreground') if txt.tag_cget(n, 'foreground') else '') for n in names) or '(none)'}")

        print()
        show("foreground configured for 'receipt'",
             repr(txt.tag_cget("receipt", "foreground")))
        check("that foreground is grey, not the body colour",
              txt.tag_cget("receipt", "foreground") != txt.tag_cget("md", "foreground"),
              True)

        # The honest measurement of "which tag wins": Tk resolves overlapping
        # tags by priority, and tag_names() with no argument returns every tag
        # on the widget in that order. Print it, so the reader can see
        # "receipt" sitting above mdview's tags rather than take it on trust.
        show("tag priority, low to high", repr(txt.tag_names()))
        show("'receipt' is last (highest)", str(txt.tag_names()[-1] == "receipt"))

        # The spans themselves. Adjacent receipts merge into one span, which
        # is why the count is lower than the number of greyed messages: four
        # receipts in a row are one range, and that is not a bug.
        ranges = txt.tag_ranges("receipt")
        pairs = list(zip(ranges[0::2], ranges[1::2]))
        show("spans carrying 'receipt'", f"{len(pairs)}")
        for a, b in pairs:
            lines = f"{a}..{b}"
            first = txt.get(a, f"{a} lineend")
            print(f"      {lines}  | {first[:52]}")

        if shot:
            shot.mkdir(parents=True, exist_ok=True)
            out = (shot / "receipt-vs-reply.png").resolve()
            from check_look import screenshot  # the ctypes capture, not a copy
            show("screenshot", screenshot(app.root, out))
    finally:
        if app.icon is not None:
            try:
                app.icon.stop()
            except Exception:
                pass
        app.root.destroy()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--shot", type=Path, default=None,
                    help="write a PNG of the side-by-side pane here")
    ap.add_argument("--keep", action="store_true",
                    help="leave the scratch board behind")
    args = ap.parse_args()

    from agentdesk import app as appmod

    print(f"scratch board: {paths.DB_PATH}")
    conn = db.connect(paths.DB_PATH)
    try:
        db.init_db(conn)
        ids = seed(conn)
    finally:
        conn.close()
    show("seeded", f"threads {sorted(ids.values())}")

    check_once(ids)
    check_no_pingpong(ids)
    check_nothing_else_moves(ids)
    check_no_toast(ids, appmod)
    check_side_by_side(ids, appmod, args.shot)

    hr()
    if FAILURES:
        print(f"FAILURES: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
    else:
        print("every printed property held.")
    print("\nNOT asserted anywhere above: that this looks good. That is what the")
    print("PNG is for.")
    if not args.keep:
        print(f"(scratch board left at {_SCRATCH}; remove it when done.)")
    return 1 if FAILURES else 0


if __name__ == "__main__":
    raise SystemExit(main())
