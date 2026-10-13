"""Claude Code hooks and `agentdesk wait`: a reply from John reaches the agent it was meant for."""
import json
import os
import time

from lib import Board, feature, hook, wait_for


def _ask(b, session="sess-1", author="builder"):
    r = b.tool("ask_human", {"subject": "Pick one", "body": "A or B?", "author": author}, session=session)
    return r["thread_id"]


@feature("hook.session-start", "hooks", "SessionStart injects the briefing: ringing questions, work, latest posts, bios, the swarm rules", "Hooks.cs Briefing")
def _start(b):
    _ask(b)
    out = hook(b, "session-start", {"session_id": "fresh"})
    ctx = out["hookSpecificOutput"]["additionalContext"]
    assert out["hookSpecificOutput"]["hookEventName"] == "SessionStart"
    assert "AGENTDESK BRIEFING" in ctx and "Ringing for John (1)" in ctx and "Pick one" in ctx
    assert "How this swarm works" in ctx and "propose_permission_rule" in ctx and "pass_the_torch" in ctx


@feature("hook.context-inject-once", "hooks", "PostToolUse injects John's reply into the session that took part, once", "Hooks.cs Replies")
def _once(b):
    tid = _ask(b)
    assert hook(b, "context", {"session_id": "sess-1"}) is None
    b.need("ui:reply", {"thread_id": tid, "body": "B please"})
    out = hook(b, "context", {"session_id": "sess-1"})
    ctx = out["hookSpecificOutput"]["additionalContext"]
    assert "JOHN REPLIED" in ctx and "B please" in ctx
    assert hook(b, "context", {"session_id": "sess-1"}) is None, "shown once"
    rows = b.sql("SELECT method, state FROM deliveries")
    assert any(r["method"] == "hook" and r["state"] == "injected" for r in rows), rows


@feature("hook.context-only-own-session", "hooks", "A reply is never injected into a session that was not on the thread", "Hooks.cs Replies")
def _own(b):
    tid = _ask(b)
    b.need("ui:reply", {"thread_id": tid, "body": "yes"})
    assert hook(b, "context", {"session_id": "somebody-else"}) is None


@feature("hook.stop-blocks-unread-reply", "hooks", "Stop is blocked until the session has seen John's reply", "Hooks.cs Stop")
def _stop_block(b):
    tid = _ask(b)
    b.need("ui:reply", {"thread_id": tid, "body": "go"})
    out = hook(b, "stop", {"session_id": "sess-1"})
    assert out["decision"] == "block" and "go" in out["reason"]
    assert hook(b, "stop", {"session_id": "sess-1"}) is None, "once seen it may stop"


def _transcript(b, tool_names):
    p = os.path.join(b.work, "t.jsonl")
    with open(p, "w") as f:
        for n in tool_names:
            f.write(json.dumps({"message": {"content": [{"type": "tool_use", "name": n, "input": {}}]}}) + "\n")
    return p


@feature("hook.stop-needs-board-post-after-edit", "hooks", "Stop asks for a board note when files were edited and nothing was posted; not twice", "Hooks.cs Stop/Scan")
def _stop_edit(b):
    t = _transcript(b, ["Edit"])
    out = hook(b, "stop", {"session_id": "x", "transcript_path": t})
    assert out["decision"] == "block" and "haven't told the swarm" in out["reason"]
    assert hook(b, "stop", {"session_id": "x", "transcript_path": t, "stop_hook_active": True}) is None
    t2 = _transcript(b, ["Edit", "mcp__agentdesk__post_message"])
    assert hook(b, "stop", {"session_id": "x", "transcript_path": t2}) is None


@feature("hook.phoenix-at-60-percent", "hooks", "At 60% of the context window the session is told to hand off, once", "Hooks.cs Phoenix, phoenix.md")
def _phoenix(b):
    p = os.path.join(b.work, "big.jsonl")
    with open(p, "w") as f:
        f.write(json.dumps({"message": {"model": "claude-sonnet-5-5", "usage": {"input_tokens": 700000, "output_tokens": 10}}}) + "\n")
    out = hook(b, "context", {"session_id": "phx", "transcript_path": p})
    assert out and "PHOENIX" in out["hookSpecificOutput"]["additionalContext"] and "70%" in out["hookSpecificOutput"]["additionalContext"]
    assert hook(b, "context", {"session_id": "phx", "transcript_path": p}) is None, "once per session"
    small = os.path.join(b.work, "small.jsonl")
    with open(small, "w") as f:
        f.write(json.dumps({"message": {"model": "claude-opus-4", "usage": {"input_tokens": 50000}}}) + "\n")
    assert hook(b, "context", {"session_id": "other", "transcript_path": small}) is None


@feature("hook.pr-notice", "hooks", "A merged/closed PR notice on a thread the session took part in is injected as GitHub's, not John's", "Hooks.cs Replies")
def _prnotice(b):
    tid = _ask(b)
    now = time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime())
    b.sql("INSERT INTO messages (ts, thread_id, author, author_kind, body, meta) VALUES (?,?,?,?,?,?)",
          (now, tid, "agentdesk", "agent", "Merged: o/r#9 was merged.", json.dumps({"kind": "pr-merged"})))
    out = hook(b, "context", {"session_id": "sess-1"})
    ctx = out["hookSpecificOutput"]["additionalContext"]
    assert "PULL REQUEST UPDATE" in ctx and "not John's words" in ctx


@feature("hook.never-fails", "hooks", "A hook never breaks a session: garbage on stdin, an unknown event, an empty board", "Program.cs hook")
def _never(b):
    for ev, stdin in (("context", "{{{ not json"), ("stop", ""), ("nonsense", "{}"), ("session-start", "{}")):
        r = b.run("hook", ev, input=stdin)
        assert r.returncode == 0, (ev, r.stderr[:200])


@feature("wait.returns-reply", "hooks", "`agentdesk wait <thread>` blocks until John replies, prints his words, records a 'woke' delivery", "Hooks.cs Wait")
def _wait(b):
    tid = _ask(b)
    p = b.spawn("wait", str(tid))
    time.sleep(0.5)
    b.need("ui:reply", {"thread_id": tid, "body": "ship it"})
    out, _ = p.communicate(timeout=15)
    assert "John replied on thread #%d" % tid in out and "ship it" in out
    assert any(r["method"] == "watcher" and r["state"] == "woke" for r in b.sql("SELECT * FROM deliveries"))


@feature("ask.wake-command", "hooks", "ask_human returns a wake_on_reply command that runs `wait <thread>` in the background", "AgentBoard.AskHuman")
def _wakecmd(b):
    r = b.tool("ask_human", {"subject": "q", "body": "ok?", "author": "builder"})
    assert r["wake_on_reply"]["run_in_background"] is True
    assert r["wake_on_reply"]["command"].rstrip().endswith("wait %d" % r["thread_id"])
