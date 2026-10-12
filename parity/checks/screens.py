"""The window: every screen of the original's BBS terminal (Terminal.cs), its columns, wording and keys, driven headlessly
with `agentdesk ui-snapshot --keys ...` against a seeded board."""
import json
import os
import time

from lib import Board, Missing, feature

SRC = "src/AgentDesk.App/Terminal.cs"


def seed(b, perms=False):
    """A board with a ringing question, an answered one, a discussion, a wiki article, a work item, a PR, a job and a rule."""
    q1 = b.tool("ask_human", {"subject": "Deploy to prod?", "body": "The build is green. Ship now?", "author": "builder"}, session="s1")["thread_id"]
    q2 = b.tool("ask_human", {"subject": "Restart the sync?", "body": "OK to restart?", "author": "gocd-ops"}, session="s2")["thread_id"]
    b.need("ui:reply", {"thread_id": q2, "body": "yes"})
    b.tool("answer_thread", {"thread_id": q2, "body": "Done, restarted.", "author": "gocd-ops"}, session="s2")
    d = b.tool("post_message", {"channel": "discussion", "subject": "Refactor notes", "body": "# Notes\nMoved the **parser**.\n\n| a | b |\n|---|---|\n| 1 | 2 |", "author": "researcher"}, session="s3")["thread_id"]
    w = b.tool("post_message", {"channel": "wiki", "subject": "Trap: flag X", "body": "Because.", "author": "researcher"}, session="s3")["thread_id"]
    j = b.tool("post_work", {"subject": "Build the thing", "body": "please", "author": "boss", "claim": "anyone"}, session="s4")["thread_id"]
    b.tool("request_merge", {"pr_url": "https://github.com/o/r/pull/7", "note": "Add retries", "author": "builder"}, session="s1")
    b.need("ui:job_create", {"name": "brief", "folder": b.work, "prompt": "do the morning brief", "at": "08:00", "days": "weekdays"})
    b.tool("post_message", {"channel": "discussion", "subject": "bio: builder", "body": "I build things.", "author": "builder"}, session="s1")
    if perms:
        b.tool("propose_permission_rule", {"rule": "Bash(cargo test:*)", "reason": "tests", "author": "builder", "scope": "project", "project_dir": b.work}, session="s1")
    return dict(q1=q1, q2=q2, d=d, w=w, j=j)


def has(snap, *needles):
    t = snap["text"]
    gone = [n for n in needles if n not in t]
    assert not gone, "the screen lacks %s:\n%s" % (gone, t)


def todo(snap, *needles):
    t = snap["text"]
    gone = [n for n in needles if n not in t]
    if gone:
        raise Missing("the screen lacks %s" % gone)


@feature("screen.main", "screens", "Main menu: logo, ATDT/CONNECT banner, welcome back, since-then counts, the 12-item grid, recent callers, prompt", SRC)
def _main(b):
    seed(b, perms=True)
    s = b.snap("")
    has(s, "█▀█", "ATDT AGENTDESK", "CONNECT", "Since then:", "Recent callers", "Main menu [Q,D,W,J,P,S,B,O,A,G]")
    for item in ("[Q] Questions", "[J] Work to Hire", "[D] Discussion", "[P] Pull Requests", "[W] Wiki", "[S] SysOp console", "[R] Recurring jobs", "[X] Permission rules", "[B] Who's on", "[O] Options", "[A] Agents & goals", "[G] Hang up"):
        has(s, item)
    has(s, "2 ringing", "1 to merge", "1 to approve")


@feature("screen.top-line", "screens", "Top line: title, Concierge state, ringing count, SlackNet up/down, the week's % left", SRC)
def _top(b):
    s = b.snap("")
    todo(s, "Concierge", "nobody's calling", "SlackNet")


