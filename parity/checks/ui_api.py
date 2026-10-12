"""Every request of the original's window-to-core protocol (docs/ui-api.md), called through `agentdesk ui <request>`.

A request passes when it exists and returns the documented document. A request the build does not implement answers
`not implemented` (exit 2) and the feature reports as missing."""
import json
import os
import time

from lib import Board, Missing, feature, make_stub

SRC = "docs/ui-api.md"
SLEEPER = "import time\ntime.sleep(60)\n"


def keys(doc, *ks):
    gone = [k for k in ks if k not in doc]
    if gone:
        raise Missing("the document lacks %s" % gone)


def question(b, body="Go?"):
    return b.tool("ask_human", {"subject": "Q", "body": body, "author": "builder"})["thread_id"]


@feature("ui.subscribe", "ui-api", "ui:subscribe marks the connection a subscriber", SRC)
def _subscribe(b):
    assert b.need("ui:subscribe") == {"ok": True}


@feature("ui.thread", "ui-api", "ui:thread reads a thread with no side effects (no read receipt, no session record)", SRC)
def _thread(b):
    tid = b.tool("post_message", {"channel": "discussion", "subject": "s", "body": "b", "author": "w"})["thread_id"]
    before = (b.sql("SELECT COUNT(*) n FROM messages")[0]["n"], b.sql("SELECT COUNT(*) n FROM receipts")[0]["n"])
    d = b.need("ui:thread", {"thread_id": tid})
    keys(d, "thread", "messages")
    assert d["thread"]["id"] == tid and len(d["messages"]) == 1
    assert (b.sql("SELECT COUNT(*) n FROM messages")[0]["n"], b.sql("SELECT COUNT(*) n FROM receipts")[0]["n"]) == before
    assert "error" in b.ui("ui:thread", {"thread_id": 999})[0]


@feature("ui.reply", "ui-api", "ui:reply posts as john, answers an open question, queues the opener's ack; blank body is an error", SRC)
def _reply(b):
    tid = question(b)
    assert "error" in b.ui("ui:reply", {"thread_id": tid, "body": "  "})[0]
    d = b.need("ui:reply", {"thread_id": tid, "body": "yes"})
    keys(d, "ok", "message_id")
    assert b.sql("SELECT status FROM threads WHERE id=?", (tid,))[0]["status"] == "answered"
    m = b.sql("SELECT author, author_kind FROM messages WHERE id=?", (d["message_id"],))[0]
    assert (m["author"], m["author_kind"]) == ("john", "human")
    assert b.sql("SELECT state, agent FROM acks")[0] == {"state": "pending", "agent": "builder"}


@feature("ui.reply.autowake", "ui-api", "5 s after a reply, an identity opener that was not handed it is woken (settings.json auto_wake)", SRC)
def _autowake(b):
    b.stub("claude", SLEEPER)
    _daemon(b)  # the wake is the core's work: here that is the daemon (or the window)
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    b.need("ui:identity_start", {"name": "builder"})
    tid = b.tool("ask_human", {"subject": "Q", "body": "Go?", "author": "builder"}, session="sx")["thread_id"]
    b.need("ui:reply", {"thread_id": tid, "body": "yes"})
    time.sleep(7)
    rows = b.sql("SELECT method, state FROM deliveries")
    assert any(r["method"] == "wake" for r in rows), rows


@feature("ui.close", "ui-api", "ui:close closes a question without answering and clears archive_hold; not a question: nothing changes", SRC)
def _close(b):
    tid = question(b)
    assert b.need("ui:close", {"thread_id": tid}) == {"ok": True, "closed": True}
    assert b.sql("SELECT status FROM threads WHERE id=?", (tid,))[0]["status"] == "closed"
    d = b.tool("post_message", {"channel": "discussion", "subject": "s", "body": "b", "author": "w"})["thread_id"]
    assert b.need("ui:close", {"thread_id": d})["closed"] is False


