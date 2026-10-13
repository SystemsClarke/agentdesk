"""The agent swarm: identities and the session host, Phoenix, wake, goals and the Concierge, the session pool and the
usage governor. These need `agentdesk daemon` (whoever holds the supervisor lock owns the child sessions)."""
import json
import os
import time

from lib import Board, Missing, feature, make_stub, wait_for

SLEEPER = "import time\ntime.sleep(120)\n"


def up(b, claude=SLEEPER):
    argv_log = os.path.join(b.work, "claude-argv.log")
    b.stub("claude", "import json,os,sys,time\nopen(%r,'a').write(json.dumps({'argv':sys.argv[1:],'env':{k:v for k,v in os.environ.items() if k.startswith('AGENTDESK')},'cwd':os.getcwd()})+'\\n')\n%s" % (argv_log, claude))
    p = b.spawn("daemon")
    time.sleep(1.0)
    return p, argv_log


def launches(path):
    return [json.loads(l) for l in open(path).read().splitlines()] if os.path.exists(path) else []


def ident(b, name="builder", **kw):
    return b.need("ui:identity_create", dict({"name": name, "folder": b.work}, **kw))


@feature("identity.chain-charter", "swarm", "A started identity gets AGENTDESK_IDENTITY/AUTHOR, a standard chain charter (one generation of a long-lived agent; hand off at 60%) then its own charter", "docs/ui-api.md identity_start")
def _charter(b):
    p, log = up(b)
    ident(b, charter="Own the build.")
    b.need("ui:identity_start", {"name": "builder"})
    run = wait_for(lambda: launches(log), 8, what="claude to start")[0]
    a = run["argv"]
    prompt = a[a.index("--append-system-prompt") + 1]
    assert "pass_the_torch" in prompt and prompt.rstrip().endswith("Own the build.")
    assert run["env"]["AGENTDESK_IDENTITY"] == "builder" and run["env"]["AGENTDESK_AUTHOR"] == "builder"
    assert run["cwd"] == b.work


@feature("identity.resume", "swarm", "--resume <id> when that conversation has a transcript, else --session-id <new id> (recorded)", "docs/ui-api.md identity_start")
def _resume(b):
    p, log = up(b)
    ident(b)
    b.need("ui:identity_start", {"name": "builder"})
    first = wait_for(lambda: launches(log), 8, what="start")[0]["argv"]
    assert "--session-id" in first
    sid = b.sql("SELECT claude_session_id FROM identities")[0]["claude_session_id"]
    assert sid == first[first.index("--session-id") + 1]
    b.need("ui:identity_stop", {"name": "builder"})
    proj = os.path.join(b.home, ".claude", "projects", "p")
    os.makedirs(proj, exist_ok=True)
    open(os.path.join(proj, sid + ".jsonl"), "w").write(json.dumps({"type": "user", "cwd": b.work}) + "\n")
    b.need("ui:identity_start", {"name": "builder"})
    second = wait_for(lambda: len(launches(log)) > 1 and launches(log)[1], 8, what="restart")["argv"]
    assert "--resume" in second and sid in second


@feature("identity.pool-ceiling", "swarm", "Sessions run only up to max_sessions (default 3); past that an identity is queued and launched oldest first when a slot frees", "governor.md The session pool")
def _pool(b):
    p, log = up(b)
    with open(os.path.join(b.data, "settings.json"), "w") as f:
        json.dump({"max_sessions": 1}, f)
    ident(b, "a")
    ident(b, "b")
    assert b.need("ui:identity_start", {"name": "a"})["state"] == "running"
    assert b.need("ui:identity_start", {"name": "b"})["state"] == "queued"
    b.need("ui:identity_stop", {"name": "a"})
    wait_for(lambda: b.sql("SELECT state FROM identities WHERE name='b'")[0]["state"] == "running", 10, what="the queued one to start")


