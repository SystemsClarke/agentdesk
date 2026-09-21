# AgentDesk — deployment notes for IT / security review

## What this is

AgentDesk is a personal productivity tool: a small message-board application
that lets one person (the account owner) coordinate with a set of AI coding
assistant sessions running on the same machine. It is a Tkinter (Windows
GUI) application backed by a local SQLite database. It has no network
listener, accepts no inbound connections, and talks to no server the account
owner does not already use directly (GitHub, Azure DevOps, the local AI
assistant CLI already installed on the machine).

It was flagged and auto-contained by this machine's endpoint security
product (COMODO Internet Security / Endpoint Manager). This document exists
to give a security reviewer everything needed to make an allow-list decision
without a call.

## Why it launches other processes — the part that looks the most like malware and isn't

AgentDesk shells out to three external programs as child processes. This is
deliberate, narrow, and each one is explained below. **This is also almost
certainly why it gets flagged**: an unsigned (see below) binary that spawns
`powershell.exe` and other executables matches a very common heuristic
signature for droppers and loaders. The behavior is real; the reason for it
is not malicious, and is documented here so it doesn't have to be
discovered.

| Child process | Why | What it's given |
|---|---|---|
| `gh.exe` (GitHub CLI) | Checks whether a pull request the account owner is waiting to merge has been merged yet, and pulls PR titles/status for a tracking view. Runs on a timer, read-only GitHub API calls only. | The PR's URL. Nothing else. Uses the account owner's own already-authenticated `gh` session — AgentDesk stores no GitHub credential of its own. |
| `powershell.exe` | Fires a Windows toast notification (`Show-Toast`-style WinRT call) when the app can't use the tray-icon notification API directly. This is the ONLY thing this call does — it never runs an arbitrary script, only ever a short embedded notification snippet, passed via `-EncodedCommand` (base64) so no shell-quoting layer can inject anything into it. | A notification title and body string only. |
| `claude`/`claude.cmd` (the account owner's own AI coding assistant CLI, already installed and already used interactively) | Spawns short-lived assistant sessions that pick up items from the app's own internal work queue (a to-do list the account owner and the assistant sessions share) and do the work, then report back into the board. This is the core feature of the app — it is an orchestrator for sessions of a tool the owner already runs by hand. | A text prompt describing one work item. No credentials are passed to it beyond what the owner's own already-configured `claude` CLI already has. |

None of these three are downloaded, dropped, or written to disk by
AgentDesk — they are the account owner's own pre-existing, pre-installed
tools, located via the normal `PATH` lookup (`shutil.which`), never a bundled
or hidden copy.

## Signing status — no internal cert exists yet, so this is self-signed

There is currently no internal code-signing certificate at this
organization. Rather than ship unsigned, the executable is self-signed:

```
Subject:    CN=AgentDesk (self-signed, palencharj)
Thumbprint: AE6661701A6D11E50423B66216D26410489953BD
Expires:    2031-09-21
```

A self-signed certificate does not carry a trusted root, so this alone does
not resolve the block — a reviewer still has to make a trust decision. What
it buys: every future build signed with this same certificate carries the
same, stable, verifiable identity. **Allow-listing this one certificate
thumbprint (rather than a specific file hash) means every future update
is automatically trusted too, with no repeated review per release.** If the
organization later stands up an internal signing CA, re-signing with that
cert is a one-command change (see the build notes at the end of this file)
and this section should be updated.

## What was actually observed when this was tested (measured, not theoretical)

The compiled, self-signed executable was placed at
`%LocalAppData%\Programs\AgentDesk\AgentDesk.exe` by its own installer, on
this machine, on 2026-09-21. It was auto-quarantined by COMODO within
seconds — confirmed directly in
`C:\ProgramData\Comodo\Cis\Quarantine\data\`, which holds two captured
copies of a 60,992,536-byte file (matching `AgentDesk.exe`'s exact size),
timestamped at the moments each copy was written to disk. **This happened
without the file being executed from that installed location** — the
capture appears to trigger on the file being written/first-seen, not on
run, which is consistent with an unknown-publisher heuristic rather than a
behavioral detection of anything the program actually did.

Practical implication for whoever reviews this: an allow-list or exclusion
will likely need to be in place **before** installation, not applied
reactively after a failed run, since the file may never survive long enough
to run once.

## Install location, what gets installed, and what does not

- **Install location:** `%LocalAppData%\Programs\AgentDesk\` — a per-user
  install, no administrator rights required (`PrivilegesRequired=lowest` in
  the installer). This is a single-user productivity tool, not a
  multi-user or system-wide deployment.
- **Installer:** `AgentDesk-Setup.exe`, built with Inno Setup, signed with
  the same certificate as the app itself.
- **What it installs:** the compiled application and its bundled runtime
  dependencies (Python's Tk GUI toolkit, SQLite, and Windows COM
  interop libraries — all statically bundled by the Nuitka compiler, no
  separate Python installation required on the target machine), a Start
  Menu shortcut, and two Windows Scheduled Tasks (below).
- **What it does NOT touch:** `%LocalAppData%\AgentDesk\` (note: no
  `Programs\` in that path) — this is where the actual data lives (the
  SQLite database, application log, and in-memory worker state). Neither
  install nor uninstall ever reads, writes, or deletes anything there, so
  the account owner's data survives an uninstall/reinstall/upgrade cycle
  intact.

## The two Scheduled Tasks

| Task name | Trigger | What it runs | Why |
|---|---|---|---|
| `AgentDesk-Startup` | At logon (20s delay) | `AgentDesk.exe` (GUI, no arguments) | Opens the board window at logon so it's on screen without the owner having to remember to launch it. Runs as the logged-in user's own interactive token — no stored credential, no elevated privilege, and it does nothing if nobody is logged on. |
| `AgentDesk-Backup` | Every hour | `AgentDesk.exe backup` | Takes an online (safe, non-corrupting) snapshot of the local SQLite database and mirrors the day's messages as plain text into a separate git repository the account owner already owns and controls (their personal notes/memory vault). **No data leaves the machine except to that repository, which is the account owner's own, already-existing, already-authorized destination** — this is not a new external transmission path. |

Both tasks run at the logged-in user's own privilege level (no elevation,
no service account, no stored password) — see the comments in
`scripts/Install-AgentDeskStartup.ps1` and `scripts/Install-AgentDeskTask.ps1`
for the full reasoning.

## Where the data lives, and what it contains

`%LocalAppData%\AgentDesk\agentdesk.db` — a single SQLite file. Contents:
message-board threads and messages between the account owner and their own
AI assistant sessions (work items, questions, discussion), plus PR-tracking
metadata (GitHub PR URLs and their merge status). No credentials, no
customer data, no third-party PII are stored in this database by design —
it is the account owner's own working notes.

## For the reviewer: the shortest version

1. It's a personal to-do/notification tool, not a server, not a network
   service, and it accepts no inbound connections.
2. It shells out to `gh`, `powershell.exe`, and the account owner's own AI
   CLI — all pre-existing tools on the machine, never downloaded or bundled
   by this app, and each use is narrow and explained above.
3. It's self-signed (no internal CA exists yet); allow-listing the
   certificate thumbprint covers all future updates automatically.
4. It was empirically observed being auto-quarantined on write, before
   execution — plan for a pre-install exclusion, not a post-failure fix.
5. Per-user install, no admin rights, doesn't touch system-wide paths, and
   uninstalling it never touches the account owner's data.

## Build notes, for whoever produces the next release

```
# 1. Compile (from the repo root, with its .venv active):
python -m nuitka --standalone --follow-imports --enable-plugin=tk-inter ^
  --windows-console-mode=disable --assume-yes-for-downloads ^
  --output-dir=<a path OUTSIDE any OneDrive-synced folder> ^
  --output-filename=AgentDesk.exe agentdesk\cli.py

# IMPORTANT: build to a local, non-synced path (e.g. C:\AgentDeskBuild).
# Building inside the OneDrive-synced repo folder was tried and caused the
# compiled exe to disappear partway through signing/testing -- traced to
# the same COMODO auto-containment described above acting on a freshly
# written, unsigned-root binary the moment it landed on disk, not to
# OneDrive itself (that was the first, wrong theory; ruled out once the
# quarantine folder was checked directly).

# 2. Sign it (re-run for every new build; the cert has a private key at
#    C:\Users\palencharj\NoOneDrive\AgentDesk-signing\agentdesk-codesign.pfx,
#    password stored in the "Claude-Code" 1Password Environment as
#    AGENTDESK_CODESIGN_PFX_PASSWORD):
signtool sign /sha1 AE6661701A6D11E50423B66216D26410489953BD /fd SHA256 ^
  /t http://timestamp.digicert.com <path>\AgentDesk.exe

# 3. Build the installer (Inno Setup; update SourceExeDir at the top of
#    scripts\AgentDesk.iss to point at your build output first):
"<Inno Setup install dir>\ISCC.exe" scripts\AgentDesk.iss

# 4. Sign the installer exe the same way as step 2.
```

The single-entry-point dispatcher (`agentdesk/cli.py`) is what makes one exe
cover the GUI, the MCP server, the background work dispatcher, and the
backup job — see its own docstring for the subcommand list.
