"""Behaviour added in the Rust build after the port: the folder step of the new-agent/goal/job forms and `ui:folders`, the script
runner, the session switcher's LIST protocol, the core's own hourly backup, and child processes that open no window. They are new
here, so `source` names this repo's files rather than the original's."""
import os
import re
import socket
import sys
import time

from lib import ROOT, Waived, feature, make_stub, wait_for

WIN = sys.platform == "win32"


def snap_has(s, *needles):
    gone = [n for n in needles if n not in s["text"]]
    assert not gone, "the screen lacks %s:\n%s" % (gone, s["text"])


@feature("ui.folder-step", "ui-api", "New agent, goal and job ask for a folder: Enter alone takes the folder the window runs in (it is shown in the prompt); a typed one is stored", "src/tui/mod.rs ask_next")
def _folder_step(b):
    same = lambda x, y: os.path.normcase(os.path.realpath(x)) == os.path.normcase(os.path.realpath(y))
    snap_has(b.snap("a n t:builder enter"), "NEW AGENT", "The folder it works in")
    b.snap("a n t:builder enter enter enter")  # folder: Enter for the default, charter: Enter skips
    b.snap("a n t:typed enter t:%s enter enter" % b.work.replace(" ", "~"))
    rows = {r["name"]: r["folder"] for r in b.sql("SELECT name, folder FROM identities")}
    assert same(rows["builder"], b.work) and same(rows["typed"], b.work), rows
    # a goal and a job show the default of the Enter key
    snap_has(b.snap("a g t:g1 enter"), "NEW GOAL", "Enter for ")
    snap_has(b.snap("r n t:nightly enter"), "NEW RECURRING JOB", "Enter for ")


def _write_thread(b, body):
    q = b.tool("ask_human", {"subject": "Run this?", "body": body, "author": "builder"}, session="s1")["thread_id"]
    return q


@feature("reader.script-runner", "screens", "Alt+R in the reader shows a thread's fenced shell script without running it; a second Alt+R runs it and streams the output into a pane; no script says so", "src/tui/runner.rs")
def _runner(b):
    lang = "cmd" if WIN else "sh"
    _write_thread(b, "Please run this, ok?\n\n```%s\necho parity-runner-ok\n```\n" % lang)
    s = b.snap("q enter alt+r")
    snap_has(s, "Script 1/1", "Alt+R runs it")
    assert "running" not in s["text"] and "exit 0" not in s["text"], "shown, not run"
    s = b.snap("q enter alt+r alt+r wait:2500")
    # the pane's own copy of the script is the line starting `>`; the output is a bare line after it
    out = [l for l in s["lines"] if l.strip("│ ").strip() == "parity-runner-ok"]
    assert out, "the output reaches the pane: " + s["text"]
    assert "done (exit 0)" in s["text"], s["text"]


@feature("reader.script-runner-none", "screens", "Alt+R on a thread without a fenced shell block flashes that it needs one, and opens nothing", "src/tui/runner.rs")
def _runner_none(b):
    _write_thread(b, "Just a question, no code?")
    s = b.snap("q enter alt+r")
    assert "No script in this thread" in (s["flash"] or "") + s["text"], s["text"]


def _addr(b):
    port, key = open(os.path.join(b.data, "supervisor.addr")).read().split()
    return int(port), key


def _list(port, key):
    c = socket.create_connection(("127.0.0.1", port), timeout=5)
    f = c.makefile("rwb", buffering=0)
    f.write(("LIST %s\n" % key).encode())
    lines = []
    while True:
        l = f.readline().decode().strip()
        if not l or l == "END":
            break
        lines.append(l)
    c.close()
    return lines


@feature("attach.session-list", "swarm", "The supervisor answers `LIST <key>` with one session name per line then END (the attach switcher walks it: identities first, then plain:<name>); a wrong key gets an ERR, not the list", "src/attach.rs")
def _session_list(b):
    b.spawn("daemon")
    time.sleep(1)
    script = make_stub(b.stubs, "agent", "import time\ntime.sleep(60)\n")
    for n in ("zeta", "alpha"):
        b.need("ui:session_start", {"name": n, "folder": b.work, "command": script})
    wait_for(lambda: os.path.exists(os.path.join(b.data, "supervisor.addr")), 8, what="supervisor.addr")
    port, key = _addr(b)
    names = wait_for(lambda: (lambda l: l if len(l) >= 2 else None)(_list(port, key)), 8, what="both sessions in the list")
    assert names == ["plain:alpha", "plain:zeta"], names
    assert _list(port, "wrong-key") != names
    assert not any(n == "plain:alpha" for n in _list(port, "wrong-key"))


@feature("backup.in-core", "backup", "The core takes the hourly backup itself (no scheduled task): a board with no snapshot gets one shortly after the daemon starts", "src/service.rs backup_if_due")
def _in_core_backup(b):
    b.env.pop("AGENTDESK_NO_BACKUP", None)
    b.env["AGENTDESK_BACKUP_FIRST_SECS"] = "1"
    b.need("ui:identity_create", {"name": "builder", "folder": b.work})
    b.spawn("daemon")
    d = os.path.join(b.data, "backups")
    snaps = wait_for(lambda: os.path.isdir(d) and [f for f in os.listdir(d) if re.match(r"agentdesk-\d{8}T\d{6}\.db$", f)], 20, what="the daemon's own backup")
    assert len(snaps) == 1, snaps


@feature("core.no-console-windows", "core", "Every child process the core or tray starts goes through paths::new_command / hide (CREATE_NO_WINDOW on Windows): no bare Command::new outside the helper and the tray's own hidden launcher", "src/paths.rs new_command")
def _hidden_children(b):
    allowed = {os.path.join("src", "paths.rs"), os.path.join("src", "tray", "mod.rs"), os.path.join("src", "tests.rs"), os.path.join("src", "tui", "tests_pty.rs")}
    bare = []
    for root, _, files in os.walk(os.path.join(ROOT, "src")):
        for f in files:
            if not f.endswith(".rs"):
                continue
            path = os.path.join(root, f)
            rel = os.path.relpath(path, ROOT)
            if rel in allowed:
                continue
            for n, line in enumerate(open(path, encoding="utf-8"), 1):
                if re.search(r"std::process::Command::new\(|\bCommand::new\(", line) and "CommandBuilder" not in line:
                    bare.append("%s:%d" % (rel, n))
    assert not bare, "start these through paths::new_command: %s" % bare
    helper = open(os.path.join(ROOT, "src", "paths.rs"), encoding="utf-8").read()
    assert "0x0800_0000" in helper and "pub fn new_command" in helper, "the helper must set CREATE_NO_WINDOW"
    tray = open(os.path.join(ROOT, "src", "tray", "mod.rs"), encoding="utf-8").read()
    assert "0x0800_0000" in tray, "the tray's own launcher sets CREATE_NO_WINDOW"


@feature("options.font-size", "screens", "Options: a font size setting and Ctrl +/- zoom", "parity/README.md, Deliberate non-ports")
def _font(b):
    raise Waived("a terminal program draws in the terminal's font: size and zoom belong to the terminal (Ctrl +/- there)")


@feature("startup.open-window", "install", "Open the window at logon (the .NET app's startup option)", "parity/README.md, Deliberate non-ports")
def _logon_window(b):
    raise Waived("a TUI has no window of its own to open; the tray's Open entry launches a terminal running `agentdesk tui` instead (see tray.terminal-override)")