@feature("identity.restart-recovery", "swarm", "When the core starts it queues again every identity that was running or queued, and every autostart one", "docs/ui-api.md identity_start")
def _recovery(b):
    p, log = up(b)
    ident(b, "auto", autostart=True)
    p.kill()
    b.sql("UPDATE identities SET state='running' WHERE name='auto'")
    up(b)
    wait_for(lambda: len(launches(log)) >= 1, 10, what="autostart")


@feature("identity.stop-forget", "swarm", "Stop ends the session; forget deletes the row and leaves the conversation alone; stop on a plain session name stops that session", "docs/ui-api.md")
def _stopforget(b):
    p, log = up(b)
    ident(b)
    b.need("ui:identity_start", {"name": "builder"})
    assert b.need("ui:identity_stop", {"name": "builder"})["state"] == "stopped"
    assert b.need("ui:identity_forget", {"name": "builder"}) == {"forgotten": "builder"}


@feature("phoenix.handoff-restart", "swarm", "pass_the_torch by an identity's own session, then its next non-blocking Stop hook: successor starts (generation+1) in the same slot with the handoff as its first prompt; phoenix_chain recorded; bio thread notes it", "docs/ui-api.md Phoenix, phoenix.md")
def _phoenix(b):
    p, log = up(b)
    ident(b)
    b.need("ui:identity_start", {"name": "builder"})
    wait_for(lambda: launches(log), 8, what="start")
    sid = b.sql("SELECT claude_session_id FROM identities")[0]["claude_session_id"]
    env = {"AGENTDESK_IDENTITY": "builder", "AGENTDESK_AUTHOR": "builder", "CLAUDE_CODE_SESSION_ID": sid, "CLAUDECODE": "1"}
    m = b.mcp(env=env)
    m.initialize()
    m.call("pass_the_torch", {"handoff": "Owns X. Next: Y."})
    m.close()
    r = b.run("hook", "stop", input=json.dumps({"session_id": sid}), env=env)
    assert r.returncode == 0
    wait_for(lambda: b.sql("SELECT generation FROM identities")[0]["generation"] == 2, 15, what="generation 2")
    wait_for(lambda: len(launches(log)) >= 2, 15, what="the successor's session")  # the row changes before the new process is up
    second = launches(log)[-1]["argv"]
    assert "You are builder, generation 2" in " ".join(second) and "Owns X. Next: Y." in " ".join(second)
    c = b.sql("SELECT generation, handoff_msg FROM phoenix_chain")[0]
    assert c["generation"] == 1


@feature("phoenix.rate-limit", "swarm", "At most one Phoenix restart per identity per 2 minutes: a handoff inside that window just ends the turn", "docs/ui-api.md Phoenix")
def _phoenix_rate(b):
    p, log = up(b)
    ident(b)
    b.need("ui:identity_start", {"name": "builder"})
    wait_for(lambda: launches(log), 8, what="start")

    def handoff(note):
        sid = b.sql("SELECT claude_session_id FROM identities")[0]["claude_session_id"]
        env = {"AGENTDESK_IDENTITY": "builder", "AGENTDESK_AUTHOR": "builder", "CLAUDE_CODE_SESSION_ID": sid, "CLAUDECODE": "1"}
        m = b.mcp(env=env)
        m.initialize()
        m.call("pass_the_torch", {"handoff": note})
        m.close()
        b.run("hook", "stop", input=json.dumps({"session_id": sid}), env=env)

    handoff("first")
    wait_for(lambda: b.sql("SELECT generation FROM identities")[0]["generation"] == 2, 15, what="generation 2")
    wait_for(lambda: len(launches(log)) >= 2, 10, what="the successor")
    handoff("second, inside two minutes")
    time.sleep(6)
    assert b.sql("SELECT generation FROM identities")[0]["generation"] == 2, "a second restart inside 2 minutes is refused: the handoff just ends the turn"
    assert b.sql("SELECT COUNT(*) n FROM phoenix_chain")[0]["n"] == 1