@feature("ui.post", "ui-api", "ui:post starts a thread as john; blank subject becomes (no subject); blank body / unknown channel is an error", SRC)
def _post(b):
    d = b.need("ui:post", {"channel": "discussion", "subject": "", "body": "hello"})
    keys(d, "ok", "thread_id")
    t = b.sql("SELECT subject, opened_by FROM threads WHERE id=?", (d["thread_id"],))[0]
    assert t == {"subject": "(no subject)", "opened_by": "john"}
    assert "error" in b.ui("ui:post", {"channel": "discussion", "subject": "s", "body": ""})[0]
    assert "error" in b.ui("ui:post", {"channel": "chat", "subject": "s", "body": "x"})[0]


@feature("ui.unarchive", "ui-api", "ui:unarchive restores the settled status and sets archive_hold; not archived: nothing changes", SRC)
def _unarchive(b):
    tid = question(b)
    b.need("ui:reply", {"thread_id": tid, "body": "ok"})
    b.sql("UPDATE threads SET status='archived', meta=? WHERE id=?", (json.dumps({"archived_from": "answered"}), tid))
    assert b.need("ui:unarchive", {"thread_id": tid}) == {"ok": True, "unarchived": True}
    t = b.sql("SELECT status, meta FROM threads WHERE id=?", (tid,))[0]
    assert t["status"] == "answered" and "archive_hold" in t["meta"]
    assert b.need("ui:unarchive", {"thread_id": tid})["unarchived"] is False


@feature("ui.threads", "ui-api", "ui:threads is list_threads without last_body", SRC)
def _threads(b):
    for i in range(3):
        b.need("ui:post", {"channel": "wiki", "subject": "w%d" % i, "body": "x"})
    d = b.need("ui:threads", {"channel": "wiki", "limit": 2})
    assert len(d["threads"]) == 2 and "last_body" not in d["threads"][0]
    keys(d["threads"][0], "waiting", "message_count", "last_author", "delivery")


@feature("ui.status", "ui-api", "ui:status: slack, concierge, sessions, usage, governor, prs, bridge, goals", SRC)
def _status(b):
    d = b.need("ui:status")
    keys(d, "slack", "concierge", "sessions", "usage", "governor", "prs", "bridge", "goals")
    keys(d["usage"], "lines", "summary")
    keys(d["sessions"], "running", "max", "ceiling", "why", "summary")


@feature("ui.check_prs", "ui-api", "ui:check_prs runs a merge-list check now; only GitHub saying merged/closed takes a row off", SRC)
def _checkprs(b):
    b.stub("gh", "import json,sys\nprint(json.dumps({'state':'MERGED','title':'Fix it'}))\n")
    b.tool("request_merge", {"pr_url": "https://github.com/o/r/pull/5", "author": "builder"})
    assert b.need("ui:check_prs") == {"ok": True}
    r = b.sql("SELECT state, title FROM pull_requests")[0]
    assert r["state"] == "merged" and r["title"] == "Fix it"
    b.stub("gh", "import sys\nsys.stderr.write('boom')\nsys.exit(1)\n")
    b.tool("request_merge", {"pr_url": "https://github.com/o/r/pull/6", "author": "builder"})
    b.need("ui:check_prs")
    r = [x for x in b.sql("SELECT * FROM pull_requests") if x["number"] == 6][0]
    assert r["state"] == "open" and r["last_error"], "a failed check leaves the row open with last_error"


@feature("ui.governor", "ui-api", "ui:governor: the usage forecast, caps and model tiers, from the last 5 weeks of samples", SRC)
def _governor(b):
    d = b.need("ui:governor")
    keys(d, "enforcing", "advisory", "fresh", "samples", "caps", "models", "series", "summary", "would_queue", "would_shed", "held", "pool")


@feature("ui.governor_enforce", "ui-api", "ui:governor_enforce {on} (John only) sets settings.json governor_enforce, keeping other keys", SRC)
def _enforce(b):
    with open(os.path.join(b.data, "settings.json"), "w") as f:
        json.dump({"keep": 1}, f)
    d = b.need("ui:governor_enforce", {"on": True})
    assert d["enforcing"] is True
    s = json.load(open(os.path.join(b.data, "settings.json")))
    assert s["governor_enforce"] is True and s["keep"] == 1
    doc, rc = b.ui("ui:governor_enforce", {"on": False}, env={"AGENTDESK_IDENTITY": "agent1"})
    assert "error" in doc, "an agent's session gets an error"


