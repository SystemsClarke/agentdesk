"""The command line of the original's agentdesk.exe (Program.cs header): every verb exists and does its job."""
import json
import os
import time

from lib import Board, Missing, feature, make_stub

SRC = "src/AgentDesk.Cli/Program.cs"


def verb(b, *args, input=None):
    """Runs a verb; a verb the build does not have is Missing."""
    r = b.run(*args, input=input)
    if r.returncode == 2 and ("unrecognized subcommand" in r.stderr or "unexpected argument" in r.stderr):
        raise Missing("`agentdesk %s` is not a command here" % " ".join(args[:2]))
    return r


@feature("cli.mcp", "cli", "No arguments on a pipe is the MCP server on stdio (how every session reaches the board)", SRC)
def _mcp(b):
    r = b.run(input=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "ping"}) + "\n")
    assert json.loads(r.stdout.splitlines()[0])["result"] == {}


@feature("cli.hook", "cli", "`agentdesk hook <event>`: stdin JSON in, hook JSON out, never fails", SRC)
def _hook(b):
    assert verb(b, "hook", "context", input="{}").returncode == 0


@feature("cli.wait", "cli", "`agentdesk wait <thread>`", SRC)
def _wait(b):
    r = verb(b, "wait", "--help")
    assert r.returncode == 0


@feature("cli.job.new", "cli", "`agentdesk job new <name> <folder> --prompt T (--at HH:mm [--days D] | --every M) [--model m] [--catch-up M]`", SRC)
def _jnew(b):
    r = verb(b, "job", "new", "brief", b.work, "--prompt", "do it", "--at", "08:00", "--days", "weekdays", "--model", "haiku", "--catch-up", "60")
    assert r.returncode == 0, r.stderr
    j = json.loads(r.stdout)
    assert j["model"] == "haiku" and j["catch_up_minutes"] == 60 and j["days"] == "weekdays"
    assert verb(b, "job", "new", "bad name", b.work, "--prompt", "p", "--every", "5").returncode != 0


@feature("cli.job.list-enable-disable-delete", "cli", "`job list`, `job enable|disable <name>`, `job delete <name>`", SRC)
def _jmgmt(b):
    verb(b, "job", "new", "brief", b.work, "--prompt", "p", "--every", "120")
    assert json.loads(verb(b, "job", "list").stdout)["jobs"][0]["name"] == "brief"
    assert json.loads(verb(b, "job", "disable", "brief").stdout)["enabled"] == 0
    assert json.loads(verb(b, "job", "enable", "brief").stdout)["enabled"] == 1
    assert json.loads(verb(b, "job", "delete", "brief").stdout)["deleted"] == "brief"


@feature("cli.job.run", "cli", "`agentdesk job run <name>` starts a run now", SRC)
def _jrun(b):
    out = os.path.join(b.work, "ran.txt")
    b.stub("claude", "open(%r,'w').write('x')\n" % out)
    verb(b, "job", "new", "brief", b.work, "--prompt", "p", "--every", "120")
    assert verb(b, "job", "run", "brief").returncode == 0
    assert os.path.exists(out)


@feature("cli.sessions", "cli", "`agentdesk sessions` lists the headless sessions the core hosts", SRC)
def _sessions(b):
    r = verb(b, "sessions")
    assert "sessions" in json.loads(r.stdout)


@feature("cli.start-stop-session", "cli", "`agentdesk start <name> <folder> [command...]` / `stop <name>` run and end a plain session", SRC)
def _startstop(b):
    r = verb(b, "start", "s1", b.work, "python3", "-c", "import time;time.sleep(30)")
    assert json.loads(r.stdout)["name"] == "s1"
    assert "stopped" in verb(b, "stop", "s1").stdout


@feature("cli.attach", "cli", "`agentdesk attach <name>` uses a session from this console like tmux attach; Ctrl+] detaches", SRC)
def _attach(b):
    r = verb(b, "attach", "--help")
    assert r.returncode == 0


@feature("cli.identity", "cli", "`agentdesk new <name> <folder> [--charter T] [--host H] [--autostart]`, `list`, `start|stop|forget <name>`", SRC)
def _identity(b):
    r = verb(b, "new", "builder", b.work, "--charter", "builds", "--autostart")
    assert json.loads(r.stdout)["autostart"] == 1
    assert json.loads(verb(b, "list").stdout)["identities"][0]["name"] == "builder"
    assert "forgotten" in verb(b, "forget", "builder").stdout


@feature("cli.adopt", "cli", "`agentdesk adoptable` and `agentdesk adopt <session-id> <name>`", SRC)
def _adopt(b):
    assert "sessions" in json.loads(verb(b, "adoptable").stdout)


@feature("cli.update", "cli", "`agentdesk update [--apply]` checks GitHub Releases (and swaps the binary in)", SRC)
def _update(b):
    r = verb(b, "update", "--help")
    assert r.returncode == 0 and "--apply" in r.stdout


@feature("cli.goal", "cli", "`agentdesk goal new|approve|stop|status|list`", SRC)
def _goal(b):
    b.stub("claude", "import time\ntime.sleep(30)\n")
    r = verb(b, "goal", "new", "g1", b.work, "make", "it", "faster")
    assert json.loads(r.stdout)["state"] == "draft"
    assert "goals" in json.loads(verb(b, "goal", "list").stdout)


@feature("cli.migrate", "cli", "(new) `agentdesk migrate [--apply]` moves a machine from the .NET build without losing the board", "this repo")
def _migrate(b):
    cfg = os.path.join(b.home, ".claude.json")
    with open(cfg, "w") as f:
        json.dump({"mcpServers": {"agentdesk": {"command": "C:/old/agentdesk.exe", "args": []}}}, f)
    r = verb(b, "migrate")
    assert "dry run" in r.stdout and "C:/old" in open(cfg).read()
    verb(b, "migrate", "--apply")
    assert json.load(open(cfg))["mcpServers"]["agentdesk"]["args"] == ["mcp"]
