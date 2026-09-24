"""Deterministic charts and tables, drawn as images from a small JSON spec.

Agents write the spec in a ```chart fenced block. Same spec, same pixels: fixed fonts,
sizes and palette, no randomness, no time-of-day. The terminal shows the image inline;
the Slack bridge uploads the same PNG, so the phone sees exactly what the window does.

Types: line, bar, progress, burndown, timeline, sparkline, stat, table.
See CHART_HELP for the spec of each.
"""

from __future__ import annotations

import io
import json
import math
from pathlib import Path
from typing import Optional

from PIL import Image, ImageDraw, ImageFont

FONTS = Path(r"C:\Windows\Fonts")
SS = 2  # drawn at 2x and downsampled, so lines and text are smooth

THEMES = {
    "dark": dict(bg="#2d2a2e", panel="#221f22", grid="#403e41", axis="#5b595c", fg="#fcfcfa",
                 mu="#939293", good="#a9dc76", bad="#ff6188", goal="#ffd866",
                 series=["#78dce8", "#a9dc76", "#ffd866", "#ab9df2", "#fc9867", "#ff6188"]),
    "light": dict(bg="#faf4f2", panel="#ede7e5", grid="#e0dad9", axis="#bfb9ba", fg="#29242a",
                  mu="#706b6e", good="#269d69", bad="#e14775", goal="#cc7a0a",
                  series=["#1c8ca8", "#269d69", "#cc7a0a", "#7058be", "#e16032", "#e14775"]),
}

CHART_HELP = """Charts: put JSON in a ```chart block. Fields common to all: "type", "title", optional "unit".
  line      {"type":"line","title":"Compile time","unit":"min","x":["09-01","09-08"],"series":{"JAWS":[41,29]},"goal":25}
  bar       {"type":"bar","title":"Builds per lane","labels":["compile","sign"],"values":[41,12]}
  progress  {"type":"progress","title":"Migration","items":[{"label":"groups","done":7,"total":20}]}
  burndown  {"type":"burndown","title":"Sprint 2609","x":["Mon","Tue","Wed"],"remaining":[30,24,19]}
  timeline  {"type":"timeline","title":"Cutover","events":[{"when":"09-01","label":"canary"}]}
  sparkline {"type":"sparkline","title":"queue depth","values":[3,5,2,8,4]}
  stat      {"type":"stat","tiles":[{"label":"green builds","value":"94%","delta":"+6%"}]}
  table     {"type":"table","columns":["run","result"],"rows":[["#45","pass"],["#46","fail"]]}"""


class ChartError(ValueError):
    pass


def _font(bold: bool, size: int, mono: bool = False):
    name = "CascadiaMono.ttf" if mono else ("segoeuib.ttf" if bold else "segoeui.ttf")
    try:
        return ImageFont.truetype(str(FONTS / name), size * SS)
    except OSError:
        return ImageFont.load_default()


class _Pen:
    """Logical-pixel drawing on a supersampled image."""

    def __init__(self, w: int, h: int, t: dict) -> None:
        self.w, self.h, self.t = w, h, t
        self.img = Image.new("RGB", (w * SS, h * SS), t["bg"])
        self.d = ImageDraw.Draw(self.img)

    def line(self, pts, color, width=1.0, dash: Optional[int] = None):
        pts = [(x * SS, y * SS) for x, y in pts]
        if not dash:
            self.d.line(pts, fill=color, width=max(1, round(width * SS)), joint="curve")
            return
        for (x1, y1), (x2, y2) in zip(pts, pts[1:]):
            length = math.hypot(x2 - x1, y2 - y1)
            n = max(1, int(length / (dash * SS * 2)))
            for k in range(n):
                a, b = k / n, (k + 0.5) / n
                self.d.line([(x1 + (x2 - x1) * a, y1 + (y2 - y1) * a), (x1 + (x2 - x1) * b, y1 + (y2 - y1) * b)],
                            fill=color, width=max(1, round(width * SS)))

    def rect(self, x1, y1, x2, y2, fill=None, outline=None, radius=0):
        box = [x1 * SS, y1 * SS, x2 * SS, y2 * SS]
        if radius:
            self.d.rounded_rectangle(box, radius=radius * SS, fill=fill, outline=outline)
        else:
            self.d.rectangle(box, fill=fill, outline=outline)

    def dot(self, x, y, r, color):
        self.d.ellipse([(x - r) * SS, (y - r) * SS, (x + r) * SS, (y + r) * SS], fill=color)

    def text(self, x, y, s, color, size=11, bold=False, anchor="la", mono=False):
        self.d.text((x * SS, y * SS), str(s), fill=color, font=_font(bold, size, mono), anchor=anchor)

    def width_of(self, s, size=11, bold=False, mono=False) -> float:
        return self.d.textlength(str(s), font=_font(bold, size, mono)) / SS

    def done(self) -> Image.Image:
        k = SS / 2  # the display scale this render was asked for
        return self.img.resize((round(self.w * k), round(self.h * k)), Image.LANCZOS)


