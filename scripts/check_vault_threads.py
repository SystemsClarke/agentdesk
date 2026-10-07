"""Acceptance checks for the board-in-the-vault integration: a note per thread
with working wikilinks, a map, and the hourly commit + push of all of it.

Everything runs in a scratch LOCALAPPDATA, a scratch vault (a real git repo) and
a scratch bare repository standing in for GitHub: John's vault is never opened.

    python scripts/check_vault_threads.py
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-vaultthreads-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from agentdesk import backup, db, paths, vault_threads  # noqa: E402

_VAULT = _SCRATCH / "vault"
paths.VAULT_DIR = _VAULT
paths.VAULT_AGENTDESK = _VAULT / "agentdesk"
paths.VAULT_LOG = _VAULT / "log"
paths.VAULT_NOTES = _VAULT / "notes"
paths.VAULT_MAPS = _VAULT / "maps"
paths.VAULT_PARKED = paths.VAULT_AGENTDESK / "parked"
paths.VAULT_QUESTIONS = paths.VAULT_AGENTDESK / "questions"

FAILURES: list[str] = []


def check(label: str, got, want) -> None:
    ok = got == want
    if not ok:
        FAILURES.append(f"{label}: got {got!r}, wanted {want!r}")
    print(f"  [{'ok' if ok else 'FAIL'}] {label}: {got!r}")


def git(*args: str, cwd: Path = _VAULT) -> str:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=True).stdout.strip()


def main() -> int:
    remote = _SCRATCH / "remote.git"
    subprocess.run(["git", "init", "-q", "--bare", "-b", "Main", str(remote)], check=True)
    _VAULT.mkdir(parents=True)
    git("init", "-q", "-b", "Main")
    git("config", "user.name", "tester")
    git("config", "user.email", "t@example.com")
    git("remote", "add", "origin", str(remote))
    (_VAULT / "Home.md").write_text("home\n")
    git("add", "Home.md")
    git("-c", "commit.gpgsign=false", "commit", "-q", "-m", "seed")
    git("push", "-q", "-u", "origin", "Main")
    (_VAULT / "notes").mkdir()
    (_VAULT / "notes" / "someone-elses-draft.md").write_text("mid-write\n")  # must not be swept up

    paths.ensure_dirs()
    conn = db.connect()
    db.init_db(conn)
    bio = db.start_thread(conn, "discussion", "bio: builder", "builder", paths.AGENT_KIND, "I build things")
    t = db.start_thread(conn, "discussion", 'Fix "quotes" in the parser', "builder", paths.AGENT_KIND,
                        "Working on it, see thread 999 and thread %d" % bio)
    conn.close()

    print("\n-- first run")
    r = backup.run_once(json_out=True)
    note = paths.VAULT_AGENTDESK / "threads" / f"thread-{t}.md"
    text = note.read_text(encoding="utf-8")
    check("a note per thread", sorted(p.name for p in note.parent.glob("*.md")),
          sorted([f"thread-{bio}.md", f"thread-{t}.md"]))
    check("frontmatter has the subject as an alias", f'aliases: ["Fix \\"quotes\\" in the parser"]' in text, True)
    check("author links to their bio thread", f"[[agentdesk/threads/thread-{bio}|builder]]" in text, True)
    check("'thread 999' (no such thread) stays plain text", "thread 999" in text and "thread-999" not in text, True)
    check("'thread N' in a body is a link", f"thread [[agentdesk/threads/thread-{bio}|{bio}]]" in text, True)
    day = next(paths.VAULT_AGENTDESK.glob("20*.md")).read_text(encoding="utf-8")
    check("the day file links to the thread note", f"[[agentdesk/threads/thread-{t}|thread {t}]]" in day, True)
    check("the day is linked back from the thread", f"[[agentdesk/{next(paths.VAULT_AGENTDESK.glob('20*.md')).stem}]]" in text, True)
    moc = (paths.VAULT_MAPS / "AgentDesk MOC.md").read_text(encoding="utf-8")
    check("the map lists the thread", f"[[agentdesk/threads/thread-{t}|" in moc, True)
    check("committed", r["git"]["committed"], True)
    check("pushed", r["git"]["pushed"], True)
    check("the remote has it", "AgentDesk board backup" in git("log", "-1", "--format=%s", "Main", cwd=remote), True)
    check("author is claude[bot]", git("log", "-1", "--format=%an", "Main", cwd=remote), "claude[bot]")
    check("another session's file was not committed",
          "notes/" in git("status", "--porcelain"), True)
    check("...and is not in the commit",
          "someone-elses-draft" in git("show", "--stat", "--format=", "HEAD"), False)

    print("\n-- second run, nothing new")
    head = git("rev-parse", "HEAD")
    r2 = backup.run_once(json_out=True)
    check("no thread note rewritten", r2["threads_written"], 0)
    check("no new commit when only the hourly noise is unchanged", r2["git"]["committed"] and git("rev-parse", "HEAD") != head, False)

    print("\n-- a reply lands")
    conn = db.connect()
    db.reply(conn, t, "builder", paths.AGENT_KIND, "Done.") if hasattr(db, "reply") else None
    conn.close()

    print("\n-- the remote is gone: the backup still finishes and says so")
    git("remote", "set-url", "origin", str(_SCRATCH / "nowhere.git"))
    (paths.VAULT_AGENTDESK / "extra.md").write_text("x\n")
    r3 = backup.run_once(json_out=True)
    check("it committed locally", r3["git"]["committed"], True)
    check("the failure is reported, not raised", bool(r3["git"]["error"]), True)
    check("and counted as unpushed", r3["git"]["unpushed"], 1)

    if FAILURES:
        print(f"\n{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print("  -", f)
        return 1
    print("\nevery check held.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
