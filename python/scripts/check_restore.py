"""Acceptance checks for database restore (thread 82 section 8, built as
work item 170 -- split out of #113/#103/#105 after three duplicate items
all rediscovered the same gap and none of them built it).

The design's shape, restated as what this file proves rather than describes:

  1. Restore uses the online backup API in reverse (`archive.backup(live)`),
     not a file copy -- so it is safe with a live writer holding the
     database.                                                  (section 3)
  2. A restore is itself undoable: it snapshots the current state before
     overwriting it.                                             (section 3)
  3. Row counts after restore match the ARCHIVE, not the pre-restore live
     state -- the whole point of a restore is discarding what came after
     the snapshot.                                                (section 3)
  4. Restoring from an archive stamped after the live database's latest
     message is refused by default, and only proceeds with an explicit
     override.                                                    (section 4)
  5. The CLI's dry run (no --yes) changes nothing.                 (section 5)
  6. A restore posts a line to the discussion board.               (section 6)

Runs entirely against a scratch database under a scratch LOCALAPPDATA --
never the live one, per the item's own acceptance test.

    .venv\\Scripts\\python.exe scripts\\check_restore.py
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import tempfile
import threading
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

# --- isolation, BEFORE agentdesk is imported -----------------------------------
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-restore-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import backup, db, paths  # noqa: E402

WIDTH = 78
FAILURES: list[str] = []


def hr(title: str = "") -> None:
    if title:
        print(f"\n--- {title} " + "-" * max(0, WIDTH - len(title) - 5))
    else:
        print("-" * WIDTH)


def show(label: str, value) -> None:
    print(f"  {label:<44} {value}")


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label:<42} {got!r}")


def note(text: str) -> None:
    for line in text.strip().splitlines():
        print(f"  {line}")


def post(conn, channel, subject, author, body, meta=None) -> int:
    return db.start_thread(conn, channel, subject, author, paths.AGENT_KIND,
                           body, meta=meta)


def message_bodies(conn) -> list[str]:
    return [r["body"] for r in conn.execute("SELECT body FROM messages ORDER BY id")]


def fresh_env(tag: str) -> None:
    """Point paths.DATA_DIR/DB_PATH/ARCHIVE_DIR at a brand new scratch
    directory, so each section starts from an empty board rather than
    reasoning about what an earlier section's restores left behind."""
    d = Path(tempfile.mkdtemp(prefix=f"agentdesk-restore-{tag}-"))
    paths.DATA_DIR = d
    paths.DB_PATH = d / "agentdesk.db"
    paths.ARCHIVE_DIR = d / "archive"
    paths.ensure_dirs()


# --- section 3: reverse-backup restore, undoable, replaces live with archive ---

def section_basic_restore() -> None:
    hr("3. reverse-backup restore: undoable, and replaces live with archive")
    fresh_env("basic")
    conn = db.connect()
    db.init_db(conn)

    post(conn, "discussion", "kept", "builder", "this row is in the archive")
    archive_path = backup.snapshot(conn)
    show("archive taken", archive_path.name)
    archive_counts_before = backup._row_counts(sqlite3.connect(str(archive_path)))
    show("archive's own row counts", archive_counts_before)

    post(conn, "discussion", "damage", "builder",
        "this row was added AFTER the snapshot and must not survive a restore")
    live_before = backup._row_counts(conn)
    show("live counts before restore (archive + the extra row)", live_before)
    check("the extra row really did land", live_before["messages"],
          archive_counts_before["messages"] + 1)

    n_archives_before = len(list(paths.ARCHIVE_DIR.glob("agentdesk-*.db")))
    outcome = backup.restore(archive_path, conn)
    show("restore() returned", {k: outcome[k] for k in ("before", "after")})

    check("row counts after restore match the ARCHIVE, not pre-restore live",
          outcome["after"], archive_counts_before)
    check("...not the pre-restore live counts",
          outcome["after"] == live_before, False)
    check("'damage' did not survive the restore",
          any("damage" in b for b in message_bodies(conn)), False)
    check("'kept' did survive the restore",
          any("this row is in the archive" in b for b in message_bodies(conn)), True)

    n_archives_after = len(list(paths.ARCHIVE_DIR.glob("agentdesk-*.db")))
    show("archive files before / after restore", (n_archives_before, n_archives_after))
    check("restore took its own pre-restore snapshot first (undoable)",
          n_archives_after, n_archives_before + 1)
    check("...and it is the file restore() named",
          Path(outcome["pre_restore_snapshot"]).exists(), True)

    conn.close()