def _nice(lo: float, hi: float, ticks: int = 5) -> list:
    if hi == lo:
        hi = lo + 1
    raw = (hi - lo) / ticks
    mag = 10 ** math.floor(math.log10(raw))
    step = min((m * mag for m in (1, 2, 2.5, 5, 10) if m * mag >= raw), default=mag * 10)
    start = math.floor(lo / step) * step
    out, v = [], start
    while v <= hi + step * 0.5:
        out.append(round(v, 10))
        v += step
    return out


def _fmt(v: float, unit: str = "") -> str:
    s = f"{v:,.0f}" if abs(v) >= 100 or float(v).is_integer() else f"{v:,.1f}"
    return s + (unit if unit in ("%",) else "")


def _title(p: _Pen, spec: dict, y: int = 14) -> int:
    t = p.t
    if spec.get("title"):
        p.text(16, y, spec["title"], t["fg"], 15, bold=True)
        y += 24
    if spec.get("subtitle") or spec.get("unit"):
        p.text(16, y, spec.get("subtitle") or f"in {spec['unit']}", t["mu"], 10)
        y += 18
    return y


def _legend(p: _Pen, names: list, y: int) -> int:
    x = 16
    for i, name in enumerate(names):
        c = p.t["series"][i % len(p.t["series"])]
        p.rect(x, y + 3, x + 10, y + 13, fill=c, radius=2)
        p.text(x + 15, y, name, p.t["fg"], 11)
        x += 15 + p.width_of(name) + 18
    return y + 22


def _axes(p: _Pen, left, top, right, bottom, ticks, lo, hi, unit):
    t = p.t
    for v in ticks:
        y = bottom - (v - lo) / (hi - lo) * (bottom - top)
        p.line([(left, y), (right, y)], t["grid"], 1)
        p.text(left - 8, y, _fmt(v, unit), t["mu"], 10, anchor="rm", mono=True)
    p.line([(left, bottom), (right, bottom)], t["axis"], 1.2)


def _xlabels(p: _Pen, labels: list, xs: list, bottom: float):
    if not labels:
        return
    room = (xs[-1] - xs[0]) if len(xs) > 1 else 100
    every = max(1, math.ceil(len(labels) * 64 / max(room, 1)))
    for i, (lab, x) in enumerate(zip(labels, xs)):
        if i % every == 0 or i == len(labels) - 1:
            p.text(x, bottom + 8, lab, p.t["mu"], 10, anchor="ma")


