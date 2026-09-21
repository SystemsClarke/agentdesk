"""Does a session still read as `claude` on the board? Run this and see.

Five checks, in rising order of how much they assume (the numbers below are the
section headers in the output):

 1. DERIVATION. Two sessions' worth of environment in, two different author
    strings out, and the rendering rules for a derived name and a legacy one.
 2. THE COLUMN. The label widths are measured in the app's own Tk font against
    the width the "by" column actually has, and the output says plainly what is
    measured and what a font-blind module cannot prove.
 3. END TO END, over stdio, against the real MCP server. Two server processes
    are started the way the client starts them -- own environment, own working
    directory -- each posts to a scratch database, and the rows that land are
    printed. This is the acceptance test: it does not call the tool function
    in-process, it speaks the protocol to a separate process.
 4. THROUGH THE WINDOW'S OWN CODE. The real `Page` widget is built over the
    scratch database and its refresh_list / _on_select run unmodified, so what
    is printed is the column and the detail pane as they will actually be
    drawn -- not a second implementation that agrees with the first by
    construction.
 5. THE CREW'S ROLE STAMP. Each role runs through the real `run_claude` with a
    stand-in CLI that reports the environment it was handed, so "a role keeps
    its name across a session reset" is a child process's answer and not a
    reading of the source.

    .venv\\Scripts\\python.exe scripts\\check_identity.py

Nothing here touches the real board: the child processes are given their own
LOCALAPPDATA under a temp directory, which is where paths.py puts the database.
"""

from __future__ import annotations

import asyncio
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PY = str(REPO / ".venv" / "Scripts" / "python.exe")
if not Path(PY).exists():
    PY = sys.executable
sys.path.insert(0, str(REPO))

from agentdesk import identity  # noqa: E402

FAILURES: list[str] = []


def check(what: str, got, want) -> None:
    ok = got == want
    print(f"  [{'ok ' if ok else 'FAIL'}] {what}: {got!r}"
          + ("" if ok else f"   (wanted {want!r})"))
    if not ok:
        FAILURES.append(what)


def hr(title: str) -> None:
    print(f"\n=== {title} ===")


# --- 1. derivation -------------------------------------------------------------

SESSION_A = {
    "CLAUDECODE": "1",
    "CLAUDE_CODE_ENTRYPOINT": "cli",
    "CLAUDE_CODE_SESSION_ID": "a1b2c3d4-0000-0000-0000-000000000001",
}
SESSION_B = {
    "CLAUDECODE": "1",
    "CLAUDE_CODE_ENTRYPOINT": "sdk-cli",
    "CLAUDE_CODE_SESSION_ID": "9f8e7d6c-0000-0000-0000-000000000002",
}


def part1() -> None:
    hr("1. derivation")
    a = identity.session_identity(SESSION_A, cwd=r"C:\work\project-alpha")
    b = identity.session_identity(SESSION_B, cwd=r"C:\work\project-alpha")
    c = identity.session_identity(SESSION_A, cwd=r"C:\work\project-beta")
    print(f"  session A in project-alpha -> {a}")
    print(f"  session B in project-alpha -> {b}")
    print(f"  session A in project-beta  -> {c}")
    check("two sessions, same project, different authors", a != b, True)
    check("same session, different projects, different authors", a != c, True)

    check("a session may not post as the harness",
          identity.resolve("claude", SESSION_A, cwd=r"C:\work\project-alpha"), a)
    check("a session may not post as an empty name",
          identity.resolve("", SESSION_A, cwd=r"C:\work\project-alpha"), a)
    check("a role keeps its own name",
          identity.resolve("builder", SESSION_A, cwd=r"C:\work\project-alpha"),
          "builder")
    check("a stamped role wins over a generic argument",
          identity.resolve("claude", {"AGENTDESK_AUTHOR": "verifier"},
                           cwd=r"C:\work\project-alpha"), "verifier")
    check("the human is untouched",
          identity.resolve("john", SESSION_A, cwd=r"C:\work\project-alpha"), "john")

    hr("1b. rendering")
    print(f"  {a!r}")
    print(f"    by column  -> {identity.label(a)!r}")
    print(f"    detail pane-> {identity.describe(a)!r}")
    for legacy in ("claude", "claude-code", "builder", "researcher",
                   "work-dispatcher", "agentdesk-crew", "john"):
        print(f"  legacy {legacy!r}: by {identity.label(legacy)!r} / "
              f"detail {identity.describe(legacy)!r}")
    check("a legacy name renders as itself",
          identity.describe("builder"), "builder")
    check("no label exceeds the character cap",
          max(len(identity.label(a)),
              len(identity.label(c)),
              len(identity.label(identity.session_identity(SESSION_B,
                                                           cwd=r"C:\work\x"))))
          <= identity.LABEL_MAX, True)


# --- 2. end to end through the real server --------------------------------------

