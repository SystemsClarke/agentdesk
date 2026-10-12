"""The surfaces around the core: Slack, the ops console, updates and install, the tray's job, dictation."""
import json
import os
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

from lib import Board, Missing, Waived, feature, wait_for

README = "README.md"


@feature("slack.heartbeat", "slack", "slack_bridge.state in the data folder ({ts, poll_s, last_relay}) is what ui:status.slack reports; up means ts < 90 s old", "docs/ui-api.md ui:status")
def _beat(b):
    ts = time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime())
    with open(os.path.join(b.data, "slack_bridge.state"), "w") as f:
        json.dump({"ts": ts, "poll_s": 3, "last_relay": {"ts": ts, "thread_id": 4}}, f)
    s = b.need("ui:status")["slack"]
    assert s and s["poll_s"] == 3 and s["last_relay"]["thread_id"] == 4


@feature("slack.bridge-supervision", "slack", "The core supervises the bridge: external / stale-orphan / supervised / starting, restarts on exit, kills one hung 180 s", "docs/ui-api.md ui:status bridge")
def _supervise(b):
    d = b.need("ui:status")
    if "bridge" not in d:
        raise Missing("ui:status has no bridge section")
    assert {"enabled", "state"} <= set(d["bridge"])


class _FakeSlack(BaseHTTPRequestHandler):
    posts = []

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode()
        _FakeSlack.posts.append((self.path, body))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({"ok": True, "ts": "1.1", "channel": "D1", "messages": [], "user": {"id": "U1"}}).encode())

    do_GET = do_POST

    def log_message(self, *a):
        pass


@feature("slack.relay-question", "slack", "A new question is relayed to John's Slack DM (formatted: tables, mermaid, charts as images); his threaded reply comes back as a board reply with via=slack", "scripts/slack_bridge.py")
def _relay(b):
    srv = HTTPServer(("127.0.0.1", 0), _FakeSlack)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    r = b.run("slack-bridge", "--once", env={"SLACK_API_URL": "http://127.0.0.1:%d/api/" % srv.server_port, "SLACK_BOT_TOKEN": "xoxb-test"})
    srv.shutdown()
    if r.returncode == 2 and "unrecognized" in r.stderr:
        raise Missing("no Slack bridge")


@feature("slack.phone-commands", "slack", "John's DM commands: status, concierge on|off, governor on|off, wake (in a question's thread), swarms / swarm new|approve|end|reset", "docs/ui-api.md Swarm slots, slackcmd.py")
def _phone(b):
    raise Missing("needs slack.relay-question first")


@feature("slack.swarm-channels", "slack", "Swarm slots: each slot's channel mirrors its goal thread as a persona and relays John's channel messages to the lead (`John (via Slack): ...`)", "docs/ui-api.md Swarm slots")
def _swarmslack(b):
    raise Missing("needs slack.relay-question first")


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


@feature("tray.surface", "tray", "The tray icon (open ops console, restart to update) and its click-to-open toasts", "Tray.cs")
def _tray(b):
    raise Waived("a terminal program has no tray: toasts and the window's own Options/Update rows replace it")


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
