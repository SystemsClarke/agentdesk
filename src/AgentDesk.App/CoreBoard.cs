using System.Text.Json;
using AgentDesk.Contracts;

namespace AgentDesk.App;

/// <summary>The real board, over the core's pipe (docs/ui-api.md). Reads never leave receipts; John's writes go through ui:*.</summary>
public sealed class CoreBoard : IBoard, IDisposable
{
    readonly CoreConnection core;

    public event EventHandler? Changed;

    CoreBoard(CoreConnection core)
    {
        this.core = core;
        core.Pushed += text => { if (text.Contains("board.changed")) Changed?.Invoke(this, EventArgs.Empty); };
        core.Reconnected += async () => // the core restarted (an update): subscribe again and redraw
        {
            try { await core.Call("ui:subscribe"); Changed?.Invoke(this, EventArgs.Empty); } catch (System.IO.IOException) { }
        };
    }

    public static async Task<CoreBoard> Connect()
    {
        var board = new CoreBoard(await CoreConnection.Connect(new Caller(null, null, Environment.CurrentDirectory, "ui", Environment.ProcessId)));
        await board.core.Call("ui:subscribe");
        return board;
    }

    async Task<JsonElement> Call(string tool, object? args = null)
    {
        var root = JsonDocument.Parse(await core.Call(tool, args is null ? null : JsonSerializer.SerializeToElement(args))).RootElement;
        return root.TryGetProperty("error", out var e) ? throw new InvalidOperationException(e.GetString()) : root;
    }

    static string? Str(JsonElement o, string name) => o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
    static DateTimeOffset Ts(JsonElement o, string name) => DateTimeOffset.TryParse(Str(o, name), out var t) ? t : default;
    static int Int(JsonElement o, string name) => o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt32() : 0;
    static double? Num(JsonElement o, string name) => o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetDouble() : null;
    static readonly JsonElement None = JsonDocument.Parse("[]").RootElement;
    static JsonElement.ArrayEnumerator Arr(JsonElement o, string name) => (o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Array ? v : None).EnumerateArray();

    /// <summary>A message's or thread's meta is JSON inside a string.</summary>
    static string? Meta(JsonElement o, string key)
    {
        try
        {
            return Str(o, "meta") is { } m && JsonDocument.Parse(m).RootElement is { ValueKind: JsonValueKind.Object } meta ? Str(meta, key) : null;
        }
        catch (JsonException)
        {
            return null;
        }
    }

    static ThreadRow Row(JsonElement t) => new(Int(t, "id") is var id and > 0 ? id : Int(t, "thread_id"), Str(t, "channel") ?? "question",
        Str(t, "status") ?? "open", Str(t, "subject") ?? "", Str(t, "opened_by") ?? "", Ts(t, "created_ts"), Ts(t, "updated_ts"),
        Int(t, "message_count"), Int(t, "waiting") != 0, Str(t, "last_author"), Str(t, "delivery"), Meta(t, "assignee"));

    static Post ToPost(JsonElement m) => new(Str(m, "author") ?? "", Ts(m, "ts"), Int(m, "thread_id"), Str(m, "subject") ?? "",
        Str(m, "channel") ?? "", Kind: Meta(m, "kind"), Via: Meta(m, "via"));

    async Task<List<ThreadRow>> List(object args) => [.. (await Call("list_threads", args)).GetProperty("threads").EnumerateArray().Select(Row)];

    public async Task<IReadOnlyList<ThreadRow>> ListThreadsAsync(string channel) =>
        await List(new { channel, limit = 300, include_archived = true });

    public async Task<ThreadDetail?> ReadThreadAsync(int id)
    {
        try
        {
            var doc = await Call("ui:thread", new { thread_id = id });
            return new(Row(doc.GetProperty("thread")), [.. doc.GetProperty("messages").EnumerateArray().Select(m =>
                new Message(Str(m, "author") ?? "", Ts(m, "ts"), Str(m, "body") ?? "", Meta(m, "kind"), Meta(m, "via")))]);
        }
        catch (InvalidOperationException)
        {
            return null; // no such thread
        }
    }

