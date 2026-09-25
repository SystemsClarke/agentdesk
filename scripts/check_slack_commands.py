"""Checks for the Slack phone commands (agentdesk/slackcmd.py): parsing, John-only authorisation,
per-bot area routing, bots.json parsing and its no-config fallback, the swarm slots (agentdesk/swarm.py)
against a fake Slack client and a fake core, and the pipe client against a fake core on a private pipe.
No Slack, no real core.

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
from datetime import date
from multiprocessing.connection import Listener
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import slackcmd, swarm  # noqa: E402

JOHN, OTHER = "U_JOHN", "U_OTHER"
FAILURES: list[str] = []
sys.stdout.reconfigure(errors="replace")  # details carry ⚠ and ·; a cp1252 console must not crash the run


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
                   "ui:status": {"slack": None, "worker": {"running": False}, "crew": {"max": 3}, "usage": {"summary": "12% used"},
                                  "governor": {"summary": "governor: 20% spendable of 88% left"}},
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
          all(s in out for s in ("Slack* down", "Worker* off", "1 running / cap 3", "Open questions* 2", "12% used", "Governor* 20% spendable")), out)
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

print("swarm slots")


class SlackErr(Exception):
    """What slack_sdk raises: SlackApiError, with Slack's error code in .response["error"]."""

    def __init__(self, code):
        super().__init__(code)
        self.response = {"ok": False, "error": code}


class FakeSlack:
    """The WebClient methods swarm.py uses. Posts with a persona fail with missing_scope unless `customize`."""

    def __init__(self, customize=True):
        self.customize, self.calls, self.posts, self.channels, self.fail = customize, [], [], {}, {}
        self._ids, self._ts = iter(range(1, 999)), iter(range(1, 999))

    def _rec(self, name, kw):
        self.calls.append((name, kw))
        if name in self.fail:
            raise SlackErr(self.fail.pop(name))

    def conversations_create(self, **kw):
        self._rec("create", kw)
        cid = f"C{next(self._ids)}"
        self.channels[cid] = {"id": cid, "name": kw["name"], "archived": False}
        return {"channel": dict(self.channels[cid])}

    def conversations_rename(self, **kw):
        self._rec("rename", kw)
        ch = self.channels.get(kw["channel"])
        if ch is None or ch["archived"]:
            raise SlackErr("channel_not_found" if ch is None else "is_archived")
        ch["name"] = kw["name"]
        return {"channel": dict(ch)}

    def conversations_invite(self, **kw):
        self._rec("invite", kw)
        return {"ok": True}

    def conversations_archive(self, **kw):
        self._rec("archive", kw)
        self.channels[kw["channel"]]["archived"] = True
        return {"ok": True}

    def chat_postMessage(self, **kw):
        self._rec("post", kw)
        if "username" in kw and not self.customize:
            raise SlackErr("missing_scope")
        ts = f"{next(self._ts)}.000"
        self.posts.append({**kw, "ts": ts})
        return {"ts": ts}


