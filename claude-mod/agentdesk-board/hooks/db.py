"""The only thing the agentdesk-board mod runs: it reads and writes AgentDesk's SQLite file, nothing else.

No pipe, no daemon, no HTTP, none of AgentDesk's own binaries. Python's standard-library sqlite3 opens
%LOCALAPPDATA%\\AgentDesk\\agentdesk.db ($AGENTDESK_DATA, or $AGENTDESK_DB for the file itself). Reads open
it read-only. Every write mirrors the Rust board (src/db.rs, src/events.rs, src/permissions.rs) row for row,
event-log entry included, in one transaction.

Output is one JSON document on stdout; a failure is {"error": "..."} and exit code 1.

  db.py tab <questions|discussion|work|goals|prs|wiki>   one tab's rows, plus how many questions wait on John
  db.py thread <id>                                      a thread, its messages, and its permission proposal if any
  db.py reply <id>                                       John replies; the body is stdin   (Db::john_replies)
  db.py close <id>                                       John closes a question unanswered (Db::close_question)
  db.py permission <approve|reject> <thread_id>          John decides a permission rule    (permissions::decide)
"""
import json
import os
import random
import re
import sqlite3
import sys
import time
from datetime import datetime, timezone

DATA = os.environ.get("AGENTDESK_DATA") or os.path.join(os.environ.get("LOCALAPPDATA", "."), "AgentDesk")
DB = os.environ.get("AGENTDESK_DB") or os.path.join(DATA, "agentdesk.db")
# paths::claude_settings is ~/.claude/settings.json. AGENTDESK_TEST_SETTINGS (a folder) sends every settings write,
# user and project rules alike, under it instead: what the tests use, so they never touch a real settings file.
TEST_SETTINGS = os.environ.get("AGENTDESK_TEST_SETTINGS")
CLAUDE_SETTINGS = (os.path.join(TEST_SETTINGS, "user", "settings.json") if TEST_SETTINGS
                   else os.path.join(os.path.expanduser("~"), ".claude", "settings.json"))
JOHN, HUMAN, AGENT = "john", "human", "agent"
RECEIPTS = ("ack", "ack-note", "read-receipt")

# db.rs WAITING: an open question whose newest non-receipt message is not John's.
WAITING = """t.channel = 'question' AND t.status = 'open' AND NOT (COALESCE((
    SELECT m.author_kind FROM messages m WHERE m.thread_id = t.id
      AND COALESCE(json_extract(m.meta, '$.kind'), '') NOT IN ('ack', 'ack-note', 'read-receipt')
    ORDER BY m.id DESC LIMIT 1), 'human') = 'human')"""


# ---- time, ids and the event log (src/events.rs)

def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S+00:00")


def dumps(v):
    """db::py_dumps: Python's json.dumps spacing, ', ' and ': '."""
    return json.dumps(v, ensure_ascii=False)


def uuid7(hlc):
    ms, r = hlc >> 16, random.getrandbits(64)
    b = bytearray(16)
    b[:6] = ms.to_bytes(8, "big")[2:]
    b[6] = 0x70 | ((hlc >> 8) & 0x0F)
    b[7] = hlc & 0xFF
    b[8:] = r.to_bytes(8, "big")
    b[8] = 0x80 | (b[8] & 0x3F)
    h = b.hex()
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}"


def has_table(c, name):
    return c.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)).fetchone() is not None


def append_event(c, kind, actor, actor_kind, thread_id=None, ref=None, body=None, data=None, ts=None):
    """Db::append_event, inside the caller's transaction. A board without the events table gets none."""
    if not has_table(c, "events"):
        return
    row = c.execute("SELECT value FROM event_marks WHERE name='node'").fetchone() if has_table(c, "event_marks") else None
    if row is None:
        raise RuntimeError("the board has an event log but no node id yet: open AgentDesk once, then retry")
    origin = row[0]
    seq = c.execute("SELECT COALESCE(MAX(actor_seq), 0) + 1 FROM events WHERE origin=? AND actor=?", (origin, actor)).fetchone()[0]
    last = c.execute("SELECT hlc FROM events ORDER BY seq DESC LIMIT 1").fetchone()
    hlc = max(int(time.time() * 1000) << 16, (last[0] if last else 0) + 1)
    c.execute("INSERT INTO events (id, ts, hlc, origin, actor, actor_kind, actor_seq, kind, thread_id, ref, body, data) "
              "VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
              (uuid7(hlc), ts or now_iso(), hlc, origin, actor, actor_kind, seq, kind, thread_id, ref, body,
               None if data is None else json.dumps(data, separators=(",", ":"))))