@feature("ui.concierge", "ui-api", "ui:concierge: state with no args; on:true creates the standing goal `concierge`; on:false stops it", SRC)
def _concierge(b):
    d = b.need("ui:concierge")
    keys(d, "on", "state", "lead", "lead_state", "thread_id", "open", "members", "held")
    assert d["on"] is False and d["state"] == "off"
    b.stub("claude", SLEEPER)
    d = b.need("ui:concierge", {"on": True})
    assert d["on"] is True and d["lead"] == "concierge-lead"
    g = b.sql("SELECT * FROM goals WHERE name='concierge'")[0]
    assert g["standing"] == 1 and g["measure_cmd"] == "internal:open_work" and g["success"] == "value <= 0" and g["max_members"] == 3
    assert b.need("ui:concierge", {"on": False})["on"] is False


@feature("ui.wake", "ui-api", "ui:wake brings John's latest reply to the agent that asked; records a deliveries row, posts nothing", SRC)
def _wake(b):
    b.stub("claude", SLEEPER)
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    tid = b.tool("ask_human", {"subject": "Q", "body": "Go?", "author": "builder"}, session="sx")["thread_id"]
    b.need("ui:reply", {"thread_id": tid, "body": "yes"})
    msgs = b.sql("SELECT COUNT(*) n FROM messages")[0]["n"]
    d = b.need("ui:wake", {"thread_id": tid})
    assert d["ok"] is True and d["said"].startswith("woke builder")
    assert b.sql("SELECT COUNT(*) n FROM messages")[0]["n"] == msgs
    assert any(r["method"] == "wake" for r in b.sql("SELECT * FROM deliveries"))


@feature("ui.dictate", "ui-api", "ui:dictate start/poll/stop/arm/disarm: local speech-to-text; failures come back as state done with error", SRC)
def _dictate(b):
    d = b.need("ui:dictate", {"action": "start"})
    keys(d, "state", "text")
    assert d["state"] in ("loading", "downloading", "listening", "done")


@feature("ui.folders", "ui-api", "ui:folders ranks the folders John works in (identities, goals, sessions, Claude conversations; halves every 21 days)", SRC)
def _folders(b):
    proj = os.path.join(b.home, ".claude", "projects", "p1")
    os.makedirs(proj)
    work = os.path.join(b.dir, "repo")
    os.makedirs(work)
    with open(os.path.join(proj, "c1.jsonl"), "w") as f:
        f.write(json.dumps({"type": "user", "cwd": work, "message": {"role": "user", "content": "hi"}}) + "\n")
    d = b.need("ui:folders")
    assert any(x["path"] == work for x in d["folders"]), d
    keys(d["folders"][0], "path", "who", "uses", "last")


def _daemon(b):
    p = b.spawn("daemon")
    time.sleep(1.0)
    return p


@feature("ui.session_start", "ui-api", "ui:session_start runs a headless session on a pseudoconsole; CLAUDE*/AGENTDESK_* are dropped from its environment", SRC)
def _sstart(b):
    _daemon(b)
    script = make_stub(b.stubs, "agent", "import os,time\nopen(os.path.join(os.getcwd(),'env.txt'),'w').write(str(sorted(k for k in os.environ if k.startswith(('CLAUDE','AGENTDESK')))))\ntime.sleep(30)\n")
    d = b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script}, env={"CLAUDE_CODE_SESSION_ID": "leak"})
    keys(d, "name", "pid")
    time.sleep(1.5)
    env_seen = open(os.path.join(b.work, "env.txt")).read()
    assert "CLAUDE_CODE_SESSION_ID" not in env_seen and "AGENTDESK_HEADLESS" in env_seen, env_seen


