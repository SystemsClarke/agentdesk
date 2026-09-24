"""Claude plan usage (5-hour and weekly windows), as last reported by the status-line feeder."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from agentdesk import paths

WEEK_WARN_PERCENT = 75
STALE_MINUTES = 30


def feed_path():
    return paths.DATA_DIR / "claude_usage.json"


def _parse_ts(value) -> Optional[datetime]:
    if value in (None, ""):
        return None
    try:
        if isinstance(value, (int, float)):
            return datetime.fromtimestamp(value / 1000 if value > 1e11 else value, timezone.utc)
        dt = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
        return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)
    except (ValueError, OSError, OverflowError):
        return None


def load() -> Optional[dict]:
    try:
        return json.loads(feed_path().read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def span(seconds: float) -> str:
    seconds = max(0, int(seconds))
    d, rem = divmod(seconds, 86400)
    h, rem = divmod(rem, 3600)
    m = rem // 60
    if d:
        return f"{d}d {h}h"
    if h:
        return f"{h}h {m:02d}m"
    return f"{m}m"


def lines(now: Optional[datetime] = None) -> list:
    """Phrases to rotate through on the main menu prompt; [] when nothing is known."""
    now = now or datetime.now(timezone.utc)
    data = load()
    if not data:
        return []
    captured = _parse_ts(data.get("captured_ts"))
    stale = ""
    if captured and (now - captured).total_seconds() > STALE_MINUTES * 60:
        stale = f", as of {span((now - captured).total_seconds())} ago"
    out = []
    five, week = data.get("five_hour") or {}, data.get("seven_day") or {}
    reset = _parse_ts(five.get("resets_at"))
    if reset and reset <= now:
        out.append("time left: fresh 5-hour window, the meter just reset")
    elif five:
        left = span((reset - now).total_seconds()) if reset else "unknown"
        out.append(f"time left: {left} in your 5-hour window, {five.get('used', '?')}% used{stale}")
    wreset = _parse_ts(week.get("resets_at"))
    try:
        week_used = float(week.get("used"))
    except (TypeError, ValueError):
        week_used = -1
    if week_used >= WEEK_WARN_PERCENT and not (wreset and wreset <= now):
        left = span((wreset - now).total_seconds()) if wreset else "unknown"
        out.append(f"time left: {left} on the week, {week.get('used')}% used, getting close{stale}")
    spend = _spend_text(data, now)
    if spend:
        out.append(spend)
    return out


def _spend_text(data: dict, now: datetime) -> str:
    x = data.get("extra") or {}
    try:
        spent, limit = float(x["spent"]), float(x["limit"])
    except (KeyError, TypeError, ValueError):
        return ""
    pct = round(100 * spent / limit) if limit else 0
    when = _parse_ts(x.get("captured_ts"))
    age = f", as of {span((now - when).total_seconds())} ago" if when and (now - when).total_seconds() > 3600 else ""
    return f"monthly spend: ${spent:,.2f} of ${limit:,.0f} ({pct}%){' · nearly capped' if pct >= 90 else ''}{age}"


_MONTHS = {m: i for i, m in enumerate(("Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"), 1)}


def _parse_reset(text: str, now: datetime) -> Optional[str]:
    """'Sep 26, 3:59am (America/New_York)' or 'Sep 26, 4am' -> ISO UTC, read as this machine's local time."""
    m = re.match(r"\s*(\w{3}) (\d{1,2}), (\d{1,2})(?::(\d{2}))?\s*([ap]m)", text or "")
    if not m or m.group(1) not in _MONTHS:
        return None
    hour = int(m.group(3)) % 12 + (12 if m.group(5) == "pm" else 0)
    local = datetime(now.year, _MONTHS[m.group(1)], int(m.group(2)), hour, int(m.group(4) or 0)).astimezone()
    if (local - now).days < -30:  # "Jan 2" read in late December
        local = local.replace(year=now.year + 1)
    return local.astimezone(timezone.utc).isoformat(timespec="seconds")


def refresh() -> bool:
    """Ask the claude CLI for plan usage (no hooks, no model call) and update the feed. True if updated."""
    exe = shutil.which("claude") or str(Path.home() / ".local" / "bin" / "claude.exe")
    try:
        out = subprocess.run([exe, "-p", "/usage", "--setting-sources", "project"], cwd=tempfile.gettempdir(),
                             capture_output=True, text=True, encoding="utf-8", errors="replace",
                             timeout=60, creationflags=0x08000000).stdout
    except (OSError, subprocess.SubprocessError):
        return False
    now = datetime.now(timezone.utc)
    windows = {}
    for key, label in (("five_hour", "Current session"), ("seven_day", "Current week (all models)")):
        m = re.search(re.escape(label) + r": (\d+)% used(?: · resets (.+))?", out)
        if m:
            windows[key] = {"used": int(m.group(1)), "resets_at": _parse_reset(m.group(2) or "", now)}
    if not windows:
        return False
    data = load() or {}
    data.update(windows, source="claude -p /usage", captured_ts=now.isoformat(timespec="seconds"))
    tmp = feed_path().with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
    os.replace(tmp, feed_path())
    return True


def summary(now: Optional[datetime] = None) -> str:
    now = now or datetime.now(timezone.utc)
    data = load()
    if not data:
        return "no feed yet (add scripts/claude_usage_feed.py to your status line)"
    five, week = data.get("five_hour") or {}, data.get("seven_day") or {}
    bits = []
    if five:
        r = _parse_ts(five.get("resets_at"))
        bits.append(f"5h {five.get('used', '?')}%" + (f", resets in {span((r - now).total_seconds())}" if r and r > now else ""))
    if week:
        r = _parse_ts(week.get("resets_at"))
        bits.append(f"week {week.get('used', '?')}%" + (f", resets in {span((r - now).total_seconds())}" if r and r > now else ""))
    spend = _spend_text(data, now)
    if spend:
        bits.append(spend.replace("monthly spend: ", "spend "))
    captured = _parse_ts(data.get("captured_ts"))
    if captured:
        bits.append(f"reported {span((now - captured).total_seconds())} ago")
    return " · ".join(bits) or "feed has no plan limits in it"