# ---- board writes (src/db.rs)

def mentions(body):
    """db::mentions: @name tokens outside fenced code, '@' starting the token."""
    text = re.sub(r"(?s)```.*?```", "", body)
    out = []
    for i, ch in enumerate(text):
        if ch != "@" or (i > 0 and (text[i - 1].isalnum() or text[i - 1] in "_.")):
            continue
        if i + 1 >= len(text) or not (text[i + 1].isascii() and text[i + 1].isalpha()):
            continue
        name = ""
        for x in text[i + 1:]:
            if not (x.isalnum() or x in "_:#-"):
                break
            name += x
        if name not in out:
            out.append(name)
    return out


def reply(c, thread_id, author, kind, body, meta=None):
    """Db::reply: the message, its msg.posted event, the thread's updated_ts, the agent's presence."""
    names = mentions(body)
    if names:
        meta = dict(meta or {})
        meta["mentions"] = names
    stored = None if meta is None else dumps(meta)
    ts = now_iso()
    mid = c.execute("INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta) VALUES (?,?,?,?,?,NULL,?)",
                    (ts, thread_id, author, kind, body, stored)).lastrowid
    data = {"message_id": mid}
    if meta and "kind" in meta:
        data["kind"] = meta["kind"]
    append_event(c, "msg.posted", author, kind, thread_id, f"message:{mid}", body, data, ts)
    c.execute("UPDATE threads SET updated_ts=? WHERE id=?", (ts, thread_id))
    if kind == AGENT:
        c.execute("INSERT INTO presence (author, seen_ts) VALUES (?,?) ON CONFLICT(author) DO UPDATE SET seen_ts=excluded.seen_ts",
                  (author, now_iso()))
    return mid


def john_replies(c, thread_id, body):
    """Db::john_replies: John's message; an open question becomes answered; the asking agent owes an ack."""
    t = c.execute("SELECT channel, status FROM threads WHERE id=?", (thread_id,)).fetchone()
    if t is None:
        raise RuntimeError(f"no such thread: {thread_id}")
    mid = reply(c, thread_id, JOHN, HUMAN, body)
    if t["channel"] != "question":
        return mid
    if t["status"] == "open":
        ts = now_iso()
        c.execute("UPDATE threads SET status='answered', updated_ts=? WHERE id=?", (ts, thread_id))
        append_event(c, "thread.status", JOHN, HUMAN, thread_id, data={"status": "answered"}, ts=ts)
    first = c.execute("SELECT author_kind FROM messages WHERE thread_id=? ORDER BY id LIMIT 1", (thread_id,)).fetchone()
    if first and first[0] == AGENT:
        ts = now_iso()
        cur = c.execute("INSERT OR IGNORE INTO acks (message_id, thread_id, agent, state, created_ts) "
                        "SELECT ?, id, opened_by, 'pending', ? FROM threads WHERE id=?", (mid, ts, thread_id))
        if cur.rowcount == 1:
            agent = c.execute("SELECT agent FROM acks WHERE message_id=?", (mid,)).fetchone()[0]
            append_event(c, "ack", "agentdesk", "system", thread_id, f"message:{mid}",
                         data={"state": "pending", "agent": agent}, ts=ts)
    return mid


