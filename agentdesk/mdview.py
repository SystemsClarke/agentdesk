"""Render a message body's Markdown into a tk Text widget.

Bodies on the board are written in Markdown -- the agents write headings,
bold, fenced code blocks, lists and links into every thread -- and the window
used to show those markers as literal text. This renders them instead.

It is a renderer and not a Markdown library on purpose: no new dependency for
a GUI that already ships three, and the output is Text-widget tags rather
than HTML, which is the only thing this window can display. The subset is
what the board actually carries: headings, fenced code, bold / italic /
inline code, links, bullet and numbered lists, quotes, rules and tables.
Anything else passes through as plain text, which is the safe failure.

Two passes, deliberately: line structure (headings, fences, lists, tables) is
decided per line FIRST, and inline spans only run on the content of a line that
has none of that structure -- so a `*` that opens a bullet is never also read as
the start of italics, and a fence is never parsed for bold.

Every rendered span also carries the base "md" tag, which is this module's
replacement for the literal-body margin the old insert used.

A TABLE is rendered as a grid of padded monospace columns, not as a widget --
there is no grid widget in this window, and a `Text` is the only thing the pane
has. Column widths come from the widest cell in each column, cells are rendered
through the same inline pass as prose so bold and code survive inside a cell,
and padding is computed on the VISIBLE text so the markup markers do not throw
the columns out.

THE TABLE IS LAID OUT TO THE PANE IT IS DRAWN IN, because it has to be. The
detail pane is about 400px at the window's default size -- some 47 monospace
characters -- and a four-column table of ordinary values wants 78, so a table
at its natural width is not a grid in that pane, it is a grid the widget wraps
at the edge, and wrapping destroys the alignment that is the entire point. So
`render` reads the widget's own width, and if the table is too wide for it the
widest columns are narrowed and their cells are wrapped onto continuation lines
inside the cell -- rows get taller, the grid holds. A cell whose word carries
inline markup is never cut, because cutting mid-span turns `**bold**` into
`**bold`; such a word overflows its column instead, which is visible and rare.
An unmapped widget reports a width of 1, and then the natural widths are used
and nothing is fitted -- the first paint of a window that is still being built
gets the full-width table, and the next refresh gets the fitted one.

`wrap` is a widget option in Tk and not a tag option, so fitting in this way is
the only lever the renderer has: it cannot ask the pane to give the table more
room, so it asks the table to take less.

Leniency is load-bearing, and it is worth naming why. GFM requires the
delimiter row to have the same number of cells as the header, and renders
anything else as plain text. John's own reported table (work item #102) opens
`u| run | container | time | submodule phases |` -- five cells -- over a
four-cell `|---|---|---|---|`. Taken strictly that is not a table at all, which
is exactly the bug report: it renders as its own source. So the column count
here is the widest row in the block and no cell is ever dropped; a header that
disagrees with its rows shows every value it was written with, in a grid, one
column out of step. That is a worse-looking table than a typo-free one and a
much better outcome than a silent deletion or a wall of pipes.
"""

from __future__ import annotations

import re
import tkinter as tk
import tkinter.font as tkfont
import webbrowser

# Inline spans, code tried before bold so a backtick span is never further
# marked up, and bold before italic so **x** is never read as two *x*.
_INLINE = re.compile(
    r"`(?P<code>[^`\n]+)`"
    r"|\*\*(?P<bold>[^*\n]+?)\*\*"
    r"|\*(?P<ital>[^*\n]+)\*"
    r"|\[(?P<ltxt>[^\]\n]+)\]\((?P<lurl>[^)\s]+)\)"
)
_HEADING = re.compile(r"^(#{1,6})\s+(.+?)\s*#*\s*$")
_BULLET = re.compile(r"^(\s*)[-*+]\s+(.*)$")
_NUMBERED = re.compile(r"^(\s*)(\d+)[.)]\s+(.*)$")
_RULE = re.compile(r"^\s*(?:-{3,}|\*{3,}|_{3,})\s*$")
_QUOTE = re.compile(r"^\s*>\s?(.*)$")

