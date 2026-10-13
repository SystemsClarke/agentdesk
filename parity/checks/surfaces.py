"""The surfaces around the core: Slack, the ops console, updates and install, the tray's job, dictation."""
import json
import os
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

from lib import Board, Missing, feature, wait_for

README = "README.md"


@feature("slack.heartbeat", "slack", "slack_bridge.state in the data folder ({ts, poll_s, last_relay}) is what ui:status.slack reports; up means ts < 90 s old", "docs/ui-api.md ui:status")
def _beat(b):
    ts = time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime())
    with open(os.path.join(b.data, "slack_bridge.state"), "w") as f:
        json.dump({"ts": ts, "poll_s": 3, "last_relay": {"ts": ts, "thread_id": 4}}, f)
    s = b.need("ui:status")["slack"]
    assert s and s["poll_s"] == 3 and s["last_relay"]["thread_id"] == 4


STUB_BRIDGE = """import json, os, sys, time
data = os.environ["AGENTDESK_DATA"]
n = int(open(os.path.join(data, "stub_runs")).read() or 0) if os.path.exists(os.path.join(data, "stub_runs")) else 0
open(os.path.join(data, "stub_runs"), "w").write(str(n + 1))
life = float(os.environ.get("STUB_LIFE", "0"))
t0 = time.time()
while True:
    ts = time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime())
    json.dump({"pid": os.getpid(), "ts": ts, "poll_s": 1, "last_relay": None}, open(os.path.join(data, "slack_bridge.state"), "w"))
    if life and time.time() - t0 > life:
        sys.exit(3)
    time.sleep(0.3)
"""

BRIDGE_ENV = {"AGENTDESK_BRIDGE_POLL": "1", "AGENTDESK_BRIDGE_RESTART": "1", "AGENTDESK_BRIDGE_HUNG": "30"}


@feature("slack.bridge-supervision", "slack", "The core supervises the bridge: external / stale-orphan / supervised / starting, restarts on exit, kills one hung 180 s", "docs/ui-api.md ui:status bridge")
def _supervise(b):
    d = b.need("ui:status")
    if "bridge" not in d:
        raise Missing("ui:status has no bridge section")
    assert {"enabled", "state"} <= set(d["bridge"])
    # off in settings: nothing is started
    with open(os.path.join(b.data, "settings.json"), "w") as f:
        json.dump({"slack_bridge": False}, f)
    assert b.need("ui:status")["bridge"]["enabled"] is False
    os.remove(os.path.join(b.data, "settings.json"))
    # someone else's bridge, beating: left alone
    ts = time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime())
    with open(os.path.join(b.data, "slack_bridge.state"), "w") as f:
        json.dump({"pid": os.getpid(), "ts": ts, "poll_s": 1}, f)
    assert b.need("ui:status")["bridge"]["state"] == "external"
    os.remove(os.path.join(b.data, "slack_bridge.state"))
    # no bridge at all: the daemon starts the configured command, and starts it again when it exits
    stub = b.stub("bridgestub", STUB_BRIDGE)
    env = dict(BRIDGE_ENV, AGENTDESK_BRIDGE_CMD='"%s"' % stub, STUB_LIFE="2")
    b.spawn("daemon", env=env)
    wait_for(lambda: b.need("ui:status")["bridge"].get("state") == "supervised" and b.need("ui:status")["bridge"].get("pid"), 20, what="a supervised bridge with a pid")
    runs = os.path.join(b.data, "stub_runs")
    wait_for(lambda: os.path.exists(runs) and int(open(runs).read() or 0) >= 2, 30, what="the bridge restarted after it exited")
    br = b.need("ui:status")["bridge"]
    assert br["restarts"] >= 1 and br["last_exit"] == 3, br


def _fake_env(b, history=None):
    log = os.path.join(b.dir, "slack.log")
    events = os.path.join(b.dir, "slack.events")
    creds = os.path.join(b.dir, "slack-notify")
    os.makedirs(creds, exist_ok=True)
    for n, v in (("bot-token.txt", "xoxb-fake"), ("app-token.txt", "xapp-fake")):
        with open(os.path.join(creds, n), "w") as f:
            f.write(v)
    for p in (log, events):
        open(p, "w").close()
    fakes = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "fakes")
    env = {"PYTHONPATH": fakes, "FAKE_SLACK_LOG": log, "FAKE_SLACK_EVENTS": events, "AGENTDESK_SLACK_DIR": creds,
           "AGENTDESK_SLACK_DM": "DJOHN", "AGENTDESK_SLACK_USER": "UJOHN"}
    if history:
        env["FAKE_SLACK_HISTORY"] = history
    return env, log, events


