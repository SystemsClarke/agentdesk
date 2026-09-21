"""The store. One SQLite file, shared by the app, the MCP server and the backup.

Every process opens its own connection. WAL mode is what makes that safe: many
readers and one writer at a time, without the app and an MCP server blocking
each other. Do not switch it off.

Nothing here formats anything for display. Callers get rows.
"""

import json
import re
import sqlite3
from datetime import datetime, timedelta, timezone
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

-- Agent presence, for the acknowledgement watcher. One row per author, holding
-- the last time that author wrote to the board through any of the paths below.
-- It answers "is this agent still around", and nothing else - it is not
-- read-tracking and it must never drive a notification on its own.
CREATE TABLE IF NOT EXISTS presence (
    author  TEXT PRIMARY KEY,
    seen_ts TEXT NOT NULL
);

-- One row per human reply that an agent owes an acknowledgement for. The
-- PRIMARY KEY is the human message itself, which is the whole once-mechanism:
-- two watchers, or this one twice, can only ever create the row once, and
-- every later step is a conditional update of the row that already exists.
-- state: 'pending' -> 'posted' (the agent acknowledged it). noted_ts is set
-- when the board has said out loud that nobody has picked the reply up yet;
-- the row then stays 'pending', because the agent may still come back.
CREATE TABLE IF NOT EXISTS acks (
    message_id INTEGER PRIMARY KEY,
    thread_id  INTEGER NOT NULL REFERENCES threads(id),
    agent      TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'pending',
    created_ts TEXT NOT NULL,
    noted_ts   TEXT,
    settled_ts TEXT
);

-- Read receipts: the record that an agent actually picked a thread up. One row
-- per (agent, thread), and the PRIMARY KEY is the entire once-mechanism, for
-- the same reason acks keys on the message id: reading a thread ten times a
-- minute, from any number of MCP server processes, can only ever create the
-- row once, and everything after that is a conditional write against a row
-- that already exists. It is a table and not a flag in the message's meta
-- because a check in Python -- "has this agent receipted here yet?" -- is a
-- race between two MCP servers, and a uniqueness constraint is not.
--
-- CREATE TABLE IF NOT EXISTS, so init_db still opens a database that predates
-- this: an existing board gains the empty table and keeps every row it had.
-- There is no ALTER and therefore no migration to get wrong.
CREATE TABLE IF NOT EXISTS receipts (
    agent      TEXT NOT NULL,
    thread_id  INTEGER NOT NULL REFERENCES threads(id),
    created_ts TEXT NOT NULL,
    message_id INTEGER,
    PRIMARY KEY (agent, thread_id)
);

-- The merge list: pull requests an agent has opened and John has to merge by
-- hand. The URL is UNIQUE and that is the whole once-mechanism, for the reason
-- receipts keys on (agent, thread): two agents registering the same PR, or one
-- registering it twice, resolve in SQLite and exactly one row exists. A check
-- in Python would be a race between two MCP server processes.
--
-- state is deliberately the same vocabulary as paths.PR_* and NOT thread
-- status. 'open' means GitHub still reports it open -- and ALSO means the last
-- check could not run. Those are the same value on purpose: a failed check has
-- to leave the row indistinguishable from an unchecked one, so that no failure
-- can ever read to John as "this was merged".
--
-- checked_ts/last_error exist so the tab can say WHEN it last asked and WHAT
-- went wrong, rather than showing a stale row that looks current.
--
-- CREATE TABLE IF NOT EXISTS, so an existing board gains the empty table and
-- keeps every row it had. No ALTER, so no migration to get wrong.
CREATE TABLE IF NOT EXISTS pull_requests (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    url          TEXT NOT NULL UNIQUE,
    repo         TEXT NOT NULL,
    number       INTEGER NOT NULL,
    title        TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    requested_ts TEXT NOT NULL,
    thread_id    INTEGER REFERENCES threads(id),
    state        TEXT NOT NULL DEFAULT 'open',
    checked_ts   TEXT,
    settled_ts   TEXT,
    last_error   TEXT,
    notified_ts  TEXT
);

-- The checker asks "which PRs are not settled" on every pass, and the tab asks
-- for open ones first on every redraw.
CREATE INDEX IF NOT EXISTS idx_prs_state ON pull_requests(state, id);

-- What a background agent is doing while it works. One row per thing the
-- dispatcher watched it do: claimed, reading a file, running a command,
-- editing, finished, failed.
--
-- A table rather than a tail of agentdesk.log, which is the other way this
-- could have been done, and the reasons are worth writing down because the log
-- version looks cheaper right up until it is used. Log lines are formatted for
-- debugging, they interleave with the window's and every other process's, and
-- nothing in one says which work item it belongs to -- so "show me item #50's
-- progress" would be a guess over text. `work_id` makes it a query.
--
-- It is also the difference between showing the agent's own output and showing
-- an inference from it. The dispatcher WRITES these as it reads the agent's
-- stream, so a row exists because something actually happened, not because a
-- poll decided the item had been quiet for a while.
--
-- CREATE TABLE IF NOT EXISTS, so an existing board gains the empty table and
-- keeps every row it had. No ALTER, so no migration to get wrong.
CREATE TABLE IF NOT EXISTS work_events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    work_id INTEGER NOT NULL REFERENCES threads(id),
    ts      TEXT NOT NULL,
    kind    TEXT NOT NULL,
    body    TEXT NOT NULL
);

