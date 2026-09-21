"""The backup. Snapshots the database hourly and writes the day's messages into
the memory vault as plain markdown, which is the only copy that survives the
machine."""

import argparse
import json
import sqlite3
from datetime import date, datetime, time, timedelta, timezone
from pathlib import Path

from . import db, paths

# The snapshot filename carries its UTC timestamp so prune_archive can be
# judged by the name rather than by a file mtime, which a copy or a sync
# client can reset.
_SNAPSHOT_STAMP = "%Y%m%dT%H%M%S"

# This module deliberately contains no git calls, even though the vault is a
# git repository. Commits on the vault carry an explicit authorship convention
# that belongs to a human decision, and an hourly job must not author a commit
# an hour. If you are about to add a git call here, put it behind a human
# instead.


def _to_local(ts: str) -> datetime:
    """Parse a timestamp from the database into a local aware datetime.

    The stored strings are UTC from now_iso(); a naive value is tolerated so a
    hand-inserted row cannot crash the backup.
    """
    dt = datetime.fromisoformat(ts)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone()


def _local_midnight_utc(local_date: date) -> str:
    """Local midnight on `local_date`, as a UTC string in now_iso()'s format.

    astimezone() called on a NAIVE datetime is the platform's local-time
    conversion: it applies the UTC offset actually in force at that date and
    time, including a DST shift.
    """
    return (
        datetime.combine(local_date, time.min)
        .astimezone(timezone.utc)
        .isoformat(timespec="seconds")
    )


def _day_bounds_utc(local_date: date) -> tuple[str, str]:
    """The [start, end) of a local day as UTC strings in now_iso()'s format.

    The day boundary is decided in local time first, then converted, or a
    message sent at 20:00 local lands in tomorrow's file. The boundaries are
    formatted through isoformat() so SQL's string comparison against the stored
    timestamps compares like with like.

    The two ends are converted SEPARATELY, each from its own naive local
    midnight, because they do not always share a UTC offset -- on the day the
    clocks shift, the local day is 23 or 25 hours long. Computing the end as
    start + timedelta(days=1) instead does wall-clock arithmetic on an aware
    datetime, which adds exactly 24 hours and keeps the start's offset, so on
    a DST day the last hour (or the first) is filed under the wrong date.
    """
    return (
        _local_midnight_utc(local_date),
        _local_midnight_utc(local_date + timedelta(days=1)),
    )


def snapshot(conn: sqlite3.Connection) -> Path:
    """A consistent copy of the live database into paths.ARCHIVE_DIR, named
    agentdesk-<UTC timestamp>.db.

    Uses SQLite's online backup API rather than a file copy: the database is in
    WAL mode with a writer that may be mid-transaction, and copying the .db file
    alone yields a torn snapshot that is missing whatever is still in the
    write-ahead log. conn.backup() copies a committed page image instead.

    The stamp is second-resolution, so two snapshots inside the same second
    (the hourly cadence never does this, but restore() taking its own
    pre-restore snapshot can land in the same second as a snapshot just taken
    by hand) get a `-N` suffix rather than colliding. Silently overwriting an
    existing archive under its own name is the one failure a backup function
    must never have -- it can turn "restore from X" into "X no longer exists,
    a different file does" with nothing on screen to say so.
    """
    paths.ensure_dirs()
    stamp = datetime.now(timezone.utc).strftime(_SNAPSHOT_STAMP)
    dest_path = paths.ARCHIVE_DIR / f"agentdesk-{stamp}.db"
    if dest_path.exists():
        n = 1
        while (candidate := paths.ARCHIVE_DIR / f"agentdesk-{stamp}-{n}.db").exists():
            n += 1
        dest_path = candidate
    dest = sqlite3.connect(str(dest_path))
    try:
        conn.backup(dest)
    finally:
        dest.close()
    return dest_path


