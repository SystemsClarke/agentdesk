using System.Globalization;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// The Concierge's hands (docs/DISPATCH.md): while the Concierge is on, open Work to Hire items are claimed and handed to a
/// worker by plain code, in priority order, with no LLM deciding. The lead (concierge-lead) is woken only for items marked
/// `triage` (a vague or large one) and ones that failed three times. Order: meta.priority (0 urgent .. 4 whenever, default 2),
/// then id. An item waits for its meta.after (ids that must be done), and an `eager` item only runs while the week is behind its
/// plan (the governor's trend ends under its plan), which is how spare budget gets spent; being behind also lifts the cap on workers
/// to what the governor can afford.
/// </summary>
public sealed class Dispatcher(BoardStore store, Goals goals, string data)
{
    const int MaxAttempts = 3;
    static readonly string[] Mechanical = ["rename", "typo", "docstring", "changelog", "summar", "lookup", "format", "list "];
    public static readonly Caller AsLead = new(null, Concierge.Lead, null, "claude-code", 0, Concierge.Lead);

    /// <summary>Every <paramref name="every"/>: <see cref="Tick"/>.</summary>
    public async Task Run(TimeSpan every, CancellationToken ct = default)
    {
        try
        {
            using var db = store.Open(); // the lead is woken for triage only, not for every open item
            db.Exec("UPDATE goals SET measure_cmd='internal:open_triage' WHERE name=$n AND measure_cmd='internal:open_work'", ("n", Concierge.Name));
        }
        catch (Exception e) { Log.Warn($"dispatcher start failed: {e.Message}"); } // a locked db at boot must not end the dispatcher for the whole run
        using var timer = new PeriodicTimer(every);
        while (await timer.WaitForNextTickAsync(ct))
            try { await Tick(); }
            catch (Exception e) { Log.Warn($"dispatcher tick failed: {e.Message}"); }
    }

    /// <summary>When a recurring item (meta.recur) has not come round yet, and the next one once it is done. recur is "HH:MM" (every day at that
    /// local time) or N minutes / "Nh" (that long after the last finished): the "run every morning" job.</summary>
    public static class Recur
    {
        public static bool Valid(string recur) => Parse(recur) is not null;

        static (TimeSpan? Every, TimeOnly? At)? Parse(string r)
        {
            r = r.Trim();
            if (TimeOnly.TryParseExact(r, "HH:mm", CultureInfo.InvariantCulture, DateTimeStyles.None, out var at)) return (null, at);
            var unit = r.EndsWith('h') ? 60.0 : 1.0;
            return double.TryParse(r.TrimEnd('m', 'h'), CultureInfo.InvariantCulture, out var n) && n >= 1 ? (TimeSpan.FromMinutes(n * unit), null) : null;
        }

        /// <summary>The first run: a daily time waits for its next occurrence; an interval starts at once.</summary>
        public static DateTimeOffset First(string recur, DateTimeOffset now) => Next(recur, now, first: true);

        public static DateTimeOffset Next(string recur, DateTimeOffset from, bool first = false)
        {
            var (every, at) = Parse(recur)!.Value;
            if (every is { } e) return first ? from : from + e;
            var local = from.ToLocalTime();
            var due = new DateTimeOffset(local.Date + at!.Value.ToTimeSpan(), local.Offset);
            return due > local ? due : due.AddDays(1);
        }
    }

    /// <summary>Dispatches while there is room; returns the item ids it started a worker for.</summary>
    public async Task<List<long>> Tick(DateTimeOffset? now = null)
    {
        var started = new List<long>();
        List<(long Id, JsonObject Item, string Model)> picks;
        using (var db = store.Open())
        {
            if (db.Rows("SELECT max_members FROM goals WHERE name=$n AND state='running'", ("n", Concierge.Name)).FirstOrDefault() is not { } g) return started;
            var members = (long)db.Scalar("SELECT COUNT(*) FROM goal_members WHERE goal=$n", ("n", Concierge.Name))!;
            // Behind the plan (the week is on course to end with budget unspent): the queue is where it goes, so allow as many workers as
            // the governor says it can afford, beyond the usual few. On pace, back to the usual few.
            var (behind, spare) = Spend(db, now ?? DateTimeOffset.UtcNow);
            var cap = behind ? Math.Max(Concierge.BaseMembers, members + spare) : Concierge.BaseMembers;
            if (cap != (long)g["max_members"]!) db.Exec("UPDATE goals SET max_members=$m WHERE name=$n", ("m", cap), ("n", Concierge.Name));
            var room = cap - members;
            // A member still queued for a session is a worker already waiting: more would only pile up claimed items.
            var waiting = (long)db.Scalar("SELECT COUNT(*) FROM goal_members m JOIN identities i ON i.name=m.identity WHERE m.goal=$n AND i.state<>'running'", ("n", Concierge.Name))!;
            if (room <= 0 || waiting > 0) return started;
            var eagerOk = behind;
            Renew(db, now ?? DateTimeOffset.UtcNow);
            var open = db.Rows("SELECT id, subject, meta FROM threads WHERE channel='work' AND status='open'");
            picks = [.. open.Select(t => (Id: (long)t["id"]!, Item: t, Meta: Meta(t)))
                .Where(x => Str(x.Meta, "claim") != "anyone" && x.Meta["triage"]?.GetValue<bool>() != true && (eagerOk || x.Meta["eager"]?.GetValue<bool>() != true) && Ready(db, x.Meta) && Due(x.Meta, now ?? DateTimeOffset.UtcNow))
                .OrderBy(x => Priority(x.Meta)).ThenBy(x => x.Id).Take((int)room)
                .Select(x => (x.Id, x.Item, ModelFor(x.Meta, Str(x.Item, "subject") ?? "")))];
        }
        foreach (var (id, item, model) in picks)
        {
            using (var db = store.Open())
            {
                var meta = Meta(item);
                var tries = (meta["attempts"]?.GetValue<long>() ?? 0) + 1;
                if (tries > MaxAttempts) { Triage(db, id, meta, $"{MaxAttempts} workers ended without completing it"); continue; }
                if (!db.ClaimTask(id, Concierge.Lead)) continue; // another claimed it first
                meta = Meta(db.Rows("SELECT meta FROM threads WHERE id=$i", ("i", id))[0]);
                meta["attempts"] = tries;
                db.Exec("UPDATE threads SET meta=$m WHERE id=$i", ("m", Py.Dumps(meta)), ("i", id));
            }
            try
            {
                await goals.Spawn(AsLead, Concierge.Name, $"w{id}-{Guid.NewGuid().ToString("N")[..4]}", $"Work to Hire item #{id}: {Str(item, "subject")}. Read it with read_thread {id}.", model, id);
                started.Add(id);
                Log.Info($"dispatcher: work item #{id} to a {model} worker");
            }
            catch (ArgumentException e)
            {
                Log.Warn($"dispatcher: could not start a worker for #{id}: {e.Message}");
                using var db = store.Open();
                db.ReopenTask(id);
            }
        }
        return started;
    }

