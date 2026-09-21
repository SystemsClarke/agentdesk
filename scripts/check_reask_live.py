"""The re-ask acceptance, run against a COPY of the live board.

Work item #75, whose acceptance test reads:

    open_questions() before the fix -> []. After -> contains #54, with its
    subject. Post an ack into #54 -> still exactly one entry for #54, and it
    did not leave the list. Have John reply again -> #54 leaves the list. Have
    an agent ask again -> #54 returns.

WHY A COPY OF THE LIVE BOARD AND NOT A FIXTURE. The claim under test is that a
question John has already answered once can never be asked again, and the
evidence for it is a real thread that is in exactly that state: #54 is
`answered`, its last word is an agent's, and the live view does not show it.
A scratch fixture would be a thread I built to look like that, which proves the
rule and not the defect. So the fixture is the board itself.

SAFETY. The live database is opened READ-ONLY, copied through SQLite's own
backup API (a file copy during a write yields a torn file missing the
write-ahead log), and every redirect below points the package at the copy.
Nothing in this script writes to `%LOCALAPPDATA%\\AgentDesk`, and every
`VAULT_*` path is moved to scratch before the first import of `vault`, because
section 8 archives a question and the archive writes a transcript.

    .venv\\Scripts\\python.exe scripts\\check_reask_live.py
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

# The live board, read-only, BEFORE agentdesk.paths is imported -- the package
# resolves DATA_DIR from LOCALAPPDATA at import time, so this has to be read
# from the environment directly rather than from paths.
_LIVE_DIR = Path(os.environ.get("LOCALAPPDATA", Path.home())) / "AgentDesk"
_LIVE_DB = _LIVE_DIR / "agentdesk.db"

_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-reask-live-"))

from agentdesk import db, mcp_server, paths  # noqa: E402

# --- the redirect ---------------------------------------------------------------
# Everything writable, moved to scratch. The copy of the database is made
# through the backup API first so the scratch board starts as the live board.
paths.DATA_DIR = _SCRATCH
paths.DB_PATH = _SCRATCH / "agentdesk.db"
paths.ARCHIVE_DIR = _SCRATCH / "archive"
paths.LOG_PATH = _SCRATCH / "agentdesk.log"
paths.WORKER_STATE = _SCRATCH / "worker.state"
paths.WORKER_STOP = _SCRATCH / "worker.stop"
paths.NOTIFY_STATE = _SCRATCH / "notify_state.json"

_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = paths.VAULT_AGENTDESK / "questions"
for _d in (paths.ARCHIVE_DIR, paths.VAULT_AGENTDESK, paths.VAULT_LOG,
           paths.VAULT_NOTES, paths.VAULT_MAPS, paths.VAULT_PARKED,
           paths.VAULT_QUESTIONS):
    _d.mkdir(parents=True, exist_ok=True)

THREAD = 54
WIDTH = 78
FAILURES: list[str] = []

# The definition the live board carried before this change, verbatim, so the
# BEFORE arm can still be run once the live board has migrated -- which it will
# have done the first time ANY MCP tool touched it, because init_db runs on
# connect and the DROP is what applies the new definition. See the note in
# section 0/1 about which of the two states this run found.
OLD_VIEW_SQL = """
CREATE VIEW open_questions AS
    SELECT t.id AS thread_id, t.subject, t.opened_by, t.created_ts, t.updated_ts
    FROM threads t
    WHERE t.channel = 'question' AND t.status = 'open';