def render_day(conn: sqlite3.Connection, local_date: date) -> str:
    """The full markdown for one local day: a header with counts, the threads
    touched that day, then every message in order."""
    start, end = _day_bounds_utc(local_date)
    rows = [
        dict(r)
        for r in conn.execute(
            "SELECT m.id, m.ts, m.thread_id, m.author, m.author_kind, m.body,"
            "       m.reply_to, t.subject, t.channel, t.status, t.opened_by"
            " FROM messages m JOIN threads t ON t.id = m.thread_id"
            " WHERE m.ts >= ? AND m.ts < ?"
            " ORDER BY m.id",
            (start, end),
        )
    ]

    human = sum(1 for r in rows if r["author_kind"] == paths.HUMAN_KIND)
    agent = len(rows) - human
    threads = len({r["thread_id"] for r in rows})

    lines = [f"# AgentDesk daily transcript - {local_date.isoformat()}", ""]
    lines.append(f"{threads} threads touched, {len(rows)} messages "
                 f"({human} human, {agent} agent).")
    # A day with no messages still gets its file, so a gap in the daily series
    # reads as "nothing happened" rather than as "the backup did not run".
    if not rows:
        lines += ["", "Nothing happened on this day - the backup ran and found no messages."]

    lines += ["", "## Threads touched", ""]
    if rows:
        seen: set[int] = set()
        for r in rows:
            if r["thread_id"] in seen:
                continue
            seen.add(r["thread_id"])
            lines.append(
                f'- thread {r["thread_id"]}: "{r["subject"]}" - channel {r["channel"]},'
                f' status {r["status"]}, opened by {r["opened_by"]}'
            )
    else:
        lines.append("None.")

    lines += ["", "## Messages", ""]
    if not rows:
        lines.append("None.")
    for r in rows:
        stamp_local = _to_local(r["ts"]).strftime("%H:%M")
        reply_note = f" (reply to #{r['reply_to']})" if r["reply_to"] else ""
        # Continuation lines are indented so a multi-line body stays inside the
        # list item instead of breaking out of it.
        body = r["body"].replace("\r\n", "\n").replace("\n", "\n  ")
        lines.append(
            f'- {stamp_local} **{r["author"]}** ({r["author_kind"]})'
            f' [thread {r["thread_id"]}]{reply_note} - {body}'
        )
    return "\n".join(lines).rstrip("\n") + "\n"


def write_vault(conn: sqlite3.Connection, local_date: date | None = None) -> Path:
    """Rewrite paths.VAULT_AGENTDESK/<local-date>.md from the database and make
    sure the vault log for that day carries the pointer line.

    The day file is rewritten whole every run rather than appended to: it is
    derived data, so a rewrite is idempotent, where an hourly append would grow
    the same message into the vault a dozen times a day. Transcripts live in
    agentdesk/ only, never in the vault's notes/ directory, which is hand-written
    atomic memories under a search index.
    """
    if local_date is None:
        local_date = datetime.now().astimezone().date()
    text = render_day(conn, local_date)
    paths.ensure_dirs()

    day_file = paths.VAULT_AGENTDESK / f"{local_date.isoformat()}.md"
    with day_file.open("w", encoding="utf-8", newline="\n") as f:
        f.write(text)

    # The pointer line is keyed to the date alone. Putting message counts in it
    # would change the line through the day and defeat the idempotence check
    # below, which compares whole lines.
    pointer = f"- [[agentdesk/{local_date.isoformat()}]] - AgentDesk daily transcript"
    log_file = paths.VAULT_LOG / f"{local_date.isoformat()}.md"
    if log_file.exists():
        existing = log_file.read_text(encoding="utf-8")
        if pointer not in existing.splitlines():
            # If the file does not end on a newline, a plain append would weld
            # the pointer onto the last existing line.
            prefix = "" if (not existing or existing.endswith("\n")) else "\n"
            with log_file.open("a", encoding="utf-8", newline="\n") as f:
                f.write(prefix + pointer + "\n")
    else:
        with log_file.open("w", encoding="utf-8", newline="\n") as f:
            f.write(pointer + "\n")
    return day_file


def _archive_stamp(f: Path) -> datetime | None:
    """The UTC timestamp encoded in an archive's filename, or None if `f` is
    not one of ours (a foreign .db file dropped into ARCHIVE_DIR must not be
    treated as a snapshot by prune_archive or restore).

    Splits off a trailing `-N` disambiguator (see snapshot()) before parsing,
    so a same-second collision's second file still prunes and restores by the
    same clock time as the first.
    """
    prefix_len = len("agentdesk-")
    raw = f.stem[prefix_len:].split("-", 1)[0]
    try:
        return datetime.strptime(raw, _SNAPSHOT_STAMP).replace(tzinfo=timezone.utc)
    except ValueError:
        return None


def prune_archive(keep_days: int = 14) -> int:
    """Delete snapshots older than keep_days, judged by the timestamp in the
    filename. Returns how many went."""
    cutoff = datetime.now(timezone.utc) - timedelta(days=keep_days)
    pruned = 0
    for f in paths.ARCHIVE_DIR.glob("agentdesk-*.db"):
        stamp = _archive_stamp(f)
        if stamp is None:
            continue  # not one of ours; leave it alone
        if stamp < cutoff:
            f.unlink()
            pruned += 1
    return pruned


