"""A stand-in for slack_bolt, so the original's slack_bridge.py runs unchanged with no Slack: every call is logged as a JSON line to
$FAKE_SLACK_LOG, and conversations_history answers from $FAKE_SLACK_HISTORY (a JSON object: channel -> [messages])."""
import json
import os


def log(kind, **kw):
    with open(os.environ["FAKE_SLACK_LOG"], "a") as f:
        f.write(json.dumps(dict(kind=kind, **kw), default=str) + "\n")


class Client:
    token = "xoxb-fake"

    def __init__(self):
        self.n = 0

    def _ts(self):
        self.n += 1
        return "%d.%04d" % (1700000000 + self.n, self.n)

    def chat_postMessage(self, **kw):
        ts = self._ts()
        log("post", ts=ts, **kw)
        return {"ok": True, "ts": ts}

    def files_upload_v2(self, **kw):
        log("file", **{k: v for k, v in kw.items() if k != "content"})
        return {"ok": True}

    def conversations_create(self, name, is_private=False):
        log("create", name=name)
        return {"channel": {"id": "C" + name[-6:].upper().replace("-", "X"), "name": name}}

    def conversations_rename(self, channel, name):
        log("rename", channel=channel, name=name)
        return {"channel": {"id": channel, "name": name}}

    def conversations_invite(self, channel, users):
        log("invite", channel=channel, users=users)
        return {"ok": True}

    def conversations_archive(self, channel):
        log("archive", channel=channel)
        return {"ok": True}

    def conversations_history(self, channel, oldest=None, limit=100):
        path = os.environ.get("FAKE_SLACK_HISTORY")
        msgs = json.load(open(path)).get(channel, []) if path and os.path.exists(path) else []
        msgs = sorted(msgs, key=lambda m: -float(m["ts"]))
        if oldest:
            msgs = [m for m in msgs if float(m["ts"]) > float(oldest)]
        return {"messages": msgs[:limit]}


class App:
    handlers = {}

    def __init__(self, token=None, **kw):
        self.client = Client()

    def event(self, name):
        def deco(fn):
            App.handlers.setdefault(name, []).append(fn)
            return fn

        return deco