def _line(spec: dict, t: dict) -> Image.Image:
    xl = [str(v) for v in spec.get("x") or []]
    series = spec.get("series") or {}
    if not xl or not series:
        raise ChartError('line needs "x" (labels) and "series" ({"name": [values]})')
    w, h = int(spec.get("width", 640)), int(spec.get("height", 300))
    p = _Pen(w, h, t)
    y = _title(p, spec)
    if len(series) > 1:
        y = _legend(p, list(series), y)
    vals = [v for s in series.values() for v in s if v is not None]
    goal = spec.get("goal")
    lo = min(vals + ([goal] if goal is not None else []))
    hi = max(vals + ([goal] if goal is not None else []))
    lo = min(lo, 0) if spec.get("zero", True) and lo >= 0 else lo
    ticks = _nice(lo, hi)
    lo, hi = ticks[0], ticks[-1]
    unit = spec.get("unit", "")
    left = 16 + max(p.width_of(_fmt(v, unit), 10, mono=True) for v in ticks) + 12
    top, right, bottom = y + 16, w - 64, h - 34
    _axes(p, left, top, right, bottom, ticks, lo, hi, unit)
    n = len(xl)
    xs = [left + (right - left) * (i / (n - 1) if n > 1 else 0.5) for i in range(n)]
    ymap = lambda v: bottom - (v - lo) / (hi - lo) * (bottom - top)
    if goal is not None:
        gy = ymap(goal)
        p.line([(left, gy), (right, gy)], t["goal"], 1.2, dash=4)
        p.text(right + 6, gy, f"goal {_fmt(goal, unit)}", t["goal"], 10, anchor="lm")
    for i, (name, data) in enumerate(series.items()):
        c = t["series"][i % len(t["series"])]
        pts = [(xs[k], ymap(v)) for k, v in enumerate(data[:n]) if v is not None]
        if len(pts) > 1:
            p.line(pts, c, 2.4)
        for x, yv in pts:
            p.dot(x, yv, 2.2, c)
        if pts:
            last = [v for v in data[:n] if v is not None][-1]
            p.dot(pts[-1][0], pts[-1][1], 4, c)
            p.text(pts[-1][0] + 8, pts[-1][1], _fmt(last, unit), c, 11, bold=True, anchor="lm", mono=True)
    _xlabels(p, xl, xs, bottom)
    return p.done()


def _bar(spec: dict, t: dict) -> Image.Image:
    labels = [str(v) for v in spec.get("labels") or []]
    values = spec.get("values")
    if not labels or values is None:
        raise ChartError('bar needs "labels" and "values"')
    unit = spec.get("unit", "")
    horizontal = spec.get("horizontal", len(labels) > 8 or max(len(l) for l in labels) > 10)
    if horizontal:
        w = int(spec.get("width", 640))
        h = 60 + 30 * len(labels) + (24 if spec.get("title") else 0)
        p = _Pen(w, h, t)
        y = _title(p, spec) + 6
        labw = max(p.width_of(l, 11) for l in labels) + 24
        vmax = max(values) or 1
        for i, (lab, v) in enumerate(zip(labels, values)):
            c = t["series"][i % len(t["series"])] if spec.get("colorful") else t["series"][0]
            p.text(16 + labw - 12, y + 10, lab, t["fg"], 11, anchor="rm")
            bw = (w - labw - 90) * v / vmax
            p.rect(16 + labw, y + 2, 16 + labw + max(bw, 1), y + 20, fill=c, radius=3)
            p.text(16 + labw + bw + 8, y + 11, _fmt(v, unit), t["fg"], 11, bold=True, anchor="lm", mono=True)
            y += 30
        return p.done()
    w, h = int(spec.get("width", 640)), int(spec.get("height", 280))
    p = _Pen(w, h, t)
    y = _title(p, spec)
    ticks = _nice(0, max(values) or 1)
    left = 16 + max(p.width_of(_fmt(v, unit), 10, mono=True) for v in ticks) + 12
    top, right, bottom = y + 30, w - 16, h - 34
    _axes(p, left, top, right, bottom, ticks, ticks[0], ticks[-1], unit)
    slot = (right - left) / len(labels)
    bw = min(56, slot * 0.6)
    xs = []
    for i, (lab, v) in enumerate(zip(labels, values)):
        cx = left + slot * (i + 0.5)
        xs.append(cx)
        yv = bottom - v / ticks[-1] * (bottom - top)
        c = t["series"][i % len(t["series"])] if spec.get("colorful") else t["series"][0]
        p.rect(cx - bw / 2, yv, cx + bw / 2, bottom, fill=c, radius=3)
        p.text(cx, yv - 6, _fmt(v, unit), t["fg"], 11, bold=True, anchor="md", mono=True)
    _xlabels(p, labels, xs, bottom)
    return p.done()