-- The tab reads one item's newest events on every redraw; the poll asks
-- whether anything has been appended at all.
CREATE INDEX IF NOT EXISTS idx_work_events ON work_events(work_id, id);
"""

# The work_events kinds, as a shared vocabulary rather than loose strings: the
# worker writes them and the window colours by them, and a typo in either place
# would show up as an event that renders in the wrong colour instead of as an
# error. WORK_STEP is the one that carries the answer to "what is it doing" --
# one row per tool call the agent made, which is the agent's own account of its
# work rather than the dispatcher's guess at it.
WORK_START = "start"      # claimed, and the agent spawned
WORK_STEP = "step"        # a tool call the agent made
WORK_OUTPUT = "output"    # a line the agent said
WORK_DONE = "done"        # finished, and the item is complete
WORK_ERROR = "error"      # stopped without finishing; body says why
WORK_EVENT_KINDS = (WORK_START, WORK_STEP, WORK_OUTPUT, WORK_DONE, WORK_ERROR)

# The meta.kind values that mark a message as a RECEIPT rather than a reply.
# Shared vocabulary, so it lives beside the tables that write it: the window
# greys anything whose kind is in here, and the warrant test below ignores
# these when deciding whether a thread has anything worth receipting.
#
# 'ack' and 'ack-note' are the John-reply acknowledgements that predate read
# receipts. They were written with this marker from the start - mcp_server's
# ACK_BODY says so in as many words - and until now nothing read it, so a
# receipt and a reply rendered identically. They are in this tuple because the
# grey belongs to both: the thing being distinguished is "a receipt" versus
# "somebody said something", not one flavour of receipt from another.
RECEIPT_KINDS = ("ack", "ack-note", "read-receipt")

# "Waiting on John", as SQL, in ONE place.
#
# Two halves, and the second is the fix this exists for.
#
# THE THREAD IS STILL LIVE. `open` or `answered` -- not `closed`, not
# `archived`. This half is NOT the interesting one and is deliberately the
# weaker test: `closed` is John acting, through the Close & archive button, to
# say he is done with a question, and an inference from the messages must not
# overrule a button he pressed. It also cannot be dropped: closing posts no
# message, so without it a closed question whose last agent note is still in
# it would read as waiting for ever and the Close button would file nothing.
#
# JOHN HAS THE LAST WORD. This half is the fix. A question's status went `open`
# -> `answered` on John's first reply and nothing anywhere ever set it back, so
# every later ask posted into that thread reached nobody -- not the MCP
# `open_questions` tool, not the toast, not the title count, not the red row on
# the tab. Thread #54 was the live victim: John replied (message 188, status
# flipped), an agent asked him to authorise a deploy (message 193), and the
# board reported zero open questions while it sat there unread.
#
# Three surfaces read this, and they must be the same words or they disagree in
# public: the `open_questions` view below (the MCP tool, the toast and the
# title), `unread_summary`'s count (the tray), and the `waiting` column
# `list_threads`/`get_thread` hand the window (the red row and the tab count).
# Hence a module-level string interpolated into all of them rather than the
# same predicate typed out three times and drifting.
#
# RECEIPT_KINDS IS LOAD-BEARING, AND IT IS THE WHOLE TUPLE. The obvious version
# of this clause names `'ack'` alone and is wrong twice over. An 'ack' is the
# receipt an agent delivers for John's reply; an 'ack-note' is the board saying
# out loud that nobody has picked that reply up yet; a 'read-receipt' is posted
# when an agent so much as READS the thread. Every one of them is
# `author_kind='agent'`, so counting any of them would make the board's own
# housekeeping re-open the question it is housekeeping about -- the toast would
# repeat for ever, and a thread would come back to John simply because an agent
# looked at it. The distinction the rule needs is "somebody said something"
# versus "the board kept its own books", which is what RECEIPT_KINDS already
# means everywhere else.
#
# THE `'human'` DEFAULT IS DELIBERATELY QUIET. `start_thread` always writes an
# opening message, so the default is only reached by a thread with no readable
# messages, and it says "not waiting". A false "John owes you an answer" costs
# more than a missed one, because it trains him to ignore the toast.
#
# It is written against the alias `t` for `threads`, because every caller
# already aliases it that way.
JOHN_HAS_LAST_WORD_SQL = (
    "COALESCE((SELECT m.author_kind FROM messages m"
    " WHERE m.thread_id = t.id"
    " AND COALESCE(json_extract(m.meta, '$.kind'), '') NOT IN ("
    + ", ".join(f"'{kind}'" for kind in RECEIPT_KINDS) + ")"
    " ORDER BY m.id DESC LIMIT 1), 'human') = 'human'")

WAITING_SQL = ("(t.channel = 'question'"
               " AND t.status IN ('open', 'answered')"
               " AND NOT (" + JOHN_HAS_LAST_WORD_SQL + "))")


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


# The toast, the MCP `open_questions` tool and the window's title all re-ask
# "which questions are unanswered" every few seconds, so it is a view rather
# than a query -- a single indexed read instead of a scan.
#
# It is OUTSIDE `SCHEMA` on purpose, and built from WAITING_SQL above, because
# of how it has to be applied. Every one of those readers calls init_db on
# every open, against a database that already contains a view of this name,
# and `CREATE VIEW IF NOT EXISTS` would make any edit here a silent no-op on
# every board that already exists: the change ships, the checks pass against a
# fresh database, and John's board is exactly as it was. So init_db drops the
# view and builds it again from this one definition. That drop IS the
# migration, and it is the difference between a fix and the appearance of one.
OPEN_QUESTIONS_VIEW = f"""
CREATE VIEW IF NOT EXISTS open_questions AS
    SELECT t.id AS thread_id, t.subject, t.opened_by, t.created_ts, t.updated_ts
    FROM threads t
    WHERE {WAITING_SQL};
