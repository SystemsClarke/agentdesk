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


def _fake_env(b):
    log = os.path.join(b.dir, "slack.log")
    events = os.path.join(b.dir, "slack.events")
    creds = os.path.join(b.dir, "slack-notify")
    os.makedirs(creds, exist_ok=True)
    for n, v in (("bot-token.txt", "xoxb-fake"), ("app-token.txt", "xapp-fake")):
        with open(os.path.join(creds, n), "w") as f:
            f.write(v)
    for p in (log, events):
        open(p, "w").close()
    return log, events


def _calls(log):
    return [json.loads(l) for l in open(log).read().splitlines() if l.strip()]


def _event(user, text, thread_ts=None, ts=None):
    ev = {"type": "message", "channel": "DJOHN", "channel_type": "im", "user": user, "text": text, "ts": ts or "%.4f" % (time.time() + 1000)}
    if thread_ts:
        ev["thread_ts"] = thread_ts
    return ev


class NativeBridge:
    """The Rust bridge (`agentdesk slack-bridge`, the same code the core runs when `slack_native` is on) against parity/fakes/slack_web.py:
    real HTTP for the Web API and a real websocket for Socket Mode. `files` gives it its credentials as files in AGENTDESK_SLACK_DIR, as
    on John's machine; otherwise SLACK_BOT_TOKEN / SLACK_APP_TOKEN."""

    def __init__(self, b, history=None, files=False, spawn=True):
        import sys
        sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "fakes"))
        from slack_web import FakeSlack
        self.b = b
        self.log, self.events = _fake_env(b)
        self.creds = os.path.join(b.dir, "slack-notify")
        self.fake = FakeSlack(self.log, self.events, history)
        self.env = {"SLACK_API_URL": self.fake.api_url, "AGENTDESK_SLACK_DM": "DJOHN", "AGENTDESK_SLACK_USER": "UJOHN", "AGENTDESK_SLACK_POLL": "1"}
        if files:
            self.env["AGENTDESK_SLACK_DIR"] = self.creds
        else:
            self.env.update({"SLACK_BOT_TOKEN": "xoxb-fake", "SLACK_APP_TOKEN": "xapp-fake"})
        self.p = None
        self.spawn = spawn  # False: someone else (the daemon) runs the bridge

    def start(self):
        if self.p is None and self.spawn:
            self.p = self.b.spawn("slack-bridge", env=self.env)

    def calls(self):
        return _calls(self.log)

    def event(self, ev):
        with open(self.events, "a") as f:
            f.write(json.dumps({"event": ev}) + "\n")

    def says(self):
        return [c for c in self.calls() if c["kind"] == "post" and not (c.get("text") or "").startswith("Question #")]

    @staticmethod
    def text_of(say):
        blocks = say.get("blocks") or []
        return "\n".join(b["text"]["text"] for b in blocks if b.get("text")) or say.get("text") or ""

    def dm_says(self):
        return [c for c in self.says() if c.get("channel") == "DJOHN"]

    def command(self, text, user="UJOHN"):
        self.start()
        before = len(self.dm_says())
        self.event(_event(user, text))
        try:
            wait_for(lambda: len(self.dm_says()) > before, 4 if user == "UJOHN" else 1.5, what="a reply")
        except AssertionError:
            return None
        return self.text_of(self.dm_says()[before])

    def stop(self):
        with open(self.events, "a") as f:
            f.write(json.dumps({"quit": True}) + "\n")
        if self.p is not None:
            self.p.terminate()
            try:
                self.p.wait(timeout=10)
            except Exception:
                self.p.kill()
        self.fake.stop()


def _first_post(drv):
    return wait_for(lambda: next((c for c in drv.calls() if c["kind"] == "post" and c.get("channel") == "DJOHN" and (c.get("text") or "").startswith("Question #")), None), 30, what="the question relayed to John's DM")