def _row_counts(conn: sqlite3.Connection) -> dict:
    return {
        "threads": conn.execute("SELECT COUNT(*) FROM threads").fetchone()[0],
        "messages": conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0],
    }


class RestoreRefused(Exception):
    """The restore did not run because a safety check declined it -- not a
    failure of the backup API itself. The caller decides whether to override."""


def restore(archive_path: Path, live_conn: sqlite3.Connection,
           *, force_newer: bool = False) -> dict:
    """Restore `live_conn`'s database from an archive snapshot, in place, with
    live writers present.

    Uses SQLite's online backup API in reverse from snapshot(): opening the
    archive and backing IT into the live connection, rather than copying the
    file. This is the one method that is correct with concurrent writers on
    live_conn, for the same reason snapshot() is -- it goes through SQLite's
    own page-level locking, so a writer that collides with the copy is made to
    wait (the backup API's own retry-with-sleep loop) rather than torn or
    rejected. A file copy is wrong twice over here: `live_conn`'s process
    keeps its handle to the old inode after a copy (its next commit vanishes
    into unlinked space), and a copy taken from a live WAL-mode database can
    itself be torn.

    Refuses when the archive's own timestamp is after the live database's
    latest known message -- restoring "forward" like that is not what a
    restore is for, and is far more likely to be the wrong archive picked than
    an emergency -- unless force_newer overrides it.

    Snapshots the current (about-to-be-overwritten) state first, so the one
    operation meant to recover data cannot be the one operation that destroys
    it without a way back.

    Does not touch notify_state.json: the announced-open-question ids it
    remembers will not match the restored set, so a toast may re-fire or miss
    one until the next cycle. Known and accepted, not silently swallowed --
    see thread 82 section 8.3.
    """
    archive_path = Path(archive_path)
    if not archive_path.is_file():
        raise FileNotFoundError(f"no such archive: {archive_path}")

    stamp = _archive_stamp(archive_path)
    live_latest_raw = live_conn.execute(
        "SELECT MAX(ts) FROM messages").fetchone()[0]
    live_latest = _to_local(live_latest_raw).astimezone(timezone.utc) \
        if live_latest_raw else None

    if stamp is not None and live_latest is not None and stamp > live_latest \
            and not force_newer:
        raise RestoreRefused(
            f"archive {archive_path.name} is stamped {stamp.isoformat()}, "
            f"after the live database's latest message "
            f"({live_latest.isoformat()}) -- restoring forward is refused "
            f"by default; pass force_newer=True to do it anyway"
        )

    pre_restore_snapshot = snapshot(live_conn)
    before = _row_counts(live_conn)

    archive_conn = sqlite3.connect(str(archive_path))
    try:
        archive_conn.backup(live_conn)
    finally:
        archive_conn.close()

    # The restored file may predate a schema change (a new column, the
    # open_questions view definition) -- init_db is idempotent and this is
    # what makes a restore from an older snapshot come back current rather
    # than quietly missing whatever shipped since.
    db.init_db(live_conn)
    after = _row_counts(live_conn)

    return {
        "archive": str(archive_path),
        "pre_restore_snapshot": str(pre_restore_snapshot),
        "before": before,
        "after": after,
    }


def run_once(json_out: bool = False, skip_vault: bool = False) -> dict:
    """One full cycle: snapshot the database, rewrite the vault day file, prune
    old snapshots.

    Prints a one-line summary, or the whole result as JSON when json_out is set.
    The CLI and the scheduled task both go through here so the two cannot drift.

    skip_vault exists because paths.VAULT_DIR is a hardcoded absolute path that
    does NOT follow a LOCALAPPDATA override the way DATA_DIR/DB_PATH/ARCHIVE_DIR
    do (see the comment on VAULT_DIR) -- so a scratch/ad hoc run of this
    function against an isolated database can still write into John's real
    vault unless it either reassigns every VAULT_* constant first, or passes
    skip_vault=True to opt out of the vault step entirely. Production (the
    scheduled hourly task and a plain `python -m agentdesk.backup`) leaves this
    False; it exists for the case that clobbered a real transcript on
    2026-09-19 (item #179).
    """
    paths.ensure_dirs()
    conn = db.connect()
    try:
        # Idempotent, so the backup can run on a machine where nothing else has
        # created the schema yet.
        db.init_db(conn)
        today = datetime.now().astimezone().date()
        snap = snapshot(conn)
        vault = None if skip_vault else write_vault(conn, today)
        pruned = prune_archive()
        n_messages = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    finally:
        conn.close()

    result = {
        "day": today.isoformat(),
        "messages": n_messages,
        "snapshot": str(snap),
        "vault": str(vault) if vault is not None else None,
        "pruned": pruned,
    }
    if json_out:
        print(json.dumps(result, ensure_ascii=False))
    else:
        vault_note = f"vault {Path(result['vault']).name}" if vault is not None \
            else "vault SKIPPED (--skip-vault)"
        print(f"agentdesk backup: day {result['day']}, {n_messages} message(s), "
              f"snapshot {snap.name}, {vault_note}, {pruned} pruned")
    return result


