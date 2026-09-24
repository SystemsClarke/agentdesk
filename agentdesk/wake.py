"""Wake the agent that asked a question: resume its Claude Code session with John's reply.

Only ever run because John pressed Wake (Ctrl+R on the Questions tab, or "wake" in the
question's Slack thread). Nothing calls this on a timer: resuming a session hands it
an instruction in John's name, so a human decides each one.
"""

from __future__ import annotations

import ctypes
from pathlib import Path

from agentdesk import db, paths


def pid_alive(pid) -> bool:
    if not pid:
        return False
    k = ctypes.windll.kernel32
    h = k.OpenProcess(0x1000, False, int(pid))  # PROCESS_QUERY_LIMITED_INFORMATION
    if not h:
        return False
    code = ctypes.c_ulong()
    ok = k.GetExitCodeProcess(h, ctypes.byref(code))
    k.CloseHandle(h)
    return bool(ok) and code.value == 259  # STILL_ACTIVE


def wake(conn, thread_id: int) -> str:
    """Resume the asking agent's session with John's latest reply. Returns what happened, for the UI."""
    t = conn.execute("SELECT * FROM threads WHERE id=?", (thread_id,)).fetchone()
    if not t:
        return f"#{thread_id} not found"
    msg = conn.execute("SELECT id, body FROM messages WHERE thread_id=? AND author_kind=? ORDER BY id DESC LIMIT 1",
                       (thread_id, paths.HUMAN_KIND)).fetchone()
    if not msg:
        return f"you haven't replied on #{thread_id} yet"
    agent = t["opened_by"]
    s = db.session_for(conn, agent)
    if not s:
        return f"no session on record for {agent}: it asked before sessions were tracked"
    if pid_alive(s["pid"]):
        db.set_delivery(conn, msg["id"], "wake", "stuck", "session still open")
        return f"{agent}'s session is still open; it sees your reply on its next board write"
    from agentdesk import sessions
    if not sessions.claude_exe():
        db.set_delivery(conn, msg["id"], "wake", "failed", "claude CLI not found")
        return "couldn't find the claude CLI"
    cwd = s["cwd"] if s["cwd"] and Path(s["cwd"]).is_dir() else None
    # Through sessions.run: it takes a slot (the Options cap) and the chosen backend.
    sessions.run_detached(agent, db.relay_prompt(thread_id, t["subject"], msg["body"]),
                          resume=s["session_id"], cwd=cwd)
    db.set_delivery(conn, msg["id"], "wake", "resumed", s["session_id"])
    live = len(sessions.status()["live"])
    queued = " (queued: every session slot is busy)" if live >= sessions.max_sessions() else ""
    return f"woke {agent}: resuming its session with your reply{queued}"
