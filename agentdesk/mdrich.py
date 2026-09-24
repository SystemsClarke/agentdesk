"""The terminal view's richer Markdown: Mermaid drawn as box-drawing art, and code highlighting.

Pure functions returning lines of (text, colour) segments, so a caller can render them
however it likes and the layout can be tested without a window. Also the line-structure
regexes and table parsing slackfmt uses to turn a board message into Slack blocks. Colours are the
terminal palette's names: fg mu fa rule pk or ye gr cy pu.
"""

from __future__ import annotations

import re
from typing import Optional

Seg = tuple  # (text, colour or None)

# --- a character canvas ----------------------------------------------------------

_U, _D, _L, _R = 1, 2, 4, 8
_GLYPH = {_U | _D: "│", _L | _R: "─", _D | _R: "┌", _D | _L: "┐", _U | _R: "└", _U | _L: "┘",
          _U | _D | _R: "├", _U | _D | _L: "┤", _D | _L | _R: "┬", _U | _L | _R: "┴",
          _U | _D | _L | _R: "┼", _U: "│", _D: "│", _L: "─", _R: "─"}


class Canvas:
    """A character grid. Lines record which directions meet in each cell, so corners
    and junctions resolve to the right box-drawing glyph when the grid is rendered."""

    def __init__(self, w: int, h: int) -> None:
        self.w, self.h = w, h
        self.ch = [[" "] * w for _ in range(h)]
        self.col: list = [[None] * w for _ in range(h)]
        self.mask = [[0] * w for _ in range(h)]
        self.dash = [[False] * w for _ in range(h)]

    def put(self, x: int, y: int, c: str, color=None, over: bool = True) -> None:
        if 0 <= x < self.w and 0 <= y < self.h:
            if not over and (self.ch[y][x] != " " or self.mask[y][x]):
                return
            self.ch[y][x] = c
            self.col[y][x] = color
            self.mask[y][x] = 0

    def text(self, x: int, y: int, s: str, color=None, over: bool = True) -> None:
        for i, c in enumerate(s):
            self.put(x + i, y, c, color, over)

    def _line(self, x: int, y: int, bits: int, color, dashed: bool) -> None:
        if 0 <= x < self.w and 0 <= y < self.h and self.ch[y][x] == " ":
            self.mask[y][x] |= bits
            self.col[y][x] = color
            self.dash[y][x] = self.dash[y][x] or dashed

    def hline(self, x1: int, x2: int, y: int, color=None, dashed: bool = False) -> None:
        lo, hi = min(x1, x2), max(x1, x2)
        if lo == hi:
            return  # a zero-length hop draws nothing; otherwise straight edges get a stray ┼
        for x in range(lo, hi + 1):
            self._line(x, y, (_L if x > lo else 0) | (_R if x < hi else 0), color, dashed)

    def vline(self, x: int, y1: int, y2: int, color=None, dashed: bool = False) -> None:
        lo, hi = min(y1, y2), max(y1, y2)
        if lo == hi:
            return
        for y in range(lo, hi + 1):
            self._line(x, y, (_U if y > lo else 0) | (_D if y < hi else 0), color, dashed)

    def box(self, x: int, y: int, w: int, lines: list, shape: str, color=None, text_color="fg") -> None:
        h = len(lines) + 2
        tl, tr, bl, br = {"round": "╭╮╰╯", "diamond": "◆◆◆◆", "double": "╔╗╚╝"}.get(shape, "┌┐└┘")
        hz, vt = ("═", "║") if shape == "double" else ("─", "│")
        self.put(x, y, tl, color)
        self.put(x + w - 1, y, tr, color)
        self.put(x, y + h - 1, bl, color)
        self.put(x + w - 1, y + h - 1, br, color)
        for i in range(1, w - 1):
            self.put(x + i, y, hz, color)
            self.put(x + i, y + h - 1, hz, color)
        for j in range(1, h - 1):
            self.put(x, y + j, vt, color)
            self.put(x + w - 1, y + j, vt, color)
            label = lines[j - 1]
            pad = (w - 2 - len(label)) // 2
            self.text(x + 1, y + j, " " * (w - 2), None)
            self.text(x + 1 + pad, y + j, label, text_color)

    def render(self) -> list:
        out = []
        for y in range(self.h):
            row = [(_GLYPH.get(self.mask[y][x], "┼") if self.mask[y][x] and self.ch[y][x] == " " else self.ch[y][x])
                   for x in range(self.w)]
            for x in range(self.w):
                if self.mask[y][x] and self.dash[y][x] and row[x] in "│─":
                    row[x] = "┆" if row[x] == "│" else "┄"
            last = len(row)
            while last > 0 and row[last - 1] == " ":
                last -= 1
            segs, cur_col, buf = [], object(), ""
            for x in range(last):
                c = self.col[y][x]
                if c != cur_col and buf:
                    segs.append((buf, cur_col))
                    buf = ""
                cur_col = c
                buf += row[x]
            if buf:
                segs.append((buf, cur_col))
            out.append(segs)
        while out and not out[-1]:
            out.pop()
        return out


