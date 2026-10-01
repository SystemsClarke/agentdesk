using System.Globalization;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// The Concierge's hands (docs/DISPATCH.md): while the Concierge is on, open Work to Hire items are claimed and handed to a
/// worker by plain code, in priority order, with no LLM deciding. The lead (concierge-lead) is woken only for items marked
/// `triage` (a vague or large one) and ones that failed three times. Order: meta.priority (0 urgent .. 4 whenever, default 2),
/// then id. An item waits for its meta.after (ids that must be done), and an `eager` item only runs while the week's forecast
/// leaves room (projected end below <see cref="EagerBelow"/> percent), which is how spare budget gets spent.
/// </summary>
public sealed class Dispatcher(BoardStore store, Goals goals, string data)
{
    public const double EagerBelow = 90;
    const int MaxAttempts = 3;
    static readonly string[] Mechanical = ["rename", "typo", "docstring", "changelog", "summar", "lookup", "format", "list "];
    public static readonly Caller AsLead = new(null, Concierge.Lead, null, "claude-code", 0, Concierge.Lead);

    /// <summary>Every <paramref name="every"/>: <see cref="Tick"/>.</summary>
    public async Task Run(TimeSpan every, CancellationToken ct = default)
    {
        using (var db = store.Open()) // the lead is woken for triage only, not for every open item
            db.Exec("UPDATE goals SET measure_cmd='internal:open_triage' WHERE name=$n AND measure_cmd='internal:open_work'", ("n", Concierge.Name));
        using var timer = new PeriodicTimer(every);
        while (await timer.WaitForNextTickAsync(ct))
            try { await Tick(); }
            catch (Exception e) { Log.Warn($"dispatcher tick failed: {e.Message}"); }
    }

    /// <summary>Dispatches while there is room; returns the item ids it started a worker for.</summary>
    public async Task<List<long>> Tick(DateTimeOffset? now = null)
    {
        var started = new List<long>();
        List<(long Id, JsonObject Item, string Model)> picks;
        using (var db = store.Open())
        {
            if (db.Rows("SELECT max_members FROM goals WHERE name=$n AND state='running'", ("n", Concierge.Name)).FirstOrDefault() is not { } g) return started;
            var room = (long)g["max_members"]! - (long)db.Scalar("SELECT COUNT(*) FROM goal_members WHERE goal=$n", ("n", Concierge.Name))!;
            // A member still queued for a session is a worker already waiting: more would only pile up claimed items.
            var waiting = (long)db.Scalar("SELECT COUNT(*) FROM goal_members m JOIN identities i ON i.name=m.identity WHERE m.goal=$n AND i.state<>'running'", ("n", Concierge.Name))!;
            if (room <= 0 || waiting > 0) return started;
            var eagerOk = Projected(db, now ?? DateTimeOffset.UtcNow) is { } p && p < EagerBelow;
            var open = db.Rows("SELECT id, subject, meta FROM threads WHERE channel='work' AND status='open'");
            picks = [.. open.Select(t => (Id: (long)t["id"]!, Item: t, Meta: Meta(t)))
                .Where(x => Str(x.Meta, "claim") != "anyone" && x.Meta["triage"]?.GetValue<bool>() != true && (eagerOk || x.Meta["eager"]?.GetValue<bool>() != true) && Ready(db, x.Meta))
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

    /// <summary>The week's forecast at its reset (the governor's projected_end_pct), or null with no usage samples.</summary>
    double? Projected(BoardDb db, DateTimeOffset now) =>
        Governor.Report(db, data, now)["projected_end_pct"] is JsonValue v && v.TryGetValue<double>(out var d) ? d : null;

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
