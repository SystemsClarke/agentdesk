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
window (ui:reply, BoardStore.cs in the C# core): db.reply(...HUMAN,
HUMAN_KIND...) followed by db.set_thread_status(..., STATUS_ANSWERED) if the
thread was still open. Nothing here writes a status directly or takes a
shortcut around that path.

Commands: a top-level DM from John such as `status`, `agents` or `concierge off` runs a
phone command against the core's pipe (agentdesk/slackcmd.py; `help` lists them).
Several bots: bots.json next to the credentials lists Slack bots, each with its own
credential folder and areas; with no bots.json there is one bot with every area.

Swarm slots: the bot with the `swarms` area runs `swarms` / `swarm new|approve|end|reset`
(agentdesk/swarm.py), relays John's messages in a slot's channel to that goal's lead, and posts
each goal's board-thread activity into its channel as the persona. The Slack scopes and events
it needs: docs/ui-api.md, "Swarm slots".

Run: the AgentDesk core starts this with itself and restarts it whenever it
exits (src/AgentDesk.Core/Host/Supervisor.cs); settings.json's slack_bridge: false
turns that off. A scheduled task for it hung
silently on this machine, so there deliberately is none. By hand:
`python scripts/slack_bridge.py`.
Needs `slack_bolt` installed and two credential files that are NOT part of
this repo:
  C:\\Users\\palencharj\\.claude\\slack-notify\\bot-token.txt  (xoxb-...)
  C:\\Users\\palencharj\\.claude\\slack-notify\\app-token.txt  (xapp-...)
"""

from __future__ import annotations

import json
import os
import sys
import threading
import time
from pathlib import Path

if sys.stdout is None or sys.stderr is None:  # pythonw (the scheduled task): no console, so log to a file
    _log = Path(os.environ.get("LOCALAPPDATA", Path.home())) / "AgentDesk" / "slack_bridge.log"
    _log.parent.mkdir(parents=True, exist_ok=True)
    sys.stdout = sys.stderr = open(_log, "a", encoding="utf-8", buffering=1)

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO))

from agentdesk import db, identity, paths, slackcmd, slackfmt, swarm  # noqa: E402

from slack_bolt import App  # noqa: E402
from slack_bolt.adapter.socket_mode import SocketModeHandler  # noqa: E402

CREDS_DIR = Path(r"C:\Users\palencharj\.claude\slack-notify")
JOHN_DM_CHANNEL = "D0C3KDM3DNX"
JOHN_USER = "U0C4L06N4KA"  # only his messages in his DM are answers
STATE_FILE = CREDS_DIR / "bridge_state.json"
POLL_SECONDS = 15

app, BOT_TOKEN = None, ""  # the board bot's, with JOHN_DM_CHANNEL: set in __main__ from bots.json
_state_lock = threading.Lock()


def load_state() -> dict:
    if STATE_FILE.exists():
        return json.loads(STATE_FILE.read_text())
    # thread_id (str) -> {"ts": slack_thread_ts, "updated_ts": AgentDesk
    # updated_ts at the time we posted}.
    return {"posted": {}}


def save_state(state: dict) -> None:
    import os
    tmp = STATE_FILE.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, indent=2))
    os.replace(tmp, STATE_FILE)  # a crash mid-write must not lose every mapping


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


def _post(q: dict, prev: dict | None) -> str:
    """Post a question; a follow-up on one already in Slack goes into John's existing thread."""
    body = latest_body(q["thread_id"])
    asker = identity.label(q.get("opened_by") or "an agent")
    body_blocks, images = slackfmt.md_to_slack(body)
    head = (f"*☎ Question #{q['thread_id']}* from *{slackfmt._esc(asker)}*\n*{slackfmt._inline(q['subject'])}*"
            if not prev else f"*↩ Follow-up on #{q['thread_id']}* from *{slackfmt._esc(asker)}*")
    blocks = ([{"type": "section", "text": {"type": "mrkdwn", "text": head}}, {"type": "divider"}]
              + body_blocks
              + [{"type": "context", "elements": [{"type": "mrkdwn", "text":
                  "Reply *in this thread* to answer. Photos work too."}]}])
    extra = {"thread_ts": prev["ts"], "reply_broadcast": True} if prev else {}
    resp = app.client.chat_postMessage(
        channel=JOHN_DM_CHANNEL, blocks=blocks[:50], unfurl_links=False, unfurl_media=False,
        text=f"Question #{q['thread_id']} from {asker}: {q['subject']} · " + slackfmt.fallback_text(body, 200),
        **extra)
    thread_ts = prev["ts"] if prev else resp["ts"]
    for png, filename, title in images:
        try:
            app.client.files_upload_v2(channel=JOHN_DM_CHANNEL, thread_ts=thread_ts,
                                       content=png, filename=filename, title=title)
        except Exception as exc:  # the question is already in Slack; a lost chart must not re-post it
            print(f"[slack_bridge] chart upload failed: {exc}", file=sys.stderr)
    return thread_ts


