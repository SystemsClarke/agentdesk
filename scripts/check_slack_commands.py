"""Checks for the Slack phone commands (agentdesk/slackcmd.py): parsing, John-only authorisation,
per-bot area routing, bots.json parsing and its no-config fallback, and the pipe client against a
fake core on a private pipe. No Slack, no real core.

    .venv\\Scripts\\python.exe scripts\\check_slack_commands.py
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
import threading
import uuid
from multiprocessing.connection import Listener
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import slackcmd  # noqa: E402

JOHN, OTHER = "U_JOHN", "U_OTHER"
FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if not ok:
        FAILURES.append(label)
    print(f"  [{'ok' if ok else 'FAIL'}] {label}" + (f" -- {detail}" if detail else ""))


class FakeCore:
    """Stands in for slackcmd.call: records requests, answers from a table."""

    def __init__(self, **answers):
        self.calls, self.answers = [], answers

    def __call__(self, tool, args=None):
        self.calls.append((tool, args or {}))
        return self.answers.get(tool, {"error": f"unknown request: {tool}"})


ROW = {"name": "scout", "state": "running", "folder": r"C:\work", "generation": 2}
core = FakeCore(**{"ui:identity_list": {"identities": [ROW]}, "ui:identity_create": ROW, "ui:identity_start": ROW,
                   "ui:identity_stop": {**ROW, "state": "stopped"}, "ui:identity_forget": {"forgotten": "scout"},
                   "ui:status": {"slack": None, "worker": {"running": False}, "crew": {"max": 3}, "usage": {"summary": "12% used"}},
                   "ui:worker": {"ok": True, "started": True}, "list_threads": {"threads": [{}, {}]}})
every = slackcmd.Commands(list(slackcmd.AREAS), JOHN, "AgentDesk", call=core)

print("authorisation")
err = io.StringIO()
with contextlib.redirect_stderr(err):
    out = every.handle("agents", OTHER)
check("a non-John user gets nothing and runs nothing", out is None and not core.calls)
check("the refusal is logged with the Slack user id", "user=U_OTHER" in err.getvalue() and "REFUSED" in err.getvalue())
with contextlib.redirect_stderr(err):
    out = every.handle("agents", JOHN)
check("John's command is logged", "user=U_JOHN" in err.getvalue() and "ran" in err.getvalue())

print("commands")
with contextlib.redirect_stderr(io.StringIO()):
    check("agents lists state, generation and folder", "scout" in out and "running" in out and "gen 2" in out and r"C:\work" in out, out)
    core.calls.clear()
    out = every.handle('agent new scout "C:\\my work" Watch the &lt;build&gt; logs', JOHN)
    check("agent new creates with folder and charter, then starts",
          core.calls[0] == ("ui:identity_create", {"name": "scout", "folder": r"C:\my work", "charter": "Watch the <build> logs"})
          and core.calls[1] == ("ui:identity_start", {"name": "scout"}), str(core.calls))
    check("agent stop", "stopped" in every.handle("agent stop scout", JOHN))
    core.calls.clear()
    out = every.handle("agent forget scout", JOHN)
    check("agent forget asks first and forgets nothing", "yes" in out and not core.calls, out)
    out = every.handle("yes", JOHN)
    check("yes confirms the forget", core.calls == [("ui:identity_forget", {"name": "scout"})] and "Forgot" in out, out)
    check("a second yes has nothing to confirm", every.handle("yes", JOHN) == "Nothing to confirm.")
    check("worker shows its status", every.handle("worker", JOHN) == "Worker is off.")
    core.calls.clear()
    check("worker off when already off changes nothing", every.handle("worker off", JOHN) == "Worker is off."
          and ("ui:worker", {}) not in core.calls)
    check("worker on starts it", every.handle("worker on", JOHN) == "Worker starting.")
    out = every.handle("status", JOHN)
    check("status has slack, worker, agents/cap, questions, usage",
          all(s in out for s in ("Slack* down", "Worker* off", "1 running / cap 3", "Open questions* 2", "12% used")), out)
    check("update without ui:update says so", "isn't available yet" in every.handle("update", JOHN))
    core.answers["ui:update"] = {"current": "0.1.40", "latest": "0.1.42"}
    out = every.handle("update apply", JOHN)
    check("update apply sends apply and shows versions", core.calls[-1] == ("ui:update", {"apply": True}) and "0.1.42" in out, out)
    check("not a command: None, so the board relay answers", every.handle("thanks!", JOHN) is None)

print("area routing")
with contextlib.redirect_stderr(io.StringIO()):
    workerbot = slackcmd.Commands(["worker"], JOHN, "Worker", call=core)
    check("a worker-only bot ignores agents and status", workerbot.handle("agents", JOHN) is None and workerbot.handle("status", JOHN) is None)
    check("a worker-only bot answers worker", workerbot.handle("worker", JOHN) is not None)
    h = workerbot.handle("help", JOHN)
    check("help lists only that bot's commands", "`worker`" in h and "agents" not in h and "`status`" not in h and "thread" not in h, h)
    check("the full bot's help includes the board relay", "thread" in every.handle("help", JOHN))
    check("help is John-only too", workerbot.handle("help", OTHER) is None)

print("bots.json")
tmp = Path(tempfile.mkdtemp(prefix="agentdesk-slackcmd-"))
bots = slackcmd.load_bots(tmp / "bots.json", tmp, "D_DEFAULT")
check("no bots.json: one bot, every area, the original credentials",
      len(bots) == 1 and bots[0]["areas"] == list(slackcmd.AREAS) and bots[0]["creds"] == tmp and bots[0]["dm"] == "D_DEFAULT")
(tmp / "bots.json").write_text(json.dumps([{"name": "AgentDesk", "areas": ["board", "ops"]},
                                           {"name": "Agents", "areas": ["agents", "worker"], "creds": "agents-bot"}]))
bots = slackcmd.load_bots(tmp / "bots.json", tmp, "D_DEFAULT")
check("two bots parsed; creds relative to the credentials folder",
      [b["name"] for b in bots] == ["AgentDesk", "Agents"] and bots[0]["creds"] == tmp and bots[1]["creds"] == tmp / "agents-bot")
for bad, why in (([{"name": "x", "areas": ["nope"]}], "unknown area"),
                 ([{"name": "a", "areas": ["board"]}, {"name": "b", "areas": ["board"]}], "two board bots")):
    (tmp / "bots.json").write_text(json.dumps(bad))
    try:
        slackcmd.load_bots(tmp / "bots.json", tmp, "D")
        check(f"{why} is rejected", False)
    except ValueError:
        check(f"{why} is rejected", True)

print("pipe client")
name = f"agentdesk-check-{uuid.uuid4().hex[:8]}"
listener = Listener("\\\\.\\pipe\\" + name, family="AF_PIPE")
seen = []


def serve():
    conn = listener.accept()
    req = json.loads(conn.recv_bytes())
    seen.append(req)
    conn.send_bytes(b'{"id":0,"text":"{\\"event\\":\\"board.changed\\"}"}\n')  # a push first: must be skipped
    conn.send_bytes((json.dumps({"id": req["id"], "text": json.dumps({"identities": []})}) + "\n").encode())
    conn.close()


threading.Thread(target=serve, daemon=True).start()
os.environ["AGENTDESK_PIPE"] = name
got = slackcmd.call("ui:identity_list")
listener.close()
check("request carries id, tool, args and a camelCase caller",
      seen and seen[0]["tool"] == "ui:identity_list" and seen[0]["args"] == {} and seen[0]["caller"]["harness"] == "slack", str(seen))
check("the id-0 push is skipped and the reply parsed", got == {"identities": []}, str(got))

print(f"\n{len(FAILURES)} failure(s)" if FAILURES else "\nall passed")
sys.exit(1 if FAILURES else 0)