def _progress(spec: dict, t: dict) -> Image.Image:
    items = spec.get("items") or []
    if not items:
        raise ChartError('progress needs "items": [{"label", "done", "total"}]')
    w = int(spec.get("width", 640))
    h = 40 + 44 * len(items) + (24 if spec.get("title") else 0)
    p = _Pen(w, h, t)
    y = _title(p, spec) + 4
    for it in items:
        total = float(it.get("total", 100) or 100)
        done = float(it.get("done", it.get("value", 0)))
        frac = max(0.0, min(1.0, done / total))
        c = t["good"] if frac >= 1 else t["series"][0]
        p.text(16, y, it.get("label", ""), t["fg"], 12, bold=True)
        right_txt = f"{_fmt(done)}/{_fmt(total)}  {frac * 100:.0f}%" if "total" in it else f"{frac * 100:.0f}%"
        p.text(w - 16, y, right_txt, t["mu"], 11, anchor="ra", mono=True)
        p.rect(16, y + 20, w - 16, y + 30, fill=t["panel"], radius=5)
        if frac > 0:
            p.rect(16, y + 20, 16 + (w - 32) * frac, y + 30, fill=c, radius=5)
        y += 44
    return p.done()


def _burndown(spec: dict, t: dict) -> Image.Image:
    xl = [str(v) for v in spec.get("x") or []]
    rem = spec.get("remaining") or []
    if not xl or not rem:
        raise ChartError('burndown needs "x" and "remaining"')
    total = float(spec.get("total", rem[0]))
    n = len(xl)
    ideal = [total - total * i / (n - 1) if n > 1 else 0 for i in range(n)]
    s = dict(spec, type="line", series={"remaining": rem, "ideal": [round(v, 1) for v in ideal]}, zero=True)
    img = _line(s, dict(t, series=[t["bad"] if rem[-1] > ideal[len(rem) - 1] else t["good"], t["axis"]] + t["series"]))
    return img


def _timeline(spec: dict, t: dict) -> Image.Image:
    events = spec.get("events") or []
    if not events:
        raise ChartError('timeline needs "events": [{"when", "label"}]')
    w, h = int(spec.get("width", 640)), int(spec.get("height", 190))
    p = _Pen(w, h, t)
    y = _title(p, spec)
    axis_y = y + (h - y) / 2
    left, right = 40, w - 40
    p.line([(left, axis_y), (right, axis_y)], t["axis"], 2)
    n = len(events)
    for i, ev in enumerate(events):
        x = left + (right - left) * (i / (n - 1) if n > 1 else 0.5)
        c = t["good"] if ev.get("done") else (t["goal"] if ev.get("now") else t["series"][i % len(t["series"])])
        p.dot(x, axis_y, 6 if ev.get("now") else 5, c)
        up = i % 2 == 0
        p.line([(x, axis_y), (x, axis_y + (-18 if up else 18))], t["axis"], 1)
        p.text(x, axis_y + (-22 if up else 22), ev.get("label", ""), t["fg"], 11, bold=bool(ev.get("now")),
               anchor="md" if up else "ma")
        p.text(x, axis_y + (-38 if up else 38), ev.get("when", ""), t["mu"], 10, anchor="md" if up else "ma", mono=True)
    return p.done()


