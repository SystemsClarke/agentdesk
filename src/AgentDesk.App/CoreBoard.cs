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
        var worker = beat.GetProperty("worker");
        var slack = beat.GetProperty("slack") is { ValueKind: JsonValueKind.Object } s ? s : default;
        var usage = beat.GetProperty("usage");
        var relay =slack.ValueKind == JsonValueKind.Object && slack.TryGetProperty("last_relay", out var r) && r.ValueKind == JsonValueKind.Object
            ? new Post("john", Ts(r, "ts"), Int(r, "thread_id"), "", "question") : null;
        var prs = beat.GetProperty("prs").EnumerateArray().Select(p => new PrRow(Str(p, "repo") ?? "", Int(p, "number"), Str(p, "title") ?? "",
            Str(p, "url") ?? "", Str(p, "state") ?? "open", Str(p, "requested_by") ?? "", Str(p, "checked_ts") is null ? null : Ts(p, "checked_ts"),
            Str(p, "last_error"), Str(p, "triage"), Int(p, "thread_id") is var t and > 0 ? t : null, Str(p, "source") == "github-scan"));
        return new([.. prs], [.. recent.Take(5)], [.. recent.Where(p => p.Author != "john" && p.Ts > DateTimeOffset.Now.AddDays(-1)).DistinctBy(p => p.Author)],
            bios, john, recent.TakeWhile(p => p.Author != "john").Count(p => p.Kind != "read-receipt"), filed,
            worker.GetProperty("running").GetBoolean(), Int(worker, "held") is var held and > 0 ? held : null,
            [.. worker.GetProperty("events").EnumerateArray().Select(e => new WorkEvent(Ts(e, "ts"), Str(e, "kind") ?? "", Str(e, "body") ?? ""))],
            slack.ValueKind == JsonValueKind.Object ? Ts(slack, "ts") : null, slack.ValueKind == JsonValueKind.Object && Int(slack, "poll_s") is var poll and > 0 ? poll : 15,
            relay, [], [.. usage.GetProperty("lines").EnumerateArray().Select(l => l.GetString()!)], Str(usage, "summary") ?? "", [], "");
    }

    public async Task<string?> ActAsync(string request, object? args = null) => Str(await Call(request, args), "said");

    public void Dispose() => core.Dispose();
}
