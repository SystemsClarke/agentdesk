"""What the original's core did on timers and the data around it: the board's schema, jobs, toasts, the merge list, the usage
meter, the backup and the vault."""
import datetime
import json
import os
import re
import subprocess
import time

from lib import ORIGINAL, Board, Missing, feature, wait_for

SCHEMA = json.load(open(os.path.join(ORIGINAL, "schema.json")))
SLEEPER = "import time\ntime.sleep(60)\n"


def now_iso(delta=0):
    return (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=delta)).strftime("%Y-%m-%dT%H:%M:%S+00:00")


def daemon(b, env=None):
    p = b.spawn("daemon", env=env)
    time.sleep(1.0)
    return p


# ---- the board's data model

@feature("data.schema", "data", "A fresh board has every table and column of the original (19 tables incl. identities, goals, slots, usage_samples)", "BoardStore.cs Schema, Governor.cs")
def _schema(b):
    b.need("ui:threads")
    have = {r["name"] for r in b.sql("SELECT name FROM sqlite_master WHERE type='table'")}
    missing = []
    for t, cols in SCHEMA["tables"].items():
        if t not in have:
            missing.append(t)
            continue
        got = {r["name"] for r in b.sql("PRAGMA table_info(%s)" % t)}
        gone = [c for c in cols if c not in got]
        if gone:
            missing.append("%s.%s" % (t, ",".join(gone)))
    if missing:
        raise Missing("absent: " + "; ".join(missing))


@feature("data.open-questions-view", "data", "The open_questions view is 'questions still waiting on John' and is rebuilt on start", "BoardStore.cs OpenQuestionsView")
def _view(b):
    b.tool("ask_human", {"subject": "q", "body": "ok?", "author": "x"})
    assert len(b.sql("SELECT * FROM open_questions")) == 1


@feature("data.user-version", "data", "PRAGMA user_version carries the schema version, so a board at the current version is left alone on open", "BoardStore.cs InitDb")
def _userver(b):
    b.need("ui:threads")
    v = b.sql("PRAGMA user_version")[0]["user_version"]
    if v == 0:
        raise Missing("the board is unversioned")


@feature("data.wal", "data", "The board is SQLite in WAL mode, owned by the data folder (AGENTDESK_DATA), never the repo", "README.md")
def _wal(b):
    b.need("ui:threads")
    assert b.sql("PRAGMA journal_mode")[0]["journal_mode"] == "wal"
    assert os.path.exists(os.path.join(b.data, "agentdesk.db"))


@feature("data.concurrent-writers", "data", "Many processes write the same board at once without losing or corrupting a message", "BoardStore.cs")
def _concurrent(b):
    b.need("ui:threads")
    procs = [b.spawn("ui", "ui:post", json.dumps({"channel": "discussion", "subject": "t%d" % i, "body": "x"})) for i in range(12)]
    for p in procs:
        p.communicate(timeout=60)
    assert b.sql("SELECT COUNT(*) n FROM threads")[0]["n"] == 12
    assert b.sql("PRAGMA integrity_check")[0]["integrity_check"] == "ok"


# ---- recurring jobs

