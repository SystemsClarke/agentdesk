"""Phone commands for the Slack bridge: John types `agents`, `worker off`, `status`... in a bot's DM.

Each command belongs to an area (agents, worker, ops, swarms); the bridge can run several Slack bots from
bots.json, each answering only its own areas plus `help`. The `board` area is the question relay and
`wake`, which live in scripts/slack_bridge.py. Commands reach the core over its pipe (docs/ui-api.md)
and only John's Slack user may run them.
"""

from __future__ import annotations

import html
import itertools
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

AREAS = ("board", "agents", "worker", "ops", "swarms")
_ids = itertools.count(1)
_pipe: str | None = None


def pipe_path() -> str:
    """\\\\.\\pipe\\agentdesk-<SID>, or AGENTDESK_PIPE's name, as PipeNames.Board has it."""
    global _pipe
    if name := os.environ.get("AGENTDESK_PIPE"):
        return name if name.startswith("\\\\") else "\\\\.\\pipe\\" + name
    if _pipe is None:
        out = subprocess.run(["whoami", "/user", "/fo", "csv", "/nh"], capture_output=True, text=True).stdout
        _pipe = "\\\\.\\pipe\\agentdesk-" + out.strip().split(",")[-1].strip('"')
    return _pipe


def call(tool: str, args: dict | None = None) -> dict:
    """One request to the core; skips id-0 pushes and returns the parsed reply."""
    rid = next(_ids)
    req = {"id": rid, "tool": tool, "args": args or {},
           "caller": {"sessionId": None, "envAuthor": None, "cwd": os.getcwd(), "harness": "slack", "pid": os.getpid()}}
    for attempt in range(3):  # every pipe instance busy for a moment: try again
        try:
            f = open(pipe_path(), "r+b", buffering=0)
            break
        except OSError:
            if attempt == 2:
                raise
            time.sleep(0.3)
    with f:
        f.write((json.dumps(req) + "\n").encode())
        buf = b""
        while True:
            chunk = f.read(65536)
            if not chunk:
                raise OSError("the core closed the pipe")
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                resp = json.loads(line)
                if resp.get("id") == rid:
                    return json.loads(resp["text"])


def load_bots(config: Path, default_creds: Path, default_dm: str) -> list[dict]:
    """bots.json: [{"name", "areas", "creds": <folder with bot-token.txt and app-token.txt>, "dm"?}].
    No file: one bot with every area and the original credentials, as before bots.json existed."""
    if not config.exists():
        return [{"name": "AgentDesk", "areas": list(AREAS), "creds": default_creds, "dm": default_dm}]
    bots = json.loads(config.read_text(encoding="utf-8"))
    for b in bots:
        if bad := set(b["areas"]) - set(AREAS):
            raise ValueError(f"bot {b['name']}: unknown areas {sorted(bad)}")
        b["creds"] = default_creds / b.get("creds", ".")  # relative to the slack-notify folder; absolute stays
        b.setdefault("dm", default_dm)
    if sum("board" in b["areas"] for b in bots) > 1:
        raise ValueError("only one bot can have the board area (it owns bridge_state.json)")
    return bots


def _ago(iso: str | None) -> float:
    try:
        return (datetime.now(timezone.utc) - datetime.fromisoformat(str(iso).replace("Z", "+00:00"))).total_seconds()
    except ValueError:
        return 1e9


def _err(r: dict) -> str | None:
    return f"⚠ {r['error']}" if isinstance(r, dict) and r.get("error") else None


def _row(r: dict) -> str:
    return f"*{r['name']}* · {r['state']}{' · gen ' + str(r['generation']) if r.get('generation') is not None else ''} · `{r['folder']}`"


def agents(cmd: "Commands", text: str) -> str:
    rows = cmd.call("ui:identity_list").get("identities", [])
    return "\n".join(_row(r) for r in rows) or "No agents yet. `agent new <name> <folder> [charter]`"


def agent(cmd: "Commands", text: str) -> str:
    if m := re.match(r"agent\s+new\s+(\S+)\s+(\"[^\"]+\"|\S+)\s*(.*)", text, re.I | re.S):
        name, folder, charter = m[1], m[2].strip('"'), m[3].strip() or None
        made = cmd.call("ui:identity_create", {"name": name, "folder": folder, "charter": charter})
        return _err(made) or _err(r := cmd.call("ui:identity_start", {"name": name})) or "Created " + _row(r)
    m = re.match(r"agent\s+(start|stop|forget)\s+(\S+)\s*$", text, re.I)
    if not m:
        return "Usage: `agent new <name> <folder> [charter]` · `agent start|stop|forget <name>`"
    verb, name = m[1].lower(), m[2]
    if verb == "forget":
        cmd.pending = (name, time.time())
        return f"Reply `yes` to forget *{name}* (its Claude conversation is kept)."
    r = cmd.call(f"ui:identity_{verb}", {"name": name})
    return _err(r) or _row(r)


def yes(cmd: "Commands", text: str) -> str:
    name, when = cmd.pending or ("", 0)
    cmd.pending = None
    if not name or time.time() - when > 300:
        return "Nothing to confirm."
    return _err(r := cmd.call("ui:identity_forget", {"name": name})) or f"Forgot *{r.get('forgotten', name)}*."


def worker(cmd: "Commands", text: str) -> str:
    arg = text.split()[1].lower() if len(text.split()) > 1 else ""
    w = cmd.call("ui:status").get("worker") or {}
    if arg in ("on", "off") and (arg == "on") != bool(w.get("running")):
        r = cmd.call("ui:worker")
        return _err(r) or ("Worker starting." if r.get("started") else
                           "Worker will stop after its current item." if r.get("stop_requested") else "Worker already starting.")
    held = f", holding #{w['held']}" if w.get("held") else ""
    return f"Worker is {'on' if w.get('running') else 'off'}{held}."


