using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>The governor enforcing (docs/GOAL.md milestone 7) over identities with cmd standing in for claude: swarm starts gated
/// and queued, Phoenix never blocked, shedding order, fail closed, the tier override, and the switch off changing nothing.
/// A sample of 10% used with the reset 2 hours off funds more than any cap, so governor_max_sessions is the cap; a 5-hour window
/// at 80% steps tiers down, and at 95% allows no swarm sessions at all.</summary>
public sealed class GovernorEnforceTests : IDisposable
{
    const string Claude = "cmd /d /q /k echo author=%AGENTDESK_AUTHOR%";
    readonly string dir = Directory.CreateTempSubdirectory("governor-enforce-").FullName;
    readonly BoardStore store;
    readonly Sessions sessions = new();
    readonly Identities ids;

    public GovernorEnforceTests()
    {
        store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
        ids = new Identities(store, sessions, dir, Claude);
    }

    public void Dispose()
    {
        using (var db = store.Open()) db.Exec("DELETE FROM identities"); // else a stop frees a slot and launches the next queued one
        foreach (var n in JsonDocument.Parse(sessions.List().Result).RootElement.GetProperty("sessions").EnumerateArray())
            try { sessions.Stop(n.GetProperty("name").GetString()!).Wait(); } catch (AggregateException) { }
    }

    void Settings(bool enforce, int cap) =>
        File.WriteAllText(Path.Combine(dir, "settings.json"), $$"""{"max_sessions": 10, "governor_enforce": {{(enforce ? "true" : "false")}}, "governor_max_sessions": {{cap}}}""");

    /// <summary>The one usage sample the governor sees: <paramref name="ageMinutes"/> old.</summary>
    void Sample(double five = 10, double ageMinutes = 0)
    {
        var now = DateTimeOffset.UtcNow;
        using var db = store.Open();
        db.Exec("DELETE FROM usage_samples");
        Governor.Insert(db, new(now.AddMinutes(-ageMinutes), 10, now.AddHours(2), five, now.AddHours(3), 0));
    }

    /// <summary>A running goal (lead &lt;goal&gt;-lead) with these members, all created as identities.</summary>
    async Task Goal(string goal, string leadModel = "opus", params (string Name, string Model)[] members)
    {
        await ids.Create(goal + "-lead", dir, null, null, model: leadModel);
        using var db = store.Open();
        db.Exec("INSERT INTO goals (name, objective, measure_folder, lead, state, created_ts, updated_ts) VALUES ($g, 'x', $f, $l, 'running', $ts, $ts)",
            ("g", goal), ("f", dir), ("l", goal + "-lead"), ("ts", db.NowIso()));
        foreach (var (name, model) in members)
        {
            db.Exec("INSERT INTO goal_members (identity, goal, task, created_ts) VALUES ($i, $g, 't', $ts)", ("i", name), ("g", goal), ("ts", db.NowIso()));
            await ids.Create(name, dir, null, null, model: model);
        }
    }

    static JsonElement Json(Task<string> t) => JsonDocument.Parse(t.Result).RootElement;

    string State(string name) => Row(name).GetProperty("state").GetString()!;

    JsonElement Row(string name) => Json(ids.List()).GetProperty("identities").EnumerateArray().Single(r => r.GetProperty("name").GetString() == name);

    string Command(string name) => Json(sessions.List()).GetProperty("sessions").EnumerateArray()
        .Single(s => s.GetProperty("name").GetString() == name).GetProperty("command").GetString()!;

    static bool Logged(string text) => Log.Tail(1000).Any(l => l.Contains(text));

    [Fact]
    public async Task Starts_over_the_cap_are_queued_and_drained_later()
    {
        Settings(enforce: true, cap: 1);
        Sample();
        await Goal("g1", members: [("g1-m1", "sonnet")]);
        await ids.Create("johns", dir, null, null);
        Assert.Equal("running", Json(ids.Start("g1-lead")).GetProperty("state").GetString());
        var member = Json(ids.Start("g1-m1", "your task"));
        Assert.Equal("queued", member.GetProperty("state").GetString()); // queued, not failed
        Assert.Contains("usage governor", member.GetProperty("governor").GetString());
        Assert.True(Logged("governor: queued g1-m1 (1 running, cap 1)"));
        Assert.Equal("running", Json(ids.Start("johns")).GetProperty("state").GetString()); // John's own identity is not gated
        var g = Json(ids.GovernorUi());
        Assert.True(g.GetProperty("enforcing").GetBoolean());
        Assert.Equal("g1-m1", Assert.Single(g.GetProperty("held").EnumerateArray()).GetString());

        await ids.Tick(); // still over: nothing changes
        Assert.Equal("queued", State("g1-m1"));
        Settings(enforce: true, cap: 3);
        await ids.Tick();
        Assert.Equal("running", State("g1-m1"));
        Assert.EndsWith("\"your task\"", Command("g1-m1")); // the prompt it was started with survives the wait
        Assert.Empty(Json(ids.GovernorUi()).GetProperty("held").EnumerateArray());
    }