def close_question(c, thread_id):
    """Db::close_question: closed without an answer; archive_hold cleared so the sweep files it."""
    t = c.execute("SELECT channel, status, meta FROM threads WHERE id=?", (thread_id,)).fetchone()
    if t is None:
        raise RuntimeError(f"no such thread: {thread_id}")
    if t["channel"] != "question" or t["status"] == "archived":
        return False
    try:
        meta = json.loads(t["meta"] or "{}")
    except ValueError:
        meta = {}
    meta = meta if isinstance(meta, dict) else {}
    meta.pop("archive_hold", None)
    ts = now_iso()
    changed = c.execute("UPDATE threads SET status='closed', updated_ts=?, meta=? WHERE id=?", (ts, dumps(meta), thread_id)).rowcount
    if changed:
        append_event(c, "thread.status", JOHN, HUMAN, thread_id, data={"status": "closed"}, ts=ts)
    return changed == 1


# ---- permission rules (src/permissions.rs)

def edit_settings(path, rule):
    """permissions::edit(add): back the file up, add the rule to permissions.allow once, keep every other key."""
    backup = None
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            text = f.read()
        try:
            root = json.loads(text)
        except ValueError as e:
            raise RuntimeError(f"{path} is not valid JSON ({e}): not touched. Fix it by hand, then approve again.")
        if not isinstance(root, dict):
            raise RuntimeError(f"{path} is not a JSON object: not touched. Fix it by hand, then approve again.")
        folder = os.path.join(DATA, "permission_backups")
        os.makedirs(folder, exist_ok=True)
        now = datetime.now(timezone.utc)
        origin = os.path.basename(os.path.dirname(path)) or "settings"
        backup = os.path.join(folder, f"{now.strftime('%Y%m%d-%H%M%S')}-{now.microsecond // 1000:03d}-{origin}.json")
        with open(backup, "w", encoding="utf-8", newline="") as f:
            f.write(text)
    else:
        root = {}
    perms = root.get("permissions")
    if not isinstance(perms, dict):
        perms = root["permissions"] = {}
    allow = perms.get("allow")
    if not isinstance(allow, list):
        allow = perms["allow"] = []
    if rule not in allow:
        allow.append(rule)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = f"{path}.{os.getpid()}.tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(json.dumps(root, indent=2, ensure_ascii=False) + "\n")
    os.replace(tmp, path)
    return backup


def settings_for(p):
    if p["scope"] == "user":
        return CLAUDE_SETTINGS
    if not p["project_dir"]:
        raise RuntimeError("a project rule needs a project_dir")
    if TEST_SETTINGS:  # a test never writes a real project's settings
        return os.path.join(TEST_SETTINGS, os.path.basename(p["project_dir"].rstrip("\\/")), ".claude", "settings.json")
    return os.path.join(p["project_dir"], ".claude", "settings.json")


def decide(c, thread_id, approve):
    """permissions::decide, found by its question thread."""
    p = c.execute("SELECT * FROM permission_proposals WHERE thread_id=? ORDER BY id DESC LIMIT 1", (thread_id,)).fetchone()
    if p is None:
        raise RuntimeError(f"thread {thread_id} has no permission proposal")
    if p["status"] != "pending":
        raise RuntimeError(f"this rule is already {p['status']}")
    rule, ts = p["rule"], now_iso()
    if approve:
        path = settings_for(p)
        backup = edit_settings(path, rule)  # the file first, as Rust does: a refused file leaves the board untouched
        c.execute("UPDATE permission_proposals SET status='approved', decided_ts=?, settings_path=?, backup=? WHERE id=?",
                  (ts, path, backup, p["id"]))
        append_event(c, "approval.decided", JOHN, HUMAN, thread_id, f"proposal:{p['id']}", data={"rule": rule, "status": "approved"}, ts=ts)
        reply(c, thread_id, "AgentDesk", AGENT,
              f"Approved in AgentDesk: `{rule}` is now in `permissions.allow` of {path}. Retry the action.")
        john_replies(c, thread_id, "Approved in AgentDesk.")
    else:
        c.execute("UPDATE permission_proposals SET status='rejected', decided_ts=? WHERE id=?", (ts, p["id"]))
        append_event(c, "approval.decided", JOHN, HUMAN, thread_id, f"proposal:{p['id']}", data={"rule": rule, "status": "rejected"}, ts=ts)
        reply(c, thread_id, "AgentDesk", AGENT,
              f"Rejected in AgentDesk: `{rule}` was not added. Do not retry the action; ask John another way.")
        john_replies(c, thread_id, "Rejected in AgentDesk.")
    return {"ok": True, "status": "approved" if approve else "rejected", "rule": rule}