def _wrap(label: str, width: int) -> list:
    words, lines, cur = label.split(), [], ""
    for w in words:
        if len(w) > width:
            w = w[: width - 1] + "…"
        if cur and len(cur) + 1 + len(w) > width:
            lines.append(cur)
            cur = w
        else:
            cur = f"{cur} {w}" if cur else w
    if cur:
        lines.append(cur)
    return lines or [""]


# --- flowcharts and state diagrams -----------------------------------------------

_SHAPES = [("((", "))", "round"), ("([", "])", "round"), ("[[", "]]", "double"), ("[(", ")]", "round"),
           ("{{", "}}", "diamond"), ("[", "]", "rect"), ("(", ")", "round"), ("{", "}", "diamond"),
           (">", "]", "rect")]
_NODE_RE = re.compile(r"\s*([A-Za-z0-9_\-\.\*\[\]]+?)\s*(\(\(.*?\)\)|\(\[.*?\]\)|\[\[.*?\]\]|\[\(.*?\)\]|"
                      r"\{\{.*?\}\}|\[.*?\]|\(.*?\)|\{.*?\}|>.*?\])?\s*$")
# Labelled forms first: otherwise `A -- label --> B` splits at the leading `--`.
_ARROW_SPLIT = re.compile(r"(\s*(?:--\s+[^->|]+?\s+-->|==\s+[^=>|]+?\s+==>|<?-\.+->?|<?-{2,}[>xo]?|<?={2,}>?)"
                          r"\s*(?:\|[^|]*\|)?\s*)")


def _parse_node(tok: str, nodes: dict, order: list) -> Optional[str]:
    tok = tok.strip()
    if not tok:
        return None
    m = _NODE_RE.match(tok)
    if not m:
        nid, shape_part = tok, None
    else:
        nid, shape_part = m.group(1), m.group(2)
    label, shape = None, "rect"
    if shape_part:
        for open_, close, sh in _SHAPES:
            if shape_part.startswith(open_) and shape_part.endswith(close):
                label = shape_part[len(open_): len(shape_part) - len(close)].strip().strip('"')
                shape = sh
                break
    if nid not in nodes:
        star = {"[*]": "●", "[*]end": "◉"}.get(nid)
        nodes[nid] = {"label": label or star or nid, "shape": "round" if star else shape}
        order.append(nid)
    elif label:
        nodes[nid]["label"], nodes[nid]["shape"] = label, shape
    return nid


