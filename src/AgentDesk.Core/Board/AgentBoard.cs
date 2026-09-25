using System.Diagnostics;
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using AgentDesk.Contracts;
using Microsoft.Data.Sqlite;

namespace AgentDesk.Core.Board;

/// <summary>The 17 tools of agentdesk/mcp_server.py. Each returns the tool's JSON document; parity with Python is proven in AgentDesk.Tests.</summary>
/// <param name="waitCommandTemplate">ask_human's wake_on_reply command, a format string whose {0} is the new thread's id.</param>
public sealed partial class AgentBoard(BoardStore store, IPythonPlugins plugins, string waitCommandTemplate) : IAgentBoard
{
    const string AckBody = "Acknowledged - your reply has been picked up.";
    const string Agent = BoardDb.Agent;

    [GeneratedRegex(@"^https?://(?:www\.)?github\.com/(?<owner>[A-Za-z0-9._-]+)/(?<repo>[A-Za-z0-9._-]+)/pull/(?<number>\d+)(?:[/?#].*)?$", RegexOptions.IgnoreCase)]
    private static partial Regex PrUrl();

    [GeneratedRegex(@"^SQLite Error \d+: '(.*)'\.$", RegexOptions.Singleline)]
    private static partial Regex SqliteText();

    static string Dump(JsonNode n) => n.ToJsonString(Wire.Indented);
    static Task<string> Error(string e) => Task.FromResult(Dump(new JsonObject { ["error"] = e }));
    static JsonObject Ok(params (string Key, JsonNode? Value)[] fields)
    {
        var o = new JsonObject { ["ok"] = true };
        foreach (var (k, v) in fields) o[k] = v;
        return o;
    }
    static string Who(string? author, Caller c) => Identity.Resolve(author, c);

    /// <summary>One connection per call. BoardError is Python's ValueError; SqliteException is sqlite3.Error.</summary>
    async Task<string> RunAsync(Func<BoardDb, Task<JsonNode>> body)
    {
        using var db = store.Open();
        try { return Dump(await body(db)); }
        catch (BoardError e) { return Dump(new JsonObject { ["error"] = e.Message }); }
        catch (SqliteException e) { return Dump(new JsonObject { ["error"] = "database error: " + (SqliteText().Match(e.Message) is { Success: true } m ? m.Groups[1].Value : e.Message) }); }
    }

    Task<string> Run(Func<BoardDb, JsonNode> body) => RunAsync(db => Task.FromResult(body(db)));

