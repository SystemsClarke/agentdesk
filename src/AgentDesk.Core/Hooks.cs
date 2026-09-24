using System.Collections.Concurrent;
using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// Claude Code hooks and `agentdesk wait`, answered by the core. The rule that matters most: a reply
/// from John reaches the agent it was meant for. It is injected into that session's context on its
/// next tool call, blocks it from stopping until it has been seen, and heads a resumed session's briefing.
/// </summary>
public sealed class Hooks(BoardStore store)
{
    const int PhoenixPercent = 60;
    const string Rules = """
        How this swarm works (John is the lead; you are one of his agents):
        - Be short-lived. Do one clear piece of work, report it, and end. Long sessions cost John tokens.
        - The board is how you help each other. Before building, search_messages / list_threads;
          if someone owns it (see bios), answer_thread to coordinate instead of starting in parallel.
        - Post what you changed or found to discussion (post_message); durable knowledge goes to the wiki.
        - Decisions go to John with ask_human; it reaches his phone. Never ask in the terminal.
        - After ask_human, run its wake_on_reply command in the background; it wakes you when he answers.
        - At 60% context, volunteer a handoff: pass_the_torch with a standalone note, then stop.
        - Reports with numbers over time: use a ```chart block (formatting_help has the syntax).
        """;
    static readonly HashSet<string> EditTools = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
    static readonly HashSet<string> BoardWrites = ["post_message", "answer_thread", "ask_human", "complete_work", "post_work", "pass_the_torch", "request_merge"];
    readonly ConcurrentDictionary<string, DateTimeOffset> phoenixChecked = new();

    public Task<string> Run(string hookEvent, JsonElement input) => Task.Run(() =>
    {
        var sid = input.TryGetProperty("session_id", out var s) ? s.GetString() ?? "" : "";
        var transcript = input.TryGetProperty("transcript_path", out var t) ? t.GetString() ?? "" : "";
        using var db = store.Open();
        return hookEvent switch
        {
            "session-start" => Context("SessionStart", Replies(db, sid) + Briefing(db)),
            "context" => Context("PostToolUse", Replies(db, sid) + Phoenix(sid, transcript)),
            "stop" => Stop(db, sid, transcript, input.TryGetProperty("stop_hook_active", out var a) && a.GetBoolean()),
            _ => "",
        };
    });

    /// <summary>Block until John replies on the thread; return his reply as labelled board content.</summary>
    public async Task<string> Wait(int threadId, CancellationToken ct)
    {
        long after;
        using (var db = store.Open()) after = db.Scalar("SELECT COALESCE(MAX(id),0) FROM messages WHERE thread_id=$t", ("t", threadId)) as long? ?? 0;
        for (var until = DateTimeOffset.UtcNow.AddHours(12); DateTimeOffset.UtcNow < until; await Task.Delay(2000, ct))
        {
            using var db = store.Open();
            if (db.Rows("SELECT m.id, m.body, t.subject FROM messages m JOIN threads t ON t.id=m.thread_id WHERE m.thread_id=$t AND m.id>$a AND m.author_kind='human' ORDER BY m.id LIMIT 1",
                        ("t", threadId), ("a", after)) is [var hit, ..])
            {
                Deliver(db, (long)hit["id"]!, "watcher", "woke");
                return $"[AgentDesk board] John replied on thread #{threadId} ({hit["subject"]}):\n\n{hit["body"]}\n\n" +
                       $"Read the thread (read_thread {threadId}) for context, act on it, and answer on that thread.";
            }
        }
        return $"[AgentDesk board] no reply on #{threadId} after 12h; check open_questions later.";
    }