def _calls(log):
    return [json.loads(l) for l in open(log).read().splitlines() if l.strip()]


@feature("slack.relay-question", "slack", "A new question is relayed to John's Slack DM (formatted: tables, mermaid, charts as images); his threaded reply comes back as a board reply with via=slack", "scripts/slack_bridge.py")
def _relay(b):
    env, log, events = _fake_env(b)
    q = b.tool("ask_human", {"subject": "Deploy to prod?", "body": "| a | b |\n|---|---|\n| 1 | 2 |\n\nShip it?", "author": "builder"}, session="s1")["thread_id"]
    p = b.spawn("python", "scripts/slack_bridge.py", env=env)
    try:
        post = wait_for(lambda: next((c for c in _calls(log) if c["kind"] == "post" and c.get("channel") == "DJOHN"), None), 30, what="the question relayed to John's DM")
        assert "Deploy to prod?" in json.dumps(post) and "#%d" % q in json.dumps(post), post
        with open(events, "a") as f:
            f.write(json.dumps({"event": {"channel": "DJOHN", "channel_type": "im", "user": "UJOHN", "thread_ts": post["ts"], "text": "Yes, ship it", "ts": "1800000000.0001"}}) + "\n")
        wait_for(lambda: any(m["author"] == "john" and m.get("meta") for m in [dict(r) for r in b.sql("SELECT author, meta FROM messages WHERE thread_id=? AND author_kind='human'", (q,))]), 20, what="the reply on the board")
        row = b.sql("SELECT body, meta FROM messages WHERE thread_id=? AND author_kind='human'", (q,))[0]
        assert row["body"].strip() == "Yes, ship it" and json.loads(row["meta"]).get("via") == "slack", row
        assert b.sql("SELECT status FROM threads WHERE id=?", (q,))[0]["status"] == "answered"
        wait_for(lambda: any("Recorded as your answer" in (c.get("text") or "") for c in _calls(log) if c["kind"] == "say"), 10, what="the confirmation in the thread")
        # someone else's reply is not an answer
        with open(events, "a") as f:
            f.write(json.dumps({"event": {"channel": "DJOHN", "channel_type": "im", "user": "UOTHER", "thread_ts": post["ts"], "text": "nope", "ts": "1800000000.0002"}}) + "\n")
        time.sleep(1.0)
        assert len(b.sql("SELECT id FROM messages WHERE thread_id=? AND author_kind='human'", (q,))) == 1
    finally:
        with open(events, "a") as f:
            f.write(json.dumps({"quit": True}) + "\n")
        try:
            p.wait(timeout=10)
        except Exception:
            p.kill()


def _commands(b, text, user="UJOHN", env=None):
    code = ("from agentdesk import slackcmd; import sys, json\n"
            "c = slackcmd.Commands(list(slackcmd.AREAS), 'UJOHN', 'AgentDesk')\n"
            "print(json.dumps(c.handle(sys.argv[1], sys.argv[2])))")
    path = os.path.join(b.work, "cmd.py")
    with open(path, "w") as f:
        f.write(code)
    e = {"AGENTDESK_BIN": b.bin}
    e.update(env or {})
    r = b.run("python", path, text, user, env=e)
    assert r.returncode == 0, r.stderr[-500:]
    return json.loads(r.stdout.strip().splitlines()[-1])


@feature("slack.phone-commands", "slack", "John's DM commands: status, concierge on|off, governor on|off, wake (in a question's thread), swarms / swarm new|approve|end|reset", "docs/ui-api.md Swarm slots, slackcmd.py")
def _phone(b):
    h = _commands(b, "help")
    for w in ("status", "concierge", "governor", "swarm"):
        assert w in h, h
    assert _commands(b, "status", user="UOTHER") is None, "only John may run commands"
    assert _commands(b, "hello there") is None
    _commands(b, "concierge on")
    assert b.need("ui:status")["concierge"]["on"] is True
    _commands(b, "concierge off")
    assert b.need("ui:status")["concierge"]["on"] is False
    _commands(b, "governor off")
    assert b.need("ui:status")["governor"]["enforcing"] is False
    _commands(b, "governor on")
    assert b.need("ui:status")["governor"]["enforcing"] is True
    assert "Slack" in _commands(b, "status") or "slack" in _commands(b, "status").lower()
    # wake, in a question's thread, goes to the core
    q = b.tool("ask_human", {"subject": "wake me", "body": "?", "author": "builder"}, session="s1")["thread_id"]
    env, log, events = _fake_env(b)
    p = b.spawn("python", "scripts/slack_bridge.py", env=env)
    try:
        post = wait_for(lambda: next((c for c in _calls(log) if c["kind"] == "post" and c.get("channel") == "DJOHN"), None), 30, what="relay")
        with open(events, "a") as f:
            f.write(json.dumps({"event": {"channel": "DJOHN", "channel_type": "im", "user": "UJOHN", "thread_ts": post["ts"], "text": "wake", "ts": "1800000000.0003"}}) + "\n")
        wait_for(lambda: any(c["kind"] == "say" and c.get("thread_ts") == post["ts"] for c in _calls(log)), 20, what="the core's answer to wake")
        assert len(b.sql("SELECT id FROM messages WHERE thread_id=? AND author_kind='human'", (q,))) == 0, "wake is not an answer"
    finally:
        with open(events, "a") as f:
            f.write(json.dumps({"quit": True}) + "\n")
        try:
            p.wait(timeout=10)
        except Exception:
            p.kill()