# --- section 4: refuse an archive stamped after live's latest message ----------

def section_refuse_forward() -> None:
    hr("4. refuses to restore from an archive newer than the live database")
    fresh_env("forward")
    conn = db.connect()
    db.init_db(conn)

    post(conn, "discussion", "old news", "builder", "the live db's latest message")

    # A hand-built archive stamped one hour in the future, so it postdates the
    # live message above without needing to sleep a real hour.
    future = datetime.now(timezone.utc) + timedelta(hours=1)
    future_name = f"agentdesk-{future.strftime(backup._SNAPSHOT_STAMP)}.db"
    future_path = paths.ARCHIVE_DIR / future_name
    arc = sqlite3.connect(str(future_path))
    try:
        conn.backup(arc)
    finally:
        arc.close()
    show("archive stamped", future.isoformat())

    refused = False
    try:
        backup.restore(future_path, conn)
    except backup.RestoreRefused as e:
        refused = True
        show("refused with", str(e))
    check("a forward restore is refused by default", refused, True)
    check("...and nothing was touched by the refusal",
          backup._row_counts(conn)["messages"], 1)

    outcome = backup.restore(future_path, conn, force_newer=True)
    show("force_newer=True proceeded", {k: outcome[k] for k in ("before", "after")})
    check("...and the override actually runs it",
          outcome["after"]["messages"], 1)

    conn.close()


# --- section 5: the CLI dry run changes nothing ---------------------------------

def section_dry_run() -> None:
    hr("5. the --restore CLI dry-runs by default (no --yes)")
    fresh_env("dryrun")
    conn = db.connect()
    db.init_db(conn)
    post(conn, "discussion", "dry-run-marker", "builder", "must survive a dry run")
    archive_path = backup.snapshot(conn)
    post(conn, "discussion", "post-snapshot", "builder", "added after the snapshot")
    before = backup._row_counts(conn)
    conn.close()

    result = backup.restore_cli(archive_path, yes=False, json_out=True)
    show("dry run result", result)
    check("dry run reports dry_run=True", result["dry_run"], True)

    conn = db.connect()
    after = backup._row_counts(conn)
    check("dry run changed nothing", after, before)
    check("...the post-snapshot row is still there",
          any("added after the snapshot" in b for b in message_bodies(conn)), True)
    conn.close()


# --- section 6: a real restore posts to the board -------------------------------

def section_posts_to_board() -> None:
    hr("6. a real restore (--yes) posts a line to the discussion board")
    fresh_env("post")
    conn = db.connect()
    db.init_db(conn)
    archive_path = backup.snapshot(conn)
    n_threads_before = conn.execute("SELECT COUNT(*) FROM threads").fetchone()[0]
    conn.close()

    result = backup.restore_cli(archive_path, yes=True, json_out=True)
    show("restore_cli(--yes) result", {k: result[k] for k in ("before", "after")})

    conn = db.connect()
    posted = conn.execute(
        "SELECT subject, author, body FROM threads t JOIN messages m"
        " ON m.thread_id = t.id"
        " WHERE t.channel='discussion' AND t.subject LIKE 'Database restored%'"
        " ORDER BY t.id DESC LIMIT 1"
    ).fetchone()
    conn.close()
    show("board post", dict(posted) if posted else None)
    check("a board post exists", posted is not None, True)
    if posted:
        check("...authored by the watcher, not an agent name",
              posted["author"], paths.WATCHER)
        check("...names the archive file", archive_path.name in posted["body"], True)
        check("...names the pre-restore snapshot too",
              "pre-restore snapshot" in posted["body"], True)


# --- section 7: no such archive, and a live writer during restore --------------