@feature("jobs.schedule", "jobs", "`at HH:mm` on `days` (daily, weekdays, weekends, mon,thu) in local time, or every N minutes: next_run_ts is the first run strictly after now", "Jobs.cs Next")
def _sched(b):
    soon = (datetime.datetime.now() + datetime.timedelta(minutes=30)).strftime("%H:%M")
    j = b.need("ui:job_create", {"name": "a", "folder": b.work, "prompt": "p", "at": soon})
    nxt = datetime.datetime.strptime(j["next_run_ts"], "%Y-%m-%dT%H:%M:%S+00:00").replace(tzinfo=datetime.timezone.utc).astimezone()
    assert nxt.strftime("%H:%M") == soon and 0 < (nxt - datetime.datetime.now().astimezone()).total_seconds() < 3600
    j = b.need("ui:job_create", {"name": "w", "folder": b.work, "prompt": "p", "at": "03:00", "days": "weekdays"})
    d = datetime.datetime.strptime(j["next_run_ts"], "%Y-%m-%dT%H:%M:%S+00:00").replace(tzinfo=datetime.timezone.utc).astimezone()
    assert d.weekday() < 5
    j = b.need("ui:job_create", {"name": "e", "folder": b.work, "prompt": "p", "every_minutes": 90})
    d = datetime.datetime.strptime(j["next_run_ts"], "%Y-%m-%dT%H:%M:%S+00:00").replace(tzinfo=datetime.timezone.utc)
    assert abs((d - datetime.datetime.now(datetime.timezone.utc)).total_seconds() - 5400) < 120
    assert "error" in b.ui("ui:job_create", {"name": "bad", "folder": b.work, "prompt": "p", "at": "03:00", "days": "someday"})[0]


@feature("jobs.fires-when-due", "jobs", "A due job runs (a fresh claude session named after it, --model, scheduled-run suffix) and moves to its next run", "Jobs.cs Tick/Fire")
def _fires(b):
    log = os.path.join(b.work, "run.json")
    b.stub("claude", "import json,os,sys\njson.dump({'argv':sys.argv[1:],'author':os.environ.get('AGENTDESK_AUTHOR'),'identity':os.environ.get('AGENTDESK_IDENTITY')},open(%r,'w'))\n" % log)
    b.need("ui:job_create", {"name": "brief", "folder": b.work, "prompt": "do the brief", "every_minutes": 120, "model": "haiku"})
    b.sql("UPDATE recurring_jobs SET next_run_ts=?", (now_iso(-60),))
    daemon(b)
    run = json.loads(wait_for(lambda: os.path.exists(log) and open(log).read(), 8, what="the job to run"))
    joined = " ".join(run["argv"])
    assert "do the brief" in joined and "scheduled run of the recurring job `brief`" in joined and "haiku" in joined
    assert run["author"] == "brief"
    j = b.sql("SELECT next_run_ts, runs, last_status FROM recurring_jobs")[0]
    assert j["runs"] == 1 and j["next_run_ts"] > now_iso(3600) and j["last_status"].startswith("started")


@feature("jobs.catch-up", "jobs", "A run missed while down fires once if under --catch-up minutes late, else it is skipped and the status says 'missed'", "Jobs.cs Tick")
def _catchup(b):
    out = os.path.join(b.work, "ran.txt")
    b.stub("claude", "open(%r,'a').write('x')\n" % out)
    b.need("ui:job_create", {"name": "late", "folder": b.work, "prompt": "p", "every_minutes": 600, "catch_up_minutes": 60})
    b.need("ui:job_create", {"name": "stale", "folder": b.work, "prompt": "p", "every_minutes": 600, "catch_up_minutes": 60})
    b.sql("UPDATE recurring_jobs SET next_run_ts=? WHERE name='late'", (now_iso(-600),))
    b.sql("UPDATE recurring_jobs SET next_run_ts=? WHERE name='stale'", (now_iso(-7200),))
    daemon(b)
    wait_for(lambda: os.path.exists(out), 8, what="the catch-up run")
    time.sleep(1)
    assert open(out).read() == "x", "only the one within its window ran"
    assert b.sql("SELECT last_status FROM recurring_jobs WHERE name='stale'")[0]["last_status"].startswith("missed")


