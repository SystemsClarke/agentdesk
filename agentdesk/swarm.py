"""Swarm slots on Slack (docs/GOAL.md, milestone 5): 10 reusable channels, each speaking as one goal's persona.

The core stores slot -> channel, persona and goal (ui:slot_*); this module makes every Slack call and keeps that mapping
current. John's commands (`swarms`, `swarm new|approve|end|reset`, agentdesk/slackcmd.py) land here, John's messages in a
slot's channel are typed into the goal's lead, and each new post on the goal's board thread is relayed into the channel:
verdicts and proposals top level, member notes in a thread under one "activity" message a day.

Persona posts use chat.postMessage's username/icon overrides, which need the chat:write.customize scope. Without it Slack
says missing_scope; then every post is a plain one prefixed with the persona's name, and that is logged once.
"""

from __future__ import annotations

import json
import os
import re
import sys
import time
from datetime import date
from pathlib import Path

from agentdesk import slackfmt

SLOTS = 10
ICONS = (":large_blue_circle:", ":large_green_circle:", ":large_orange_circle:", ":large_purple_circle:", ":red_circle:",
         ":large_yellow_circle:", ":large_brown_circle:", ":white_circle:", ":black_circle:", ":large_blue_diamond:")
PROPOSING = "Proposing a hypothesis…"
GONE = {"channel_not_found", "is_archived", "not_in_channel"}  # a post that can never land: skip it, don't retry forever
RECEIPTS = {"ack", "ack-note", "read-receipt"}
NAME = re.compile(r"^[A-Za-z][A-Za-z0-9-]{0,31}$")


def slack_error(exc: Exception) -> str | None:
    """Slack's error code from a slack_sdk SlackApiError (or a fake with the same .response), else None."""
    resp = getattr(exc, "response", None)
    try:
        return resp["error"] if resp is not None else None
    except (KeyError, TypeError):
        return None


def _err(r) -> str | None:
    return r["error"] if isinstance(r, dict) and r.get("error") else None