# A line that could be a table's delimiter row: only pipes, hyphens, colons and
# spaces. Written this loosely and then checked cell by cell, rather than as one
# clever pattern, because the alternation of `\s*` inside a repeated group is a
# backtracking trap on a long line of spaces and the payoff would be cosmetic.
_TABLE_CHARS = re.compile(r"^[|\-:\s]+$")
_DELIM_CELL = re.compile(r"^\s*(:?)-+(:?)\s*$")
# The character an escaped pipe is hidden behind while a row is split, so that
# `a \| b` is one cell rather than two. NUL cannot occur in a message body.
_ESCAPED_PIPE = "\x00"

# The md tag's left margin, on both sides. `render` reads it back from the tag
# rather than duplicating the number, so a table is measured against the width
# the text actually has and not the width of the widget.
_MD_TAG = "md"
# Columns are never narrowed past this many characters, and a table that cannot
# be made to fit at this floor is left at its natural width: a grid that
# overflows the pane is worse than a narrow one, but a grid of two-character
# columns is worse than either.
_TABLE_MIN_COL = 6
# Blank columns either side of the text, and slack so that a table which exactly
# fills the available width cannot trip the widget's wrap on a rounding error.
_TABLE_SLACK_PX = 12


def configure(widget: tk.Text) -> None:
    """Create the tags render() will use. Call once, when the widget is built."""
    base = tkfont.nametofont("TkTextFont")
    family, size = base.actual("family"), base.actual("size")
    mono = tkfont.nametofont("TkFixedFont").actual("family")
    widget._md_links: dict[str, str] = {}
    widget.tag_configure("md", lmargin1=12, lmargin2=12)
    widget.tag_configure("md-h1", font=(family, size + 3, "bold"),
                         spacing1=8, spacing3=2)
    widget.tag_configure("md-h2", font=(family, size + 2, "bold"),
                         spacing1=8, spacing3=2)
    widget.tag_configure("md-h3", font=(family, size + 1, "bold"), spacing1=6)
    widget.tag_configure("md-h456", font=(family, size, "bold"), spacing1=6)
    widget.tag_configure("md-bold", font=(family, size, "bold"))
    widget.tag_configure("md-italic", font=(family, size, "italic"))
    widget.tag_configure("md-code", font=(mono, size), background="#efefef")
    widget.tag_configure("md-codeblock", font=(mono, size),
                         background="#f5f5f5", spacing1=4)
    widget.tag_configure("md-bullet", lmargin1=26, lmargin2=14)
    widget.tag_configure("md-quote", foreground="#555555",
                         lmargin1=30, lmargin2=18)
    widget.tag_configure("md-rule", foreground="#aaaaaa", spacing1=6, spacing3=6)
    # The table tags are monospace, which is the only reason the columns can
    # line up at all: the grid is padding characters, and padding only measures
    # what it says if every character is the same width. Courier New is what
    # TkFixedFont is on this machine, and it was measured rather than assumed --
    # the box-drawing characters below report 8px, the same as an "a", so the
    # rule under the header is exactly as wide as the header cell above it.
    widget.tag_configure("md-table", font=(mono, size))
    widget.tag_configure("md-th", font=(mono, size, "bold"))
    widget.tag_configure("md-trule", font=(mono, size), foreground="#999999")
    widget.tag_configure("md-link", foreground="#0b5394", underline=True)
    # One shared binding: the handler resolves which URL from the more
    # specific per-link tag under the cursor, not from the event itself.
    widget.tag_bind("md-link", "<Button-1>", _link_click)
    widget.tag_bind("md-link", "<Enter>",
                    lambda _e: widget.config(cursor="hand2"))
    widget.tag_bind("md-link", "<Leave>",
                    lambda _e: widget.config(cursor=""))


