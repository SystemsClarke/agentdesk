"""The core: one `agentdesk daemon` per user owns the sessions; `agentdesk core` and the window start, ask and stop it."""
import json
import time

from lib import feature, wait_for


def status(b):
    r = b.run("core", "status")
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout)


@feature("core.single-owner", "core", "A second `agentdesk daemon` does not start: one core per data folder, and `core status` names it", "docs/ARCHITECTURE.md")
def _single(b):
    p = b.spawn("daemon")
    wait_for(lambda: status(b).get("running"), 15, what="the core to claim its lock")
    first = status(b)["pid"]
    r = b.run("daemon", timeout=30)
    assert r.returncode == 0 and "already running" in r.stdout, (r.stdout, r.stderr)
    assert status(b)["pid"] == first and p.poll() is None


@feature("core.start-stop", "core", "`core start` runs a detached core, `core stop` ends it cleanly and `core status` says running false", "docs/ARCHITECTURE.md")
def _startstop(b):
    assert status(b) == {"running": False}
    s = json.loads(b.run("core", "start", timeout=60).stdout)
    assert s["running"] is True and s["pid"]
    assert json.loads(b.run("core", "start", timeout=60).stdout)["pid"] == s["pid"], "start is idempotent"
    b.run("core", "stop", timeout=60)
    assert status(b) == {"running": False}


@feature("core.restart", "core", "`core restart` replaces the core: a new pid, the sessions the board wants come back", "docs/ARCHITECTURE.md")
def _restart(b):
    s = json.loads(b.run("core", "start", timeout=60).stdout)
    r = json.loads(b.run("core", "restart", timeout=90).stdout)
    assert r["running"] is True and r["pid"] != s["pid"]
    b.run("core", "stop", timeout=60)
