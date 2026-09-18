"""The message board other agents talk to John and each other through.

This is an MCP server over stdio. Every tool opens its own connection and
closes it before returning: WAL mode makes concurrent access safe, but only
if no process holds a connection open while it thinks. Errors come back as
{"error": ...} rather than exceptions, because the caller is another agent
that can act on returned JSON but never on a traceback.
"""

from __future__ import annotations

import json
import sqlite3

from mcp.server.mcpserver import MCPServer

from . import db, paths


def _dump(data) -> str:
    """One JSON string per tool answer, so a calling agent always has
    something structured to read, errors included."""
    return json.dumps(data, indent=2, ensure_ascii=False)


server = MCPServer(
    name=paths.APP_NAME,
    instructions=(
        "A shared message board. Post updates, read what others wrote, and "
        "ask the human questions that need his decision."
    ),
)


@server.tool()
def post_message(channel: str, subject: str, body: str, author: str,
                 thread_id: int | None = None) -> str:
    """Post a message, appending to an existing thread if thread_id is given
    or starting a new one if not. Use this for discussion and wiki posts, or
    to continue a thread you are already part of; when you need a decision
    from John, use ask_human instead, because only question threads surface
    in his pending list."""
    if channel not in paths.CHANNELS:
        return _dump({"error": f"channel must be one of {list(paths.CHANNELS)}, got {channel!r}"})
    conn = db.connect()
    try:
        tid = db.start_thread(conn, channel, subject, author, paths.AGENT_KIND, body,
                              thread_id=thread_id)
        return _dump({"ok": True, "thread_id": tid})
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def ask_human(subject: str, body: str, author: str, meta: dict | None = None) -> str:
    """Ask John a question he must answer before you can carry on. Use this
    whenever you are blocked on a decision, a preference, or a permission;
    do not use it for progress reports, which belong in post_message under
    discussion."""
    conn = db.connect()
    try:
        tid = db.start_thread(conn, "question", subject, author, paths.AGENT_KIND, body,
                              meta=meta)
        return _dump({"ok": True, "thread_id": tid})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def list_threads(channel: str | None = None, status: str | None = None,
                 limit: int = 50) -> str:
    """List threads newest-activity-first, each with its message count and
    last message, optionally filtered by channel (question, discussion, wiki)
    and status (open, answered, closed, fyi). Use this to catch up on what
    has happened, or to find a thread id before reading one in full."""
    conn = db.connect()
    try:
        return _dump({"threads": db.list_threads(conn, channel=channel, status=status,
                                                 limit=limit)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def read_thread(thread_id: int) -> str:
    """Read one thread in full: the thread row plus every message in order.
    Use this after list_threads or open_questions gives you a thread id and
    you need the actual words, not just the subject line."""
    conn = db.connect()
    try:
        return _dump(db.get_thread(conn, thread_id))
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def open_questions() -> str:
    """List the question threads still waiting on John. Check this before
    calling ask_human, so you do not ask again what is already pending, and
    when you come online to see what is blocked on him."""
    conn = db.connect()
    try:
        return _dump({"open_questions": db.open_questions(conn)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def answer_thread(thread_id: int, body: str, author: str) -> str:
    """Add an answer from one agent to an existing thread. Use this to reply
    to another agent's question or to contribute to a discussion. It
    deliberately does not change a question thread's status: only John's
    answer closes those, because 'answered' is what stops his toast
    repeating."""
    conn = db.connect()
    try:
        db.get_thread(conn, thread_id)  # a clean error here beats a foreign-key one
        msg_id = db.reply(conn, thread_id, author, paths.AGENT_KIND, body)
        return _dump({"ok": True, "thread_id": thread_id, "message_id": msg_id})
    except ValueError as exc:
        return _dump({"error": str(exc)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def search_messages(query: str, limit: int = 20) -> str:
    """Search every message by keyword, full-text plus substring, so an
    identifier or hostname is found even though tokenisation splits it. Use
    this to find where something was discussed before posting a duplicate."""
    conn = db.connect()
    try:
        return _dump({"results": db.search(conn, query, limit=limit)})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


@server.tool()
def recent_messages(limit: int = 30) -> str:
    """The newest messages across every channel, newest first, each with its
    thread's subject and channel. Use this as a quick 'what did I miss' when
    you come online, before deciding which threads to read in full."""
    conn = db.connect()
    try:
        rows = conn.execute(
            "SELECT m.*, t.subject, t.channel FROM messages m"
            " JOIN threads t ON t.id = m.thread_id"
            " ORDER BY m.id DESC LIMIT ?",
            (int(limit),),
        )
        return _dump({"messages": [dict(r) for r in rows]})
    except sqlite3.Error as exc:
        return _dump({"error": f"database error: {exc}"})
    finally:
        conn.close()


def main() -> None:
    # Before the first tool call, not at import: on a machine that has never
    # run the tray app the database does not exist yet, and the server should
    # be the thing that creates it rather than the thing that crashes on it.
    conn = db.connect()
    try:
        db.init_db(conn)
    finally:
        conn.close()
    server.run(transport="stdio")


if __name__ == "__main__":
    main()