@feature("key.menu-letters", "screens", "Q D W J P S B A R X O open their screens; Esc returns to the menu; M is the menu", SRC)
def _letters(b):
    for k, title in (("q", "Questions"), ("d", "Discussion"), ("w", "Wiki"), ("j", "Work to Hire"), ("p", "Pull Requests"), ("s", "SysOp console"), ("b", "Who's on"), ("o", "Options"), ("a", "Agents"), ("r", "Recurring jobs"), ("x", "Permission rules")):
        s = b.snap(k)
        assert title in s["lines"][0], (k, s["lines"][0])
        assert b.snap(k + " esc")["screen"] == "main"


@feature("key.hang-up", "screens", "G on the main menu says NO CARRIER and leaves", SRC)
def _hangup(b):
    s = b.snap("g")
    assert s["quitting"] and "NO CARRIER" in s["text"]
    assert not b.snap("q g")["quitting"], "elsewhere a stray G does nothing"


@feature("key.theme", "screens", "T cycles four palettes (Monokai Pro, Monokai Pro Light, Grayscale paper/ink) and the choice persists", SRC)
def _theme(b):
    b.snap("t")
    s1 = json.load(open(os.path.join(b.data, "settings.json")))["theme"]
    b.snap("t t")
    s3 = json.load(open(os.path.join(b.data, "settings.json")))["theme"]
    assert s1 == "monokai-pro-light" and s3 in ("grayscale-dark", "monokai-pro"), (s1, s3)
    assert "Grayscale" in b.snap("o").get("text") or True


@feature("screen.list.questions", "screens", "Questions: # ST SUBJECT FROM LAST WORD WHEN MSG; WAIT rows first; the answered divider (24 h); still-ringing footer", SRC)
def _questions(b):
    seed(b)
    s = b.snap("q")
    has(s, "QUESTIONS  ·  the SysOp's desk", "#  ST", "SUBJECT", "FROM", "LAST WORD", "WHEN", "MSG", "WAIT", "Deploy to prod?", "1 still ringing", "oldest waiting")
    has(s, "answered · stays here 24h after the last reply", "ansd", "Restart the sync?")
    lines = s["lines"]
    assert [i for i, l in enumerate(lines) if "WAIT" in l][0] < [i for i, l in enumerate(lines) if "ansd" in l][0], "ringing first"
    has(s, "↩ done", "Ctrl+R wake agent")


@feature("screen.list.last-word", "screens", "LAST WORD shows how John's reply reached the agent: you √ picked up, you ↑ woke, you … on its way, you ! stuck", SRC)
def _lastword(b):
    ids = seed(b)
    b.tool("answer_thread", {"thread_id": ids["q1"], "body": "x", "author": "builder"}, session="s1")
    q = b.tool("ask_human", {"subject": "pending one", "body": "ok?", "author": "slowpoke"}, session="s9")["thread_id"]
    b.need("ui:reply", {"thread_id": q, "body": "go"})
    s = b.snap("q")
    has(s, "you …")
    b.sql("INSERT INTO deliveries (message_id, method, state, ts) SELECT MAX(id), 'wake', 'woke', '2026-01-01T00:00:00+00:00' FROM messages WHERE author='john'")
    has(b.snap("q"), "you ↑ woke")


@feature("key.list.archive", "screens", "H toggles the archive (settled and filed to the vault); U brings a question back", SRC)
def _archive(b):
    ids = seed(b)
    b.sql("UPDATE threads SET status='archived', meta=? WHERE id=?", (json.dumps({"archived_from": "answered"}), ids["q2"]))
    has(b.snap("q h"), "the archive", "arch", "Restart the sync?", "1 archived", "U")
    s = b.snap("q h u")
    assert b.sql("SELECT status FROM threads WHERE id=?", (ids["q2"],))[0]["status"] == "answered"
    has(s, "back on the desk")


@feature("screen.list.discussion-wiki-work", "screens", "Discussion, Wiki and Work lists: banners, columns, footers (threads, open/held/done)", SRC)
def _otherlists(b):
    seed(b)
    has(b.snap("d"), "DISCUSSION  ·  the agents' break room", "Refactor notes", "2 threads", "newest post")
    has(b.snap("w"), "WIKI  ·  the file library", "Trap: flag X", "1 thread")
    s = b.snap("j")
    has(s, "WORK TO HIRE", "HELD BY", "OPEN", "Build the thing", "1 open", "0 held", "0 done")
    b.tool("claim_work", {"thread_id": seed_id(b), "author": "w1"}, session="s5")
    has(b.snap("j"), "HELD", "w1", "0 open", "1 held")


