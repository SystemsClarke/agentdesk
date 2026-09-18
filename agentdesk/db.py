"""The store. One SQLite file, shared by the app, the MCP server and the backup.

Every process opens its own connection. WAL mode is what makes that safe: many
readers and one writer at a time, without the app and an MCP server blocking
each other. Do not switch it off.

Nothing here formats anything for display. Callers get rows.
"""

import json
import sqlite3
from datetime import datetime, timezone
from typing import Any, Iterable, Optional

from . import paths

SCHEMA = """
CREATE TABLE IF NOT EXISTS threads (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    created_ts  TEXT NOT NULL,
    updated_ts  TEXT NOT NULL,
    channel     TEXT NOT NULL,
    subject     TEXT NOT NULL,
    opened_by   TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'open',
    meta        TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT NOT NULL,
    thread_id   INTEGER REFERENCES threads(id),
    author      TEXT NOT NULL,
    author_kind TEXT NOT NULL,
    body        TEXT NOT NULL,
    reply_to    INTEGER REFERENCES messages(id),
    meta        TEXT
);

CREATE INDEX IF NOT EXISTS idx_messages_thread  ON messages(thread_id, id);
CREATE INDEX IF NOT EXISTS idx_messages_ts      ON messages(ts);
CREATE INDEX IF NOT EXISTS idx_threads_channel  ON threads(channel, status);
CREATE INDEX IF NOT EXISTS idx_threads_updated  ON threads(updated_ts);

-- Full-text search. Contentless-delete FTS so a thread can be edited without
-- leaving stale rows behind.
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    body, content='messages', content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO messages_fts(rowid, body) VALUES (new.id, new.body);
END;

-- The toast needs 'which questions are unanswered', and it re-asks every few
-- seconds. A view keeps that a single indexed read rather than a scan.
CREATE VIEW IF NOT EXISTS open_questions AS
    SELECT t.id AS thread_id, t.subject, t.opened_by, t.created_ts, t.updated_ts
    FROM threads t
    WHERE t.channel = 'question' AND t.status = 'open';
"""


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def connect(db_path=None) -> sqlite3.Connection:
    """Open the database, creating it if needed. Safe to call from any process."""
    paths.ensure_dirs()
    target = str(db_path or paths.DB_PATH)
    conn = sqlite3.connect(target, timeout=30.0, isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA busy_timeout=30000")
    conn.execute("PRAGMA foreign_keys=ON")
    return conn


def init_db(conn: sqlite3.Connection) -> None:
    conn.executescript(SCHEMA)


def _json(value: Optional[Any]) -> Optional[str]:
    return None if value is None else json.dumps(value, ensure_ascii=False)


# --- writes -------------------------------------------------------------------

def start_thread(conn, channel, subject, opened_by, author_kind, body,
                 meta=None, thread_id=None) -> int:
    """Post a message, creating a thread if `thread_id` is None.

    Returns the THREAD id, so the caller can carry on the conversation with it.

    It used to return the message id -- cur.lastrowid from the INSERT below --
    which is the obvious thing to write and the wrong thing to return. Threads
    and messages have independent id sequences, so the two numbers coincide
    only on an empty database and drift apart from the first reply onwards. A
    caller doing the natural thing with the result (reply to what start_thread
    handed back) then hit a foreign key error, or, where the message id
    happened to match an unrelated thread's id, silently posted into that other
    conversation.

    A question thread is born 'open'; everything else is born 'fyi', because
    only a question is waiting on the human.
    """
    if channel not in paths.CHANNELS:
        raise ValueError(f"channel must be one of {paths.CHANNELS}, got {channel!r}")
    ts = now_iso()

    if thread_id is None:
        # A question waits on the human and a work item waits on an agent; both
        # are 'open' in the sense that something is outstanding. Only the
        # question one reaches the toast, because the view filters the channel.
        status = (paths.STATUS_OPEN
                  if channel in ("question", "work") else paths.STATUS_FYI)
        cur = conn.execute(
            "INSERT INTO threads (created_ts, updated_ts, channel, subject, opened_by, status, meta)"
            " VALUES (?,?,?,?,?,?,?)",
            (ts, ts, channel, subject, opened_by, status, _json(meta)),
        )
        thread_id = int(cur.lastrowid)
    else:
        if conn.execute("SELECT 1 FROM threads WHERE id=?", (thread_id,)).fetchone() is None:
            raise ValueError(f"no such thread: {thread_id}")
        conn.execute("UPDATE threads SET updated_ts=? WHERE id=?", (ts, thread_id))

    conn.execute(
        "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta)"
        " VALUES (?,?,?,?,?,?,?)",
        (ts, thread_id, opened_by, author_kind, body, None, _json(meta)),
    )
    return int(thread_id)


def reply(conn, thread_id, author, author_kind, body, reply_to=None, meta=None) -> int:
    """Add a message to an existing thread. Touches the thread's updated_ts."""
    ts = now_iso()
    cur = conn.execute(
        "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta)"
        " VALUES (?,?,?,?,?,?,?)",
        (ts, thread_id, author, author_kind, body, reply_to, _json(meta)),
    )
    conn.execute("UPDATE threads SET updated_ts=? WHERE id=?", (ts, thread_id))
    return int(cur.lastrowid)


def set_thread_status(conn, thread_id, status) -> None:
    """Open -> answered when the human replies. That transition is what stops
    the toast repeating."""
    conn.execute(
        "UPDATE threads SET status=?, updated_ts=? WHERE id=?",
        (status, now_iso(), thread_id),
    )


def set_thread_subject(conn, thread_id, subject) -> None:
    conn.execute(
        "UPDATE threads SET subject=?, updated_ts=? WHERE id=?",
        (subject, now_iso(), thread_id),
    )


# --- the work queue -----------------------------------------------------------
#
# A work thread is the job. Claiming is a status move plus a name written into
# the thread's meta, so two agents pulling from the queue at once cannot both
# come away believing they own the same task.

def _thread_meta(conn, thread_id) -> dict:
    row = conn.execute("SELECT meta FROM threads WHERE id=?", (thread_id,)).fetchone()
    if row is None:
        raise ValueError(f"no such thread: {thread_id}")
    if not row["meta"]:
        return {}
    try:
        return json.loads(row["meta"]) or {}
    except (TypeError, ValueError):
        # A thread whose meta is not JSON is still a thread; refusing to claim
        # it would be worse than losing whatever was written there.
        return {}


def claim_task(conn, thread_id, agent) -> bool:
    """Take a work item. True if THIS call took it.

    The check and the write are one statement, so two agents racing for the
    same task resolve in SQLite rather than in whichever one read first. A
    False return is not an error: it means somebody else has it.
    """
    ts = now_iso()
    meta = _thread_meta(conn, thread_id)
    meta.update({"assignee": agent, "claimed_ts": ts})
    cur = conn.execute(
        "UPDATE threads SET status=?, updated_ts=?, meta=?"
        " WHERE id=? AND channel=? AND status=?",
        (paths.STATUS_CLAIMED, ts, _json(meta), thread_id, "work",
         paths.STATUS_OPEN),
    )
    return cur.rowcount == 1


def complete_task(conn, thread_id, agent) -> bool:
    """Finish a work item this agent holds. False if it does not hold it.

    Only the assignee may complete, so a task cannot be closed out by an agent
    that never did the work.
    """
    ts = now_iso()
    meta = _thread_meta(conn, thread_id)
    if meta.get("assignee") != agent:
        return False
    meta["completed_ts"] = ts
    cur = conn.execute(
        "UPDATE threads SET status=?, updated_ts=?, meta=?"
        " WHERE id=? AND channel=? AND status=?",
        (paths.STATUS_DONE, ts, _json(meta), thread_id, "work",
         paths.STATUS_CLAIMED),
    )
    return cur.rowcount == 1


def release_task(conn, thread_id, agent, note=None) -> bool:
    """Put a claimed item back on the queue. False if this agent does not hold it.

    This exists because a claim is a lock, and a lock with no release leaks. If
    the agent that took an item dies, times out, or is killed, the item sits in
    'claimed' for ever: it is not open, so no other agent will pick it up, and
    it is not done, so nobody notices it stopped moving. The queue silently
    loses the work, which is worse than either succeeding or failing loudly.

    The attempt counter is the other half. A release puts the item back in front
    of every agent, so an item that reliably kills whatever takes it would be
    retried for ever. Whoever releases increments the count, and the dispatcher
    uses it to stop picking the item up after a few tries.
    """
    ts = now_iso()
    meta = _thread_meta(conn, thread_id)
    if meta.get("assignee") != agent:
        return False
    meta.pop("assignee", None)
    meta.pop("claimed_ts", None)
    meta["attempts"] = int(meta.get("attempts") or 0) + 1
    meta["last_released_ts"] = ts
    if note:
        meta["last_release_note"] = note
    cur = conn.execute(
        "UPDATE threads SET status=?, updated_ts=?, meta=?"
        " WHERE id=? AND channel=? AND status=?",
        (paths.STATUS_OPEN, ts, _json(meta), thread_id, "work",
         paths.STATUS_CLAIMED),
    )
    return cur.rowcount == 1


def list_work(conn, status=None, limit=100) -> list:
    """The work queue, newest first. No status means every state."""
    return list_threads(conn, channel="work", status=status, limit=limit)


# --- reads --------------------------------------------------------------------

def get_thread(conn, thread_id) -> dict:
    t = conn.execute("SELECT * FROM threads WHERE id=?", (thread_id,)).fetchone()
    if t is None:
        raise ValueError(f"no such thread: {thread_id}")
    msgs = [dict(m) for m in conn.execute(
        "SELECT * FROM messages WHERE thread_id=? ORDER BY id", (thread_id,))]
    return {"thread": dict(t), "messages": msgs}


def list_threads(conn, channel=None, status=None, limit=100, since=None) -> list:
    """Threads newest-first, with their message count and last message."""
    sql = [
        "SELECT t.*, COUNT(m.id) AS message_count,",
        "       (SELECT body FROM messages WHERE thread_id=t.id ORDER BY id DESC LIMIT 1) AS last_body",
        "FROM threads t LEFT JOIN messages m ON m.thread_id = t.id",
    ]
    where, args = [], []
    if channel:
        where.append("t.channel = ?"); args.append(channel)
    if status:
        where.append("t.status = ?"); args.append(status)
    if since:
        where.append("t.updated_ts >= ?"); args.append(since)
    if where:
        sql.append("WHERE " + " AND ".join(where))
    sql.append("GROUP BY t.id ORDER BY t.updated_ts DESC LIMIT ?")
    args.append(int(limit))
    return [dict(r) for r in conn.execute(" ".join(sql), args)]


def open_questions(conn) -> list:
    return [dict(r) for r in conn.execute(
        "SELECT * FROM open_questions ORDER BY updated_ts DESC")]


def search(conn, query, limit=50) -> list:
    """Substring search as well as FTS, because most queries here are an
    identifier or a hostname, and FTS tokenisation splits those badly."""
    out, seen = [], set()
    try:
        for r in conn.execute(
            "SELECT m.*, t.subject, t.channel FROM messages_fts f"
            " JOIN messages m ON m.id = f.rowid JOIN threads t ON t.id = m.thread_id"
            " WHERE messages_fts MATCH ? ORDER BY m.id DESC LIMIT ?",
            (query, int(limit)),
        ):
            out.append(dict(r)); seen.add(r["id"])
    except sqlite3.OperationalError:
        pass  # a query FTS cannot parse is not an error, just a miss
    for r in conn.execute(
        "SELECT m.*, t.subject, t.channel FROM messages m JOIN threads t ON t.id = m.thread_id"
        " WHERE m.body LIKE ? OR t.subject LIKE ? ORDER BY m.id DESC LIMIT ?",
        (f"%{query}%", f"%{query}%", int(limit)),
    ):
        if r["id"] not in seen:
            out.append(dict(r)); seen.add(r["id"])
    return out[: int(limit)]


def unread_summary(conn) -> dict:
    """What the tray icon and the toast care about."""
    row = conn.execute(
        "SELECT"
        " (SELECT COUNT(*) FROM threads WHERE channel='question' AND status='open') AS open_questions,"
        " (SELECT COUNT(*) FROM messages) AS total_messages,"
        " (SELECT COUNT(*) FROM threads) AS total_threads"
    ).fetchone()
    return dict(row) if row else {}


def stats(conn) -> dict:
    return unread_summary(conn)