class SwarmCore:
    """The core's pipe as swarm.py sees it: slots, goals with board threads, identities and ui:input."""

    def __init__(self):
        self.calls, self.inputs, self.mid, self.threads = [], [], 100, {}
        self.slots = {n: {"n": n, "channel_id": None, "channel_name": None, "persona_name": None, "persona_icon": None, "goal": None}
                      for n in range(1, 11)}
        self.goals, self.unreachable = {}, set()

    def say(self, goal, author, body, kind="agent", meta=None):
        self.mid += 1
        self.threads[self.goals[goal]["thread_id"]].append(
            {"id": self.mid, "author": author, "author_kind": kind, "body": body, "meta": json.dumps(meta) if meta else None})

    def __call__(self, tool, args=None):
        args = dict(args or {})
        self.calls.append((tool, args))
        return getattr(self, tool.removeprefix("ui:"))(**args)

    def slot_list(self):
        out = []
        for s in self.slots.values():
            g = self.goals.get((s["goal"] or "").lower()) or {}
            out.append({**s, "state": g.get("state"), "lead": g.get("lead"), "thread_id": g.get("thread_id"),
                        "last_value": g.get("last_value"), "members": 0})
        return {"slots": out}

    def slot_assign(self, n, **kw):
        keys = {"goal": "goal", "persona": "persona_name", "persona_icon": "persona_icon", "channel_id": "channel_id", "channel_name": "channel_name"}
        self.slots[n].update({keys[k]: v for k, v in kw.items() if v is not None})
        return next(r for r in self.slot_list()["slots"] if r["n"] == n)

    def slot_clear(self, n):
        self.slots[n].update(goal=None, persona_name=None, persona_icon=None)
        return self.slots[n]

    def goal_create(self, name, objective, folder):
        if name.lower() in self.goals:
            return {"error": f"goal already exists: {name}"}
        if folder == "nope":
            return {"error": f"no such folder: {folder}"}
        tid = 500 + len(self.goals)
        self.goals[name.lower()] = {"name": name, "state": "draft", "objective": objective, "measure_folder": folder,
                                    "lead": f"{name}-lead", "thread_id": tid}
        self.threads[tid] = []
        self.say(name.lower(), "agentdesk", f"**Goal {name}** (draft): {objective}")
        return self.goals[name.lower()]

    def goal_status(self, name):
        return self.goals.get(name.lower()) or {"error": f"no such goal: {name}"}

    def goal_approve(self, name, **opts):
        g = self.goals[name.lower()]
        g.update(state="running", max_members=opts.get("max_members", 3), max_hours=opts.get("max_hours", 24), cadence_minutes=30)
        self.say(name.lower(), "agentdesk", "**Approved by John.**")
        return g

    def goal_stop(self, name):
        g = self.goals[name.lower()]
        g["state"] = "stopped"
        self.say(name.lower(), "agentdesk", "**Goal stopped**: stopped by John.")
        return g

    def identity_forget(self, name):
        return {"forgotten": name}

    def input(self, name, data):
        if name in self.unreachable:
            return {"error": f"no such session: {name}"}
        self.inputs.append((name, data))
        return {"ok": True}

    def thread(self, thread_id):
        return {"thread": {"id": thread_id}, "messages": list(self.threads[thread_id])}


state_dir = REPO / "obj" / "check-swarm"  # repo-local scratch, not %TEMP%
state_dir.mkdir(parents=True, exist_ok=True)
(state_dir / "swarm_state.json").unlink(missing_ok=True)
day = ["2026-09-25"]
logs, sleeps = [], []
slack, sc = FakeSlack(), SwarmCore()
bot = slackcmd.Commands(["swarms"], JOHN, "Swarms", call=sc)
bot.swarm = swarm.Swarms(slack, sc, JOHN, state_dir / "swarm_state.json", log=logs.append, sleep=sleeps.append,
                         today=lambda: date.fromisoformat(day[0]))