    [Fact]
    public async Task A_stale_sample_or_failing_usage_fails_closed()
    {
        Settings(enforce: true, cap: 5);
        Sample(ageMinutes: 31);
        await Goal("g2");
        Assert.Equal("queued", Json(ids.Start("g2-lead")).GetProperty("state").GetString());
        var g = Json(ids.GovernorUi());
        Assert.False(g.GetProperty("fresh").GetBoolean());
        Assert.Contains("31 minutes old", g.GetProperty("fail_closed").GetString());
        Sample(ageMinutes: 1);
        ids.UsageFailing = () => true;
        await ids.Tick();
        Assert.Equal("queued", State("g2-lead"));
        Assert.Contains("/usage is failing", Json(ids.GovernorUi()).GetProperty("fail_closed").GetString());
        ids.UsageFailing = () => false;
        await ids.Tick();
        Assert.Equal("running", State("g2-lead"));
    }

    [Fact]
    public async Task Phoenix_is_never_blocked()
    {
        Settings(enforce: true, cap: 1);
        Sample();
        await Goal("g3", members: [("g3-m1", "sonnet")]);
        var sid = Json(ids.Start("g3-m1")).GetProperty("claude_session_id").GetString()!;
        Sample(five: 95, ageMinutes: 45); // over every cap, and stale: no new swarm session could start now
        var board = new AgentBoard(store, null!, "wait {0}");
        var me = new Caller(sid, "g3-m1", dir, "claude-code", 1, "g3-m1");
        await ids.Torch(me, board.PassTheTorch(me, "Owns the parser.", null));
        await ids.AfterTurn(JsonDocument.Parse($$"""{"session_id": "{{sid}}"}""").RootElement, Task.FromResult(""));
        JsonElement row;
        for (var sw = Stopwatch.StartNew(); (row = Row("g3-m1")).GetProperty("pid").ValueKind == JsonValueKind.Null || row.GetProperty("generation").GetInt32() != 2; await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(15), "the governor blocked a Phoenix restart");
        Assert.Equal("running", row.GetProperty("state").GetString());
        Assert.False(Logged("governor: queued g3-m1"));
    }

