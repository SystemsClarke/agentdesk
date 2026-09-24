using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests.Parity;

/// <summary>
/// Runs one scenario through Python's agentdesk.mcp_server (via driver.py) and through the C# AgentBoard,
/// each on its own copy of a board seeded by Python, and asserts the documents and the final rows are identical.
/// Nothing is ignored: the clock is a shared counter, so timestamps and ids are deterministic on both sides.
/// Never touches the live board: every database lives under %TEMP%.
/// </summary>
static class Harness
{
    public const int ScenarioTick = 1000;
    static readonly string Repo = FindRepo(AppContext.BaseDirectory);

    static string FindRepo(string dir) => File.Exists(Path.Combine(dir, "agentdesk", "db.py"))
        ? dir : FindRepo(Path.GetDirectoryName(dir) ?? throw new DirectoryNotFoundException("the AgentDesk repo (agentdesk/db.py)"));
    static readonly string[] Tables = ["threads", "messages", "presence", "acks", "receipts", "pull_requests", "work_events", "handoffs", "sessions", "deliveries"];

    /// <summary>Who calls. alpha/beta are derived session names; crew is stamped AGENTDESK_AUTHOR=builder; bare has no harness or session.</summary>
    static readonly JsonObject Callers = JsonSerializer.SerializeToNode(new
    {
        alpha = new { session = "a1b2c3d4-0000", harness = "claude-code", cwd = "proj-alpha" },
        beta = new { session = "Be-7a-99", harness = "claude-code", cwd = "proj-beta" },
        crew = new { session = "c0ffee", harness = "claude-code", cwd = "crew", env_author = "builder" },
        bare = new { cwd = "proj-bare" },
    })!.AsObject();

    public static JsonObject Step(string op, string caller, object? args = null) =>
        new() { ["op"] = op, ["caller"] = caller, ["args"] = JsonSerializer.SerializeToNode(args ?? new { }) };
    public static JsonObject JohnReply(long threadId, string body) => Step("john_reply", "", new { thread_id = threadId, body });
    public static JsonObject TorchDue(string name) => Step("torch_due", "", new { name });

    /// <summary>The history every scenario starts from, written by Python alone: threads 1-4, a settled question with an ack owed to beta, a torch due for builder.</summary>
    static readonly Lazy<string> Seed = new(() =>
    {
        var root = Scratch();
        RunPython(root, Path.Combine(root, "seed"), 0, true, [
            Step("post_message", "alpha", new { channel = "discussion", subject = "hello board", body = "Deploying build-01.vispero.local tonight. cc @builder" }),
            Step("post_message", "crew", new { channel = "discussion", subject = "bio: builder", body = "I build things." }),
            Step("ask_human", "beta", new { subject = "Old deploy question", body = "May I deploy?" }),
            JohnReply(3, "Yes."),
            Step("post_work", "bare", new { subject = "Pre-existing job", body = "do it", claim = "anyone" }),
            TorchDue("builder"),
        ]);
        return DbIn(Path.Combine(root, "seed"));
    });

    public sealed record Outcome(List<JsonNode?> Python, List<string> CSharp, string PythonDb, string CSharpDb);

    public static Outcome Run(params JsonObject[] steps)
    {
        var root = Scratch();
        var (pyHome, csDb) = (Path.Combine(root, "py"), Path.Combine(root, "cs", "agentdesk.db"));
        CopyDb(Seed.Value, DbIn(pyHome));
        CopyDb(Seed.Value, csDb);
        var py = RunPython(root, pyHome, ScenarioTick, false, steps);

        var clock = Clock(ScenarioTick);
        var store = new BoardStore(csDb, clock);
        var wait = py["wait"]!.AsArray();
        var board = new AgentBoard(store, new FakePlugins(csDb), $"\"{wait[0]}\" \"{wait[1]}\" {{0}}");
        var cs = new List<string>();
        var pyResults = py["results"]!.AsArray().ToList();
        for (int i = 0; i < steps.Length; i++)
        {
            var (op, a) = ((string)steps[i]["op"]!, steps[i]["args"]!.AsObject());
            if (op == "john_reply") { JohnReplyCs(store, (long)a["thread_id"]!, (string)a["body"]!); cs.Add(""); continue; }
            if (op == "torch_due") { TorchDueCs(store, (string)a["name"]!); cs.Add(""); continue; }
            var name = (string)steps[i]["caller"]!;
            var c = Callers[name]!;
            var caller = new Caller((string?)c["session"], (string?)c["env_author"], (string)py["cwds"]![name]!, (string?)c["harness"], (int)py["pid"]!);
            cs.Add(Call(board, op, a, caller).GetAwaiter().GetResult());
            if ((string?)pyResults[i]?["text"] is { } text)
                Assert.True(Canon(text) == Canon(cs[i]), $"step {i} {op} differs\n--- python\n{text}\n--- c#\n{cs[i]}");
            else
                Assert.True(steps[i]["python_raises"] is not null, $"step {i} {op}: python raised {pyResults[i]}");
        }
        if (steps.All(s => s["python_raises"] is null))
            Assert.Equal(Dump(DbIn(pyHome)), Dump(csDb));
        return new(pyResults, cs, DbIn(pyHome), csDb);
    }

    /// <summary>The same document, whitespace and escaping aside; key order still counts. Non-JSON text (formatting_help) compares as is.</summary>
    static string Canon(string text)
    {
        try { return JsonNode.Parse(text)!.ToJsonString(Wire.Indented); }
        catch (JsonException) { return text; }
    }