def render(widget: tk.Text, text: str) -> None:
    """Append `text` to the widget, rendered.

    The caller owns clearing the widget and its disabled state; this only
    inserts. A per-link tag map is left on the widget for the click handler.

    That map is rebuilt once per REFRESH, not once per call, and the empty
    widget is the only place it restarts. The caller clears the widget and
    then renders every message into it, so resetting the numbering per call
    would hand the first link of every message the same tag -- md-link-0 --
    and leave one map entry behind it holding the LAST url rendered. Clicking
    any link in the pane then opened the final message's link instead of its
    own. Measured on two messages each holding one link, before this was per
    refresh: both clicks opened the second message's url.
    """
    if widget.index("end-1c") == "1.0":
        widget._md_links = {}
    lines = text.split("\n")
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        if stripped.startswith("```"):
            # A fenced block is verbatim: no inline markup inside it, and the
            # content keeps its own line breaks. An unterminated fence takes
            # the rest of the body, which beats silently eating the markers.
            block: list[str] = []
            i += 1
            while i < len(lines) and not lines[i].strip().startswith("```"):
                block.append(lines[i])
                i += 1
            if i < len(lines):
                i += 1  # the closing fence
            widget.insert("end", "\n".join(block), ("md", "md-codeblock"))
            _newline(widget, "md-codeblock")
            continue
        # A table is tried before the rule, because a delimiter row is made of
        # hyphens and would otherwise be caught by _RULE. It cannot swallow a
        # bare rule: _table_at requires a pipe on the line above.
        table = _table_at(lines, i)
        if table is not None:
            header, delim, rows, i = table
            _table(widget, header, delim, rows)
            continue
        if _RULE.match(line):
            widget.insert("end", "─" * 46, ("md", "md-rule"))
            _newline(widget)
            i += 1
            continue
        m = _HEADING.match(line)
        if m:
            depth = len(m.group(1))
            tag = ("md-h1" if depth == 1 else "md-h2" if depth == 2 else
                   "md-h3" if depth == 3 else "md-h456")
            widget.insert("end", m.group(2), ("md", tag))
            _newline(widget)
            i += 1
            continue
        m = _QUOTE.match(line)
        if m:
            widget.insert("end", m.group(1), ("md", "md-quote", "md-italic"))
            _newline(widget)
            i += 1
            continue
        m = _BULLET.match(line)
        if m:
            widget.insert("end", ("    " if m.group(1) else "") + "• ",
                          ("md", "md-bullet"))
            _inline(widget, m.group(2), ("md-bullet",))
            _newline(widget)
            i += 1
            continue
        m = _NUMBERED.match(line)
        if m:
            widget.insert("end", f"{m.group(1)}{m.group(2)}. ",
                          ("md", "md-bullet"))
            _inline(widget, m.group(3), ("md-bullet",))
            _newline(widget)
            i += 1
            continue
        _inline(widget, line)
        _newline(widget)
        i += 1


# --- tables ---------------------------------------------------------------------

def _split_row(line: str) -> list[str]:
    """One table row's cells, with one optional leading and trailing pipe gone.

    `\\|` is an escaped pipe and belongs to the cell it is written in, so it is
    hidden behind a NUL before the split and restored after -- otherwise a cell
    containing a pipe would be silently cut into two cells, which shows up as a
    column of garbage rather than as an error anybody can see.
    """
    s = line.strip().replace("\\|", _ESCAPED_PIPE)
    if s.startswith("|"):
        s = s[1:]
    if s.endswith("|"):
        s = s[:-1]
    return [c.strip().replace(_ESCAPED_PIPE, "|") for c in s.split("|")]


def _is_delim(line: str) -> bool:
    """Is this line a table's delimiter row (`|---|---|`, `:--: | ---`)?

    Every cell must be at least one hyphen, optionally colon-wrapped. A line of
    hyphens and spaces that is not made of such cells -- a bullet, say -- is not
    a delimiter however much of the alphabet it shares.
    """
    if not _TABLE_CHARS.match(line) or "-" not in line:
        return False
    return all(_DELIM_CELL.match(c.strip()) for c in _split_row(line))


