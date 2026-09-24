"""Claude plan usage (5-hour and weekly windows), as last reported by the status-line feeder."""

from __future__ import annotations

import json
from datetime import datetime, timezone
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
    return out


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
    captured = _parse_ts(data.get("captured_ts"))
    if captured:
        bits.append(f"reported {span((now - captured).total_seconds())} ago")
    return " · ".join(bits) or "feed has no plan limits in it"