DELIVERY_NOTES = {
    "picked-up": "✓ The agent picked up your reply.",
    "woke": "✓ Your reply woke the agent; it's working on it.",
    "resumed": "✓ Woke the agent: resumed its session with your reply.",
    "late": "⚠ The agent hasn't picked up your reply after 15 min. Reply *wake* here to resume its session.",
}


def _tell_deliveries(state: dict) -> None:
    """In each question's Slack thread, say once whether John's latest reply reached the agent."""
    told = state.setdefault("told", {})
    conn = db.connect()
    try:
        for tid, info in list(state["posted"].items()):
            h = conn.execute("SELECT id, ts FROM messages WHERE thread_id=? AND author_kind=? ORDER BY id DESC LIMIT 1",
                             (int(tid), paths.HUMAN_KIND)).fetchone()
            if not h or (time.time() - _epoch(h["ts"])) > 86400:
                continue
            status = db.delivery_status(conn, h["id"])
            if status == "pending" and time.time() - _epoch(h["ts"]) > 15 * 60:
                status = "late"
            key = f"{h['id']}:{status}"
            if status not in DELIVERY_NOTES or told.get(tid) == key:
                continue
            app.client.chat_postMessage(channel=JOHN_DM_CHANNEL, thread_ts=info["ts"], text=DELIVERY_NOTES[status])
            with _state_lock:
                told[tid] = key
                save_state(state)
    finally:
        conn.close()


def _epoch(iso: str) -> float:
    from datetime import datetime
    try:
        return datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return 0.0


def poll_loop() -> None:
    while True:
        beat()
        try:
            if app is None:  # no bot has the board area: heartbeat only, so the app keeps the bridge alive
                time.sleep(POLL_SECONDS)
                continue
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
                ts = _post(q, prev)
                with _state_lock:  # saved before anything else can fail, so a pass never re-posts
                    state["posted"][tid] = {"ts": ts, "updated_ts": q["updated_ts"]}
                    save_state(state)
                _last_relay.update({"ts": db.now_iso(), "thread_id": q["thread_id"]})
            _tell_deliveries(state)
        except Exception as exc:  # a bad pass must not kill future polling
            print(f"[slack_bridge] poll error: {exc}", file=sys.stderr)
        time.sleep(POLL_SECONDS)


swarm_bots: list = []  # agentdesk/swarm.Swarms, one per bot with the swarms area


def swarm_loop() -> None:
    """Relays each swarm goal's new board-thread posts into its slot's channel, as the persona."""
    while True:
        for s in swarm_bots:
            try:
                s.relay()
            except Exception as exc:  # a bad pass must not kill future relaying
                print(f"[slack_bridge] swarm relay error: {exc}", file=sys.stderr)
        time.sleep(POLL_SECONDS)


ATTACHMENTS = paths.DATA_DIR / "attachments"


def save_photos(files: list, thread_ts: str) -> list:
    """Download images attached to a Slack reply into LOCALAPPDATA; returns their paths."""
    import urllib.request
    saved = []
    for f in files:
        if not str(f.get("mimetype", "")).startswith("image/"):
            continue
        url = f.get("url_private_download") or f.get("url_private")
        name = "".join(c for c in (f.get("name") or "photo.jpg") if c.isalnum() or c in "._-") or "photo.jpg"
        dest = ATTACHMENTS / f"{thread_ts.replace('.', '_')}_{f.get('id', '')}_{name}"
        try:
            ATTACHMENTS.mkdir(parents=True, exist_ok=True)
            req = urllib.request.Request(url, headers={"Authorization": f"Bearer {BOT_TOKEN}"})
            with urllib.request.urlopen(req, timeout=30) as r:
                data = r.read()
            if data[:1] == b"<":
                continue  # an HTML sign-in page, not the image: the files:read scope is missing
            dest.write_bytes(data)
            saved.append(dest)
        except Exception as exc:
            print(f"[slack_bridge] photo download failed: {exc}", file=sys.stderr)
    return saved


