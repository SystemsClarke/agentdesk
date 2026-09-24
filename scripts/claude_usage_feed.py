"""Claude Code status-line command: records plan usage for AgentDesk, then prints the prompt.

Claude Code pipes a JSON snapshot of the session to the status-line command on every
render. The rate-limit windows in it are normalized into LOCALAPPDATA/AgentDesk/
claude_usage.json, which the AgentDesk window reads for its "time left" line.
"""

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

DATA_DIR = Path(os.environ.get("LOCALAPPDATA", Path.home())) / "AgentDesk"


def _find(obj, names):
    if isinstance(obj, dict):
        for k, v in obj.items():
            if isinstance(v, dict) and any(n in k.lower() for n in names):
                return v
        for v in obj.values():
            hit = _find(v, names)
            if hit is not None:
                return hit
    elif isinstance(obj, list):
        for v in obj:
            hit = _find(v, names)
            if hit is not None:
                return hit
    return None


def _window(d):
    if not isinstance(d, dict):
        return None
    used = next((d[k] for k in ("used_percentage", "utilization", "percentUsed", "percent_used", "used")
                 if k in d), None)
    if isinstance(used, (int, float)) and 0 < used <= 1 and "utilization" in d:
        used = round(used * 100)
    reset = next((d[k] for k in ("resets_at", "resetsAt", "reset_at") if k in d), None)
    if used is None and reset is None:
        return None
    return {"used": round(used) if isinstance(used, (int, float)) else used, "resets_at": reset}


def main():
    raw = sys.stdin.read()
    try:
        data = json.loads(raw) if raw.strip() else {}
    except ValueError:
        data = {}
    five = _window(_find(data, ("five_hour", "5h", "five-hour")))
    week = _window(_find(data, ("seven_day", "weekly", "7d", "seven-day")))
    try:
        DATA_DIR.mkdir(parents=True, exist_ok=True)
        (DATA_DIR / "statusline_keys.json").write_text(
            json.dumps(sorted(data.keys()) if isinstance(data, dict) else []), encoding="utf-8")
        if five or week:
            try:  # merge, so the last-known monthly spend ("extra") survives
                out = json.loads((DATA_DIR / "claude_usage.json").read_text(encoding="utf-8"))
            except (OSError, ValueError):
                out = {}
            out.update({"source": "claude code status line",
                        "captured_ts": datetime.now(timezone.utc).isoformat(timespec="seconds"),
                        "five_hour": five, "seven_day": week})
            tmp = DATA_DIR / "claude_usage.json.tmp"
            tmp.write_text(json.dumps(out, indent=2), encoding="utf-8")
            os.replace(tmp, DATA_DIR / "claude_usage.json")
    except OSError:
        pass
    cwd = (data.get("workspace") or {}).get("current_dir") or data.get("cwd") or ""
    sys.stdout.write(f"PS {cwd}>")


if __name__ == "__main__":
    main()