    public async Task<IReadOnlyList<ThreadRow>> OpenQuestionsAsync() =>
        [.. (await Call("open_questions")).GetProperty("open_questions").EnumerateArray().Select(q => Row(q) with { Waiting = true })];

    public Task ReplyAsync(int id, string body) => Call("ui:reply", new { thread_id = id, body });

    public Task CloseAsync(int id) => Call("ui:close", new { thread_id = id });

    public Task UnarchiveAsync(int id) => Call("ui:unarchive", new { thread_id = id });

    public async Task<int> PostAsync(string channel, string subject, string body) =>
        Int(await Call("ui:post", new { channel, subject, body }), "thread_id");

    public async Task<BoardStatus> StatusAsync()
    {
        var recent = (await Call("recent_messages", new { limit = 60 })).GetProperty("messages").EnumerateArray()
            .Where(m => Meta(m, "kind") is not ("ack" or "ack-note")).Select(ToPost).ToList();
        var john = recent.FirstOrDefault(p => p.Author == "john");
        var bios = (await List(new { channel = "discussion", limit = 300 })).Where(t => t.Subject.StartsWith("bio: "))
            .GroupBy(t => t.Subject[5..].Trim()).ToDictionary(g => g.Key, g => g.First().Id);
        var filed = (await List(new { channel = "question", status = "archived", limit = 1 })).FirstOrDefault();
        var beat = await Call("ui:status");
        var concierge = beat.GetProperty("concierge");
        var sessions = beat.GetProperty("sessions");
        var slack = beat.GetProperty("slack") is { ValueKind: JsonValueKind.Object } s ? s : default;
        var usage = beat.GetProperty("usage");
        var relay =slack.ValueKind == JsonValueKind.Object && slack.TryGetProperty("last_relay", out var r) && r.ValueKind == JsonValueKind.Object
            ? new Post("john", Ts(r, "ts"), Int(r, "thread_id"), "", "question") : null;
        var prs = beat.GetProperty("prs").EnumerateArray().Select(p => new PrRow(Str(p, "repo") ?? "", Int(p, "number"), Str(p, "title") ?? "",
            Str(p, "url") ?? "", Str(p, "state") ?? "open", Str(p, "requested_by") ?? "", Str(p, "checked_ts") is null ? null : Ts(p, "checked_ts"),
            Str(p, "last_error"), Str(p, "triage"), Int(p, "thread_id") is var t and > 0 ? t : null, Str(p, "source") == "github-scan"));
        var swarm = concierge.GetProperty("members").EnumerateArray().Select(m => new SwarmMember(Str(m, "identity") ?? "", Str(m, "task") ?? "",
            Int(m, "work_id") is var w and > 0 ? w : null));
        var held = concierge.GetProperty("held").EnumerateArray().Select(h => h.GetInt32()).FirstOrDefault();
        return new([.. prs], [.. recent.Take(5)], [.. recent.Where(p => p.Author != "john" && p.Ts > DateTimeOffset.Now.AddDays(-1)).DistinctBy(p => p.Author)],
            bios, john, recent.TakeWhile(p => p.Author != "john").Count(p => p.Kind != "read-receipt"), filed,
            concierge.GetProperty("on").GetBoolean(), held > 0 ? held : null, [.. swarm],
            slack.ValueKind == JsonValueKind.Object ? Ts(slack, "ts") : null, slack.ValueKind == JsonValueKind.Object && Int(slack, "poll_s") is var poll and > 0 ? poll : 15,
            relay, [], [.. usage.GetProperty("lines").EnumerateArray().Select(l => l.GetString()!)], Str(usage, "summary") ?? "",
            Int(sessions, "running"), Int(sessions, "max"), ToBudget(beat.GetProperty("governor")), [.. Arr(beat, "goals").Select(ToGoal)]);
    }

