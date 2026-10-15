# AgentDesk: what to know before porting it (to Rust, a terminal UI, and Linux)

Written by AgentDesk_Updater, the agent that has maintained the Windows implementation, for the agent building the Rust / terminal /
Linux version. Treat it as a brain transplant: what the thing is for, what it does, where the traps are. Read `README.md`,
`docs/phoenix.md`, `docs/governor.md`, `docs/GOAL.md`, `docs/permissions.md` and `docs/ui-api.md` after this; they are the detail.

## 1. What AgentDesk is for

John runs a swarm of Claude Code sessions. AgentDesk is their shared brain and his one window onto them:

- **A message board** (SQLite): agents ask John questions, post findings, hand work off. A question typed into a terminal is a question
  nobody read; a question on the board rings John's window, a toast and his phone (Slack).
- **A session supervisor**: it launches, resumes and retires long-lived agents ("identities"), keeps their number inside what his Claude
  plan can afford (the governor), runs scheduled jobs, and runs goals (a lead plus members chasing a measured number).
- **A memory**: every message is backed up into a markdown vault (Obsidian, git) with links.

The design rule behind everything: **the board is the swarm's shared memory, John is the lead, every agent is short-lived**. Agents hand
off at 60% context (Phoenix) rather than compact. Humans decide; agents propose.

## 2. Architecture today (Windows)

| Piece | Job |
|---|---|
| `AgentDesk.Core` | The one per-user process. Owns the SQLite board (`%LOCALAPPDATA%\AgentDesk\agentdesk.db`, WAL), serves a named pipe, runs sessions in ConPTY, tray + toasts, PR checker, usage meter, jobs, governor, Slack bridge supervision. |
| `AgentDesk.Cli` (`agentdesk.exe`) | What a Claude Code session runs: an MCP server on stdio (tool calls relayed to the core), `agentdesk hook <event>`, `agentdesk wait <thread>`, `attach`, `job`, `update`. |
| `AgentDesk.App` | WPF window: a BBS-style *text* UI (lines of coloured segments in a RichTextBox). Talks to the core with `ui:*` pipe requests (`docs/ui-api.md`). |
| `AgentDesk.Contracts` | The pipe protocol, `Tools.g.cs` (every MCP tool: name, description, JSON schema, dispatch), `Caller`. |
| `agentdesk/*.py` | The Python that remains: vault mirroring/search, the hourly backup, Slack bridge. A good executable specification of the data model (`db.py`) and the vault layout. |

**Everything is a request to the core**, `{tool, args, caller}` over the pipe. The window, the CLI relay and the web console are all
just clients; the core is the only writer. Port the core first and keep that shape.

## 3. The data model (the contract)

Tables in `src/AgentDesk.Core/Board/BoardStore.cs` (and mirrored in `agentdesk/db.py`): `threads`, `messages`, `presence`, `acks`,
`receipts`, `pull_requests`, `work_events`, `handoffs`, `sessions`, `deliveries`, `identities`, `phoenix_chain`, `goals`, `experiments`,
`goal_members`, `slots`, `recurring_jobs`, `permission_proposals`. Keep the **same schema**: John's existing database, the backups
(`agentdesk/backup.py` snapshots and restores it) and the vault tooling all depend on it. Schema version is in `PRAGMA user_version`.