# ---- reads

def when(ts):
    return (ts or "")[5:16].replace("T", " ")


def kind_of(meta):
    try:
        m = json.loads(meta or "{}")
        return m.get("type", "") if isinstance(m, dict) else ""
    except ValueError:
        return ""


TABS = ("questions", "discussion", "work", "goals", "prs", "wiki")
LIMIT = 100


def snapshot(c):
    """Everything the pane draws without asking again: every tab's rows, and every waiting question in full, so
    switching tabs and opening a question cost no process at all. One read transaction, so it is self-consistent."""
    c.execute("BEGIN")
    try:
        tabs = {name: tab(c, name) for name in TABS}
        threads = {str(r["thread_id"]): thread(c, r["thread_id"])["thread"] for r in tabs["questions"]["rows"]}
    finally:
        c.execute("COMMIT")
    return {"tabs": tabs, "threads": threads, "waiting": tabs["questions"]["waiting"]}


def tab(c, name):
    out = []
    waiting = c.execute(f"SELECT COUNT(*) FROM threads t WHERE {WAITING}").fetchone()[0]
    total = None
    if name in ("discussion", "wiki", "work"):
        total = c.execute("SELECT COUNT(*) FROM threads WHERE channel=? AND status <> 'archived'", (name,)).fetchone()[0]
    elif name == "goals":
        total = c.execute("SELECT COUNT(*) FROM goals").fetchone()[0]
    elif name == "prs":
        total = c.execute("SELECT COUNT(*) FROM pull_requests").fetchone()[0]
    if name == "questions":
        for r in c.execute(f"SELECT t.id, t.subject, t.opened_by, t.updated_ts, t.meta FROM threads t "
                           f"WHERE {WAITING} ORDER BY t.updated_ts DESC"):
            out.append({"thread_id": r["id"], "title": r["subject"], "who": r["opened_by"], "when": when(r["updated_ts"]), "ts": r["updated_ts"],
                        "badge": kind_of(r["meta"]) or "ask"})
    elif name in ("discussion", "wiki", "work"):
        for r in c.execute("SELECT t.id, t.subject, t.opened_by, t.status, t.updated_ts, "
                           "(SELECT COUNT(*) FROM messages m WHERE m.thread_id = t.id) AS n "
                           "FROM threads t WHERE t.channel = ? AND t.status <> 'archived' ORDER BY t.updated_ts DESC LIMIT ?", (name, LIMIT)):
            out.append({"thread_id": r["id"], "title": r["subject"], "who": f"{r['opened_by']} · {r['n']} msgs",
                        "when": when(r["updated_ts"]), "ts": r["updated_ts"], "badge": r["status"]})
    elif name == "goals":
        for r in c.execute("SELECT name, objective, state, lead, thread_id, updated_ts FROM goals "
                           "ORDER BY CASE state WHEN 'running' THEN 0 WHEN 'draft' THEN 1 ELSE 2 END, updated_ts DESC LIMIT ?", (LIMIT,)):
            out.append({"thread_id": r["thread_id"], "title": f"{r['name']}: {(r['objective'] or '')[:120]}",
                        "who": f"lead {r['lead']}", "when": when(r["updated_ts"]), "ts": r["updated_ts"], "badge": r["state"]})
    elif name == "prs":
        for r in c.execute("SELECT repo, number, title, requested_by, requested_ts, state, thread_id, last_error "
                           "FROM pull_requests ORDER BY CASE state WHEN 'open' THEN 0 ELSE 1 END, id DESC LIMIT ?", (LIMIT,)):
            who = r["requested_by"] + (f" · {r['last_error']}" if r["last_error"] else "")
            out.append({"thread_id": r["thread_id"], "title": f"{r['repo']}#{r['number']} {r['title']}",
                        "who": who, "when": when(r["requested_ts"]), "ts": r["requested_ts"], "badge": r["state"]})
    else:
        raise RuntimeError(f"unknown tab: {name}")
    return {"rows": out, "waiting": waiting, "total": len(out) if total is None else total}