    static Budget ToBudget(JsonElement g)
    {
        var caps = g.TryGetProperty("caps", out var c) ? c : default;
        int Cap(string name) => caps.ValueKind == JsonValueKind.Object ? Int(caps, name) : 0;
        var mode = g.TryGetProperty("enforcing", out var e) && e.ValueKind is JsonValueKind.True or JsonValueKind.False ? (e.GetBoolean() ? "enforcing" : "advisory")
            : g.TryGetProperty("advisory", out var a) && a.ValueKind is JsonValueKind.True or JsonValueKind.False ? (a.GetBoolean() ? "advisory" : "enforcing") : null;
        return new(Int(g, "samples"), Num(g, "remaining") ?? 0, Num(g, "reset_in_hours") ?? 0, Num(g, "projected_end_pct") ?? 0, Cap("total_sessions"),
            Cap("swarms"), Cap("members_per_swarm"), Str(g, "reason") ?? "", Str(g, "summary") ?? "", [.. Arr(g, "series").Select(v => v.GetDouble())], mode);
    }

    static GoalRow ToGoal(JsonElement g) => new(Str(g, "name") ?? "", Str(g, "state") ?? "draft", Str(g, "objective") ?? "", Str(g, "lead") ?? "",
        Str(g, "success"), Int(g, "experiments"), Num(g, "last_value"), Int(g, "members"), Int(g, "max_members"), Int(g, "standing") != 0,
        Int(g, "thread_id") is var t and > 0 ? t : null);

    public async Task<IReadOnlyList<Slot>> SlotsAsync() =>
        [.. Arr(await Call("ui:slot_list"), "slots").Select(s => new Slot(Int(s, "n"), Str(s, "goal"), Str(s, "channel_name"), Str(s, "persona_name")))];

    public async Task<GoalDetail?> GoalAsync(string name)
    {
        try
        {
            var g = await Call("ui:goal_status", new { name });
            var log = Arr(g, "experiments").Select(e => new Experiment(Int(e, "n"), Str(e, "change") ?? "", Str(e, "owner"), Num(e, "value"), Str(e, "verdict"))).ToList();
            return new(ToGoal(g) with { Experiments = log.Count, LastValue = log.LastOrDefault(e => e.Value is not null)?.Value,
                    Members = Arr(g, "members").Count() }, Str(g, "hypothesis"), Str(g, "measure_cmd"), Num(g, "max_hours") ?? 0, Num(g, "cadence_minutes") ?? 0,
                Str(g, "started_ts") is null ? null : Ts(g, "started_ts"), log, [.. Arr(g, "history").Select(v => v.GetDouble())],
                [.. Arr(g, "members").Select(m => new SwarmMember(Str(m, "identity") ?? "", Str(m, "task") ?? "", Int(m, "work_id") is var w and > 0 ? w : null))],
                Str(g, "summary") ?? "");
        }
        catch (InvalidOperationException)
        {
            return null; // no such goal
        }
    }

    public async Task<IReadOnlyList<Identity>> IdentitiesAsync() =>
        [.. (await Call("ui:identity_list")).GetProperty("identities").EnumerateArray().Select(r => new Identity(Str(r, "name") ?? "",
            Str(r, "state") ?? "stopped", Math.Max(1, Int(r, "generation")), Str(r, "host") ?? "windows", Str(r, "folder") ?? "", Str(r, "model")))];

    public async Task<IReadOnlyList<Adoptable>> AdoptableAsync() =>
        [.. (await Call("ui:adoptable")).GetProperty("sessions").EnumerateArray().Select(s => new Adoptable(Str(s, "session_id") ?? "",
            Str(s, "folder") ?? "", Ts(s, "last_activity"), Str(s, "first_message") ?? ""))];

    public async Task<string?> WebUrlAsync() => Str(await Call("ui:web_url"), "url");

    public async Task<string?> ActAsync(string request, object? args = null) => await Call(request, args) is var r ? Str(r, "said") ?? Str(r, "note") : null;

    public void Dispose() => core.Dispose();
}