def _table_at(lines: list[str], i: int):
    """(header, delimiter, rows, index_after) if a table starts at lines[i].

    The pipe in the header line is the first of two conditions and both are
    needed: a delimiter row with nothing over it is just a line of hyphens.

    The second condition is the leniency the module docstring explains. A
    delimiter row written WITHOUT pipes (`---` under `| Name |`) is only a
    delimiter when its cell count matches the header's, as GFM says -- otherwise
    it is a horizontal rule under a line that happened to contain a pipe, which
    is what it looks like, and treating it as a table would eat the rule and
    invent a one-column table nobody wrote. A delimiter row written WITH pipes
    is taken as deliberate: its width is trusted and a header that disagrees is
    rendered anyway rather than dropped.
    """
    if i + 1 >= len(lines) or "|" not in lines[i]:
        return None
    delim_line = lines[i + 1]
    if not _is_delim(delim_line):
        return None
    header, delim = _split_row(lines[i]), _split_row(delim_line)
    if "|" not in delim_line and len(delim) != len(header):
        return None
    rows: list[list[str]] = []
    j = i + 2
    while j < len(lines):
        line = lines[j]
        stripped = line.strip()
        # The block ends at a blank line, at a line with no pipe in it, and at
        # anything that is plainly another construct -- a fence, a heading, a
        # rule, a second delimiter. Without the last of those, a heading that
        # happens to contain a pipe becomes a row of a table it does not belong
        # to, which is worse than ending the table a row early.
        if (not stripped or "|" not in line or stripped.startswith("```")
                or _is_delim(line) or _HEADING.match(line) or _RULE.match(line)):
            break
        rows.append(_split_row(line))
        j += 1
    return header, delim, rows, j


def _plain(text: str) -> str:
    """A cell's text as it will APPEAR -- markup markers and urls removed.

    Column widths are computed from this and not from the source, because the
    source is longer than the screen by however many `**` and backticks it
    carries: sizing on the raw cell would leave every bolded column two
    characters wider than its contents and the grid would be visibly loose.
    """
    out: list[str] = []
    pos = 0
    for m in _INLINE.finditer(text):
        out.append(text[pos:m.start()])
        for group in ("code", "bold", "ital", "ltxt"):
            if m.group(group) is not None:
                out.append(m.group(group))
                break
        pos = m.end()
    out.append(text[pos:])
    return "".join(out)


def _char_px(widget: tk.Text) -> int:
    """The rendered width of one monospace character, measured once.

    Measured and not assumed: the whole grid is padding characters, and if the
    font were not monospace -- or were one where a box-drawing character is
    double width -- every column would be laid out to a width it does not have.
    Cached on the widget because measuring is a round trip into Tcl and this is
    called for every table of every repaint.
    """
    px = getattr(widget, "_md_char_px", 0)
    if not px:
        px = tkfont.nametofont("TkFixedFont").measure("0") or 1
        widget._md_char_px = px
    return px