def restore_cli(archive_path: Path, *, yes: bool, force_newer: bool = False,
                json_out: bool = False) -> dict:
    """The `--restore` entry point: dry-run by default, `--yes` to act.

    A dry run prints both paths and both row counts and changes nothing -- the
    same shape as every other destructive AgentDesk operation, and the reason
    is the same one given for `--yes` everywhere else: the one command meant
    to fix a mistake must not itself be the mistake.
    """
    archive_path = Path(archive_path)
    paths.ensure_dirs()
    if not archive_path.is_file():
        print(f"no such archive: {archive_path}")
        raise SystemExit(1)

    conn = db.connect()
    try:
        db.init_db(conn)
        live_before = _row_counts(conn)
        arc = sqlite3.connect(str(archive_path))
        try:
            archive_counts = _row_counts(arc)
        finally:
            arc.close()

        if not yes:
            result = {
                "dry_run": True,
                "archive": str(archive_path),
                "live_db": str(paths.DB_PATH),
                "live_counts": live_before,
                "archive_counts": archive_counts,
            }
            if json_out:
                print(json.dumps(result, ensure_ascii=False))
            else:
                print("DRY RUN -- nothing changed. Pass --yes to restore for real.")
                print(f"  archive:  {archive_path}  {archive_counts}")
                print(f"  live db:  {paths.DB_PATH}  {live_before}")
            return result

        try:
            outcome = restore(archive_path, conn, force_newer=force_newer)
        except RestoreRefused as e:
            print(f"refused: {e}")
            raise SystemExit(1)

        db.start_thread(
            conn, "discussion",
            f"Database restored from {archive_path.name}",
            paths.WATCHER, paths.AGENT_KIND,
            f"Restored `{paths.DB_PATH}` from archive `{archive_path.name}`. "
            f"Row counts before: {outcome['before']}, after: {outcome['after']}. "
            f"A pre-restore snapshot was taken first: "
            f"`{Path(outcome['pre_restore_snapshot']).name}`. "
            f"notify_state.json was not touched, so a toast may re-fire or "
            f"miss one until it next runs -- known, not swallowed."
        )

        if json_out:
            print(json.dumps(outcome, ensure_ascii=False))
        else:
            print(f"restored {paths.DB_PATH} from {archive_path.name}: "
                  f"{outcome['before']} -> {outcome['after']}")
        return outcome
    finally:
        conn.close()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Run one AgentDesk backup cycle, or restore from an "
                    "archive snapshot.")
    parser.add_argument("--json", action="store_true",
                        help="print the cycle result as JSON")
    parser.add_argument("--restore", type=Path, metavar="ARCHIVE",
                        help="restore the live database from this archive "
                             "snapshot (dry run unless --yes is given)")
    parser.add_argument("--yes", action="store_true",
                        help="with --restore, actually perform it")
    parser.add_argument("--force-newer", action="store_true",
                        help="with --restore, allow restoring from an "
                             "archive stamped after the live database's "
                             "latest message")
    parser.add_argument("--skip-vault", action="store_true",
                        help="do not write the day's transcript into the "
                             "memory vault -- use this for an ad hoc/manual "
                             "run against a real database when you only want "
                             "the snapshot, not a vault write (see paths.py's "
                             "VAULT_DIR comment for why this exists)")
    args = parser.parse_args()

    if args.restore is not None:
        restore_cli(args.restore, yes=args.yes, force_newer=args.force_newer,
                   json_out=args.json)
        return
    run_once(json_out=args.json, skip_vault=args.skip_vault)


if __name__ == "__main__":
    main()
