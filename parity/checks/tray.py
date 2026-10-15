"""The tray: `agentdesk tray --self-test` builds the menu model and the icons for sample states and prints them, with no window and no
display, so every OS can verify it headlessly."""
import json

from lib import Missing, feature


def self_test(b, **env):
    r = b.run("tray", "--self-test", env=env or None)
    if r.returncode == 2 and ("unrecognized subcommand" in r.stderr or "unexpected argument" in r.stderr):
        raise Missing("`agentdesk tray` is not a command here")
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout)


@feature("tray.surface", "tray", "The tray icon: core status, open questions, open the window, Concierge toggle, stop the core, quit (menu model checked headlessly)", "Tray.cs")
def _tray(b):
    doc = self_test(b)
    assert doc["ok"] is True
    by = {s["sample"]: s for s in doc["states"]}
    assert [by[k]["icon"] for k in ("idle", "attention", "down")] == ["idle", "attention", "down"]
    assert len({by[k]["icon_fingerprint"] for k in by}) == 3, "the three icons differ"
    size = doc["icon_size"]
    assert all(s["icon_bytes"] == size * size * 4 for s in by.values())
    menu = by["attention"]["menu"]
    assert "Open AgentDesk" in menu and "Stop the core" in menu and "Quit tray" in menu
    assert any(m.startswith("#7 ") for m in menu), "open questions are listed"
    assert "2 open questions" in by["attention"]["status"] and "Concierge on" in by["attention"]["status"]
    assert "Core not running" in by["down"]["status"]


@feature("tray.terminal-override", "tray", "AGENTDESK_TERMINAL picks the terminal the tray opens the window in", "Tray.cs")
def _override(b):
    doc = self_test(b, AGENTDESK_TERMINAL="myterm --flag")
    assert doc["terminal"] == ["myterm", "--flag", "agentdesk", "tui"], doc["terminal"]