@feature("jobs.skip-while-running", "jobs", "A run due while the last is still going is skipped, not stacked", "Jobs.cs Fire")
def _skip(b):
    b.stub("claude", SLEEPER)
    b.need("ui:job_create", {"name": "slow", "folder": b.work, "prompt": "p", "every_minutes": 600})
    b.sql("UPDATE recurring_jobs SET next_run_ts=?", (now_iso(-30),))
    daemon(b)
    wait_for(lambda: b.sql("SELECT runs FROM recurring_jobs")[0]["runs"] == 1, 8, what="first run")
    b.sql("UPDATE recurring_jobs SET next_run_ts=?", (now_iso(-30),))
    wait_for(lambda: (b.sql("SELECT last_status FROM recurring_jobs")[0]["last_status"] or "").startswith("skipped"), 40, what="the skip")
    assert b.sql("SELECT runs FROM recurring_jobs")[0]["runs"] == 1


@feature("jobs.disabled-never-fire", "jobs", "A disabled job never runs, however overdue", "Jobs.cs Tick")
def _disabled(b):
    out = os.path.join(b.work, "ran.txt")
    b.stub("claude", "open(%r,'a').write('x')\n" % out)
    b.need("ui:job_create", {"name": "off", "folder": b.work, "prompt": "p", "every_minutes": 600})
    b.need("ui:job_enable", {"name": "off", "on": False})
    b.sql("UPDATE recurring_jobs SET next_run_ts=?", (now_iso(-30),))
    daemon(b)
    time.sleep(3)
    assert not os.path.exists(out)


# ---- toasts, merge list, usage, sweep

@feature("notify.toast-question", "notify", "Each new question toasts once ('#id subject'); only questions toast", "docs/ui-api.md, Tray.cs")
def _toast(b):
    log = os.path.join(b.dir, "toasts.txt")
    daemon(b, env={"AGENTDESK_TOAST_LOG": log})
    b.tool("post_message", {"channel": "discussion", "subject": "chatter", "body": "x", "author": "a"})
    qid = b.tool("ask_human", {"subject": "Deploy it?", "body": "ok?", "author": "builder"})["thread_id"]
    wait_for(lambda: os.path.exists(log), 8, what="a toast")
    time.sleep(2.5)
    text = open(log).read()
    assert "#%d Deploy it?" % qid in text and "chatter" not in text and text.count("Deploy it?") == 1


@feature("notify.toast-follow-up", "notify", "The opener's Done / follow-up after John's reply toasts once as 'Done from <agent>' / 'Follow-up from <agent>'", "docs/ui-api.md")
def _toast_follow(b):
    log = os.path.join(b.dir, "toasts.txt")
    qid = b.tool("ask_human", {"subject": "Deploy it?", "body": "ok?", "author": "builder"})["thread_id"]
    daemon(b, env={"AGENTDESK_TOAST_LOG": log})
    time.sleep(1)
    b.need("ui:reply", {"thread_id": qid, "body": "yes"})
    b.tool("answer_thread", {"thread_id": qid, "body": "Done: deployed.", "author": "builder"})
    wait_for(lambda: os.path.exists(log) and "Done from builder" in open(log).read(), 10, what="the Done toast")


@feature("prs.checker", "prs", "The merge list: gh pr view every minute; merged/closed settles the row and posts the notice on its thread as `agentdesk`", "docs/ui-api.md ui:check_prs")
def _prs(b):
    b.stub("gh", "import json\nprint(json.dumps({'state':'MERGED','title':'T'}))\n")
    tid = b.tool("ask_human", {"subject": "q", "body": "ok?", "author": "builder"})["thread_id"]
    b.tool("request_merge", {"pr_url": "https://github.com/o/r/pull/5", "thread_id": tid, "author": "builder"})
    b.need("ui:check_prs")
    msgs = b.sql("SELECT author, meta FROM messages WHERE thread_id=? ORDER BY id", (tid,))
    assert any(m["author"] == "agentdesk" and "pr-merged" in (m["meta"] or "") for m in msgs), msgs


