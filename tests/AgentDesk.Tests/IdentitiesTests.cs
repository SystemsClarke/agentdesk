using System.Diagnostics;
using System.Text;
using System.Text.Json;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>Identities over headless sessions, with cmd standing in for claude: it echoes its author and arguments, then stays up.</summary>
public sealed class IdentitiesTests : IDisposable
{
    const string Claude = "cmd /d /q /k echo author=%AGENTDESK_AUTHOR%";
    readonly string dir = Directory.CreateTempSubdirectory("identities-").FullName;
    readonly string projects;
    readonly BoardStore store;
    readonly List<Sessions> cores = [];
    readonly CancellationTokenSource gone = new();

    public IdentitiesTests()
    {
        File.WriteAllText(Path.Combine(dir, "settings.json"), """{"max_sessions": 2}""");
        projects = Directory.CreateDirectory(Path.Combine(dir, "projects")).FullName;
        Environment.SetEnvironmentVariable("AGENTDESK_CLAUDE_PROJECTS", projects);
        store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
    }

    public void Dispose()
    {
        gone.Cancel();
        using (var db = store.Open()) db.Exec("DELETE FROM identities"); // else a stop frees a slot and launches the next queued one
        foreach (var s in cores)
            foreach (var n in JsonDocument.Parse(s.List().Result).RootElement.GetProperty("sessions").EnumerateArray())
                try { s.Stop(n.GetProperty("name").GetString()!).Wait(); } catch (AggregateException) { }
        Environment.SetEnvironmentVariable("AGENTDESK_CLAUDE_PROJECTS", null);
    }

    /// <summary>A core: its own sessions, and identities over the shared board.</summary>
    (Sessions, Identities) Core()
    {
        var s = new Sessions();
        cores.Add(s);
        return (s, new Identities(store, s, dir, Claude));
    }

    static JsonElement Json(Task<string> t) => JsonDocument.Parse(t.Result).RootElement;

    Dictionary<string, string> States(Identities ids) =>
        Json(ids.List()).GetProperty("identities").EnumerateArray().ToDictionary(r => r.GetProperty("name").GetString()!, r => r.GetProperty("state").GetString()!);

    async Task Sees(Sessions s, string name, string text)
    {
        var seen = new StringBuilder();
        await s.Attach(name, 200, 30, e =>
        {
            if (JsonDocument.Parse(e).RootElement.TryGetProperty("data", out var d)) lock (seen) seen.Append(Encoding.UTF8.GetString(d.GetBytesFromBase64()));
            return Task.CompletedTask;
        }, gone.Token);
        for (var sw = Stopwatch.StartNew(); sw.Elapsed < TimeSpan.FromSeconds(15); await Task.Delay(50))
            lock (seen) if (seen.ToString().Contains(text)) return;
        lock (seen) Assert.Fail($"never saw '{text}' in: {seen}");
    }

    [Fact]
    public async Task Create_start_list_forget()
    {
        var (s, ids) = Core();
        await ids.Create("alpha", dir, "be brief", null);
        Assert.Equal("stopped", States(ids)["alpha"]);
        var row = Json(ids.Start("alpha"));
        Assert.Equal("running", row.GetProperty("state").GetString());
        var id = row.GetProperty("claude_session_id").GetString()!;
        await Sees(s, "alpha", $"author=alpha --session-id {id} --append-system-prompt \"You are one generation of alpha, a long-lived AgentDesk agent.");
        Assert.EndsWith("that handoff.\n\nbe brief\"", Command(s));
        var pid = row.GetProperty("pid").GetInt32();
        Assert.Contains("\"forgotten\"", await ids.Forget("alpha"));
        Assert.Empty(States(ids));
        Assert.Throws<ArgumentException>(() => Process.GetProcessById(pid));
    }

    [Fact]
    public async Task Starts_past_the_cap_queue_until_a_slot_frees()
    {
        var (_, ids) = Core();
        foreach (var n in new[] { "a", "b", "c" }) { await ids.Create(n, dir, null, null); await ids.Start(n); }
        Assert.Equal(["queued", "running", "running"], States(ids).Values.Order());
        var running = States(ids).First(p => p.Value == "running").Key;
        await ids.Stop(running);
        Assert.Equal("stopped", States(ids)[running]);
        Assert.Equal(2, States(ids).Values.Count(v => v == "running"));
    }

