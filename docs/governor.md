# The usage governor (advisory)

Milestone 3 of `docs/GOAL.md`. The governor recommends; nothing enforces its caps yet (that is milestone 7).

## Samples

Every 5 minutes the core runs `claude -p /usage` (no model call) into `claude_usage.json`, and records the reading in the
board's `usage_samples` table: `ts` (the feed's `captured_ts`, unique), `weekly_pct`, `weekly_reset_ts`, `five_hour_pct`,
`five_hour_reset_ts`, and `swarm_sessions`, the identities with `state='running'` at that moment. Readings the status-line
feeder writes are recorded too.

## Forecast

Consecutive samples in the same week become hourly rates (weekly-% per hour; an hour needs 30 minutes of coverage).

- **John's baseline**: hours with no identity session running. An EWMA per hour of week (168 buckets, UTC, alpha 0.5, so
  about two weeks of memory) and a global EWMA with its variance (alpha 0.02). A bucket with under 2 hours falls back to the
  global rate; under 6 baseline hours in all, to `governor_default_rate`.
- **sigma**: the square root of the global variance. It includes the daily pattern, so it errs high.
- **Per-session burn**: hours with sessions, as `(rate - baseline bucket) / sessions`, EWMA alpha 0.2. Under 3 such hours:
  `governor_default_session_rate`.

```
spendable    = max(0, remaining - margin - E[baseline until reset] - k * sigma * sqrt(T hours))
allowed rate = spendable / T
sessions     = min(governor_max_sessions, floor(allowed rate / per-session burn))
```

The reserve `k*sigma*sqrt(T)` shrinks to nothing at the reset, so the governor is cautious early and flat out at the end.
`margin` covers `/usage` reporting whole percentages and the swarm's own noise.

Caps: a swarm is a lead plus `min(governor_max_members, sessions - 1)` members; swarms = sessions / (1 + members), at most
`governor_max_swarms`. While `five_hour_pct >= 90` (and its window has not reset) it recommends no new sessions. When fewer
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

## Backtest

`GovernorTests.Backtest_following_the_caps_ends_the_week_between_85_and_100` runs three weeks at 5-minute steps (two steady
weeks from no history, then steady, a bursty Wednesday, or a light week), following the recommended caps, over 10 seeds.
Sessions burn 1.2%/h on average, John's trace has noise, `/usage` is rounded down, and a 5-hour window is a quarter of a week.
Every week ends at 97-99% and none goes over 100%.

What it does not cover: John spending far more than his history late in the week (say 30% in the last 12 hours) goes over,
because only the swarm is governed. Enforcement (milestone 7) can only shed the swarm.

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