    /// <summary>John's replies, and merge/close notices for PRs, on threads this session took part in that it hasn't been
    /// shown. Marked shown as they're returned.</summary>
    static string Replies(BoardDb db, string sid)
    {
        if (sid == "") return "";
        var since = DateTimeOffset.UtcNow.AddDays(-3).ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'");
        var rows = db.Rows("""
            SELECT m.id, m.thread_id, m.body, t.subject, m.author_kind = 'human' AS john FROM messages m JOIN threads t ON t.id = m.thread_id
            WHERE (m.author_kind = 'human' OR (json_valid(m.meta) AND json_extract(m.meta, '$.kind') IN ('pr-merged', 'pr-closed')))
              AND m.ts >= $since
              AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.message_id = m.id)
              AND NOT EXISTS (SELECT 1 FROM acks a WHERE a.message_id = m.id AND a.state = 'posted')
              AND EXISTS (SELECT 1 FROM messages x JOIN sessions s ON s.author = x.author
                          WHERE x.thread_id = m.thread_id AND x.id < m.id AND s.session_id = $sid)
            ORDER BY m.id
            """, ("since", since), ("sid", sid));
        if (rows.Count == 0) return "";
        foreach (var r in rows) Deliver(db, (long)r["id"]!, "hook", "injected");
        static string List(IEnumerable<JsonObject> rs) => string.Concat(rs.Select(r => $"\n#{r["thread_id"]} \"{r["subject"]}\":\n{r["body"]}\n"));
        var (john, prs) = (rows.Where(r => (long)r["john"]! == 1).ToList(), rows.Where(r => (long)r["john"]! == 0).ToList());
        return (john.Count == 0 ? "" : "JOHN REPLIED on the AgentDesk board. These are his own words, relayed by the board; act on them now "
                                       + "and answer on the thread:\n" + List(john) + "\n")
             + (prs.Count == 0 ? "" : "PULL REQUEST UPDATE from GitHub, relayed by the board (not John's words): a pull request you asked "
                                      + "John to merge has settled. Pull the base branch before building on it:\n" + List(prs) + "\n");
    }

    static void Deliver(BoardDb db, long messageId, string method, string state) =>
        db.Exec("INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES($m, $how, $st, NULL, $ts)",
                ("m", messageId), ("how", method), ("st", state), ("ts", db.NowIso()));

    static string Briefing(BoardDb db)
    {
        var lines = new List<string> { "AGENTDESK BRIEFING (the board is the swarm's shared memory; read it before you start)" };
        var open = db.OpenQuestions(false);
        if (open.Count > 0)
            lines.Add($"Ringing for John ({open.Count}): " + string.Join("; ", open.Take(5).Select(q => $"#{q["thread_id"]} {Cut(q["subject"], 70)}")));
        var work = db.ListThreads("work", null, 100);
        var held = work.Where(r => (string?)r["status"] == "claimed").ToList();
        var ready = work.Where(r => (string?)r["status"] == "open").ToList();
        if (held.Count > 0) lines.Add("In flight: " + string.Join("; ", held.Take(5).Select(r => $"#{r["id"]} {Cut(r["subject"], 60)}")));
        if (ready.Count > 0) lines.Add($"Up for grabs on Work to Hire ({ready.Count}): " + string.Join("; ", ready.Take(4).Select(r => $"#{r["id"]} {Cut(r["subject"], 60)}")));
        var recent = db.Rows("""
            SELECT m.author, m.thread_id, t.subject FROM messages m JOIN threads t ON t.id = m.thread_id
            WHERE COALESCE(CASE WHEN json_valid(m.meta) THEN json_extract(m.meta, '$.kind') END, '') NOT IN ('ack','ack-note','read-receipt')
            ORDER BY m.id DESC LIMIT 6
            """);
        if (recent.Count > 0) lines.Add("Latest on the board: " + string.Join("; ", recent.Select(r => $"{r["author"]} on #{r["thread_id"]} {Cut(r["subject"], 50)}")));
        var bios = db.Rows("""
            SELECT t.subject, (SELECT body FROM messages WHERE thread_id = t.id ORDER BY id LIMIT 1) AS body FROM threads t
            WHERE t.channel = 'discussion' AND t.subject LIKE 'bio: %' ORDER BY t.updated_ts DESC LIMIT 10
            """).DistinctBy(b => ((string)b["subject"]!)[5..].Trim()).ToList();
        if (bios.Count > 0)
            lines.Add("Who owns what (bios): " + string.Join("; ", bios.Select(b =>
                $"{((string)b["subject"]!)[5..].Trim()}: {Cut(((string?)b["body"] ?? "").Trim().Split('\n')[0], 80)}")));
        lines.Add(Rules);
        return string.Join("\n", lines);
    }