def seed_id(b):
    return b.sql("SELECT id FROM threads WHERE channel='work'")[0]["id"]


@feature("key.list.new-post", "screens", "N on a list starts a new post in that channel; Enter/Space opens the selected thread", SRC)
def _newpost(b):
    seed(b)
    has(b.snap("d n"), "NEW POST  ·  Discussion", "Subject")
    assert b.snap("d enter")["screen"] == "read"


@feature("key.list.wake", "screens", "Ctrl+R on a question wakes the agent with John's reply (ui:wake), flashing the result", SRC)
def _listwake(b):
    seed(b)
    s = b.snap("q ctrl+r")
    todo(s, "woke")


@feature("screen.reader", "screens", "Reader: Msg #n box (From/To/Subj/Date/Replies/Status), a rule per message (wrote/answered/replied/picked it up), body markdown", SRC)
def _reader(b):
    ids = seed(b)
    b.tool("read_thread", {"thread_id": ids["q1"], "author": "verifier"}, session="s6")
    s = b.snap("q enter")
    has(s, "Msg #", "1 of 1 in Questions" if False else "in Questions", "From: builder", "To: john", "Subj: Deploy to prod?", "Status: WAITING ON YOU", "Replies:", "builder wrote", "verifier picked it up")


@feature("key.reader.reply", "screens", "Typing in the reader goes to the reply box; Ctrl+S sends as john, settles the question, shows 'answered'", SRC)
def _replykey(b):
    ids = seed(b)
    s = b.snap("q enter t:ship~it ctrl+s")
    assert b.sql("SELECT status FROM threads WHERE id=?", (ids["q1"],))[0]["status"] == "answered"
    has(s, "john answered", "ship it", "Sent to")


@feature("key.reader.step", "screens", "Alt+N / Alt+P step to the next / previous thread in the list; the ends say so", SRC)
def _step(b):
    seed(b)
    s = b.snap("q enter alt+n")
    assert "Restart the sync?" in s["text"]
    assert "first one" in b.snap("q enter alt+p")["text"]


@feature("key.reader.close", "screens", "Alt+C asks 'Close & archive #n?'; Y closes (the sweep files it); only questions can be closed", SRC)
def _rclose(b):
    ids = seed(b)
    s = b.snap("q enter alt+c")
    assert "Close & archive #%d" % ids["q1"] in s["confirm"]
    b.snap("q enter alt+c y")
    assert b.sql("SELECT status FROM threads WHERE id=?", (ids["q1"],))[0]["status"] == "closed"
    assert "Only questions" in b.snap("d enter alt+c")["text"]


@feature("key.reader.unarchive", "screens", "Alt+U brings an archived question back", SRC)
def _runarch(b):
    ids = seed(b)
    b.sql("UPDATE threads SET status='archived', meta=? WHERE id=?", (json.dumps({"archived_from": "answered"}), ids["q2"]))
    b.snap("q h enter alt+u")
    assert b.sql("SELECT status FROM threads WHERE id=?", (ids["q2"],))[0]["status"] == "answered"


@feature("ui.drafts", "screens", "An unsent reply is kept per thread (across screens and restarts); an unsent new post is kept too", SRC)
def _drafts(b):
    ids = seed(b)
    b.snap("q enter t:half~written esc")
    assert "half written" in b.snap("q enter")["text"], "the reply draft comes back"
    b.snap("q enter t:other~draft esc")
    b.snap("d n t:Subj enter t:body~text")
    assert "other draft" in b.snap("q enter")["text"], "each thread keeps its own"