def _child_env(localappdata: Path, session: dict) -> dict:
    env = dict(os.environ)
    env.update(session)
    env["LOCALAPPDATA"] = str(localappdata)
    env["PYTHONPATH"] = str(REPO)
    env.pop("AGENTDESK_AUTHOR", None)  # no role: the anonymous case
    return env


async def _post(project: Path, localappdata: Path, env: dict, subject: str,
                body: str) -> str:
    from mcp import StdioServerParameters
    from mcp.client.session import ClientSession
    from mcp.client.stdio import stdio_client

    params = StdioServerParameters(command=PY, args=["-m", "agentdesk.mcp_server"],
                                   env=env, cwd=str(project))
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            tools = await session.list_tools()
            result = await session.call_tool(
                "post_message",
                {"channel": "discussion", "subject": subject, "body": body})
    return result.content[0].text, [t.name for t in tools.tools]


def part_e2e(scratch: Path) -> None:
    hr("3. two sessions, two real MCP server processes, one scratch database")
    localappdata = scratch / "appdata"
    for name in ("project-alpha", "project-beta"):
        (scratch / name).mkdir(parents=True, exist_ok=True)
    print(f"  database: {localappdata / 'AgentDesk' / 'agentdesk.db'}")
    print("  (LOCALAPPDATA is redirected for the children; the real board is "
          "not touched)")

    text_a, tool_names = asyncio.run(_post(
        scratch / "project-alpha", localappdata, _child_env(localappdata, SESSION_A),
        "from the alpha session", "posted by session A"))
    print(f"\n  session A post_message -> {text_a}")
    print(f"  tools the server advertised: {', '.join(tool_names)}")
    text_b, _ = asyncio.run(_post(
        scratch / "project-beta", localappdata, _child_env(localappdata, SESSION_B),
        "from the beta session", "posted by session B"))
    print(f"  session B post_message -> {text_b}\n")

    db_path = localappdata / "AgentDesk" / "agentdesk.db"
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys=ON")
    # A row of the OLD shape, written straight into the scratch database, to
    # show what a board that predates this change looks like next to the new
    # ones. Nothing rewrites it: the fix applies to posts, not to history.
    conn.execute(
        "INSERT INTO threads (created_ts, updated_ts, channel, subject,"
        " opened_by, status) VALUES ('2026-01-01T00:00:00+00:00',"
        " '2026-01-01T00:00:00+00:00', 'discussion', 'an old thread',"
        " 'claude', 'fyi')")
    conn.execute(
        "INSERT INTO messages (ts, thread_id, author, author_kind, body)"
        " VALUES ('2026-01-01T00:00:00+00:00', 3, 'claude', 'agent',"
        " 'posted before the change')")
    conn.commit()
    rows = [dict(r) for r in conn.execute(
        "SELECT subject, opened_by FROM threads ORDER BY id")]
    conn.close()
    print("  the board as the window would draw it (old row included):")
    authors = []
    for r in rows:
        authors.append(r["opened_by"])
        print(f"    {identity.label(r['opened_by']):<14} | {r['subject']}")
        print(f"        detail pane | {identity.describe(r['opened_by'])} (agent)")
    new_authors = [r["opened_by"] for r in rows if r["subject"] != "an old thread"]
    check("two sessions produced two new rows", len(new_authors), 2)
    check("...with two different authors", len(set(new_authors)), 2)
    check("neither new author is the bare harness name",
          any(identity.is_anonymous(a) for a in new_authors), False)
    check("the old row is left exactly as it was",
          [r["opened_by"] for r in rows if r["subject"] == "an old thread"],
          ["claude"])


# --- 3. the column ---------------------------------------------------------------

def part_column() -> None:
    hr("2. the 'by' column, measured in the app's own font")
    import tkinter as tk
    from tkinter import font as tkfont

    COLUMN = 80          # app.py: self.tree.column("by", width=80, ...)
    PADDING = 8          # what a Treeview cell spends left and right
    roof = COLUMN - PADDING

    root = tk.Tk()
    root.withdraw()
    try:
        font = tkfont.nametofont("TkDefaultFont")
        print(f"  column width: {COLUMN}px (unchanged from before this change)")
        print(f"  'Opened by' heading occupies: {font.measure('Opened by')}px")
        # There is NO pixel bound here and it would be dishonest to print one.
        # The cap is on characters, and 11 capitals are wider than 11 lower-
        # case letters. What is measured below is every label a real board
        # produces; a pathological all-capitals project name could still clip
        # -- exactly as "work-dispatcher" already clips today at 86px.
        print(f"  the cap is {identity.LABEL_MAX} CHARACTERS, not pixels; a "
              f"hard pixel bound\n  is not available to a font-blind module, "
              f"so what follows is measured, not proved")

        worst, who = 0, ""
        samples = ["builder", "verifier", "researcher", "john", "claude",
                   "claude-code", "agentdesk-crew", "work-dispatcher"]
        for env, cwd in ((SESSION_A, r"C:\work\project-alpha"),
                         (SESSION_B, r"C:\work\short"),
                         (SESSION_A, r"C:\work\a-very-long-project-name")):
            samples.append(identity.session_identity(env, cwd=cwd))
        for s in samples:
            lbl = identity.label(s)
            w = font.measure(lbl)
            if w > worst:
                worst, who = w, f"{s} -> {lbl!r}"
            print(f"  {w:4d}px  {lbl:<14} (from {s!r})")
        print(f"  widest real label: {worst}px ({who})")
        check(f"every real label fits the {roof}px usable width",
              worst <= roof, True)
    finally:
        root.destroy()


