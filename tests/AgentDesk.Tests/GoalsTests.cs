using System.Diagnostics;
using System.Text;
using System.Text.Json;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>Goals over identities, with a PowerShell script standing in for claude (it prints what it is typed) and cmd's
/// `type value.txt` for the measure.</summary>
public sealed class GoalsTests : IDisposable
{
    readonly string dir = Directory.CreateTempSubdirectory("goals-").FullName;
    readonly string claude;
    readonly BoardStore store;
    readonly Sessions sessions = new();
    readonly Identities ids;
    readonly Goals goals;
    readonly CancellationTokenSource gone = new();
    readonly Caller john;

    public GoalsTests()
    {
        File.WriteAllText(Path.Combine(dir, "settings.json"), """{"max_sessions": 6}""");
        var script = Path.Combine(dir, "standin.ps1");
        File.WriteAllText(script, "\"ready $env:AGENTDESK_AUTHOR\"\nwhile ($null -ne ($l = [Console]::In.ReadLine())) { \"got: $l\" }\n");
        claude = $"powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{script}\"";
        store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
        ids = new Identities(store, sessions, dir, claude);
        goals = new Goals(store, ids, sessions) { InlineWait = TimeSpan.FromSeconds(30) };
        john = new Caller(null, null, dir, null, 1);
    }

    public void Dispose()
    {
        gone.Cancel();
        using (var db = store.Open()) db.Exec("DELETE FROM identities");
        foreach (var n in Json(sessions.List()).GetProperty("sessions").EnumerateArray())
            try { sessions.Stop(n.GetProperty("name").GetString()!).Wait(); } catch (AggregateException) { }
    }

    static JsonElement Json(Task<string> t) => JsonDocument.Parse(t.Result).RootElement;

    JsonElement Identity(string name) => Json(ids.List()).GetProperty("identities").EnumerateArray().Single(r => r.GetProperty("name").GetString() == name);

    Caller As(string name) => new(Identity(name).GetProperty("claude_session_id").GetString(), name, dir, "claude-code", 1, name);

    string Command(string name) => Json(sessions.List()).GetProperty("sessions").EnumerateArray().Single(s => s.GetProperty("name").GetString() == name).GetProperty("command").GetString()!;

    void Value(string v) => File.WriteAllText(Path.Combine(dir, "value.txt"), v);

    /// <summary>A goal "toy" through create, propose and John's approval: running, lower is better, success under 100.</summary>
    async Task<Caller> Running(int? members = null, double? cadence = null)
    {
        await goals.Create("toy", "make the value small", dir);
        var lead = As("toy-lead");
        await goals.Propose(lead, "toy", "halving works", "type value.txt", "value < 100", 1);
        await goals.Approve(john, "toy", members, null, cadence);
        return lead;
    }

    static async Task Until(Func<bool> ok, string what)
    {
        for (var sw = Stopwatch.StartNew(); !ok(); await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(20), $"never: {what}");
    }