def parse_flow(src: list) -> tuple:
    head = src[0].strip().split()
    kind = head[0].lower()
    direction = head[1].upper() if len(head) > 1 else "TD"
    if kind.startswith("state"):
        direction = "TD"
    for line in src[1:]:
        s = line.strip()
        if s.lower().startswith("direction "):
            direction = s.split()[1].upper()
    nodes, order, edges = {}, [], []
    for line in src[1:]:
        s = line.strip().rstrip(";")
        if not s or s.startswith("%%"):
            continue
        low = s.lower()
        if low.startswith(("subgraph", "end", "style ", "classdef", "class ", "click ", "linkstyle",
                           "direction ", "note ", "state ")) or s == "end":
            continue
        label_after = None
        if kind.startswith("state") and ":" in s and "-->" in s:
            s, label_after = s.split(":", 1)
            label_after = label_after.strip()
        parts = _ARROW_SPLIT.split(s)
        if len(parts) == 1:
            _parse_node(parts[0], nodes, order)
            continue
        prev = [_parse_node(p, nodes, order) for p in parts[0].split("&")]
        i = 1
        while i + 1 < len(parts):
            arrow, target = parts[i], parts[i + 1]
            lbl = None
            m = re.search(r"\|([^|]*)\|", arrow)
            if m:
                lbl = m.group(1).strip()
            m2 = re.match(r"\s*(?:--|==)\s+(.+?)\s+(?:-->|==>)", arrow)
            if m2:
                lbl = m2.group(1).strip()
            if label_after and i + 2 >= len(parts):
                lbl = label_after
            dashed = "." in arrow
            if kind.startswith("state") and target.strip() == "[*]":
                target = "[*]end"
            cur = [_parse_node(t, nodes, order) for t in target.split("&")]
            for a in prev:
                for b in cur:
                    if a and b:
                        edges.append((a, b, lbl, dashed))
            prev = cur
            i += 2
    return direction, nodes, order, edges


def _levels(order: list, edges: list) -> tuple:
    succ = {n: [] for n in order}
    for a, b, *_ in edges:
        succ[a].append(b)
    level = {n: 0 for n in order}
    back = set()
    state = {}

    def dfs(n, depth_stack):
        state[n] = 1
        for m in succ[n]:
            if state.get(m) == 1:
                back.add((n, m))
                continue
            if level[m] < level[n] + 1:
                level[m] = level[n] + 1
                dfs(m, depth_stack)
            elif state.get(m) != 2:
                dfs(m, depth_stack)
        state[n] = 2

    targets = {b for _a, b, *_ in edges}
    for n in [x for x in order if x not in targets] + order:
        if n not in state:
            dfs(n, [])
    return level, back


