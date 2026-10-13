"""EXPLAIN QUERY PLAN for the window's slowest list query (`Db::list_threads`) on a COPY of a board, with and without the indexes
the window relies on. Standard library only.

    python scripts/ui_explain.py path/to/copy-of-agentdesk.db

Never point it at the live board: it creates indexes (in a throwaway copy it makes itself, next to the file you name).
See docs/ui-bench.md for how to make the copy."""
import os
import re
import shutil
import sqlite3
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def const(src, name):
    m = re.search(r"(?:pub )?const %s: &str = \"((?:[^\"\\]|\\.)*)\";" % name, src, re.S)
    return re.sub(r"\\\n\s*", "", m.group(1)).replace('\\"', '"')  # Rust line continuations, escaped quotes


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    work = sys.argv[1] + ".explain.tmp"
    shutil.copy(sys.argv[1], work)
    c = sqlite3.connect(work)
    src = open(os.path.join(ROOT, "src", "db.rs"), encoding="utf8").read()
    waiting, follow_where, follow_mark = const(src, "WAITING_SQL"), const(src, "FOLLOW_UP_WHERE"), const(src, "FOLLOW_UP_MARK")
    sql = (
        "SELECT t.*, COUNT(m.id) AS message_count,"
        " (SELECT author FROM messages WHERE thread_id=t.id AND COALESCE(CASE WHEN json_valid(meta) THEN json_extract(meta,'$.kind') END,'') NOT IN ('ack','ack-note','read-receipt') ORDER BY id DESC LIMIT 1) AS last_author,"
        " (SELECT COALESCE(CASE WHEN a.state='posted' THEN 'picked-up' END, d.state, CASE WHEN a.message_id IS NOT NULL THEN 'pending' END, '') || '|' || h.ts"
        "  FROM messages h LEFT JOIN acks a ON a.message_id=h.id LEFT JOIN deliveries d ON d.message_id=h.id"
        "  WHERE h.thread_id=t.id AND h.author_kind='human' ORDER BY h.id DESC LIMIT 1) AS delivery,"
        " (CASE WHEN %s THEN 1 ELSE 0 END) AS waiting,"
        " (SELECT %s FROM messages f WHERE f.thread_id=t.id AND %s ORDER BY f.id DESC LIMIT 1) AS follow_up"
        " FROM (SELECT * FROM threads t WHERE t.channel = ? ORDER BY t.updated_ts DESC, t.id DESC LIMIT 300) t LEFT JOIN messages m ON m.thread_id = t.id"
        " GROUP BY t.id ORDER BY t.updated_ts DESC, t.id DESC"
    ) % (waiting, follow_mark, follow_where)

    def run(label):
        print("--- " + label)
        for r in c.execute("EXPLAIN QUERY PLAN " + sql, ("discussion",)):
            print("   ", r[3])
        for ch in ("question", "discussion", "work"):
            ts = []
            for _ in range(8):
                t = time.perf_counter()
                c.execute(sql, (ch,)).fetchall()
                ts.append((time.perf_counter() - t) * 1000)
            ts.sort()
            print("    %-10s median %6.2f ms  min %6.2f ms" % (ch, ts[len(ts) // 2], ts[0]))

    print("sqlite", sqlite3.sqlite_version)
    have = {r[1] for r in c.execute("SELECT * FROM sqlite_master WHERE type='index'")}
    if "idx_messages_thread_kind" in have:
        c.execute("DROP INDEX idx_messages_thread_kind")
    run("without idx_messages_thread_kind (a board from before it)")
    c.execute("CREATE INDEX idx_messages_thread_kind ON messages(thread_id, author_kind, id)")
    c.execute("ANALYZE")
    run("with idx_messages_thread_kind (created by Db::init)")
    c.close()
    os.remove(work)


main()
