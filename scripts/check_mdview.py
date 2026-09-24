"""Acceptance checks for Markdown rendering in the message pane (work item 40).

What John asked for, in his words: "make it so that md is rendered."

The board carries agent-written Markdown -- headings, bold, fenced code, lists,
links -- and the window used to show the markers as literal text. `mdview.py`
renders a subset of it into `tk.Text` tags, and `Page.refresh_detail` calls it
per message. This script measures what actually lands on screen.

It is written against the WIDGET, not against mdview's functions. Every claim
below is read back out of the Text widget the reader is looking at, by text
index and tag name, because "the renderer returns the right tags" and "the pane
shows rendered text" are different statements and only the second one is the
promise. Two sections do call mdview directly, for the cases the pane cannot
easily be made to produce (an empty body, an unterminated fence).

The sections:

  1. line structure -- headings, bullets, numbered items, quotes, a rule, and a
     fenced block, each present as its rendered form and absent as its source;
  2. inline spans -- bold, italic, inline code, a link whose text is shown and
     whose url is not;
  3. the two-pass rule: a fence's content is verbatim, so Markdown inside it
     survives as characters rather than being marked up;
  4. a repaint does not render anything twice;
  5. a receipt stays greyed as one range even when its body carries Markdown;
  6. LINK RESOLUTION -- this is the one that failed when this script was first
     written, and the reason it exists. Every link in the pane must open its
     OWN url. Before the fix, `render` reset the per-link tag map on every
     call, so each message's first link got the same tag name and the map kept
     only the last url rendered: clicking any link opened the final message's
     link. Two messages, one link each, both opening the second url.
  7. bodies that should not be able to raise: empty, unclosed fence, lone
     backtick, a body that is all whitespace.
  8. TABLES IN THE REAL PANE, against John's own reported table (work item
     #102), byte for byte as the live board holds it. That message is the
     fixture because it is the complaint: he pasted a table and said it did not
     render well. It is also the hard case, because the header line has one cell
     MORE than the delimiter row and every data row -- see BODY_TABLE_JOHN --
     and a strict GFM reader renders such a table as plain text, which is
     precisely the complaint. This section claims only what holds at ANY pane
     width, because the width it inherits is a fact about this machine's window.
  9. THE SAME TWO TABLES AT A WIDTH THAT NEEDS NO FITTING -- the app's table
     (five cells, one stray) and the table he meant (four), cell for cell at
     1000px, where every row is one line. The difference between them is the
     finding: the renderer is a grid either way, and one stray character puts a
     header one column right of its data. A check that only carried the
     corrected body could not tell you which of the two things was wrong.
 10. THE SAME TABLE FITTED TO A NARROW PANE, at the 386px the app's own window
     gives its detail side. This is where a table stops being a grid and starts
     being wrapped cells inside a column, and the trade is asserted rather than
     assumed: nothing drawn wider than the pane, no character lost, every
     boundary still lined up, and the rows taller -- which is the price.

    python scripts/check_mdview.py [--shot DIR]
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import tempfile
import time
import tkinter as tk
import tkinter.font as tkfont
import webbrowser
from pathlib import Path
from typing import Optional

# Before anything prints: this script's own output is a rendered table, which
# means bullets and box-drawing rule/join characters (`•─│┼`), none of which
# cp1252 can represent -- on a plain PowerShell/cmd console (cp1252 on this
# machine) that is a bare UnicodeEncodeError that reads as the test crashing
# rather than what it is, a console-encoding mismatch (work item #118's class
# of bug, found here by the same grep that item asked for and reproduced with
# PYTHONIOENCODING=cp1252). See check_toast.py for the identical fix and the
# fuller reasoning; the two should stay in sync if either changes.
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

# --- isolation, BEFORE agentdesk is imported -----------------------------------
#
# LOCALAPPDATA first: paths.py reads it at import time, and db.DB_PATH with it.
# A scratch value here is what keeps this from touching John's real board.
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-mdview-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))
# scripts/ as well, so the --shot path can borrow check_look.screenshot, which
# is the version that actually photographs the window.
sys.path.insert(0, str(REPO / "scripts"))

from agentdesk import app as appmod               # noqa: E402
from agentdesk import db, mdview, notify, paths   # noqa: E402

# The vault is redirected as well, even though nothing here archives anything:
# the App's poll loop runs the archive pass on every tick, and a board that
# happened to hold a settled question would otherwise write into the real vault.
_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = paths.VAULT_AGENTDESK / "questions"

WIDTH = 78
FAILURES: list[str] = []


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<44} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<42} {got!r}")


def note(text: str) -> None:
    for line in text.strip().splitlines():
        print(f"  {line}")


# --- reading the widget ---------------------------------------------------------
#
# Everything here reads through Tk, never through mdview's internals. `search`
# takes a text index, not a view index, so the answer is the same whether or
# not the widget is scrolled.

def tags_at(widget: tk.Text, needle: str) -> tuple:
    """(index, tags) at the first place `needle` appears, or (None, ())."""
    idx = widget.search(needle, "1.0", stopindex="end", exact=True)
    if not idx:
        return None, ()
    return idx, widget.tag_names(idx)


def rendered(widget: tk.Text) -> str:
    return widget.get("1.0", "end-1c")


def count_of(widget: tk.Text, needle: str) -> int:
    n, at = 0, "1.0"
    while True:
        at = widget.search(needle, at, stopindex="end", exact=True)
        if not at:
            return n
        n += 1
        at = f"{at}+1c"


def visible_text(widget: tk.Text) -> str:
    """The widget's text with every newline dropped, so a substring check does
    not depend on where a line happens to wrap or break."""
    return rendered(widget).replace("\n", " ")


# --- the bodies under test ------------------------------------------------------

# Distinct tokens for every construct: a word that appears once is a word whose
# tag can be looked up without ambiguity.
BODY_STRUCTURE = """# Heading one

