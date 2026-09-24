"""Terminal palettes read from the dark themes installed in VS Code.

Each theme's workbench and terminal ANSI colours are mapped onto the terminal view's
palette keys; a theme without ANSI colours gets VS Code's default terminal palette. Themes installed later appear
automatically. Nothing is required: without VS Code the list is just empty.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path

ROOTS = [Path(os.path.expandvars(r"%USERPROFILE%\.vscode\extensions")),
         Path(os.path.expandvars(r"%LOCALAPPDATA%\Programs\Microsoft VS Code\resources\app\extensions"))]


def _load_json(path: Path) -> dict:
    raw = path.read_text(encoding="utf-8", errors="replace")
    raw = re.sub(r'("(?:\\.|[^"\\])*")|//[^\n]*|/\*.*?\*/', lambda m: m.group(1) or "", raw, flags=re.S)
    raw = re.sub(r",(\s*[}\]])", r"\1", raw)
    return json.loads(raw)


def _theme(path: Path, depth: int = 0) -> dict:
    """colors + tokenColors, following VS Code's "include" chain."""
    data = _load_json(path)
    colors, tokens = {}, []
    if data.get("include") and depth < 4:
        base = _theme((path.parent / data["include"]).resolve(), depth + 1)
        colors, tokens = dict(base["colors"]), list(base["tokenColors"])
    colors.update(data.get("colors") or {})
    tc = data.get("tokenColors")
    if isinstance(tc, list):
        tokens += tc
    return {"colors": colors, "tokenColors": tokens}


def _rgb(h: str):
    h = h.lstrip("#")
    if len(h) in (3, 4):
        h = "".join(c * 2 for c in h)
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4)), (int(h[6:8], 16) / 255 if len(h) == 8 else 1.0)


def _hex(rgb) -> str:
    return "#" + "".join(f"{max(0, min(255, round(c))):02x}" for c in rgb)


def _blend(fg: str, bg: str, a: float) -> str:
    (f, _), (b, _) = _rgb(fg), _rgb(bg)
    return _hex(b[i] + (f[i] - b[i]) * a for i in range(3))


def _solid(color: str, bg: str) -> str:
    rgb, a = _rgb(color)
    return _blend(_hex(rgb), bg, a) if a < 1 else _hex(rgb)


def _lum(h: str) -> float:
    (r, g, b), _ = _rgb(h)
    return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255


def palette(label: str, data: dict) -> dict:
    c = data["colors"]
    bg = _solid(c.get("editor.background") or "#1e1e1e", "#000000")
    fg = _solid(c.get("editor.foreground") or "#d4d4d4", bg)
    get = lambda *keys, default=None: next((_solid(c[k], bg) for k in keys if c.get(k)), default)
    # Terminal ANSI colours carry the meaning here (red = needs you, green = done), so a theme
    # without them gets VS Code's own default terminal palette rather than its syntax colours.
    accent = lambda ansi, default: _solid(c.get(ansi) or default, bg)
    pal = {
        "label": label, "dark": _lum(bg) < 0.5,
        "bg": bg, "fg": fg,
        "panel": get("sideBar.background", "titleBar.activeBackground", default=_blend("#000000", bg, 0.25)),
        "line": get("editorWidget.background", "input.background", default=_blend(fg, bg, 0.15)),
        "mu": get("descriptionForeground", default=_blend(fg, bg, 0.62)),
        "fa": get("editorLineNumber.foreground", default=_blend(fg, bg, 0.45)),
        "rule": get("editorIndentGuide.background", "editorIndentGuide.background1", default=_blend(fg, bg, 0.25)),
        "pk": accent("terminal.ansiRed", "#f14c4c"),
        "ye": accent("terminal.ansiYellow", "#e5e510"),
        "gr": accent("terminal.ansiGreen", "#23d18b"),
        "cy": accent("terminal.ansiCyan", "#29b8db"),
        "pu": accent("terminal.ansiMagenta", "#d670d6"),
        "on_bar": bg,
    }
    pal["or"] = _solid(c.get("terminal.ansiBrightRed") or _blend(pal["ye"], pal["pk"], 0.5), bg)
    pal["textsel"] = get("editor.selectionBackground", default=pal["line"])
    # A panel identical to the background makes the title and command bars vanish.
    if abs(_lum(pal["panel"]) - _lum(bg)) < 0.02:
        pal["panel"] = _blend(fg, bg, 0.08)
    for k in ("pk", "or", "ye", "gr", "cy", "pu", "mu"):
        if abs(_lum(pal[k]) - _lum(bg)) < 0.18:
            pal[k] = _blend(pal[k], fg, 0.5)
    return pal


def discover(dark_only: bool = True) -> dict:
    """key -> palette for every installed VS Code theme (dark ones by default)."""
    found = {}
    for root in ROOTS:
        if not root.is_dir():
            continue
        for pkg in sorted(root.glob("*/package.json")):
            try:
                themes = (_load_json(pkg).get("contributes") or {}).get("themes") or []
            except Exception:
                continue
            for t in themes:
                if dark_only and t.get("uiTheme") not in ("vs-dark", "hc-black"):
                    continue
                label = t.get("label") or t.get("id") or ""
                try:
                    pal = palette(label, _theme((pkg.parent / t["path"]).resolve()))
                except Exception:
                    continue
                found[f"vscode:{label}"] = pal
    return dict(sorted(found.items(), key=lambda kv: kv[1]["label"].lower()))


def cached() -> dict:
    """discover(), but read from a cache in LOCALAPPDATA unless an extension changed (~2 s saved)."""
    from agentdesk import paths
    cache = paths.DATA_DIR / "vscode_themes.json"
    sig = []
    for root in ROOTS:
        if root.is_dir():
            sig += [f"{p}:{p.stat().st_mtime_ns}" for p in sorted(root.glob("*/package.json"))]
    try:
        data = json.loads(cache.read_text(encoding="utf-8"))
        if data.get("sig") == sig:
            return data["themes"]
    except (OSError, ValueError, KeyError):
        pass
    themes = discover()
    try:
        paths.ensure_dirs()
        cache.write_text(json.dumps({"sig": sig, "themes": themes}), encoding="utf-8")
    except OSError:
        pass
    return themes
