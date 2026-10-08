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
        core.Reconnected += () => _ = Subscribe(); // the core restarted (an update): subscribe again and redraw
    }

    static readonly Caller Me = new(null, null, Environment.CurrentDirectory, "ui", Environment.ProcessId);

    /// <summary>The board before the core has answered: the window shows itself at once and the first calls wait for the connection.
    /// Subscribes (and so starts the core, if it is not running) in the background, trying until it is up.</summary>
    public static CoreBoard Create()
    {
        var board = new CoreBoard(CoreConnection.Create(Me));
        _ = board.Subscribe();
        return board;
    }

    volatile bool disposed;

    /// <summary>Asks for the core's pushes, until it answers: a core that is starting, or restarting after an update, is tried again with a
    /// growing pause (up to 5 s). Nothing may escape it: it runs on a pool thread.</summary>
    async Task Subscribe()
    {
        for (var attempt = 1; !disposed; attempt++)
        {
            try { await core.Call("ui:subscribe"); }
            catch (Exception) { await Task.Delay(Math.Min(attempt, 5) * 1000); continue; }
            Changed?.Invoke(this, EventArgs.Empty);
            return;
        }
    }

    public static async Task<CoreBoard> Connect()
    {
        var board = new CoreBoard(await CoreConnection.Connect(Me));
        await board.core.Call("ui:subscribe");
        return board;
    }

    async Task<JsonElement> Call(string tool, object? args = null)
    {
        var root = JsonDocument.Parse(await core.Call(tool, args is null ? null : JsonSerializer.SerializeToElement(args))).RootElement;
        // {"error": null} is a reply that carries no error (the dictation state has one always); only a message is a failure.
        return root.TryGetProperty("error", out var e) && e.ValueKind == JsonValueKind.String ? throw new InvalidOperationException(e.GetString()) : root;
    }

    static string? Str(JsonElement o, string name) => o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
    static DateTimeOffset Ts(JsonElement o, string name) => DateTimeOffset.TryParse(Str(o, name), out var t) ? t : default;
    static int Int(JsonElement o, string name) => o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt32() : 0;
    static double? Num(JsonElement o, string name) => o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetDouble() : null;
    static readonly JsonElement None = JsonDocument.Parse("[]").RootElement, NoFields = JsonDocument.Parse("{}").RootElement;
    /// <summary>A member that is an object, or an empty one: a core that answers with a different shape shows less, not nothing.</summary>
    static JsonElement Obj(JsonElement o, string name) => o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Object ? v : NoFields;
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
        Int(t, "message_count"), Int(t, "waiting") != 0, Str(t, "last_author"), Str(t, "delivery"), Meta(t, "assignee"), Str(t, "follow_up"));

    static Post ToPost(JsonElement m) => new(Str(m, "author") ?? "", Ts(m, "ts"), Int(m, "thread_id"), Str(m, "subject") ?? "",
        Str(m, "channel") ?? "", Kind: Meta(m, "kind"), Via: Meta(m, "via"));

    // ui:threads is list_threads without each thread's last message body, which Row never reads and which was most of the bytes.
    async Task<List<ThreadRow>> List(object args) => [.. (await Call("ui:threads", args)).GetProperty("threads").EnumerateArray().Select(Row)];

    /// <summary>The discussion list the window just asked for: StatusAsync reads bios from the same 300 rows, and takes this one when it is
    /// fresh instead of asking the core for them again.</summary>
    (Task<List<ThreadRow>> Rows, long At)? discussion;
    const long DiscussionShareMs = 3000;

    public async Task<IReadOnlyList<ThreadRow>> ListThreadsAsync(string channel)
    {
        if (channel == "discussion")
        {
            var rows = List(new { channel, limit = 300, include_archived = true });
            discussion = (rows, Environment.TickCount64);
            return await rows;
        }
        if (channel != "question")
            return await List(new { channel, limit = 300, include_archived = true });
        // The newest 300 of everything let a long archive push an old, still-waiting question off the list: take each side on its own cap.
        var (active, filed) = (List(new { channel, limit = 300, include_archived = false }), List(new { channel, status = "archived", limit = 300 }));
        await Task.WhenAll(active, filed);
        return [.. await active, .. await filed];
    }

    public async Task<ThreadDetail?> ReadThreadAsync(int id)
    {
        try
        {
            var doc = await Call("ui:thread", new { thread_id = id });
            return new(Row(doc.GetProperty("thread")), [.. doc.GetProperty("messages").EnumerateArray().Select(m =>
                new Message(Str(m, "author") ?? "", Ts(m, "ts"), Str(m, "body") ?? "", Meta(m, "kind"), Meta(m, "via")))]);
        }
        catch (InvalidOperationException e) when (e.Message.Contains("no such thread", StringComparison.OrdinalIgnoreCase))
        {
            return null; // any other core error is a failure, not a missing thread
        }
    }

    public async Task<IReadOnlyList<ThreadRow>> OpenQuestionsAsync() =>
        [.. (await Call("open_questions")).GetProperty("open_questions").EnumerateArray().Select(q => Row(q) with { Waiting = true })];

    public Task ReplyAsync(int id, string body) => Call("ui:reply", new { thread_id = id, body });

    /// <summary>{"closed": false} is a reply that did nothing; a core that does not say is taken at its word.</summary>
    static bool Did(JsonElement r, string name) => !(r.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.False);

    public async Task<bool> CloseAsync(int id) => Did(await Call("ui:close", new { thread_id = id }), "closed");

    public async Task<bool> UnarchiveAsync(int id) => Did(await Call("ui:unarchive", new { thread_id = id }), "unarchived");

    public async Task<int> PostAsync(string channel, string subject, string body) =>
        Int(await Call("ui:post", new { channel, subject, body }), "thread_id");

    public async Task<BoardStatus> StatusAsync()
    {
        // Five independent reads: ask for all of them at once (the core answers each on its own thread) instead of one after another.
        var recentCall = Call("recent_messages", new { limit = 60 });
        var biosCall = discussion is { } d && Environment.TickCount64 - d.At < DiscussionShareMs && !d.Rows.IsFaulted && !d.Rows.IsCanceled
            ? d.Rows : List(new { channel = "discussion", limit = 300 });
        var filedCall = List(new { channel = "question", status = "archived", limit = 1 });
        var beatCall = Call("ui:status");
        await Task.WhenAll(recentCall, biosCall, filedCall, beatCall);
        var recent = Arr(await recentCall, "messages")
            .Where(m => Meta(m, "kind") is not ("ack" or "ack-note")).Select(ToPost).ToList();
        var john = recent.FirstOrDefault(p => p.Author == "john");
        var bios = (await biosCall).Where(t => t.Subject.StartsWith("bio: "))
            .GroupBy(t => t.Subject[5..].Trim()).ToDictionary(g => g.Key, g => g.First().Id);
        var filed = (await filedCall).FirstOrDefault();
        var beat = await beatCall;
        var concierge = Obj(beat, "concierge");
        var sessions = Obj(beat, "sessions");
        var slack = beat.TryGetProperty("slack", out var sl) && sl.ValueKind == JsonValueKind.Object ? sl : default;
        var usage = Obj(beat, "usage");
        var relay =slack.ValueKind == JsonValueKind.Object && slack.TryGetProperty("last_relay", out var r) && r.ValueKind == JsonValueKind.Object
            ? new Post("john", Ts(r, "ts"), Int(r, "thread_id"), "", "question") : null;
        var prs = Arr(beat, "prs").Select(p => new PrRow(Str(p, "repo") ?? "", Int(p, "number"), Str(p, "title") ?? "",
            Str(p, "url") ?? "", Str(p, "state") ?? "open", Str(p, "requested_by") ?? "", Str(p, "checked_ts") is null ? null : Ts(p, "checked_ts"),
            Str(p, "last_error"), Str(p, "triage"), Int(p, "thread_id") is var t and > 0 ? t : null, Str(p, "source") == "github-scan"));
        var swarm = Arr(concierge, "members").Select(m => new SwarmMember(Str(m, "identity") ?? "", Str(m, "task") ?? "",
            Int(m, "work_id") is var w and > 0 ? w : null));
        var held = Arr(concierge, "held").Where(h => h.ValueKind == JsonValueKind.Number).Select(h => h.GetInt32()).FirstOrDefault();
        return new([.. prs], [.. recent.Take(5)], [.. recent.Where(p => p.Author != "john" && p.Ts > DateTimeOffset.Now.AddDays(-1)).DistinctBy(p => p.Author)],
            bios, john, recent.TakeWhile(p => p.Author != "john").Count(p => p.Kind != "read-receipt"), filed,
            concierge.TryGetProperty("on", out var on) && on.ValueKind == JsonValueKind.True, held > 0 ? held : null, [.. swarm],
            slack.ValueKind == JsonValueKind.Object ? Ts(slack, "ts") : null, slack.ValueKind == JsonValueKind.Object && Int(slack, "poll_s") is var poll and > 0 ? poll : 15,
            relay, [], [.. Arr(usage, "lines").Select(l => l.GetString() ?? "")], Str(usage, "summary") ?? "",
            Int(sessions, "running"), Int(sessions, "max"), ToBudget(Obj(beat, "governor")), [.. Arr(beat, "goals").Select(ToGoal)],
            Obj(beat, "vault") is { ValueKind: JsonValueKind.Object } v && Str(v, "ts") is not null
                ? new VaultHealth(Ts(v, "ts"), v.TryGetProperty("pushed", out var pu) && pu.ValueKind == JsonValueKind.True, v.TryGetProperty("unpushed", out var un) && un.ValueKind == JsonValueKind.Number ? un.GetInt32() : null, Str(v, "error")) : null);
    }

    static Budget ToBudget(JsonElement g)
    {
        var caps = g.TryGetProperty("caps", out var c) ? c : default;
        int Cap(string name) => caps.ValueKind == JsonValueKind.Object ? Int(caps, name) : 0;
        var mode = g.TryGetProperty("enforcing", out var e) && e.ValueKind is JsonValueKind.True or JsonValueKind.False ? (e.GetBoolean() ? "enforcing" : "advisory")
            : g.TryGetProperty("advisory", out var a) && a.ValueKind is JsonValueKind.True or JsonValueKind.False ? (a.GetBoolean() ? "advisory" : "enforcing") : null;
        return new(Int(g, "samples"), Num(g, "remaining") ?? 0, Num(g, "reset_in_hours") ?? 0, Num(g, "projected_end_pct") ?? 0, Cap("total_sessions"),
            Cap("swarms"), Cap("members_per_swarm"), Str(g, "reason") ?? "", [.. Arr(g, "series").Where(v => v.ValueKind == JsonValueKind.Number).Select(v => v.GetDouble())], mode,
            g.TryGetProperty("pool", out var p) && p.ValueKind == JsonValueKind.Object ? Str(p, "summary") ?? "" : "",
            Str(g, "status") ?? "", Num(g, "plan_end_pct") ?? 0, [.. Arr(g, "forecast").Where(v => v.ValueKind == JsonValueKind.Number).Select(v => v.GetDouble())]);
    }

    static GoalRow ToGoal(JsonElement g) => new(Str(g, "name") ?? "", Str(g, "state") ?? "draft", Str(g, "objective") ?? "", Str(g, "lead") ?? "",
        Str(g, "success"), Int(g, "experiments"), Num(g, "last_value"), Int(g, "members"), Int(g, "max_members"), Int(g, "standing") != 0);

    public async Task<IReadOnlyList<JobRow>> JobsAsync() =>
        [.. Arr(await Call("ui:job_list"), "jobs").Select(j => new JobRow(Str(j, "name") ?? "", Str(j, "folder") ?? "", Str(j, "prompt") ?? "", Str(j, "model") ?? "sonnet",
            Str(j, "at"), Str(j, "days") ?? "daily", Num(j, "every_minutes"), Int(j, "enabled") != 0,
            Str(j, "next_run_ts") is null ? null : Ts(j, "next_run_ts"), Str(j, "last_run_ts") is null ? null : Ts(j, "last_run_ts"), Str(j, "last_status"), Int(j, "runs")))];

    public async Task<IReadOnlyList<Slot>> SlotsAsync() =>
        [.. Arr(await Call("ui:slot_list"), "slots").Select(s => new Slot(Int(s, "n"), Str(s, "goal")))];

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
        catch (InvalidOperationException e) when (e.Message.Contains("no such goal", StringComparison.OrdinalIgnoreCase))
        {
            return null; // any other core error is a failure, not a missing goal
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

    public async Task<IReadOnlyList<FolderPick>> FoldersAsync() =>
        [.. (await Call("ui:folders")).GetProperty("folders").EnumerateArray().Select(f => new FolderPick(Str(f, "path") ?? "",
            [.. Arr(f, "who").Select(w => w.GetString() ?? "")], Int(f, "uses"), Ts(f, "last")))];

    public async Task<DictationState> DictateAsync(string action)
    {
        // The core answers a plugin or microphone failure as {"state":"done","error":"..."}: that is a state to show, not a call that failed.
        var r = JsonDocument.Parse(await core.Call("ui:dictate", JsonSerializer.SerializeToElement(new { action }))).RootElement;
        if (Str(r, "state") is null && Str(r, "error") is { } failed)
            throw new InvalidOperationException(failed);
        return new(Str(r, "state") ?? "done", Str(r, "text") ?? "", Str(r, "error"), Num(r, "progress") ?? 0);
    }

    public void Dispose()
    {
        disposed = true;
        core.Dispose();
    }
}