def _sparkline(spec: dict, t: dict) -> Image.Image:
    vals = [v for v in spec.get("values") or [] if v is not None]
    if len(vals) < 2:
        raise ChartError('sparkline needs at least two "values"')
    label = spec.get("title", "")
    w, h = int(spec.get("width", 260)), 44
    p = _Pen(w, h, t)
    lw = p.width_of(label, 11, bold=True) + 12 if label else 0
    last = _fmt(vals[-1], spec.get("unit", ""))
    rw = p.width_of(last, 12, bold=True, mono=True) + 12
    p.text(8, h / 2, label, t["fg"], 11, bold=True, anchor="lm")
    lo, hi = min(vals), max(vals)
    span = (hi - lo) or 1
    x0, x1 = 8 + lw, w - rw - 4
    pts = [(x0 + (x1 - x0) * i / (len(vals) - 1), h - 10 - (v - lo) / span * (h - 20)) for i, v in enumerate(vals)]
    c = t["good"] if vals[-1] >= vals[0] else t["bad"]
    if spec.get("lower_is_better"):
        c = t["good"] if vals[-1] <= vals[0] else t["bad"]
    p.line(pts, c, 2)
    p.dot(*pts[-1], 3.5, c)
    p.text(w - 8, h / 2, last, c, 12, bold=True, anchor="rm", mono=True)
    return p.done()


def _stat(spec: dict, t: dict) -> Image.Image:
    tiles = spec.get("tiles") or []
    if not tiles:
        raise ChartError('stat needs "tiles": [{"label", "value", "delta"}]')
    per = min(4, len(tiles))
    rows = math.ceil(len(tiles) / per)
    w = int(spec.get("width", 640))
    tw = (w - 16 - 12 * (per - 1) - 16) / per
    h = 16 + rows * 96 + (rows - 1) * 12 + (30 if spec.get("title") else 0) + 8
    p = _Pen(w, h, t)
    y0 = _title(p, spec)
    for i, tile in enumerate(tiles):
        r, cidx = divmod(i, per)
        x, y = 16 + cidx * (tw + 12), y0 + r * 108
        p.rect(x, y, x + tw, y + 90, fill=t["panel"], radius=8)
        p.text(x + 14, y + 12, tile.get("label", ""), t["mu"], 11)
        p.text(x + 14, y + 32, tile.get("value", ""), t["fg"], 24, bold=True)
        delta = str(tile.get("delta", ""))
        if delta:
            good = delta.startswith("+") != bool(tile.get("lower_is_better"))
            p.text(x + 14, y + 68, delta, t["good"] if good else t["bad"], 11, bold=True, mono=True)
    return p.done()