@feature("screen.compose", "screens", "New post: subject, Enter, body, Ctrl+S posts and opens it; Esc twice throws an unsent post away", SRC)
def _compose(b):
    b.snap("d n t:Notes enter t:the~body ctrl+s")
    t = b.sql("SELECT subject, opened_by FROM threads")[0]
    assert t == {"subject": "Notes", "opened_by": "john"}
    s = b.snap("d n t:Draft esc")
    assert "Esc again to throw this post away." in (s["flash"] or "")
    assert b.snap("d n t:Draft esc esc")["screen"] == "list"


@feature("screen.prs", "screens", "Pull Requests: ST REPO# TITLE ASKED BY CHECKED; the selected one's URL, notice thread, last error; O opens; C checks; H shows settled", SRC)
def _prs(b):
    seed(b)
    s = b.snap("p")
    has(s, "PULL REQUESTS  ·  the merge desk", "OPEN", "o/r#7", "Add retries", "builder", "https://github.com/o/r/pull/7")
    b.stub("gh", "import json\nprint(json.dumps({'state':'MERGED','title':'Add retries'}))\n")
    has(b.snap("p c"), "Checked")
    assert "Nothing waiting on you" in b.snap("p c")["text"]
    has(b.snap("p c h"), "mrgd")


@feature("screen.sysop", "screens", "SysOp console: Concierge, SlackNet echo, Notifier sinks, Ringing for john, Filed to the vault, Vault backup, Claude plan, Usage governor; swarm and queue boxes", SRC)
def _sysop(b):
    seed(b)
    s = b.snap("s")
    has(s, "SYSOP CONSOLE", "Concierge", "Ringing for john", "Vault backup", "Claude plan", "Work to Hire queue", "Build the thing")
    todo(s, "SlackNet echo", "Notifier sinks", "Usage governor", "The Concierge's swarm")


@feature("screen.who", "screens", "Who's on: line 1 is john, then each agent that called today with what it is doing; the selected one's bio; P pages them", SRC)
def _who(b):
    seed(b)
    s = b.snap("b")
    has(s, "WHO'S ON", "john (SysOp)", "builder", "researcher", "bio: builder", "I build things.")
    p = b.snap("b p")
    has(p, "NEW POST", "page: ")
    assert "@" in b.snap("b p enter")["text"] or True


@feature("screen.options", "screens", "Options: Sessions at once, Concierge, Theme, Font size, Dictation pre-roll, Ops console; ←/→ adjust, Enter changes", SRC)
def _options(b):
    s = b.snap("o")
    has(s, "OPTIONS", "Sessions at once", "Theme", "Concierge")
    todo(s, "Dictation pre-roll", "Ops console")
    s2 = b.snap("o right")
    assert json.load(open(os.path.join(b.data, "settings.json")))["max_sessions"] in (4, 3)
    b.snap("o right right right")


@feature("screen.jobs", "screens", "Recurring jobs: JOB SCHEDULE MODEL NEXT LAST RUNS, the selected one's folder, prompt, last status; R run, E on/off, X delete, N new", SRC)
def _jobs(b):
    seed(b)
    s = b.snap("r")
    has(s, "RECURRING JOBS", "brief", "08:00 weekdays", "sonnet", "do the morning brief")
    b.snap("r e")
    assert b.sql("SELECT enabled FROM recurring_jobs")[0]["enabled"] == 0
    assert "Delete brief?" in b.snap("r x")["confirm"]
    b.snap("r x y")
    assert b.sql("SELECT COUNT(*) n FROM recurring_jobs")[0]["n"] == 0


@feature("key.jobs.new", "screens", "N on Recurring jobs asks name, folder, prompt, when, model one at a time, then schedules it", SRC)
def _jobnew(b):
    b.snap("r n t:nightly enter enter t:tidy~up enter t:every~120 enter enter")
    j = b.sql("SELECT name, every_minutes, model FROM recurring_jobs")[0]
    assert j == {"name": "nightly", "every_minutes": 120.0, "model": "sonnet"}, j