- A **thread** has a channel (`question`, `discussion`, `wiki`, `work`), a subject, an opener and a status. `question` threads are born
  `open`; **only a human reply closes one** (an agent's reply never does, on purpose: the toast repeats until John answers).
- A **message** has an author, an `author_kind` (`human` / `agent`), a `kind` (`ack`, `phoenix`, `read-receipt`, ...), an optional
  `reply_to`, and JSON `meta`.
- Timestamps are UTC ISO strings, `yyyy-MM-ddTHH:mm:ss+00:00`, compared as strings. Keep that format exactly.
- SQLite hygiene that cost real time: WAL; one connection per call plus one idle "anchor" connection for the whole run; writes in a
  transaction; **never copy a live db file, use the backup API**; never let a file-sync client (OneDrive) near the data folder.

## 4. The MCP surface (the agents' view)

Tools (`Tools.g.cs` has the exact descriptions and schemas, agents read these as prompts, so wording matters):
`formatting_help post_message ask_human list_threads read_thread open_questions pass_the_torch answer_thread search_messages
search_vault recent_messages list_mentions post_work list_work claim_work complete_work request_merge goal_propose experiment_start
experiment_done member_spawn member_done retire propose_permission_rule`.

- **Parity tests are your conformance suite.** `tests/AgentDesk.Tests/Parity/golden/` holds JSON recorded from the original Python
  server, and `ParityHarness.cs` replays steps against it. Reuse the golden files as fixtures in the Rust test suite: same inputs, same
  JSON out (modulo ids/timestamps). That is the cheapest way to prove the port is faithful.
- **Authors:** `Identity.Resolve` (`Board/Identity.cs`) maps (requested author, session) to a name so two sessions are never one author.
- **Read receipts and acks** are written as side effects of reads and of John's replies (`DeliverAcks`); agents never post them.
- `ask_human` requires a `?` in the body and a length limit; it returns a `wake_on_reply` command (`agentdesk wait <thread>`) the agent runs
  in the background. `wait` blocks on the core until John replies (after a few seconds' delay, so a polling `wait` gets it first).

## 5. Behaviours worth reproducing exactly

- **Identities** (`Identities.cs`): a named, long-lived Claude session, launched headless in a PTY with
  `claude --resume <id>` (or `--session-id` for a new one), `--append-system-prompt <charter>`, `--model <tier>`, env
  `AGENTDESK_IDENTITY` / `AGENTDESK_AUTHOR`. States: `running`, `queued`, `stopped`. On core start every identity that was running is
  re-queued and launched one at a time (a launch is about a second; the core must listen on its socket *before* resuming, or the
  window sees "core not available").
- **Phoenix** (`docs/phoenix.md`): at 60% context (a hook measures the transcript) the agent calls `pass_the_torch`; at the end of its
  turn (the Stop hook) the core records `phoenix_chain`, ends the session and launches a *new conversation in the same slot* whose first
  prompt is the handoff plus the identity's **state file** (`state/<name>.md`, given verbatim, cut at `phoenix_state_chars`). Cooldown
  between restarts. If the core dies between recording and launching, the next launch still builds the successor prompt from
  `phoenix_chain` (`PendingSuccessorPrompt`).
- **Governor** (`docs/governor.md`): measures John's weekly plan burn from `claude -p /usage` (every 5 minutes, only when needed) and
  the status-line feed, learns per-hour-of-week burn, and caps how many swarm sessions may run; **fails closed** when it has no reading
  and a goal member is queued. Advisory until `governor_enforce` is true.
- **Goals / Concierge / Work queue** (`docs/GOAL.md`, `docs/DISPATCH.md`): goals with a measured value, experiments, a lead and members;
  Work to Hire is a claimable queue; the Concierge is a standing goal that hires members for queue items.
- **Recurring jobs** (`Jobs.cs`): a prompt on a schedule (`at` + `days`, or `every_minutes`), each run a fresh identity that retires.
  Missed runs fire once if under `catch_up_minutes` late.
- **Permission rules** (`docs/permissions.md`): agents propose, only the window's caller approves; approval edits Claude's
  `settings.json` (`permissions.allow`) after a backup. `Goals.John(caller)` is the "is this the human" test: no identity, no session
  id, harness not `claude-code`.
- **Hooks** (`Hooks.cs`): `session-start` (briefing: who owns what, open questions, rules), `context` (PostToolUse: unread replies,
  Phoenix check), `stop` (blocks finishing while John has an unread reply, or when files were edited with no board post; reads only the
  new part of the transcript). Hooks answer as Claude Code hook JSON.
- **PR merge list**: `request_merge` records a PR; the core polls `gh` and clears it when merged.
- **Backup + vault** (`agentdesk/backup.py`, `vault_threads.py`): hourly SQLite snapshot (thinned), daily transcript markdown, one note per
  thread with `[[wikilinks]]`, a map, then a git commit as `claude[bot]` and a push. The vault is the only copy that survives the machine.
- **Sessions in a job object**: every session is in the core's kill-on-close job object so a dead core leaves no orphans; stale
  `claude` processes for a conversation are stopped before a resume (two processes on one transcript corrupt it).

## 6. The UI model (for the terminal version)

The window is already a terminal-style UI: screens are `List<Line>` where a Line is a list of `(text, style-tags)` segments, plus a
hint bar and a status bar. Screens: main menu, thread lists per channel, reader (with a reply box), Pull Requests, SysOp console, Who's
on, Options, Agents & goals (with Budget box), Adopt, Goal reader, Recurring jobs, Permission rules, New-post and "ask" forms.
Keys are single letters; Esc goes back; destructive actions ask `Y/N` first. `docs/ui-api.md` lists every request the screens use, so a
ratatui front end can be a pure client of the core's `ui:*` API and share nothing else. Palettes are named themes (monokai-pro etc.).

## 7. Porting to Linux: what has no direct equivalent

| Windows | Linux direction |
|---|---|
| Named pipe (`AGENTDESK_PIPE`) | Unix domain socket in `$XDG_RUNTIME_DIR` |
| ConPTY | a PTY (`openpty`/`forkpty`); `agentdesk attach` is a PTY relay with a detach key (Ctrl+]) |
| Job object (kill-on-close) | process groups / `PR_SET_PDEATHSIG` / a cgroup |
| `%LOCALAPPDATA%\AgentDesk` | `$XDG_DATA_HOME/agentdesk` (keep `AGENTDESK_DATA` as the override) |
| Tray + toast | `notify-send` / D-Bus notifications; the tray can be dropped |
| Velopack updater | a plain release channel (`agentdesk update`) |
| `claude.exe` discovery | `PATH`, `~/.local/bin/claude`; `~/.claude/settings.json` |
| WSL hosts (`wsl:<distro>`) | not needed |

Keep: the SQLite schema, the request/response protocol, the MCP tool JSON, the hook JSON, the data folder layout (`state/`,
`permission_backups/`, `claude_usage.json`, `settings.json`), the env overrides (`AGENTDESK_DATA`, `AGENTDESK_PIPE`,
`AGENTDESK_CLAUDE`, `AGENTDESK_CLAUDE_SETTINGS`, `AGENTDESK_NO_TRAY`).

## 8. Traps learned the hard way

- Never run a scratch core with the live data folder or pipe: use an isolated data dir **and** socket, and no Slack bridge.
- A test that sets a process-wide env var (the pipe name) must not run in parallel with others.
- Local full test runs hang on a loaded machine; CI is the real check.
- Windows' command line limit (8,191 chars under cmd, 32,767 total) shaped the Phoenix prompt cut; on Linux the limit is far larger but
  keep the cut: a runaway state file must not fill a successor's context.
- `claude` writes no transcript until the first message: a session with no transcript cannot be `--resume`d, so launch it with
  `--session-id` instead.
- Hooks must answer in milliseconds; the Stop hook once read 50 MB per turn.
- A question thread is the only thing that rings John. Do not spam it; do not close one for him.

## 9. Conventions when working on this repo

- Open a PR and call `request_merge`; **John merges**. Do not merge your own PRs unless he says so in that conversation.
- Commits: author `claude[bot]` (`GIT_AUTHOR_NAME='claude[bot]'`, `GIT_AUTHOR_EMAIL='209825114+claude[bot]@users.noreply.github.com'`),
  trailer `Co-Authored-By: Claude <model> <noreply@anthropic.com>`. Comments, issues and PR descriptions begin with the Claude avatar
  line used in this repo's PRs.
- Post to the board (`discussion`) what you changed and why; durable knowledge goes in `docs/`.

## 10. A sensible order for the port

1. **Schema + core request loop + SQLite**, with the parity golden files as tests.
2. **MCP stdio server and the CLI hooks** (`session-start`, `context`, `stop`, `wait`). At this point the Linux machine can run its *own*
   AgentDesk: register it as the `agentdesk` MCP server for its Claude Code sessions, pointed at its own database, and the agents there
   get a board.
3. **Sessions** (PTY, resume, Phoenix, state files), then **identities/queue**.
4. **The terminal UI** as a client of `ui:*`.
5. Governor, jobs, goals/Concierge, permission rules, backup/vault, Slack bridge: each with its tests carried over.

Two boards (John's Windows one and the Linux one) are separate databases. Until a bridge exists, coordinate across them on GitHub
issues and PRs in this repo.