    [Fact]
    public async Task Create_propose_approve_runs()
    {
        var g = Json(goals.Create("toy", "make the value small", dir));
        Assert.Equal("draft", g.GetProperty("state").GetString());
        Assert.Equal("running", Identity("toy-lead").GetProperty("state").GetString());
        Assert.Equal("opus", Identity("toy-lead").GetProperty("model").GetString());
        Assert.Contains("call goal_propose with name=toy", Command("toy-lead"));
        Assert.Contains(" --model opus ", Command("toy-lead"));
        using (var db = store.Open()) Assert.Equal("goal: toy", db.Scalar("SELECT subject FROM threads WHERE id=$t", ("t", g.GetProperty("thread_id").GetInt64())));

        var lead = As("toy-lead");
        var stranger = new Caller(Guid.NewGuid().ToString(), "someone", dir, "claude-code", 1, "someone");
        await Assert.ThrowsAsync<ArgumentException>(() => goals.Propose(stranger, "toy", "h", "type value.txt", "value < 100", 1));
        await Assert.ThrowsAsync<ArgumentException>(() => goals.Propose(lead, "toy", "h", "type value.txt", "smaller is better", 1));
        await Assert.ThrowsAsync<ArgumentException>(() => goals.Approve(john, "toy", null, null, null)); // nothing proposed yet
        await goals.Propose(lead, "toy", "halving works", "type value.txt", "value < 100", 3);
        await Assert.ThrowsAsync<ArgumentException>(() => goals.Approve(lead, "toy", null, null, null)); // only John
        var run = Json(goals.Approve(john, "toy", 2, 1.5, 5));
        Assert.Equal(("running", 2, 1.5, 5.0, 3), (run.GetProperty("state").GetString(), run.GetProperty("max_members").GetInt32(),
            run.GetProperty("max_hours").GetDouble(), run.GetProperty("cadence_minutes").GetDouble(), run.GetProperty("samples").GetInt32()));
        Assert.Equal("running", Json(goals.List()).GetProperty("goals")[0].GetProperty("state").GetString());
        Assert.Equal("toy", goals.Summaries()[0]!["name"]!.ToString());
    }

    [Fact]
    public async Task Experiment_done_runs_the_measure_and_records_the_value()
    {
        var lead = await Running();
        var stranger = new Caller(Guid.NewGuid().ToString(), "someone", dir, "claude-code", 1, "someone");
        await Assert.ThrowsAsync<ArgumentException>(() => goals.ExperimentStart(stranger, "toy", "mine"));

        Value("measured in 12 ms: 250\n");
        var n = Json(goals.ExperimentStart(lead, "toy", "baseline")).GetProperty("n").GetInt32();
        var e = Json(goals.ExperimentDone(lead, "toy", n));
        Assert.Equal((1, 250.0, "no gain", "toy-lead"), (n, e.GetProperty("value").GetDouble(), e.GetProperty("verdict").GetString(), e.GetProperty("owner").GetString()));
        await Assert.ThrowsAsync<ArgumentException>(() => goals.ExperimentDone(lead, "toy", n)); // measured once

        Value("180");
        e = Json(goals.ExperimentDone(lead, "toy", Json(goals.ExperimentStart(lead, "toy", "halve it")).GetProperty("n").GetInt32()));
        Assert.Equal("improved", e.GetProperty("verdict").GetString());
        Value("no digits here");
        e = Json(goals.ExperimentDone(lead, "toy", Json(goals.ExperimentStart(lead, "toy", "break it")).GetProperty("n").GetInt32()));
        Assert.Equal("error: no number on stdout", e.GetProperty("verdict").GetString());

        var status = Json(goals.Status("toy"));
        Assert.Equal([250.0, 180.0], status.GetProperty("history").EnumerateArray().Select(v => v.GetDouble()));
        Assert.Contains("Metric trend: 250 -> 180", status.GetProperty("summary").GetString());
        using var db = store.Open();
        Assert.Equal(3L, db.Scalar("SELECT COUNT(*) FROM messages WHERE body LIKE 'Experiment #%'"));
    }

    [Fact]
    public async Task Success_stops_the_loop()
    {
        var lead = await Running();
        Value("42");
        var e = Json(goals.ExperimentDone(lead, "toy", Json(goals.ExperimentStart(lead, "toy", "the fix")).GetProperty("n").GetInt32()));
        Assert.Equal("met", e.GetProperty("verdict").GetString());
        Assert.Equal("succeeded", Json(goals.Status("toy")).GetProperty("state").GetString());
        await Until(() => Identity("toy-lead").GetProperty("state").GetString() == "stopped", "the lead stops");
        await Assert.ThrowsAsync<ArgumentException>(() => goals.ExperimentStart(lead, "toy", "more"));
        using var db = store.Open();
        Assert.StartsWith("**Goal succeeded**: experiment #1 measured 42", (string)db.Scalar("SELECT body FROM messages ORDER BY id DESC LIMIT 1")!);
    }

