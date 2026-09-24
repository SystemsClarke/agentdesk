using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests.Parity;

/// <summary>
/// Runs one scenario through the C# AgentBoard and compares every step's document, and the final rows of every
/// table, with golden/&lt;scenario&gt;.json: what Python's agentdesk.mcp_server produced for the same steps from the
/// same board (golden/seed.db, written by Python) before it was deleted. The clock is a tick counter (one second per
/// timestamp, starting where the recording did), so ids and timestamps match exactly; nothing is ignored.
/// In the recording, paths were normalised to C:\parity, session pids to 4242 and the wait command to "wait {0}".
/// Set AGENTDESK_BLESS=1 to rewrite the goldens from C# after an intended change. Every database lives under %TEMP%.
/// </summary>
static class Harness
{
    static readonly string Golden = Path.Combine(Here(), "golden");
    static readonly string[] Tables = ["threads", "messages", "presence", "acks", "receipts", "pull_requests", "work_events", "handoffs", "sessions", "deliveries"];

    /// <summary>alpha/beta post under derived session names; crew is stamped AGENTDESK_AUTHOR=builder; bare has no harness or session.</summary>
    static readonly Dictionary<string, Caller> Callers = new()
    {
        ["alpha"] = new("a1b2c3d4-0000", null, @"C:\parity\proj-alpha", "claude-code", 4242),
        ["beta"] = new("Be-7a-99", null, @"C:\parity\proj-beta", "claude-code", 4242),
        ["crew"] = new("c0ffee", "builder", @"C:\parity\crew", "claude-code", 4242),
        ["bare"] = new(null, null, @"C:\parity\proj-bare", null, 4242),
    };

    public static JsonObject Step(string op, string caller, object? args = null) =>
        new() { ["op"] = op, ["caller"] = caller, ["args"] = JsonSerializer.SerializeToNode(args ?? new { }) };
    public static JsonObject JohnReply(long threadId, string body) => Step("john_reply", "", new { thread_id = threadId, body });

    public sealed record Outcome(List<string> Results, string Db);