@feature("phoenix.state-file", "swarm", "Each identity has state/<name>.md edited in place; the chain charter says so; the session starts with --add-dir for it; the successor's prompt carries the file (cut at 12,000 chars; phoenix_state_file:false removes it)", "phoenix.md")
def _statefile(b):
    p, log = up(b)
    ident(b)
    b.need("ui:identity_start", {"name": "builder"})
    run = wait_for(lambda: launches(log), 8, what="start")[0]["argv"]
    assert "--add-dir" in run and os.path.join(b.data, "state") in " ".join(run)
    assert "DECISIONS" in run[run.index("--append-system-prompt") + 1]


@feature("identity.wake", "swarm", "A running identity gets John's reply typed into its session (injected); a stopped one is started with it as its first prompt (resumed); a live-but-stuck one is left to see it on its next write", "docs/ui-api.md ui:wake")
def _wakeident(b):
    p, log = up(b)
    ident(b)
    tid = b.tool("ask_human", {"subject": "Q", "body": "Go?", "author": "builder"}, session="x")["thread_id"]
    b.need("ui:reply", {"thread_id": tid, "body": "yes do it"})
    d = b.need("ui:wake", {"thread_id": tid})
    assert "resumed" in json.dumps(b.sql("SELECT state FROM deliveries")) or d["said"].startswith("woke")
    wait_for(lambda: launches(log), 8, what="the wake to start the identity")
    assert "yes do it" in " ".join(launches(log)[0]["argv"])


@feature("identity.adopt-folders", "swarm", "Any agent's last Claude Code session (the sessions table) is adopted as an identity of the same name, in its folder, on wake", "docs/ui-api.md ui:wake")
def _adoptwake(b):
    p, log = up(b)
    proj = os.path.join(b.home, ".claude", "projects", "p")
    os.makedirs(proj, exist_ok=True)
    open(os.path.join(proj, "sess-scout.jsonl"), "w").write(json.dumps({"type": "user", "cwd": b.work}) + "\n")
    tid = b.tool("ask_human", {"subject": "Q", "body": "Go?", "author": "scout"}, session="sess-scout")["thread_id"]
    b.sql("UPDATE sessions SET cwd=? WHERE author='scout'", (b.work,))
    b.need("ui:reply", {"thread_id": tid, "body": "yes"})
    d = b.need("ui:wake", {"thread_id": tid})
    assert d["said"].startswith("woke scout")
    row = b.sql("SELECT folder, claude_session_id FROM identities WHERE name='scout'")[0]
    assert row["folder"] == b.work and row["claude_session_id"] == "sess-scout"
    run = wait_for(lambda: launches(log), 8, what="the resumed session")[0]["argv"]
    assert "--resume" in run and "sess-scout" in run


@feature("session.headless-env", "swarm", "A session starts with CLAUDE*, AGENTDESK_SESSION and AGENTDESK_AUTHOR dropped and AGENTDESK_HEADLESS=<name> set", "docs/ui-api.md ui:session_start")
def _headless(b):
    p = b.spawn("daemon")
    time.sleep(1)
    out = os.path.join(b.work, "env.txt")
    script = make_stub(b.stubs, "agent", "import os,time\nopen(%r,'w').write(repr(sorted(k for k in os.environ if k.startswith(('CLAUDE','AGENTDESK')))))\ntime.sleep(20)\n" % out)
    b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script}, env={"CLAUDE_CODE_SESSION_ID": "x", "CLAUDECODE": "1", "AGENTDESK_AUTHOR": "leak", "AGENTDESK_SESSION": "leak"})
    seen = wait_for(lambda: os.path.exists(out) and open(out).read(), 8, what="the session's environment")
    assert "'CLAUDE" not in seen and "AGENTDESK_AUTHOR" not in seen and "AGENTDESK_SESSION" not in seen and "AGENTDESK_HEADLESS" in seen, seen