"""


def init_db(conn: sqlite3.Connection) -> None:
    """Create the schema, then bring the open_questions view up to date.

    The second half is not tidiness. `SCHEMA` is all CREATE ... IF NOT EXISTS,
    which is what lets it run against a board that predates any given table --
    and which would equally let a corrected open_questions view never be
    applied to a board that already has the old one. Dropping it first is the
    only thing that makes an edited definition take effect there, and it is
    idempotent: a drop with nothing behind it is free, and the create follows.
    """
    conn.executescript(SCHEMA)
    conn.executescript("DROP VIEW IF EXISTS open_questions;")
    conn.executescript(OPEN_QUESTIONS_VIEW)
    _migrate_pr_columns(conn)


def _migrate_pr_columns(conn: sqlite3.Connection) -> None:
    """Add columns item #115 needs to an existing pull_requests table.

    SCHEMA is CREATE TABLE IF NOT EXISTS, so a board that already has the
    table from #38 never gets these columns from SCHEMA alone. ALTER TABLE
    ADD COLUMN has no IF NOT EXISTS in SQLite, so this checks pragma
    table_info first and is a no-op on a board that already has them --
    safe to call every startup, which is what lets it run from init_db
    unconditionally rather than needing a version table.
    """
    cols = {r["name"] for r in conn.execute("PRAGMA table_info(pull_requests)")}
    if "source" not in cols:
        # Existing rows all came from an agent calling request_merge --
        # the github-scan path did not exist before this migration ran.
        conn.execute(
            "ALTER TABLE pull_requests ADD COLUMN source TEXT NOT NULL"
            " DEFAULT 'agent-registered'")
    if "triage" not in cols:
        conn.execute("ALTER TABLE pull_requests ADD COLUMN triage TEXT")
    if "triage_ts" not in cols:
        conn.execute("ALTER TABLE pull_requests ADD COLUMN triage_ts TEXT")


def _json(value: Optional[Any]) -> Optional[str]:
    return None if value is None else json.dumps(value, ensure_ascii=False)


def _touch_presence(conn, author, author_kind) -> None:
    """Record that this author wrote to the board, right now.

    Called from the write primitives below, which is the point: every path an
    agent can act through (the MCP tools, and a dispatcher posting an agent's
    report) goes through one of them, so presence needs no second mechanism and
    cannot drift from the writes it describes. A stale 'seen' means an agent
    that has gone quiet, which is exactly what the ack watcher needs to know
    before it says "nobody has picked this up". Human writes are skipped - the
    watcher only ever asks about agents.
    """
    if author_kind != paths.AGENT_KIND:
        return
    conn.execute(
        "INSERT INTO presence (author, seen_ts) VALUES (?,?)"
        " ON CONFLICT(author) DO UPDATE SET seen_ts=excluded.seen_ts",
        (author, now_iso()),
    )


# --- writes -------------------------------------------------------------------

QUESTION_WORD_LIMIT = 400


def _enforce_question_length(channel: str, author_kind: str, body: str) -> None:
    """John asked twice for this, in these words the second time: a question
    should read in four or five sentences, about 400 words, so it is
    parsable. Human replies are never limited -- it is his board, and the
    problem was never his writing, it was an agent's follow-up correction
    burying the actual ask under its own working-out (thread 83 is the
    measured case: nine agent messages, several over 500 words, before the
    ask itself changed).

    Rejected rather than truncated. A question cut off mid-sentence is a
    wrong question, not a shorter one, and the agent that wrote it knows
    which 400 words matter better than a byte offset does.
    """
    if channel != "question" or author_kind != paths.AGENT_KIND:
        return
    words = len((body or "").split())
    if words > QUESTION_WORD_LIMIT:
        raise ValueError(
            f"this question is {words} words; John asked for questions and "
            f"replies in a question thread to stay under {QUESTION_WORD_LIMIT}. "
            "State the decision you need in a few sentences and put any "
            "supporting detail in a linked discussion thread or document "
            "instead of in the question itself.")


# --- mentions -------------------------------------------------------------------
#
# Item #111: John wants an agent to be able to aim a message at a specific
# other agent -- a question or a finding named FOR someone -- and have that
# targeting be a distinct, filterable thing rather than prose in the body that
# only a careful reader notices. Investigated first (posted to thread 111):
# crew roles poll the board and could plausibly get a true routing nudge, but
# ad-hoc Claude Code sessions do not poll anything, and reaching one that is
# idle would mean depending on Claude Code's own undocumented, key-guarded
# cross-session daemon (~/.claude/daemon) -- a private harness mechanism this
# standalone SQLite/Tkinter app has no business tying its behaviour to. So this
# is the version the investigation's own "minimum useful" fallback describes:
# a mention is recorded, and answers "messages that mention me" on demand, for
# every population, rather than promising a push nobody can honestly deliver.
#
# The pattern requires the "@" to START the token -- not preceded by a word
# character or a dot -- so "user@example.com" and a decorator/flag inside a
# quoted shell line are not false positives; a name may contain the punctuation
# this board's own names actually use (':' and '#' for a session id like
# "claude-code:GoCDPipelineTool#59cb", '-' for role/agent names). Applied to
# the body with fenced code blocks stripped first, the same "structure before
# content" idea mdview.py uses for the same reason: a decorator or an
# "@echo off" inside a code sample is not somebody being addressed.
_MENTION_RE = re.compile(r"(?<![\w.])@([A-Za-z][\w:#-]*)")
_FENCE_RE = re.compile(r"```.*?```", re.DOTALL)


def extract_mentions(body: str) -> list:
    """Every distinct @name in `body`, in first-seen order. [] for none."""
    text = _FENCE_RE.sub("", body or "")
    seen: list = []
    for m in _MENTION_RE.finditer(text):
        name = m.group(1)
        if name not in seen:
            seen.append(name)
    return seen


def _meta_with_mentions(meta: Optional[dict], body: str) -> Optional[dict]:
    """`meta` plus a "mentions" key, only when the body actually names someone.

    A message without an "@" gets exactly the meta it was given -- this must
    not add an empty list to every one of the board's messages forever to
    record a fact that is true of almost none of them.
    """
    mentions = extract_mentions(body)
    if not mentions:
        return meta
    meta = dict(meta or {})
    meta["mentions"] = mentions
    return meta


def list_mentions(conn, name: str, limit: int = 50) -> list:
    """Messages that @-mention `name` (case-insensitive), newest first.

    Reads through SQLite's JSON1 `json_each` over `meta.mentions` -- the same
    extension `JOHN_HAS_LAST_WORD_SQL` above already relies on, so this adds
    no new dependency. The guard clause (meta present, valid JSON, and the
    path actually exists) runs BEFORE the json_each join: without it, a row
    whose meta is NULL, not JSON, or has no "mentions" key is not simply
    excluded, `json_each` errors on it, which would make one malformed row
    take down every caller's query rather than just fail to match.
    """
    rows = conn.execute(
        "SELECT m.*, t.subject, t.channel FROM messages m"
        " JOIN threads t ON t.id = m.thread_id,"
        " json_each(m.meta, '$.mentions') je"
        " WHERE m.meta IS NOT NULL AND json_valid(m.meta)"
        " AND json_extract(m.meta, '$.mentions') IS NOT NULL"
        " AND lower(je.value) = lower(?)"
        " ORDER BY m.id DESC LIMIT ?",
        (name, int(limit)),
    )
    return [dict(r) for r in rows]


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
    _enforce_question_length(channel, author_kind, body)
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
        (ts, thread_id, opened_by, author_kind, body, None,
         _json(_meta_with_mentions(meta, body))),
    )
    _touch_presence(conn, opened_by, author_kind)
    return int(thread_id)


def reply(conn, thread_id, author, author_kind, body, reply_to=None, meta=None) -> int:
    """Add a message to an existing thread. Touches the thread's updated_ts."""
    row = conn.execute(
        "SELECT channel FROM threads WHERE id=?", (thread_id,)).fetchone()
    if row is not None:
        _enforce_question_length(row[0], author_kind, body)
    ts = now_iso()
    cur = conn.execute(
        "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta)"
        " VALUES (?,?,?,?,?,?,?)",
        (ts, thread_id, author, author_kind, body, reply_to,
         _json(_meta_with_mentions(meta, body))),
    )
    conn.execute("UPDATE threads SET updated_ts=? WHERE id=?", (ts, thread_id))
    _touch_presence(conn, author, author_kind)
    return int(cur.lastrowid)