@feature("ui.session_list", "ui-api", "ui:session_list lists the running sessions (name, folder, command, pid, started, viewers)", SRC)
def _slist(b):
    _daemon(b)
    script = make_stub(b.stubs, "agent", SLEEPER)
    b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script})
    d = b.need("ui:session_list")
    keys(d["sessions"][0], "name", "folder", "command", "pid", "started", "viewers")


@feature("ui.session_stop", "ui-api", "ui:session_stop kills the session's process tree and waits for it to exit", SRC)
def _sstop(b):
    _daemon(b)
    script = make_stub(b.stubs, "agent", SLEEPER)
    b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script})
    assert b.need("ui:session_stop", {"name": "s1"}) == {"stopped": "s1"}
    assert b.need("ui:session_list")["sessions"] == []


@feature("ui.attach", "ui-api", "ui:attach makes the connection a viewer: replays the last 256 KB, then streams; several viewers; slow ones dropped", SRC)
def _attach(b):
    _daemon(b)
    script = make_stub(b.stubs, "agent", "print('hello from the session', flush=True)\nimport time\ntime.sleep(30)\n")
    b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script})
    time.sleep(1)
    d = b.need("ui:attach", {"name": "s1", "cols": 100, "rows": 30})
    keys(d, "attached", "replayed")


@feature("ui.input", "ui-api", "ui:input types text into the session", SRC)
def _input(b):
    _daemon(b)
    script = make_stub(b.stubs, "agent", "import sys\nopen('got.txt','w').write(sys.stdin.readline())\n")
    b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script})
    assert b.need("ui:input", {"name": "s1", "data": "ping\r"}) == {"ok": True}
    time.sleep(1.5)
    assert "ping" in open(os.path.join(b.work, "got.txt")).read()


@feature("ui.resize", "ui-api", "ui:resize resizes the session's pseudoconsole", SRC)
def _resize(b):
    _daemon(b)
    script = make_stub(b.stubs, "agent", SLEEPER)
    b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script})
    assert b.need("ui:resize", {"name": "s1", "cols": 90, "rows": 25}) == {"ok": True}


ROW = ("name", "folder", "charter", "host", "claude_session_id", "pid", "state", "autostart", "model", "created_ts", "updated_ts", "generation", "phoenix_msg")


@feature("ui.identity_create", "ui-api", "ui:identity_create records an identity (host, model tier, charter); starts nothing", SRC)
def _icreate(b):
    d = b.need("ui:identity_create", {"name": "builder", "folder": b.work, "charter": "builds", "model": "haiku"})
    keys(d, *ROW)
    assert d["state"] == "stopped" and d["model"] == "haiku" and d["generation"] == 1


@feature("ui.identity_list", "ui-api", "ui:identity_list: every identity with state running/queued/stopped and its Phoenix generation", SRC)
def _ilist(b):
    b.need("ui:identity_create", {"name": "a", "folder": b.work})
    d = b.need("ui:identity_list")
    keys(d["identities"][0], *ROW)


@feature("ui.identity_start", "ui-api", "ui:identity_start runs `claude --resume/--session-id ... --append-system-prompt`, queued past the session ceiling", SRC)
def _istart(b):
    log = os.path.join(b.work, "argv.txt")
    b.stub("claude", "import os,sys,time\nopen(%r,'w').write(repr(sys.argv[1:])+os.environ.get('AGENTDESK_IDENTITY',''))\ntime.sleep(60)\n" % log)
    _daemon(b)
    b.need("ui:identity_create", {"name": "builder", "folder": b.work, "charter": "be careful"})
    d = b.need("ui:identity_start", {"name": "builder"})
    assert d["state"] in ("running", "queued")
    time.sleep(2)
    argv = open(log).read()
    assert "--session-id" in argv and "--append-system-prompt" in argv and "be careful" in argv and "pass_the_torch" in argv and argv.endswith("builder"), argv


@feature("ui.identity_stop", "ui-api", "ui:identity_stop stops it (a queued one may take the slot); a plain session name stops that session", SRC)
def _istop(b):
    b.stub("claude", SLEEPER)
    _daemon(b)
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    b.need("ui:identity_start", {"name": "builder"})
    assert b.need("ui:identity_stop", {"name": "builder"})["state"] == "stopped"