def status(cmd: "Commands", text: str) -> str:
    s = cmd.call("ui:status")
    if e := _err(s):
        return e
    ids = cmd.call("ui:identity_list").get("identities", [])
    qs = cmd.call("list_threads", {"channel": "question", "status": "open", "limit": 200})
    slack, w, crew = s.get("slack") or {}, s.get("worker") or {}, s.get("crew") or {}
    count = lambda st: sum(r.get("state") == st for r in ids)
    governor = (s.get("governor") or {}).get("summary", "")  # advisory; an older core has none
    return "\n".join([
        f"*Slack* {'up' if _ago(slack.get('ts')) < 90 else 'down'}",
        f"*Worker* {'on' if w.get('running') else 'off'}" + (f", holding #{w['held']}" if w.get("held") else ""),
        f"*Agents* {count('running')} running / cap {crew.get('max', '?')}, {count('queued')} queued",
        f"*Open questions* {len(qs.get('threads', []))}",
        f"*Usage* {(s.get('usage') or {}).get('summary', '?')}"]
        + ([f"*Governor* {governor.removeprefix('governor: ')}"] if governor else []))


def update(cmd: "Commands", text: str) -> str:
    r = cmd.call("ui:update", {"apply": text.lower().split()[1:] == ["apply"]})
    if "unknown request" in str(r.get("error", "")):
        return "Update isn't available yet (this core has no `ui:update`)."
    return _err(r) or "\n".join(f"*{k}* {v}" for k, v in r.items() if not isinstance(v, (dict, list)))


NO_SWARMS = "Swarm slots need the Slack bridge's client (this bot has none)."
SWARM_USAGE = ("Usage: `swarm new <name> <folder> <objective>` · `swarm approve <slot> [members=N hours=H cadence=M]` · "
               "`swarm end <slot>` · `swarm reset <slot>`")
_OPTS = {"members": ("max_members", int), "hours": ("max_hours", float), "cadence": ("cadence_minutes", float)}


def _guarded(fn, *args) -> str:
    """A swarm call; a Slack refusal mid-command becomes the reply. OSError (no core) is Commands.handle's to say."""
    try:
        return fn(*args)
    except OSError:
        raise
    except Exception as exc:
        return f"⚠ {type(exc).__name__}: {exc}"


def swarms(cmd: "Commands", text: str) -> str:
    s = getattr(cmd, "swarm", None)  # agentdesk/swarm.Swarms, set by the bridge on the bot with the swarms area
    return _guarded(s.list) if s else NO_SWARMS


def swarm(cmd: "Commands", text: str) -> str:
    s = getattr(cmd, "swarm", None)
    if s is None:
        return NO_SWARMS
    if m := re.match(r"swarm\s+new\s+(\S+)\s+(\"[^\"]+\"|\S+)\s+(\S.*)", text, re.I | re.S):
        return _guarded(s.new, m[1], m[2].strip('"'), m[3].strip())
    m = re.match(r"swarm\s+(approve|end|reset)\s+(\d+)((?:\s+\w+=[\d.]+)*)\s*$", text, re.I)
    if not m:
        return SWARM_USAGE
    verb, n, opts = m[1].lower(), int(m[2]), {}
    for k, v in (o.split("=") for o in m[3].split()):
        if verb != "approve" or k.lower() not in _OPTS:
            return SWARM_USAGE
        key, conv = _OPTS[k.lower()]
        try:
            opts[key] = conv(v)
        except ValueError:
            return SWARM_USAGE
    return _guarded(s.approve, n, opts) if verb == "approve" else _guarded(getattr(s, verb), n)


COMMANDS = {  # area -> command word -> (handler, help line)
    "agents": {"agents": (agents, "`agents` list them"),
               "agent": (agent, "`agent new <name> <folder> [charter]` · `agent start|stop|forget <name>`"),
               "yes": (yes, None)},
    "worker": {"worker": (worker, "`worker` status · `worker on|off`")},
    "ops": {"status": (status, "`status` core, worker, agents, questions, usage"),
            "update": (update, "`update` versions · `update apply` update and restart")},
    "board": {},
    "swarms": {"swarms": (swarms, "`swarms` the 10 swarm slots"),
               "swarm": (swarm, "`swarm new <name> <folder> <objective>` · `swarm approve|end|reset <slot>`")},
}
BOARD_HELP = "Reply *in a question's thread* to answer it; `wake` there resumes the agent."


class Commands:
    """One bot's commands: only its areas, only from John."""

    def __init__(self, areas: list, john: str, bot: str = "AgentDesk", call=call):
        self.areas, self.john, self.bot, self.call, self.pending = areas, john, bot, call, None
        self.table = {w: h for a in areas for w, h in COMMANDS.get(a, {}).items()}

    def help(self) -> str:
        lines = [h for _, h in self.table.values() if h] + ([BOARD_HELP] if "board" in self.areas else [])
        return f"*{self.bot}* commands\n" + "\n".join("• " + h for h in lines + ["`help` this list"])

    def handle(self, text: str, user: str | None) -> str | None:
        """The reply to post, or None when this is not one of this bot's commands (or not John's)."""
        text = html.unescape(text or "").strip()
        word = text.split()[0].lower() if text else ""
        if word != "help" and word not in self.table:
            return None
        print(f"[slack_cmd] bot={self.bot} user={user} {'ran' if user == self.john else 'REFUSED'}: {text[:200]!r}",
              file=sys.stderr, flush=True)
        if user != self.john:
            return None  # silently: never run anything for anyone else
        if word == "help":
            return self.help()
        try:
            return self.table[word][0](self, text)
        except OSError as exc:
            return f"⚠ Can't reach the core ({exc})."