def table_image(columns: list, rows: list, t: dict, aligns: Optional[list] = None, title: str = "",
                max_width: int = 900) -> Image.Image:
    """A table as a grid image: header band, zebra rows, numbers right-aligned, long cells wrapped."""
    ncols = max([len(columns)] + [len(r) for r in rows]) if (columns or rows) else 1
    cols = [str(c) for c in columns] + [""] * (ncols - len(columns))
    body = [[str(c) for c in r] + [""] * (ncols - len(r)) for r in rows]
    probe = _Pen(10, 10, t)
    pad = 12

    def is_num(s):
        return bool(s) and s.replace(",", "").replace(".", "").replace("%", "").replace("-", "").replace("+", "").strip().isdigit()

    natural = [max([probe.width_of(cols[j], 12, bold=True)] + [probe.width_of(r[j], 12) for r in body]) + 2 * pad
               for j in range(ncols)]
    avail = max_width - 32
    widths = list(natural)
    if sum(widths) > avail:
        floor = 70
        room = sum(max(0, w - floor) for w in widths)
        slack = sum(widths) - avail
        widths = [w - (max(0, w - floor) / room * slack if room else 0) for w in widths]

    def wrap(s, width, bold=False):
        words, lines, cur = s.split(), [], ""
        for word in words:
            cand = f"{cur} {word}" if cur else word
            if probe.width_of(cand, 12, bold) + 2 * pad <= width or not cur:
                cur = cand
            else:
                lines.append(cur)
                cur = word
        return lines + [cur] if cur else (lines or [""])

    head = [wrap(c, widths[j], True) for j, c in enumerate(cols)]
    cells = [[wrap(c, widths[j]) for j, c in enumerate(r)] for r in body]
    line_h = 18
    head_h = max(len(c) for c in head) * line_h + 14
    row_hs = [max(len(c) for c in r) * line_h + 12 for r in cells]
    w = int(sum(widths)) + 32
    top = 44 if title else 16
    h = top + head_h + sum(row_hs) + 16
    p = _Pen(w, h, t)
    if title:
        p.text(16, 14, title, t["fg"], 15, bold=True)
    x0, y = 16, top
    p.rect(x0, y, x0 + sum(widths), y + head_h, fill=t["panel"], radius=6)
    aligns = aligns or []
    numeric = [all(is_num(r[j]) or not r[j] for r in body) and any(r[j] for r in body) for j in range(ncols)]

    def draw_row(lines_per_cell, y, bold, color):
        x = x0
        for j, lines in enumerate(lines_per_cell):
            a = aligns[j] if j < len(aligns) else ("r" if numeric[j] and not bold else "l")
            for k, s in enumerate(lines):
                ty = y + 7 + k * line_h
                if a == "r":
                    p.text(x + widths[j] - pad, ty, s, color, 12, bold, anchor="ra", mono=numeric[j] and not bold)
                elif a == "c":
                    p.text(x + widths[j] / 2, ty, s, color, 12, bold, anchor="ma")
                else:
                    p.text(x + pad, ty, s, color, 12, bold)
            x += widths[j]

    draw_row(head, y, True, t["fg"])
    y += head_h
    for i, (r, rh) in enumerate(zip(cells, row_hs)):
        if i % 2:
            p.rect(x0, y, x0 + sum(widths), y + rh, fill=t["panel"])
        draw_row(r, y, False, t["fg"])
        y += rh
    p.line([(x0, y), (x0 + sum(widths), y)], t["grid"], 1)
    return p.done()


def _table(spec: dict, t: dict) -> Image.Image:
    return table_image(spec.get("columns") or [], spec.get("rows") or [], t, spec.get("align"),
                       spec.get("title", ""), int(spec.get("width", 900)))


_TYPES = {"line": _line, "area": _line, "bar": _bar, "progress": _progress, "burndown": _burndown,
          "timeline": _timeline, "sparkline": _sparkline, "stat": _stat, "table": _table}


def parse(text: str) -> dict:
    try:
        spec = json.loads(text)
    except ValueError as exc:
        raise ChartError(f"chart spec is not valid JSON: {exc}") from None
    if not isinstance(spec, dict) or spec.get("type") not in _TYPES:
        raise ChartError(f'chart "type" must be one of: {", ".join(_TYPES)}')
    return spec


def theme_from_palette(pal: dict) -> dict:
    """Chart colours from a terminal palette, so charts match whichever theme is on."""
    return dict(bg=pal["bg"], panel=pal["panel"], grid=pal["line"], axis=pal["rule"], fg=pal["fg"],
                mu=pal["mu"], good=pal["gr"], bad=pal["pk"], goal=pal["ye"],
                series=[pal["cy"], pal["gr"], pal["ye"], pal["pu"], pal["or"], pal["pk"]])


def render(spec: dict, theme="dark", scale: float = 1.0) -> Image.Image:
    """`theme` is "dark", "light" or a colour dict; `scale` is the display's DPI scale (1.5 at 150%)."""
    global SS
    SS = max(2, round(2 * scale))
    t = theme if isinstance(theme, dict) else THEMES.get(theme, THEMES["dark"])
    try:
        return _TYPES[spec["type"]](spec, t)
    finally:
        SS = 2


def table(columns: list, rows: list, theme="dark", aligns=None, max_width: int = 900,
          scale: float = 1.0) -> Image.Image:
    return render({"type": "table", "columns": columns, "rows": rows, "align": aligns, "width": max_width},
                  theme, scale)


def png(img: Image.Image) -> bytes:
    buf = io.BytesIO()
    img.save(buf, "PNG", optimize=True)
    return buf.getvalue()
