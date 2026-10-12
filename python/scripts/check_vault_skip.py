"""Acceptance checks for item #179: paths.VAULT_DIR does not follow a
LOCALAPPDATA override, so an ad hoc run of backup.run_once() against a
scratch database can still write into John's REAL vault. Confirmed the hard
way on 2026-09-19: a "quick sanity check" of the CLI clobbered a whole day's
real transcript with an empty one.

The fix has two halves and this file proves both:

  1. A documented trap (paths.py's comment on VAULT_DIR) -- not machine
     checkable, read it yourself if you want that half verified.
  2. A functional escape hatch: `run_once(skip_vault=True)` / `--skip-vault`
     on the CLI, which skips the vault write entirely rather than relying on
     the caller to know paths.py's internals.               (sections 1-3)

WHY THIS SCRIPT NEVER TOUCHES THE REAL VAULT, EVEN THOUGH IT IS TESTING A BUG
ABOUT THE REAL VAULT BEING TOUCHED: same isolation pattern as check_vault.py
and check_archive.py -- LOCALAPPDATA is overridden AND every paths.VAULT_*
constant is reassigned to a scratch directory, before agentdesk is imported.
Proving "--skip-vault leaves the vault alone" by pointing it at the real
vault and hoping the flag works would be exactly the mistake this item is
about, done a second time to test the fix for the first.

    .venv\\Scripts\\python.exe scripts\\check_vault_skip.py
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

# --- isolation, BEFORE agentdesk is imported -----------------------------------
_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-vaultskip-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import backup, db, paths  # noqa: E402

_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = paths.VAULT_AGENTDESK / "questions"

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


def day_file() -> Path:
    today = __import__("datetime").datetime.now().astimezone().date()
    return paths.VAULT_AGENTDESK / f"{today.isoformat()}.md"


# --- section 1: the real trap this item is about --------------------------------

def section_the_trap_confirmed() -> None:
    hr("1. confirming the trap still exists (VAULT_DIR ignores LOCALAPPDATA)")
    note("This is what item #179 found: overriding LOCALAPPDATA changes")
    note("DATA_DIR/DB_PATH/ARCHIVE_DIR but does nothing to VAULT_DIR, because")
    note("VAULT_DIR is a hardcoded absolute path, not derived from it.")
    print()
    import importlib
    fresh_paths = importlib.import_module("agentdesk.paths")
    show("os.environ['LOCALAPPDATA']", os.environ["LOCALAPPDATA"])
    show("paths.DATA_DIR (follows it)", str(fresh_paths.DATA_DIR))
    check("DATA_DIR is under the scratch LOCALAPPDATA",
          str(fresh_paths.DATA_DIR).startswith(os.environ["LOCALAPPDATA"]), True)
    # VAULT_DIR was already reassigned above in THIS script -- the point being
    # made is that reassignment is a deliberate manual step, not something
    # LOCALAPPDATA does automatically. Demonstrate that by re-reading the raw
    # source default rather than the module's current (already-patched) value.
    src = (REPO / "agentdesk" / "paths.py").read_text(encoding="utf-8")
    default_line = [l for l in src.splitlines() if l.startswith("VAULT_DIR =")][0]
    show("VAULT_DIR's actual source line", default_line.strip())
    check("...is a hardcoded literal path, no os.environ reference",
          "os.environ" in default_line, False)


# --- section 2: skip_vault=True leaves the vault untouched ----------------------

def section_skip_vault_python() -> None:
    hr("2. run_once(skip_vault=True) touches the database, not the vault")
    paths.ensure_dirs()

    # Seed the scratch vault with pre-existing content standing in for "a real
    # day's transcript that must not be clobbered."
    df = day_file()
    sentinel = "# THIS IS THE PRE-EXISTING REAL TRANSCRIPT -- must survive\n"
    df.write_text(sentinel, encoding="utf-8")
    before_mtime = df.stat().st_mtime_ns
    show("scratch vault day file seeded with", sentinel.strip())

    conn = db.connect()
    db.init_db(conn)
    db.start_thread(conn, "discussion", "should not reach the vault",
                    "builder", paths.AGENT_KIND, "posted after skip_vault seed")
    conn.close()

    result = backup.run_once(json_out=True, skip_vault=True)
    show("run_once(skip_vault=True) result", result)
    check("result['vault'] is None", result["vault"], None)
    check("the day file's content is untouched",
          df.read_text(encoding="utf-8"), sentinel)
    check("...and its mtime never moved",
          df.stat().st_mtime_ns, before_mtime)
    check("the snapshot still happened (skip_vault only skips the vault)",
          Path(result["snapshot"]).exists(), True)


# --- section 3: default behaviour (no flag) still writes the vault --------------

def section_default_still_writes() -> None:
    hr("3. the default (no flag) still writes the vault -- no regression")
    conn = db.connect()
    db.start_thread(conn, "discussion", "should reach the vault this time",
                    "builder", paths.AGENT_KIND, "posted before a real run_once")
    conn.close()

    result = backup.run_once(json_out=True, skip_vault=False)
    show("run_once(skip_vault=False) result", result)
    check("result['vault'] is a real path", result["vault"] is not None, True)
    content = day_file().read_text(encoding="utf-8")
    check("the sentinel from section 2 is GONE (the file was really rewritten)",
          "PRE-EXISTING REAL TRANSCRIPT" in content, False)
    check("...and the new message is really in it",
          "should reach the vault this time" in content, True)


# --- section 4: the actual CLI entry point, argparse and all -------------------

def section_cli_end_to_end() -> None:
    hr("4. `agentdesk.backup.main()` with argv=['--skip-vault', '--json']")
    note("Exercising the real argparse wiring, not just the Python function --")
    note("a typo'd dest= or a flag that never reaches the call would look fine")
    note("in section 2 and still be broken here.")
    note("")
    note("Deliberately NOT a subprocess: a real `python -m agentdesk.backup`")
    note("child process would re-import paths.py fresh, getting the REAL,")
    note("hardcoded VAULT_DIR -- this script's monkeypatch only applies to")
    note("modules already loaded in THIS process. If --skip-vault were broken,")
    note("a subprocess test would write into the real vault to prove it, which")
    note("is the exact incident #179 is about. Calling main() in-process uses")
    note("the already-patched `paths` module, so a bug here fails loudly")
    note("against the scratch vault instead of silently against the real one.")
    print()

    df = day_file()
    sentinel = "# CLI-ENTRY-POINT TEST SENTINEL -- --skip-vault must leave this alone\n"
    df.write_text(sentinel, encoding="utf-8")

    import contextlib
    import io
    argv_before = sys.argv
    sys.argv = ["backup.py", "--skip-vault", "--json"]
    out = io.StringIO()
    try:
        with contextlib.redirect_stdout(out):
            backup.main()
    finally:
        sys.argv = argv_before
    stdout = out.getvalue()
    show("stdout", stdout.strip())

    try:
        payload = json.loads(stdout.strip().splitlines()[-1])
    except (ValueError, IndexError):
        payload = None
    show("parsed JSON", payload)
    check("main() with --skip-vault --json produced valid JSON",
          payload is not None, True)
    check("...and its vault field is null",
          payload.get("vault") if payload else "NOT JSON", None)
    check("the scratch vault file is untouched by the real CLI entry point",
          df.read_text(encoding="utf-8"), sentinel)


# --- main -----------------------------------------------------------------------

def main() -> int:
    print(f"scratch board: {paths.DB_PATH}")
    print(f"scratch vault: {paths.VAULT_DIR}")
    paths.ensure_dirs()

    section_the_trap_confirmed()
    section_skip_vault_python()
    section_default_still_writes()
    section_cli_end_to_end()

    hr()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        print(f"\nscratch left at {_SCRATCH}")
        return 1
    print("every printed property held.")
    print(f"\nscratch board and vault left at {_SCRATCH}")
    print("(the REAL vault was never opened -- see the module docstring)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