# --- 4. through the window's own code --------------------------------------------

def part_window(db_path: Path) -> None:
    hr("4. the real Page widget, rendering that same database")
    import tkinter as tk

    from agentdesk import app as appmod
    from agentdesk import db as dbmod

    class Stub:
        """Just enough App for a Page: it only ever calls compose() and reads
        db_path. The window's own refresh_list / refresh_detail / _on_select
        run unmodified against these rows -- this is not a re-implementation
        of the rendering, it is the rendering."""
        def __init__(self, db_path):
            self.db_path = db_path
            self.icon = None

        def compose(self, channel):
            pass

        def post_reply(self, channel, thread_id, body):
            pass

    root = tk.Tk()
    root.withdraw()
    try:
        page = appmod.Page(root, Stub(db_path), "discussion")
        conn = dbmod.connect(db_path)
        try:
            page.refresh_list(conn)
            # values are (open, subject, by, updated, msgs); the thread id is
            # the row's iid, and the list is newest-activity-first.
            listed = [(page.tree.item(i)["values"][1], page.tree.item(i)["values"][2])
                      for i in page.tree.get_children()]
            print("  the 'by' column, as the window builds it:")
            for subject, by in listed:
                print(f"    {by!r:<16} {subject}")
            cells = dict(listed)
            check("the column holds the short form, not the stored string",
                  cells["from the alpha session"],
                  identity.label("claude-code:project-alpha#a1b2"))
            check("two sessions are two different cells",
                  len({cells["from the alpha session"],
                       cells["from the beta session"]}), 2)
            check("the pre-change row still reads as it was stored",
                  cells["an old thread"], "claude")

            target = [i for i in page.tree.get_children()
                      if page.tree.item(i)["values"][1] == "from the alpha session"]
            page.tree.selection_set(target[0])
            page._on_select()
            detail = page.msgs_txt.get("1.0", "end-1c")
        finally:
            conn.close()
        print("  the detail pane, as the window builds it:")
        for line in detail.splitlines()[:2]:
            print(f"    {line}")
        check("the detail pane names the harness, the project and the session",
              "claude-code in project-alpha, session a1b2 (agent)" in detail, True)
    finally:
        root.destroy()


# --- 5. the crew's role stamp ----------------------------------------------------

def part_crew(scratch: Path) -> None:
    hr("5. a crew role's session keeps its role, across session resets")
    from agentdesk import crew, roles

    fake = scratch / "claude.bat"
    # A stand-in for the claude CLI: it reports the environment it was handed,
    # in the JSON shape run_claude parses. It is a real child process, which is
    # the only thing a helper-level test would not show -- and the reason the
    # stamp exists is that complete_work matches on this name.
    fake.write_text('@echo off\r\n'
                    'echo {"result":"%AGENTDESK_AUTHOR%","is_error":false}\r\n',
                    encoding="ascii")
    for role in roles.ROLES:
        crew._claude_exe = lambda: str(fake)
        res = crew.run_claude(role, "check", None, timeout=30)
        print(f"  {role.name:<10} child inherited AGENTDESK_AUTHOR={res.result!r}")
        check(f"{role.name}: the spawned session resolves to its role",
              identity.resolve("claude", {"AGENTDESK_AUTHOR": res.result or ""},
                               cwd=r"C:\x\AgentDesk"), role.name)
    check("...even when the model passes no author at all",
          identity.resolve(None, {"AGENTDESK_AUTHOR": "builder"},
                           cwd=r"C:\x\AgentDesk"), "builder")


def main() -> int:
    part1()
    part_column()
    with tempfile.TemporaryDirectory(prefix="agentdesk-identity-") as tmp:
        scratch = Path(tmp)
        part_e2e(scratch)
        part_window(scratch / "appdata" / "AgentDesk" / "agentdesk.db")
        part_crew(scratch)
    hr("result")
    if FAILURES:
        print(f"  {len(FAILURES)} check(s) FAILED:")
        for f in FAILURES:
            print(f"    - {f}")
        return 1
    print("  all checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
