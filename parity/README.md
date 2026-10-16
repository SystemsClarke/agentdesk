# Feature parity with the .NET AgentDesk, as a test suite

"100% parity" is only a goal if it can be measured. This directory measures it: one registered **feature** per
behaviour of the original, each with a check that drives the real `agentdesk` binary and passes, fails or reports
`missing`. The score is `passed / (total - waived)`.

```
cargo build
python3 parity/run.py                     # report; exit 1 only if something that used to pass now fails
python3 parity/run.py --strict            # exit 1 unless every feature passes: the 100% bar
python3 parity/run.py -k governor -v      # one slice, with the reason for every non-pass
python3 parity/run.py --update-baseline   # record today's passes as the floor (parity/baseline.json)
python3 parity/coverage.py                # is the manifest itself complete? (see below)
```

Python 3 standard library only; it runs on Linux, macOS and Windows. Each check gets a fresh board in a temp folder
(`AGENTDESK_DATA`, `HOME` and the vault are all isolated) and, where the original shelled out to `claude` or `gh`, a
stand-in executable on `PATH`, so nothing touches real data or the network.

## How a feature is tested

| Layer | Driven through |
|---|---|
| The MCP contract: 24 tools, instructions, JSON-RPC | `agentdesk mcp` on stdio, compared with `original/tools.json` (extracted from `Tools.g.cs`) |
| Recorded behaviour of the tools | `cargo test parity::` replays the original's seed.db + golden JSON (`testdata/golden/`) |
| Hooks and `wait` | `agentdesk hook <event>`, `agentdesk wait` |
| The window-to-core protocol (`docs/ui-api.md`, 50 requests) | `agentdesk ui <request> [json]`: every `ui:*` request is one stable command |
| Screens and keys | `agentdesk ui-snapshot --keys "q enter ctrl+s" --json`: renders the window headlessly after the keys |
| Timers (jobs, toasts, merge list, sweep) | `agentdesk daemon` plus the board file and `AGENTDESK_TOAST_LOG` |
| Schema | `original/schema.json` (extracted from `BoardStore.cs` + `Governor.cs`) against the fresh board |
| Sessions, goals, governor | `agentdesk daemon` with a stand-in `claude`, then the `ui:*` requests |

`ui-snapshot` keys are space-separated: letters, `enter esc tab up down left right pgup pgdn home end backspace`,
`ctrl+x alt+x`, and `t:text~with~spaces` to type.

Slack has two fakes: `fakes/slack_bolt` (the Python bridge runs unchanged on it) and `fakes/slack_web.py`, a Web API on
HTTP plus a Socket Mode websocket on 127.0.0.1 that the native bridge (`agentdesk slack-bridge`, or the daemon with
`"slack_native": true`) is pointed at with `SLACK_API_URL`. The Slack checks in `checks/surfaces.py` run the same body
against either one (`PyBridge` / `NativeBridge`), registered as `slack.<feature>` and `slack.<feature>-native`.

## Statuses

- **pass**: the check ran and the behaviour matches.
- **fail**: the feature exists and does something different from the original. Fix it.
- **missing**: the feature does not exist yet (or only partly: the reason says what is absent). The check is the
  acceptance test: when it passes, the feature is done.
- **waived**: deliberately not reproduced, with the reason in `Waived(...)`. Waivers are visible in the report and
  should stay rare; the list is in "Deliberate non-ports" below.

## Deliberate non-ports

Things the .NET window did that the terminal version does not, on purpose. Each is registered as a waived feature, so the
report shows it instead of the score hiding it.

| Feature | Why it is not ported |
|---|---|
| Font size option and Ctrl +/- zoom (`options.font-size`) | A terminal program draws in the terminal's own font. Size and zoom are the terminal's setting (Ctrl +/- in Windows Terminal, iTerm, GNOME Terminal). |
| Open a window at logon (`startup.open-window`) | A TUI has no window to open. `agentdesk install` starts the core and the tray at login; the tray's Open entry (and a double click) launches a terminal running `agentdesk tui` (`tray.terminal-override`, `AGENTDESK_TERMINAL` overrides it). |
| Embedding search in the vault (`vault.embedding-search`) | Switched off in the original too (Ollama is not wanted); search is lexical. |

Not waived, but not done yet: `agentdesk install` does not register a Start Menu shortcut that opens Windows Terminal (or conhost)
on `agentdesk tui`. A `.lnk` is a binary file that only the shell (COM / `WScript.Shell`) writes, which the install `Host` fake
cannot model faithfully, and a text file in the Start Menu folder does not appear in Start. Until it is built, the tray's Open entry
is the way in on Windows.

## The manifest cannot silently leave things out

`coverage.py` reads the original's own documents (`original/`: `ui-api.md`, `tools.json`, `cli-usage.txt`,
`hints.json` from the window's footers, the design docs) and fails if a request, tool, command or screen they
define has no feature here. Add a feature when the original has a behaviour nobody listed.

## The ratchet

`baseline.json` is the set of features that pass. CI fails if any of them stop passing, so parity only moves
forward. When you make a missing feature pass, run `--update-baseline` in the same PR. When all features pass and
nothing is waived without reason, switch CI to `--strict`.

## Adding a feature

```python
@feature("ui.thing", "ui-api", "ui:thing does X", "docs/ui-api.md")
def _thing(b):                       # b is a fresh Board
    d = b.need("ui:thing", {...})    # raises Missing if the request is not implemented
    assert d["x"] == 1               # a wrong answer fails the check
```

Put it in `checks/<area>.py`; `run.py` finds it. `lib.py` has the helpers: `b.tool()` (an MCP call as a Claude Code
session), `b.ui()`, `b.snap()`, `b.sql()`, `b.stub("claude", python_body)`, `b.spawn("daemon")`.

## Where the oracle comes from

There is no .NET build on Linux, so the oracle is what the original left in writing and in recordings: its
documents, its tool schemas, its schema, and the golden outputs its own parity tests used. Where a behaviour is only in
the original's C# (not in a document or a golden), the feature's `source` names the file to read, and the check was
written from that source. A check that turns out to disagree with the real .NET app on a live board is a bug in the
check: fix it here, with the evidence.