    [Fact]
    public async Task A_restarted_core_resumes_what_was_running()
    {
        var (_, ids) = Core();
        await ids.Create("keep", dir, null, null);
        await ids.Create("auto", dir, null, null, autostart: true);
        var before = Json(ids.Start("keep"));
        var id = before.GetProperty("claude_session_id").GetString()!;
        File.WriteAllText(Path.Combine(Directory.CreateDirectory(Path.Combine(projects, "C--x")).FullName, $"{id}.jsonl"), "{}\n"); // it has a conversation now

        var (s2, restarted) = Core(); // the old core's processes are still up here; its sessions are not this core's
        restarted.Resume();
        var after = Json(restarted.List()).GetProperty("identities").EnumerateArray().ToDictionary(r => r.GetProperty("name").GetString()!);
        Assert.Equal("running", after["keep"].GetProperty("state").GetString());
        Assert.NotEqual(before.GetProperty("pid").GetInt32(), after["keep"].GetProperty("pid").GetInt32());
        Assert.Equal("running", after["auto"].GetProperty("state").GetString());
        await Sees(s2, "keep", $"author=keep --resume {id}");
    }

    static string Command(Sessions s) => Assert.Single(Json(s.List()).GetProperty("sessions").EnumerateArray()).GetProperty("command").GetString()!;

    static JsonElement Hook(string sid) => JsonDocument.Parse(JsonSerializer.Serialize(new Dictionary<string, string> { ["session_id"] = sid })).RootElement;

    JsonElement Row(Identities ids, string name) => Json(ids.List()).GetProperty("identities").EnumerateArray().Single(r => r.GetProperty("name").GetString() == name);

    [Fact]
    public async Task A_handoff_restarts_the_identity_from_it_once_the_turn_ends()
    {
        var (s, ids) = Core();
        var board = new AgentBoard(store, null!, "wait {0}");
        await ids.Create("phx", dir, null, null);
        var sid = Json(ids.Start("phx")).GetProperty("claude_session_id").GetString()!;
        var events = new List<string>();
        await s.Attach("phx", 120, 30, e => { lock (events) events.Add(e); return Task.CompletedTask; }, gone.Token);

        var me = new Caller(sid, "phx", dir, "claude-code", 1, "phx");
        await ids.Torch(me, board.PassTheTorch(me, "Owns the parser. Next: its tests.", null));
        Assert.Equal("", await ids.AfterTurn(Hook(sid), Task.FromResult(""))); // the hook answers at once; the restart follows
        JsonElement row;
        for (var sw = Stopwatch.StartNew(); (row = Row(ids, "phx")).GetProperty("pid").ValueKind == JsonValueKind.Null || row.GetProperty("generation").GetInt32() != 2; await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(15), "never restarted");

        var next = row.GetProperty("claude_session_id").GetString()!;
        Assert.NotEqual(sid, next);
        Assert.Equal("running", row.GetProperty("state").GetString());
        Assert.StartsWith($"{Claude} --session-id {next} --append-system-prompt ", Command(s));
        Assert.EndsWith("\"You are phx, generation 2. Your previous generation handed off with:\n\nOwns the parser. Next: its tests.\"", Command(s));
        lock (events) Assert.Contains(events, e => e.Contains("session.restarted"));
        using (var db = store.Open())
        {
            var chain = Assert.Single(db.Rows("SELECT * FROM phoenix_chain"));
            var handoff = (long)chain["handoff_msg"]!;
            Assert.Equal(("phx", 1L, sid), ((string)chain["identity"]!, (long)chain["generation"]!, (string)chain["claude_session_id"]!));
            Assert.Equal($"generation 2 started from handoff #{handoff}", db.Scalar("SELECT body FROM messages ORDER BY id DESC LIMIT 1"));
        }

        // The successor hands off straight away: within two minutes of the last restart, nothing happens.
        var successor = me with { SessionId = next };
        await ids.Torch(successor, board.PassTheTorch(successor, "Again, already.", null));
        await ids.AfterTurn(Hook(next), Task.FromResult(""));
        await Task.Delay(2500);
        Assert.Equal(next, Row(ids, "phx").GetProperty("claude_session_id").GetString());
        Assert.Equal(2, Row(ids, "phx").GetProperty("generation").GetInt32());
        using (var db = store.Open()) Assert.Single(db.Rows("SELECT * FROM phoenix_chain"));
    }

