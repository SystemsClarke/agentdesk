"""The window's own preferences, in LOCALAPPDATA next to the database."""

from __future__ import annotations

import json
import os

from agentdesk import paths

DEFAULTS = {
    "theme": "monokai-pro",
    "screech": True,
    "font_size": 11,
    "preroll": True,
    "max_sessions": 3,     # agent sessions that may run at once (sessions.py enforces it everywhere)
    "provider": "claude",  # backend for agent sessions: a profile in ~/.claude/providers.json
}


def _path():
    # Resolved per call: check scripts repoint paths.DATA_DIR at a scratch folder.
    return paths.DATA_DIR / "settings.json"


def load() -> dict:
    data = dict(DEFAULTS)
    try:
        stored = json.loads(_path().read_text(encoding="utf-8"))
        if isinstance(stored, dict):
            data.update({k: v for k, v in stored.items() if k in DEFAULTS})
    except (OSError, ValueError):
        pass
    return data


def save(data: dict) -> None:
    try:
        paths.ensure_dirs()
        tmp = _path().with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
        os.replace(tmp, _path())  # a crash mid-write must not reset every preference
    except OSError:
        pass
