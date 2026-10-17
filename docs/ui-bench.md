# `agentdesk ui-bench`: how fast is the window?

A hidden command (it is not in `--help`) that times the terminal window on a board and prints JSON. It exists because the window
was very laggy on Windows and nobody could say why; it is how the cause was found, and how the next regression is caught.

## Run it, on a COPY of a board

It opens threads, saves drafts and settings, and posts a few messages to the board it is pointed at. It refuses to run
unless `AGENTDESK_DATA` names a folder of its own. Never point it at the live board: copy it first, read-only.

```python
# copy_board.py: a read-only snapshot of the live board into a scratch folder (SQLite's backup API; the source is opened mode=ro)
import os, sqlite3, sys
src = os.path.join(os.environ["LOCALAPPDATA"], "AgentDesk", "agentdesk.db")   # ~/.local/share/AgentDesk/agentdesk.db elsewhere
os.makedirs(sys.argv[1], exist_ok=True)
s = sqlite3.connect("file:" + src.replace("\\", "/") + "?mode=ro", uri=True)
d = sqlite3.connect(os.path.join(sys.argv[1], "agentdesk.db"))
s.backup(d)
```

```powershell
python copy_board.py $env:TEMP\board-copy
$env:AGENTDESK_DATA = "$env:TEMP\board-copy"; $env:USERPROFILE = "$env:TEMP\board-home"; $env:HOME = $env:USERPROFILE
agentdesk ui-bench                 # about 25 s; add --quick for 5 s, --runs N for the repeats (default 15, the median is reported)
```

Use a release build (`cargo build --release`): a debug build is several times slower and says nothing about the window.

## What it prints (milliseconds unless it says otherwise)

| Section | What it measures |
|---|---|
| `board` | rows in the copy, so a number can be read against its size |
| `bridge::running() ...` / `tui::live::slack_up ...` | the Slack-bridge check the window used to make on every frame (it started `tasklist`), and what replaced it |
| `reload_ms` | `App::reload` (the exact reload after a write), and every step of `State::load` one by one, slowest first |
| `draw_ms` | `App::draw` per screen into a ratatui `TestBackend` (120x40); opening the longest thread cold, and drawing it warm |
| `render_ms` | the Markdown / mermaid / chart renderer on the longest thread, whole and for its longest message |
| `key_to_frame_ms` | a key and the frame it causes: moving in a list, typing and scrolling in the reader, switching screens, the theme key; and `reload` against `reload_nav` (switching screen reuses the slow-moving budget) |
| `terminal_frame` | what one frame sends the console: bytes and `write` calls, for a full screen, an unchanged screen, one changed row and a theme change; the old path (crossterm over `stdout`'s 1 KiB `LineWriter`) against the window's (one write, synchronized-update markers) |
| `event_loop` | `PRAGMA data_version` cost, what an idle loop does (it must request no frames), and the stall on the window's thread while the board changes ten times a second |

`State::load`'s steps are named in the code (`step("governor::report", ...)` in `src/tui/state.rs`); a step you add shows up here by
wrapping it the same way.

To read the plan of the list query, and what its covering index buys: `python scripts/ui_explain.py <copy of the board>`.

## What it found

The lag was a process started inside the frame (`bridge::running()` ran `tasklist` on every frame: about 300 ms each, an endpoint-security
agent scans every new process). Fixed: a frame is now 0.2 to 1.4 ms on any screen, an idle screen draws nothing, a full frame is one
`write`, and board changes load on a worker thread. The daemon's `bridge::heartbeat()` still makes the same `tasklist` check.

## The tests that keep it fast

`cargo test` (`src/tui/tests_perf.rs`): a generated board of 5,000 threads reloads and draws inside a generous budget; a frame needs
no process; an unchanged screen costs no frame (events that change nothing, a quiet clock, a flash message that has not ended);
a refresh asked for before a write is dropped; the worker's changes reach the screen without the window's thread waiting;
the reader renders a long thread once. `cargo test -- --ignored tui::tests_pty` runs the real window under a real pseudo-console
(ConPTY on Windows): first frame, zero bytes while idle, a key reaching the console, and quitting. Set `AGENTDESK_TEST_EXE` to
measure another build with the same script.

`python parity/run.py --no-ratchet -k perf -v` runs `ui-bench --quick` through the binary and opens a long thread.
