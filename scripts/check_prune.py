"""Acceptance checks for archive thinning: every snapshot for 2 days, then the
newest of each local day until 14 days, then nothing; foreign files are never
touched; a snapshot is integrity-checked and a bad one is quarantined.

Runs entirely in a scratch LOCALAPPDATA.

    python scripts/check_prune.py
"""

from __future__ import annotations

import os
import sqlite3
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-prune-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from agentdesk import backup, paths  # noqa: E402

FAILURES: list[str] = []


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label}: {got!r}")


def make(stamp: datetime, suffix: str = "") -> Path:
    f = paths.ARCHIVE_DIR / f"agentdesk-{stamp.strftime('%Y%m%dT%H%M%S')}{suffix}.db"
    f.write_bytes(b"x" * 100)
    return f


def main() -> int:
    paths.ensure_dirs()
    now = datetime.now(timezone.utc).replace(microsecond=0)

    recent = [make(now - timedelta(hours=h)) for h in (1, 5, 30)]       # < 2 days
    # Four snapshots on one local day 5 days back (midday-anchored so a UTC
    # offset cannot split them across two local days).
    local_noon = (now - timedelta(days=5)).astimezone().replace(hour=12, minute=0, second=0)
    same_day = [make((local_noon + timedelta(hours=h)).astimezone(timezone.utc))
                for h in (-3, 0, 2, 4)]
    other_day = make((local_noon - timedelta(days=1)).astimezone(timezone.utc))
    too_old = [make(now - timedelta(days=15)), make(now - timedelta(days=40))]
    foreign = [paths.ARCHIVE_DIR / "notes.txt", paths.ARCHIVE_DIR / "other.db",
               paths.ARCHIVE_DIR / "agentdesk-garbage.db",
               paths.ARCHIVE_DIR / "agentdesk-20200101T000000.db.corrupt"]
    for f in foreign:
        f.write_bytes(b"keep me")

    doomed = set(backup.plan_prune(now=now))
    print("\n-- plan")
    check("recent snapshots all stay", any(f in doomed for f in recent), False)
    check("only the newest of the 5-days-ago day stays",
          sorted(f.name for f in same_day if f not in doomed), [same_day[-1].name])
    check("the other day's lone snapshot stays", other_day in doomed, False)
    check("older than 14 days goes", all(f in doomed for f in too_old), True)
    check("foreign files never planned", any(f in doomed for f in foreign), False)

    check("dry run deletes nothing", backup.prune_archive(dry_run=True) == len(
        backup.plan_prune()) and all(f.exists() for f in recent + same_day + too_old), True)
    n = backup.prune_archive()
    check("prune count", n, len(doomed))
    check("foreign files survive", all(f.exists() for f in foreign), True)
    check("the doomed are gone", any(f.exists() for f in doomed), False)

    print("\n-- integrity")
    good = paths.ARCHIVE_DIR / "agentdesk-20991231T000000.db"
    c = sqlite3.connect(str(good))
    c.execute("CREATE TABLE t(a)")
    c.commit()
    c.close()
    check("good snapshot verifies", backup.verify_snapshot(good), "ok")
    check("...and stays", good.exists(), True)
    bad = paths.ARCHIVE_DIR / "agentdesk-20991231T000001.db"
    bad.write_bytes(b"this is not a database" * 200)
    check("garbage is flagged", backup.verify_snapshot(bad) != "ok", True)
    check("...and quarantined", (bad.exists(), bad.with_name(bad.name + ".corrupt").exists()),
          (False, True))

    if FAILURES:
        print(f"\n{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("\nevery check held.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