    [Fact]
    public async Task The_member_budget_holds()
    {
        var lead = await Running(members: 2);
        await Assert.ThrowsAsync<ArgumentException>(() => goals.Spawn(new Caller(Guid.NewGuid().ToString(), "x", dir, "claude-code", 1, "x"), "toy", "a", "t", null));
        await goals.Spawn(lead, "toy", "a", "try halving", "haiku");
        await goals.Spawn(lead, "toy", "b", "try thirds", null);
        Assert.Equal(("running", "haiku"), (Identity("toy-a").GetProperty("state").GetString(), Identity("toy-a").GetProperty("model").GetString()));
        Assert.Contains("Your task for goal toy: try halving", Command("toy-a"));
        var over = await Assert.ThrowsAsync<ArgumentException>(() => goals.Spawn(lead, "toy", "c", "one too many", null));
        Assert.StartsWith("budget:", over.Message);

        var a = As("toy-a");
        Value("300");
        Assert.Equal("toy-a", Json(goals.ExperimentDone(a, "toy", Json(goals.ExperimentStart(a, "toy", "halve")).GetProperty("n").GetInt32())).GetProperty("owner").GetString());
        await goals.MemberDone(a, "halving alone is not enough");
        await Until(() => Json(ids.List()).GetProperty("identities").EnumerateArray().All(r => r.GetProperty("name").GetString() != "toy-a"), "toy-a is forgotten");
        await goals.Spawn(lead, "toy", "c", "now there is room", null);
        Assert.Equal(["toy-b", "toy-c"], Json(goals.Status("toy")).GetProperty("members").EnumerateArray().Select(m => m.GetProperty("identity").GetString()).Order());
    }

    [Fact]
    public async Task The_cadence_wakes_the_lead()
    {
        await Running(cadence: 0.02); // 1.2 s
        var seen = new StringBuilder();
        await sessions.Attach("toy-lead", 400, 30, e =>
        {
            if (JsonDocument.Parse(e).RootElement.TryGetProperty("data", out var d)) lock (seen) seen.Append(Encoding.UTF8.GetString(d.GetBytesFromBase64()));
            return Task.CompletedTask;
        }, gone.Token);
        _ = goals.Run(TimeSpan.FromMilliseconds(200), gone.Token);
        int Wakes() { lock (seen) return seen.ToString().Split("got: [AgentDesk goal wake] Goal toy (running)").Length - 1; }
        await Until(() => Wakes() >= 2, "two wakes");
        lock (seen) Assert.Contains("Hypothesis: halving works", seen.ToString());
    }

    [Fact]
    public async Task A_phoenix_successor_starts_with_the_goal_status()
    {
        var lead = await Running();
        Value("250");
        await goals.ExperimentDone(lead, "toy", Json(goals.ExperimentStart(lead, "toy", "baseline")).GetProperty("n").GetInt32());
        var board = new AgentBoard(store, null!, "wait {0}");
        await ids.Torch(lead, board.PassTheTorch(lead, "Owns the toy goal. Next: halve it.", null));
        var hook = JsonDocument.Parse(JsonSerializer.Serialize(new Dictionary<string, string> { ["session_id"] = lead.SessionId! })).RootElement;
        await ids.AfterTurn(hook, Task.FromResult(""));
        await Until(() => Identity("toy-lead").GetProperty("generation").GetInt32() == 2 && Identity("toy-lead").GetProperty("pid").ValueKind != JsonValueKind.Null, "generation 2");
        var command = Command("toy-lead");
        Assert.Contains("Next: halve it.\n\nYour goal's status now, from the core:\nGoal toy (running)", command);
        Assert.Contains("Hypothesis: halving works", command);
        Assert.Contains("Last experiments: #1 by toy-lead, baseline: 250 (no gain)", command);
        Assert.Contains("Metric trend: 250", command);
    }
}