class Swarms:
    """One Slack bot's swarm slots. `client` is a slack_sdk WebClient; `call` is slackcmd.call (the core's pipe)."""

    def __init__(self, client, call, john: str, state_path: Path | None = None, log=None, sleep=time.sleep, today=date.today):
        self.client, self.call, self.john, self.sleep, self.today = client, call, john, sleep, today
        self.state_path = state_path
        self.log = log or (lambda s: print(f"[swarm] {s}", file=sys.stderr, flush=True))
        self.customize = True  # False once Slack says the persona override needs chat:write.customize
        self.state = {"goals": {}}
        if state_path and state_path.exists():
            try:
                self.state = json.loads(state_path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                pass
        self.state.setdefault("goals", {})

    # ---- plumbing

    def _save(self) -> None:
        if not self.state_path:
            return
        tmp = self.state_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self.state, indent=2), encoding="utf-8")
        os.replace(tmp, self.state_path)

    def slots(self) -> list[dict]:
        r = self.call("ui:slot_list")
        if e := _err(r):
            raise RuntimeError(e)
        return r["slots"]

    def slot(self, n: int) -> dict:
        return next(s for s in self.slots() if s["n"] == n)

    def post(self, slot: dict, md: str, thread_ts: str | None = None) -> str:
        """Posts Markdown into the slot's channel as its persona; returns the message ts."""
        persona = slot.get("persona_name") or "swarm"
        icon = slot.get("persona_icon") or ICONS[(slot["n"] - 1) % SLOTS]
        if not self.customize:
            md = f"**{persona}:** {md}"
        blocks, _ = slackfmt.md_to_slack(md)
        kw = {"channel": slot["channel_id"], "text": slackfmt.fallback_text(md.replace("**", ""), 200), "blocks": blocks[:50],
              "unfurl_links": False, "unfurl_media": False}
        if thread_ts:
            kw["thread_ts"] = thread_ts
        if self.customize:
            kw["username"] = persona
            kw["icon_url" if icon.startswith("http") else "icon_emoji"] = icon
        try:
            return self.client.chat_postMessage(**kw)["ts"]
        except Exception as exc:
            if not self.customize or slack_error(exc) != "missing_scope":
                raise
            self.customize = False
            self.log("chat:write.customize is missing: posting as the bot, each post prefixed with the persona's name. "
                     "Add the scope to the Slack app and reinstall it.")
            return self.post(slot, md, thread_ts)

    def _channel(self, slot: dict, name: str, fresh: bool = False) -> tuple[str, str]:
        """The slot's channel renamed to swarm-<name> (or a new one when it has none, or `fresh`), with John invited."""
        want = f"swarm-{name.lower()}"
        ch = None
        if slot.get("channel_id") and not fresh:
            try:
                ch = self.client.conversations_rename(channel=slot["channel_id"], name=want)["channel"]
            except Exception as exc:
                if slack_error(exc) not in ("channel_not_found", "is_archived"):
                    raise
        if ch is None:
            ch = self.client.conversations_create(name=want, is_private=False)["channel"]
        try:
            self.client.conversations_invite(channel=ch["id"], users=self.john)
        except Exception as exc:
            if slack_error(exc) not in ("already_in_channel", "cant_invite_self"):
                raise
        return ch["id"], ch["name"]

    def _start(self, n: int, slot: dict, name: str, objective: str, folder: str, fresh: bool) -> str:
        """goal_create, then the slot's channel and persona, then the persona's first post. Returns John's reply."""
        made = self.call("ui:goal_create", {"name": name, "objective": objective, "folder": folder})
        if e := _err(made):
            return f"⚠ {e}"
        icon = ICONS[n - 1]
        try:
            cid, cname = self._channel(slot, name, fresh)
        except Exception as exc:
            self.call("ui:slot_assign", {"n": n, "goal": name, "persona": name, "persona_icon": icon, **({"channel_id": ""} if fresh else {})})
            return f"⚠ Goal *{name}* created in slot {n}, but Slack refused the channel: {slack_error(exc) or exc}."
        row = self.call("ui:slot_assign", {"n": n, "goal": name, "persona": name, "persona_icon": icon, "channel_id": cid, "channel_name": cname})
        if e := _err(row):
            return f"⚠ {e}"
        self.post(row, PROPOSING)
        return f"Slot {n}: *{name}* in <#{cid}>. Its lead is proposing a hypothesis; approve with `swarm approve {n}`."

    def _goal_slot(self, n: int) -> tuple[dict | None, str | None]:
        if not 1 <= n <= SLOTS:
            return None, f"A slot is 1 to {SLOTS}."
        s = self.slot(n)
        return (s, None) if s.get("goal") else (None, f"Slot {n} is free.")

    # ---- John's commands

    def list(self) -> str:
        lines = []
        for s in self.slots():
            ch = f" · <#{s['channel_id']}>" if s.get("channel_id") else ""
            if not s.get("goal"):
                lines.append(f"*{s['n']}* · free{ch}")
                continue
            last = s.get("last_value")
            lines.append(f"*{s['n']}* · {s.get('persona_name') or s['goal']} · {s.get('state') or '?'} · last "
                         f"{'—' if last is None else f'{last:g}'} · {s.get('members') or 0} member(s){ch}")
        return "\n".join(lines)

    def new(self, name: str, folder: str, objective: str) -> str:
        if not NAME.match(name):
            return "⚠ A swarm name is letters, digits and dashes (at most 32), starting with a letter."
        free = [s for s in self.slots() if not s.get("goal")]
        if not free:
            return f"⚠ All {SLOTS} slots are busy: `swarm end <slot>` one first."
        slot = min(free, key=lambda s: (not s.get("channel_id"), s["n"]))  # reuse a channel before making one
        return self._start(slot["n"], slot, name, objective, folder, fresh=False)

    def approve(self, n: int, opts: dict | None = None) -> str:
        s, why = self._goal_slot(n)
        if why:
            return why
        r = self.call("ui:goal_approve", {"name": s["goal"], **(opts or {})})
        if e := _err(r):
            return f"⚠ {e}"
        self.relay_slot(s)
        return (f"Approved *{s['goal']}*: {r.get('max_members')} members, {r.get('max_hours')} h, "
                f"the lead wakes every {r.get('cadence_minutes')} min.")

    def end(self, n: int) -> str:
        s, why = self._goal_slot(n)
        if why:
            return why
        r = self.call("ui:goal_stop", {"name": s["goal"]})
        if e := _err(r):
            return f"⚠ {e}"
        self.relay_slot(s)  # the core's "Goal stopped" note, before the slot forgets the goal
        self.call("ui:slot_clear", {"n": n})
        self.state["goals"].pop(s["goal"], None)
        self._save()
        return f"Ended *{s['goal']}* ({r.get('state')}); slot {n} is free and keeps its channel."

    def reset(self, n: int) -> str:
        s, why = self._goal_slot(n)
        if why:
            return why
        old = s["goal"]
        st = self.call("ui:goal_status", {"name": old})
        if e := _err(st) or _err(self.call("ui:goal_stop", {"name": old})):
            return f"⚠ {e}"
        self.call("ui:identity_forget", {"name": st.get("lead") or f"{old}-lead"})  # the chatbot starts over as a new identity
        if s.get("channel_id"):
            try:
                self.client.conversations_archive(channel=s["channel_id"])
            except Exception as exc:
                if slack_error(exc) != "already_archived":
                    self.log(f"slot {n}: archiving {s['channel_id']} failed: {slack_error(exc) or exc}")
        self.state["goals"].pop(old, None)
        self._save()
        base, k = re.sub(r"-\d+$", "", old), 2
        while not _err(self.call("ui:goal_status", {"name": f"{base}-{k}"})):  # goal names are never reused
            k += 1
        return self._start(n, s, f"{base}-{k}", st["objective"], st["measure_folder"], fresh=True)

    # ---- inbound: John in a slot's channel

    def route(self, event: dict) -> bool:
        """A channel message: if it is in a slot's channel, John's words go to the goal's lead. True when it was a slot's."""
        s = next((x for x in self.slots() if x.get("channel_id") and x["channel_id"] == event.get("channel")), None)
        if s is None:
            return False
        if event.get("user") != self.john or event.get("bot_id") or event.get("subtype") not in (None, "file_share"):
            return True  # only John steers a swarm; bots (the persona itself) and edits are not input
        text = slackfmt.slack_to_md(event.get("text") or "").strip()
        if not text:
            return True
        if not s.get("goal"):
            self.post(s, f"Slot {s['n']} is free. Start a swarm with `swarm new <name> <folder> <objective>`.", event.get("ts"))
            return True
        lead = s.get("lead") or f"{s['goal']}-lead"
        r = self.call("ui:input", {"name": lead, "data": "John (via Slack): " + text.replace("\n", " / ")})
        if e := _err(r):
            self.post(s, f"⚠ Couldn't reach {lead} (goal {s.get('state')}): {e}", event.get("ts"))
            return True
        self.sleep(0.3)  # text and Enter in one write read as a paste
        self.call("ui:input", {"name": lead, "data": "\r"})
        return True

    # ---- outbound: the goal's board thread into the channel

    def relay(self) -> int:
        """One pass over every slot with a goal; returns how many posts went out."""
        return sum(self.relay_slot(s) for s in self.slots() if s.get("goal"))

    def relay_slot(self, s: dict) -> int:
        if not (s.get("channel_id") and s.get("thread_id")):
            return 0
        doc = self.call("ui:thread", {"thread_id": s["thread_id"]})
        msgs = [] if _err(doc) else doc.get("messages", [])
        st = self.state["goals"].setdefault(s["goal"], {})
        if "last" not in st:
            st["last"] = msgs[0]["id"] if msgs else 0  # the thread's opener says what "swarm new" already said
        sent = 0
        for m in msgs:
            if m["id"] <= st["last"]:
                continue
            try:
                if not self._skip(m):
                    top = m.get("author") in ("agentdesk", s.get("lead"))
                    if top:
                        self.post(s, m["body"])
                    else:
                        self.post(s, f"**{m.get('author')}:** {m['body']}", self._activity(s, st))
                    sent += 1
            except Exception as exc:
                code = slack_error(exc)
                self.log(f"slot {s['n']}: relaying message {m['id']} failed: {code or exc}")
                if code not in GONE:
                    break  # try again next pass
            st["last"] = m["id"]
            self._save()
        return sent

    @staticmethod
    def _skip(m: dict) -> bool:
        meta = m.get("meta")
        if isinstance(meta, str):
            try:
                meta = json.loads(meta)
            except ValueError:
                meta = None
        kind = meta.get("kind") if isinstance(meta, dict) else None
        return kind in RECEIPTS or m.get("author_kind") == "human"  # receipts are noise; John knows what he wrote

    def _activity(self, s: dict, st: dict) -> str:
        """The ts of today's "activity" message in the slot's channel, posted on first use."""
        day = self.today().isoformat()
        a = st.get("activity") or {}
        if a.get("day") != day or a.get("channel") != s["channel_id"]:
            a = {"day": day, "channel": s["channel_id"], "ts": self.post(s, f"Activity, {day}: member notes in this thread.")}
            st["activity"] = a
            self._save()
        return a["ts"]
