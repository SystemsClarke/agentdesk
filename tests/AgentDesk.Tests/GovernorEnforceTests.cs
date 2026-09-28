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

    void Settings(bool enforce, int cap, int max = 10) =>
        File.WriteAllText(Path.Combine(dir, "settings.json"), $$"""{"max_sessions": {{max}}, "governor_enforce": {{(enforce ? "true" : "false")}}, "governor_max_sessions": {{cap}}}""");

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

    // ---- the one session pool (Budget)

    static Budget.Want W(string name, string goal, bool lead, bool running, string since = "") => new(name, goal, lead, running, since);

    /// <summary>Two goals and the Concierge split the swarm's part (the ceiling minus John): leads first, then members round-robin,
    /// the goal that has waited longest first, each within its max_members.</summary>
    [Fact]
    public void Fair_share_splits_the_pool_across_two_goals_and_the_concierge()
    {
        var b = new Budget
        {
            Hard = 8, Plan = 8, Enforce = true, Running = 4, John = 1,
            MaxMembers = new Dictionary<string, int> { ["a"] = 3, ["b"] = 3, [Concierge.Name] = 2 },
            Wants =
            [
                W("a-lead", "a", true, true), W("a-1", "a", false, true), W("a-2", "a", false, false, "10:00"), W("a-3", "a", false, false, "10:05"),
                W("b-lead", "b", true, false, "09:00"), W("b-1", "b", false, false, "09:01"), W("b-2", "b", false, false, "09:02"),
                W("concierge-lead", Concierge.Name, true, true), W("concierge-w1", Concierge.Name, false, false, "11:00"),
                W("concierge-w2", Concierge.Name, false, false, "11:01"), W("concierge-w3", Concierge.Name, false, false, "11:02"),
            ],
        };
        // 7 swarm slots: b waited longest, then a, then the Concierge. Leads 1 each, then members b, a, Concierge (at its max_members 2), b.
        Assert.Equal(new Dictionary<string, int> { ["b"] = 3, ["a"] = 2, [Concierge.Name] = 2 }, b.Shares(8));
        Assert.Null(b.CanStart("b", lead: true, 8));
        Assert.Contains("goal a has its share of the pool running (2 of 2)", b.CanStart("a", lead: false, 8));
        Assert.Null(b.CanStart(Concierge.Name, lead: false, 8));
        // A small pool still gives every goal its lead before any member: 3 slots, one lead each.
        Assert.Equal(new Dictionary<string, int> { ["b"] = 1, ["a"] = 1, [Concierge.Name] = 1 }, b.Shares(4));
        Assert.Contains("goal concierge has its share", (b with { Running = 3 }).CanStart(Concierge.Name, lead: false, 4));
        Assert.Contains("the pool is full", b.CanStart("b", lead: true, 4));
        // max_members is a goal's bound inside the pool, however much room there is.
        var full = b with { Wants = [.. b.Wants.Select(w => w.Name is "concierge-w1" or "concierge-w2" ? w with { Running = true } : w)], Running = 6 };
        Assert.Contains("max_members 2", full.CanStart(Concierge.Name, lead: false, 50));
        // John's own identities answer only to max_sessions, never to the plan.
        Assert.Null((b with { Plan = null }).CanStart(null, lead: false, 0));
        Assert.NotNull((b with { Running = 8 }).CanStart(null, lead: false, 8));
        // Advisory, the ceiling is max_sessions and the plan's lower one only shows what enforcing would do.
        var advisory = b with { Enforce = false, Plan = 3 };
        Assert.Equal((8, 3, "max_sessions"), (advisory.Ceiling, advisory.Governed, advisory.Why));
        Assert.Equal((3, "plan"), ((advisory with { Enforce = true }).Ceiling, (advisory with { Enforce = true }).Why));
        Assert.Equal((0, "fail closed"), ((advisory with { Enforce = true, Plan = null }).Ceiling, (advisory with { Enforce = true, Plan = null }).Why));
    }

    /// <summary>The pool counts every kind of session, and ui:status's "sessions" and ui:governor's "pool" say who owns what.</summary>
    [Fact]
    public async Task The_pool_counts_every_kind_and_reports_it_by_owner()
    {
        Settings(enforce: false, cap: 10, max: 6);
        Sample();
        await Goal("g8", members: [("g8-m1", "sonnet"), ("g8-m2", "sonnet")]);
        await Goal(Concierge.Name, "sonnet", ("concierge-w1", "sonnet"));
        await ids.Create("mine", dir, null, null);
        foreach (var n in new[] { "mine", "g8-lead", "g8-m1", "concierge-lead", "concierge-w1" }) await ids.Start(n);
        var pool = ids.Counts();
        Assert.Equal((5, 6, 6, "max_sessions", 1), ((int)pool["running"]!, (int)pool["max"]!, (int)pool["ceiling"]!, (string)pool["why"]!, (int)pool["john"]!));
        var owners = pool["owners"]!.AsArray().ToDictionary(o => (string)o!["owner"]!, o => o!);
        Assert.Equal(["Concierge", "John", "g8"], owners.Keys.Order(StringComparer.Ordinal));
        Assert.Equal((2, 0, 2), ((int)owners["g8"]["running"]!, (int)owners["g8"]["queued"]!, (int)owners["g8"]["share"]!));
        Assert.Equal(2, (int)owners["Concierge"]["running"]!); // a share never exceeds what a goal wants: the sixth slot is free
        Assert.Equal("5 of 6 sessions (max_sessions) · John 1 · Concierge 2/2 · g8 2/2", (string)pool["summary"]!);
        Assert.Equal(5, Json(ids.GovernorUi()).GetProperty("pool").GetProperty("running").GetInt32());

        await ids.Start("g8-m2"); // the last slot
        Assert.Equal(6, (int)ids.Counts()["running"]!);
        Settings(enforce: false, cap: 10, max: 5); // John lowers "Sessions at once": nothing is stopped, nothing more starts
        await ids.Create("mine2", dir, null, null);
        Assert.Equal("queued", Json(ids.Start("mine2")).GetProperty("state").GetString());
    }

    /// <summary>A freed slot goes to the goal that has waited longest, not to the goal that already holds the most.</summary>
    [Fact]
    public async Task A_freed_slot_goes_to_the_goal_that_waited_longest()
    {
        Settings(enforce: false, cap: 10, max: 5);
        Sample();
        await Goal("ga", members: [("ga-1", "sonnet"), ("ga-2", "sonnet"), ("ga-3", "sonnet")]);
        await Goal(Concierge.Name, "sonnet", ("concierge-w1", "sonnet"));
        await ids.Create("mine", dir, null, null);
        foreach (var n in new[] { "mine", "ga-lead", "ga-1", "ga-2", "concierge-lead" }) await ids.Start(n);
        Assert.Equal("queued", Json(ids.Start("concierge-w1")).GetProperty("state").GetString()); // the pool is full
        Assert.Equal("queued", Json(ids.Start("ga-3")).GetProperty("state").GetString());
        using (var db = store.Open()) // the Concierge's member has waited longer
            db.Exec("UPDATE identities SET updated_ts = CASE name WHEN 'concierge-w1' THEN '2026-01-01T00:00:00+00:00' ELSE '2026-01-01T00:01:00+00:00' END WHERE name IN ('concierge-w1', 'ga-3')");
        await ids.Stop("ga-1"); // a member finishes
        Assert.Equal("running", State("concierge-w1"));
        Assert.Equal("queued", State("ga-3"));
    }

    /// <summary>A Phoenix successor takes its predecessor's slot: the pool's count never moves, and nothing queued slips into the gap.</summary>
    [Fact]
    public async Task Phoenix_takes_no_new_slot()
    {
        Settings(enforce: true, cap: 2);
        Sample();
        await Goal("g9", members: [("g9-m1", "sonnet"), ("g9-m2", "sonnet")]);
        await ids.Start("g9-lead");
        var sid = Json(ids.Start("g9-m1")).GetProperty("claude_session_id").GetString()!;
        Assert.Equal("queued", Json(ids.Start("g9-m2")).GetProperty("state").GetString());
        var board = new AgentBoard(store, null!, "wait {0}");
        var me = new Caller(sid, "g9-m1", dir, "claude-code", 1, "g9-m1");
        await ids.Torch(me, board.PassTheTorch(me, "Owns the lexer.", null));
        await ids.AfterTurn(JsonDocument.Parse($$"""{"session_id": "{{sid}}"}""").RootElement, Task.FromResult(""));
        for (var sw = Stopwatch.StartNew(); Row("g9-m1").GetProperty("generation").GetInt32() != 2 || Row("g9-m1").GetProperty("pid").ValueKind == JsonValueKind.Null; await Task.Delay(50))
        {
            Assert.Equal(2, (int)ids.Counts()["running"]!);
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(15), "no generation 2");
        }
        await ids.Tick();
        Assert.Equal((2, "queued"), ((int)ids.Counts()["running"]!, State("g9-m2")));
    }
}
