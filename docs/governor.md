# The usage governor

Milestones 3 and 7 of `docs/GOAL.md`. The governor forecasts and recommends (milestone 3), and it can enforce what it recommends
(milestone 7, [Enforcement](#enforcement)). Enforcement ships **off**: until John turns it on, it only logs what it would have done.

## Samples

Every 5 minutes the core runs `claude -p /usage` (no model call) into `claude_usage.json`, and records the reading in the
board's `usage_samples` table: `ts` (the feed's `captured_ts`, unique), `weekly_pct`, `weekly_reset_ts`, `five_hour_pct`,
`five_hour_reset_ts`, and `swarm_sessions`, the identities with `state='running'` at that moment. `haiku_sessions`,
`sonnet_sessions` and `opus_sessions` split that count by the tier each session was actually launched at (`identities.running_model`,
which differs from the stored `model` when the governor stepped it down), so per-tier burn can be measured later. Readings the
status-line feeder writes are recorded too.

## Forecast

Consecutive samples in the same week become hourly rates (weekly-% per hour; an hour needs 30 minutes of coverage).

- **John's baseline**: hours with no identity session running. An EWMA per hour of week (168 buckets, UTC, alpha 0.5, so
  about two weeks of memory) and a global EWMA with its variance (alpha 0.02). A bucket with under 2 hours falls back to the
  global rate; under 6 baseline hours in all, to `governor_default_rate`.
- **sigma**: the square root of the global variance. It includes the daily pattern, so it errs high.
- **Per-session burn**: hours with sessions, as `(rate - baseline bucket) / sessions`, EWMA alpha 0.2. Under 3 such hours:
  `governor_default_session_rate`.
- **Baseline starvation.** Once swarms run constantly there are few session-free hours, and the buckets and the global rate go
  unfed. So every hour (with sessions or not, oldest first) also feeds a long-memory EWMA of John's estimated rate,
  `total rate - sessions x per-session burn`, with a 4-week half-life counted in observed hours (alpha = 1 - 0.5^(1/672)). Only
  when there are under 6 session-free hours does it stand in for the global rate (and its variance for sigma); `baseline_source`
  in `ui:governor` says which is in use (`measured`, `estimated` or `default`). The catch is that `(rate - baseline) / sessions`
  needs a baseline, so with no session-free hours it would only echo the default back. Then the per-session burn comes from a
  regression instead: rate on session count over the same long memory, whose slope is the per-session burn and whose intercept
  is John. That works whenever the swarm changes size (a variance of at least 0.25 in the hourly session count). A swarm that
  never changes size cannot be told apart from John, and the estimate then rests on the default rates. It is a 4-week memory
  rather than the buckets' 2 weeks because the estimate is noisier (it inherits the per-session error) and because
  session-free hours become rare, so the few that exist should be remembered.

```
spendable    = max(0, remaining - margin - E[baseline until reset] - k * sigma * sqrt(T hours))
allowed rate = spendable / T
sessions     = min(governor_max_sessions, floor(allowed rate / per-session burn))
```

The reserve `k*sigma*sqrt(T)` shrinks to nothing at the reset, so the governor is cautious early and flat out at the end.
`margin` covers `/usage` reporting whole percentages and the swarm's own noise.

Caps: a swarm is a lead plus `min(governor_max_members, sessions - 1)` members; swarms = sessions / (1 + members), at most
`governor_max_swarms`. While `five_hour_pct >= 90` (and its window has not reset) it recommends no swarm sessions at all, so
enforcing, it sheds them: holding the running ones would take the window past 90%, which the milestone 7 check forbids. When fewer
than one session is affordable, or the 5-hour window is at 75% or more, it recommends stepping down a tier: members on haiku,
leads on sonnet (otherwise members sonnet, leads opus). Each identity has a `model` (default `sonnet`), passed as `--model`.

## Settings (`settings.json` in the data folder)

| key | default |
|---|---|
| `governor_k` | 2 |
| `governor_margin` | 2 (weekly %) |
| `governor_default_rate` | 0.3 (%/h, John, until measured) |
| `governor_default_sigma` | 0.5 (%/h) |
| `governor_default_session_rate` | 3 (%/session-hour, until measured) |
| `governor_max_sessions` / `_members` / `_swarms` | 12 / 4 / 10 |
| `governor_enforce` | false (advisory). `ui:governor_enforce {on}` sets it |

## Enforcement

Every session start goes through `Identities.Drain`: `identity_start`, a goal's `member_spawn`, the Concierge's dispatch (a
`member_spawn` with `work_id`), a lead woken while stopped (goal wakes and Ctrl+R), and the core resuming identities at start.
The governor applies to **swarm identities**, a goal's lead (`goals.lead`) or member (`goal_members`); John's own identities
(adopted, or made with `agent new`) are never gated, re-tiered or shed, though they count as running.