def set_thread_status(conn, thread_id, status, meta_updates=None) -> None:
    """Open -> answered when the human replies. That transition is what stops
    the toast repeating.

    `meta_updates` merges keys into the thread's meta in the SAME UPDATE, and
    the atomically part is the point: the archive and the note recording what
    it was archived FROM are one write, so a crash cannot leave a thread
    archived with nothing saying what it was before. A `None` value REMOVES
    the key rather than storing a null, so "no longer held" is expressible
    without a second spelling.
    """
    if meta_updates:
        meta = _thread_meta(conn, thread_id)
        for key, value in meta_updates.items():
            if value is None:
                meta.pop(key, None)
            else:
                meta[key] = value
        conn.execute(
            "UPDATE threads SET status=?, updated_ts=?, meta=? WHERE id=?",
            (status, now_iso(), json.dumps(meta), thread_id),
        )
        return
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


# --- who a work item is for ----------------------------------------------------
#
# Both dispatchers -- worker._pick and crew.coordinator_loop -- poll for OPEN
# items and claim them within seconds. That IS the design: it is what makes a
# posted job start by itself, with nobody watching. But it has a consequence
# nobody chose, and it is the reason John asked for this at all: ANY agent that
# looks at the queue finds it already drained. An outside agent could always
# call claim_work -- it simply never saw an open item to call it on. The queue
# was never closed to other agents; it was emptied before they arrived.
#
# So this is a reservation, not a permission, and the distinction is the whole
# design. `auto` keeps the existing behaviour and stays the default. `anyone`
# tells both dispatchers to leave the item alone so it waits for an agent to
# take it deliberately with claim_work.
#
# There is deliberately NO way to reserve an item for one NAMED agent. A name
# is a promise the board cannot keep -- that agent may be gone, or may never
# look -- and honouring it would need an expiry to avoid stranding the item
# open for ever. "Some agent must choose this" is a claim the board can keep.

CLAIM_AUTO = "auto"
CLAIM_ANYONE = "anyone"
CLAIM_POLICIES = (CLAIM_AUTO, CLAIM_ANYONE)


def claim_policy(item) -> str:
    """How this item may be picked up. Unknown or malformed reads as `auto`.

    Defaulting to auto is the safe direction: an item whose meta failed to
    parse, or that predates this key, should be picked up and finished by a
    dispatcher rather than stranded open for ever waiting for a deliberate
    claim that nobody knows to make.
    """
    try:
        meta = json.loads(item.get("meta") or "{}") or {}
    except (TypeError, ValueError, AttributeError):
        return CLAIM_AUTO
    return CLAIM_ANYONE if meta.get("claim") == CLAIM_ANYONE else CLAIM_AUTO


def open_to_dispatcher(item) -> bool:
    """True if a dispatcher may claim this item.

    The rule lives here, in one function, because it has two callers -- the
    worker and the crew coordinator -- and a fix applied to one of them is not
    a fix: the other would still take the item, and the symptom (an agent finds
    nothing to claim) is identical to the one this exists to remove.
    """
    return claim_policy(item) != CLAIM_ANYONE


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
    if cur.rowcount == 1:
        _touch_presence(conn, agent, paths.AGENT_KIND)
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
    if cur.rowcount == 1:
        _touch_presence(conn, agent, paths.AGENT_KIND)
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


# --- what a background agent is doing ------------------------------------------
#
# The dispatcher appends here as it reads the agent's output stream, and the
# Work to Hire tab reads here to show progress. Nothing in this section
# interprets an event: `kind` is the worker's word and `body` is what it saw,
# and the reading happens in app.py where the reader is.

def add_work_event(conn, work_id, kind, body, ts=None) -> int:
    """Record one thing the agent did. Returns the new row's id.

    `ts` is settable so a caller replaying a stream can date an event when it
    happened rather than when it was written, which matters when the writer is
    a thread that may lag the reader's output.
    """
    cur = conn.execute(
        "INSERT INTO work_events (work_id, ts, kind, body) VALUES (?,?,?,?)",
        (int(work_id), ts or now_iso(), str(kind), str(body)))
    return int(cur.lastrowid)


def list_work_events(conn, work_id, limit=80) -> list:
    """One item's events, oldest first, capped to the newest `limit`.

    Oldest-first is the reading order and the caller has no way to reverse it
    without knowing this. The cap is applied to the NEWEST rows before the
    reversal -- taking the first `limit` and reversing would show the agent's
    first eighty steps for ever while it worked, which is the one window that
    gets less useful as the job goes on.
    """
    rows = [dict(r) for r in conn.execute(
        "SELECT * FROM work_events WHERE work_id=? ORDER BY id DESC LIMIT ?",
        (int(work_id), int(limit)))]
    rows.reverse()
    return rows


def work_thread(conn, work_id) -> Optional[dict]:
    """One work item's own row, for a reader that needs its live status.

    Deliberately not folded into list_work: the activity panel asks this on
    every poll tick for the one selected item, and list_work would build the
    whole queue -- message counts and last bodies included -- to answer a
    question about a single row.
    """
    row = conn.execute(
        "SELECT id, subject, status, meta FROM threads WHERE id=? AND channel='work'",
        (int(work_id),)).fetchone()
    return dict(row) if row is not None else None


# --- settled questions, waiting for the vault -----------------------------------
#
# A question is settled when John has answered it or when somebody closed it;
# it is FINISHED when its transcript is in the vault. Those are two different
# facts and this query is the gap between them, so the archive step can be
# retried by whoever notices rather than only by whoever caused it.