    public static string Dump(string db)
    {
        using var d = new BoardStore(db).Open();
        var sb = new StringBuilder();
        foreach (var t in Tables) sb.AppendLine($"{t}: {BoardDb.Arr(d.Rows($"SELECT * FROM {t} ORDER BY rowid")).ToJsonString(Wire.Indented)}");
        return sb.ToString();
    }

    static Task<string> Call(IAgentBoard b, string op, JsonObject a, Caller c)
    {
        string? S(string k) => (string?)a[k];
        int? I(string k) => (int?)a[k];
        bool? B(string k) => (bool?)a[k];
        return op switch
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
            _ => throw new ArgumentException(op),
        };
    }

    /// <summary>driver.py's john_reply, step for step (same statements, same clock ticks).</summary>
    static void JohnReplyCs(BoardStore store, long tid, string body)
    {
        using var db = store.Open();
        var t = db.Rows("SELECT channel, status FROM threads WHERE id=$tid", ("tid", tid))[0];
        var mid = db.Reply(tid, "john", BoardDb.Human, body);
        if ((string)t["channel"]! == "question" && (string)t["status"]! == "open")
            db.Exec("UPDATE threads SET status='answered', updated_ts=$ts WHERE id=$tid", ("ts", db.NowIso()), ("tid", tid));
        if ((string)t["channel"]! == "question" && db.Scalar("SELECT author_kind FROM messages WHERE thread_id=$tid ORDER BY id LIMIT 1", ("tid", tid)) as string == BoardDb.Agent)
            db.Exec("INSERT OR IGNORE INTO acks (message_id, thread_id, agent, state, created_ts) SELECT $mid, id, opened_by, 'pending', $ts FROM threads WHERE id=$tid",
                ("mid", mid), ("tid", tid), ("ts", db.NowIso()));
    }

    static void TorchDueCs(BoardStore store, string name)
    {
        using var db = store.Open();
        db.Exec("INSERT INTO handoffs (name, body, path, updated_ts, updated_by, torch_due) VALUES ($n, '', NULL, $ts, $n, 1) ON CONFLICT(name) DO UPDATE SET torch_due=excluded.torch_due",
            ("n", name), ("ts", db.NowIso()));
    }

    static Func<DateTimeOffset> Clock(int start)
    {
        var tick = start;
        return () => new DateTimeOffset(2026, 1, 1, 0, 0, 0, TimeSpan.Zero).AddSeconds(tick++);
    }

    static JsonObject RunPython(string root, string localAppData, int tick, bool init, JsonObject[] steps)
    {
        var callers = Callers.DeepClone().AsObject();
        foreach (var (_, c) in callers) c!["cwd"] = Path.Combine(root, (string)c["cwd"]!);
        var (specPath, outPath) = (Path.Combine(root, $"spec{tick}.json"), Path.Combine(root, $"out{tick}.json"));
        File.WriteAllText(specPath, new JsonObject
        {
            ["repo"] = Repo, ["scratch"] = root, ["tick"] = tick, ["init"] = init, ["callers"] = callers,
            ["steps"] = new JsonArray([.. steps.Select(s => s.DeepClone())]),
        }.ToJsonString());
        var psi = new ProcessStartInfo(Path.Combine(Repo, ".venv", "Scripts", "python.exe")) { RedirectStandardError = true, RedirectStandardOutput = true };
        foreach (var arg in new[] { DriverPy(), specPath, outPath }) psi.ArgumentList.Add(arg);
        psi.Environment["LOCALAPPDATA"] = localAppData;
        using var p = Process.Start(psi)!;
        var err = p.StandardError.ReadToEndAsync();
        p.StandardOutput.ReadToEnd();
        p.WaitForExit();
        Assert.True(p.ExitCode == 0, "driver.py failed:\n" + err.Result);
        return JsonNode.Parse(File.ReadAllText(outPath))!.AsObject();
    }

    static string DriverPy([CallerFilePath] string self = "") => Path.Combine(Path.GetDirectoryName(self)!, "driver.py");
    static string DbIn(string localAppData) => Path.Combine(localAppData, "AgentDesk", "agentdesk.db");
    static string Scratch() => Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "agentdesk-parity", Guid.NewGuid().ToString("N")[..8])).FullName;

    static void CopyDb(string from, string to)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(to)!);
        File.Copy(from, to);
    }

    /// <summary>The C# twin of driver.py's fake vault: same answers, same failures.</summary>
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
            // "vault.search" returns what Python's search_vault tool builds: hits, bodies inlined for hits[:full].
            var (q, k, full) = ((string)args["query"]!, (int)args["k"]!, (int)args["full"]!);
            if (q == "offline") return Result(new { error = "Ollama is not reachable" });
            var scores = new[] { "1.0", "0.5", "0.3333" };
            var hits = Enumerable.Range(0, Math.Clamp(k, 0, 3)).Select(i => JsonNode.Parse(
                $$"""{"path": "notes/{{q}}-{{i}}.md", "score": {{scores[i]}}, "type": "note", "summary": "hit {{i}} for {{q}} ✓"}""")!.AsObject()).ToList();
            foreach (var h in hits.Take(full >= 0 ? full : Math.Max(0, hits.Count + full))) h["body"] = $"body of {h["path"]}";
            return Task.FromResult(JsonDocument.Parse(new JsonObject { ["hits"] = new JsonArray([.. hits]) }.ToJsonString()).RootElement);
        }

        static Task<JsonElement> Result(object o) => Task.FromResult(JsonSerializer.SerializeToElement(o));
    }
}