Prose with **loud** and *soft* and `tick` in it.

## Heading two

### Heading three

- bullet alpha
- bullet bravo with **loud2**

1. numbered uno
2. numbered dos

> quoted line

---

```
verbatim **not bold** and # not a heading
```

Tail with a [here](https://example.invalid/one) link."""

# The link wording is deliberately absent from BODY_STRUCTURE. The first
# version used "alpha", which also appears as "bullet alpha" in the body above;
# the click test then found the bullet's word, which carries no link tag, and
# reported a product failure that was actually the sample colliding with
# itself. A token that appears exactly once is the only kind worth searching for.
BODY_LINK_A = "See [charlie](https://example.invalid/charlie)."
BODY_LINK_B = "See [delta](https://example.invalid/delta)."

RECEIPT_MARK = "SEEDRECEIPT read this thread, and the body says **loud3** in it"

# John's table, byte for byte as work item #102 holds it -- the message whose
# whole content is "see it does not render good". The leading "u" on the first
# line is HIS, not a transcription slip. It makes the header five cells over a
# four-cell delimiter row and four-cell data rows.
BODY_TABLE_JOHN = (
    "u| run | container | time | submodule phases |\n"
    "|---|---|---|---|\n"
    "| AuthTools2027_Simkovitz #182 | docker-19 | 16:10 UTC | "
    "280.6 / 310.9 / 287.8 s |\n"
    "| AuthTools2027_Simkovitz #187 | docker-19 | 19:22 UTC | "
    "91.7 / 85.8 / 86.8 s |\n"
    "| AuthTools2027_Simkovitz #188 | docker-19 | 19:41 UTC | "
    "86.9 / 79.3 / 81.7 s |\n"
    "\nsee it does not render good"
)

# The same table as he meant it, for the comparison in section 9. Separate
# message ids, and seeded first, so both are in the pane at once and the order
# of the two rendered header lines is deterministic rather than incidental.
BODY_TABLE_CLEAN = (
    "| run | container | time | submodule phases |\n"
    "|---|---|---|---|\n"
    "| AuthTools2027_Simkovitz #182 | docker-19 | 16:10 UTC | "
    "280.6 / 310.9 / 287.8 s |\n"
    "| AuthTools2027_Simkovitz #187 | docker-19 | 19:22 UTC | "
    "91.7 / 85.8 / 86.8 s |\n"
    "| AuthTools2027_Simkovitz #188 | docker-19 | 19:41 UTC | "
    "86.9 / 79.3 / 81.7 s |\n"
    "\nand this one should line up"
)

TABLE_HEADER_CELL = "submodule phases"


def pane_lines(txt: tk.Text) -> list[str]:
    """The pane's text as lines, which is what a reader sees line by line."""
    return rendered(txt).split("\n")


def squash(text: str) -> str:
    """The text with every whitespace character removed.

    A cell that wrapped is split across lines and padded with blanks, so its
    value is not a substring of what is on screen -- "AuthTools2027_Simkovitz
    #182" arrives as "AuthTool", "s2027_Si", "mkovitz", "#182" on four lines.
    Squashing both sides puts the characters back in sequence and asks the
    question that matters, which is whether every character of the value is
    there in the right order. What it cannot check, and says so, is whether the
    wrap landed somewhere sensible -- that is the wide-pane section's job.
    """
    return "".join(text.split())


def table_block(lines: list[str], nth: int) -> list[str]:
    """Every rendered line of the nth table, wrapped rows included.

    Located by the table's own RULE and not by its header text, because the
    header is the first thing to wrap when the pane is narrow: looking for
    "submodule phases" finds nothing once it is drawn as "submodu", "le",
    "phases" on three lines. The rule contains a cross character no other
    construct renders, and it is one line wide, so it is the one landmark in a
    table that survives being fitted. From it the block is walked outwards
    while the neighbouring lines still carry a column separator.
    """
    rules = [k for k, line in enumerate(lines) if "┼" in line]
    if len(rules) <= nth:
        return []
    k = rules[nth]
    first, last = k, k
    while first > 0 and "│" in lines[first - 1]:
        first -= 1
    while last + 1 < len(lines) and "│" in lines[last + 1]:
        last += 1
    return lines[first:last + 1]


def grid_cells(line: str) -> list[str]:
    """A rendered table line split on the column separator.

    Split on the box-drawing separator and not on " │ ", so a row whose last
    column is empty still yields its empty final cell instead of quietly losing
    the column -- which is the exact difference sections 8 and 9 turn on.
    """
    s = line.strip()
    if s[:1] in "│├┌└":
        s = s[1:]
    if s[-1:] in "│┤┐┘":
        s = s[:-1]
    return [c.strip() for c in s.split("│")]


def boundaries(line: str, font, sep: str = "│") -> list[int]:
    """Pixel x of each INNER column separator (the box's outer border is skipped).

    The rule line joins its dashes with a CROSS and not with the separator the
    rows use, so the caller names the character. That is not cosmetic: the cross
    is the one character in the rule that has to sit exactly under the rows'
    separator, and looking for the wrong one finds nothing and reports an empty
    list rather than an error -- which is how this check first failed, claiming
    the rule had no boundaries when it had three.
    """
    last = len(line.rstrip()) - 1
    return [font.measure(line[:m.start() + 1])
            for m in re.finditer(re.escape(sep), line) if 0 < m.start() < last]


class Click:
    """Just enough of a Tk event for the link handler, which reads .widget/.x/.y."""

    def __init__(self, widget: tk.Text, x: int, y: int) -> None:
        self.widget, self.x, self.y = widget, x, y


OPENED: list = []


def install_webbrowser_probe():
    """Record what a link click would open instead of opening it.

    Patched on the webbrowser MODULE, which is the object mdview holds a
    reference to, so the real handler runs and only the browser launch is
    intercepted. A test that opens a real browser tab is a test that gets
    commented out.
    """
    original = webbrowser.open
    webbrowser.open = lambda url, *a, **k: (OPENED.append(url), True)[1]
    return original


def show_window(app, page) -> None:
    """Put the pane on screen for the clicks, and leave it there.

    A Text has to be MAPPED for Tk to answer bbox(), and this App is withdrawn
    while it settles. Both halves are needed: the window shown, and the page
    the notebook's SELECTED tab -- a Text on a tab that is not displayed is
    unmapped too, which is the same trap that once put the wrong tab in the
    merge-list screenshot. Hidden again by hide_window() after the section,
    rather than between clicks, because withdrawing re-unmaps the widget and
    the next bbox() answers None.
    """
    app.root.deiconify()
    app.nb.select(page)
    for _ in range(3):
        app.root.update()
        time.sleep(0.1)


def hide_window(app) -> None:
    app.root.withdraw()
    app.root.update()


def click_link(page, needle: str) -> str:
    """Click the link whose visible text is `needle`; return the url it opened.

    The coordinates come from bbox() for the link's own index, so this clicks
    where the text is rather than at a guessed position; a click that misses
    would otherwise be indistinguishable from a handler that resolves nothing.
    """
    widget = page.msgs_txt
    if not widget.winfo_ismapped():
        return (f"(pane not mapped: {widget.winfo_width()}x"
                f"{widget.winfo_height()})")
    idx = widget.search(needle, "1.0", stopindex="end", exact=True)
    if not idx:
        return "(link text not on screen)"
    tags = widget.tag_names(idx)
    if not any(t == "md-link" for t in tags):
        return f"(no link on '{needle}'; tags={tags})"
    # bbox() answers None for a character that is scrolled out of view, and the
    # pane jumps to the bottom whenever messages arrived -- so a link in an
    # earlier message is genuinely off-screen and genuinely unclickable until
    # the reader scrolls to it. Scroll first, then measure.
    widget.see(idx)
    widget.update()
    box = widget.bbox(idx)
    if box is None:
        return f"(bbox returned None for {idx} even after see())"
    x, _y, _w, h = box
    before = len(OPENED)
    mdview._link_click(Click(widget, x + 2, box[1] + h // 2))
    return OPENED[-1] if len(OPENED) > before else "(nothing opened)"


# --- the board under test -------------------------------------------------------

def seed(conn) -> dict:
    """One discussion thread carrying every construct, and a receipt.

    Discussion and not question on purpose: an open question makes this window
    fire a real toast, and a test that announces itself to John on every run is
    a test that gets turned off. Nothing here needs the question lifecycle.
    """
    ids = {}
    ids["t"] = db.start_thread(
        conn, "discussion", "SEEDMD how the renderer behaves",
        "researcher", paths.AGENT_KIND, BODY_STRUCTURE)
    # Order matters, and it is load-bearing for section 6. The two linked
    # messages must come LAST, so that the last thing rendered into the pane
    # holds a link: with the tag map reset per message, that is what makes both
    # clicks resolve to the final url. Put a linkless message after them and
    # the same bug shows up as "nothing opens at all" instead -- the map is
    # emptied by the last render and no link resolves. The receipt is
    # deliberately in the middle because it is linkless.
    db.reply(conn, ids["t"], "verifier", paths.AGENT_KIND, RECEIPT_MARK,
             meta={"kind": "read-receipt"})
    # The two tables, before the linked messages and after the receipt, so the
    # ordering constraint above still holds. John's first, so the first rendered
    # header line in the pane is his -- section 8 asserts that, rather than
    # trusting it, because both tables share their header cell text.
    db.reply(conn, ids["t"], "john", paths.HUMAN_KIND, BODY_TABLE_JOHN)
    db.reply(conn, ids["t"], "researcher", paths.AGENT_KIND, BODY_TABLE_CLEAN)
    db.reply(conn, ids["t"], "researcher", paths.AGENT_KIND, BODY_LINK_A)
    db.reply(conn, ids["t"], "builder", paths.AGENT_KIND, BODY_LINK_B)
    return ids


def select_thread(app, channel: str, thread_id: int):
    """Select a thread the way a reader does: through the tree, then the handler
    the tree's binding calls. Assigning page.thread_id directly would skip
    refresh_detail, which is the whole thing under test."""
    page = app.pages[channel]
    if not page.tree.exists(str(thread_id)):
        return None
    page.tree.selection_set(str(thread_id))
    page._on_select(None)
    app.root.update()
    return page


# --- sections -------------------------------------------------------------------

def check_structure(page) -> None:
    hr("1. line structure")
    txt = page.msgs_txt
    text = visible_text(txt)
    note("what the pane shows, as one line:")
    print(f"  {text[:300]!r}")
    print()

    for label, needle, tag in (
            ("h1 renders as a heading", "Heading one", "md-h1"),
            ("h2 renders as a heading", "Heading two", "md-h2"),
            ("h3 renders as a heading", "Heading three", "md-h3"),
            ("a bullet renders as a bullet", "• bullet alpha", "md-bullet"),
            ("a numbered item keeps its number", "1. numbered uno", "md-bullet"),
            ("a quote renders as a quote", "quoted line", "md-quote"),
            ("a rule renders as a rule", "─" * 10, "md-rule"),
    ):
        _idx, tags = tags_at(txt, needle)
        check(label, tag in tags, True)

    # The same constructs, as their source. These are the assertions that fail
    # if the pane is showing the raw body -- which is what this item is about.
    for label, needle in (
            ("the heading's # is gone", "# Heading one"),
            ("the bullet's - is gone", "- bullet alpha"),
            ("the quote's > is gone", "> quoted line"),
            ("a rule is not three hyphens", "---"),
    ):
        check(label, needle in text, False)


def check_inline(page) -> None:
    hr("2. inline spans")
    txt = page.msgs_txt
    text = visible_text(txt)

    for label, needle, tag in (
            ("bold is marked up", "loud", "md-bold"),
            ("italic is marked up", "soft", "md-italic"),
            ("inline code is marked up", "tick", "md-code"),
            ("a link keeps its text", "here", "md-link"),
    ):
        _idx, tags = tags_at(txt, needle)
        check(label, tag in tags, True)

    for label, needle in (
            ("the ** markers are gone", "**loud**"),
            ("the * markers are gone", "*soft*"),
            ("the backticks are gone", "`tick`"),
    ):
        check(label, needle in text, False)

    check("the link's url is not shown as text",
          "https://example.invalid/one" in text, False)


def check_fence_is_verbatim(page) -> None:
    hr("3. the two-pass rule: a fence is verbatim")
    txt = page.msgs_txt
    verbatim = "verbatim **not bold** and # not a heading"
    idx, tags = tags_at(txt, verbatim)
    show("the fence line is on screen unchanged", idx is not None)
    check("...as one codeblock range", "md-codeblock" in tags, True)
    check("...and its ** was NOT read as bold", "md-bold" in tags, False)
    check("...and its # was NOT read as a heading", "md-h1" in tags, False)


def check_no_double_render(app, page) -> None:
    hr("4. a repaint renders nothing twice")
    before = count_of(page.msgs_txt, "Heading one")
    show("occurrences of 'Heading one' before a repaint", before)
    for _ in range(2):
        app.refresh_now()
        app.root.update()
    after = count_of(page.msgs_txt, "Heading one")
    check("...and after two repaints", after, before)
    check("...and 'Heading one' appears once", after, 1)


def check_receipt_stays_grey(page) -> None:
    hr("5. a receipt is still one grey range")
    txt = page.msgs_txt
    idx, tags = tags_at(txt, "loud3")
    show("tags where the receipt's bold word sits", tags)
    check("the receipt range covers it", "receipt" in tags, True)
    # The grey is a foreground on the receipt tag, and Tk applies the
    # LAST-CREATED tag first. Page builds mdview's tags and then configures
    # "receipt", and refresh_detail raises it as well, so the receipt must be
    # the highest-priority tag on that range -- otherwise a heading inside a
    # receipt would paint over the grey that marks it as not-a-reply.
    check("...and the receipt outranks the markdown tag", tags[-1], "receipt")
    check("...and the markdown tag is still there", "md-bold" in tags, True)


def check_links(app, page) -> None:
    hr("6. every link opens its own url")
    note("this is the section that failed when this script was written: the")
    note("tag map was reset per message, so both links opened the last url.")
    print()
    show_window(app, page)
    try:
        got_c = click_link(page, "charlie")
        got_d = click_link(page, "delta")
        show("clicking charlie's link opened", got_c)
        show("clicking delta's link opened", got_d)
        check("charlie's link opens charlie", got_c,
              "https://example.invalid/charlie")
        check("delta's link opens delta", got_d,
              "https://example.invalid/delta")
        print()
        note("and again after a repaint, because that is what re-renders the pane:")
        app.refresh_now()
        app.root.update()
        got_c2 = click_link(page, "charlie")
        check("...charlie still opens charlie", got_c2,
              "https://example.invalid/charlie")
    finally:
        hide_window(app)


def measure_in(app, width_px: int, body: str) -> tuple[int, int, list[str]]:
    """Render `body` into a Text exactly `width_px` wide, and read it back.

    Returns (the Text's real width, the characters it can hold, its lines).

    A scratch toplevel rather than the app's own pane, because the claims in
    sections 9 and 10 are claims about WIDTHS and the app's pane width is a
    fact about the reader's window -- 386px at the default geometry, wider on a
    maximised one. A check that inherited it could only ever test this
    machine's window size; here the width is chosen, so the fitted and the
    natural renders can both be asked for by name.

    Mapped for real before rendering, and not merely created: winfo_width()
    answers 1 until a widget is mapped, and by design an unmapped pane gets the
    natural-width render (see mdview.capacity_chars), so rendering first would
    silently measure the wrong branch. Not `transient()` to the app's root
    either -- a transient child of a withdrawn window is itself unmapped, which
    is the same trap arriving by a different road.
    """
    win = tk.Toplevel(app.root)
    txt = tk.Text(win, wrap="word")
    txt.pack(fill="both", expand=True)
    mdview.configure(txt)
    try:
        win.geometry(f"{width_px}x400")
        for _ in range(4):
            win.update()
            time.sleep(0.05)
        mdview.render(txt, body)
        win.update()
        return txt.winfo_width(), mdview.capacity_chars(txt), pane_lines(txt)
    finally:
        win.destroy()


def column_text(block: list[str]) -> list[str]:
    """Per column, every character drawn in it, whitespace and separators gone.

    The only honest way to ask "is this value on screen" once cells can wrap.
    A wrapped value is split into pieces that are no longer adjacent, so it is
    not a substring of any line; grouping the pieces BY COLUMN and squashing
    each column recovers it, in draw order. Squashing the whole block instead
    would not: the separators between the columns would land in the middle of
    the search text and break the value apart -- which is how this check first
    failed, reporting four values missing that were plainly on screen.

    The rule line is skipped rather than grouped: it is one cell drawn with
    crosses, it belongs to no column, and it carries no value.
    """
    width = max(len(grid_cells(line)) for line in block)
    cols: list[list[str]] = [[] for _ in range(width)]
    for line in block:
        if "┼" in line:
            continue
        for j, cell in enumerate(grid_cells(line)):
            cols[j].append(cell)
    return [squash("".join(parts)) for parts in cols]


def wait_for_fit(app, txt: tk.Text, cap: int, block: int = 0,
                 avoid: Optional[int] = None) -> list[str]:
    """The nth table's lines, once they fit `cap` and are not `avoid` wide.

    `avoid` is the width the table had before a resize, and it is what makes
    this a wait for a REDRAW rather than for a fit. Waiting only for the fit is
    not enough and was the first version's bug: the table was already narrower
    than the new pane, so the condition was satisfied before the poll could run
    at all, and the section then reported "the table was redrawn" while having
    measured the drawing made before the resize.

    Gives up after two poll intervals and returns whatever is there, so that a
    pane which never refits is a failure with a number attached rather than a
    loop waiting for something that is not coming.
    """
    waited = 0.0
    lines = pane_lines(txt)
    while True:
        block_lines = table_block(lines, block)
        widest = max((len(ln) for ln in block_lines), default=0)
        if block_lines and widest <= cap and widest != avoid:
            return block_lines
        if waited >= 2 * appmod.App.POLL_MS / 1000.0:
            return block_lines
        app.root.update()
        time.sleep(0.2)
        waited += 0.2
        lines = pane_lines(txt)


def check_values(block: list[str], where: tuple[tuple[int, str], ...]) -> None:
    """Each value is drawn in the column it belongs to, characters intact."""
    cols = column_text(block)
    for col, value in where:
        on_screen = col < len(cols) and squash(value) in cols[col]
        check(f"...'{value}' is in column {col}", on_screen, True)


def prefixes_of(block: list[str], font) -> tuple[list[int], list[bool]]:
    """Whether every line's column separators sit where the widest line's do.

    The reference is the line with the most separators -- the header. A
    PREFIX comparison and not equality, because a row whose last cell is empty
    is drawn without the separator that would introduce it: mdview trims that
    one blank rather than emitting a trailing "│ " and deleting it again. So a
    data line legitimately has one separator fewer than the header, and asking
    for equal lists would report a wrapped row as a misaligned one.
    """
    bounds = [boundaries(line, font) for line in block]
    ref = max(bounds, key=len) if bounds else []
    return ref, [b == ref[:len(b)] for b in bounds]


def check_table_in_the_pane(app, page) -> None:
    """John's table in the real pane, at whatever width John's window is.

    Section 8 makes only the claims that must hold at ANY pane width, because
    the width here is inherited from this machine's window and sash rather than
    chosen. Everything that depends on a width -- "his header has five cells",
    "a row is one line tall", "the grid is exact" -- is asserted in sections 9
    and 10, where the width is picked. What is left is what a reader sees
    whatever his window: the source is gone, every character he wrote survives,
    the tags are right, and nothing is drawn too wide to fit.
    """
    hr("8. John's table (work item #102), in the real pane")
    txt = page.msgs_txt
    font = tkfont.nametofont("TkFixedFont")
    text = visible_text(txt)

    # The source must be gone. This is the whole of what he reported: the
    # pipes and the delimiter row were shown as characters.
    check("the delimiter row is not on screen", "|---|---|" in text, False)
    check("...and no source pipe survives anywhere", "|" in text, False)
    check("...and the delimiter row is not a horizontal rule either",
          "-|-" in text, False)
    note("the pipe check is not vacuous: the pane is full of │ separators, which")
    note("is a different character from the source's |, and it is deliberately")
    note("so -- a separator that was the source's own pipe would make the")
    note('assertion above pass while proving nothing.')

    # THE PANE ON SCREEN, resized for real, and waited for between widths.
    #
    # Resizing is the whole subject of this item and not a way of arranging the
    # test: a reader meets "the table does not render well" by widening the
    # window, and nothing else in this section moves -- no message arrives at
    # any point in it -- so if the table's drawn width changes here, it changed
    # because the pane's width did. That is the chain the Configure handler and
    # the poll's fourth gate condition exist to close, and it is the one an
    # earlier version of this check caught half-built: the mark was set on every
    # resize and nothing ever read it, so the table kept its old width for ever.
    #
    # Widths are chosen to stay above the fitted table's floor rather than to be
    # extreme, because below it the renderer has no honest option left -- see
    # mdview._fit_widths. John's five columns cannot be drawn in fewer than 42
    # characters (5 columns x the 6-character floor, plus 3 for each separator),
    # and at the app's own 426px pane there are only 48. So the window is made
    # wider to prove the redraw, not narrower.
    show_window(app, page)
    try:
        pane_px = txt.winfo_width()
        cap = mdview.capacity_chars(txt)
        block = wait_for_fit(app, txt, cap)
        widest = max((len(ln) for ln in block), default=0)
        show("the pane, in pixels", pane_px)
        show("...and the characters it can draw in it", cap)
        show("...and the table's widest rendered line", f"{widest} characters")
        check("...so the table is no wider than the pane it is drawn in",
              widest <= cap, True)

        reference = app.root.geometry()
        screen = app.root.winfo_screenwidth()
        if screen >= 1500:
            app.root.geometry("1440x640")
            for _ in range(3):
                app.root.update()
                time.sleep(0.15)
            cap_w = mdview.capacity_chars(txt)
            block_w = wait_for_fit(app, txt, cap_w, avoid=widest)
            widest_w = max((len(ln) for ln in block_w), default=0)
            show("after widening the window to 1440px:", f"cap {cap_w}, "
                 f"table {widest_w} characters")
            check("...a wider window really is wider", cap_w > cap, True)
            check("...and the table was redrawn to it", widest_w <= cap_w, True)
            check("...at a different width, so it was redrawn and not merely "
                  "re-clipped", widest_w != widest, True)
            check("...and no message arrived to trigger that",
                  len(pane_lines(txt)) > 0, True)

            app.root.geometry(reference)
            for _ in range(3):
                app.root.update()
                time.sleep(0.15)
            wait_for_fit(app, txt, mdview.capacity_chars(txt))
            show("...and back to", reference)
        else:
            note(f"the screen is {screen}px wide, too narrow to widen the")
            note("window on it; the redraw is asserted below instead.")
    finally:
        hide_window(app)

    # The same chain, driven directly, so that it is asserted whether or not
    # this screen was wide enough to resize on. The two fields set here are the
    # two the Configure handler sets, mirrored rather than called because that
    # handler is a closure bound to the pane and cannot be invoked from
    # outside. So the limit is worth stating: what this proves is that a marked
    # pane is noticed by the next poll, repainted and unmarked; what it does not
    # prove is that the handler sets exactly those two fields. The resize above
    # is what covers that half, where the screen allows it.
    page.detail_stale = True
    page.shown_count = -1
    check("a marked pane is noticed by the poll",
          app._pane_needs_repaint(), True)
    app.refresh_now()
    app.root.update()
    check("...and the poll repaints and unmarks it", page.detail_stale, False)
    check("...so the tick after that has nothing to do",
          app._pane_needs_repaint(), False)

    block = table_block(pane_lines(txt), 0)
    check("his table rendered as something, and it opens the pane",
          len(block) >= 3, True)
    if len(block) < 3:
        return
    check("the first table in the pane is his and not the corrected one",
          grid_cells(block[0])[0], "u")

    note("what the pane shows for it, which is the deliverable:")
    for line in block:
        print(f"  |{line}|")
    print()

    # Everything he wrote is on screen, nothing dropped for being awkward.
    # Grouped by column, because at a narrow pane a cell wraps and its value is
    # no longer a contiguous run of characters -- see column_text().
    check_values(block, (
        (0, "AuthTools2027_Simkovitz #182"), (0, "AuthTools2027_Simkovitz #187"),
        (0, "AuthTools2027_Simkovitz #188"), (1, "docker-19"),
        (2, "16:10 UTC"), (2, "19:22 UTC"), (2, "19:41 UTC"),
        (3, "280.6 / 310.9 / 287.8 s"), (3, "91.7 / 85.8 / 86.8 s"),
        (3, "86.9 / 79.3 / 81.7 s"), (4, TABLE_HEADER_CELL),
    ))

    # The tags, read off the widget rather than off the renderer's return.
    _i, tags = tags_at(txt, "submodu")
    check("a column name is tagged as a header cell", "md-th" in tags, True)
    _i, tags = tags_at(txt, "docker")
    check("a value is tagged as table text", "md-table" in tags, True)
    check("...and not as a header", "md-th" in tags, False)
    rule_idx = txt.search("─┼─", "1.0", stopindex="end", exact=True)
    check("the header rule is on screen", rule_idx is not None, True)
    if rule_idx:
        check("...and is tagged as a rule",
              "md-trule" in txt.tag_names(rule_idx), True)

    # THE PROMISE, and the reason the renderer reads the pane's width at all:
    # no line of the table is wider than the pane can draw. A Text wraps, and
    # `wrap` is a widget option in Tk rather than a tag option -- so a renderer
    # cannot ask for "do not wrap this table", it can only ask the table to
    # take less room, which is what this asserts. Measured in characters
    # because that is what the fit is computed in; the font is monospace and
    # measured so in section 6, which is what makes characters the same unit.
    widest = max(len(ln) for ln in block)
    show("the table's widest rendered line, after the repaint",
         f"{widest} characters")
    check("...so the table is no wider than the pane", widest <= cap, True)

    ref, ok = prefixes_of(block, font)
    show("pixel x of each column boundary", ref)
    check("every line's separators sit under the header's", ok, [True] * len(block))


def check_table_natural(app) -> None:
    """The same two tables at a width that needs no fitting at all.

    This is where the cell-level claims live, and they need a chosen width for
    a reason: at the app's own 386px pane, John's five-column table cannot be
    drawn as five columns of anything, so each cell is wrapped and cut and the
    question "does the timing sit under its header" cannot even be asked. At
    1000px the natural widths fit (97 characters for his table, 78 for the
    corrected one), nothing is narrowed, and every row is exactly one line.
    """
    hr("9. both tables at natural width -- cell for cell")
    font = tkfont.nametofont("TkFixedFont")

    width, cap, lines = measure_in(app, 1000, BODY_TABLE_JOHN)
    show("Text width, pixels / characters", f"{width} / {cap}")
    block = table_block(lines, 0)
    check("his table rendered as five lines, one per row", len(block), 5)
    if len(block) != 5:
        for line in block:
            print(f"  |{line}|")
        return
    hdr, rule, *rows = block
    note("what it shows, unfitted:")
    for line in block:
        print(f"  |{line}|")
    print()
    check("...at a width that needed no fitting",
          max(len(ln) for ln in block) <= cap, True)

    # THE MEASUREMENT THAT MATTERS. His header has five cells and his rows have
    # four, so the grid is real but the last column has no data under it and
    # every value sits one column left of its label. Asserted as the difference
    # it is, rather than asserted away: a renderer that quietly dropped the
    # spare header cell would pass a weaker test by deleting "submodule phases".
    check("his header carries his five cells, the stray 'u' included",
          grid_cells(hdr), ["u", "run", "container", "time",
                            TABLE_HEADER_CELL])
    check("...and his rows carry four values each, so the last column is empty",
          [len(grid_cells(r)) for r in rows], [5, 5, 5])
    check("...the trailing cell of a row really is empty, not lost",
          [grid_cells(r)[-1] for r in rows], ["", "", ""])
    check("...and the timing that belongs under the last header sits one left",
          grid_cells(rows[0])[3], "280.6 / 310.9 / 287.8 s")
    check("...so the labels are one column right of the data they name",
          grid_cells(hdr).index("time"), 3)
    check("...and no rendered line ends in a blank",
          [ln[-1:] for ln in block], ["│", "┤", "│", "│", "│"])

    # The control: the same table with the stray cell removed. The comparison
    # is between two independent renders, so "it lines up now" is measured
    # against the failing case and not against a remembered expectation.
    _w, _c, lines = measure_in(app, 1000, BODY_TABLE_CLEAN)
    block = table_block(lines, 0)
    check("the corrected table rendered as five lines too", len(block), 5)
    if len(block) != 5:
        return
    hdr, rule, *rows = block
    note("and the control, which is the same table without the stray cell:")
    for line in block:
        print(f"  |{line}|")
    print()
    check("its header carries exactly its four cells",
          grid_cells(hdr), ["run", "container", "time", TABLE_HEADER_CELL])
    check("...and so does every row", [len(grid_cells(r)) for r in rows], [4, 4, 4])
    check("...with no empty trailing cell anywhere",
          [grid_cells(r)[-1] for r in rows],
          ["280.6 / 310.9 / 287.8 s", "91.7 / 85.8 / 86.8 s",
           "86.9 / 79.3 / 81.7 s"])
    # The thing "renders well" actually means: every column boundary is at the
    # same pixel x in the header, the rule and all three rows. Measured with the
    # font that draws them, not with the character count -- the character count
    # agreeing is an assumption that the font is monospace, and the pixel x
    # agreeing is the reader's experience of it.
    ref = boundaries(hdr, font)
    show("pixel x of each column boundary", ref)
    check("...in the rule too, whose crosses are what the eye follows",
          boundaries(rule, font, "┼"), ref)
    check("...and in all three rows", [boundaries(r, font) for r in rows],
          [ref, ref, ref])
    check("...with a value under every one of the four headers",
          all(grid_cells(r)[j] for r in rows for j in range(4)), True)


def check_table_fitted(app) -> None:
    """The same table where it cannot fit, at the app's own pane width.

    The honest half. John's table wants 97 characters and his pane was holding
    43, so at that width there is no arrangement of it that is five tidy
    columns: the renderer narrows the widest columns and wraps their cells
    inside the column, which keeps every boundary lined up and every character
    on screen, and buys it by making the rows taller. What is asserted is the
    trade itself -- nothing wider than the pane, boundaries still aligned, no
    character lost -- and the row height is reported rather than asserted,
    because "how tall is acceptable" is a judgement and not a measurement.
    """
    hr("10. the same table fitted to a narrow pane, at the app's own width")
    font = tkfont.nametofont("TkFixedFont")
    width, cap, lines = measure_in(app, 386, BODY_TABLE_JOHN)
    show("Text width, pixels / characters", f"{width} / {cap}")
    block = table_block(lines, 0)
    if not block:
        check("the table rendered at a narrow width", False, True)
        return
    note("what it shows, fitted:")
    for line in block:
        print(f"  |{line}|")
    print()

    widest = max(len(ln) for ln in block)
    check("no rendered line is wider than the pane can draw", widest <= cap, True)
    check("...which is less room than the table wanted",
          widest < 97, True)

    # Wrapping is what makes it fit, so the fitted block must be TALLER than
    # five lines -- otherwise the fit did nothing and the source numbers above
    # would be claiming a fit that came from somewhere else.
    check("...and the rows are therefore taller than one line each",
          len(block) > 5, True)

    check_values(block, (
        (0, "AuthTools2027_Simkovitz #182"), (0, "AuthTools2027_Simkovitz #187"),
        (0, "AuthTools2027_Simkovitz #188"), (1, "docker-19"),
        (2, "16:10 UTC"), (3, "280.6 / 310.9 / 287.8 s"),
        (4, TABLE_HEADER_CELL),
    ))

    ref, ok = prefixes_of(block, font)
    show("pixel x of each column boundary", ref)
    check("...and every line's separators still sit under the header's",
          ok, [True] * len(block))
    check("...and there are still five columns, so none was dropped",
          len(ref), 4)

    # The header is the first thing to overflow if the fit misses it, so it is
    # the line worth naming: at this width its cells wrap like the rows'.
    check("...and the header itself was wrapped rather than left overflowing",
          "submodule phases" not in " ".join(block), True)
    show("row height, in rendered lines", f"{len(block)} for 3 rows + header")


def check_edge_bodies(app) -> None:
    hr("7. bodies that must not raise")
    # A throwaway Text in the same Tk instance rather than a second Tk(): two
    # roots in one process fight over the interpreter. No geometry is needed
    # here, so this is independent of whether the window is mapped.
    widget = tk.Text(app.root, wrap="word")
    mdview.configure(widget)
    try:
        for label, body in (
                ("an empty body", ""),
                ("a body that is only whitespace", "   \n\n\t\n"),
                ("an unclosed fence", "text\n```\nand then nothing"),
                ("a lone backtick", "a ` b"),
                ("a bullet with no text", "- \n"),
                ("a heading with no text", "#\n"),
                # Tables, which are the newest way to make this renderer divide
                # by zero or index off the end of a row.
                ("a table with no data rows", "| a | b |\n|---|---|"),
                ("a lone pipe", "|"),
                ("a delimiter row with nothing over it", "|---|---|"),
                ("a row with more cells than its delimiter",
                 "| a | b |\n|---|---|\n| 1 | 2 | 3 |"),
                ("a row with fewer cells than its header",
                 "| a | b | c |\n|---|---|---|\n| 1 |"),
                ("an escaped pipe inside a cell", "| a \\| b |\n|---|\n| c |"),
                ("a table inside a fence, which must stay verbatim",
                 "```\n| a | b |\n|---|---|\n```"),
                ("a table with an empty cell under a right-aligned column",
                 "| a | b |\n|---|---:|\n| | 1 |"),
                ("a table with nothing but pipes", "| | |\n|-|-|\n| | |"),
        ):
            widget.delete("1.0", "end")
            try:
                mdview.render(widget, body)
                out = widget.get("1.0", "end-1c")
                check(f"{label} renders", "raised" in out, False)
                show(f"  {label} ->", repr(out)[:60])
            except Exception as exc:                    # noqa: BLE001
                check(f"{label} renders", repr(exc), "no exception")
    finally:
        widget.destroy()


def check_table_guards(app) -> None:
    """Lines that look a little like tables and must not be read as one.

    A renderer that is lenient about column counts (section 8) buys the risk of
    being lenient about everything, and the cost of a false positive is worse
    than the cost of a miss: a horizontal rule eaten by a table, or a paragraph
    turned into a one-column grid, is damage to text that was already rendering
    correctly. So each guard below is asserted on the rendered output.
    """
    hr("11. lines that must NOT become a table")
    widget = tk.Text(app.root, wrap="word")
    mdview.configure(widget)
    try:
        for label, body, expect in (
            # A delimiter row with no pipes is only a delimiter when its width
            # matches the line above. Here it does not: that "---" is a rule.
            ("a rule under a line that happens to contain a pipe",
             "Note | something\n---\ntail", "─"),
            ("...and the pipe above it is left as the text it is",
             "Note | something\n---\ntail", "Note | something"),
            # With the width disagreeing the other way, again: a rule.
            ("a rule under a three-cell line", "a | b | c\n---", "─"),
            # A fence wins over everything: two passes, structure before inline.
            ("a table inside a fence stays verbatim",
             "```\n| a | b |\n|---|---|\n```", "|---|---|"),
            ("a delimiter row with nothing above it stays literal",
             "|---|---|\ntext", "|"),
        ):
            widget.delete("1.0", "end")
            mdview.render(widget, body)
            out = widget.get("1.0", "end-1c")
            check(label, expect in out, True)
    finally:
        widget.destroy()


# --- main -----------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shot", type=Path, default=None,
                        help="directory to write a PNG of the window into")
    args = parser.parse_args()

    print(f"scratch board: {paths.DB_PATH}")
    print(f"scratch vault: {paths.VAULT_DIR}")
    paths.ensure_dirs()
    conn = db.connect(paths.DB_PATH)
    try:
        db.init_db(conn)
        ids = seed(conn)
    finally:
        conn.close()
    print(f"  seeded{'':<37}thread {ids['t']}")

    original_open = install_webbrowser_probe()
    app = None
    try:
        app = appmod.App(paths.DB_PATH)
        app.root.withdraw()
        app.root.update()
        time.sleep(0.3)
        for _ in range(3):
            app.refresh_now()
            app.root.update()

        page = select_thread(app, "discussion", ids["t"])
        if page is None:
            check("the seeded thread is on the discussion tab", False, True)
            print("\n  the pane was never reached; nothing below is meaningful.")
            FAILURES.append("the seeded thread never appeared on its tab")
        else:
            show("the pane is showing", f"thread {page.thread_id}")
            check("...and it is the thread that was selected",
                  page.thread_id, ids["t"])
            check_structure(page)
            check_inline(page)
            check_fence_is_verbatim(page)
            check_no_double_render(app, page)
            check_receipt_stays_grey(page)
            check_links(app, page)
            check_table_in_the_pane(app, page)
        check_table_natural(app)
        check_table_fitted(app)

        check_edge_bodies(app)
        check_table_guards(app)

        if args.shot is not None:
            args.shot.mkdir(parents=True, exist_ok=True)
            show_window(app, page)
            try:
                # check_look's screenshot, not a screen grab: it asks the
                # window to draw itself with PrintWindow, so what lands in the
                # PNG is this window rather than whatever happens to be on top
                # of it. A first attempt here used ImageGrab and reliably
                # photographed the terminal instead.
                from check_look import screenshot
                png = (args.shot / "mdview.png").resolve()
                show("screenshot", screenshot(app.root, png))
            except Exception as exc:                    # noqa: BLE001
                show("screenshot FAILED", repr(exc))
            finally:
                hide_window(app)
    finally:
        webbrowser.open = original_open
        if app is not None:
            if app.icon is not None:
                try:
                    app.icon.stop()
                except Exception:
                    pass
            app.root.destroy()

    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch board left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print()
    print("WHAT IS NOT ASSERTED HERE: that the rendering LOOKS right -- the fonts,")
    print("the indent and the colours are read by a person, not by this script.")
    print("Nor that the live window John has open is running this code: a running")
    print("App does not reload, so the window on screen is whatever was there")
    print("when it started until it is restarted.")
    print(f"\nscratch board and vault left at {_SCRATCH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
