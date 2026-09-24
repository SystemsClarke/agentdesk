"""The AgentDesk icon: three agents in speech bubbles, eyeing each other over a shared spark.

One geometry, three outputs -- the SVG in assets/, the PIL image the tray and window
use at runtime, and the .ico the packaged exe is stamped with -- so they cannot drift.
"""

from __future__ import annotations

import math
from pathlib import Path

from PIL import Image, ImageDraw

BG = "#221f22"
RIM = "#403e41"
EYE = "#fcfcfa"
PUPIL = "#19181a"
SPARK = "#fcfcfa"
CENTER = (128.0, 134.0)
OFFSET = 42.0
RADIUS = 60.0
ALPHA = 0.85
BUBBLES = ((-90, "#78dce8"), (30, "#ffd866"), (150, "#ff6188"))


def _geometry() -> dict:
    bubbles, eyes = [], []
    for ang, color in BUBBLES:
        a = math.radians(ang)
        cx, cy = CENTER[0] + math.cos(a) * OFFSET, CENTER[1] + math.sin(a) * OFFSET

        def at(deg: float, dist: float, cx=cx, cy=cy, a=a) -> tuple:
            t = a + math.radians(deg)
            return (cx + math.cos(t) * dist, cy + math.sin(t) * dist)

        bubbles.append({"color": color, "c": (cx, cy),
                        "tail": [at(24, RADIUS * 0.82), at(-6, RADIUS * 0.82), at(16, RADIUS + 30)]})
        perp = (-math.sin(a), math.cos(a))
        for side in (-1, 1):
            ex = cx + math.cos(a) * 22 + perp[0] * 14 * side
            ey = cy + math.sin(a) * 22 + perp[1] * 14 * side
            vx, vy = CENTER[0] - ex, CENTER[1] - ey
            n = math.hypot(vx, vy) or 1.0
            eyes.append({"c": (ex, ey), "p": (ex + vx / n * 4.5, ey + vy / n * 4.5)})
    spark = []
    for i in range(8):
        r = 19 if i % 2 == 0 else 5
        t = math.radians(i * 45 - 90)
        spark.append((CENTER[0] + math.cos(t) * r, CENTER[1] + math.sin(t) * r))
    return {"bubbles": bubbles, "eyes": eyes, "spark": spark}


def svg() -> str:
    g = _geometry()
    pts = lambda ps: " ".join(f"{x:.1f},{y:.1f}" for x, y in ps)
    out = ['<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="256" height="256">',
           '<title>AgentDesk</title>',
           f'<rect x="8" y="8" width="240" height="240" rx="56" fill="{BG}" stroke="{RIM}" stroke-width="4"/>']
    for b in g["bubbles"]:
        cx, cy = b["c"]
        out.append(f'<g fill="{b["color"]}" fill-opacity="{ALPHA}">'
                   f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="{RADIUS:.0f}"/>'
                   f'<polygon points="{pts(b["tail"])}"/></g>')
    for e in g["eyes"]:
        (ex, ey), (px, py) = e["c"], e["p"]
        out.append(f'<circle cx="{ex:.1f}" cy="{ey:.1f}" r="11" fill="{EYE}"/>'
                   f'<circle cx="{px:.1f}" cy="{py:.1f}" r="5.5" fill="{PUPIL}"/>')
    out.append(f'<polygon points="{pts(g["spark"])}" fill="{SPARK}"/>')
    out.append("</svg>")
    return "\n".join(out)


def _rgba(hex_color: str, alpha: float = 1.0) -> tuple:
    h = hex_color.lstrip("#")
    return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16), round(255 * alpha))


def render(size: int) -> Image.Image:
    """The icon at `size` px, drawn at 4x and downsampled so the curves are smooth."""
    big = max(size, 16) * 4
    s = big / 256.0
    sc = lambda p: (p[0] * s, p[1] * s)
    g = _geometry()
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    ImageDraw.Draw(img).rounded_rectangle(
        (8 * s, 8 * s, 248 * s, 248 * s), radius=56 * s,
        fill=_rgba(BG), outline=_rgba(RIM), width=max(1, round(4 * s)))
    for b in g["bubbles"]:
        layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
        d = ImageDraw.Draw(layer)
        fill = _rgba(b["color"], ALPHA)
        cx, cy = sc(b["c"])
        r = RADIUS * s
        d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=fill)
        d.polygon([sc(p) for p in b["tail"]], fill=fill)
        img.alpha_composite(layer)
    d = ImageDraw.Draw(img)
    for e in g["eyes"]:
        (ex, ey), (px, py) = sc(e["c"]), sc(e["p"])
        d.ellipse((ex - 11 * s, ey - 11 * s, ex + 11 * s, ey + 11 * s), fill=_rgba(EYE))
        d.ellipse((px - 5.5 * s, py - 5.5 * s, px + 5.5 * s, py + 5.5 * s), fill=_rgba(PUPIL))
    d.polygon([sc(p) for p in g["spark"]], fill=_rgba(SPARK))
    return img.resize((size, size), Image.LANCZOS)


def photo_images(root) -> list:
    from PIL import ImageTk
    return [ImageTk.PhotoImage(render(n), master=root) for n in (16, 32, 48, 256)]


def write_assets(folder: Path) -> None:
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "agentdesk.svg").write_text(svg(), encoding="utf-8")
    render(256).save(folder / "agentdesk.png")
    render(256).save(folder / "agentdesk.ico",
                     sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])


if __name__ == "__main__":
    write_assets(Path(__file__).resolve().parent / "assets")