@feature("slack.swarm-channels", "slack", "Swarm slots: each slot's channel mirrors its goal thread as a persona and relays John's channel messages to the lead (`John (via Slack): ...`)", "docs/ui-api.md Swarm slots")
def _swarmslack(b):
    hist = os.path.join(b.dir, "history.json")
    env, log, events = _fake_env(b, hist)
    code = ("import json, os, sys\nfrom pathlib import Path\nfrom agentdesk import slackcmd, swarm\nimport slack_bolt\n"
            "c = slack_bolt.Client()\n"
            "s = swarm.Swarms(c, slackcmd.call, 'UJOHN', Path(os.environ['AGENTDESK_DATA']) / 'swarm_state.json')\n"
            "s.sleep = lambda x: None\n"
            "step = sys.argv[1]\n"
            "if step == 'new': print(s.new('alpha', os.getcwd(), 'raise the score'))\n"
            "elif step == 'list': print(s.list())\n"
            "elif step == 'tick': print(s.poll(), s.relay())\n")
    path = os.path.join(b.work, "swarm.py")
    with open(path, "w") as f:
        f.write(code)

    def step(name):
        r = b.run("python", path, name, env=dict(env, AGENTDESK_BIN=b.bin))
        assert r.returncode == 0, r.stderr[-600:]
        return r.stdout

    out = step("new")
    assert "Slot 1" in out and "alpha" in out, out
    calls = [c["kind"] for c in _calls(log)]
    assert "create" in calls, calls
    assert "alpha" in b.need("ui:goal_status", {"name": "alpha"}).get("name", "")
    assert "alpha" in step("list")
    step("tick")  # first sight of the channel: the watermark starts at its newest message
    cid = next(c for c in _calls(log) if c["kind"] == "create")
    chan = "C" + cid["name"][-6:].upper().replace("-", "X")
    with open(hist, "w") as f:
        json.dump({chan: [{"ts": "%d.0001" % (time.time() + 100), "user": "UJOHN", "text": "try the other approach"}]}, f)
    step("tick")
    lead = b.need("ui:goal_status", {"name": "alpha"}).get("lead") or "alpha-lead"
    # the lead is not running, so the core refuses the input and the bridge says so in the channel
    posts = [c for c in _calls(log) if c["kind"] == "post" and c.get("channel") == chan]
    assert any(lead in json.dumps(c) or "free" in json.dumps(c) or "reach" in json.dumps(c) for c in posts), posts


class _Console:
    pass


def _console(b):
    p = b.spawn("daemon")
    time.sleep(1.5)
    d = b.need("ui:web_url")
    url = d.get("url")
    if not url:
        raise Missing("no ops console")
    assert url.startswith("http://127.0.0.1:") and "?k=" in url
    base, key = url.split("/?k=")
    return base, key


def _request(base, method, path, headers=None, body=None):
    import http.client
    host = base.split("//")[1]
    c = http.client.HTTPConnection(host, timeout=10)
    c.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
    h = {"Host": host}
    h.update(headers or {})
    for k, v in h.items():
        c.putheader(k, v)
    if body is not None:
        c.putheader("Content-Length", str(len(body)))
    c.endheaders(body)
    r = c.getresponse()
    data = r.read()
    out = (r.status, dict((k.lower(), v) for k, v in r.getheaders()), data)
    c.close()
    return out


