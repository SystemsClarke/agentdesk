"""Runs AgentDesk's real Python MCP tools against a scratch board and records every answer.

    python driver.py spec.json out.json      (LOCALAPPDATA must already point inside spec["scratch"])

The clock is frozen to a counter (one second per now_iso() call) and the vault plugins are faked, so the
C# side, given the same starting database, the same counter and the same fakes, must produce identical
documents and identical rows.
"""
import json
import os
import sys
from datetime import datetime, timedelta, timezone

spec = json.load(open(sys.argv[1], encoding="utf-8"))
assert os.environ["LOCALAPPDATA"].startswith(spec["scratch"]), "refusing to run outside the scratch dir"
for key in ("AGENTDESK_SESSION", "AGENTDESK_AUTHOR", "AGENTDESK_HARNESS", "AGENTDESK_PROJECT",
            "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AI_AGENT", "CLAUDE_CODE_SESSION_ID"):
    os.environ.pop(key, None)
sys.path.insert(0, spec["repo"])

from agentdesk import paths  # noqa: E402
paths.ensure_dirs = lambda: paths.DATA_DIR.mkdir(parents=True, exist_ok=True)  # never touch the real vault
from agentdesk import db, mcp_server, notify, vault, vault_search  # noqa: E402

tick = [spec["tick"]]


def now_iso():
    t = datetime(2026, 1, 1, tzinfo=timezone.utc) + timedelta(seconds=tick[0])
    tick[0] += 1
    return t.isoformat(timespec="seconds")


def fake_mirror(conn, tid):
    subject = conn.execute("SELECT subject FROM threads WHERE id=?", (tid,)).fetchone()[0]
    if "FAIL" in subject:
        raise RuntimeError("vault down: " + subject)
    return {"thread_id": tid, "status": "mirrored", "note": f"notes/{subject}.md", "reasons": []}


def fake_search(query, k=10):
    if query == "offline":
        raise vault_search.VaultSearchUnavailable("Ollama is not reachable")
    return [{"path": f"notes/{query}-{i}.md", "score": round(1 / (i + 1), 4), "type": "note",
             "summary": f"hit {i} for {query} ✓"} for i in range(min(k, 3))]


db.now_iso = now_iso
notify.log_line = lambda message: None  # the real one writes a timestamp, which would consume a tick
vault.mirror_thread = fake_mirror
vault_search.search = fake_search
vault_search.read_note = lambda path: f"body of {path}"

cwds = {}


def use(name):
    c = spec["callers"][name]
    for key, val in (("CLAUDE_CODE_SESSION_ID", c.get("session")), ("AGENTDESK_AUTHOR", c.get("env_author")),
                     ("AGENTDESK_HARNESS", c.get("harness"))):
        if val is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = val
    mcp_server.SESSION_ID = c.get("session") or ""
    os.makedirs(c["cwd"], exist_ok=True)
    os.chdir(c["cwd"])
    cwds[name] = os.getcwd()


def john_reply(tid, body):
    """What the window does when John answers: agentdesk/app.py post_reply, then the watcher queues the ack."""
    conn = db.connect()
    try:
        thread = db.get_thread(conn, tid)["thread"]
        mid = db.reply(conn, tid, paths.HUMAN, paths.HUMAN_KIND, body)
        if thread["channel"] == "question" and thread["status"] == paths.STATUS_OPEN:
            db.set_thread_status(conn, tid, paths.STATUS_ANSWERED)
        db.queue_ack_for_message(conn, mid)
    finally:
        conn.close()


if spec.get("init"):
    conn = db.connect()
    db.init_db(conn)
    conn.close()

results = []
for step in spec["steps"]:
    op = step["op"]
    try:
        if op == "john_reply":
            john_reply(step["args"]["thread_id"], step["args"]["body"])
            results.append(None)
        elif op == "torch_due":
            conn = db.connect()
            db.set_torch_due(conn, step["args"]["name"], True)
            conn.close()
            results.append(None)
        else:
            use(step["caller"])
            results.append({"text": getattr(mcp_server, op)(**step.get("args", {}))})
    except Exception as exc:  # recorded, so a Python bug shows up as data rather than a crash
        results.append({"exception": type(exc).__name__})

json.dump({"pid": os.getpid(), "cwds": cwds, "results": results,
           "wait": [sys.executable, os.path.join(os.path.dirname(mcp_server.__file__), "wait.py")]},
          open(sys.argv[2], "w", encoding="utf-8"), ensure_ascii=False)