def questions_to_archive(conn) -> list:
    """Settled question threads whose transcript is not in the vault yet.

    Settled means 'answered' or 'closed', AND not still waiting on John. The
    second half used to be implied by the first: while a thread's status was
    the only thing that said whether it was waiting, a question he had not
    seen could not carry a settled status. It can now (see WAITING_SQL - he
    answers, an agent asks again, the status stays 'answered'), and filing
    that thread would take it off his tab while it is the one thing on the
    board he still owes an answer to. That is exactly the toast-suppression
    the status test was already here to prevent, so it is stated once.

    Note which way round this goes for `closed`. Closing is John pressing the
    button, and it posts no message -- so a closed thread whose last note is
    an agent's is NOT waiting by WAITING_SQL (a status he set outranks an
    inference from the messages) and is filed. Only an 'answered' thread can
    be held back here, and it is held back exactly when his answer was not the
    last word.

    A thread carrying an archive hold is skipped, and the skip is here rather
    than in the status because a hold and a settled status have to coexist:
    bringing an archived question back restores it to the status it was settled
    with, and if that alone were enough to be re-archived the sweep would file
    it again on the next tick -- three seconds later, so the row would flash on
    the tab and vanish, and it would read as the button not working.
    """
    return [dict(r) for r in conn.execute(
        "SELECT id, subject, status, meta FROM threads t"
        " WHERE t.channel='question' AND t.status IN (?,?)"
        " AND NOT (" + WAITING_SQL + ") ORDER BY t.id",
        (paths.STATUS_ANSWERED, paths.STATUS_CLOSED))
        if not archive_held(r)]


def archive_held(row) -> bool:
    """Is this thread held back from the archive sweep? Reads the row's meta.

    Tolerates anything: a meta column that is not JSON, or that is JSON of the
    wrong shape, means "no hold" rather than an exception. This runs inside the
    sweep, which runs inside the window's poll loop, and a malformed meta on
    one row must not stop every other settled question being filed.
    """
    try:
        return bool((json.loads(row["meta"] or "{}") or {}).get("archive_hold"))
    except (TypeError, ValueError, KeyError, IndexError):
        return False


def unarchive_thread(conn, thread_id) -> bool:
    """Take a question back off the archive so it is on the Questions tab again.

    Returns True when this call changed something and False when the thread was
    not archived, which is not an error -- the caller is a button, and pressing
    it twice is a thing people do.

    The status is restored to whatever the thread was settled as, recovered
    from `archived_from` which the archive wrote in the same UPDATE that
    archived it. That is why the key exists rather than assuming 'answered': a
    question John explicitly CLOSED and later brought back would otherwise
    reappear claiming he had answered it, which is a lie about the state of his
    own board, and the whole point of restoring it is that he can act on what
    it says.

    The hold is what makes the restore stick. See questions_to_archive.
    """
    row = conn.execute(
        "SELECT status, meta FROM threads WHERE id=?", (thread_id,)).fetchone()
    if row is None or row["status"] != paths.STATUS_ARCHIVED:
        return False
    try:
        meta = json.loads(row["meta"] or "{}") or {}
    except (TypeError, ValueError):
        meta = {}
    settled = meta.get("archived_from") or paths.STATUS_ANSWERED
    set_thread_status(conn, thread_id, settled,
                      meta_updates={"archived_from": None, "archive_hold": True})
    return True


# --- acknowledgements ----------------------------------------------------------
#
# When John replies to a question an agent asked, the agent acknowledges it -
# once. The design has three moving parts, and the division between them is
# what keeps the acknowledgement from becoming noise:
#
#   THE WATCHER (the app's poll loop) detects the human reply and queues it
#   here. It never posts as the agent; posting on an agent's behalf is the one
#   thing that would turn a receipt into a forgery.
#
#   THE AGENT delivers, through its own session: mcp_server.py calls
#   deliver_pending_acks() when the agent writes to the board. An MCP server
#   process only exists inside a live agent session, so a dead agent is
#   structurally unable to acknowledge - it does not have to be policed.
#
#   THE BOARD says so when nobody has picked the reply up: the watcher posts
#   one note under paths.WATCHER saying the agent has gone quiet. That note is
#   the honest alternative to a fabricated receipt.
#
# Once-ness rests on the acks table alone. The row is created by INSERT OR
# IGNORE on the human message's id, so any number of watchers produce one row;
# delivery and the note are each a single conditional UPDATE whose rowcount
# decides, so two agents - or two sessions of the same agent - can race and
# exactly one wins. The loser sees rowcount 0 and moves on; there is nothing
# to retry, because the work is done by whoever won.

def queue_ack_for_message(conn, message_id) -> bool:
    """Queue an acknowledgement for this human reply, if it deserves one.

    Deserves: a HUMAN message (only John's replies are acknowledged - an agent
    replying must not trigger anything, or two agents would ping-pong for
    ever), in the question channel, on a thread an agent opened. John
    answering his own thread is nobody's to acknowledge; a work thread is a
    job, not a conversation, and its claim/complete trail already says who is
    on it. True if THIS call created the row; False covers every other case,
    including "already queued", which is the normal result of a second poll
    tick and not an error.
    """
    row = conn.execute(
        "SELECT m.thread_id AS thread_id, m.author_kind AS msg_kind,"
        " t.channel AS channel, t.opened_by AS opened_by,"
        " (SELECT author_kind FROM messages WHERE thread_id = m.thread_id"
        "  ORDER BY id LIMIT 1) AS opener_kind"
        " FROM messages m JOIN threads t ON t.id = m.thread_id WHERE m.id = ?",
        (message_id,),
    ).fetchone()
    if row is None or row["msg_kind"] != paths.HUMAN_KIND:
        return False
    if row["channel"] != "question" or row["opener_kind"] != paths.AGENT_KIND:
        return False
    cur = conn.execute(
        "INSERT OR IGNORE INTO acks (message_id, thread_id, agent, state, created_ts)"
        " VALUES (?,?,?,?,?)",
        (message_id, row["thread_id"], row["opened_by"],
         "pending", now_iso()),
    )
    return cur.rowcount == 1


def row_thread_id(conn, message_id) -> int:
    row = conn.execute("SELECT thread_id FROM messages WHERE id=?",
                       (message_id,)).fetchone()
    if row is None:
        raise ValueError(f"no such message: {message_id}")
    return int(row["thread_id"])


def _cutoff_ts() -> str:
    """The freshness cutoff, as an ISO string so it compares against seen_ts
    directly. now_iso() is always UTC at second resolution, so a plain string
    compare is order-safe."""
    return (datetime.now(timezone.utc)
            - timedelta(seconds=paths.ACK_ACTIVE_SECONDS)).isoformat(
                timespec="seconds")


def due_ack_notes(conn) -> list:
    """Pending acknowledgements the board should speak about: the agent's
    presence has gone quiet (or was never recorded) and nothing has said so
    yet. Rows only - the caller writes the words."""
    return [dict(r) for r in conn.execute(
        "SELECT a.message_id, a.thread_id, a.agent, p.seen_ts AS last_seen_ts"
        " FROM acks a LEFT JOIN presence p ON p.author = a.agent"
        " WHERE a.state='pending' AND a.noted_ts IS NULL"
        " AND (p.seen_ts IS NULL OR p.seen_ts < ?)"
        " ORDER BY a.message_id", (_cutoff_ts(),))]