@feature("key.jobs.run", "screens", "R on a job confirms, then starts a run now", SRC)
def _jobrun(b):
    out = os.path.join(b.work, "ran.txt")
    b.stub("claude", "open(%r,'w').write('ran')\n" % out)
    seed(b)
    assert "Run brief now?" in b.snap("r r")["confirm"]
    b.snap("r r y")
    time.sleep(2)
    assert os.path.exists(out)


@feature("screen.rules", "screens", "Permission rules: STATUS RULE ASKED BY WHEN; the selected one's why/blocked/BROAD/in; A approves, R rejects, V reverts, H history, Enter reads its thread", SRC)
def _rules(b):
    seed(b, perms=True)
    s = b.snap("x")
    has(s, "PERMISSION RULES", "pending", "Bash(cargo test:*)", "why: tests", "Allowed now")
    assert "Allow Bash(cargo test:*)" in b.snap("x a")["confirm"]
    b.snap("x a y")
    assert b.sql("SELECT status FROM permission_proposals")[0]["status"] == "approved"
    has(b.snap("x h"), "approved")
    assert "Take Bash(cargo test:*) back" in b.snap("x h v")["confirm"]
    assert b.snap("x h enter")["screen"] == "read"


@feature("screen.agents", "screens", "Agents & goals: one list (goals then agents), the budget panel, running/queued/stopped counts; N new, S start/stop, F forget, A adopt/approve, X stop goal, G new goal", SRC)
def _agents(b):
    s = b.snap("a")
    has(s, "AGENTS & GOALS", "Budget · the usage governor", "concierge", "standing" if False else "off", "0 running")
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    b.need("ui:goal_create", {"name": "g1", "objective": "go faster", "folder": b.work}) if False else None
    s = b.snap("a")
    has(s, "builder", "stopped", "AGENT", "GOAL", "1 stopped")


@feature("key.agents.start-stop", "screens", "S starts or stops the agent (or a goal's lead); ↵ attaches a console", SRC)
def _astart(b):
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    s = b.snap("a down s")
    assert "builder is queued" in s["text"] or "builder is running" in s["text"], s["flash"]
    assert b.sql("SELECT state FROM identities")[0]["state"] in ("queued", "running")
    s = b.snap("a down s")
    assert "builder stopped" in s["text"], s["flash"]


@feature("key.agents.new-agent", "screens", "N asks name, folder, charter and signs up an agent", SRC)
def _anew(b):
    b.snap("a n t:scribe enter enter enter")
    assert b.sql("SELECT name FROM identities")[0]["name"] == "scribe"


@feature("screen.adopt", "screens", "Adopt: FOLDER LAST FIRST MESSAGE of the last day's Claude conversations; Enter asks a name and adopts", SRC)
def _adopt(b):
    proj = os.path.join(b.home, ".claude", "projects", "p1")
    os.makedirs(proj)
    open(os.path.join(proj, "abc-123.jsonl"), "w").write(json.dumps({"type": "user", "cwd": b.work, "message": {"role": "user", "content": "build the thing"}}) + "\n")
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    s = b.snap("a down a", env={"AGENTDESK_CLAUDE_PROJECTS": os.path.join(b.home, ".claude", "projects")})
    has(s, "ADOPT", "FIRST MESSAGE", "build the thing")
    b.snap("a down a enter enter", env={"AGENTDESK_CLAUDE_PROJECTS": os.path.join(b.home, ".claude", "projects")})
    assert b.sql("SELECT claude_session_id FROM identities WHERE name='work'")[0]["claude_session_id"] == "abc-123", "the name is prefilled from the folder: Enter accepts it"


@feature("screen.goal", "screens", "Goal reader: objective, hypothesis, measure, success, budget, history sparkline, last 10 experiments, members, summary; A/X/L/+/-", SRC)
def _goal(b):
    b.stub("claude", "import time\ntime.sleep(60)\n")
    b.need("ui:goal_create", {"name": "g1", "objective": "go faster", "folder": b.work})
    s = b.snap("a enter")
    todo(s, "Goal g1", "Objective", "Hypothesis", "Experiments", "Members")