def section_missing_archive() -> None:
    hr("7. restoring from a path that does not exist fails loudly")
    fresh_env("missing")
    conn = db.connect()
    missing = paths.ARCHIVE_DIR / "agentdesk-19700101T000000.db"
    raised = False
    try:
        backup.restore(missing, conn)
    except FileNotFoundError:
        raised = True
    check("a missing archive raises FileNotFoundError, not a silent no-op",
          raised, True)
    conn.close()


def section_concurrent_writer() -> None:
    hr("8. a live writer holding the database is ordered cleanly, never torn")
    note("A second connection opens BEGIN IMMEDIATE and holds it, forcing the")
    note("restore's backup() call to contend for the lock exactly as a live")
    note("MCP server session would. If restore used a file copy instead of the")
    note("backup API this would deadlock or corrupt rather than simply wait.")
    note("")
    note("What this does NOT prove, and does not need to: that a concurrent")
    note("write 'wins' against a restore that is already in flight. It cannot")
    note("-- a write already holding the lock when restore() starts is, by")
    note("construction, one more thing that happened before the archive's")
    note("moment, and a restore's whole job is discarding exactly that. The")
    note("property that matters is that it is discarded CLEANLY (integrity")
    note("check passes, no exception on either side) rather than corrupted or")
    note("half-applied.")
    print()

    fresh_env("concurrent")
    conn = db.connect()
    db.init_db(conn)
    post(conn, "discussion", "pre-writer", "builder", "present before the writer test")
    archive_path = backup.snapshot(conn)

    lock_held = threading.Event()
    writer_done = threading.Event()
    writer_error: list[BaseException] = []

    def writer() -> None:
        try:
            wconn = sqlite3.connect(str(paths.DB_PATH), timeout=30.0,
                                    isolation_level=None)
            wconn.execute("PRAGMA busy_timeout=30000")
            wconn.execute("BEGIN IMMEDIATE")
            lock_held.set()
            time.sleep(0.4)  # hold the lock while restore() is in flight
            wconn.execute(
                "INSERT INTO threads (created_ts, updated_ts, channel, subject,"
                " opened_by, status) VALUES (datetime('now'), datetime('now'),"
                " 'discussion', 'writer landed', 'writer-thread', 'fyi')"
            )
            wconn.commit()
            wconn.close()
        except BaseException as e:  # noqa: BLE001 - reported, not swallowed
            writer_error.append(e)
        finally:
            writer_done.set()

    t = threading.Thread(target=writer, daemon=True)
    t.start()
    if not lock_held.wait(timeout=5):
        FAILURES.append("writer thread never acquired its lock -- test setup broken")

    started = time.monotonic()
    outcome = backup.restore(archive_path, conn)
    elapsed = time.monotonic() - started
    show("restore() took (seconds)", round(elapsed, 3))
    check("restore actually waited on the held lock rather than erroring",
          elapsed >= 0.3, True)

    writer_done.wait(timeout=5)
    show("writer thread error", writer_error[0] if writer_error else None)
    check("the concurrent writer was not rejected or corrupted",
          len(writer_error), 0)

    integrity = conn.execute("PRAGMA integrity_check").fetchone()[0]
    show("PRAGMA integrity_check after concurrent restore", integrity)
    check("the database is not corrupted", integrity, "ok")

    subjects = [r["subject"] for r in conn.execute("SELECT subject FROM threads")]
    show("threads present after restore + writer settle", subjects)
    check("the writer's row was cleanly superseded, same as any other "
          "pre-restore write (not left half-applied)",
          "writer landed" in subjects, False)
    check("...and the archive's own row is exactly what remains",
          subjects, ["pre-writer"])

    conn.close()


# --- main -----------------------------------------------------------------------

def main() -> int:
    print(f"scratch board: {paths.DB_PATH}")
    print(f"scratch archive dir: {paths.ARCHIVE_DIR}")
    paths.ensure_dirs()

    section_basic_restore()
    section_refuse_forward()
    section_dry_run()
    section_posts_to_board()
    section_missing_archive()
    section_concurrent_writer()

    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print(f"\nscratch board and archive left at {_SCRATCH}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