    /// <summary>Whether the week is running behind its plan (its last six hours' pace would end under the plan's landing), and how many more sessions the governor says are affordable.</summary>
    (bool Behind, int Spare) Spend(BoardDb db, DateTimeOffset now)
    {
        var r = Governor.Report(db, data, now);
        var behind = r["trend_end_pct"] is JsonValue t && r["plan_end_pct"] is JsonValue p && t.TryGetValue<double>(out var trend) && p.TryGetValue<double>(out var plan) && trend < plan - 2;
        var spare = r["caps"]?["new_sessions"] is JsonValue n && n.TryGetValue<int>(out var ns) ? ns : 0;
        // The sessions running now may burn far less than the allowance (idle ones cost little): the headroom between what the week is
        // really burning and what the plan allows buys more workers, at the governor's cost per session. It is re-read every tick, so as the
        // workers burn, the headroom closes.
        double Num(string k) => r[k] is JsonValue v && v.TryGetValue<double>(out var x) ? x : 0;
        if (behind && Num("reset_in_hours") > 0)
        {
            var trendRate = (Num("trend_end_pct") - Num("used")) / Num("reset_in_hours");
            spare = Math.Max(spare, (int)Math.Floor(Math.Max(0, Num("allowed_rate") - trendRate) / Math.Max(0.05, Num("session_rate"))));
        }
        return (behind, spare);
    }

    static bool Due(JsonObject meta, DateTimeOffset now) => Str(meta, "due") is not { } d || DateTimeOffset.Parse(d, CultureInfo.InvariantCulture) <= now;

    /// <summary>A finished recurring item posts its next occurrence, once: a fresh open item, due when the schedule says.</summary>
    static void Renew(BoardDb db, DateTimeOffset now)
    {
        foreach (var t in db.Rows("SELECT id, subject, opened_by, meta FROM threads WHERE channel='work' AND status='done' AND json_valid(meta) "
                                  + "AND json_extract(meta, '$.recur') IS NOT NULL AND json_extract(meta, '$.renewed') IS NULL"))
        {
            var meta = Meta(t);
            var next = new JsonObject();
            foreach (var k in new[] { "claim", "priority", "model", "eager", "after", "recur" }) if (meta[k] is { } v) next[k] = v.DeepClone();
            next["due"] = Recur.Next(Str(meta, "recur")!, now).ToString("o", CultureInfo.InvariantCulture);
            var body = (string?)db.Scalar("SELECT body FROM messages WHERE thread_id=$t ORDER BY id LIMIT 1", ("t", (long)t["id"]!)) ?? "";
            var id = db.StartThread("work", Str(t, "subject")!, Str(t, "opened_by")!, BoardDb.Agent, body, next);
            meta["renewed"] = id;
            db.Exec("UPDATE threads SET meta=$m WHERE id=$i", ("m", Py.Dumps(meta)), ("i", (long)t["id"]!));
        }
    }

    static JsonObject Meta(JsonObject t) { try { return Str(t, "meta") is { } m && JsonNode.Parse(m) is JsonObject o ? o : []; } catch (System.Text.Json.JsonException) { return []; } }
    static string? Str(JsonObject o, string k) => o[k]?.ToString();
    static long Priority(JsonObject meta) => meta["priority"]?.GetValue<long>() ?? 2;

    static bool Ready(BoardDb db, JsonObject meta) =>
        meta["after"] is not JsonArray after || after.All(a => db.Scalar("SELECT status FROM threads WHERE id=$i", ("i", a!.GetValue<long>())) as string is "done");

    static string ModelFor(JsonObject meta, string subject) =>
        Str(meta, "model") is ("haiku" or "sonnet" or "opus") and { } m ? m : Mechanical.Any(k => subject.Contains(k, StringComparison.OrdinalIgnoreCase)) ? "haiku" : "sonnet";

    /// <summary>Puts the item back on the queue marked for the lead: a worker could not do it, or it needs splitting.</summary>
    static void Triage(BoardDb db, long id, JsonObject meta, string why)
    {
        meta["triage"] = true;
        foreach (var k in new[] { "assignee", "claimed_ts" }) meta.Remove(k);
        db.Exec("UPDATE threads SET meta=$m WHERE id=$i", ("m", Py.Dumps(meta)), ("i", id));
        db.Reply(id, BoardDb.Agent, BoardDb.Agent, $"Dispatcher: {why}, so it goes to the Concierge's lead to split or finish.");
    }
}
