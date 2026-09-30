# Dispatch: one queue, one operator, mostly SQL

Status: proposal (2026-09-30), not built. Replaces the separate goal / Concierge / identity-wake paths with one mechanism.

## The idea

Work to Hire (the `work` channel) is the only place work starts. A **dispatcher in the core** (plain C# and SQLite, no LLM) is the
operator: it looks at open items, the session pool and the spend budget, and plugs a worker into each item. An LLM is only used
where judgement is needed (doing the work, or splitting a vague item), never to decide *whether* or *when* to start something.

```
post_work / John / a goal's lead / a schedule
        |
        v
   work item (thread, channel=work)  --priority, model, eager, due, parent, measure in meta/columns
        |
   Dispatcher.Tick  (every 5 s and on every board change)
        |  1. pick: highest priority, then oldest, whose dependencies are done
        |  2. gate: pool slot free AND budget room (governor)
        |  3. claim (one conditional UPDATE) + launch worker identity "w<id>" with the item as its first prompt
        v
   worker does it -> complete_work -> retire (killed at once, slot freed) -> Dispatcher.Tick picks the next
```

## What collapses into it

| Today | Becomes |
|---|---|
| Concierge (LLM lead that triages the queue) | The dispatcher. The lead is only called for items tagged `needs_split`. |
| Goal (hypothesis, measure, wakes) | A work item with a `measure` and a `success` line that re-queues itself until met. |
| Goal member (`member_spawn`) | A child work item (`parent`) the dispatcher runs. |
| Recurring / run-once-each-morning job | A work item with `due`/`every` in meta. The dispatcher posts the next one when it finishes. |
| Wake on John's reply | Dispatcher rule: a human reply on a thread wakes its asker at once (types in if running, resumes if asleep). |
| Identity queue + Budget shares | One ordered query over work items and "wake" requests. |

## Deterministic pieces (SQLite does the thinking)

- **Priority**: `meta.priority` 0 (urgent) .. 4 (whenever); default 2. Order: priority, then `due`, then id.
- **Dependencies**: `meta.after = [ids]`; an item is runnable when all are `done`.
- **Model choice**: `meta.model`, else by kind keyword (mechanical -> haiku, default sonnet, `hard` -> opus). A table, not a prompt.
- **Retries**: a worker that ends without `complete_work` re-opens the item with `attempts+1`; after 3 it goes to John as a question.
- **Budget**: the governor already forecasts the week. Add two modes per item: `normal` (only while the plan allows) and `eager`
  ("spend the room"): when projected end-of-week spend is under target, the dispatcher fills every free slot with eager items,
  as hard as the pool allows, and stops when the forecast hits the line. Monthly spend is a setting the forecast already
  has the shape for (`projected_end_pct`).
- **Answers are prioritised**: an agent that John just replied to goes to the front of the slot queue.

## Why replies feel slow today (and the fix)

1. A reply only reaches a running agent if it started a `wait`, or John presses Ctrl+R. Fix: reply => dispatcher wake, automatically.
2. Sessions are queued behind a pool over its cap (7-8 running vs cap 1 in the log). Fix: workers retire at once; replies jump the queue.
3. Goal leads wake on a 10-30 min timer. Fix: event-driven (board change), timer only as a backstop.

## Build order (each step shippable, cuts code)

1. **Reply wakes asker + replied-to jumps the slot queue.** Small; fixes the speed complaint. (`Identities.Wake` already exists; call it from `ui:reply`.)
2. **Dispatcher v1**: priority/after/model in `post_work`, deterministic claim + worker launch, retire on complete. Concierge lead becomes optional.
3. **Eager mode** against the governor's forecast.
4. **Recurring items** (due/every) - this is the "run every morning" feature.
5. **Goals as measured work items**; delete the goals table paths, `Goals.cs` shrinks to measure + verdict.
6. Window: one Agents/Work screen instead of Agents + Goals.

## Open questions for John

- Should a reply wake a sleeping agent every time, or only agents marked `auto_wake`? (Waking spends tokens.)
- Monthly budget: what number, and is "eager" allowed to use all of it or a fraction?
- Keep the word "goal" in the window, or show them as "measured jobs"?