def _relay_check(b, drv):
    q = b.tool("ask_human", {"subject": "Deploy to prod?", "body": "| a | b |\n|---|---|\n| 1 | 2 |\n\nShip it?", "author": "builder"}, session="s1")["thread_id"]
    drv.start()
    try:
        post = _first_post(drv)
        assert "Deploy to prod?" in json.dumps(post) and "#%d" % q in json.dumps(post), post
        drv.event(_event("UJOHN", "Yes, ship it", post["ts"], "1800000000.0001"))
        wait_for(lambda: any(m["author"] == "john" and m.get("meta") for m in [dict(r) for r in b.sql("SELECT author, meta FROM messages WHERE thread_id=? AND author_kind='human'", (q,))]), 20, what="the reply on the board")
        row = b.sql("SELECT body, meta FROM messages WHERE thread_id=? AND author_kind='human'", (q,))[0]
        assert row["body"].strip() == "Yes, ship it" and json.loads(row["meta"]).get("via") == "slack", row
        assert b.sql("SELECT status FROM threads WHERE id=?", (q,))[0]["status"] == "answered"
        wait_for(lambda: any("Recorded as your answer" in (c.get("text") or "") for c in drv.says()), 10, what="the confirmation in the thread")
        # someone else's reply is not an answer
        drv.event(_event("UOTHER", "nope", post["ts"], "1800000000.0002"))
        time.sleep(1.0)
        assert len(b.sql("SELECT id FROM messages WHERE thread_id=? AND author_kind='human'", (q,))) == 1
        # John's credentials are files, and the bridge keeps the same state file next to them
        state = json.load(open(os.path.join(drv.creds, "bridge_state.json")))
        assert state["posted"][str(q)]["ts"] == post["ts"], state
        beat = json.load(open(os.path.join(b.data, "slack_bridge.state")))
        assert beat["pid"] and beat["ts"] and "poll_s" in beat, beat
    finally:
        drv.stop()


@feature("slack.relay-question-native", "slack", "The native bridge relays a question to John's DM over the Web API and takes his threaded reply from Socket Mode as a board reply with via=slack, credentials from the files in slack-notify", "src/slack.rs")
def _relay_native(b):
    _relay_check(b, NativeBridge(b, files=True))


def _phone_check(b, drv):
    try:
        h = drv.command("help")
        for w in ("status", "concierge", "governor", "swarm"):
            assert w in h, h
        assert drv.command("status", user="UOTHER") is None, "only John may run commands"
        assert "inside the thread" in (drv.command("hello there") or "")  # the board area tells John where an answer goes
        drv.command("concierge on")
        assert b.need("ui:status")["concierge"]["on"] is True
        drv.command("concierge off")
        assert b.need("ui:status")["concierge"]["on"] is False
        drv.command("governor off")
        assert b.need("ui:status")["governor"]["enforcing"] is False
        drv.command("governor on")
        assert b.need("ui:status")["governor"]["enforcing"] is True
        assert "slack" in drv.command("status").lower()
        # wake, in a question's thread, goes to the core
        q = b.tool("ask_human", {"subject": "wake me", "body": "?", "author": "builder"}, session="s1")["thread_id"]
        drv.start()
        post = _first_post(drv)
        before = len(drv.says())
        drv.event(_event("UJOHN", "wake", post["ts"], "1800000000.0003"))
        wait_for(lambda: any(c.get("thread_ts") == post["ts"] for c in drv.says()[before:]), 20, what="the core's answer to wake")
        assert "replied" in drv.says()[before]["text"], drv.says()[before]
        assert len(b.sql("SELECT id FROM messages WHERE thread_id=? AND author_kind='human'", (q,))) == 0, "wake is not an answer"
    finally:
        drv.stop()


@feature("slack.phone-commands-native", "slack", "The native bridge answers John's DM commands (help, status, concierge, governor, wake) and no one else's", "src/slack_cmd.rs")
def _phone_native(b):
    _phone_check(b, NativeBridge(b))


