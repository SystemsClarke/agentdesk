"""db.py against a throwaway board: `python claude-mod/agentdesk-board/tests/test_db.py`.

Builds the board's own tables (the columns src/db.rs creates) in a temp folder, points every settings write at that
folder too (AGENTDESK_TEST_SETTINGS), and runs each op as the mod does: a child process, JSON out. Nothing outside the
temp folder is read or written.
"""
import json
import os
import sqlite3
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
DB_PY = os.path.join(HERE, "..", "hooks", "db.py")

SCHEMA = """
CREATE TABLE threads (id INTEGER PRIMARY KEY AUTOINCREMENT, created_ts TEXT NOT NULL, updated_ts TEXT NOT NULL,
    channel TEXT NOT NULL, subject TEXT NOT NULL, opened_by TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'open', meta TEXT);
CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, thread_id INTEGER REFERENCES threads(id),
    author TEXT NOT NULL, author_kind TEXT NOT NULL, body TEXT NOT NULL, reply_to INTEGER REFERENCES messages(id), meta TEXT);
CREATE TABLE presence (author TEXT PRIMARY KEY, seen_ts TEXT NOT NULL);
CREATE TABLE acks (message_id INTEGER PRIMARY KEY, thread_id INTEGER NOT NULL, agent TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending', created_ts TEXT NOT NULL, noted_ts TEXT, settled_ts TEXT);
CREATE TABLE pull_requests (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL UNIQUE, repo TEXT NOT NULL,
    number INTEGER NOT NULL, title TEXT NOT NULL, requested_by TEXT NOT NULL, requested_ts TEXT NOT NULL, thread_id INTEGER,
    state TEXT NOT NULL DEFAULT 'open', checked_ts TEXT, settled_ts TEXT, last_error TEXT, notified_ts TEXT);
CREATE TABLE goals (name TEXT PRIMARY KEY, objective TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'draft', lead TEXT NOT NULL,
    thread_id INTEGER, updated_ts TEXT NOT NULL);
CREATE TABLE permission_proposals (id INTEGER PRIMARY KEY AUTOINCREMENT, rule TEXT NOT NULL, scope TEXT NOT NULL, project_dir TEXT,
    reason TEXT NOT NULL, blocked TEXT, requested_by TEXT NOT NULL, thread_id INTEGER, status TEXT NOT NULL DEFAULT 'pending',
    created_ts TEXT NOT NULL, decided_ts TEXT, reverted_ts TEXT, settings_path TEXT, backup TEXT);
CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, ts TEXT NOT NULL, hlc INTEGER NOT NULL,
    origin TEXT NOT NULL, actor TEXT NOT NULL, actor_kind TEXT NOT NULL, actor_seq INTEGER NOT NULL, kind TEXT NOT NULL, thread_id INTEGER,
    ref TEXT, body TEXT, data TEXT, UNIQUE(origin, actor, actor_seq));
CREATE TABLE event_marks (name TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO event_marks VALUES ('node', '00000000000000aa');
"""
T = "2026-11-10T10:00:00+00:00"


def run(env, *args, stdin=""):
    p = subprocess.run([sys.executable, DB_PY, *args], input=stdin.encode("utf-8"), capture_output=True, env=env)
    return json.loads(p.stdout.decode("utf-8"))


