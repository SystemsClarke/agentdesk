"""Claude Code hooks that make every session part of John's agent swarm.

  session-start  inject a board briefing: what's ringing for John, what's in flight,
                 recent activity, who owns what, and the swarm's rules
  stop           a session that edited files but never posted to the board is sent
                 back once to say what it did (never loops: stop_hook_active)
  context        after tool calls, measure the session's context from its transcript;
                 at 60% tell the agent, once, to volunteer a Phoenix handoff

Registered in ~/.claude/settings.json. Every mode is fast and fails silent: a hook
must never break a session.
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO))

PHOENIX_PERCENT = 60
CONTEXT_CHECK_EVERY_S = 60
EDIT_TOOLS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}
BOARD_WRITES = {"post_message", "answer_thread", "ask_human", "complete_work", "post_work",
                "pass_the_torch", "request_merge"}

RULES = """How this swarm works (John is the lead; you are one of his agents):
- Be short-lived. Do one clear piece of work, report it, and end. Long sessions cost John tokens.
- The board is how you help each other. Before building, search_messages / list_threads;
  if someone owns it (see bios), answer_thread to coordinate instead of starting in parallel.
- Post what you changed or found to discussion (post_message); durable knowledge goes to the wiki.
- Decisions go to John with ask_human; it reaches his phone. Never ask in the terminal.
- At 60% context, volunteer a handoff: pass_the_torch with a standalone note, then stop.
- Reports with numbers over time: use a ```chart block (formatting_help has the syntax)."""


def _stdin() -> dict:
    try:
        return json.loads(sys.stdin.read() or "{}")
    except ValueError:
        return {}


def session_start() -> None:
    from agentdesk import db, identity, terminal
    conn = db.connect()
    try:
        open_qs = db.open_questions(conn)
        work = db.list_threads(conn, channel="work", limit=100)
        recent = conn.execute(
            "SELECT m.ts, m.author, m.thread_id, t.subject, t.channel FROM messages m"
            " JOIN threads t ON t.id = m.thread_id WHERE NOT (json_valid(m.meta) AND"
            " json_extract(m.meta,'$.kind') IN ('ack','ack-note','read-receipt'))"
            " ORDER BY m.id DESC LIMIT 6").fetchall()
        bios = conn.execute(
            "SELECT t.subject, (SELECT body FROM messages WHERE thread_id=t.id ORDER BY id LIMIT 1) AS body"
            " FROM threads t WHERE t.channel='discussion' AND t.subject LIKE 'bio: %'"
            " ORDER BY t.updated_ts DESC LIMIT 10").fetchall()
    finally:
        conn.close()
    lines = ["AGENTDESK BRIEFING (the board is the swarm's shared memory; read it before you start)"]
    if open_qs:
        lines.append(f"Ringing for John ({len(open_qs)}): " + "; ".join(
            f"#{q['thread_id']} {q['subject'][:70]} ({identity.label(q['opened_by'])})" for q in open_qs[:5]))
    held = [r for r in work if r["status"] == "claimed"]
    ready = [r for r in work if r["status"] == "open"]
    if held:
        lines.append("In flight: " + "; ".join(
            f"#{r['id']} {r['subject'][:60]} ({identity.label(terminal.holder(r)) or '?'})" for r in held[:5]))
    if ready:
        lines.append(f"Up for grabs on Work to Hire ({len(ready)}): " + "; ".join(
            f"#{r['id']} {r['subject'][:60]}" for r in ready[:4]))
    if recent:
        lines.append("Latest on the board: " + "; ".join(
            f"{identity.label(r['author'])} on #{r['thread_id']} {r['subject'][:50]}" for r in recent))
    owners = {}
    for b in bios:  # newest first; an agent with several bio threads is listed once
        owners.setdefault(b["subject"][5:].strip(), ((b["body"] or "").strip().splitlines() or [""])[0][:80])
    if owners:
        lines.append("Who owns what (bios): " + "; ".join(f"{n}: {first}" for n, first in owners.items()))
    lines.append(RULES)
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "SessionStart",
                                             "additionalContext": "\n".join(lines)}}))


def _tool_uses(transcript: str):
    names = []
    try:
        with open(transcript, encoding="utf-8", errors="replace") as f:
            for line in f:
                if '"tool_use"' not in line:
                    continue
                try:
                    msg = json.loads(line).get("message") or {}
                except ValueError:
                    continue
                for part in msg.get("content") or []:
                    if isinstance(part, dict) and part.get("type") == "tool_use":
                        names.append(part.get("name", ""))
    except OSError:
        pass
    return names


def stop() -> None:
    data = _stdin()
    if data.get("stop_hook_active") or not data.get("transcript_path"):
        return
    names = _tool_uses(data["transcript_path"])
    edited = any(n in EDIT_TOOLS for n in names)
    posted = any(n.startswith("mcp__agentdesk__") and n.split("__")[-1] in BOARD_WRITES for n in names)
    if edited and not posted:
        print(json.dumps({"decision": "block", "reason":
            "Before you finish: you changed files this session but haven't told the swarm. Post a short "
            "note to the AgentDesk board (post_message on discussion, or answer_thread on the thread you "
            "worked from): what you changed, what you verified, what's left. Then stop."}))


def _context_tokens(transcript: str):
    """(tokens in the latest turn's prompt, window) from the transcript tail."""
    try:
        size = os.path.getsize(transcript)
        with open(transcript, "rb") as f:
            f.seek(max(0, size - 400_000))
            tail = f.read().decode("utf-8", errors="replace").splitlines()
    except OSError:
        return None
    for line in reversed(tail):
        if '"usage"' not in line:
            continue
        try:
            u = (json.loads(line).get("message") or {}).get("usage") or {}
        except ValueError:
            continue
        tokens = sum(int(u.get(k) or 0) for k in
                     ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"))
        if tokens:
            return tokens, (1_000_000 if tokens > 200_000 else 200_000)
    return None


def context() -> None:
    data = _stdin()
    sid, transcript = data.get("session_id") or "", data.get("transcript_path") or ""
    if not sid or not transcript:
        return
    mark = Path(tempfile.gettempdir()) / f"agentdesk-phoenix-{sid}.json"
    try:
        state = json.loads(mark.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        state = {"checked": 0, "fired": False}
    if state["fired"] or time.time() - state["checked"] < CONTEXT_CHECK_EVERY_S:
        return
    state["checked"] = time.time()
    got = _context_tokens(transcript)
    if got:
        tokens, window = got
        pct = 100 * tokens / window
        if pct >= PHOENIX_PERCENT:
            state["fired"] = True
            print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext":
                f"PHOENIX: this session is at {pct:.0f}% of its context ({tokens:,} tokens). Volunteer a "
                "handoff now: finish the step you're on, call pass_the_torch on the agentdesk server with a "
                "standalone note (what you own, what's done, what's next, any open decision), post one line "
                "to discussion that you've handed off, then stop. A fresh session picks up from your note."}}))
    try:
        mark.write_text(json.dumps(state), encoding="utf-8")
    except OSError:
        pass


if __name__ == "__main__":
    try:
        {"session-start": session_start, "stop": stop, "context": context}[sys.argv[1]]()
    except Exception:
        pass  # a hook must never break a session