@feature("session.attach-replay", "swarm", "A viewer gets the last 256 KB of output first, then everything new; one that falls 256 chunks behind is dropped with session.overflow", "docs/ui-api.md ui:attach")
def _replay(b):
    import base64
    import socket
    p = b.spawn("daemon")
    time.sleep(1)
    script = make_stub(b.stubs, "agent", "import sys,time\nprint('hello from the session', flush=True)\nfor line in sys.stdin:\n    print('echo:' + line.strip(), flush=True)\n")
    b.need("ui:session_start", {"name": "s1", "folder": b.work, "command": script})
    time.sleep(1.5)
    port, key = open(os.path.join(b.data, "supervisor.addr")).read().split()

    def viewer(cols=100, rows=30):
        c = socket.create_connection(("127.0.0.1", int(port)), timeout=5)
        f = c.makefile("rwb", buffering=0)
        f.write(("ATTACH %s plain:s1 %d %d\n" % (key, cols, rows)).encode())
        assert f.readline().strip() == b"OK"
        return c, f

    def read_until(c, needle):
        got = b""
        c.settimeout(5)
        while needle not in got:
            got += c.recv(4096)
        return got

    c1, f1 = viewer()
    read_until(c1, b"hello from the session")
    c2, f2 = viewer()
    read_until(c2, b"hello from the session")
    f1.write(("INPUT %s\n" % base64.b64encode(b"ping\r").decode()).encode())
    read_until(c2, b"echo:ping")
    bad = socket.create_connection(("127.0.0.1", int(port)), timeout=5)
    bad.sendall(b"ATTACH wrongkey plain:s1 80 24\n")
    assert bad.makefile("rb").readline().startswith(b"ERR")


# ---- goals

def goal(b, name="g1"):
    p, log = up(b)
    b.need("ui:goal_create", {"name": name, "objective": "reduce the count", "folder": b.work})
    return log


@feature("goal.measure-verdicts", "swarm", "experiment_done runs measure_cmd samples times in the goal's folder (10 min cap) and records the median of the last number printed with a verdict: met / improved / no gain / error", "docs/ui-api.md Goals")
def _measure(b):
    log = goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "python3 -c \"print('count 7')\"", "success": "value < 3", "samples": 3})
    b.need("ui:goal_approve", {"name": "g1"})
    env = {"AGENTDESK_IDENTITY": "g1-lead", "AGENTDESK_AUTHOR": "g1-lead", "CLAUDE_CODE_SESSION_ID": "lead", "CLAUDECODE": "1"}
    m = b.mcp(env=env)
    m.initialize()
    n = m.call("experiment_start", {"goal": "g1", "change": "cache it"})["n"]
    m.call("experiment_done", {"goal": "g1", "n": n})
    m.close()
    e = b.sql("SELECT value, verdict FROM experiments")[0]
    assert e["value"] == 7.0 and e["verdict"] in ("improved", "no gain")


@feature("goal.met-ends-goal", "swarm", "A measure that crosses the success line ends the goal as `succeeded`; `pass` success means exit code 0", "docs/ui-api.md Goals")
def _met(b):
    goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "python3 -c \"print(1)\"", "success": "value < 3"})
    b.need("ui:goal_approve", {"name": "g1"})
    env = {"AGENTDESK_IDENTITY": "g1-lead", "AGENTDESK_AUTHOR": "g1-lead", "CLAUDE_CODE_SESSION_ID": "lead", "CLAUDECODE": "1"}
    m = b.mcp(env=env)
    m.initialize()
    n = m.call("experiment_start", {"goal": "g1", "change": "x"})["n"]
    m.call("experiment_done", {"goal": "g1", "n": n})
    m.close()
    assert b.need("ui:goal_status", {"name": "g1"})["state"] == "succeeded"


@feature("goal.hours-exhausted", "swarm", "Every 15 s the core ends goals whose max_hours are spent (`exhausted`) and wakes leads whose cadence is due", "docs/ui-api.md Goals")
def _hours(b):
    goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "echo 5", "success": "value < 3"})
    b.need("ui:goal_approve", {"name": "g1", "max_hours": 1})
    b.sql("UPDATE goals SET started_ts='2020-01-01T00:00:00+00:00' WHERE name='g1'")
    wait_for(lambda: b.sql("SELECT state FROM goals")[0]["state"] == "exhausted", 40, what="exhaustion")