@feature("prs.notified-once", "prs", "A settled PR's notice is posted once, however often the checker runs", "PrChecker.cs")
def _prs_once(b):
    b.stub("gh", "import json\nprint(json.dumps({'state':'CLOSED','title':'T'}))\n")
    tid = b.tool("ask_human", {"subject": "q", "body": "ok?", "author": "builder"})["thread_id"]
    b.tool("request_merge", {"pr_url": "https://github.com/o/r/pull/5", "thread_id": tid, "author": "builder"})
    for _ in range(3):
        b.need("ui:check_prs")
    assert len([m for m in b.sql("SELECT meta FROM messages WHERE thread_id=?", (tid,)) if "pr-closed" in (m["meta"] or "")]) == 1


@feature("usage.report", "usage", "The plan meter reads claude_usage.json (five_hour/seven_day {used, resets_at}, captured_ts, extra spend) into the original's lines and summary", "Usage.cs Report")
def _usage(b):
    soon = now_iso(2 * 3600 + 30)
    wk = now_iso(3 * 86400 + 1800)
    with open(os.path.join(b.data, "claude_usage.json"), "w") as f:
        json.dump({"five_hour": {"used": 40, "resets_at": soon}, "seven_day": {"used": 80, "resets_at": wk}, "captured_ts": now_iso(), "extra": {"spent": 12.5, "limit": 100, "captured_ts": now_iso()}}, f)
    u = b.need("ui:status")["usage"]
    assert "time left: 2h 00m in your 5-hour window, 40% used" in u["lines"], u
    assert any(l.startswith("time left:") and "on the week, 80% used, getting close" in l for l in u["lines"]), u
    assert "monthly spend: $12.50 of $100 (13%)" in u["lines"], u
    assert u["summary"].startswith("5h 40%, resets in 2h 00m · week 80%, resets in 3d 0h"), u["summary"]
    assert u["summary"].endswith("reported 0m ago")


@feature("usage.no-feed", "usage", "Without a feed the summary says so (and how to add one)", "Usage.cs Report")
def _usage_none(b):
    u = b.need("ui:status")["usage"]
    assert u["lines"] == [] and u["summary"].startswith("no feed yet")


@feature("usage.feed-script", "usage", "The status-line feeder normalises Claude Code's JSON into claude_usage.json, keeping the last-known spend, and prints the prompt", "scripts/claude_usage_feed.py")
def _feed(b):
    r = b.run("usage-feed", input=json.dumps({"model": {"display_name": "Opus"}, "rate_limits": {"five_hour": {"used_percentage": 23.5, "resets_at": 1893456000}, "seven_day": {"used_percentage": 41, "resets_at": 1893999999}}}))
    assert r.returncode == 0 and r.stdout.strip()
    d = json.load(open(os.path.join(b.data, "claude_usage.json")))
    assert d["five_hour"]["used"] in (23, 24) and "resets_at" in d["seven_day"] and "captured_ts" in d


@feature("usage.probe", "usage", "Every 5 minutes (when wanted) `claude -p /usage --setting-sources project --no-session-persistence` is folded into the feed", "Usage.cs KeepFresh/Apply")
def _probe(b):
    b.stub("claude", "print('Current session: 33% used · resets Sep 26, 3:59am (America/New_York)')\nprint('Current week (all models): 61% used · resets Oct 1, 4am')\n")
    r = b.run("usage-probe")
    if r.returncode == 2 and "unrecognized" in r.stderr:
        raise Missing("no `usage-probe` command: the 5-minute `claude -p /usage` refresh is not implemented")
    d = json.load(open(os.path.join(b.data, "claude_usage.json")))
    assert d["five_hour"]["used"] == 33 and d["seven_day"]["used"] == 61 and d["source"] == "claude -p /usage"