    [Fact]
    public async Task Shedding_stops_members_first_concierge_first_then_the_longest_idle_and_never_johns()
    {
        Settings(enforce: true, cap: 10);
        Sample();
        await Goal("g4", members: [("g4-a", "sonnet"), ("g4-b", "sonnet")]);
        await Goal(Concierge.Name, "sonnet", ("concierge-w1", "sonnet"));
        await ids.Create("mine", dir, null, null);
        foreach (var n in new[] { "g4-lead", "concierge-lead", "g4-a", "g4-b", "concierge-w1", "mine" }) await ids.Start(n);
        Assert.All(new[] { "g4-lead", "concierge-lead", "g4-a", "g4-b", "concierge-w1", "mine" }, n => Assert.Equal("running", State(n)));
        using (var db = store.Open()) // g4-b and g4-lead wrote to the board lately; the others are idle
            foreach (var n in new[] { "g4-b", "g4-lead" })
                db.Exec("INSERT INTO presence (author, seen_ts) VALUES ($a, $ts)", ("a", n), ("ts", DateTimeOffset.UtcNow.AddHours(1).ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'")));

        string[] Running() => [.. new[] { "g4-lead", "concierge-lead", "g4-a", "g4-b", "concierge-w1", "mine" }.Where(n => State(n) == "running")];
        Settings(enforce: true, cap: 4);
        await ids.Tick();
        Assert.Equal(["g4-lead", "concierge-lead", "g4-b", "mine"], Running()); // the Concierge's member, then the idle member
        Assert.Equal("queued", State("concierge-w1")); // shed back to the queue, to resume when the governor allows
        Settings(enforce: true, cap: 3);
        await ids.Tick();
        Assert.Equal(["g4-lead", "concierge-lead", "mine"], Running()); // members before leads
        Settings(enforce: true, cap: 2);
        await ids.Tick();
        Assert.Equal(["g4-lead", "mine"], Running()); // of the leads, the longest idle
        Sample(five: 95); // the 5-hour guard: no swarm sessions at all
        await ids.Tick();
        Assert.Equal(["mine"], Running()); // John's own identity is never stopped
        Assert.True(Logged("John's own identities (mine) are over it and are never stopped automatically"));
        Sample();
        Settings(enforce: true, cap: 10);
        await ids.Tick();
        Assert.Equal(6, Running().Length); // all back, resumed
    }

    [Fact]
    public async Task Stepping_down_launches_at_the_recommended_tier_and_records_it()
    {
        Settings(enforce: true, cap: 10);
        Sample(five: 80); // the 5-hour window at 75% or more: step down
        await Goal("g5", "opus", ("g5-m1", "sonnet"), ("g5-m2", "haiku"));
        await ids.Create("mine", dir, null, null, model: "opus");
        foreach (var n in new[] { "g5-lead", "g5-m1", "g5-m2", "mine" }) await ids.Start(n);
        Assert.Contains(" --model sonnet ", Command("g5-lead")); // leads opus -> sonnet
        Assert.Contains(" --model haiku ", Command("g5-m1")); // members sonnet -> haiku
        Assert.Contains(" --model haiku ", Command("g5-m2")); // never a step up
        Assert.Contains(" --model opus ", Command("mine")); // John's own identity keeps its tier
        Assert.Equal("opus", Row("g5-lead").GetProperty("model").GetString()); // the stored column is unchanged
        Assert.Equal("sonnet", Row("g5-lead").GetProperty("running_model").GetString());

        var feed = Path.Combine(dir, "claude_usage.json");
        File.WriteAllText(feed, $$$"""{"captured_ts": "{{{DateTimeOffset.UtcNow.AddSeconds(5):yyyy-MM-ddTHH:mm:ss}}}+00:00", "seven_day": {"used": 11}}""");
        Assert.True(Governor.Record(store, feed));
        using var db = store.Open();
        Assert.Equal("4|2|1|1", db.Scalar("SELECT swarm_sessions || '|' || haiku_sessions || '|' || sonnet_sessions || '|' || opus_sessions FROM usage_samples ORDER BY ts DESC LIMIT 1"));
    }

    [Fact]
    public async Task Switched_off_nothing_changes_but_the_would_have_log()
    {
        Settings(enforce: false, cap: 10);
        Sample(five: 95); // enforcing, this would queue every swarm start and shed every swarm session
        await Goal("g6", "opus", ("g6-m1", "sonnet"));
        Assert.Equal("running", Json(ids.Start("g6-lead")).GetProperty("state").GetString());
        Assert.Equal("running", Json(ids.Start("g6-m1")).GetProperty("state").GetString());
        Assert.Contains(" --model opus ", Command("g6-lead")); // the stored tier
        Assert.True(Logged("governor (advisory): would have queued g6-m1"));
        Assert.True(Logged("governor (advisory): would have launched g6-lead at sonnet instead of opus"));
        await ids.Tick();
        await ids.Tick(); // counted once per session, not per tick
        Assert.Equal("running", State("g6-lead"));
        Assert.Equal("running", State("g6-m1"));
        Assert.True(Logged("governor (advisory): would have shed g6-m1"));
        var g = Json(ids.GovernorUi());
        Assert.False(g.GetProperty("enforcing").GetBoolean());
        Assert.True(g.GetProperty("advisory").GetBoolean());
        Assert.Equal(2, g.GetProperty("would_queue").GetInt32());
        Assert.Equal(2, g.GetProperty("would_shed").GetInt32());
    }

    [Fact]
    public async Task Only_john_flips_the_switch_and_it_keeps_other_settings()
    {
        Settings(enforce: false, cap: 7);
        var agent = new Caller("sid", "a", dir, "claude-code", 1, "a");
        await Assert.ThrowsAsync<ArgumentException>(() => ids.Enforce(agent, true));
        var g = Json(ids.Enforce(new Caller(null, null, null, "slack", 0), true));
        Assert.True(g.GetProperty("enforcing").GetBoolean());
        var s = JsonNode.Parse(File.ReadAllText(Path.Combine(dir, "settings.json")))!;
        Assert.Equal((true, 7, 10), ((bool)s["governor_enforce"]!, (int)s["governor_max_sessions"]!, (int)s["max_sessions"]!));
        Assert.False(Json(ids.Enforce(new Caller(null, null, null, "web", 0), false)).GetProperty("enforcing").GetBoolean());
    }
}