@feature("goal.member-spawn", "swarm", "member_spawn (lead only) starts <goal>-<name> as the pool allows (queued past max_members); work_id hands a claimed Work to Hire item to the member", "docs/ui-api.md Goals")
def _spawn(b):
    goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "echo 5", "success": "value < 3"})
    b.need("ui:goal_approve", {"name": "g1"})
    env = {"AGENTDESK_IDENTITY": "g1-lead", "AGENTDESK_AUTHOR": "g1-lead", "CLAUDE_CODE_SESSION_ID": "lead", "CLAUDECODE": "1"}
    m = b.mcp(env=env)
    m.initialize()
    r = m.call("member_spawn", {"goal": "g1", "name": "m1", "task": "do a small thing", "model": "haiku"})
    m.close()
    assert b.sql("SELECT identity FROM goal_members")[0]["identity"] == "g1-m1", r


@feature("goal.member-done", "swarm", "member_done posts the summary, forgets the member, wakes the lead; an item it held but did not complete goes back to the lead", "docs/ui-api.md Goals")
def _mdone(b):
    goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "echo 5", "success": "value < 3"})
    b.need("ui:goal_approve", {"name": "g1"})
    lead = {"AGENTDESK_IDENTITY": "g1-lead", "AGENTDESK_AUTHOR": "g1-lead", "CLAUDE_CODE_SESSION_ID": "lead", "CLAUDECODE": "1"}
    m = b.mcp(env=lead)
    m.initialize()
    m.call("member_spawn", {"goal": "g1", "name": "m1", "task": "small", "model": "haiku"})
    m.close()
    member = {"AGENTDESK_IDENTITY": "g1-m1", "AGENTDESK_AUTHOR": "g1-m1", "CLAUDE_CODE_SESSION_ID": "mem", "CLAUDECODE": "1"}
    m = b.mcp(env=member)
    m.initialize()
    m.call("member_done", {"summary": "I tidied the cache."})
    m.close()
    assert b.sql("SELECT COUNT(*) n FROM goal_members")[0]["n"] == 0
    tid = b.sql("SELECT thread_id FROM goals WHERE name='g1'")[0]["thread_id"]
    assert any("I tidied the cache." in x["body"] for x in b.sql("SELECT body FROM messages WHERE thread_id=?", (tid,)))
    assert not b.sql("SELECT * FROM identities WHERE name='g1-m1'"), "the member is forgotten"


@feature("goal.work-returns", "swarm", "When a goal ends, work items its lead or members still hold go back on the queue, open", "docs/ui-api.md Goals")
def _wreturn(b):
    goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "echo 5", "success": "value < 3"})
    b.need("ui:goal_approve", {"name": "g1"})
    wid = b.tool("post_work", {"subject": "job", "body": "do", "author": "boss", "claim": "anyone"})["thread_id"]
    lead = {"AGENTDESK_IDENTITY": "g1-lead", "AGENTDESK_AUTHOR": "g1-lead", "CLAUDE_CODE_SESSION_ID": "lead", "CLAUDECODE": "1"}
    m = b.mcp(env=lead)
    m.initialize()
    assert m.call("claim_work", {"thread_id": wid})["claimed"] is True
    m.call("member_spawn", {"goal": "g1", "name": "m1", "task": "the job", "work_id": wid})
    m.close()
    assert b.sql("SELECT json_extract(meta,'$.assignee') a FROM threads WHERE id=?", (wid,))[0]["a"] == "g1-m1", "the claim is handed to the member"
    b.need("ui:goal_stop", {"name": "g1"})
    t = b.sql("SELECT status, meta FROM threads WHERE id=?", (wid,))[0]
    assert t["status"] == "open" and "assignee" not in (t["meta"] or ""), t


@feature("goal.internal-measure", "swarm", "measure_cmd internal:open_work counts Work to Hire items open and not reserved (claim: anyone); standing goals idle at the line and never end", "docs/ui-api.md Goals")
def _internal(b):
    p, log = up(b)
    b.tool("post_work", {"subject": "a", "body": "b", "author": "x"})
    b.tool("post_work", {"subject": "c", "body": "d", "author": "x", "claim": "anyone"})
    d = b.need("ui:concierge", {"on": True})
    assert d["open"] == 1