@feature("ui.identity_forget", "ui-api", "ui:identity_forget stops it and deletes the row; the conversation is left alone", SRC)
def _iforget(b):
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    assert b.need("ui:identity_forget", {"name": "builder"}) == {"forgotten": "builder"}
    assert b.need("ui:identity_list")["identities"] == []


@feature("ui.adoptable", "ui-api", "ui:adoptable lists Claude conversations from the last 24 h that someone typed in, newest first", SRC)
def _adoptable(b):
    proj = os.path.join(b.home, ".claude", "projects", "p1")
    os.makedirs(proj)
    with open(os.path.join(proj, "abc-123.jsonl"), "w") as f:
        f.write(json.dumps({"type": "user", "cwd": b.work, "message": {"role": "user", "content": "<system-reminder>x</system-reminder>build the thing"}}) + "\n")
    d = b.need("ui:adoptable", env={"AGENTDESK_CLAUDE_PROJECTS": os.path.join(b.home, ".claude", "projects")})
    s = d["sessions"][0]
    keys(s, "session_id", "folder", "last_activity", "first_message")
    assert s["session_id"] == "abc-123" and s["first_message"] == "build the thing"


@feature("ui.adopt", "ui-api", "ui:adopt turns a conversation into a started identity in its own folder, with a note to close it elsewhere", SRC)
def _adopt(b):
    b.stub("claude", SLEEPER)
    proj = os.path.join(b.home, ".claude", "projects", "p1")
    os.makedirs(proj)
    with open(os.path.join(proj, "abc-123.jsonl"), "w") as f:
        f.write(json.dumps({"type": "user", "cwd": b.work, "message": {"role": "user", "content": "hi"}}) + "\n")
    d = b.need("ui:adopt", {"session_id": "abc-123", "name": "adopted"}, env={"AGENTDESK_CLAUDE_PROJECTS": os.path.join(b.home, ".claude", "projects")})
    keys(d, "name", "note")
    assert d["claude_session_id"] == "abc-123"


@feature("ui.log_tail", "ui-api", "ui:log_tail returns the last lines of core.log (at most 1000)", SRC)
def _logtail(b):
    with open(os.path.join(b.data, "core.log"), "w") as f:
        f.write("\n".join("line %d" % i for i in range(1500)) + "\n")
    d = b.need("ui:log_tail", {"lines": 5000})
    assert len(d["lines"]) == 1000 and d["lines"][-1] == "line 1499"


@feature("ui.web_url", "ui-api", "ui:web_url is the ops console's URL with its key", SRC)
def _weburl(b):
    d = b.need("ui:web_url")
    if d.get("url") is None:
        raise Missing("no ops console is running")
    assert d["url"].startswith("http://127.0.0.1:") and "?k=" in d["url"]


@feature("ui.update", "ui-api", "ui:update checks GitHub Releases: installed, current, latest, pending, downloaded, restarting", SRC)
def _update(b):
    d = b.need("ui:update")
    keys(d, "installed", "current", "latest", "pending", "downloaded", "restarting")


GOAL = ("name", "state", "objective", "lead", "thread_id", "success", "experiments", "last_value", "members", "max_members", "standing")


def _goal(b, name="g1"):
    b.stub("claude", SLEEPER)
    _daemon(b)
    return b.need("ui:goal_create", {"name": name, "objective": "make it faster", "folder": b.work})


@feature("ui.goal_create", "ui-api", "ui:goal_create: a draft goal, its board thread, and a started lead <name>-lead (opus)", SRC)
def _gcreate(b):
    d = _goal(b)
    assert d["state"] == "draft"
    assert b.sql("SELECT model FROM identities WHERE name='g1-lead'")[0]["model"] == "opus"
    assert b.sql("SELECT subject FROM threads WHERE id=?", (d["thread_id"],))[0]["subject"] == "goal: g1"


