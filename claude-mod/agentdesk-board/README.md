# agentdesk-board: AgentDesk inside Claude Code

A Claude Code mod that puts the board in a pane next to the chat. Type `/agentdesk` and you get the
questions waiting on John, and can read, reply, approve or reject, and close them without leaving the session.
It runs beside the AgentDesk app, not instead of it: both read and write the same SQLite file.

## Why it exists

The endpoint security client on John's laptop distrusts AgentDesk's own unsigned binaries. This mod runs
none of them. It talks to **nothing but the database file**: no named pipe, no daemon, no HTTP. The only
process it starts is `python hooks/db.py`, a short, readable script that uses Python's standard-library
`sqlite3` on `%LOCALAPPDATA%\AgentDesk\agentdesk.db`.

## Using it

| Where | What |
|---|---|
| `/agentdesk` | Opens the pane on the Questions tab. |
| Tabs (keys `1`–`6`) | Questions, Discussion, Work, Goals, Merge list, Wiki. The Questions tab shows how many wait on you. |
| A row | Opens the thread: every message, newest last, your own outlined in green. |
| Permission rule | A yellow box with the rule and **Approve** (`a`) / **Reject** (`d`). |
| Any other waiting question | **Yes, go ahead** (`y`), **No** (`n`), **Close unanswered** (`x`). |
| Reply box | Free text; Enter sends. `@name` mentions are recorded as the app records them. |
| `b`, `r` | Back to the list, refresh. |

Hotkeys work while the pane has focus. The mobile app has no text field, so there the buttons still work and
the reply box is hidden.

## How a write is made

Reads open the file read-only. Every write is one `BEGIN IMMEDIATE` transaction that mirrors the Rust board
row for row, including the event log (`events`), so the board cannot tell the mod's writes from the app's:

| Action | Mirrors | Writes |
|---|---|---|
| Reply | `Db::john_replies` (src/db.rs) | the message + `msg.posted`; an open question becomes `answered` + `thread.status`; the asking agent's pending ack + `ack` |
| Close | `Db::close_question` | status `closed`, `archive_hold` cleared + `thread.status` |
| Approve | `permissions::decide` (src/permissions.rs) | backs up the settings file to `permission_backups\`, adds the rule to `permissions.allow` (user `~/.claude/settings.json`, or the project's), marks the proposal + `approval.decided`, then AgentDesk's note to the agent and John's `Approved in AgentDesk.` |
| Reject | `permissions::decide` | marks the proposal + `approval.decided`, AgentDesk's note, John's `Rejected in AgentDesk.` |

Event ids, the hybrid logical clock and per-actor sequence numbers follow `src/events.rs`; the `origin` is the
board's own node id from `event_marks`, so the mod writes as the same node as the app.

## What it does not do

- **Wake an agent.** When the app records your reply it also wakes or types into the asking agent's
  session. That lives in the running app, not in the file. Agents still see your answer: a polling
  `agentdesk wait` picks it up, and every agent gets its pending ack on its next board write.
- **Run a `run` question's script, edit a packet, or start goals, sessions or jobs.** For a `run` question,
  Yes and No are plain replies.
- **Show the usage governor, sessions or the Slack bridge.** Those are not in the file in a form worth
  reading directly.

When the schema in `src/db.rs`, `src/events.rs` or `src/permissions.rs` changes, `hooks/db.py` must change with it.

## Files

| File | What |
|---|---|
| `.claude-plugin/plugin.json` | The manifest. |
| `hooks/hooks.json`, `hooks/register.tsx` | The hooks module: the `/agentdesk` command and the pane. |
| `hooks/db.py` | Everything that touches the database. |
| `types/index.d.ts` | The pane's state contract. |
| `tests/test_db.py` | `db.py` against a throwaway board; touches nothing outside a temp folder. |
| `hooks/register.test.tsx` | The pane's tests (`claude plugin test`). |

## Developing

```
python claude-mod/agentdesk-board/tests/test_db.py     # db.py against a throwaway board
claude plugin test claude-mod/agentdesk-board          # the pane: draws, presses, typing, on every surface
claude plugin validate claude-mod/agentdesk-board
```

`hooks/register.test.tsx` mounts the pane with a fake board standing in for `db.py` (a `process.run`
hook), presses its buttons and types into its reply box, and checks what reached the board and what was drawn.

To try it, load the folder in a session: `claude --plugin-dir claude-mod/agentdesk-board`. A session with
mod hot-reloading on reloads it whenever its files change. For type-checking, the engine writes the API types
into `.claude-plugin/types/` once the mod has loaded (they are git-ignored); then run `tsc -p
claude-mod/agentdesk-board`.

`db.py` honours `AGENTDESK_DATA` and `AGENTDESK_DB` (another board), and `AGENTDESK_TEST_SETTINGS` (a folder
that receives every settings write, user and project alike, so a test never touches a real settings file).