@feature("goal.lead-wake", "swarm", "A lead is woken by cadence with the goal summary typed into its session (or started with it as its prompt)", "docs/ui-api.md Goals")
def _leadwake(b):
    log = goal(b)
    b.need("ui:goal_propose", {"name": "g1", "hypothesis": "h", "measure_cmd": "echo 5", "success": "value < 3"})
    b.need("ui:goal_approve", {"name": "g1", "cadence_minutes": 1})
    b.need("ui:identity_stop", {"name": "g1-lead"})
    n = len(launches(log))
    b.sql("UPDATE goals SET woke_ts='2020-01-01T00:00:00+00:00' WHERE name='g1'")
    run = wait_for(lambda: len(launches(log)) > n and launches(log)[-1], 45, what="the lead to be woken")["argv"]
    assert "g1" in " ".join(run) and "reduce the count" in " ".join(run), "started with the goal summary as its prompt"


@feature("goal.permissions", "swarm", "goal_propose is for the lead or John; approve/budget/stop are John-only or the lead as documented", "docs/ui-api.md Goals")
def _gperm(b):
    goal(b)
    doc, _ = b.ui("ui:goal_approve", {"name": "g1"}, env={"AGENTDESK_IDENTITY": "someone", "CLAUDECODE": "1", "CLAUDE_CODE_SESSION_ID": "s"})
    assert "error" in doc


@feature("concierge.drains-work", "swarm", "The Concierge (a standing goal) keeps Work to Hire drained: its lead claims open items and hands them to a small swarm", "docs/ui-api.md ui:concierge")
def _drain(b):
    p, log = up(b)
    wid = b.tool("post_work", {"subject": "Build it", "body": "please", "author": "boss"})["thread_id"]
    b.need("ui:concierge", {"on": True})
    wait_for(lambda: b.sql("SELECT status FROM threads WHERE id=?", (wid,))[0]["status"] == "claimed", 40, what="the dispatcher to hand the item to a worker")
    assert b.sql("SELECT COUNT(*) n FROM goal_members WHERE goal='concierge'")[0]["n"] >= 1


# ---- governor

def sample(b, ts, weekly, five=0, sessions=0):
    b.sql("INSERT INTO usage_samples (ts, weekly_pct, five_hour_pct, swarm_sessions) VALUES (?,?,?,?)", (ts, weekly, five, sessions))


@feature("governor.forecast", "governor", "From usage_samples: hourly rates, John's baseline (EWMA per hour of week), sigma, spendable = remaining - margin - baseline - k*sigma*sqrt(T)", "governor.md Forecast")
def _forecast(b):
    b.need("ui:governor")
    import datetime as dt
    now = dt.datetime.now(dt.timezone.utc)
    for i in range(48):
        t = now - dt.timedelta(hours=47 - i)
        sample(b, t.strftime("%Y-%m-%dT%H:%M:%S+00:00"), 10 + i * 0.5)
    reset = (now + dt.timedelta(days=3)).strftime("%Y-%m-%dT%H:%M:%S+00:00")
    b.sql("UPDATE usage_samples SET weekly_reset_ts=?", (reset,))
    d = b.need("ui:governor")
    assert d["samples"] == 48 and d["fresh"] is True
    assert 30 < d["remaining"] < 90 and d["spendable"] >= 0 and "baseline_source" in d