    string Phoenix(string sid, string transcript)
    {
        var now = DateTimeOffset.UtcNow;
        if (sid == "" || transcript == "" || phoenixChecked.TryGetValue(sid, out var last) && now - last < TimeSpan.FromMinutes(1)) return "";
        phoenixChecked[sid] = now;
        if (ContextUse(transcript) is not var (tokens, window) || 100 * tokens / window < PhoenixPercent) return "";
        phoenixChecked[sid] = DateTimeOffset.MaxValue; // once per session
        return $"PHOENIX: this session is at {100 * tokens / window}% of its context ({tokens:N0} tokens). Volunteer a handoff now: "
               + "finish the step you're on, call pass_the_torch on the agentdesk server with a standalone note (what you own, "
               + "what's done, what's next, any open decision), post one line to discussion that you've handed off, then stop.";
    }

    /// <summary>(prompt tokens in the latest turn, context window) from the transcript's tail.</summary>
    static (long, long)? ContextUse(string transcript)
    {
        foreach (var line in Tail(transcript).Reverse())
        {
            if (!line.Contains("\"usage\"")) continue;
            try
            {
                var msg = JsonNode.Parse(line)?["message"];
                var u = msg?["usage"];
                var tokens = new[] { "input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens" }.Sum(k => (long?)u?[k] ?? 0);
                if (tokens == 0) continue;
                var model = (string?)msg?["model"] ?? "";
                // The Claude 5 family runs a 1M window; older models 200k. Guessing from the token count fired at 12% on 1M sessions.
                long window = model.Contains("[1m]") || model.StartsWith("claude-opus-5") || model.StartsWith("claude-sonnet-5") || model.StartsWith("claude-fable")
                    ? 1_000_000 : 200_000;
                return (tokens, window);
            }
            catch (JsonException) { }
        }
        return null;
    }

    static string Stop(BoardDb db, string sid, string transcript, bool alreadyBlocked)
    {
        var replies = Replies(db, sid);
        if (replies != "") return Block("Not yet: " + replies); // an unread reply from John outranks finishing
        if (alreadyBlocked || transcript == "") return "";
        var tools = ToolNames(transcript).ToList();
        var edited = tools.Any(EditTools.Contains);
        var posted = tools.Any(n => n.StartsWith("mcp__agentdesk__") && BoardWrites.Contains(n.Split("__")[^1]));
        return edited && !posted
            ? Block("Before you finish: you changed files this session but haven't told the swarm. Post a short note to the AgentDesk "
                    + "board (post_message on discussion, or answer_thread on the thread you worked from): what you changed, what you "
                    + "verified, what's left. Then stop.")
            : "";
    }

    static IEnumerable<string> ToolNames(string transcript)
    {
        foreach (var line in ReadLines(transcript).Where(l => l.Contains("\"tool_use\"")))
        {
            JsonArray? parts = null;
            try { parts = JsonNode.Parse(line)?["message"]?["content"] as JsonArray; } catch (JsonException) { }
            foreach (var p in parts ?? [])
                if ((string?)p?["type"] == "tool_use" && (string?)p["name"] is { } name) yield return name;
        }
    }

    static IEnumerable<string> ReadLines(string path) => File.Exists(path) ? File.ReadLines(path) : [];

    static string[] Tail(string path, int bytes = 400_000)
    {
        if (!File.Exists(path)) return [];
        using var f = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
        f.Seek(Math.Max(0, f.Length - bytes), SeekOrigin.Begin);
        return new StreamReader(f).ReadToEnd().Split('\n');
    }

    static string Cut(JsonNode? s, int n) => ((string?)s ?? "") is var v && v.Length > n ? v[..n] : v;

    static string Context(string hookEvent, string text) => text == "" ? "" :
        new JsonObject { ["hookSpecificOutput"] = new JsonObject { ["hookEventName"] = hookEvent, ["additionalContext"] = text.TrimEnd() } }.ToJsonString();

    static string Block(string reason) => new JsonObject { ["decision"] = "block", ["reason"] = reason }.ToJsonString();
}
