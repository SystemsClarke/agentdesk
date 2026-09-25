# UI API: what AgentDesk's window asks the core

The window talks to `AgentDesk.Core` over the same per-user pipe as the agents, through
`AgentDesk.Contracts.CoreConnection` (it starts the core if it isn't running):

```csharp
using var core = await CoreConnection.Connect(new Caller(null, null, Environment.CurrentDirectory, "ui", Environment.ProcessId));
core.Pushed += e => Dispatcher.BeginInvoke(Refresh);  // raised on a pool thread
await core.Call("ui:subscribe");
var doc = await core.Call("ui:thread", JsonSerializer.SerializeToElement(new { thread_id = 42 }));
```

Every reply is a JSON document. A failure is `{"error": "..."}` (bad or missing arguments, no such thread).

## Requests

| Tool | Args | Does | Returns |
|---|---|---|---|
| `ui:subscribe` | none | Marks this connection a subscriber until it closes. | `{"ok": true}` |
| `ui:thread` | `thread_id` | Reads a thread with no side effects: no read receipt, no session record. | `{"thread": {...}, "messages": [...]}`, the same document as the agents' `read_thread` |
| `ui:reply` | `thread_id`, `body` | Posts as John (`john`, kind `human`). An open question becomes `answered`; on a question an agent opened, the opener's ack is queued (delivered on its next board write). A blank body is an error. | `{"ok": true, "message_id": N}` |
| `ui:close` | `thread_id` | John closes a question without answering: status `closed`, `archive_hold` cleared so the sweep files it. Not a question, or already archived: nothing changes. | `{"ok": true, "closed": true\|false}` |
| `ui:post` | `channel`, `subject`, `body` | Starts a thread as John (`john`, kind `human`). A blank subject becomes `(no subject)`; a blank body or an unknown channel is an error. | `{"ok": true, "thread_id": N}` |
| `ui:unarchive` | `thread_id` | Brings an archived question back with the status it was settled with (`archived_from`, else `answered`) and sets `archive_hold` so the sweep leaves it. Posts nothing. Not archived: nothing changes. | `{"ok": true, "unarchived": true\|false}` |
| `ui:status` | none | The heartbeat files in the data folder, read as the Tk window read them. `slack` is `slack_bridge.state` as written (`ts`, `poll_s`, `last_relay`), or `null`; up means `ts` is under 90 s old. `worker` is `worker.state` plus `running` (its pid is alive), `held` (its `item`, else the newest claimed work item) and `events` (that item's newest 80 `work_events`, oldest first). `usage` is the Claude plan meter from `claude_usage.json` (usage.py): `lines` rotate on the main menu prompt, `summary` is SysOp's Claude plan row. `prs` is the merge list, `pull_requests` rows newest first (200 at most, settled ones included; the window filters). `crew` is Options' Agent sessions section (sessions.py, crew.py, providers.py): `live` counts the agent sessions running in any process (`live-sessions/*.json`, dead ones swept), `note` describes the backend in `settings.json`'s `provider` (plus `last run fell back to X`), `backends` is the providers.json chain ←/→ cycles through, and `roles` has one row per crew role (`running_since`, `provider`, `resumed`, `session_id`, `items`, `fresh_due`). `bridge` is the core's supervision of the Slack bridge (Host/Supervisor.cs): `enabled` (false when `settings.json` has `"slack_bridge": false`), `supervised` (false while a bridge the core did not start still has a fresh heartbeat), `pid`, `restarts`, `last_exit` and `last_exit_ts`. `governor` is the `ui:governor` document. `goals` is `ui:goal_list`'s rows. | `{"slack": {...}\|null, "worker": {...}, "usage": {"lines": [...], "summary": "..."}, "governor": {...}, "prs": [...], "crew": {...}, "bridge": {...}, "goals": [...]}` |
| `ui:governor` | none | The usage governor, advisory only (docs/governor.md): from the last 5 weeks of `usage_samples`, John's forecast burn until the weekly reset, `spendable = remaining - margin - baseline - k*sigma*sqrt(T)`, and the caps and model tiers that would spend it. Enforces nothing. With no samples: `samples: 0` and a `reason`. | `{"advisory": true, "samples", "baseline_hours", "session_hours", "sample_age_minutes", "used", "remaining", "reset_in_hours", "baseline", "sigma", "reserve", "k", "spendable", "allowed_rate", "session_rate", "projected_end_pct", "five_hour_pct", "caps": {"swarms", "members_per_swarm", "total_sessions", "running", "new_sessions"}, "models": {"step_down", "lead", "member"}, "reason", "summary"}` |
| `ui:fresh` | `name` | Queues a fresh start for that crew role (`handoffs.torch_due`): its next item begins a new session from its handoff note. | `{"ok": true}` |
| `ui:check_prs` | none | Runs a merge-list check now instead of at the next minute (prs.py). The core checks every open PR through `gh pr view` 5 s after it starts and every minute after; only GitHub saying merged or closed takes a row off, and posts the notice on its thread as `agentdesk`. A failed check leaves the row open with `last_error`. | `{"ok": true}` |
| `ui:worker` | none | Ctrl+W. A running crew (`worker.state`'s pid alive) is asked to stop: `worker.stop` is written, and the crew reads it between items, so the item it holds finishes. A stopped one is started as `python -m agentdesk.crew` from the Python repo, any leftover stop flag cleared first. A failure is an error naming it. | `{"ok": true, "stop_requested": true}` or `{"ok": true, "started": true\|false}` |
| `ui:wake` | `thread_id` | Ctrl+R (wake.py): resumes the asking agent's Claude Code session with John's latest reply, through `sessions.run` (the one engine: a session slot and the chosen backend). Only ever sent on John's keypress, never on a timer, and it posts nothing to the board: it records the carry in `deliveries` (`wake`: `resumed`, `stuck` while the session is still open, `failed` with no claude CLI). | `{"ok": true, "said": "woke builder: ..."}`, the line the window flashes |
| `ui:session_start` | `name`, `folder`, `command?` | Starts a headless session: `command` (default `claude`, found on PATH) in `folder` on a pseudoconsole with no window, 120x30 until someone attaches. `CLAUDE*`, `AGENTDESK_SESSION` and `AGENTDESK_AUTHOR` are dropped from its environment (the core may have inherited another session's) and `AGENTDESK_HEADLESS=<name>` is set. In memory: sessions end with the core. | `{"name": "...", "pid": N}` |
| `ui:session_list` | none | The running sessions. | `{"sessions": [{"name", "folder", "command", "pid", "started", "viewers"}]}` |
| `ui:session_stop` | `name` | Kills the session's process tree and waits for it to exit. | `{"stopped": "..."}` |
| `ui:attach` | `name`, `cols`, `rows` | Makes this connection a viewer until it closes: pushes the last 256 KB of output first, then everything new, then nudges the size one column and back so the program redraws. The size follows the latest attacher; several may attach. | `{"attached": "...", "replayed": true\|false}` |
| `ui:input` | `name`, `data` | Types `data` (text, sent as UTF-8) into the session. Send keys one request at a time: a connection's requests run concurrently. | `{"ok": true}` |
| `ui:resize` | `name`, `cols`, `rows` | Resizes the session's pseudoconsole. | `{"ok": true}` |
| `ui:identity_create` | `name`, `folder`, `charter?`, `host?`, `autostart?`, `model?` | Records an identity (board table `identities`): a session that outlives the core. `host` is `windows` (default) or `wsl:<distro>`; `model` is its tier, `haiku`, `sonnet` (default) or `opus`, passed to claude as `--model`; `charter` is appended to claude's system prompt. Starts nothing. | the row: `{"name", "folder", "charter", "host", "claude_session_id", "pid", "state", "autostart", "model", "created_ts", "updated_ts", "generation", "phoenix_msg"}` |
| `ui:identity_list` | none | Every identity; `state` is `running`, `queued` or `stopped`, and `generation` counts its Phoenix restarts from 1 (see below). | `{"identities": [row, ...]}` |
| `ui:identity_start` | `name` | Runs it as the headless session of the same name: `claude --resume <claude_session_id>` when that conversation has a transcript, else `claude --session-id <id>` (a new id, recorded), plus `--append-system-prompt` with a standard chain charter (you are one generation of a long-lived agent; at the 60% warning call pass_the_torch, then stop) followed by its own `charter`, with `AGENTDESK_IDENTITY` and `AGENTDESK_AUTHOR` set to its name so its board posts carry it. A `wsl:<distro>` host runs `wsl.exe -d <distro> --cd <folder> -- claude ...`, the two variables passed through `WSLENV`. At most `max_sessions` (settings.json, default 3) run at once: past that it is `queued`, and launched, oldest first, when one ends. When the core starts it queues again every identity that was running or queued, and every `autostart` one. | the row |
| `ui:identity_stop` | `name` | Stops it (`stopped`, and a queued one may take the slot). A name that is not an identity stops that plain session, as `ui:session_stop`. | the row |
| `ui:identity_forget` | `name` | Stops it and deletes the row. The Claude conversation itself is left alone. | `{"forgotten": "..."}` |
| `ui:adoptable` | none | Claude Code conversations under `~/.claude/projects/*/*.jsonl` (`AGENTDESK_CLAUDE_PROJECTS` overrides the root) written in the last 24 h that someone typed in, newest first. `first_message` has tags such as `<system-reminder>` removed and is cut to 80 characters. | `{"sessions": [{"session_id", "folder", "last_activity", "first_message"}]}` |
| `ui:adopt` | `session_id`, `name` | Creates an identity in the conversation's own folder (its transcript's `cwd`) with that `claude_session_id`, and starts it. A conversation still open in the Claude desktop app is then written by two processes; `note` says to close it there. | the row, plus `note` |
| `ui:log_tail` | `lines?` (default 100, at most 1000) | The last lines of `core.log`, read from its last 256 KB. | `{"lines": [...]}` |
| `ui:web_url` | none | The ops console's URL with its key (below), or `null` if it did not start. | `{"url": "http://127.0.0.1:<port>/?k=<key>"}` |
| `ui:update` | `apply?` | Checks GitHub Releases now and downloads a newer release (Velopack never offers an older one). With `apply` and an update ready, it replies, then restarts the core into it with `--background`; the window and relays reconnect by themselves. Every request is logged with its source (the pipe caller's pid and author, or `web`). A dev build (not installed) only reports that. `agentdesk update [--apply]` sends it; `--apply` then waits for the new core and prints its versions. Untested live: CI has no installed build. | `{"installed", "current", "latest", "pending", "downloaded", "restarting"}` |

The agents' tools (`list_threads`, `open_questions`, `recent_messages`, `search_messages`, ...) are
callable too, with the same names and arguments as the MCP server; the list reads are side-effect free.

`list_threads` rows already carry the LAST WORD column: `last_author` (the newest message that is not an ack or
read receipt) and `delivery`, `<state>|<ts>` for John's newest message on the thread, where state is `picked-up`
(the agent's ack posted), a `deliveries` state (`woke`, `resumed`, `injected`, `stuck`, `failed`, ...), `pending`
(ack queued), or empty.

The core refreshes the usage meter every 5 minutes (`claude -p /usage`, no model call), window or not, records each reading
in `usage_samples` for the governor, and pushes `board.changed` to subscribed windows when it lands.

Not in the core: notifier sinks (the Teams sink; SysOp shows "all delivering"), and the scan for unregistered PRs on
GitHub with their ladder triage line. Both were Python (notify.py/teams_sink.py, pr_scan.py) and were deleted with the
Tk app without a port; git history has them.

## Push events

| `ui:goal_create` | `name`, `objective`, `folder` | A draft goal (board table `goals`), its board thread (`goal: <name>`, discussion), and its lead identity `<name>-lead` (model opus, in `folder`), started with a first prompt to propose a hypothesis, a measure and a success line with `goal_propose`. | the goal as `ui:goal_status` |
| `ui:goal_propose` | `name`, `hypothesis`, `measure_cmd`, `success`, `samples?` | The agents' `goal_propose`: for the lead (caller's `AGENTDESK_IDENTITY`) or John, on a draft. `success` is `value < N`, `<=`, `>`, `>=`, `==`, or `pass`. Posts the proposal on the goal thread. | the goal row |
| `ui:goal_approve` | `name`, `max_members?`, `max_hours?`, `cadence_minutes?` | John only (a caller with no session id, no identity, and not `claude-code`: the window, the CLI in a plain terminal, Slack). A proposed draft (or a stopped or exhausted goal) becomes `running`; the budget defaults are 3 members, 24 h, a wake every 30 min. | the goal row |
| `ui:goal_stop` | `name` | John or the lead: `stopped`. Members are forgotten, the lead is stopped, and it is posted on the thread. | the goal row |
| `ui:goal_list` | none | Every goal, newest activity first. | `{"goals": [{"name", "state", "objective", "lead", "thread_id", "success", "experiments", "last_value", "members", "max_members"}]}` |
| `ui:goal_status` | `name` | The goal row plus `experiments` (every row), `history` (measured values, oldest first), `members` (`identity`, `task`, `created_ts`) and `summary`, the text agents are woken with. | `{...goal, "experiments": [...], "history": [...], "members": [...], "summary": "..."}` |

**Goals.** The goal tools agents call (`goal_propose`, `experiment_start`, `experiment_done`, `member_spawn`, `member_done`) are
answered by the core, which checks the caller's identity. `experiment_done` runs `measure_cmd` through `cmd /d /s /c` in the
goal's folder (10 min timeout), `samples` times, and records the median of the last number each run prints (a non-zero exit
is an error; for `pass`, exit 0 is 1 and anything else 0) with a verdict: `met`, `improved`, `no gain` or `error: ...`. It posts
the verdict on the goal thread and wakes the lead; `met` ends the goal as `succeeded`. Every 15 s the core ends goals whose
`max_hours` are spent (`exhausted`) and wakes leads whose cadence is due: the summary, as one line, is typed into the lead's
session followed by Enter, or, if the lead is not running, it is started with the summary as its first prompt (resuming its
conversation). `member_spawn` (lead only) creates and starts `<goal>-<member>` within `max_members` (past `max_sessions` it
queues); `member_done` posts the member's summary, forgets it and wakes the lead. A Phoenix successor of a lead or member gets
the goal's summary after its handoff.

A response with `Id = 0` is an event, raised as `CoreConnection.Pushed` with its JSON text.

| Event | When |
|---|---|
| `{"event":"board.changed"}` | Anyone, in any process, committed to the board, or the usage meter refreshed. Re-read what is on screen. |
| `{"event":"session.output","name":"...","data":"<base64>"}` | To viewers of that session (`ui:attach`): its raw VT output, in order. |
| `{"event":"session.exited","name":"..."}` | To viewers of that session: its process ended. |
| `{"event":"session.restarted","name":"..."}` | To viewers of that session: it ended for a Phoenix restart, and its successor takes the same name a moment later. `agentdesk attach` reattaches. |

**Phoenix.** When an identity's own session (the caller's `AGENTDESK_IDENTITY` and claude session id match the row) calls
`pass_the_torch`, the row's `phoenix_msg` records the handoff message. On that session's next Stop hook that does not block,
the hook answers at once and then the core, asynchronously: writes `phoenix_chain` (identity, generation, old claude session id,
handoff message id, ts), ends the session, and launches the successor in the same slot (it never queues) with a new
`--session-id` and the first prompt "You are <name>, generation n+1. Your previous generation handed off with:" and the
handoff text. It posts "generation n+1 started from handoff #m" on the handoff's bio thread. At most one restart per
identity per 2 minutes; a handoff inside that window just ends the turn.

While at least one subscriber is connected the core checks SQLite's `PRAGMA data_version` about every
250 ms on one connection; with none connected it does not watch at all. Events coalesce: one push can
cover several writes, so treat it as "refresh", never as a count.

## The ops console

The core also serves one web page, for the same work from a browser (or a narrow window): core version, pid, uptime and
update (Check for updates, Restart to update); agents with Start, Stop, Forget and New agent; sessions; SlackNet and
the worker, with its start/stop toggle (`ui:worker`); the usage meter; the merge list; open questions; and the core log.
The tray's **Open ops console** opens it.

![The ops console, sample data](ops-console.png)

- It listens on `http://127.0.0.1:<port>` only: the port in `web.port` in the data folder while it is free, else any
  free one. There is no other listener.
- Every request needs the key in `web.key` (32 random bytes, made once, so a bookmark keeps working). `/?k=<key>` sets
  an HttpOnly SameSite=Strict cookie and redirects the key out of the address bar; anything else without the cookie is 401.
- A Host header other than `127.0.0.1:<port>`, or a foreign `Origin`, is 400 (DNS rebinding, cross-site posts). No CORS.
- `POST /api/<op>` with a JSON body runs `core`, `status`, `open_questions`, `session_list`, `log_tail`, `worker`,
  `update`, and `identity_list|create|start|stop|forget`, through the same objects as the requests above.

## The window's Agents and Adopt screens

**A** on the main menu opens Agents (`ui:identity_list`): **Enter** opens a console running `agentdesk attach <name>` (from
beside the window, else the install folder), **S** starts or stops (`ui:identity_start|stop`), **F** forgets after a Y/N
(`ui:identity_forget`), **N** asks for a name, a folder and an optional charter (`ui:identity_create`). **A** there opens
Adopt (`ui:adoptable`); **Enter** asks for a name and sends `ui:adopt`, and Agents shows the returned `note`. Options shows
the ops console's address without its key (`ui:web_url`); **Enter** on it, or **C**, opens it.

![Agents, sample data](ui-agents.png)
![Adopt, sample data](ui-adopt.png)

A core with `AGENTDESK_DATA` set, or `AGENTDESK_NO_TRAY=1`, shows no tray icon and no toasts, so temp and test cores stay
off the taskbar.