"""


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<46} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<42} {got!r}")


def note(text: str) -> None:
    for line in text.strip().splitlines():
        print(f"  {line}")


def entry_ids(conn) -> list[int]:
    return sorted(r["thread_id"] for r in db.open_questions(conn))


def entries_for(conn, tid: int) -> list:
    return [r for r in db.open_questions(conn) if r["thread_id"] == tid]


def tail(conn, tid: int, n: int = 4) -> None:
    rows = conn.execute(
        "SELECT m.id, m.author, m.author_kind,"
        " COALESCE(json_extract(m.meta, '$.kind'), '-') AS kind,"
        " substr(m.body, 1, 46) AS body"
        " FROM messages m WHERE m.thread_id = ? ORDER BY m.id DESC LIMIT ?",
        (tid, n)).fetchall()
    for r in reversed(rows):
        print(f"    #{r['id']:<4} {r['author']:<20} {r['author_kind']:<6}"
              f" {r['kind']:<12} {r['body']!r}")


def main() -> int:
    # --- the copy, made before anything else touches the board ------------------
    hr("0. the fixture: the live board, copied, read-only on the original")
    if not _LIVE_DB.exists():
        print(f"  [FAIL] no live database at {_LIVE_DB}")
        return 1
    src = sqlite3.connect(f"file:{_LIVE_DB}?mode=ro", uri=True)
    dst = sqlite3.connect(str(paths.DB_PATH))
    with dst:
        src.backup(dst)
    src.close()
    dst.close()
    show("copied (backup API, not a file copy)", f"{_LIVE_DB} -> {paths.DB_PATH}")
    show("original opened", "read-only, and never written to")

    conn = db.connect()
    row = conn.execute("SELECT id, channel, status, opened_by, subject"
                       " FROM threads WHERE id=?", (THREAD,)).fetchone()
    check(f"thread #{THREAD} is on the copy", row is not None, True)
    show("its status is", row["status"])
    show("its subject is", row["subject"])
    note(f"the copy is thread #{THREAD} exactly as the live board has it: "
         f"{row['status']}, so John has answered it at least once.")

    # The rule the fix replaces, read back off the copy rather than assumed --
    # this is what makes "before the fix" the live board and not a plausible
    # reconstruction of it.
    stored = conn.execute("SELECT sql FROM sqlite_master"
                          " WHERE name='open_questions'").fetchone()[0]
    already_migrated = not ("t.status = 'open'" in stored
                            and "json_extract" not in stored)
    show("the view the copy carries", "NEW (already migrated)"
         if already_migrated else "OLD (pre-fix)")
    if already_migrated:
        note("The live board has migrated since this item was written, and the "
             "cause is worth naming: init_db runs on every connect and the DROP "
             "VIEW in it is what applies the new definition, so the first MCP "
             "tool call after the fix lands migrates the live board. The BEFORE "
             "arm below therefore installs the old definition on the copy "
             "explicitly. It is the same SQL the live board carried -- printed "
             "in this file as OLD_VIEW_SQL -- but it is a reconstruction and "
             "not the live board's own state, and the difference matters.")
    else:
        note("The live board has NOT migrated yet, so the BEFORE arm below "
             "measures the live board exactly as it stands. This is the "
             "stronger form of the arm and it is only available until the next "
             "connect.")

    last = conn.execute(
        "SELECT m.id, m.author, m.author_kind FROM messages m"
        " WHERE m.thread_id = ? AND COALESCE(json_extract(m.meta, '$.kind'), '')"
        " NOT IN ('ack', 'ack-note', 'read-receipt')"
        " ORDER BY m.id DESC LIMIT 1", (THREAD,)).fetchone()
    show("its last non-receipt word is", f"#{last['id']} from {last['author']}"
         f" ({last['author_kind']})")
    check("...and it is an agent's, not John's", last["author_kind"], "agent")
    note("so John is owed a read on this thread, and the status column cannot "
         "say so: it went 'open' -> 'answered' at his first reply and there is "
         "no reverse transition.")
    hr("...and the tail of it")
    tail(conn, THREAD)

    # --- arm 1: before the fix --------------------------------------------------
    hr("1. BEFORE: the live view cannot see it")
    if already_migrated:
        conn.executescript("DROP VIEW IF EXISTS open_questions;")
        conn.executescript(OLD_VIEW_SQL)
        check("the copy is put back to the pre-fix definition",
              "json_extract" in conn.execute(
                  "SELECT sql FROM sqlite_master WHERE name='open_questions'"
              ).fetchone()[0], False)
    before = entry_ids(conn)
    show("open_questions()", before)
    check(f"#{THREAD} is NOT among them", THREAD in before, False)
    note("The item predicts [] here. It is not [] because the board has moved "
         "on since the item was written and other questions are genuinely "
         "open. The claim the item is actually making -- that a question John "
         "already answered cannot come back -- is what this arm measures, and "
         "it holds: the thread is gone from the list and nothing short of an "
         "edit to the status column can return it.")

    # --- arm 2: after the fix ---------------------------------------------------
    hr("2. AFTER: init_db on the same copy, and the thread comes back")
    db.init_db(conn)
    after_sql = conn.execute("SELECT sql FROM sqlite_master"
                             " WHERE name='open_questions'").fetchone()[0]
    check("the stored definition is now the new one",
          "json_extract" in after_sql, True)
    after = entry_ids(conn)
    show("open_questions()", after)
    check(f"#{THREAD} is among them", THREAD in after, True)
    hits = entries_for(conn, THREAD)
    check("...exactly once", len(hits), 1)
    check("...with its subject", hits[0]["subject"], row["subject"])
    show("the subject, as the view returns it", hits[0]["subject"])

    # --- arm 3: a receipt is not an answer --------------------------------------
    hr("3. a receipt must not answer it for him (trap 2)")
    note("An 'ack' is what an agent delivers for John's reply and is written by "
         "that agent; it is not John speaking. Three kinds exist and all three "
         "are written by agents, so all three are posted here through the real "
         "writers rather than one.")

    # The live board has already delivered its ack and its ack-note for that
    # reply -- that is why the thread is in this state at all -- so both rows
    # are reset before the writers are called, or the writers correctly refuse
    # to post a second one and the arm proves nothing. Said out loud rather
    # than done quietly: this is the one arm that moves the board's own
    # bookkeeping to get where it is going, and the receipt it then posts is a
    # real one written by the real function.
    john_last = conn.execute(
        "SELECT id FROM messages WHERE thread_id=? AND author_kind='human'"
        " ORDER BY id DESC LIMIT 1", (THREAD,)).fetchone()["id"]
    conn.execute("DELETE FROM acks WHERE message_id=?", (john_last,))
    queued = db.queue_ack_for_message(conn, john_last)
    delivered = db.deliver_ack(conn, john_last, "claude",
                               "Acknowledged - your reply has been picked up.")
    landed = conn.execute(
        "SELECT id, author, author_kind, json_extract(meta, '$.kind') AS kind"
        " FROM messages WHERE thread_id=? ORDER BY id DESC LIMIT 1",
        (THREAD,)).fetchone()
    show("ack: queued for John's reply", f"#{john_last} queued={queued}"
         f" delivered={delivered}")
    show("...and this is the message it wrote",
         f"#{landed['id']} {landed['author']} {landed['author_kind']}"
         f" kind={landed['kind']}")
    check("...it really posted an ack into #54", landed["kind"], "ack")
    check("...from an agent, not from John", landed["author_kind"], "agent")
    check("...the list did not move", entry_ids(conn), after)
    check("...and did not leave the list", THREAD in entry_ids(conn), True)
    check("...still exactly one entry", len(entries_for(conn, THREAD)), 1)

    posted = db.post_read_receipt(conn, THREAD, "board-responder",
                                  "Read - nothing to add.")
    last_kind = conn.execute(
        "SELECT json_extract(meta, '$.kind') AS kind FROM messages"
        " WHERE thread_id=? ORDER BY id DESC LIMIT 1", (THREAD,)).fetchone()[0]
    show("read-receipt: posted", f"{posted}, kind={last_kind}")
    check("...it too landed on the thread", last_kind, "read-receipt")
    check("...the list did not move", entry_ids(conn), after)
    check("...still exactly one entry", len(entries_for(conn, THREAD)), 1)
    check("...and it did not leave the list", THREAD in entry_ids(conn), True)

    conn.execute("UPDATE acks SET noted_ts=NULL, state='pending'"
                 " WHERE message_id=?", (john_last,))
    noted = db.post_ack_note(conn, john_last, paths.WATCHER,
                             "Nobody has picked John's reply up yet.")
    last_kind = conn.execute(
        "SELECT json_extract(meta, '$.kind') AS kind FROM messages"
        " WHERE thread_id=? ORDER BY id DESC LIMIT 1", (THREAD,)).fetchone()[0]
    show("ack-note: posted (row re-armed)", f"{noted}, kind={last_kind}")
    note("called with NO meta, deliberately: the writer stamps its own kind "
         "here, the way deliver_ack stamps 'ack'. It did not before -- a bare "
         "call wrote a NULL meta, which RECEIPT_KINDS cannot exclude, so a "
         "note saying nobody has picked the reply up read as a reply.")
    check("...it too landed on the thread", last_kind, "ack-note")
    check("...the list did not move", entry_ids(conn), after)
    check("...still exactly one entry", len(entries_for(conn, THREAD)), 1)
    check("...and it did not leave the list", THREAD in entry_ids(conn), True)
    note("Every one of those is author_kind='agent'. Before the fix the "
         "exclusion named 'ack' alone, so an agent merely READING a settled "
         "question -- the commonest thing that happens to one -- would have put "
         "it back in front of John.")

    # --- arm 4: John answers again ----------------------------------------------
    hr("4. John replies again, and it leaves the list")
    # The two calls app.post_reply makes, in that order (app.py:2078-2082).
    db.reply(conn, THREAD, paths.HUMAN, paths.HUMAN_KIND,
             "Understood - do it, and keep the last step mine to authorise.")
    if db.get_thread(conn, THREAD)["thread"]["status"] == paths.STATUS_OPEN:
        db.set_thread_status(conn, THREAD, paths.STATUS_ANSWERED)
    now = entry_ids(conn)
    show("open_questions()", now)
    check(f"#{THREAD} has left the list", THREAD in now, False)
    check("...and it did not take anything else with it",
          [i for i in after if i != THREAD], now)

    # --- arm 5: an agent asks again ---------------------------------------------
    hr("5. an agent asks again, and it returns")
    reply = json.loads(mcp_server.answer_thread(
        THREAD, "One more thing on this before I close it out - "
                "the guard rail needs a number from you.",
        author="builder"))
    show("the real MCP answer_thread returned", reply)
    back = entry_ids(conn)
    show("open_questions()", back)
    check(f"#{THREAD} is back", THREAD in back, True)
    check("...exactly once", len(entries_for(conn, THREAD)), 1)
    hr()
    if FAILURES:
        print(f"\n{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("\nevery printed property held.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