def main():
    tmp = tempfile.mkdtemp(prefix="agentdesk-board-test-")
    db = os.path.join(tmp, "agentdesk.db")
    env = dict(os.environ, AGENTDESK_DATA=tmp, AGENTDESK_DB=db, AGENTDESK_TEST_SETTINGS=os.path.join(tmp, "settings"))
    c = sqlite3.connect(db)
    c.executescript(SCHEMA)
    q = lambda subject, meta: c.execute(
        "INSERT INTO threads (created_ts, updated_ts, channel, subject, opened_by, status, meta) VALUES (?,?,'question',?,'builder','open',?)",
        (T, T, subject, meta)).lastrowid
    ask, perm, proj = q("May I deploy?", "{}"), q("permission rule: Bash(make:*)", '{"type": "permission"}'), q("permission rule: Bash(x)", '{"type": "permission"}')
    for tid in (ask, perm, proj):
        c.execute("INSERT INTO messages (ts, thread_id, author, author_kind, body) VALUES (?,?,'builder','agent','please')", (T, tid))
    c.execute("INSERT INTO permission_proposals (rule, scope, reason, requested_by, thread_id, created_ts) VALUES ('Bash(make:*)','user','r','builder',?,?)", (perm, T))
    c.execute("INSERT INTO permission_proposals (rule, scope, project_dir, reason, requested_by, thread_id, created_ts) "
              "VALUES ('Bash(x)','project','C:\\\\Real\\\\Project','r','builder',?,?)", (proj, T))
    c.execute("INSERT INTO threads (created_ts, updated_ts, channel, subject, opened_by, status) VALUES (?,?,'discussion','hello','builder','fyi')", (T, T))
    c.commit()

    failures = []

    def check(name, cond):
        print(("ok   " if cond else "FAIL ") + name)
        if not cond:
            failures.append(name)

    out = run(env, "tab", "questions")
    check("three questions wait", out["waiting"] == 3 and len(out["rows"]) == 3)
    check("permission rows are badged", {r["badge"] for r in out["rows"]} == {"ask", "permission"})
    check("discussion tab lists its thread", len(run(env, "tab", "discussion")["rows"]) == 1)
    check("an unknown tab is an error", "error" in run(env, "tab", "nope"))

    t = run(env, "thread", str(perm))["thread"]
    check("a permission thread carries its pending proposal", t["proposal"]["status"] == "pending" and t["waiting"])

    out = run(env, "reply", str(ask), stdin="Yes, go ahead @builder.")
    check("reply succeeds", out.get("ok") is True)
    row = c.execute("SELECT status FROM threads WHERE id=?", (ask,)).fetchone()
    check("an answered question stops waiting", row[0] == "answered")
    check("the asker owes an ack", c.execute("SELECT agent, state FROM acks").fetchone() == ("builder", "pending"))
    check("mentions are stored", json.loads(c.execute("SELECT meta FROM messages ORDER BY id DESC LIMIT 1").fetchone()[0]) == {"mentions": ["builder"]})
    kinds = [r[0] for r in c.execute("SELECT kind FROM events ORDER BY seq")]
    check("reply writes msg.posted, thread.status, ack events", kinds == ["msg.posted", "thread.status", "ack"])
    check("an empty reply is refused", "error" in run(env, "reply", str(ask), stdin="   "))

    out = run(env, "permission", "approve", str(perm))
    check("approve succeeds", out.get("status") == "approved")
    allow = json.load(open(os.path.join(tmp, "settings", "user", "settings.json")))["permissions"]["allow"]
    check("the rule lands in permissions.allow", allow == ["Bash(make:*)"])
    bodies = [r[0] for r in c.execute("SELECT body FROM messages WHERE thread_id=? ORDER BY id", (perm,))]
    check("AgentDesk tells the agent, then John's line", bodies[-1] == "Approved in AgentDesk." and bodies[-2].startswith("Approved in AgentDesk: `Bash(make:*)`"))
    check("approving twice is refused", "already approved" in run(env, "permission", "approve", str(perm)).get("error", ""))

    out = run(env, "permission", "approve", str(proj))
    check("a project rule under test stays in the test folder",
          out.get("status") == "approved" and os.path.exists(os.path.join(tmp, "settings", "Project", ".claude", "settings.json")))

    c.execute("UPDATE permission_proposals SET status='pending' WHERE thread_id=?", (perm,))
    c.commit()
    check("reject succeeds", run(env, "permission", "reject", str(perm)).get("status") == "rejected")

    q2 = q("Another?", "{}")
    c.commit()
    check("close succeeds", run(env, "close", str(q2)).get("closed") is True)
    check("a closed question stops waiting", c.execute("SELECT status FROM threads WHERE id=?", (q2,)).fetchone()[0] == "closed")

    seqs = c.execute("SELECT actor, actor_seq FROM events ORDER BY seq").fetchall()
    per_actor = {}
    ordered = True
    for actor, seq in seqs:
        ordered &= seq == per_actor.get(actor, 0) + 1
        per_actor[actor] = seq
    check("each actor's event counter runs 1, 2, 3...", ordered)
    hlcs = [r[0] for r in c.execute("SELECT hlc FROM events ORDER BY seq")]
    check("event clock only moves forward", all(a < b for a, b in zip(hlcs, hlcs[1:])))
    check("integrity", c.execute("PRAGMA integrity_check").fetchone()[0] == "ok")

    print(f"\n{len(failures)} failed" if failures else "\nall passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
