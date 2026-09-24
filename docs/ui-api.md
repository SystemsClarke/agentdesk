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
| `ui:status` | none | The heartbeat files in the data folder, read as the Tk window read them. `slack` is `slack_bridge.state` as written (`ts`, `poll_s`, `last_relay`), or `null`; up means `ts` is under 90 s old. `worker` is `worker.state` plus `running` (its pid is alive), `held` (its `item`, else the newest claimed work item) and `events` (that item's newest 80 `work_events`, oldest first). `usage` is the Claude plan meter from `claude_usage.json` (usage.py): `lines` rotate on the main menu prompt, `summary` is SysOp's Claude plan row. `prs` is the merge list, `pull_requests` rows newest first (200 at most, settled ones included; the window filters). | `{"slack": {...}\|null, "worker": {...}, "usage": {"lines": [...], "summary": "..."}, "prs": [...]}` |
| `ui:check_prs` | none | Runs a merge-list check now instead of at the next minute (prs.py). The core checks every open PR through `gh pr view` 5 s after it starts and every minute after; only GitHub saying merged or closed takes a row off, and posts the notice on its thread as `agentdesk`. A failed check leaves the row open with `last_error`. | `{"ok": true}` |

The agents' tools (`list_threads`, `open_questions`, `recent_messages`, `search_messages`, ...) are
callable too, with the same names and arguments as the MCP server; the list reads are side-effect free.

`list_threads` rows already carry the LAST WORD column: `last_author` (the newest message that is not an ack or
read receipt) and `delivery`, `<state>|<ts>` for John's newest message on the thread, where state is `picked-up`
(the agent's ack posted), a `deliveries` state (`woke`, `resumed`, `injected`, `stuck`, `failed`, ...), `pending`
(ack queued), or empty.

While a window is subscribed, the core refreshes the usage meter every 5 minutes (`claude -p /usage`, no model call)
and pushes `board.changed` when it lands, as the Tk app did while it was open.

Not in the core yet, so the window still shows placeholders for them: the crew roster and Options' backend note,
disabled notifier sinks, the worker start/stop and Wake actions, and pr_scan.py (finding unregistered PRs on GitHub
and their ladder triage line).

## Push events

A response with `Id = 0` is an event, raised as `CoreConnection.Pushed` with its JSON text.

| Event | When |
|---|---|
| `{"event":"board.changed"}` | Anyone, in any process, committed to the board, or the usage meter refreshed. Re-read what is on screen. |

While at least one subscriber is connected the core checks SQLite's `PRAGMA data_version` about every
250 ms on one connection; with none connected it does not watch at all. Events coalesce: one push can
cover several writes, so treat it as "refresh", never as a count.