@feature("sweep.files-settled-questions", "vault", "An answered/closed question is filed to the vault and archived 24 h after its last reply (not before; archive_hold keeps it)", "README, Terminal.cs GraceHours")
def _sweep(b):
    qid = b.tool("ask_human", {"subject": "old", "body": "ok?", "author": "builder"})["thread_id"]
    b.need("ui:reply", {"thread_id": qid, "body": "yes"})
    daemon(b)
    time.sleep(13)
    assert b.sql("SELECT status FROM threads WHERE id=?", (qid,))[0]["status"] == "answered", "not filed within minutes: it stays 24 h"
    b.sql("UPDATE threads SET updated_ts=? WHERE id=?", (now_iso(-25 * 3600), qid))
    wait_for(lambda: b.sql("SELECT status FROM threads WHERE id=?", (qid,))[0]["status"] == "archived", 90, what="the sweep")


@feature("vault.mirror-wiki", "vault", "A new wiki post is mirrored into the vault and the result reports status/note/reasons", "AgentBoard.Mirror")
def _mirror(b):
    r = b.tool("post_message", {"channel": "wiki", "subject": "Trap: flag X is off", "body": "Because.", "author": "r"})
    assert r["vault"]["status"] == "mirrored" and r["vault"]["thread_id"] == r["thread_id"] and r["vault"]["reasons"] == []


@feature("vault.search", "vault", "search_vault matches a note's exact terms (identifiers, UUIDs, hostnames) and inlines `full` bodies; returns {hits:[{path,score,type,summary}]}", "docs/ui-api.md / vault_search.py")
def _vsearch(b):
    b.tool("post_message", {"channel": "wiki", "subject": "Host layout", "body": "build-01.example.local runs the agents", "author": "r"})
    r = b.tool("search_vault", {"query": "build-01.example.local", "full": 1})
    assert r["hits"] and {"path", "score", "type", "summary"} <= set(r["hits"][0]) and "build-01" in r["hits"][0]["body"]


@feature("vault.embedding-search", "vault", "search_vault also ranks by meaning (embedding search over Ollama), reporting 'Ollama is not reachable' when it is down", "vault_search.py")
def _vembed(b):
    r = b.tool("search_vault", {"query": "how do we deploy the thing"})
    if "hits" in r and not r["hits"]:
        raise Missing("keyword matching only: no semantic ranking")
    raise Missing("not implemented")


@feature("backup.snapshot", "backup", "`agentdesk backup` takes a consistent snapshot named agentdesk-YYYYmmddTHHMMSS.db, integrity-checked", "backup.py snapshot/verify_snapshot")
def _snapshot(b):
    b.tool("post_message", {"channel": "discussion", "subject": "s", "body": "x", "author": "a"})
    r = b.run("backup")
    assert r.returncode == 0, r.stderr
    names = [f for f in os.listdir(os.path.join(b.data, "backups"))]
    assert any(re.fullmatch(r"agentdesk-\d{8}T\d{6}\.db", n) for n in names), names


@feature("backup.prune", "backup", "The archive keeps every snapshot for 2 days, then one a day up to 14 days", "backup.py plan_prune")
def _prune(b):
    d = os.path.join(b.data, "backups")
    os.makedirs(d)
    now = datetime.datetime.now(datetime.timezone.utc)
    keep, drop = [], []
    for h in (1, 5, 30):
        keep.append(h)
    for hrs in (1, 5, 30, 50, 52, 80, 82, 14 * 24 + 5):
        n = "agentdesk-%s.db" % (now - datetime.timedelta(hours=hrs)).strftime("%Y%m%dT%H%M%S")
        open(os.path.join(d, n), "w").write("x")
    b.tool("post_message", {"channel": "discussion", "subject": "s", "body": "x", "author": "a"})
    b.run("backup")
    left = sorted(os.listdir(d))
    assert not any(str(14 * 24 + 5) in n for n in left)
    old = [n for n in left if n[10:18] == (now - datetime.timedelta(hours=50)).strftime("%Y%m%d")]
    assert len(old) <= 1, "one a day past 2 days: %s" % left


