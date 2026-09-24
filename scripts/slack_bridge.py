"""Bridges AgentDesk's open questions <-> a Slack DM with John.

Work item #259 (Work to Hire): John wants ask_human questions to reach him
on Slack so he can answer from his phone, not just via the desktop
toast/window.

Outbound: polls this board's own open_questions() (the same WAITING_SQL view
that drives the toast and the title-bar count) for questions John still owes
a reply to, and posts each as a new Slack thread in his DM. Tracks
thread_id -> Slack thread_ts + the AgentDesk updated_ts we last saw in
bridge_state.json (next to the credential files, not in this repo), so a
restart does not re-post a question already sitting in Slack -- but a
question that goes open -> answered -> asked-again (updated_ts moves past
what we recorded) DOES get reposted, because WAITING_SQL says John is owed a
reply again and a naive "seen once, done forever" table would swallow that
second ask silently.

Inbound: Socket Mode listens for message.im events. A reply posted *inside*
one of those Slack threads resolves back to the AgentDesk thread_id and
answers it exactly the way the desktop app does when John answers from the
window (see App.post_reply in agentdesk/app.py): db.reply(...HUMAN,
HUMAN_KIND...) followed by db.set_thread_status(..., STATUS_ANSWERED) if the
thread was still open. Nothing here writes a status directly or takes a
shortcut around that path.

Run: `python scripts/slack_bridge.py` (or via the "AgentDesk Slack Bridge"
Scheduled Task, which runs this hidden and keeps it alive across reboots).
Needs `slack_bolt` installed and two credential files that are NOT part of
this repo:
  C:\\Users\\palencharj\\.claude\\slack-notify\\bot-token.txt  (xoxb-...)
  C:\\Users\\palencharj\\.claude\\slack-notify\\app-token.txt  (xapp-...)
"""

from __future__ import annotations

import json
import sys
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO))

from agentdesk import db, identity, paths, slackfmt  # noqa: E402

from slack_bolt import App  # noqa: E402
from slack_bolt.adapter.socket_mode import SocketModeHandler  # noqa: E402

CREDS_DIR = Path(r"C:\Users\palencharj\.claude\slack-notify")
BOT_TOKEN = (CREDS_DIR / "bot-token.txt").read_text().strip()
APP_TOKEN = (CREDS_DIR / "app-token.txt").read_text().strip()
JOHN_DM_CHANNEL = "D0C3KDM3DNX"
STATE_FILE = CREDS_DIR / "bridge_state.json"
POLL_SECONDS = 15

app = App(token=BOT_TOKEN)
_state_lock = threading.Lock()


def load_state() -> dict:
    if STATE_FILE.exists():
        return json.loads(STATE_FILE.read_text())
    # thread_id (str) -> {"ts": slack_thread_ts, "updated_ts": AgentDesk
    # updated_ts at the time we posted}.
    return {"posted": {}}


def save_state(state: dict) -> None:
    STATE_FILE.write_text(json.dumps(state, indent=2))


def latest_body(thread_id: int) -> str:
    conn = db.connect()
    try:
        msgs = db.get_thread(conn, thread_id)["messages"]
        return msgs[-1]["body"] if msgs else ""
    finally:
        conn.close()


HEARTBEAT = paths.DATA_DIR / "slack_bridge.state"
_last_relay: dict = {}


def beat() -> None:
    import os
    state = {"pid": os.getpid(), "ts": db.now_iso(), "poll_s": POLL_SECONDS,
             "last_relay": _last_relay or None}
    try:
        tmp = HEARTBEAT.with_suffix(".tmp")
        tmp.write_text(json.dumps(state), encoding="utf-8")
        os.replace(tmp, HEARTBEAT)
    except OSError:
        pass


def poll_loop() -> None:
    while True:
        beat()
        try:
            with _state_lock:
                state = load_state()
                conn = db.connect()
                try:
                    waiting = db.open_questions(conn, include_archived=False)
                finally:
                    conn.close()

                for q in waiting:
                    tid = str(q["thread_id"])
                    prev = state["posted"].get(tid)
                    if prev and prev.get("updated_ts") == q["updated_ts"]:
                        continue
                    body = latest_body(q["thread_id"])
                    asker = identity.label(q.get("opened_by") or "an agent")
                    blocks = ([{"type": "section", "text": {"type": "mrkdwn", "text":
                                f"*☎ Question #{q['thread_id']}* from *{slackfmt._esc(asker)}*\n"
                                f"*{slackfmt._inline(q['subject'])}*"}},
                               {"type": "divider"}]
                              + slackfmt.md_to_slack(body)
                              + [{"type": "context", "elements": [{"type": "mrkdwn", "text":
                                  "Reply *in this thread* to answer. Markdown and Slack formatting both work."}]}])
                    resp = app.client.chat_postMessage(
                        channel=JOHN_DM_CHANNEL, blocks=blocks[:50],
                        text=f"Question #{q['thread_id']} from {asker}: {q['subject']} · "
                             + slackfmt.fallback_text(body, 200))
                    state["posted"][tid] = {
                        "ts": resp["ts"], "updated_ts": q["updated_ts"]}
                    save_state(state)
                    _last_relay.update({"ts": db.now_iso(), "thread_id": q["thread_id"]})
        except Exception as exc:  # a bad pass must not kill future polling
            print(f"[slack_bridge] poll error: {exc}", file=sys.stderr)
        time.sleep(POLL_SECONDS)


@app.event("message")
def handle_reply(event: dict, say) -> None:
    if event.get("channel_type") != "im" or event.get("bot_id"):
        return
    thread_ts = event.get("thread_ts")
    if not thread_ts:
        say("Reply *inside the thread* of the question you're answering "
            '(use "reply in thread"), not as a new message.')
        return

    with _state_lock:
        state = load_state()
        tid = next((int(k) for k, v in state["posted"].items()
                    if v["ts"] == thread_ts), None)
    if tid is None:
        say("I don't recognize that thread as an open question anymore "
            "(already answered, or from before this bridge started).")
        return

    body = slackfmt.slack_to_md(event.get("text", ""))
    conn = db.connect()
    try:
        thread = db.get_thread(conn, tid)["thread"]
        db.reply(conn, tid, paths.HUMAN, paths.HUMAN_KIND, body, meta={"via": "slack"})
        if thread["channel"] == "question" and thread["status"] == paths.STATUS_OPEN:
            db.set_thread_status(conn, tid, paths.STATUS_ANSWERED)
    finally:
        conn.close()

    say(f"Recorded as your answer to #{tid}. Thanks!", thread_ts=thread_ts)


if __name__ == "__main__":
    threading.Thread(target=poll_loop, daemon=True).start()
    SocketModeHandler(app, APP_TOKEN).start()
