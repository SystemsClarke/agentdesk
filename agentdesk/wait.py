"""Block until John replies on a thread, print his reply, exit. The agent's wake-up call.

An agent that asked John something runs this in the background right after
ask_human (Bash run_in_background). Claude Code re-invokes the agent when a
background command exits, so the agent wakes the moment he answers instead of
waiting for its next board write. The reply is printed as board content, labelled
with where it came from; it is not dressed up as something typed in the session.

    python agentdesk/wait.py THREAD_ID [--hours 12]
"""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from agentdesk import db, paths  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("thread_id", type=int)
    ap.add_argument("--hours", type=float, default=12)
    args = ap.parse_args()
    conn = db.connect()
    try:
        row = conn.execute("SELECT MAX(id) FROM messages WHERE thread_id=?", (args.thread_id,)).fetchone()
        after = int(row[0] or 0)
        deadline = time.monotonic() + args.hours * 3600
        while time.monotonic() < deadline:
            hit = conn.execute(
                "SELECT m.id, m.body, m.meta, t.subject FROM messages m JOIN threads t ON t.id=m.thread_id"
                " WHERE m.thread_id=? AND m.id>? AND m.author_kind=? ORDER BY m.id LIMIT 1",
                (args.thread_id, after, paths.HUMAN_KIND)).fetchone()
            if hit:
                db.set_delivery(conn, hit["id"], "watcher", "woke")
                print(f"[AgentDesk board] John replied on thread #{args.thread_id} ({hit['subject']}):\n")
                print(hit["body"])
                print(f"\nRead the thread (read_thread {args.thread_id}) for context, act on it, "
                      "and answer on that thread.")
                return 0
            time.sleep(3)
        print(f"[AgentDesk board] no reply on #{args.thread_id} after {args.hours:g}h; check open_questions later.")
        return 1
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
