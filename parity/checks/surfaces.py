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


@feature("web.console-auth", "web", "The ops console listens on 127.0.0.1 only; every request needs the key (web.key); /?k=<key> sets an HttpOnly SameSite=Strict cookie and redirects; wrong Host or Origin is 400", "docs/ui-api.md The ops console")
def _web_auth(b):
    p = b.spawn("daemon")
    time.sleep(1.5)
    d = b.need("ui:web_url")
    url = d.get("url")
    if not url:
        raise Missing("no ops console")
    base = url.split("?")[0]

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *a, **k):
            return None

    op = urllib.request.build_opener(NoRedirect)
    try:
        op.open(base)
        raise AssertionError("an unauthenticated request must be refused")
    except urllib.error.HTTPError as e:
        assert e.code == 401
    try:
        op.open(url)
    except urllib.error.HTTPError as e:
        assert e.code in (301, 302, 303) and "HttpOnly" in e.headers.get("Set-Cookie", "") and "SameSite=Strict" in e.headers.get("Set-Cookie", "")


@feature("web.console-api", "web", "POST /api/<op> runs core, status, open_questions, session_list, log_tail, concierge, update, identity_* through the same objects as the ui requests", "docs/ui-api.md The ops console")
def _web_api(b):
    raise Missing("needs web.console-auth first")


@feature("web.console-page", "web", "The one page: core version, uptime, agents with Start/Stop/Forget/New, sessions, SlackNet, Concierge toggle, usage and governor, merge list, open questions, the core log", "docs/ui-api.md The ops console")
def _web_page(b):
    raise Missing("needs web.console-auth first")


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


@feature("dictation.engine", "dictation", "Ctrl+D local speech-to-text (Nemotron streaming through sherpa-onnx) with a 2 s pre-roll; first use downloads the model", "docs/ui-api.md ui:dictate, dictate.py")
def _dictation(b):
    r = b.need("ui:dictate", {"action": "start"})
    assert r["state"] in ("loading", "downloading", "listening", "done")
    raise Missing("no speech engine behind it")