@feature("ui.goal_propose", "ui-api", "ui:goal_propose: the lead (or John) sets hypothesis, measure_cmd and a success line on a draft", SRC)
def _gpropose(b):
    _goal(b)
    d = b.need("ui:goal_propose", {"name": "g1", "hypothesis": "caching helps", "measure_cmd": "echo 5", "success": "value < 3"})
    assert d["success"] == "value < 3" and d["measure_cmd"] == "echo 5"
    doc, _ = b.ui("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "echo 1", "success": "whenever"})
    assert "error" in doc


@feature("ui.goal_approve", "ui-api", "ui:goal_approve (John only) makes a proposed draft running with default budget 3 members / 24 h / 30 min", SRC)
def _gapprove(b):
    _goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "echo 5", "success": "value < 3"})
    d = b.need("ui:goal_approve", {"name": "g1"})
    assert d["state"] == "running" and d["max_members"] == 3 and d["max_hours"] == 24 and d["cadence_minutes"] == 30


@feature("ui.goal_budget", "ui-api", "ui:goal_budget (John only) changes members/hours/cadence and posts it on the goal thread", SRC)
def _gbudget(b):
    _goal(b)
    d = b.need("ui:goal_budget", {"name": "g1", "max_members": 5})
    assert d["max_members"] == 5


@feature("ui.goal_stop", "ui-api", "ui:goal_stop: state stopped, members forgotten, lead stopped, posted on the thread", SRC)
def _gstop(b):
    _goal(b)
    assert b.need("ui:goal_stop", {"name": "g1"})["state"] == "stopped"


@feature("ui.goal_list", "ui-api", "ui:goal_list: every goal, newest activity first", SRC)
def _glist(b):
    _goal(b)
    keys(b.need("ui:goal_list")["goals"][0], *GOAL)


@feature("ui.goal_status", "ui-api", "ui:goal_status: the goal row plus experiments, history, members and the summary agents are woken with", SRC)
def _gstatus(b):
    _goal(b)
    keys(b.need("ui:goal_status", {"name": "g1"}), "name", "experiments", "history", "members", "summary")


@feature("ui.slot_list", "ui-api", "ui:slot_list: the 10 swarm slots, empty ones included, joined with their goal", SRC)
def _slotlist(b):
    d = b.need("ui:slot_list")
    assert len(d["slots"]) == 10
    keys(d["slots"][0], "n", "channel_id", "channel_name", "persona_name", "persona_icon", "goal", "updated_ts")


@feature("ui.slot_assign", "ui-api", "ui:slot_assign records what the bridge did for slot n; the goal must exist and be in no other slot", SRC)
def _slotassign(b):
    _goal(b)
    d = b.need("ui:slot_assign", {"n": 1, "goal": "g1", "persona": "Ada", "channel_id": "C1", "channel_name": "swarm-g1"})
    assert d["n"] == 1 and d["goal"] == "g1"
    doc, _ = b.ui("ui:slot_assign", {"n": 2, "goal": "g1"})
    assert "error" in doc


@feature("ui.slot_clear", "ui-api", "ui:slot_clear frees the slot: no goal, no persona; its channel stays", SRC)
def _slotclear(b):
    d = b.need("ui:slot_clear", {"n": 1})
    assert d["goal"] is None


PERM = {"rule": "Bash(cargo test:*)", "reason": "tests", "author": "builder", "scope": "project"}


def _propose(b):
    return b.tool("propose_permission_rule", dict(PERM, project_dir=b.work))


@feature("ui.permission_list", "ui-api", "ui:permission_list: proposals newest first (with broad), and what the user's settings.json allows", SRC)
def _plist(b):
    _propose(b)
    d = b.need("ui:permission_list")
    keys(d, "proposals", "allow", "settings")
    keys(d["proposals"][0], "id", "rule", "status", "broad")


