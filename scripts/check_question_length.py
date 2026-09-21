"""Acceptance checks for the question word limit.

John, twice: first "I think we should put a limit on question of 450 words"
(thread 10), then plainly "I really would like to have the ability to have the
questions be limited in size to like four or five sentences, maybe 400 words,
so that they're more parsable for me." The measured case for why it matters is
thread 83: nine agent messages, several over 500 words, before the actual ask
changed -- by the end nobody could tell what was still being asked without
reading the whole thread.

The limit is enforced in db.py (start_thread and reply, through the one shared
`_enforce_question_length`), not in mcp_server.py, so every caller gets it for
free and there is exactly one place to change the number.

    .venv\\Scripts\\python.exe scripts\\check_question_length.py
"""

from __future__ import annotations

import os
import sys
import tempfile
from pathlib import Path

_SCRATCH = Path(tempfile.mkdtemp(prefix="agentdesk-qlen-"))
os.environ["LOCALAPPDATA"] = str(_SCRATCH)

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

from agentdesk import db, paths  # noqa: E402

FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if not ok:
        FAILURES.append(label + (f" ({detail})" if detail else ""))
    print(f"  [{'ok' if ok else 'FAIL'}] {label}" + (f" -- {detail}" if detail else ""))


def words(n: int) -> str:
    return "word " * n


def main() -> int:
    conn = db.connect()
    db.init_db(conn)

    print("=== a short question from an agent: allowed ===")
    tid = db.start_thread(conn, "question", "short", "claude",
                          paths.AGENT_KIND, words(25))
    check("short agent question is accepted", isinstance(tid, int))

    print("\n=== a long question from an agent: rejected, not truncated ===")
    try:
        db.start_thread(conn, "question", "long", "claude",
                        paths.AGENT_KIND, words(db.QUESTION_WORD_LIMIT + 50))
        check("long agent question is rejected", False)
    except ValueError as exc:
        check("long agent question is rejected", True)
        check("the error names the actual count and the limit",
              str(db.QUESTION_WORD_LIMIT + 50) in str(exc)
              and str(db.QUESTION_WORD_LIMIT) in str(exc), str(exc))
    check("the rejected question left no thread behind",
          len(db.list_threads(conn, channel="question")) == 1)

    print("\n=== exactly the limit is allowed; one word over is not ===")
    tid2 = db.start_thread(conn, "question", "at the limit", "claude",
                           paths.AGENT_KIND, words(db.QUESTION_WORD_LIMIT))
    check(f"exactly {db.QUESTION_WORD_LIMIT} words is accepted",
          isinstance(tid2, int))
    try:
        db.reply(conn, tid2, "claude", paths.AGENT_KIND,
                 words(db.QUESTION_WORD_LIMIT + 1))
        check(f"{db.QUESTION_WORD_LIMIT + 1} words is rejected", False)
    except ValueError:
        check(f"{db.QUESTION_WORD_LIMIT + 1} words is rejected", True)

    print("\n=== a long AGENT REPLY into an open question thread: rejected ===")
    print("    (thread 83's actual failure mode -- not the opening ask)")
    try:
        db.reply(conn, tid, "gocd-pipeline-ops", paths.AGENT_KIND,
                 words(db.QUESTION_WORD_LIMIT + 200))
        check("long agent follow-up into a question thread is rejected", False)
    except ValueError:
        check("long agent follow-up into a question thread is rejected", True)

    print("\n=== John's own words are never limited -- it is his board ===")
    mid = db.reply(conn, tid, "john", paths.HUMAN_KIND,
                   words(db.QUESTION_WORD_LIMIT + 500))
    check("a long human reply is accepted", isinstance(mid, int))

    print("\n=== the limit is specific to the question channel ===")
    tid3 = db.start_thread(conn, "discussion", "long discussion", "claude",
                           paths.AGENT_KIND, words(db.QUESTION_WORD_LIMIT + 500))
    check("a long agent DISCUSSION post is unaffected", isinstance(tid3, int))
    tid4 = db.start_thread(conn, "work", "long work item", "claude",
                           paths.AGENT_KIND, words(db.QUESTION_WORD_LIMIT + 500))
    check("a long agent WORK item is unaffected", isinstance(tid4, int))

    conn.close()

    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