@feature("backup.vault-transcript", "backup", "The day's messages are written to the vault as markdown; a linked note per thread (agentdesk/threads/thread-N.md) and maps/AgentDesk MOC.md", "backup.py write_vault, vault_threads.py")
def _transcript(b):
    tid = b.tool("post_message", {"channel": "discussion", "subject": "Findings", "body": "found it", "author": "a"})["thread_id"]
    b.run("backup")
    v = b.env["AGENTDESK_VAULT"]
    p = os.path.join(v, "agentdesk", "threads", "thread-%d.md" % tid)
    assert os.path.exists(p) and "found it" in open(p).read()
    assert os.path.exists(os.path.join(v, "maps", "AgentDesk MOC.md"))


@feature("backup.vault-git-push", "backup", "The vault's own paths are committed (as claude[bot]) and pushed to its remote; a failed push is reported in vault_state.json and retried", "backup.py _write_vault_state, README")
def _gitpush(b):
    remote = os.path.join(b.dir, "remote.git")
    subprocess.run(["git", "init", "--bare", "-q", remote], check=True)
    v = b.env["AGENTDESK_VAULT"]
    os.makedirs(v)
    for c in (["init", "-q"], ["remote", "add", "origin", remote], ["checkout", "-q", "-b", "main"]):
        subprocess.run(["git", "-C", v] + c, check=True)
    b.tool("post_message", {"channel": "discussion", "subject": "S", "body": "x", "author": "a"})
    # the commit identity is the machine's own git config, or what AGENTDESK_GIT_AUTHOR_NAME/_EMAIL say (the original hard-coded claude[bot])
    ident = {"AGENTDESK_GIT_AUTHOR_NAME": "claude[bot]", "AGENTDESK_GIT_AUTHOR_EMAIL": "209825114+claude[bot]@users.noreply.github.com",
             "GIT_COMMITTER_NAME": "claude[bot]", "GIT_COMMITTER_EMAIL": "209825114+claude[bot]@users.noreply.github.com"}
    b.run("backup", env=ident)
    st = json.load(open(os.path.join(b.data, "vault_state.json"))) if os.path.exists(os.path.join(b.data, "vault_state.json")) else None
    if not st:
        raise Missing("no vault_state.json: the vault is not committed and pushed")
    assert st["pushed"] is True
    log = subprocess.run(["git", "-C", remote, "log", "--format=%an", "main"], capture_output=True, text=True).stdout
    assert "claude[bot]" in log


@feature("backup.restore", "backup", "`agentdesk restore <archive> --yes [--force-newer]` restores a snapshot into the live board after verifying it", "backup.py restore_cli")
def _restore(b):
    b.tool("post_message", {"channel": "discussion", "subject": "keep", "body": "x", "author": "a"})
    b.run("backup")
    snap = os.path.join(b.data, "backups", os.listdir(os.path.join(b.data, "backups"))[0])
    b.tool("post_message", {"channel": "discussion", "subject": "later", "body": "x", "author": "a"})
    r = b.run("restore", snap, "--yes", "--force-newer")
    if r.returncode == 2 and "unrecognized" in r.stderr:
        raise Missing("no restore command")
    subjects = [t["subject"] for t in b.sql("SELECT subject FROM threads")]
    assert "keep" in subjects and "later" not in subjects, subjects
    assert any(x.startswith("Database restored from ") for x in subjects), "a restore says so on the board"
    assert len(os.listdir(os.path.join(b.data, "backups"))) >= 2, "the state it overwrote was snapshotted first"


@feature("log.core-log", "ops", "Activity is logged to core.log in the data folder (acks delivered, jobs started, permission decisions, failures)", "README")
def _corelog(b):
    qid = b.tool("ask_human", {"subject": "q", "body": "ok?", "author": "builder"}, session="s1")["thread_id"]
    b.need("ui:reply", {"thread_id": qid, "body": "yes"})
    b.tool("post_message", {"channel": "discussion", "subject": "s", "body": "x", "author": "builder"}, session="s1")
    log = open(os.path.join(b.data, "core.log")).read()
    assert "acknowledged" in log