    [Fact]
    public async Task A_handoff_from_any_other_session_restarts_nothing()
    {
        var (s, ids) = Core();
        var board = new AgentBoard(store, null!, "wait {0}");
        await ids.Create("phx", dir, null, null);
        var before = Json(ids.Start("phx"));
        var sid = before.GetProperty("claude_session_id").GetString()!;
        var other = Guid.NewGuid().ToString();
        foreach (var caller in new[] { new Caller(other, null, dir, "claude-code", 1), new Caller(other, "phx", dir, "claude-code", 1, "phx"), new Caller(sid, "phx", dir, "claude-code", 1) })
        {
            await ids.Torch(caller, board.PassTheTorch(caller, "Not mine to hand off.", null));
            await ids.AfterTurn(Hook(caller.SessionId!), Task.FromResult(""));
        }
        await ids.AfterTurn(Hook(sid), Task.FromResult(""));
        await Task.Delay(2500);
        var after = Row(ids, "phx");
        Assert.Equal(JsonValueKind.Null, after.GetProperty("phoenix_msg").ValueKind);
        Assert.Equal((sid, 1, before.GetProperty("pid").GetInt32()),
            (after.GetProperty("claude_session_id").GetString(), after.GetProperty("generation").GetInt32(), after.GetProperty("pid").GetInt32()));
        using var db = store.Open();
        Assert.Empty(db.Rows("SELECT * FROM phoenix_chain"));
    }

    [Fact]
    public async Task Adoptable_lists_recent_transcripts_and_adopt_resumes_one()
    {
        var id = Guid.NewGuid().ToString();
        var folder = Directory.CreateDirectory(Path.Combine(projects, "C--work")).FullName;
        var cwd = JsonSerializer.Serialize(dir);
        File.WriteAllLines(Path.Combine(folder, $"{id}.jsonl"),
        [
            """{"type":"summary","summary":"x"}""",
            $$$"""{"type":"user","isMeta":true,"cwd":{{{cwd}}},"message":{"role":"user","content":"caveat"}}""",
            $$$"""{"type":"user","cwd":{{{cwd}}},"message":{"role":"user","content":"<command-name>/clear</command-name>"}}""",
            $$$"""{"type":"user","cwd":{{{cwd}}},"message":{"role":"user","content":[{"type":"text","text":"<system-reminder>\nnot typed\n</system-reminder>Fix the\nflaky test in {{{new string('x', 100)}}}"}]}}""",
            """{"type":"assistant","message":{"role":"assistant","content":"ok"}}""",
        ]);
        var old = Path.Combine(folder, $"{Guid.NewGuid()}.jsonl");
        File.WriteAllText(old, "{}\n");
        File.SetLastWriteTimeUtc(old, DateTime.UtcNow.AddDays(-2));
        File.WriteAllText(Path.Combine(folder, $"{Guid.NewGuid()}.jsonl"), // claude -p /usage: nobody typed in it
            """{"type":"user","message":{"role":"user","content":"<command-name>/usage</command-name>"}}""" + "\n");

        var row = Assert.Single(Json(Identities.Adoptable()).GetProperty("sessions").EnumerateArray());
        Assert.Equal(id, row.GetProperty("session_id").GetString());
        Assert.Equal(dir, row.GetProperty("folder").GetString());
        var first = row.GetProperty("first_message").GetString()!;
        Assert.StartsWith("Fix the flaky test in xxx", first);
        Assert.Equal(80, first.Length);

        var (s, ids) = Core();
        var adopted = Json(ids.Adopt(id, "adoptee"));
        Assert.Equal("running", adopted.GetProperty("state").GetString());
        Assert.Equal(id, adopted.GetProperty("claude_session_id").GetString());
        Assert.Contains("desktop app", adopted.GetProperty("note").GetString());
        await Sees(s, "adoptee", $"author=adoptee --resume {id}");
    }
}