@feature("ui.permission_decide", "ui-api", "ui:permission_decide (John only): approve backs the file up and adds the rule once; reject writes nothing; bad JSON refused", SRC)
def _pdecide(b):
    pid = _propose(b)["id"]
    path = os.path.join(b.work, ".claude", "settings.json")
    os.makedirs(os.path.dirname(path))
    with open(path, "w") as f:
        json.dump({"model": "x", "permissions": {"allow": ["Read(*)"]}}, f)
    doc, _ = b.ui("ui:permission_decide", {"id": pid, "approve": True}, env={"AGENTDESK_IDENTITY": "agent1"})
    assert "error" in doc, "an agent cannot approve"
    d = b.need("ui:permission_decide", {"id": pid, "approve": True})
    assert d["status"] == "approved"
    s = json.load(open(path))
    assert s["model"] == "x" and s["permissions"]["allow"] == ["Read(*)", "Bash(cargo test:*)"]
    assert os.listdir(os.path.join(b.data, "permission_backups"))
    t = b.sql("SELECT status FROM threads WHERE id=?", (d["thread_id"],))[0]["status"]
    assert t == "answered", "approving settles the question"


@feature("ui.permission_revert", "ui-api", "ui:permission_revert (John only) takes an approved rule back out of settings.json", SRC)
def _prevert(b):
    pid = _propose(b)["id"]
    b.need("ui:permission_decide", {"id": pid, "approve": True})
    d = b.need("ui:permission_revert", {"id": pid})
    assert d["status"] == "reverted"
    assert "cargo test" not in open(os.path.join(b.work, ".claude", "settings.json")).read()


@feature("ui.job_create", "ui-api", "ui:job_create validates name/folder/prompt/at|every/model and schedules next_run_ts", SRC)
def _jcreate(b):
    d = b.need("ui:job_create", {"name": "brief", "folder": b.work, "prompt": "do it", "at": "08:00", "days": "weekdays"})
    assert d["model"] == "sonnet" and d["next_run_ts"] and d["enabled"] == 1
    for bad in ({"name": "x y", "folder": b.work, "prompt": "p", "at": "08:00"}, {"name": "a", "folder": b.work, "prompt": "p"}, {"name": "b", "folder": b.work, "prompt": "p", "at": "8am"}, {"name": "c", "folder": "/nope/nothing", "prompt": "p", "at": "08:00"}):
        assert "error" in b.ui("ui:job_create", bad)[0], bad


@feature("ui.job_list", "ui-api", "ui:job_list", SRC)
def _jlist(b):
    b.need("ui:job_create", {"name": "brief", "folder": b.work, "prompt": "p", "every_minutes": 120})
    keys(b.need("ui:job_list")["jobs"][0], "name", "folder", "prompt", "model", "at", "days", "every_minutes", "catch_up_minutes", "enabled", "next_run_ts", "last_run_ts", "last_status", "runs")


@feature("ui.job_enable", "ui-api", "ui:job_enable on/off; turning on schedules from now", SRC)
def _jenable(b):
    b.need("ui:job_create", {"name": "brief", "folder": b.work, "prompt": "p", "every_minutes": 120})
    assert b.need("ui:job_enable", {"name": "brief", "on": False})["enabled"] == 0
    assert b.need("ui:job_enable", {"name": "brief", "on": True})["enabled"] == 1


@feature("ui.job_delete", "ui-api", "ui:job_delete", SRC)
def _jdelete(b):
    b.need("ui:job_create", {"name": "brief", "folder": b.work, "prompt": "p", "every_minutes": 120})
    assert b.need("ui:job_delete", {"name": "brief"}) == {"deleted": "brief"}


@feature("ui.job_run", "ui-api", "ui:job_run starts the job now, off schedule; the schedule is unchanged", SRC)
def _jrun(b):
    out = os.path.join(b.work, "ran.txt")
    b.stub("claude", "import sys\nopen(%r,'w').write(repr(sys.argv[1:]))\n" % out)
    b.need("ui:job_create", {"name": "brief", "folder": b.work, "prompt": "do the chore", "every_minutes": 120})
    before = b.sql("SELECT next_run_ts FROM recurring_jobs")[0]["next_run_ts"]
    d = b.need("ui:job_run", {"name": "brief"})
    assert d["last_status"].startswith("started")
    time.sleep(2)
    assert "do the chore" in open(out).read() and "scheduled run of the recurring job `brief`" in open(out).read()
    assert b.sql("SELECT next_run_ts FROM recurring_jobs")[0]["next_run_ts"] == before
