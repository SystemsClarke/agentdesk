# AgentDesk: the design goal

Hand this file to an agent as its goal. It says where AgentDesk is going, the rules for getting there,
and how to tell when each milestone is done. Status lives on board thread #340; this file is the target.

## The one-sentence goal

John starts and steers all AI work from AgentDesk (the window, the local ops console, or Slack), never
the Claude desktop app. Every piece of work runs as a **swarm**: a lead that splits the problem into
small, short-lived agents, paced to the weekly usage plan and kept alive indefinitely by Phoenix.

## Principles

- **One system.** Every piece of AI work (a goal, a Work to Hire item, a loop) runs as a swarm under
  one engine. Nothing does the same job a second way, and the old way is deleted in the change that
  replaces it (see `AgentDesk moves forward with no fallbacks` in the vault).
- **Deterministic plumbing, AI only for the work.** Scheduling, restarts, throttling, Slack channels,
  measures and budgets are plain code. Agents do the thinking and nothing else.
- **Long-lived identities, short-lived sessions.** Agents hand off at 60% context and the core starts
  the next generation. A goal outlives any session; no session runs long.
- **Testable goals.** A goal is a hypothesis with a measure the core runs itself. "Done" is a number
  crossing a line, not an agent's opinion.
- **Pace to the plan.** Spend is governed against the weekly usage limit: cautious when the reset is
  far off, flat out as it nears, finishing close to 100% without going over.
- **Cheapest capable model.** Haiku for mechanical work, Sonnet by default, Opus only for leads and
  hard reasoning.
- **John talks to goals, not individuals.** Each swarm has one persona. Member chatter stays on the
  board.

## The system

| Part | What it is |
|---|---|
| **Core** (`AgentDesk.Core.exe`, Native AOT) | Owns the board, hosts every Claude Code session headless (ConPTY), runs Phoenix, goals and the governor, serves the named pipe and the local ops console, tray and toasts, supervises the Slack bridge, and updates itself from GitHub Releases. |
| **Identity** | A named, long-lived agent: folder, charter, host (Windows or `wsl:<distro>`), model tier, and a chain of Phoenix generations. `agentdesk attach <name>` always reaches the current one. |
| **Phoenix** | On `pass_the_torch` the core ends that session after its turn and starts the next generation in the same slot, with the handoff as its first prompt. |
| **Goal / swarm** | A hypothesis, a measure, a success line, a budget and an experiment log. A lead identity dispatches member identities, and a loop runs until success, budget exhaustion or John stops it. State lives in the DB; findings go to the board and the vault. |
| **Swarm slots** | 10 reusable Slack channels, each with a persona (`chat:write.customize`). `swarm new <name> <objective>`, `swarm reset <slot>` (a new identity with a fresh channel; the old one is archived), `swarm end <slot>`, `swarms`. |
| **Concierge** | The standing swarm lead that replaces the crew/worker. It tends the Work to Hire board: triages items, turns each into a small goal, and starts swarms for them under the governor. Off by default; **Ctrl+W** (and Slack `concierge on\|off`) starts and stops it. |
| **Usage governor** | Records `/usage` samples, forecasts John's own burn per hour of week, and sets swarm caps from `spendable = remaining - forecast - k*sigma*sqrt(time to reset)`. The shrinking reserve is the ramp-up. It also picks model tiers and guards the 5-hour window. |
| **Window** (WPF) | The BBS board, plus Adopt (take over a live Claude session), Agents, Goals and the budget chart. |
| **Ops console** | A local-only web page inside the core (127.0.0.1, keyed): status, agents, the worker/Concierge, updates, the log. |
| **Slack** | Phone control through area bots (agents, concierge, ops, board) and the swarm-slot personas. John only. |

## Milestones and how each is checked

Each milestone lands as PRs to `main`, merged when CI is green. Every merge publishes a release, and
John applies it with the tray's Restart to update.

1. **Phoenix restarts.** A pass_the_torch followed by Stop starts generation n+1 with the handoff as
   its first prompt, and attached viewers follow. Rate-limited, and only the identity's own session can
   trigger it. *Check:* stand-in tests, plus one real claude identity across a generation.
2. **Ops console and self-update.** `agentdesk update --apply` makes the core download, apply and
   restart itself; relays and the window reconnect. *Check:* a live update from release N to N+1
   without a Claude desktop session doing the install.
3. **Governor, advisory.** Samples are stored, the forecast and caps are computed and shown, and
   model tiers are declared per role. *Check:* a backtest on recorded samples lands near 100% with no
   overshoot; unit tests on the formula.
4. **Core supervises the Slack bridge.** It is started with the core and restarted if it dies.
   *Check:* kill the bridge, and it is back within 30 s.
5. **Goals and swarm slots.** A hypothesis, a measure run by the core, an experiment log, and loops
   that survive Phoenix; 10 slots with personas; `swarm new/reset/end/list`. *Check:* a toy goal with a
   scripted measure runs at least 3 loop iterations across at least one Phoenix generation and stops
   at its success line.
6. **Concierge replaces the worker.** Ctrl+W toggles the Concierge; it turns Work to Hire items into
   swarms. `agentdesk/crew.py`, `roles.py`, the worker heartbeat and anything else only they used are
   deleted in the same change. *Check:* an item posted to Work to Hire becomes a completed swarm with
   a report on its thread, with no Python worker running.
7. **Governor enforcing.** Caps apply to goals, the Concierge and Phoenix restarts. *Check:* a few
   days advisory, then enforcing; week-end usage lands at 90-100% with the 5-hour window never over
   90%.
8. **Window screens.** Adopt first, then Agents, Goals and the budget chart. *Check:* John adopts a
   live desktop session from the window and closes the desktop app.
9. **Hardening.** Per-viewer backpressure; `agentdesk` on PATH; a toast when an agent follows up on a
   question John answered; build-speed migrated to a goal.

## Rules for agents working on this

- **Never install or launch AgentDesk from a Claude desktop session.** Its sandbox redirects AppData
  writes. Test against temp cores with both `AGENTDESK_PIPE` and `AGENTDESK_DATA` set to temp values.
- **Never kill `agentdesk.exe` relays**; one of them is your own MCP connection. Kill only what you
  started.
- **Keep the code small and AOT-clean** (0 warnings). Keep `./build.ps1 -Test` green. Every new
  request gets a test.
- **Commit as `claude[bot]`** with the automation signing key. Open PRs with the Claude marker line.
  Merge your own AgentDesk PRs only when CI is green.
- **If the permission classifier blocks an action**, stop and report it; don't route around it.
- **Report milestones on #340** and durable lessons in the vault.
