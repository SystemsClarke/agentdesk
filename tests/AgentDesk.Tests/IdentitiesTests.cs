using System.Diagnostics;
using System.Text;
using System.Text.Json;
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
        await Sees(s, "alpha", $"author=alpha --session-id {id} --append-system-prompt \"be brief\"");
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