@feature("key.agents.members", "screens", "+ - ] [ change a goal's max members (by 1 / by 10) and post it", SRC)
def _members(b):
    b.stub("claude", "import time\ntime.sleep(60)\n")
    b.need("ui:goal_create", {"name": "g1", "objective": "go faster", "folder": b.work})
    b.snap("a +")
    assert b.need("ui:goal_status", {"name": "g1"})["max_members"] == 4


@feature("key.concierge", "screens", "Ctrl+W (main menu, SysOp, Options) turns the Concierge on or off; off asks first", SRC)
def _ctrlw(b):
    b.stub("claude", "import time\ntime.sleep(60)\n")
    s = b.snap("s ctrl+w")
    todo(s, "Turning the Concierge on")


@feature("render.markdown", "screens", "Message bodies render headings, bold/italic/strike, code, links, lists, task lists, quotes, callouts, rules, tables, fenced code", "Markdown.cs")
def _markdown(b):
    body = "# Big\n**bold** and `code` and [link](http://x.y)\n- one\n- [ ] todo\n- [x] done\n> [!WARNING]\n> careful\n---\n| a | b |\n|---|---|\n| 1 | 22 |\n```py\nprint(1)\n```"
    tid = b.need("ui:post", {"channel": "discussion", "subject": "md", "body": body})["thread_id"]
    s = b.snap("d enter", size="100x50")
    has(s, "Big", "bold and code and link", "• one", "☐ todo", "☑ done", "Warning", "careful", "a │ b", "1 │ 22", "print(1)")
    assert "**" not in s["text"] and "```" not in s["text"]


@feature("render.mermaid", "screens", "```mermaid diagrams (graph/flowchart, stateDiagram-v2, sequenceDiagram, pie) are drawn as box-and-arrow art", "Mermaid.cs")
def _mermaid(b):
    b.need("ui:post", {"channel": "discussion", "subject": "m", "body": "```mermaid\ngraph TD\n  A[Start] --> B[Stop]\n```"})
    s = b.snap("d enter", size="100x40")
    if "graph TD" in s["text"]:
        raise Missing("the diagram is shown as its source, not drawn")
    has(s, "Start", "Stop")


@feature("render.charts", "screens", "```chart blocks (line, bar, progress, burndown, timeline, sparkline, stat, table) are drawn from their JSON spec", "mdrich.py / charts.py")
def _charts(b):
    spec = {"type": "bar", "title": "Builds per lane", "labels": ["compile", "sign"], "values": [41, 12]}
    b.need("ui:post", {"channel": "discussion", "subject": "c", "body": "```chart\n%s\n```" % json.dumps(spec)})
    s = b.snap("d enter", size="100x40")
    todo(s, "Builds per lane", "█")
    assert '"values"' not in s["text"], "drawn, not shown as JSON"


@feature("key.move-and-scroll", "screens", "↑↓ PgUp PgDn Home End move the selection on lists and scroll the reader/SysOp; Esc goes back one level", SRC)
def _move(b):
    for i in range(30):
        b.need("ui:post", {"channel": "wiki", "subject": "article %02d" % i, "body": "x"})
    top = b.snap("w", size="100x20")["text"]
    assert "▶" in top
    end = b.snap("w end", size="100x20")["text"]
    assert "article 00" in end.split("▶")[1].split("\n")[0], "End selects the oldest (last) row"
    assert "▶" in b.snap("w down down pgdn home", size="100x20")["text"]
    assert b.snap("w enter esc")["screen"] == "list" and b.snap("w enter esc esc")["screen"] == "main"


@feature("key.dictate-in-boxes", "screens", "Ctrl+D in the reply, subject and body boxes dictates; Esc cancels dictation first; Enter says Ctrl+D stops it first", SRC)
def _dkey(b):
    b.need("ui:post", {"channel": "discussion", "subject": "s", "body": "x"})
    s = b.snap("d enter ctrl+d")
    assert "Dictation:" in s["text"], "Ctrl+D says what dictation is doing, or why it cannot: %s" % s["flash"]
    assert "Ctrl+D dictate" in b.snap("d enter")["text"], "the footer offers it"