@feature("slack.swarm-channels-native", "slack", "The native bridge runs the swarm slots: `swarm new` makes the channel and persona, `swarms` lists it, John's channel messages are polled (with a first-sight watermark) and typed into the lead, and a lead that cannot be reached is said so in the channel", "src/swarm.rs")
def _swarmslack_native(b):
    hist = os.path.join(b.dir, "history.json")
    drv = NativeBridge(b, hist)
    try:
        drv.start()
        out = drv.command("swarm new alpha \"%s\" raise the score" % b.work)
        assert out and "Slot 1" in out and "alpha" in out, out
        assert "create" in [c["kind"] for c in drv.calls()]
        assert "invite" in [c["kind"] for c in drv.calls()], "John is invited to the channel"
        persona = next(c for c in drv.calls() if c["kind"] == "post" and "Proposing" in (c.get("text") or ""))
        assert persona.get("username") == "alpha" and persona.get("icon_emoji"), persona
        assert "alpha" in b.need("ui:goal_status", {"name": "alpha"}).get("name", "")
        assert "alpha" in (drv.command("swarms") or "")
        slot = b.need("ui:slot_list")["slots"][0]
        chan = slot["channel_id"]
        assert chan and slot["goal"] == "alpha" and slot["persona_name"] == "alpha", slot
        with open(hist, "w") as f:
            json.dump({chan: [{"ts": "%d.0001" % (time.time() + 100), "user": "UJOHN", "text": "try the other approach"}]}, f)
        lead = b.need("ui:goal_status", {"name": "alpha"}).get("lead") or "alpha-lead"
        # the lead is not running (and there is no core to type through), so the bridge says so in the channel
        wait_for(lambda: any(c.get("channel") == chan and "reach" in (c.get("text") or "") for c in drv.says()), 20, what="the unreachable lead said in the channel")
        state = json.load(open(os.path.join(b.data, "swarm_state.json")))
        assert chan in state["channels"] and "goals" in state, state
        out = drv.command("swarm end 1")
        assert out and "Ended" in out and "alpha" in out and "keeps its channel" in out, out
        assert b.need("ui:slot_list")["slots"][0]["channel_id"] == chan
    finally:
        drv.stop()


@feature("slack.native-in-core", "slack", "With slack_native on, the core runs the native bridge itself: ui:status bridge is supervised and native with the core's pid, a question is relayed and its threaded reply answers it, and John's message in a swarm channel is typed straight into the running lead", "src/bridge.rs, src/slack.rs")
def _native_core(b):
    typed = os.path.join(b.work, "typed.txt")
    lead_stub = "import sys\nfor line in sys.stdin:\n    open(%r, 'a').write(line)\n    sys.stdout.write('ok\\n')\n    sys.stdout.flush()\n" % typed
    b.stub("claude", "import sys\n" + lead_stub)
    hist = os.path.join(b.dir, "history.json")
    drv = NativeBridge(b, hist, spawn=False)
    with open(os.path.join(b.data, "settings.json"), "w") as f:
        json.dump({"slack_native": True}, f)
    daemon = b.spawn("daemon", env=dict(drv.env, AGENTDESK_NO_WEB="1"))
    try:
        st = wait_for(lambda: (lambda d: d if d.get("state") == "supervised" and d.get("pid") else None)(b.need("ui:status")["bridge"]), 30, what="the native bridge supervised")
        core = json.load(open(os.path.join(b.data, "core.json")))
        assert st["native"] is True and st["supervised"] is True and st["pid"] == core["pid"], (st, core)
        wait_for(lambda: (b.need("ui:status")["slack"] or {}).get("pid") == core["pid"], 20, what="the heartbeat, written by the core itself")
        # a question out, its reply in
        q = b.tool("ask_human", {"subject": "In the core?", "body": "Yes or no?", "author": "builder"}, session="s1")["thread_id"]
        post = _first_post(drv)
        assert "#%d" % q in json.dumps(post)
        drv.event(_event("UJOHN", "yes", post["ts"], "1800000000.0004"))
        wait_for(lambda: [r for r in b.sql("SELECT meta FROM messages WHERE thread_id=? AND author_kind='human'", (q,))], 20, what="the reply on the board")
        assert json.loads(b.sql("SELECT meta FROM messages WHERE thread_id=? AND author_kind='human'", (q,))[0]["meta"])["via"] == "slack"
        # a swarm: the lead is a session the core owns, and a message in the channel reaches its terminal
        out = drv.command("swarm new alpha \"%s\" raise the score" % b.work)
        assert out and "Slot 1" in out, out
        lead = b.need("ui:goal_status", {"name": "alpha"}).get("lead") or "alpha-lead"
        assert b.need("ui:identity_start", {"name": lead})["state"] == "running"
        chan = b.need("ui:slot_list")["slots"][0]["channel_id"]
        time.sleep(1.5)  # the lead needs to be reading
        with open(hist, "w") as f:
            json.dump({chan: [{"ts": "%d.0001" % (time.time() + 100), "user": "UJOHN", "text": "try the other approach"}]}, f)
        wait_for(lambda: os.path.exists(typed) and "John (via Slack): try the other approach" in open(typed).read(), 30, what="John's message typed into the lead")
    finally:
        drv.stop()
        daemon.kill()


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