with contextlib.redirect_stderr(io.StringIO()):
    out = bot.handle("swarms", JOHN)
    check("swarms lists 10 free slots", out.count("free") == 10 and out.startswith("*1* · free"), out)
    check("swarm commands are John-only", bot.handle("swarm new x C:\\w y", OTHER) is None and not slack.calls)
    check("a bot without the bridge's client says so", slackcmd.Commands(["swarms"], JOHN, call=sc).handle("swarms", JOHN) == slackcmd.NO_SWARMS)
    check("usage on a bad swarm command", bot.handle("swarm approve x", JOHN) == slackcmd.SWARM_USAGE
          and bot.handle("swarm end 1 members=2", JOHN) == slackcmd.SWARM_USAGE)

    out = bot.handle("swarm new alpha nope make it fast", JOHN)
    check("swarm new with a bad folder: the core's error, and no Slack calls", "no such folder" in out and not slack.calls, out)

    out = bot.handle('swarm new alpha "C:\\my work" make the build fast', JOHN)
    create = [a for t, a in sc.calls if t == "ui:goal_create"][-1]
    check("swarm new creates the goal with folder and objective",
          create == {"name": "alpha", "objective": "make the build fast", "folder": r"C:\my work"}, str(create))
    check("swarm new creates swarm-<name> (the slot had no channel) and invites John",
          [c[0] for c in slack.calls[:2]] == ["create", "invite"] and slack.calls[0][1]["name"] == "swarm-alpha"
          and slack.calls[1][1] == {"channel": "C1", "users": JOHN}, str(slack.calls))
    check("swarm new records slot 1: channel, persona, goal",
          sc.slots[1]["goal"] == "alpha" and sc.slots[1]["persona_name"] == "alpha" and sc.slots[1]["channel_id"] == "C1", str(sc.slots[1]))
    p = slack.posts[-1]
    check("swarm new posts 'Proposing a hypothesis…' as the persona",
          p["channel"] == "C1" and p["username"] == "alpha" and p["icon_emoji"] == swarm.ICONS[0] and "Proposing a hypothesis" in p["text"], str(p))
    check("swarm new replies with the slot and the channel", "Slot 1" in out and "<#C1>" in out, out)
    check("swarm new: a taken name is the core's error", "already exists" in bot.handle("swarm new alpha C:\\w again", JOHN))

    posts = len(slack.posts)
    check("relay: nothing new yet (the opener is skipped)", bot.swarm.relay() == 0 and len(slack.posts) == posts)
    sc.say("alpha", "alpha-lead", "**Proposal.** Hypothesis: caching halves it.")
    sc.say("alpha", "alpha-lead", "Read.", meta={"kind": "read-receipt"})
    sc.say("alpha", "john", "noted", kind="human")
    out = bot.handle("swarm approve 1 members=2 hours=4", JOHN)
    approve = [a for t, a in sc.calls if t == "ui:goal_approve"][-1]
    check("swarm approve passes the budget to goal_approve", approve == {"name": "alpha", "max_members": 2, "max_hours": 4.0}, str(approve))
    check("swarm approve says the budget", "Approved *alpha*" in out and "2 members" in out, out)
    new = slack.posts[posts:]
    check("relay: the proposal and the approval go top level; the receipt and John's note do not",
          [("Proposal" in q["text"], "Approved" in q["text"], "thread_ts" in q) for q in new] == [(True, False, False), (False, True, False)], str(new))
    posts = len(slack.posts)
    sc.say("alpha", "alpha-scout", "**Done.** tried ccache")
    sc.say("alpha", "alpha-scout", "second note")
    sc.say("alpha", "agentdesk", "Experiment #1 by alpha-scout (ccache): **41**, improved.")
    bot.swarm.relay()
    new = slack.posts[posts:]
    check("relay: member notes go in a thread under one daily activity message, verdicts top level",
          len(new) == 4 and "Activity, 2026-09-25" in new[0]["text"] and "thread_ts" not in new[0]
          and new[1]["thread_ts"] == new[0]["ts"] == new[2]["thread_ts"] and "alpha-scout" in new[1]["text"]
          and "thread_ts" not in new[3] and "Experiment #1" in new[3]["text"], str(new))
    check("relay: a second pass posts nothing", bot.swarm.relay() == 0)
    check("relay: the watermark survives a restart",
          swarm.Swarms(slack, sc, JOHN, state_dir / "swarm_state.json", log=logs.append).relay() == 0)
    day[0] = "2026-09-26"
    posts = len(slack.posts)
    sc.say("alpha", "alpha-scout", "next day")
    bot.swarm.relay()
    check("relay: a new day starts a new activity message", "Activity, 2026-09-26" in slack.posts[posts]["text"]
          and slack.posts[posts + 1]["thread_ts"] == slack.posts[posts]["ts"])

    print("  channel -> lead")
    check("a message outside every slot's channel is not the swarm's", bot.swarm.route({"channel": "C_OTHER", "user": JOHN, "text": "hi"}) is False)
    bot.swarm.route({"channel": "C1", "user": JOHN, "text": "try &lt;ccache&gt;\nthen measure", "ts": "9.1"})
    check("John in the slot's channel: typed into the lead with provenance, then Enter",
          sc.inputs[-2:] == [("alpha-lead", "John (via Slack): try <ccache> / then measure"), ("alpha-lead", "\r")] and sleeps == [0.3], str(sc.inputs))
    n = len(sc.inputs)
    bot.swarm.route({"channel": "C1", "user": OTHER, "text": "hijack"})
    bot.swarm.route({"channel": "C1", "user": JOHN, "bot_id": "B1", "text": "persona echo"})
    bot.swarm.route({"channel": "C1", "user": JOHN, "subtype": "message_changed", "text": "edit"})
    check("someone else, a bot, or an edit is never input", len(sc.inputs) == n)
    sc.unreachable.add("alpha-lead")
    posts = len(slack.posts)
    bot.swarm.route({"channel": "C1", "user": JOHN, "text": "hello?", "ts": "9.2"})
    check("an unreachable lead is said in the channel, under John's message",
          "Couldn't reach alpha-lead" in slack.posts[posts]["text"] and slack.posts[posts]["thread_ts"] == "9.2", str(slack.posts[posts:]))

    print("  end and reset")
    bot.handle("swarm new beta C:\\w make tests pass", JOHN)
    check("a second swarm takes slot 2 with its own channel", sc.slots[2]["goal"] == "beta" and sc.slots[2]["channel_id"] == "C2")
    posts = len(slack.posts)
    out = bot.handle("swarm end 1", JOHN)
    check("swarm end stops the goal and frees the slot, keeping its channel",
          ("ui:goal_stop", {"name": "alpha"}) in sc.calls and sc.slots[1]["goal"] is None and sc.slots[1]["channel_id"] == "C1" and "slot 1 is free" in out, out)
    check("swarm end relays the core's 'Goal stopped' first", any("Goal stopped" in q["text"] for q in slack.posts[posts:]))
    check("swarm end on a free slot says so", bot.handle("swarm end 1", JOHN) == "Slot 1 is free.")
    slack.calls.clear()
    bot.handle("swarm new gamma C:\\w go", JOHN)
    check("swarm new reuses a free slot's channel by renaming it",
          slack.calls[0] == ("rename", {"channel": "C1", "name": "swarm-gamma"}) and sc.slots[1]["goal"] == "gamma"
          and slack.channels["C1"]["name"] == "swarm-gamma", str(slack.calls[:2]))

    sc.calls.clear()
    slack.calls.clear()
    out = bot.handle("swarm reset 2", JOHN)
    check("swarm reset stops the goal and forgets its lead",
          ("ui:goal_stop", {"name": "beta"}) in sc.calls and ("ui:identity_forget", {"name": "beta-lead"}) in sc.calls, str(sc.calls))
    check("swarm reset archives the old channel and creates a fresh one",
          slack.calls[0] == ("archive", {"channel": "C2"}) and slack.calls[1][0] == "create"
          and slack.calls[1][1]["name"] == "swarm-beta-2", str(slack.calls))
    check("swarm reset creates beta-2 for the same objective and folder",
          ("ui:goal_create", {"name": "beta-2", "objective": "make tests pass", "folder": "C:\\w"}) in sc.calls
          and sc.slots[2]["goal"] == "beta-2" and sc.slots[2]["persona_name"] == "beta-2" and sc.slots[2]["channel_id"] == "C3", str(sc.slots[2]))
    check("swarm reset's persona says it is proposing", slack.posts[-1]["username"] == "beta-2" and "Proposing" in slack.posts[-1]["text"])
    bot.handle("swarm reset 2", JOHN)
    check("a second reset is beta-3, not beta-2-2", sc.slots[2]["goal"] == "beta-3", str(sc.slots[2]))
    slack.fail["create"] = "restricted_action"
    out = bot.handle("swarm reset 2", JOHN)
    check("a Slack refusal on reset still records the goal, without the archived channel",
          "Slack refused" in out and sc.slots[2]["goal"] == "beta-4" and sc.slots[2]["channel_id"] == "", out)
    check("a slot without a channel relays nothing and doesn't fail", bot.swarm.relay_slot(bot.swarm.slot(2)) == 0)

    print("  missing chat:write.customize")
    slack2, sc2, logs2 = FakeSlack(customize=False), SwarmCore(), []
    s2 = swarm.Swarms(slack2, sc2, JOHN, log=logs2.append, sleep=lambda _: None)
    s2.new("delta", "C:\\w", "go")
    s2.post(s2.slot(1), "second post")
    posts = [c for c in slack2.calls if c[0] == "post"]
    check("the persona post fails with missing_scope, then goes out plain, prefixed with *<persona>:*",
          len(posts) == 3 and "username" in posts[0][1] and "username" not in posts[1][1]
          and posts[1][1]["blocks"][0]["text"]["text"].startswith("*delta:*") and "Proposing" in posts[1][1]["text"], str(posts))
    check("later posts skip the override; the missing scope is logged once",
          "username" not in posts[2][1] and posts[2][1]["blocks"][0]["text"]["text"].startswith("*delta:*")
          and len(logs2) == 1 and "chat:write.customize" in logs2[0], str(logs2))
    for i in range(9):
        s2.new(f"g{i}", "C:\\w", "go")
    check("every slot full: swarm new says so", "busy" in s2.new("full", "C:\\w", "go"))

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