def draw_flow(src: list, max_width: int, force_direction: Optional[str] = None) -> list:
    direction, nodes, order, edges = parse_flow(src)
    direction = force_direction or direction
    if not nodes:
        return []
    level, back = _levels(order, edges)
    horizontal = direction in ("LR", "RL")
    maxlab = 18 if horizontal else 22
    for n in order:
        lines = _wrap(nodes[n]["label"], maxlab)
        nodes[n]["lines"] = lines
        nodes[n]["w"] = max(len(l) for l in lines) + 4
        nodes[n]["h"] = len(lines) + 2
    by_level: dict = {}
    for n in order:
        by_level.setdefault(level[n], []).append(n)
    depth = max(by_level) + 1
    # One barycentre pass so children sit under their parents and edges cross less.
    pos_index = {}
    for lv in range(depth):
        row = by_level.get(lv, [])
        if lv:
            def bary(n):
                parents = [pos_index[a] for a, b, *_ in edges if b == n and a in pos_index and level[a] < lv]
                return sum(parents) / len(parents) if parents else 1e9
            row.sort(key=bary)
        for i, n in enumerate(row):
            pos_index[n] = i

    if not horizontal:
        gap_x, gap_y = 4, 4
        widths = {lv: sum(nodes[n]["w"] for n in by_level.get(lv, [])) + gap_x * (len(by_level.get(lv, [])) - 1)
                  for lv in range(depth)}
        total_w = max(widths.values()) + 2
        y = 0
        for lv in range(depth):
            row = by_level.get(lv, [])
            x = (total_w - widths[lv]) // 2
            h = max(nodes[n]["h"] for n in row) if row else 0
            for n in row:
                nodes[n]["x"], nodes[n]["y"] = x, y + (h - nodes[n]["h"]) // 2
                x += nodes[n]["w"] + gap_x
            y += h + gap_y
        cv = Canvas(max(total_w, 10) + 24, y)
    else:
        gap_x, gap_y = 8, 1
        heights = {lv: sum(nodes[n]["h"] for n in by_level.get(lv, [])) + gap_y * (len(by_level.get(lv, [])) - 1)
                   for lv in range(depth)}
        total_h = max(heights.values())
        x = 0
        for lv in range(depth):
            row = by_level.get(lv, [])
            y = (total_h - heights[lv]) // 2
            w = max(nodes[n]["w"] for n in row) if row else 0
            for n in row:
                nodes[n]["x"], nodes[n]["y"] = x + (w - nodes[n]["w"]) // 2, y
                y += nodes[n]["h"] + gap_y
            x += w + gap_x
        cv = Canvas(x + 2, total_h + 1)

    loops = []
    for a, b, lbl, dashed in edges:
        if (a, b) in back or level[b] <= level[a]:
            loops.append((a, b, lbl))
            continue
        na, nb = nodes[a], nodes[b]
        if not horizontal:
            sx, sy = na["x"] + na["w"] // 2, na["y"] + na["h"]
            tx, ty = nb["x"] + nb["w"] // 2, nb["y"] - 1
            my = sy + 1
            cv.vline(sx, sy, my, "rule", dashed)
            cv.hline(sx, tx, my, "rule", dashed)
            cv.vline(tx, my, ty, "rule", dashed)
            cv.put(tx, ty, "▼", "cy")
            if lbl:
                cv.text(tx + 2, ty - 1 if ty - 1 > my else ty, lbl[:24], "pu", over=False)
        else:
            sx, sy = na["x"] + na["w"], na["y"] + na["h"] // 2
            tx, ty = nb["x"] - 1, nb["y"] + nb["h"] // 2
            mx = sx + 2
            cv.hline(sx, mx, sy, "rule", dashed)
            cv.vline(mx, sy, ty, "rule", dashed)
            cv.hline(mx, tx, ty, "rule", dashed)
            cv.put(tx, ty, "►", "cy")
            if lbl:
                cv.text(mx + 1, ty - 1, lbl[: max(4, tx - mx - 1)], "pu", over=False)
    hues = ["cy", "gr", "ye", "pu", "or", "pk"]
    for n in order:
        nd = nodes[n]
        color = "pk" if nd["shape"] == "diamond" else hues[level[n] % len(hues)]
        cv.box(nd["x"], nd["y"], nd["w"], nd["lines"], nd["shape"], color)
    out = cv.render()
    for a, b, lbl in loops:
        out.append([("  ↺ ", "or"), (nodes[a]["label"], "fg"), (" loops back to ", "mu"), (nodes[b]["label"], "fg")]
                   + ([(f"  ({lbl})", "pu")] if lbl else []))
    return out


# --- sequence diagrams -----------------------------------------------------------

_MSG_RE = re.compile(r"^\s*([^\-\s>]+?)\s*(-{1,2}>>|-{1,2}>|-{1,2}x|-{1,2}\))\s*([+\-]?)([^:]+?)\s*:\s*(.*)$")