def thread(c, thread_id):
    t = c.execute("SELECT id, channel, status, subject, opened_by, meta FROM threads WHERE id=?", (thread_id,)).fetchone()
    if t is None:
        raise RuntimeError(f"no such thread: {thread_id}")
    msgs = [dict(m) for m in c.execute(
        "SELECT id, ts, author, author_kind, body, COALESCE(json_extract(meta, '$.kind'), '') AS kind "
        "FROM messages WHERE thread_id=? ORDER BY id", (thread_id,))]
    proposal = None
    if has_table(c, "permission_proposals"):
        p = c.execute("SELECT id, rule, scope, status FROM permission_proposals WHERE thread_id=? ORDER BY id DESC LIMIT 1",
                      (thread_id,)).fetchone()
        proposal = dict(p) if p else None
    waiting = c.execute(f"SELECT COUNT(*) FROM threads t WHERE t.id=? AND {WAITING}", (thread_id,)).fetchone()[0] == 1
    return {"thread": {"thread_id": t["id"], "channel": t["channel"], "status": t["status"], "subject": t["subject"],
                       "opened_by": t["opened_by"], "kind": kind_of(t["meta"]), "waiting": waiting,
                       "proposal": proposal,
                       "messages": [{k: m[k] for k in ("id", "ts", "author", "author_kind", "body")}
                                    for m in msgs if m["kind"] not in RECEIPTS]}}


def connect(writable):
    if writable:
        c = sqlite3.connect(DB, timeout=30, isolation_level=None)
        c.execute("PRAGMA busy_timeout=30000")
    else:
        c = sqlite3.connect("file:" + DB.replace("\\", "/") + "?mode=ro", uri=True, timeout=30)
    c.row_factory = sqlite3.Row
    return c


def write(fn):
    """One write transaction: BEGIN IMMEDIATE, so a concurrent writer waits on the busy timeout instead of failing."""
    c = connect(True)
    try:
        c.execute("BEGIN IMMEDIATE")
        out = fn(c)
        c.execute("COMMIT")
        return out
    except BaseException:
        if c.in_transaction:
            c.execute("ROLLBACK")
        raise
    finally:
        c.close()


def read(fn):
    c = connect(False)
    try:
        return fn(c)
    finally:
        c.close()


def after_write(out, thread_id):
    """A write's answer carries the board as it now is, so the pane redraws from one process, not three."""
    out["snapshot"] = read(snapshot)
    out["thread"] = read(lambda c: thread(c, thread_id))["thread"]
    return out


def main(argv):
    op, args = (argv[1] if len(argv) > 1 else ""), argv[2:]
    if op == "snapshot":
        return read(snapshot)
    if op == "tab":
        return read(lambda c: tab(c, args[0] if args else "questions"))
    if op == "thread":
        return read(lambda c: thread(c, int(args[0])))
    if op == "reply":
        body = sys.stdin.buffer.read().decode("utf-8")
        if not body.strip():
            return {"error": "the reply is empty"}
        tid = int(args[0])
        return after_write({"ok": True, "message_id": write(lambda c: john_replies(c, tid, body))}, tid)
    if op == "close":
        tid = int(args[0])
        return after_write({"ok": True, "closed": write(lambda c: close_question(c, tid))}, tid)
    if op == "permission" and len(args) == 2 and args[0] in ("approve", "reject"):
        tid = int(args[1])
        return after_write(write(lambda c: decide(c, tid, args[0] == "approve")), tid)
    return {"error": f"unknown op: {' '.join(argv[1:])}"}


if __name__ == "__main__":
    try:
        out = main(sys.argv)
    except Exception as e:  # a locked, missing or refused file reaches the pane as text, never as a crash
        out = {"error": str(e) or type(e).__name__}
    sys.stdout.buffer.write(json.dumps(out, ensure_ascii=False).encode("utf-8"))
    sys.exit(1 if "error" in out else 0)