def handle_reply(event: dict, say) -> None:
    if (event.get("channel") != JOHN_DM_CHANNEL or event.get("user") != JOHN_USER or event.get("bot_id")
            or event.get("subtype") not in (None, "file_share")):
        return  # edits, deletes and anyone else's messages are not answers
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

    if (event.get("text") or "").strip().lower().rstrip("!.") in ("wake", "wake it", "wake up"):
        try:
            r = slackcmd.call("ui:wake", {"thread_id": tid})  # the core wakes it (Identities.Wake), as Ctrl+R does
            said = r.get("said") or r.get("error") or "no answer from the core"
        except OSError as exc:
            said = f"can't reach the core ({exc})"
        say(said[0].upper() + said[1:] + ".", thread_ts=thread_ts)
        return
    body = slackfmt.slack_to_md(event.get("text", ""))
    photos = save_photos(event.get("files") or [], thread_ts)
    if photos:
        body = (body + "\n\n" if body.strip() else "") + "\n".join(f"![photo]({p.as_uri()})" for p in photos)
    if not body.strip():
        return
    conn = db.connect()
    try:
        thread = db.get_thread(conn, tid)["thread"]
        db.reply(conn, tid, paths.HUMAN, paths.HUMAN_KIND, body, meta={"via": "slack"})
        if thread["channel"] == "question" and thread["status"] == paths.STATUS_OPEN:
            db.set_thread_status(conn, tid, paths.STATUS_ANSWERED)
    finally:
        conn.close()

    say(f"Recorded as your answer to #{tid}. Thanks!", thread_ts=thread_ts)


def make_app(bot: dict) -> App:
    """One Slack bot: its areas' commands (top-level DM messages from John), plus the question relay if it has board."""
    a = App(token=(bot["creds"] / "bot-token.txt").read_text().strip())
    cmds = slackcmd.Commands(bot["areas"], JOHN_USER, bot["name"])
    if "swarms" in bot["areas"]:
        cmds.swarm = swarm.Swarms(a.client, slackcmd.call, JOHN_USER, paths.DATA_DIR / "swarm_state.json")
        swarm_bots.append(cmds.swarm)

    @a.event("message")
    def on_message(event: dict, say) -> None:
        if event.get("channel_type") == "channel" and getattr(cmds, "swarm", None):
            try:
                cmds.swarm.route(event)  # John in a swarm slot's channel: to the goal's lead
            except Exception as exc:
                print(f"[slack_bridge] swarm route error: {exc}", file=sys.stderr)
            return
        if event.get("bot_id") or event.get("subtype") not in (None, "file_share") or event.get("channel_type") != "im":
            return
        if not event.get("thread_ts") and (out := cmds.handle(event.get("text") or "", event.get("user"))) is not None:
            blocks, _ = slackfmt.md_to_slack(out)
            say(blocks=blocks[:50], text=slackfmt.fallback_text(out, 200))
        elif "board" in bot["areas"]:
            handle_reply(event, say)
    return a


if __name__ == "__main__":
    handlers = []
    for bot in slackcmd.load_bots(CREDS_DIR / "bots.json", CREDS_DIR, JOHN_DM_CHANNEL):
        a = make_app(bot)
        if "board" in bot["areas"]:
            app, BOT_TOKEN, JOHN_DM_CHANNEL = a, a.client.token, bot["dm"]
        handlers.append(SocketModeHandler(a, (bot["creds"] / "app-token.txt").read_text().strip()))
    threading.Thread(target=poll_loop, daemon=True).start()
    threading.Thread(target=swarm_loop, daemon=True).start()
    for h in handlers[:-1]:
        h.connect()  # returns once connected; the last one blocks
    handlers[-1].start()