def capacity_chars(widget: tk.Text) -> int:
    """How many monospace characters fit beside the md tag's margin.

    Public because the window needs the same number to decide whether a resize
    needs a repaint -- see Page's Configure handler in app.py. Answers 0 for a
    widget that is not mapped yet, which is what makes a table fall back to its
    natural width on a first paint rather than being fitted to a 1px pane.
    """
    width = widget.winfo_width()
    if width <= 1:
        return 0
    try:
        margin = int(widget.tag_cget(_MD_TAG, "lmargin1") or 0)
    except tk.TclError:
        margin = 0
    room = width - 2 * margin - _TABLE_SLACK_PX
    return max(_TABLE_MIN_COL, room // _char_px(widget))


def _fit_widths(natural: list[int], avail: int) -> list[int]:
    """The column widths to draw with: `natural`, narrowed until it fits.

    The slack comes out of every column in proportion to how much room that
    column has above the floor, not off whichever column is currently widest.
    The difference is worth the arithmetic, and it was measured: taking a
    character off the widest column over and over LEVELS the columns, so a
    27-character run name and a 4-character header end up with the same width
    and the name is cut into four pieces. Dropping the widest column to the
    floor first would be the opposite mistake -- one column destroyed to spare
    the rest. Proportional keeps the shape of the table, so the columns that
    hold the most text keep the most room and their cells break where the words
    are instead of mid-token.

    Rounding leaves a character or two to place, and those come off the widest
    column. If the table cannot be made to fit even with every column at the
    floor, the natural widths come back and the pane wraps it -- an honest
    failure, and the pane is too narrow for any grid of this shape.
    """
    if avail <= 0 or sum(natural) + 3 * (len(natural) - 1) <= avail:
        return list(natural)
    slack = sum(natural) + 3 * (len(natural) - 1) - avail
    room = sum(w - _TABLE_MIN_COL for w in natural if w > _TABLE_MIN_COL)
    if room < slack:
        return list(natural)
    widths = list(natural)
    remaining = slack
    for j, w in enumerate(natural):
        if w <= _TABLE_MIN_COL:
            continue
        take = min(round(slack * (w - _TABLE_MIN_COL) / room), w - _TABLE_MIN_COL,
                   remaining)
        widths[j] -= take
        remaining -= take
    while remaining > 0:
        room_j = [j for j, w in enumerate(widths) if w > _TABLE_MIN_COL]
        if not room_j:
            return list(natural)
        widths[max(room_j, key=lambda j: widths[j])] -= 1
        remaining -= 1
    return widths


def _has_markup(word: str) -> bool:
    """Could cutting this word in half cut through inline markup? Assume yes."""
    return _INLINE.search(word) is not None or any(c in word for c in "*`[")


def _wrap_cell(cell: str, width: int) -> list[str]:
    """A cell's SOURCE split into pieces that each fit `width` columns.

    Source pieces and not plain ones, so the pieces are still renderable: each
    is handed to `_inline` exactly as an unwrapped cell would be, and bold and
    code survive the wrap. Packing is on whitespace and on visible width, so
    `**two words**` counts as eleven and not thirteen.

    A word too wide for its column is cut at the column edge -- that is the
    common case this exists for, a run name or a host with no spaces in it --
    but only if it carries no inline markup. Cutting a markup word would leave
    `**bold` on screen, which is visibly worse than a column that overflows, so
    such a word is left whole.
    """
    if len(_plain(cell)) <= width:
        return [cell]
    lines: list[str] = []
    cur = ""
    for word in cell.split():
        candidate = f"{cur} {word}" if cur else word
        if len(_plain(candidate)) <= width:
            cur = candidate
            continue
        if cur:
            lines.append(cur)
        while len(word) > width and not _has_markup(word):
            lines.append(word[:width])
            word = word[width:]
        cur = word
    if cur:
        lines.append(cur)
    return lines or [""]


def _aligns(delim: list[str], ncols: int) -> list[str]:
    """Per-column alignment from the delimiter row's colons; left by default."""
    out = []
    for j in range(ncols):
        m = _DELIM_CELL.match(delim[j]) if j < len(delim) else None
        if m and m.group(1) == ":" and m.group(2) == ":":
            out.append("c")
        elif m and m.group(2) == ":":
            out.append("r")
        else:
            out.append("l")
    return out


def _pads(cell: str, width: int, align: str) -> tuple[int, int]:
    """(spaces before, spaces after) that centre or push a cell to its column."""
    gap = max(0, width - len(_plain(cell)))
    if align == "r":
        return gap, 0
    if align == "c":
        return gap // 2, gap - gap // 2
    return 0, gap


def _table_line(widget: tk.Text, cells: list[str], widths: list[int],
                aligns: list[str], tags: tuple) -> None:
    """One rendered row: every cell padded to its column width.

    Trailing blanks are invisible but they are not nothing: they make the line
    longer than its text, and a pane that word-wraps will wrap a table line that
    would otherwise have fitted -- right at the moment a row's last cell is
    empty, which is the common case for a ragged row. So the last column takes
    no padding, and when it holds nothing at all it takes no separator blank
    either. Constructing that in rather than deleting it afterwards keeps this
    free of Tk's end-relative index arithmetic, which is off by one from the
    obvious reading and silent when it is wrong. The column WIDTHS are
    unaffected: the header's rule still spans every column in full.
    """
    last = len(cells) - 1
    tail_is_blank = _plain(cells[last]) == ""
    for j, cell in enumerate(cells):
        if j:
            widget.insert("end", " │" if j == last and tail_is_blank else " │ ",
                          ("md",) + tags)
        if j == last and tail_is_blank:
            continue
        left, right = _pads(cell, widths[j], aligns[j])
        if j == last:
            right = 0
        if left:
            widget.insert("end", " " * left, ("md",) + tags)
        _inline(widget, cell, tags)
        if right:
            widget.insert("end", " " * right, ("md",) + tags)
    _newline(widget)


def _emit_row(widget: tk.Text, parts: list[list[str]], widths: list[int],
              aligns: list[str], tags: tuple) -> None:
    """One logical row, as the physical lines its tallest cell needs.

    The header goes through here exactly as a data row does. It was drawn as a
    single line to begin with, and the result was a header wider than the pane
    it had just been fitted to -- "submodule phases" in a six-character column,
    sticking 12px out past the edge of a table whose rows all fit. The header is
    a row; it wraps like one.
    """
    for k in range(max(len(p) for p in parts)):
        _table_line(widget, [p[k] if k < len(p) else "" for p in parts],
                    widths, aligns, tags)


def _table(widget: tk.Text, header: list[str], delim: list[str],
           rows: list[list[str]]) -> None:
    """Render a whole table as a padded monospace grid, fitted to the pane."""
    # The column count is the widest row in the block, header and delimiter
    # included, so that NO cell is ever dropped. A body row with an extra cell
    # (a delimiter row somebody under-counted, which is common) then widens the
    # table instead of losing its last value; a short row is padded, which is
    # visible as an empty cell and honest about what was written.
    ncols = max([len(header), len(delim)] + [len(r) for r in rows])

    def cells_of(row: list[str]) -> list[str]:
        return [row[j] if j < len(row) else "" for j in range(ncols)]

    header = cells_of(header)
    body = [cells_of(r) for r in rows]
    natural = [max(len(_plain(cell)) for cell in [header[j]] + [r[j] for r in body])
               for j in range(ncols)]
    widths = _fit_widths(natural, capacity_chars(widget))
    aligns = _aligns(delim, ncols)
    _emit_row(widget, [_wrap_cell(header[j], widths[j]) for j in range(ncols)],
              widths, aligns, ("md-table", "md-th"))
    widget.insert("end", "─┼─".join("─" * w for w in widths),
                  ("md", "md-table", "md-trule"))
    _newline(widget)
    # A row is as many physical lines as its tallest wrapped cell needs. Short
    # cells on the continuation lines are blank, and blank is drawn as blank --
    # the separator is still emitted for them, so a reader can follow a column
    # down through a row that wrapped.
    for row in body:
        _emit_row(widget, [_wrap_cell(row[j], widths[j]) for j in range(ncols)],
                  widths, aligns, ("md-table",))


def _inline(widget: tk.Text, text: str, extra: tuple = ()) -> None:
    """Insert one line's content with its inline spans marked up."""
    pos = 0
    for m in _INLINE.finditer(text):
        if m.start() > pos:
            widget.insert("end", text[pos:m.start()], ("md",) + extra)
        if m.group("code") is not None:
            widget.insert("end", m.group("code"), ("md", "md-code") + extra)
        elif m.group("bold") is not None:
            widget.insert("end", m.group("bold"), ("md", "md-bold") + extra)
        elif m.group("ital") is not None:
            widget.insert("end", m.group("ital"), ("md", "md-italic") + extra)
        else:
            # Each link range gets its own tag on top of the shared "md-link"
            # look, and the URL is looked up through that tag on click -- the
            # event carries a position, not the URL, and two links on one
            # line must not resolve to each other.
            ltag = f"md-link-{len(widget._md_links)}"
            widget._md_links[ltag] = m.group("lurl")
            widget.insert("end", m.group("ltxt"),
                          ("md", "md-link", ltag) + extra)
        pos = m.end()
    if pos < len(text):
        widget.insert("end", text[pos:], ("md",) + extra)


def _newline(widget: tk.Text, *tags: str) -> None:
    widget.insert("end", "\n", ("md",) + tags)


def _link_click(event: tk.Event) -> str:
    widget = event.widget
    try:
        tags = widget.tag_names(widget.index(f"@{event.x},{event.y}"))
    except tk.TclError:
        return "break"
    links = getattr(widget, "_md_links", {})
    for tag in tags:
        url = links.get(tag)
        if url:
            webbrowser.open(url)
            break
    return "break"