def draw_sequence(src: list, max_width: int) -> list:
    parts, alias, events = [], {}, []
    for line in src[1:]:
        s = line.strip()
        if not s or s.startswith("%%"):
            continue
        m = re.match(r"^(participant|actor)\s+(\S+)(?:\s+as\s+(.+))?$", s, re.I)
        if m:
            pid = m.group(2)
            alias[pid] = (m.group(3) or pid).strip()
            if pid not in parts:
                parts.append(pid)
            continue
        m = _MSG_RE.match(s)
        if m:
            a, arrow, _act, b, text = m.groups()
            b = b.strip()
            for p in (a, b):
                if p not in parts:
                    parts.append(p)
            events.append(("msg", a, b, text, arrow.startswith("--")))
            continue
        m = re.match(r"^note\s+(over|left of|right of)\s+([^:]+):\s*(.*)$", s, re.I)
        if m:
            events.append(("note", m.group(2).split(",")[0].strip(), m.group(3)))
            continue
        m = re.match(r"^(loop|alt|else|opt|par|and|critical|break|rect)\b\s*(.*)$", s, re.I)
        if m:
            events.append(("frame", m.group(1).lower(), m.group(2)))
            continue
        if s.lower() == "end":
            events.append(("frame", "end", ""))
    if not parts:
        return []
    names = [alias.get(p, p) for p in parts]
    widths = [len(n) + 4 for n in names]
    gaps = [12] * (len(parts) - 1)
    for e in events:
        if e[0] == "msg" and e[1] in parts and e[2] in parts:
            i, j = sorted((parts.index(e[1]), parts.index(e[2])))
            if j == i + 1:
                gaps[i] = max(gaps[i], len(e[3]) + 6 - (widths[i] + widths[j]) // 2)
    centers, x = [], 2
    for i, w in enumerate(widths):
        centers.append(x + w // 2)
        x += w + (gaps[i] if i < len(gaps) else 0)
    width = x + 30
    rows = 3 + sum(2 if e[0] == "msg" else 1 for e in events) + 1
    cv = Canvas(width, rows)
    for i, n in enumerate(names):
        cv.box(centers[i] - widths[i] // 2, 0, widths[i], [n], "round", "cy", "fg")
    y = 3
    for e in events:
        if e[0] == "msg":
            _k, a, b, text, dashed = e
            ia, ib = parts.index(a), parts.index(b)
            if ia == ib:
                cv.text(centers[ia] + 2, y, f"↺ {text}", "pu")
                y += 2
                continue
            x1, x2 = centers[ia], centers[ib]
            lo, hi = sorted((x1, x2))
            cv.text(lo + 2, y, text[: max(3, hi - lo - 3)], "fg")
            cv.hline(lo + 1, hi - 1, y + 1, "ye", dashed)
            cv.put(x2 - 1 if x2 > x1 else x2 + 1, y + 1, "►" if x2 > x1 else "◄", "ye")
            y += 2
        elif e[0] == "note":
            i = parts.index(e[1]) if e[1] in parts else 0
            cv.text(centers[i] + 2, y, f"▌ {e[2]}", "or")
            y += 1
        else:
            label = e[1] if e[1] != "end" else "end"
            cv.text(1, y, f"┆ {label} {e[2]}".rstrip(), "pu")
            y += 1
    for i in range(len(parts)):
        cv.vline(centers[i], 3, y, "rule", dashed=True)
    return cv.render()


# --- pie ---------------------------------------------------------------------------

def draw_pie(src: list, max_width: int) -> list:
    title = re.sub(r"^pie\s*(title\s*)?", "", src[0].strip(), flags=re.I).strip()
    items = []
    for line in src[1:]:
        s = line.strip()
        if s.lower().startswith("title"):
            title = s[5:].strip()
            continue
        m = re.match(r'^"?(.+?)"?\s*:\s*([\d.]+)\s*$', s)
        if m:
            items.append((m.group(1), float(m.group(2))))
    if not items:
        return []
    total = sum(v for _, v in items) or 1
    labw = min(24, max(len(k) for k, _ in items))
    barw = max(10, min(40, max_width - labw - 16))
    hues = ["cy", "gr", "ye", "pu", "or", "pk"]
    out = [[(title, "ye")]] if title else []
    for i, (k, v) in enumerate(items):
        n = round(barw * v / total)
        out.append([(k[:labw].ljust(labw) + "  ", "fg"), ("█" * n, hues[i % len(hues)]),
                    ("░" * (barw - n), "rule"), (f"  {v:g}  {100 * v / total:.0f}%", "mu")])
    return out


def mermaid(block: list, max_width: int, force_direction: Optional[str] = None) -> tuple:
    """(lines, drawn) for a mermaid block. drawn is False when the type isn't supported."""
    src = [l for l in block if l.strip()]
    if not src:
        return [], False
    kind = src[0].strip().split()[0].lower()
    try:
        if kind in ("graph", "flowchart") or kind.startswith("statediagram"):
            if kind.startswith("statediagram"):
                src = [src[0]] + [re.sub(r"\[\*\]", "[*]", l) for l in src[1:]]
            lines = draw_flow(src, max_width, force_direction)
        elif kind == "sequencediagram":
            lines = draw_sequence(src, max_width)
        elif kind == "pie":
            lines = draw_pie(src, max_width)
        else:
            return [], False
    except Exception:
        return [], False
    return lines, bool(lines)


# --- code highlighting -------------------------------------------------------------

_KEYWORDS = set("""
and as assert async await break case catch class const continue def default del do elif else enum
except export extends false False finally for foreach from func function if import in interface is
lambda let match new None nil not null or param pass private protected public raise return self static
struct super switch this throw true True try type typeof using var void where while with yield
begin end select from where join on group by order insert update delete into values create table
Get Set New Remove Write-Host process
""".split())
_TOKEN = re.compile(
    r"(?P<comment>#.*$|//.*$|--\s.*$|/\*.*?\*/)"
    r"|(?P<string>\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*'|`[^`]*`)"
    r"|(?P<number>\b\d+(?:\.\d+)?\b)"
    r"|(?P<var>\$[A-Za-z_][\w:]*)"
    r"|(?P<deco>@[A-Za-z_]\w*)"
    r"|(?P<word>\b[A-Za-z_][\w-]*\b)")


def highlight(line: str, lang: str) -> list:
    """(text, colour) segments for one code line. Generic, forgiving, never raises."""
    if lang in ("text", "txt", "log", "output", ""):
        plain = lang in ("text", "txt", "log", "output")
        if plain:
            return [(line, None)]
    if len(line) > 2000:  # the token regex backtracks quadratically on long quote runs
        return highlight(line[:2000], lang) + [(line[2000:], None)]
    segs, pos = [], 0
    try:
        for m in _TOKEN.finditer(line):
            if m.start() > pos:
                segs.append((line[pos:m.start()], None))
            kind = m.lastgroup
            text = m.group()
            if kind == "comment":
                if text.startswith("#") and lang in ("c", "cpp", "cs", "csharp", "js", "ts", "javascript",
                                                     "typescript", "java", "go", "rust"):
                    segs.append((text[0], None))
                    rest = highlight(text[1:], lang)
                    segs.extend(rest)
                    pos = m.end()
                    continue
                segs.append((text, "fa"))
            elif kind == "string":
                segs.append((text, "ye"))
            elif kind == "number":
                segs.append((text, "pu"))
            elif kind in ("var", "deco"):
                segs.append((text, "or"))
            elif kind == "word" and (text in _KEYWORDS or text.lower() in ("select", "from", "where")):
                segs.append((text, "pk"))
            elif kind == "word" and line[m.end():m.end() + 1] == "(":
                segs.append((text, "gr"))
            else:
                segs.append((text, None))
            pos = m.end()
    except Exception:
        return [(line, None)]
    if pos < len(line):
        segs.append((line[pos:], None))
    return segs


# --- Markdown line structure (read by slackfmt) ----------------------------------

# Inline spans. Code first so a backtick span is never further marked up, bold before
# italic so **x** is never two *x*, images before links, explicit links before bare URLs.
_INLINE = re.compile(
    r"`(?P<code>[^`\n]+)`"
    r"|\*\*(?P<bold>[^*\n]+?)\*\*"
    r"|__(?P<bold2>[^_\n]+?)__"
    r"|~~(?P<strike>[^~\n]+?)~~"
    r"|\*(?P<ital>[^*\n]+)\*"
    r"|!\[(?P<ialt>[^\]\n]*)\]\((?P<iurl>[^)\s]+)\)"
    r"|\[(?P<ltxt>[^\]\n]+)\]\((?P<lurl>[^)\s]+)\)"
    r"|<(?P<aurl>https?://[^>\s]+)>"
    r"|(?P<burl>https?://[^\s)>\]]+)"
)
_TASK = re.compile(r"^\[( |x|X)\]\s+(.*)$")
_ADMONITION = re.compile(r"^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]\s*(.*)$", re.I)
_ADM_LOOK = {"NOTE": ("cy", "ⓘ NOTE"), "TIP": ("gr", "✓ TIP"), "IMPORTANT": ("pu", "★ IMPORTANT"),
             "WARNING": ("ye", "⚠ WARNING"), "CAUTION": ("pk", "✖ CAUTION")}
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
        if m.group("ialt") is not None:
            out.append("▣ " + (m.group("ialt") or "image"))
        else:
            for group in ("code", "bold", "bold2", "strike", "ital", "ltxt", "aurl", "burl"):
                if m.group(group) is not None:
                    out.append(m.group(group))
                    break
        pos = m.end()
    out.append(text[pos:])
    return "".join(out)


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