def post_ack_note(conn, message_id, author, body, meta=None) -> bool:
    """Say on the thread that nobody has picked John's reply up. True if THIS
    call posted the note.

    The conditional UPDATE and the INSERT are one transaction: a crash leaves
    either no note and the row still due, or the note and the row marked. Two
    watchers racing both run the UPDATE; exactly one sees rowcount 1, so the
    board says it once however many polls look at it.

    The note's own kind is stamped here rather than taken from the caller, as
    deliver_ack stamps its 'ack'. A receipt that does not carry its kind is not
    excluded by RECEIPT_KINDS, so it reads as something somebody SAID and puts
    the thread back in front of John -- which is the opposite of what a note
    saying "nobody has picked this up" is for. Leaving that to a caller who can
    pass meta=None is the same defect one layer down.
    """
    note_meta = dict(meta or {})
    note_meta.update({"kind": "ack-note", "ack_for": message_id})
    conn.execute("BEGIN IMMEDIATE")
    try:
        cur = conn.execute(
            "UPDATE acks SET noted_ts=? WHERE message_id=? AND state='pending'"
            " AND noted_ts IS NULL", (now_iso(), message_id))
        if cur.rowcount != 1:
            conn.execute("COMMIT")
            return False
        conn.execute(
            "INSERT INTO messages (ts, thread_id, author, author_kind, body, reply_to, meta)"
            " VALUES (?,?,?,?,?,?,?)",
            (now_iso(), row_thread_id(conn, message_id), author,
             paths.AGENT_KIND, body, message_id, _json(note_meta)))
        conn.execute("UPDATE threads SET updated_ts=? WHERE id=?",
                     (now_iso(), row_thread_id(conn, message_id)))
        conn.execute("COMMIT")
        return True
    except Exception:
        conn.execute("ROLLBACK")
        raise


def pending_acks_for(conn, agent) -> list:
    """Acknowledgements this agent still owes, oldest first."""
    return [dict(r) for r in conn.execute(
        "SELECT message_id, thread_id FROM acks"
        " WHERE agent=? AND state='pending' ORDER BY message_id", (agent,))]


def deliver_ack(conn, message_id, agent, body, meta=None) -> bool:
    """Post the agent's acknowledgement. True if THIS call posted it.

    The state flip and the message insert share one transaction, so there is
    no window in which the ack is marked sent but does not exist: a crash
    leaves it pending (delivered by the next board write) or posted. The agent
    test in the UPDATE is what makes a second agent unable to acknowledge
    somebody else's thread - its UPDATE matches no rows.
    """
    conn.execute("BEGIN IMMEDIATE")
    try:
        cur = conn.execute(
            "UPDATE acks SET state='posted', settled_ts=?"
            " WHERE message_id=? AND agent=? AND state='pending'",
            (now_iso(), message_id, agent))
        if cur.rowcount != 1:
            conn.execute("COMMIT")
            return False
        ack_meta = dict(meta or {})
        ack_meta.update({"kind": "ack", "ack_for": message_id})
        # via reply(), not a bare INSERT: the receipt is a real write by the
        # agent, so it marks presence like one and bumps the thread's
        # updated_ts the same way any other reply does.
        reply(conn, row_thread_id(conn, message_id), agent,
              paths.AGENT_KIND, body, reply_to=message_id, meta=ack_meta)
        conn.execute("COMMIT")
        return True
    except Exception:
        conn.execute("ROLLBACK")
        raise


def deliver_pending_acks(conn, agent, body) -> list:
    """Deliver everything this agent owes. Returns the message ids receipted.

    Called by mcp_server.py on any board write by the agent, so delivery costs
    the agent no diligence and no extra tool call: writing to the board at all
    IS the sign of life the receipt describes. A second delivery attempt finds
    no pending rows and returns [], which is how "once" looks from the
    delivery side.
    """
    delivered = []
    for row in pending_acks_for(conn, agent):
        if deliver_ack(conn, row["message_id"], agent, body):
            delivered.append(row["message_id"])
    return delivered


# --- read receipts -------------------------------------------------------------
#
# "An agent actually picked this up." Separate from the acks above, which run
# the other way: those are an agent acknowledging JOHN's reply, these are an
# agent acknowledging a THREAD. The two share only the grey.
#
# The division of labour is the same shape as the ack watcher's, and for the
# same reason: the agent's own MCP server posts its own receipt, because that
# process only exists inside a live agent session. Nothing else may post one on
# an agent's behalf - a receipt the board wrote for an agent that was not
# running is the forgery this whole feature is supposed to be the alternative
# to, which is also why the board's own "nobody has picked this up" note is in
# the acks path and not here.

def message_kind(meta) -> Optional[str]:
    """The meta.kind of a message row, whether meta arrives as the stored JSON
    string or as a dict already parsed. A row with no meta, or meta that is not
    JSON, has no kind: it is a reply, which is the safe default."""
    if isinstance(meta, dict):
        return meta.get("kind")
    if not meta:
        return None
    try:
        parsed = json.loads(meta)
    except (TypeError, ValueError):
        return None
    return parsed.get("kind") if isinstance(parsed, dict) else None


def is_receipt(meta) -> bool:
    """Whether a message is a receipt rather than a reply. The data half of the
    distinction the window draws in grey - the colour is not the only thing
    that separates them, so a reader that never sees the colour (an export, a
    transcript, the next agent reading this thread) still can."""
    return message_kind(meta) in RECEIPT_KINDS


def thread_warrants_receipt(conn, thread_id, agent) -> bool:
    """Whether `agent` reading this thread deserves a receipt at all.

    One rule, and both halves of it are load-bearing:

        The thread must hold a message that is neither a receipt nor written
        by this agent.

    "Not written by this agent" stops an agent receipting the thread it opened,
    or its own reply two seconds later. "Not a receipt" stops receipts feeding
    themselves: without it a receipt is itself a message, so an agent reading a
    thread would find something to receipt and receipt it, and every thread
    would collect a receipt from every agent the moment anyone looked at it.
    With it, a receipt can only ever be warranted by something somebody
    actually wrote.

    A scan rather than a query, because threads here are tens of messages and
    the LIKE that would express this in SQL cannot tell a real meta.kind from
    the same text inside a body.
    """
    rows = conn.execute(
        "SELECT author, meta FROM messages WHERE thread_id = ?", (thread_id,)
    ).fetchall()
    return any(r["author"] != agent and not is_receipt(r["meta"]) for r in rows)


