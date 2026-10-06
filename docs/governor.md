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

- **Young estimates.** The global rate and the starvation estimate are plain means of their first hours (an EWMA runs as a
  running mean until 1/alpha observations), so until they have a day of their own they are shrunk toward
  `governor_default_rate`, with 24 hours of weight. On 2026-09-27 the first day of samples was John's busiest: four
  session-free hours at 1.7-3%/h, then a swarm whose regression slope came out negative (it ran while John slept), so almost
  every hour was John's. That 1.17%/h, spread flat over the 137 hours to the reset, forecast 161% of a 100% plan
  (`GovernorTests.A_young_estimate_never_forecasts_more_than_is_left` rebuilds it).
- **Caps.** John's forecast is at most the % left, and the reserve at most 25 weekly % (and at most the % left).

```
spendable    = max(0, remaining - margin - min(remaining, E[baseline until reset]) - min(25, remaining, k * sigma * sqrt(T hours)))
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

## The session pool

Every running Claude session counts in one pool (`Budget` in Governor.cs): John's own identities, goal leads and members, and the
Concierge's lead and members. A Phoenix successor takes its predecessor's place and never adds a slot.

- **One ceiling.** `max_sessions` (Options' "Sessions at once", default 3), and while the governor enforces, never more than its
  `total_sessions` (0 when it fails closed). Advisory, the ceiling is `max_sessions` and the core logs what the lower one would
  have done. `why` says which: `max_sessions`, `plan` or `fail closed`.
- **One question.** `Budget.CanStart(goal, lead, ceiling)` answers every start: `identity_start`, a goal's `member_spawn`, the
  Concierge's dispatch, lead wakes and the core's resume all go through `Identities.Drain`, which asks it. John's own identities
  answer only to `max_sessions`: the plan never gates them and nothing sheds them.
- **Fair share.** Goals (the Concierge is one) split the swarm's part, the ceiling minus John's running identities: each gets
  its lead first, then members round-robin, the goal that has waited longest (its oldest queued identity) first. A share is
  never more than a goal wants, and `max_members` bounds its members within the pool: a `member_spawn` past it is queued, not
  refused. A goal starts another session only while it runs fewer than its share, so a slot a member frees goes to the goal
  that has waited longest.
- **One view.** `ui:status`'s `sessions` and `ui:governor`'s `pool`: the ceiling and why, running by owner (John, each goal, the
  Concierge), queued by goal, and each goal's share, with a one-line `summary` that the window's budget panel, Options' "Sessions
  at once", Slack `status` and the ops console show.

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

- **Gate.** A swarm identity starts only while the pool is under `min(max_sessions, total_sessions)` (all running identities,
  John's included) and its goal is under its share ([The session pool](#the-session-pool)). Otherwise it stays `queued` rather
  than failing: `identity_start` and `member_spawn` return the queued row with a `governor` note, and a wake says why. Every
  15 s the core's governor tick (`Identities.Tick`) drains again, so a held start launches, with the prompt it was given, as
  soon as the pool allows.
- **Phoenix is never blocked.** A `pass_the_torch` restart replaces a session in the same slot (`Identities.Phoenix` launches
  directly, not through `Drain`), so it is never a new session, never counted as one, and runs even when the governor would
  allow nothing, including fail closed. It does take the step-down tier.
- **Shedding.** When the ceiling, `min(max_sessions, total_sessions)`, drops below the number running (the 5-hour guard at 90%, or the reserve outgrowing what is
  left), the tick stops swarm sessions until it is back within the cap: members before leads, the Concierge's members first,
  then the longest idle first (idle means the last board write, `presence.seen_ts`, else the launch). A shed identity goes back
  to `queued`, not `stopped`, so it resumes its conversation (`--resume`) when the governor allows. John's own identities are
  never shed by the governor; if they alone are over the cap, the core logs a warning (once per change). Only active ones hold the cap: while swarm work is queued and the pool is full, each tick retires one of John's identities that has shown no sign of life (no transcript write) for 30 minutes (`AGENTDESK_IDLE_MINUTES`), oldest first. It is stopped, not queued, and John's reply (wake) starts it again on the same conversation. A session mid-Phoenix is left alone.
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
   `ui:governor_enforce {"on": true}` sent to the core (the window has no toggle for it), or `"governor_enforce": true` in `settings.json`. `governor off` turns it back.
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

## The end-of-week push (2026-10)

The aim is to finish each week near 100%, with the Work to Hire queue as where the spare budget goes.

- **Pace.** The even spread `spendable / hours left` is multiplied by `Governor.Pace`: `governor_floor` (default 0.6) at the start of the week,
  rising to 1 at the reset (`governor_ramp`, default 1; 0 is the old flat spread). Spendable is recomputed from what is really left each time, so
  the cushion held back early is spent later, harder. More cushion early, a push at the end.
- **Tracking.** `ui:governor` gains `plan_end_pct` (where the plan lands, from `PlanPath`: John's expected burn plus the allowance, hour by hour to the
  reset), `trend_end_pct` (where the last six hours' real pace would end the week), `unused_pct`, `status` (on pace / behind / on course to hit the limit),
  `pace`, `week_elapsed_pct`, and `forecast` (the plan path, a sparkline in the window's budget panel).
- **The queue soaks it up.** The Dispatcher treats the week as *behind* when `trend_end_pct` is more than 2 under `plan_end_pct`. Then it runs `eager`
  Work to Hire items and lifts the Concierge's worker cap from 3 to what the governor can afford (`caps.new_sessions`), as many workers as the pool allows. On pace, back to 3 and no eager items.

## The learned cost per session: removed

An earlier version trained a small network on the governor's own readings to learn the cost of a haiku, sonnet or opus session. It trained on the
older three quarters of the hours and had to beat the plain average on the newest quarter to be used. On John's data it never did (2026-10-06: 225
usable hours, off by 1.78%/h against 0.74 for the plain average: the sessions barely vary and the meter moves in whole percents), so the governor always used its own
estimate and the code, its tests, `governor_learn` and `ui:governor`'s `learned` block were removed. If it is ever wanted again, the better input
would be tokens per tier from the session transcripts instead of session counts.

Also: when the week is behind its plan, the Dispatcher's spare workers are the larger of the governor's `new_sessions` and the headroom between the
plan's allowed rate and the week's real recent burn, divided by the cost per session.
