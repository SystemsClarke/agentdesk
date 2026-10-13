"""Replays events from $FAKE_SLACK_EVENTS (JSON lines: {"event": {...}}, or {"quit": true}) into the handlers the bridge registered."""
import json
import os
import time

from slack_bolt import App, log


class SocketModeHandler:
    def __init__(self, app, token):
        self.app = app

    def connect(self):
        pass

    def start(self):
        path, seen = os.environ["FAKE_SLACK_EVENTS"], 0
        log("started")

        def say(text=None, blocks=None, thread_ts=None, **kw):
            log("say", text=text, thread_ts=thread_ts)

        while True:
            lines = open(path).read().splitlines() if os.path.exists(path) else []
            for line in lines[seen:]:
                msg = json.loads(line)
                if msg.get("quit"):
                    return
                for h in App.handlers.get("message", []):
                    h(msg["event"], say)
            seen = len(lines)
            time.sleep(0.2)