def post_read_receipt(conn, thread_id, agent, body,
                      kind="read-receipt") -> bool:
    """Record that this agent has picked this thread up. True if THIS call
    posted the receipt; False means it did not warrant one or this agent
    already has one, which is the normal result of a second read and not an
    error.

    Two things it deliberately does NOT do, and they are the whole difference
    between a receipt and a reply:

      It does not bump the thread's updated_ts. A receipt is not activity. A
      board that reorders itself and re-flags a thread as changed because
      somebody glanced at it is the nuisance this feature is supposed to
      avoid, and it would show up as a thread that never settles under the
      cursor while agents work through it. The thread keeps its place in the
      list, and last_body keeps being the last thing anybody actually SAID.

      It does not go through reply(), so it does not touch presence. The ack
      watcher uses presence to decide that an agent has gone quiet; if reading
      counted as being alive, an agent that reads the board every ten minutes
      and answers nothing would silence the one note that says so.

    So a receipt changes exactly one thing on the board: it adds a message
    row. Everything else - status, timestamps, ordering, the open_questions
    view the toast reads, presence - is untouched, which is what makes "no
    toast, no close" a property of the write rather than a promise about it.

    The row and the message share one transaction, so a crash leaves both or
    neither. Marking without posting would be the worst of the three states:
    the thread could never produce the receipt it never got.
    """
    if not thread_warrants_receipt(conn, thread_id, agent):
        return False
    ts = now_iso()
    conn.execute("BEGIN IMMEDIATE")
    try:
        cur = conn.execute(
            "INSERT OR IGNORE INTO receipts (agent, thread_id, created_ts)"
            " VALUES (?,?,?)", (agent, thread_id, ts))
        if cur.rowcount != 1:
            conn.execute("COMMIT")
            return False
        msg = conn.execute(
            "INSERT INTO messages (ts, thread_id, author, author_kind, body,"
            " reply_to, meta) VALUES (?,?,?,?,?,?,?)",
            (ts, thread_id, agent, paths.AGENT_KIND, body, None,
             _json({"kind": kind})),
        )
        conn.execute(
            "UPDATE receipts SET message_id=? WHERE agent=? AND thread_id=?",
            (int(msg.lastrowid), agent, thread_id))
        conn.execute("COMMIT")
        return True
    except Exception:
        conn.execute("ROLLBACK")
        raise


def receipts_for_thread(conn, thread_id) -> list:
    """Who has picked this thread up, oldest first. Rows only."""
    return [dict(r) for r in conn.execute(
        "SELECT agent, created_ts, message_id FROM receipts"
        " WHERE thread_id=? ORDER BY created_ts", (thread_id,))]


# --- the merge list ------------------------------------------------------------

def register_pr(conn, url, repo, number, title, requested_by,
                thread_id=None, source="agent-registered") -> tuple:
    """Put a pull request on John's merge list. Returns (pr_id, created).

    `created` is False when the URL was already on the list, and that is a
    success rather than an error: two agents registering the same PR is a
    normal thing to happen, and the second one should be told the row already
    exists rather than be given a duplicate or a failure. The UNIQUE constraint
    decides it, so the answer is the same from any number of processes.

    A PR that was already merged and is registered again keeps its terminal
    state. Re-registering must not resurrect a settled PR onto John's list --
    a row reappearing after it was merged is exactly the bug that would make
    him stop trusting the list.
    """
    ts = now_iso()
    cur = conn.execute(
        "INSERT OR IGNORE INTO pull_requests"
        " (url, repo, number, title, requested_by, requested_ts, thread_id,"
        "  state, source)"
        " VALUES (?,?,?,?,?,?,?,?,?)",
        (url, repo, int(number), title, requested_by, ts, thread_id,
         paths.PR_OPEN, source))
    if cur.rowcount:
        return int(cur.lastrowid), True
    row = conn.execute("SELECT id FROM pull_requests WHERE url=?", (url,)).fetchone()
    return (int(row["id"]) if row else 0), False


def set_pr_triage(conn, pr_id, label: str) -> None:
    """Record Ladder's classification for one PR. label is free text.

    A separate call from mark_pr_checked on purpose: the gh check and the
    Ladder classification are two independent things that can each fail on
    their own, and conflating them would mean a triage timeout blanks out a
    state check that succeeded, or vice versa.
    """
    conn.execute("UPDATE pull_requests SET triage=?, triage_ts=? WHERE id=?",
                 (label, now_iso(), int(pr_id)))


def get_pr(conn, url) -> Optional[dict]:
    row = conn.execute("SELECT * FROM pull_requests WHERE url=?", (url,)).fetchone()
    return dict(row) if row else None


def list_prs(conn, include_settled=False, limit=200) -> list:
    """The merge list, newest first. Open PRs only unless asked otherwise.

    `include_settled` is what the tab's "show merged" box flips. It is a WHERE
    clause and not a filter after the LIMIT, for the reason list_threads gives:
    filtering afterwards would silently shorten the list by however many
    settled PRs sat in the newest `limit` rows.
    """
    sql = "SELECT * FROM pull_requests"
    args: list = []
    if not include_settled:
        sql += " WHERE state=?"
        args.append(paths.PR_OPEN)
    sql += " ORDER BY id DESC LIMIT ?"
    args.append(int(limit))
    return [dict(r) for r in conn.execute(sql, args)]


def prs_due_for_check(conn) -> list:
    """Open PRs, oldest-checked first. Everything a check pass has to look at.

    Ordered so that repeatedly failing rows cannot starve the others: a PR
    whose check keeps erroring still gets its turn, but at the back, because a
    row that was just retried and failed is the least likely to succeed now.
    NULL checked_ts (never checked) sorts first, which is what makes a PR
    registered while the checker was down get looked at promptly.
    """
    return [dict(r) for r in conn.execute(
        "SELECT * FROM pull_requests WHERE state=?"
        " ORDER BY (checked_ts IS NOT NULL), checked_ts, id", (paths.PR_OPEN,))]


def settle_pr(conn, pr_id, state) -> None:
    """Mark a PR merged or closed -- terminal, and off the tab."""
    ts = now_iso()
    conn.execute(
        "UPDATE pull_requests SET state=?, settled_ts=?, checked_ts=?, last_error=NULL"
        " WHERE id=?", (state, ts, ts, int(pr_id)))


