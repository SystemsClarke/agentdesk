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
| `reload_ms` | `App::reload` (the exact reload after a write), warm and the first time |
| `draw_ms` | `App::draw` per screen into a ratatui `TestBackend` (120x40); opening the longest thread cold, and drawing it warm |
| `render_ms` | the Markdown / mermaid / chart renderer on the longest thread, whole and for its longest message |
| `key_to_frame_ms` | a key and the frame it causes: moving in a list, typing and scrolling in the reader, switching screens, the theme key; and `reload` against `reload_nav` (switching screen reuses the slow-moving budget) |
| `terminal_frame` | what one frame sends the console: bytes and `write` calls, for a full screen, an unchanged screen, one changed row and a theme change; the old path (crossterm over `stdout`'s 1 KiB `LineWriter`) against the window's (one write, synchronized-update markers) |
| `event_loop` | `PRAGMA data_version` cost, what an idle loop does (it must request no frames), and the stall on the window's thread while the board changes ten times a second |

To read the plan of the list query, and what its covering index buys: `python scripts/ui_explain.py <copy of the board>`.

## What it found (a board of 765 threads, 5,834 messages, 4,519 usage samples; Windows 11, release build)

The lag was not the terminal and not SQLite. It was a process started inside the frame.

| | before | after |
|---|---|---|
| a frame, any screen (`draw_ms`) | **281 to 668 ms** | **0.2 to 1.4 ms** |
| `bridge::running()` (called by the top line on every frame, twice on SysOp) | 298 to 349 ms: it starts `tasklist` to see whether the heartbeat's pid is alive | 0.01 ms (`OpenProcess`), and once per refresh instead of once per frame |
| the real window under ConPTY (`tests_pty`, release builds): first frame | 329 to 340 ms | 62 to 69 ms |
| ... a key to bytes on the console | 324 to 528 ms | 5 to 8 ms |
| ... thirteen keys typed at once, until the last frame is out | **8.1 s** (one event per 600 ms frame) | **49 ms** |
| arrow key in a list, to the frame | 396 ms | 0.9 ms |
| typing a letter in the reader | 524 ms | 0.5 ms |
| scrolling a page in the reader | 1,068 ms | 1.1 ms |
| the longest thread (841 messages, 761 KB) drawn | 795 ms, every frame | 0.5 ms (rendered once, then cached) |
| the Markdown of that thread rendered cold | 214 to 290 ms | 39 to 50 ms (`inline` copied the rest of the line at every character: quadratic) |
| frames while nothing changes | 4 a second (the loop drew on every 250 ms poll), each 300 ms or more | none |
| bytes to the console for an idle screen | one cursor update, four times a second | 0 |
| `write` calls for a full frame | 7 (`stdout` is a 1 KiB `LineWriter`) | 1 |
| a board change (27 agents write all the time) | `reload()` on the window's thread: 60 to 100 ms each time | a worker thread loads it; the window's thread spends under 5 ms taking it in, at most four times a second |
| `list_threads` (the biggest query) | 6.6 ms for the discussion list | 2.4 ms (`idx_messages_thread_kind`) |

The bytes a frame sends were never the problem (7 KB for a full screen, 300 bytes for a changed row: ratatui already sends only
changed cells). A process spawn costs a lot on this machine whatever the process is (`cmd /c exit` 132 ms, `whoami` 94 ms,
`tasklist` 356 ms: an endpoint-security agent scans every new process), which is why a spawn per frame was so expensive.
What the fix does not touch: the daemon (`bridge::heartbeat()` calls the same `tasklist` check each time it looks at the bridge).

## The tests that keep it fast

`cargo test` (`src/tui/tests_perf.rs`): a generated board of 5,000 threads reloads and draws inside a generous budget; a frame needs
no process; an unchanged screen costs no frame (events that change nothing, a quiet clock, a flash message that has not ended);
a refresh asked for before a write is dropped; the worker's changes reach the screen without the window's thread waiting;
the reader renders a long thread once. `cargo test -- --ignored tui::tests_pty` runs the real window under a real pseudo-console
(ConPTY on Windows): first frame, zero bytes while idle, a key reaching the console, and quitting. Set `AGENTDESK_TEST_EXE` to
measure another build with the same script.

`python parity/run.py --no-ratchet -k perf -v` runs `ui-bench --quick` through the binary and opens a long thread.