    public static Outcome Run(JsonObject[] steps, [CallerMemberName] string scenario = "")
    {
        var db = Path.Combine(Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "agentdesk-parity", Guid.NewGuid().ToString("N")[..8])).FullName, "agentdesk.db");
        File.Copy(Path.Combine(Golden, "seed.db"), db);
        var store = new BoardStore(db, Clock(1000));
        var board = new AgentBoard(store, new FakePlugins(db), "wait {0}");
        var results = steps.Select(s => (string)s["op"]! == "john_reply" ? JohnReplyCs(store, s["args"]!) : Call(board, s, Callers[(string)s["caller"]!]).GetAwaiter().GetResult()).ToList();
        var actual = new JsonObject
        {
            ["steps"] = new JsonArray([.. results.Select(r => r.Length == 0 ? null : Parse(r))]),
            ["tables"] = Dump(db),
        };
        var path = Path.Combine(Golden, scenario + ".json");
        if (Environment.GetEnvironmentVariable("AGENTDESK_BLESS") == "1") File.WriteAllText(path, actual.ToJsonString(Wire.Indented) + "\n");
        var golden = JsonNode.Parse(File.ReadAllText(path))!;
        for (int i = 0; i < steps.Length; i++)
            Assert.True(Text(golden["steps"]![i]) == Text(actual["steps"]![i]),
                $"step {i} {steps[i]["op"]} differs\n--- golden\n{Text(golden["steps"]![i])}\n--- c#\n{Text(actual["steps"]![i])}");
        Assert.Equal(Text(golden["tables"]), Text(actual["tables"]));
        return new(results, db);
    }

    /// <summary>Key order counts, so documents compare as canonical text; formatting_help is plain text, kept as a JSON string.</summary>
    static string Text(JsonNode? n) => n?.ToJsonString(Wire.Indented) ?? "null";

    static JsonNode Parse(string text)
    {
        try { return JsonNode.Parse(text)!; }
        catch (JsonException) { return JsonValue.Create(text); }
    }

    static JsonObject Dump(string db)
    {
        using var d = new BoardStore(db).Open();
        return new(Tables.Select(t => KeyValuePair.Create(t, (JsonNode?)BoardDb.Arr(d.Rows($"SELECT * FROM {t} ORDER BY rowid")))));
    }

    static Task<string> Call(IAgentBoard b, JsonObject step, Caller c)
    {
        var a = step["args"]!.AsObject();
        string? S(string k) => (string?)a[k];
        int? I(string k) => (int?)a[k];
        bool? B(string k) => (bool?)a[k];
        return (string)step["op"]! switch
        {
            "formatting_help" => b.FormattingHelp(),
            "post_message" => b.PostMessage(c, S("channel")!, S("subject")!, S("body")!, S("author"), I("thread_id")),
            "ask_human" => b.AskHuman(c, S("subject")!, S("body")!, S("author"),
                a["meta"] is JsonObject m ? m.ToDictionary(kv => kv.Key, kv => (object?)JsonDocument.Parse(kv.Value!.ToJsonString()).RootElement) : null),
            "list_threads" => b.ListThreads(S("channel"), S("status"), I("limit") ?? 50, B("include_archived") ?? true),
            "read_thread" => b.ReadThread(c, I("thread_id")!.Value, S("author")),
            "open_questions" => b.OpenQuestions(c, B("include_archived") ?? false, S("author")),
            "pass_the_torch" => b.PassTheTorch(c, S("handoff")!, S("author")),
            "answer_thread" => b.AnswerThread(c, I("thread_id")!.Value, S("body")!, S("author")),
            "search_messages" => b.SearchMessages(S("query")!, I("limit") ?? 20),
            "search_vault" => b.SearchVault(S("query")!, I("k") ?? 8, I("full") ?? 0),
            "recent_messages" => b.RecentMessages(I("limit") ?? 30),
            "list_mentions" => b.ListMentions(c, S("name"), I("limit") ?? 50),
            "post_work" => b.PostWork(c, S("subject")!, S("body")!, S("author"), S("claim") ?? "auto"),
            "list_work" => b.ListWork(S("status"), I("limit") ?? 100),
            "claim_work" => b.ClaimWork(c, I("thread_id")!.Value, S("author")),
            "complete_work" => b.CompleteWork(c, I("thread_id")!.Value, S("note")!, S("author")),
            "request_merge" => b.RequestMerge(c, S("pr_url")!, I("thread_id"), S("note"), S("author")),
            var op => throw new ArgumentException(op),
        };
    }

    /// <summary>What the window did when John answered (app.py post_reply, then the watcher queueing the ack), statement for statement.</summary>
    static string JohnReplyCs(BoardStore store, JsonNode args)
    {
        var (tid, body) = ((long)args["thread_id"]!, (string)args["body"]!);
        using var db = store.Open();
        var t = db.Rows("SELECT channel, status FROM threads WHERE id=$tid", ("tid", tid))[0];
        var mid = db.Reply(tid, "john", BoardDb.Human, body);
        if ((string)t["channel"]! == "question" && (string)t["status"]! == "open")
            db.Exec("UPDATE threads SET status='answered', updated_ts=$ts WHERE id=$tid", ("ts", db.NowIso()), ("tid", tid));
        if ((string)t["channel"]! == "question" && db.Scalar("SELECT author_kind FROM messages WHERE thread_id=$tid ORDER BY id LIMIT 1", ("tid", tid)) as string == BoardDb.Agent)
            db.Exec("INSERT OR IGNORE INTO acks (message_id, thread_id, agent, state, created_ts) SELECT $mid, id, opened_by, 'pending', $ts FROM threads WHERE id=$tid",
                ("mid", mid), ("tid", tid), ("ts", db.NowIso()));
        return "";
    }

    static Func<DateTimeOffset> Clock(int start)
    {
        var tick = start;
        return () => new DateTimeOffset(2026, 1, 1, 0, 0, 0, TimeSpan.Zero).AddSeconds(tick++);
    }

    static string Here([CallerFilePath] string self = "") => Path.GetDirectoryName(self)!;

    /// <summary>The fake vault the recording used: the same answers and the same failures.</summary>
    sealed class FakePlugins(string db) : IPythonPlugins
    {
        public Task<JsonElement> Call(string method, JsonObject args)
        {
            if (method == "vault.mirror_thread")
            {
                var tid = (long)args["thread_id"]!;
                using var d = new BoardStore(db).Open();
                var subject = (string)d.Scalar("SELECT subject FROM threads WHERE id=$t", ("t", tid))!;
                if (subject.Contains("FAIL")) throw new PythonPluginException($"RuntimeError('vault down: {subject}')");
                return Result(new { thread_id = tid, status = "mirrored", note = $"notes/{subject}.md", reasons = Array.Empty<string>() });
            }
            // "vault.search" returns the whole search_vault document: hits, with bodies inlined for hits[:full].
            var (q, k, full) = ((string)args["query"]!, (int)args["k"]!, (int)args["full"]!);
            if (q == "offline") return Result(new { error = "Ollama is not reachable" });
            var scores = new[] { "1.0", "0.5", "0.3333" };
            var hits = Enumerable.Range(0, Math.Clamp(k, 0, 3)).Select(i => JsonNode.Parse(
                $$"""{"path": "notes/{{q}}-{{i}}.md", "score": {{scores[i]}}, "type": "note", "summary": "hit {{i}} for {{q}} ✓"}""")!.AsObject()).ToList();
            foreach (var h in hits.Take(full >= 0 ? full : Math.Max(0, hits.Count + full))) h["body"] = $"body of {(string)h["path"]!}";
            return Task.FromResult(JsonDocument.Parse(new JsonObject { ["hits"] = new JsonArray([.. hits]) }.ToJsonString()).RootElement);
        }

        static Task<JsonElement> Result(object o) => Task.FromResult(JsonSerializer.SerializeToElement(o));
    }
}