@feature("web.console-auth", "web", "The ops console listens on 127.0.0.1 only; every request needs the key (web.key); /?k=<key> sets an HttpOnly SameSite=Strict cookie and redirects; wrong Host or Origin is 400", "docs/ui-api.md The ops console")
def _web_auth(b):
    base, key = _console(b)
    host = base.split("//")[1]
    assert len(key) == 43 and open(os.path.join(b.data, "web.key")).read().strip() == key
    assert open(os.path.join(b.data, "web.port")).read().strip() == host.split(":")[1]
    assert _request(base, "GET", "/")[0] == 401, "no key, no page"
    assert _request(base, "GET", "/?k=wrong")[0] == 401
    st, h, _ = _request(base, "GET", "/?k=" + key)
    assert st == 302 and h["location"] == "/", (st, h)
    cookie = h["set-cookie"]
    assert "HttpOnly" in cookie or "httponly" in cookie.lower()
    assert "samesite=strict" in cookie.lower()
    cj = {"Cookie": "k=" + key}
    assert _request(base, "GET", "/", cj)[0] == 200
    assert _request(base, "GET", "/", dict(cj, Host="evil.example:80"))[0] == 400, "DNS rebinding"
    assert _request(base, "POST", "/api/status", dict(cj, Origin="http://evil.example"), b"{}")[0] == 400, "a foreign Origin"
    assert _request(base, "GET", "/nothing", cj)[0] == 404


@feature("web.console-api", "web", "POST /api/<op> runs core, status, open_questions, session_list, log_tail, concierge, update, identity_* through the same objects as the ui requests", "docs/ui-api.md The ops console")
def _web_api(b):
    base, key = _console(b)
    cj = {"Cookie": "k=" + key, "Content-Type": "application/json"}

    def api(op, args=None):
        st, h, body = _request(base, "POST", "/api/" + op, cj, json.dumps(args or {}).encode())
        assert st == 200, (op, st)
        return json.loads(body)

    assert _request(base, "GET", "/api/status", cj)[0] == 405
    core = api("core")
    assert core["pid"] and core["started"] and "update" in core
    for op, keys in (("status", ("usage", "prs", "goals", "concierge")), ("open_questions", ("open_questions",)), ("session_list", ("sessions",)), ("identity_list", ("identities",)), ("log_tail", ("lines",)), ("job_list", ("jobs",))):
        d = api(op)
        assert all(k in d for k in keys), (op, d)
    made = api("identity_create", {"name": "webbed", "folder": b.work})
    assert made["name"] == "webbed"
    assert api("identity_forget", {"name": "webbed"}) == {"forgotten": "webbed"}
    assert "error" in api("no_such_op")
    assert "error" in api("thread", {"thread_id": 1}), "the console answers only the requests its page needs"


@feature("web.console-page", "web", "The one page: core version, uptime, agents with Start/Stop/Forget/New, sessions, SlackNet, Concierge toggle, usage and governor, merge list, open questions, the core log", "docs/ui-api.md The ops console")
def _web_page(b):
    base, key = _console(b)
    st, h, body = _request(base, "GET", "/", {"Cookie": "k=" + key})
    page = body.decode()
    assert st == 200 and h["content-type"].startswith("text/html")
    assert "frame-ancestors 'none'" in h["content-security-policy"] and h["x-content-type-options"] == "nosniff"
    for needle in ("Core", "Agents", "Sessions", "Concierge", "Open questions", "api("):
        assert needle in page, needle


@feature("update.check-apply", "update", "The core checks GitHub Releases every four hours and offers to apply; `agentdesk update [--apply]` downloads and swaps the binary", "README Install")
def _update(b):
    r = b.run("update", "--help")
    assert "--apply" in r.stdout


@feature("update.rollout", "update", "A tagged release publishes archives for Windows, macOS and Linux that `agentdesk update` can install (a flat archive with the binary at its root)", ".github/workflows/release.yml")
def _rollout(b):
    wf = open(os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), ".github", "workflows", "release.yml")).read()
    for t in ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"):
        assert t in wf


@feature("install.scripts", "update", "scripts/install.sh and scripts/install.ps1 install the newest release onto the PATH", "scripts/")
def _install(b):
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    assert os.path.exists(os.path.join(root, "scripts", "install.sh")) and os.path.exists(os.path.join(root, "scripts", "install.ps1"))


@feature("dictation.engine", "dictation", "Ctrl+D local speech-to-text (sherpa-onnx) with a 2 s pre-roll microphone: the original's own pre-roll check passes against the embedded Python, and a missing microphone or model comes back as state done with an error", "docs/ui-api.md ui:dictate, dictate.py, scripts/check_dictate.py")
def _dictation(b):
    r = b.run("python", "scripts/check_dictate.py")
    if r.returncode != 0 and "No such file" in (r.stderr + r.stdout) or "no Python 3 interpreter" in (r.stderr + r.stdout):
        raise Missing("no Python interpreter")
    assert r.returncode == 0, (r.stdout + r.stderr)[-600:]
    d = b.need("ui:dictate", {"action": "start"})
    assert d["state"] in ("loading", "downloading", "listening", "done")
    if d["state"] == "done":
        assert d.get("error"), "a missing microphone or model says why: %s" % d
