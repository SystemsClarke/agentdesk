"""Markdown <-> Slack. Deterministic, no model involved.

md_to_slack turns a board message (Markdown) into Slack Block Kit: headers, dividers and
mrkdwn sections, with tables as aligned monospace grids and ```mermaid drawn by the same
engine as the terminal. slack_to_md turns a reply typed in Slack back into Markdown, which
is what every agent on the board reads and writes.
"""

from __future__ import annotations

import re

from agentdesk import mdrich, mdview

SECTION_MAX = 2900   # Slack's hard limit is 3000 characters per section
HEADER_MAX = 150
BLOCKS_MAX = 50


def _esc(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def _inline(text: str) -> str:
    out, pos = [], 0
    for m in mdview._INLINE.finditer(text):
        out.append(_esc(text[pos:m.start()]))
        g = m.groupdict()
        if g["code"] is not None:
            out.append(f"`{g['code']}`")
        elif g["bold"] is not None or g["bold2"] is not None:
            out.append(f"*{_esc(g['bold'] or g['bold2'])}*")
        elif g["strike"] is not None:
            out.append(f"~{_esc(g['strike'])}~")
        elif g["ital"] is not None:
            out.append(f"_{_esc(g['ital'])}_")
        elif g["ialt"] is not None:
            out.append(f"<{g['iurl']}|▣ {_esc(g['ialt'] or 'image')}>")
        elif g["ltxt"] is not None:
            out.append(f"<{g['lurl']}|{_esc(g['ltxt'])}>")
        else:
            out.append(f"<{g['aurl'] or g['burl']}>")
        pos = m.end()
    out.append(_esc(text[pos:]))
    return "".join(out)


def _grid(header: list, delim: list, rows: list) -> str:
    ncols = max([len(header), len(delim)] + [len(r) for r in rows])
    cells = lambda r: [mdview._plain(r[j]) if j < len(r) else "" for j in range(ncols)]
    head, body = cells(header), [cells(r) for r in rows]
    widths = [max(1, max(len(c[j]) for c in [head] + body)) for j in range(ncols)]
    aligns = mdview._aligns(delim, ncols)

    def row(cs):
        parts = []
        for j, c in enumerate(cs):
            gap = widths[j] - len(c)
            left = gap if aligns[j] == "r" else gap // 2 if aligns[j] == "c" else 0
            parts.append(" " * left + c + " " * (gap - left))
        return "│ " + " │ ".join(parts) + " │"

    rule = lambda l, m, r: l + m.join("─" * (w + 2) for w in widths) + r
    lines = [rule("┌", "┬", "┐"), row(head), rule("├", "┼", "┤")] + [row(r) for r in body] + [rule("└", "┴", "┘")]
    return "```\n" + "\n".join(lines) + "\n```"


def _fence(lang: str, block: list) -> str:
    if lang == "mermaid":
        drawn, ok = mdrich.mermaid(block, 90)
        if ok:
            return "```\n" + "\n".join("".join(t for t, _ in segs) for segs in drawn) + "\n```"
    return "```\n" + "\n".join(block) + "\n```"


def _pieces(md: str) -> list:
    """("text", mrkdwn) / ("header", plain) / ("divider", "") in order."""
    out, lines, i = [], md.split("\n"), 0

    def text(s):
        out.append(("text", s))

    while i < len(lines):
        line, stripped = lines[i], lines[i].strip()
        if stripped.startswith("```"):
            lang = stripped[3:].strip().lower()
            block = []
            i += 1
            while i < len(lines) and not lines[i].strip().startswith("```"):
                block.append(lines[i])
                i += 1
            i += 1
            text(_fence(lang, block))
            continue
        table = mdview._table_at(lines, i)
        if table is not None:
            header, delim, rows, i = table
            text(_grid(header, delim, rows))
            continue
        if mdview._RULE.match(line):
            out.append(("divider", ""))
            i += 1
            continue
        m = mdview._HEADING.match(line)
        if m:
            if len(m.group(1)) == 1:
                out.append(("header", mdview._plain(m.group(2))[:HEADER_MAX]))
            else:
                text(f"*{_inline(m.group(2))}*")
            i += 1
            continue
        if mdview._QUOTE.match(line):
            quote = []
            while i < len(lines) and mdview._QUOTE.match(lines[i]):
                quote.append(mdview._QUOTE.match(lines[i]).group(1))
                i += 1
            adm = mdview._ADMONITION.match(quote[0].strip()) if quote else None
            if adm:
                title = mdview._ADM_LOOK[adm.group(1).upper()][1]
                quote = [f"*{title}*"] + ([adm.group(2)] if adm.group(2) else []) + quote[1:]
                text("\n".join("> " + (q if q.startswith("*") and q.endswith("*") else _inline(q)) for q in quote))
            else:
                text("\n".join("> " + _inline(q) for q in quote))
            continue
        m = mdview._BULLET.match(line)
        if m:
            level = min(3, len(m.group(1).expandtabs(4)) // 2)
            task = mdview._TASK.match(m.group(2))
            if task:
                mark = "☑" if task.group(1).lower() == "x" else "☐"
                text("    " * level + f"{mark} {_inline(task.group(2))}")
            else:
                text("    " * level + "•◦▪·"[level] + " " + _inline(m.group(2)))
            i += 1
            continue
        m = mdview._NUMBERED.match(line)
        if m:
            level = min(3, len(m.group(1).expandtabs(4)) // 2)
            text("    " * level + f"{m.group(2)}. {_inline(m.group(3))}")
            i += 1
            continue
        text(_inline(line))
        i += 1
    return out


def _chunks(s: str) -> list:
    """Split text into section-sized pieces, on line boundaries, never inside a ``` block."""
    if len(s) <= SECTION_MAX:
        return [s]
    out, cur, fenced = [], "", False
    for line in s.split("\n"):
        extra = ("\n" if cur else "") + line
        if len(cur) + len(extra) > SECTION_MAX and cur:
            out.append(cur + ("\n```" if fenced else ""))
            cur = ("```\n" if fenced else "") + line
        else:
            cur += extra
        if line.strip().startswith("```"):
            fenced = not fenced
    if cur:
        out.append(cur)
    return out


def md_to_slack(md: str) -> list:
    """Block Kit blocks for a Markdown body."""
    blocks, buf = [], []

    def flush():
        if buf:
            joined = "\n".join(buf).strip("\n")
            if joined.strip():
                for c in _chunks(joined):
                    blocks.append({"type": "section", "text": {"type": "mrkdwn", "text": c}})
            buf.clear()

    for kind, value in _pieces(md or ""):
        if kind == "text":
            buf.append(value)
        else:
            flush()
            if kind == "header":
                blocks.append({"type": "header", "text": {"type": "plain_text", "text": value or " ", "emoji": True}})
            else:
                blocks.append({"type": "divider"})
    flush()
    if len(blocks) > BLOCKS_MAX:
        blocks = blocks[: BLOCKS_MAX - 1] + [{"type": "context", "elements": [
            {"type": "mrkdwn", "text": "_(message truncated for Slack; the full text is on the board)_"}]}]
    return blocks


def fallback_text(md: str, limit: int = 300) -> str:
    """Plain text for the notification preview and for clients that can't show blocks."""
    keep, fenced = [], False
    for l in (md or "").splitlines():
        if l.strip().startswith("```"):
            fenced = not fenced
            continue
        if l.strip() and not fenced and not mdview._is_delim(l):
            keep.append(l)
    plain = " ".join(re.sub(r"\[!\w+\]\s*", "", mdview._plain(l.strip().lstrip("#>-*+ ").replace("|", " ")))
                     for l in keep)
    plain = re.sub(r"\s{2,}", " ", plain).strip()
    return plain[:limit] + ("…" if len(plain) > limit else "")


_SLACK_LINK = re.compile(r"<(https?://[^|>]+)\|([^>]+)>")
_SLACK_URL = re.compile(r"<(https?://[^|>]+)>")
_SLACK_MENTION = re.compile(r"<[@#!]([^|>]+)(?:\|([^>]+))?>")


def slack_to_md(text: str) -> str:
    """A reply typed in Slack, as Markdown."""
    out = []
    fenced = False
    for line in (text or "").split("\n"):
        if line.strip().startswith("```"):
            fenced = not fenced
            out.append(line)
            continue
        if fenced:
            out.append(line.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&"))
            continue
        line = _SLACK_LINK.sub(lambda m: f"[{m.group(2)}]({m.group(1)})", line)
        line = _SLACK_URL.sub(lambda m: m.group(1), line)
        line = _SLACK_MENTION.sub(lambda m: "@" + (m.group(2) or m.group(1)), line)
        segs = re.split(r"(`[^`]*`)", line)
        for k, seg in enumerate(segs):
            if seg.startswith("`"):
                continue
            seg = re.sub(r"(?<![\w*])\*(?!\s)([^*\n]+?)(?<!\s)\*(?![\w*])", r"**\1**", seg)
            seg = re.sub(r"(?<![\w_])_(?!\s)([^_\n]+?)(?<!\s)_(?![\w_])", r"*\1*", seg)
            seg = re.sub(r"(?<![\w~])~(?!\s)([^~\n]+?)(?<!\s)~(?![\w~])", r"~~\1~~", seg)
            seg = re.sub(r"^(\s*)•\s", r"\1- ", seg)
            segs[k] = seg.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
        out.append("".join(segs))
    return "\n".join(out)