- **Gate.** A swarm identity starts only while `running < total_sessions` (all running identities, John's included). Otherwise
  it stays `queued` rather than failing: `identity_start` and `member_spawn` return the queued row with a `governor` note, and a
  wake says why. Every 15 s the core's governor tick (`Identities.Tick`) drains again, so a held start launches, with the prompt
  it was given, as soon as the governor allows. `max_sessions` still applies on top.
- **Phoenix is never blocked.** A `pass_the_torch` restart replaces a session in the same slot (`Identities.Phoenix` launches
  directly, not through `Drain`), so it is never a new session, never counted as one, and runs even when the governor would
  allow nothing, including fail closed. It does take the step-down tier.
- **Shedding.** When `total_sessions` drops below the number running (the 5-hour guard at 90%, or the reserve outgrowing what is
  left), the tick stops swarm sessions until it is back within the cap: members before leads, the Concierge's members first,
  then the longest idle first (idle means the last board write, `presence.seen_ts`, else the launch). A shed identity goes back
  to `queued`, not `stopped`, so it resumes its conversation (`--resume`) when the governor allows. John's own identities are
  never stopped; if they alone are over the cap, the core logs a warning (once per change). A session mid-Phoenix is left alone.
- **Tiers.** While the governor steps down, a swarm session launches at the recommended tier (members haiku, leads sonnet)
  when that is cheaper than its stored `model`, never dearer. The stored column is unchanged; `running_model` records what ran.
- **Fail closed.** With no sample, a latest sample over 30 minutes old, or two `claude -p /usage` refreshes in a row failing,
  no new swarm session starts. Nothing is shed on stale data (there is nothing to shed on), and Phoenix restarts still run.
  `ui:governor` shows `fresh: false` and `fail_closed` with the reason.
- **The switch.** `governor_enforce` in the data folder's `settings.json`, default false. Off, behaviour is as before, except
  that the core logs `governor (advisory): would have queued X`, `would have shed X` and `would have launched X at haiku
  instead of sonnet`, and `ui:governor` counts `would_queue` and `would_shed` since the core started (a running session counts
  once for as long as it stays over the cap). On, it enforces, logging `governor: queued X`, `governor: shed X` and
  `governor: launching X at haiku`. It is read at every start and tick, so flipping it needs no restart.

### Turning it on

After a few days advisory (the milestone 7 check needs real advisory data first):

1. **Read the would-haves.** `ui:governor` (Slack `governor`, the ops console's Usage section) shows `would_queue` and
   `would_shed`, and `core.log` has one line per decision:
   `Select-String 'governor \(advisory\)' "$env:LOCALAPPDATA\AgentDesk\core.log"`. Queues while the week is young and sheds
   near a 5-hour limit are the point; sheds of useful work at 30% used mean the per-session burn or `governor_k` is off.
2. **Backtest on a copy of the board** (below): with several days of samples, `Backtest_on_recorded_samples` should end each
   week under 100%.
3. **Flip it**: Slack `governor on` (ops area), the ops console's "Enforce the governor's caps" button,
   `ui:governor_enforce {"on": true}` from the window, or `"governor_enforce": true` in `settings.json`. `governor off` turns it back.
4. **Watch the first day**: `governor: shed` lines, and `held` in `ui:governor`. A swarm that never gets going is a cap of 0,
   and its `reason` says why. Then the milestone 7 check: week-end usage lands at 90-100% with the 5-hour window never over 90%.

## Backtest

`GovernorTests.Backtest_following_the_caps_ends_the_week_between_85_and_100` runs three weeks at 5-minute steps (two steady
weeks from no history, then steady, a bursty Wednesday, or a light week), following the recommended caps, over 10 seeds.
Sessions burn 1.2%/h on average, John's trace has noise, `/usage` is rounded down, and a 5-hour window is a quarter of a week.
Every week ends at 97-99% and none goes over 100%, and the 5-hour window never passes 90% (it peaks near 80%).

What it does not cover: John spending far more than his history late in the week (say 30% in the last 12 hours) goes over,
because only the swarm is governed. Enforcement can only shed the swarm.

### On real data

Never point anything at the live board. Copy it first, with the core running or not:

```powershell
$copy = Join-Path $env:TEMP 'agentdesk-backtest.db'
sqlite3 "$env:LOCALAPPDATA\AgentDesk\agentdesk.db" ".backup '$copy'"
$env:AGENTDESK_BACKTEST_DB = $copy
dotnet test --filter Backtest_on_recorded_samples --logger "console;verbosity=detailed"
```

It replays the copy's swarm-free hours as John's own burn, hour of week for hour of week, puts the simulated swarm on top,
and prints each week's ending %. Without `AGENTDESK_BACKTEST_DB` the test does nothing. `claude_usage.json` holds only the
latest reading, so history starts when the core starts recording samples.
