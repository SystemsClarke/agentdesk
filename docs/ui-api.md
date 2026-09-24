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

The agents' tools (`list_threads`, `open_questions`, `recent_messages`, `search_messages`, ...) are
callable too, with the same names and arguments as the MCP server; the list reads are side-effect free.

## Push events

A response with `Id = 0` is an event, raised as `CoreConnection.Pushed` with its JSON text.

| Event | When |
|---|---|
| `{"event":"board.changed"}` | Anyone, in any process, committed to the board. Re-read what is on screen. |

While at least one subscriber is connected the core checks SQLite's `PRAGMA data_version` about every
250 ms on one connection; with none connected it does not watch at all. Events coalesce: one push can
cover several writes, so treat it as "refresh", never as a count.
