"""The board's SQLite file, as the Python half needs it: open it, read a thread, reply, set a status.

The Rust core owns the schema and everything else about the store; this is only what the vault
plugin and the Slack bridge call. WAL plus a long busy timeout is what lets them share the file.
"""

import json
import re
import sqlite3
from datetime import datetime, timezone
from typing import Optional

from . import paths

# Replies of these kinds are the board's own housekeeping, not somebody saying something.
RECEIPT_KINDS = ("ack", "ack-note", "read-receipt")

# "Waiting on John", against the alias `t` for `threads`: a live question whose newest
# non-receipt message is not his. The open_questions view is the same rule, kept by the core.
JOHN_HAS_LAST_WORD_SQL = (
    "COALESCE((SELECT m.author_kind FROM messages m"
    " WHERE m.thread_id = t.id"
    " AND COALESCE(json_extract(m.meta, '$.kind'), '') NOT IN ("
    + ", ".join(f"'{kind}'" for kind in RECEIPT_KINDS) + ")"
    " ORDER BY m.id DESC LIMIT 1), 'human') = 'human'")

WAITING_SQL = ("(t.channel = 'question'"
               " AND t.status = 'open'"
               " AND NOT (" + JOHN_HAS_LAST_WORD_SQL + "))")


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def connect(db_path=None) -> sqlite3.Connection:
    """Open the database. Safe to call from any process."""
    paths.ensure_dirs()
    conn = sqlite3.connect(str(db_path or paths.DB_PATH), timeout=30.0, isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA busy_timeout=30000")
    conn.execute("PRAGMA foreign_keys=ON")
    return conn


# An @name starts a token (not "user@example.com"), outside fenced code.
_MENTION_RE = re.compile(r"(?<![\w.])@([A-Za-z][\w:#-]*)")
_FENCE_RE = re.compile(r"```.*?```", re.DOTALL)


def _meta_with_mentions(meta: Optional[dict], body: str) -> Optional[dict]:
    """`meta` plus a "mentions" key, only when the body names someone."""
    mentions = list(dict.fromkeys(_MENTION_RE.findall(_FENCE_RE.sub("", body or ""))))
    return {**(meta or {}), "mentions": mentions} if mentions else meta


def reply(conn, thread_id, author, author_kind, body, reply_to=None, meta=None) -> int:
    """Add a message to an existing thread and touch the thread's updated_ts."""
    ts = now_iso()
    meta = _meta_with_mentions(meta, body)
    cur = conn.execute(
        "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta)"
        " VALUES (?,?,?,?,?,?,?)",
        (ts, thread_id, author, author_kind, body, reply_to,
         None if meta is None else json.dumps(meta, ensure_ascii=False)),
    )
    conn.execute("UPDATE threads SET updated_ts=? WHERE id=?", (ts, thread_id))
    return int(cur.lastrowid)


def set_thread_status(conn, thread_id, status) -> None:
    conn.execute("UPDATE threads SET status=?, updated_ts=? WHERE id=?", (status, now_iso(), thread_id))


def get_thread(conn, thread_id) -> dict:
    """The thread row (with `waiting`: the same state word its list row shows) and its messages."""
    t = conn.execute(
        "SELECT t.*, (CASE WHEN " + WAITING_SQL + " THEN 1 ELSE 0 END) AS waiting"
        " FROM threads t WHERE id=?", (thread_id,)).fetchone()
    if t is None:
        raise ValueError(f"no such thread: {thread_id}")
    msgs = [dict(m) for m in conn.execute(
        "SELECT * FROM messages WHERE thread_id=? ORDER BY id", (thread_id,))]
    return {"thread": dict(t), "messages": msgs}


def open_questions(conn) -> list:
    """Questions still waiting on John, newest activity first."""
    return [dict(r) for r in conn.execute("SELECT * FROM open_questions ORDER BY updated_ts DESC")]


def delivery_status(conn, message_id) -> str:
    """picked-up | pushed | woke | resumed | stuck | failed | pending | '' (nothing owed)."""
    a = conn.execute("SELECT state FROM acks WHERE message_id=?", (message_id,)).fetchone()
    d = conn.execute("SELECT state FROM deliveries WHERE message_id=?", (message_id,)).fetchone()
    if a and a["state"] == "posted":
        return "picked-up"
    return d["state"] if d else ("pending" if a else "")