def mark_pr_checked(conn, pr_id, error=None) -> None:
    """Record that a check ran, and whether it worked.

    It does NOT touch state. A check that failed leaves the PR exactly as open
    as it was, which is the whole contract: the list must only ever lose a row
    because GitHub said so, never because this machine could not ask.
    """
    conn.execute("UPDATE pull_requests SET checked_ts=?, last_error=? WHERE id=?",
                 (now_iso(), error, int(pr_id)))


def mark_pr_notified(conn, pr_id) -> bool:
    """Claim the right to post this PR's merge notice. True if we got it.

    The once-mechanism for the notification, and it is a conditional UPDATE
    rather than a read-then-write for the reason the ack table's PRIMARY KEY
    exists: two checker processes can both see notified_ts as NULL, but only
    one UPDATE changes a row.
    """
    cur = conn.execute(
        "UPDATE pull_requests SET notified_ts=? WHERE id=? AND notified_ts IS NULL",
        (now_iso(), int(pr_id)))
    return bool(cur.rowcount)


# --- reads --------------------------------------------------------------------

def get_thread(conn, thread_id) -> dict:
    # `waiting` comes back with the row for list_threads' reason: the detail
    # pane draws the same state word the row above it does, and a thread John
    # answered once that an agent has since asked again in must not read
    # "answered" in the detail header while its row reads "open".
    t = conn.execute(
        "SELECT t.*, (CASE WHEN " + WAITING_SQL +
        "       THEN 1 ELSE 0 END) AS waiting FROM threads t WHERE id=?",
        (thread_id,)).fetchone()
    if t is None:
        raise ValueError(f"no such thread: {thread_id}")
    msgs = [dict(m) for m in conn.execute(
        "SELECT * FROM messages WHERE thread_id=? ORDER BY id", (thread_id,))]
    return {"thread": dict(t), "messages": msgs}


def list_threads(conn, channel=None, status=None, limit=100, since=None,
                 include_archived=True) -> list:
    """Threads newest-first, with their message count and last message.

    `include_archived=False` drops archived threads, which is what the
    Questions tab wants: it lists what is not finished with, and an archived
    question is in the vault, so leaving it on the tab would make archiving
    invisible. It is a WHERE clause and not a filter applied to the rows
    afterwards, so the LIMIT still counts threads the caller asked for:
    filtering after the LIMIT would quietly shorten the tab by the number of
    archived questions in the newest `limit` rows.

    Defaults to True, so the only caller that has an opinion is the one that
    passes it. Every other channel has no archived state to hide, and a copy of
    the board inspected with `--db` is read precisely to see everything.

    Asking for `status='archived'` and `include_archived=False` together is a
    contradiction, and it is resolved in favour of the STATUS. The alternative
    is an empty list, and an empty list here does not read as "you contradicted
    yourself" -- it reads as "nothing has been archived", which is exactly the
    wrong conclusion to draw while looking at the Archived filter.
    """
    sql = [
        "SELECT t.*, COUNT(m.id) AS message_count,",
        "       (SELECT body FROM messages WHERE thread_id=t.id ORDER BY id DESC LIMIT 1) AS last_body,",
        # The window asks this per row for the red flag and the tab count, and
        # it is the same predicate as the view's -- one WRITING of it, three
        # readers. WAITING_SQL is false for every non-question channel, so a
        # work row cannot come back claiming John owes anybody an answer.
        "       (CASE WHEN " + WAITING_SQL + " THEN 1 ELSE 0 END) AS waiting",
        "FROM threads t LEFT JOIN messages m ON m.thread_id = t.id",
    ]
    where, args = [], []
    if channel:
        where.append("t.channel = ?"); args.append(channel)
    if status:
        where.append("t.status = ?"); args.append(status)
    if not include_archived and status != paths.STATUS_ARCHIVED:
        where.append("t.status <> ?"); args.append(paths.STATUS_ARCHIVED)
    if since:
        where.append("t.updated_ts >= ?"); args.append(since)
    if where:
        sql.append("WHERE " + " AND ".join(where))
    sql.append("GROUP BY t.id ORDER BY t.updated_ts DESC LIMIT ?")
    args.append(int(limit))
    return [dict(r) for r in conn.execute(" ".join(sql), args)]


def open_questions(conn, include_archived=False) -> list:
    """Questions still waiting on John, newest-activity-first.

    Backed by the open_questions VIEW, which is WAITING_SQL and nothing else:
    a live question whose newest non-ack message is not John's. That view is
    the one the toast and the title count read, so the question stops driving
    either the moment John has the last word -- before the vault has been
    written to, and whether or not the archive sweep ever runs.

    It is deliberately NOT `status='open'` any more. A question's status is
    still what close_question and the archive sweep move, but it stopped being
    able to answer "is John owed a reply": it goes `open` -> `answered` on his
    first reply and there is no reverse transition, so a thread he had
    answered once was a one-shot, and every later ask in it reached nobody.

    `include_archived` ADDS the archived questions; it does not filter. They
    are not open -- nothing here is claiming they are -- but an archived
    question is otherwise invisible to this tool, and this is the read that
    makes an archive reversible without going behind the board's back to the
    sqlite file. The columns returned are the view's own, so a caller cannot
    tell which branch produced the row without asking.
    """
    if include_archived:
        return [dict(r) for r in conn.execute(
            "SELECT id AS thread_id, subject, opened_by, created_ts, updated_ts"
            " FROM threads WHERE channel='question' AND status IN (?, ?)"
            " ORDER BY updated_ts DESC",
            (paths.STATUS_OPEN, paths.STATUS_ARCHIVED))]
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
    """What the tray icon and the toast care about.

    The count is the SAME predicate as the view's, interpolated rather than
    retyped. When this was `status='open'` it disagreed with the toast the
    moment the two definitions drifted -- and they had: the view said "nothing
    is waiting on John" while thread #54 had an unread ask sitting in it.
    """
    row = conn.execute(
        "SELECT"
        " (SELECT COUNT(*) FROM threads t WHERE " + WAITING_SQL
        + ") AS open_questions,"
        " (SELECT COUNT(*) FROM messages) AS total_messages,"
        " (SELECT COUNT(*) FROM threads) AS total_threads"
    ).fetchone()
    return dict(row) if row else {}


def stats(conn) -> dict:
    return unread_summary(conn)