@feature("governor.caps-and-tiers", "governor", "Caps (sessions, swarms x members) from the allowed rate / per-session burn; five_hour>=90 recommends no swarm sessions; step-down tiers at <1 session or 5h>=75", "governor.md Forecast")
def _caps(b):
    import datetime as dt
    b.need("ui:governor")
    now = dt.datetime.now(dt.timezone.utc)
    reset = (now + dt.timedelta(days=3)).strftime("%Y-%m-%dT%H:%M:%S+00:00")
    for i in range(12):
        t = (now - dt.timedelta(hours=11 - i)).strftime("%Y-%m-%dT%H:%M:%S+00:00")
        b.sql("INSERT INTO usage_samples (ts, weekly_pct, weekly_reset_ts, five_hour_pct, swarm_sessions) VALUES (?,?,?,?,0)", (t, 10 + i * 0.2, reset, 95 if i == 11 else 10))
    d = b.need("ui:governor")
    assert d["caps"]["new_sessions"] == 0 and d["caps"]["total_sessions"] == 0, "five_hour >= 90: no swarm sessions at all: %s" % d["caps"]
    b.sql("UPDATE usage_samples SET five_hour_pct=80 WHERE ts=(SELECT MAX(ts) FROM usage_samples)")
    d = b.need("ui:governor")
    assert d["models"]["step_down"] is True and d["models"]["member"] == "haiku", "five_hour >= 75: step down a tier: %s" % d["models"]


@feature("governor.fail-closed", "governor", "A stale sample (>30 min) or two failed /usage probes fail closed: no new swarm session starts, and `reason` says why", "governor.md, ui-api.md ui:governor")
def _failclosed(b):
    b.need("ui:governor")
    sample(b, "2020-01-01T00:00:00+00:00", 10)
    d = b.need("ui:governor")
    assert d["fresh"] is False and d["fail_closed"]


@feature("governor.enforcement", "governor", "governor_enforce (default off): when on, sessions past the plan's total are queued/shed at the next start or 15 s tick; advisory logs what it would have done", "governor.md Enforcement")
def _enforce(b):
    import datetime as dt
    p, log = up(b)
    now = dt.datetime.now(dt.timezone.utc)
    reset = (now + dt.timedelta(days=3)).strftime("%Y-%m-%dT%H:%M:%S+00:00")
    for i in range(12):
        t = (now - dt.timedelta(minutes=55 - i * 5)).strftime("%Y-%m-%dT%H:%M:%S+00:00")
        b.sql("INSERT INTO usage_samples (ts, weekly_pct, weekly_reset_ts, five_hour_pct, swarm_sessions) VALUES (?,?,?,?,0)", (t, 10, reset, 95))
    b.need("ui:governor_enforce", {"on": False})
    b.need("ui:goal_create", {"name": "g1", "objective": "o", "folder": b.work})
    wait_for(lambda: b.sql("SELECT state FROM identities WHERE name='g1-lead'")[0]["state"] == "running", 15, what="advisory mode to start the lead anyway")
    b.need("ui:identity_stop", {"name": "g1-lead"})
    b.need("ui:governor_enforce", {"on": True})
    b.need("ui:identity_start", {"name": "g1-lead"})
    time.sleep(6)
    assert b.sql("SELECT state FROM identities WHERE name='g1-lead'")[0]["state"] == "queued", "enforcing, the plan's cap of 0 holds the swarm lead queued"


@feature("governor.pool-fair-share", "governor", "Goals split the swarm's part of the ceiling: lead first, then members round-robin, longest-waiting goal first; John's own identities answer only to max_sessions", "governor.md The session pool")
def _fair(b):
    d = b.need("ui:status")
    if "sessions" not in d:
        raise Missing("ui:status has no session pool section")
    assert "owners" in d["sessions"]


@feature("governor.session-sampling", "governor", "Every reading is recorded in usage_samples with the running swarm session count and the per-tier counts (running_model)", "governor.md Samples")
def _sampling(b):
    import datetime as dt
    now = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S+00:00")
    with open(os.path.join(b.data, "claude_usage.json"), "w") as f:
        json.dump({"five_hour": {"used": 20, "resets_at": now}, "seven_day": {"used": 30, "resets_at": now}, "captured_ts": now}, f)
    p, log = up(b)
    row = wait_for(lambda: b.sql("SELECT * FROM usage_samples"), 15, what="a sample")[0]
    assert row["weekly_pct"] == 30 and row["five_hour_pct"] == 20
    for col in ("swarm_sessions", "haiku_sessions", "sonnet_sessions", "opus_sessions"):
        assert col in row, col