    /// <summary>After every write: note which session this author writes from, and post the acks it owes. Never fails the tool.</summary>
    static void DeliverAcks(BoardDb db, string author, Caller c)
    {
        try
        {
            db.RecordSession(author, c.SessionId, c.Cwd, c.Pid);
            if (db.DeliverPendingAcks(author, AckBody) is { Count: > 0 } done)
                Log.Info($"agent {author} acknowledged thread(s) {string.Join(", ", done.Select(t => "#" + t))}");
        }
        catch (Exception e) { Log.Warn($"ack delivery failed for {author}: {e}"); }
    }

    static void Receipt(BoardDb db, long threadId, string agent, string body)
    {
        try { db.PostReadReceipt(threadId, agent, body); }
        catch (Exception e) { Log.Warn($"read receipt failed for {agent} on thread #{threadId}: {e}"); }
    }

    async Task<JsonNode?> Mirror(long threadId)
    {
        try { return JsonNode.Parse((await plugins.Call("vault.mirror_thread", new JsonObject { ["thread_id"] = threadId })).GetRawText()); }
        catch (Exception e)
        {
            Log.Warn($"vault mirror failed for thread {threadId}: {e}");
            return new JsonObject { ["thread_id"] = threadId, ["status"] = "error", ["reason"] = e is PythonPluginException ? e.Message : $"{e.GetType().Name}({Py.Repr(e.Message)})" };
        }
    }

    public Task<string> FormattingHelp() => Task.FromResult(FormattingHelpText);

    public Task<string> PostMessage(Caller caller, string channel, string subject, string body, string? author, int? threadId)
    {
        if (!BoardDb.Channels.Contains(channel)) return Error($"channel must be one of ['question', 'discussion', 'wiki', 'work'], got {Py.Repr(channel)}");
        var who = Who(author, caller);
        return RunAsync(async db =>
        {
            var tid = db.StartThread(channel, subject, who, Agent, body, threadId: threadId);
            DeliverAcks(db, who, caller);
            var result = Ok(("thread_id", tid));
            if (channel == "wiki" && threadId is null) result["vault"] = await Mirror(tid);  // a new wiki entry is mirrored into the memory vault
            return result;
        });
    }

    public Task<string> AskHuman(Caller caller, string subject, string body, string? author, Dictionary<string, object?>? meta)
    {
        var who = Who(author, caller);
        var m = new JsonObject();
        foreach (var (k, v) in meta ?? [])
            if (k != "kind")  // a caller-set kind could hide the question from John
                m[k] = v is JsonElement e ? JsonNode.Parse(e.GetRawText()) : v is null ? null : JsonValue.Create(v.ToString());
        return Run(db =>
        {
            var tid = db.StartThread("question", subject, who, Agent, body, m);
            DeliverAcks(db, who, caller);
            return Ok(("thread_id", tid),
                ("wake_on_reply", new JsonObject { ["command"] = string.Format(CultureInfo.InvariantCulture, waitCommandTemplate, tid), ["run_in_background"] = true }),
                ("next", "Start wake_on_reply.command now with Bash run_in_background=true. It exits the moment John answers, which wakes you with his reply. "
                    + "Then carry on with other work or end your turn; do not poll."));
        });
    }

    public Task<string> ListThreads(string? channel, string? status, int limit, bool includeArchived) =>
        Run(db => new JsonObject { ["threads"] = BoardDb.Arr(db.ListThreads(channel, status, limit, includeArchived)) });

    public Task<string> ReadThread(Caller caller, int threadId, string? author)
    {
        var who = Who(author, caller);
        return Run(db =>
        {
            var data = db.GetThread(threadId);
            Receipt(db, threadId, who, $"{who} picked this thread up.");  // only after a successful read
            return data;
        });
    }

    // ---- AgentDesk's own window (docs/ui-api.md): John's actions, and a read that leaves no trace

    public Task<string> PeekThread(int threadId) => Run(db => db.GetThread(threadId));

    public Task<string> JohnReplies(int threadId, string body) =>
        string.IsNullOrWhiteSpace(body) ? Error("body must not be blank") : Run(db => Ok(("message_id", db.JohnReplies(threadId, body))));

    public Task<string> CloseQuestion(int threadId) => Run(db => Ok(("closed", db.CloseQuestion(threadId))));

    public Task<string> JohnPosts(string channel, string subject, string body) => string.IsNullOrWhiteSpace(body) ? Error("body must not be blank")
        : Run(db => Ok(("thread_id", db.StartThread(channel, string.IsNullOrWhiteSpace(subject) ? "(no subject)" : subject, BoardDb.John, BoardDb.Human, body))));

    public Task<string> Unarchive(int threadId) => Run(db => Ok(("unarchived", db.Unarchive(threadId))));

    /// <summary>The Slack bridge's heartbeat, the usage meter, the governor and the merge list: ui:status without the parts the
    /// core adds itself (Program.cs: the Concierge, sessions, the bridge's supervision, goals).</summary>
    public Task<string> Heartbeats(string data) => Run(db => new JsonObject
    {
        ["slack"] = Load(Path.Combine(data, "slack_bridge.state")),
        ["usage"] = Usage.Report(Path.Combine(data, "claude_usage.json"), DateTimeOffset.UtcNow), ["governor"] = Governor.Report(db, data, DateTimeOffset.UtcNow),
        ["prs"] = BoardDb.Arr(db.Rows("SELECT * FROM pull_requests ORDER BY id DESC LIMIT 200")),
    });

    internal static JsonObject? Load(string file) { try { return JsonNode.Parse(File.ReadAllText(file)) as JsonObject; } catch (Exception) { return null; } }

    internal static bool Alive(int pid)
    {
        try { using var p = Process.GetProcessById(pid); return !p.HasExited; }
        catch (Exception) { return false; }
    }

    public Task<string> OpenQuestions(Caller caller, bool includeArchived, string? author) => Run(db =>
    {
        var result = new JsonObject { ["open_questions"] = BoardDb.Arr(db.OpenQuestions(includeArchived)) };
        if (!string.IsNullOrEmpty(author)) result["torch_due"] = db.TorchDue(Who(author, caller));
        return result;
    });

    public Task<string> PassTheTorch(Caller caller, string handoff, string? author)
    {
        var who = Who(author, caller);
        return Run(db =>
        {
            var ts = db.PassTheTorch(who, handoff);
            var note = $"**Handoff recorded** ({ts}).\n\n{handoff}";
            var meta = new JsonObject { ["kind"] = "handoff" };
            // Point the bio thread at the latest handoff, starting one if the agent never posted a bio.
            var bio = db.BioThread($"bio: {who}");
            if (bio is { } id) db.Reply(id, who, Agent, note, meta: meta);
            else bio = db.StartThread("discussion", $"bio: {who}", who, Agent, note, meta);
            DeliverAcks(db, who, caller);
            return Ok(("name", who), ("updated_ts", ts), ("path", null), ("bio_thread_id", bio));
        });
    }

    public Task<string> AnswerThread(Caller caller, int threadId, string body, string? author)
    {
        var who = Who(author, caller);
        return Run(db =>
        {
            db.GetThread(threadId);  // a clean "no such thread" beats a foreign-key error
            var mid = db.Reply(threadId, who, Agent, body);
            DeliverAcks(db, who, caller);
            return Ok(("thread_id", threadId), ("message_id", mid));
        });
    }

    public Task<string> SearchMessages(string query, int limit) => Run(db => new JsonObject { ["results"] = BoardDb.Arr(db.Search(query, limit)) });

    public async Task<string> SearchVault(string query, int k, int full) =>
        Dump(JsonNode.Parse((await plugins.Call("vault.search", new JsonObject { ["query"] = query, ["k"] = k, ["full"] = full })).GetRawText())!);

    public Task<string> RecentMessages(int limit) => Run(db => new JsonObject { ["messages"] = BoardDb.Arr(db.Recent(limit)) });

    public Task<string> ListMentions(Caller caller, string? name, int limit)
    {
        var who = Who(name, caller);
        return Run(db => new JsonObject { ["mentions"] = BoardDb.Arr(db.ListMentions(who, limit)) });
    }

    public Task<string> PostWork(Caller caller, string subject, string body, string? author, string claim)
    {
        if (claim is not ("auto" or "anyone")) return Error($"claim must be one of ['auto', 'anyone'], got {Py.Repr(claim)}");
        var who = Who(author, caller);
        return Run(db =>
        {
            var tid = db.StartThread("work", subject, who, Agent, body, new JsonObject { ["claim"] = claim });
            DeliverAcks(db, who, caller);
            return Ok(("thread_id", tid), ("claim", claim));
        });
    }

    public Task<string> ListWork(string? status, int limit) => Run(db => new JsonObject { ["work"] = BoardDb.Arr(db.ListThreads("work", status, limit)) });

    public Task<string> ClaimWork(Caller caller, int threadId, string? author)
    {
        var who = Who(author, caller);
        return Run(db =>
        {
            var claimed = db.ClaimTask(threadId, who);
            DeliverAcks(db, who, caller);
            if (claimed) Receipt(db, threadId, who, $"{who} took this work item.");  // a failed claim leaves no receipt
            return Ok(("claimed", claimed));
        });
    }

    public Task<string> CompleteWork(Caller caller, int threadId, string note, string? author)
    {
        var who = Who(author, caller);
        return Run(db =>
        {
            var completed = db.CompleteTask(threadId, who);
            if (completed) db.Reply(threadId, who, Agent, note);
            DeliverAcks(db, who, caller);
            return Ok(("thread_id", threadId), ("completed", completed));
        });
    }

    static readonly char[] LineBreaks = ['\n', '\r', '\v', '\f', '\x1c', '\x1d', '\x1e', '\x85', (char)0x2028, (char)0x2029];

    public Task<string> RequestMerge(Caller caller, string prUrl, int? threadId, string? note, string? author)
    {
        var who = Who(author, caller);
        var m = PrUrl().Match(Py.Strip(prUrl));
        if (!m.Success) return Error($"not a GitHub pull request URL: {Py.Repr(prUrl)} (expected https://github.com/<owner>/<repo>/pull/<number>)");
        var (repo, number) = ($"{m.Groups["owner"].Value}/{m.Groups["repo"].Value}", long.Parse(m.Groups["number"].Value));
        var url = $"https://github.com/{repo}/pull/{number}";
        // The note's first line is the title until the checker asks gh. Python raises IndexError on a whitespace-only note; this falls back to the url.
        var first = Py.Strip(note);
        var title = first.Length > 0 ? first[..(first.IndexOfAny(LineBreaks) is var i and >= 0 ? i : first.Length)] : url;
        return Run(db =>
        {
            var (prId, created) = db.RegisterPr(url, repo, number, string.Concat(title.EnumerateRunes().Take(200)), who, threadId);
            if (threadId is { } tid && created)
                db.Reply(tid, who, Agent, $"Asking John to merge {repo}#{number}: {url}" + (string.IsNullOrEmpty(note) ? "" : $"\n\n{note}"), meta: new JsonObject { ["kind"] = "pr-request" });
            DeliverAcks(db, who, caller);
            return Ok(("pr_id", prId), ("url", url), ("repo", repo), ("number", number), ("created", created));
        });
    }

    static readonly string FormattingHelpText = """"
        How to format board messages. Everything here renders the same every time.

        MARKDOWN: headings, **bold**, *italic*, ~~strike~~, `code`, [links](url), bare URLs,
        bullets (nest by indenting 2 spaces), numbered lists, - [ ] / - [x] task lists, > quotes,
        --- rules, and GitHub callouts: > [!NOTE] / [!TIP] / [!IMPORTANT] / [!WARNING] / [!CAUTION].

        TABLES: ordinary pipe tables. Colons in the delimiter row align columns (|:--|--:|:-:|).
        Shown as a crisp grid image in the window; in Slack as an aligned grid, or one line per
        row when too wide for a phone. Do NOT paste pre-drawn box-character tables in code blocks.

        CODE: ```lang fences get a language label and highlighting.

        DIAGRAMS: ```mermaid with graph/flowchart TD|LR, stateDiagram-v2, sequenceDiagram, or pie.
        Drawn as box-and-arrow art in the window; in Slack, top-to-bottom sized for a phone.

        Charts: put JSON in a ```chart block. Fields common to all: "type", "title", optional "unit".
          line      {"type":"line","title":"Compile time","unit":"min","x":["09-01","09-08"],"series":{"JAWS":[41,29]},"goal":25}
          bar       {"type":"bar","title":"Builds per lane","labels":["compile","sign"],"values":[41,12]}
          progress  {"type":"progress","title":"Migration","items":[{"label":"groups","done":7,"total":20}]}
          burndown  {"type":"burndown","title":"Sprint 2609","x":["Mon","Tue","Wed"],"remaining":[30,24,19]}
          timeline  {"type":"timeline","title":"Cutover","events":[{"when":"09-01","label":"canary"}]}
          sparkline {"type":"sparkline","title":"queue depth","values":[3,5,2,8,4]}
          stat      {"type":"stat","tiles":[{"label":"green builds","value":"94%","delta":"+6%"}]}
          table     {"type":"table","columns":["run","result"],"rows":[["#45","pass"],["#46","fail"]]}

        Tips: one chart per idea; keep x labels short (dates as MM-DD); use "goal" on line charts
        and "lower_is_better" on sparklines/stat tiles where down is good. A bad spec is shown
        with its error rather than failing the message.
        """".ReplaceLineEndings("\n");
}
