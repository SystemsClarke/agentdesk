using System.Text.Json;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>The swarm slots: the core only stores slot -> channel, persona and goal; the bridge does the Slack calls.</summary>
public sealed class SlotsTests
{
    readonly BoardStore store = new(Path.Combine(Directory.CreateTempSubdirectory("slots-").FullName, "agentdesk.db"));
    readonly Slots slots;

    public SlotsTests()
    {
        slots = new Slots(store);
        using var db = store.Open();
        foreach (var g in new[] { "alpha", "beta" })
            db.Exec("INSERT INTO goals (name, objective, measure_folder, lead, state, created_ts, updated_ts) VALUES ($n,'o','C:\\','x','running',$ts,$ts)",
                ("n", g), ("ts", db.NowIso()));
        db.Exec("INSERT INTO experiments (goal, n, change, started_ts, measured_ts, value, verdict) VALUES ('alpha',1,'c',$ts,$ts,42,'no gain')", ("ts", db.NowIso()));
    }

    static JsonElement Json(Task<string> t) => JsonDocument.Parse(t.Result).RootElement;

    JsonElement Slot(int n) => Json(slots.List()).GetProperty("slots").EnumerateArray().Single(s => s.GetProperty("n").GetInt32() == n);

    [Fact]
    public void List_has_ten_empty_slots_to_begin_with()
    {
        var all = Json(slots.List()).GetProperty("slots").EnumerateArray().ToList();
        Assert.Equal(Enumerable.Range(1, 10), all.Select(s => s.GetProperty("n").GetInt32()));
        Assert.All(all, s => Assert.Equal(JsonValueKind.Null, s.GetProperty("goal").ValueKind));
    }

    [Fact]
    public void Assign_records_the_channel_persona_and_goal_and_joins_the_goals_state()
    {
        var r = Json(slots.Assign(3, "alpha", "alpha", ":robot_face:", "C123", "swarm-alpha"));
        Assert.Equal("alpha", r.GetProperty("goal").GetString());
        Assert.Equal("C123", r.GetProperty("channel_id").GetString());
        var s = Slot(3);
        Assert.Equal("swarm-alpha", s.GetProperty("channel_name").GetString());
        Assert.Equal("alpha", s.GetProperty("persona_name").GetString());
        Assert.Equal("running", s.GetProperty("state").GetString());
        Assert.Equal(42, s.GetProperty("last_value").GetDouble());

        slots.Assign(3, null, null, null, "C999", null); // what is not given is kept
        Assert.Equal("alpha", Slot(3).GetProperty("goal").GetString());
        Assert.Equal("C999", Slot(3).GetProperty("channel_id").GetString());
    }

    [Fact]
    public void Clear_frees_the_slot_but_keeps_its_channel()
    {
        slots.Assign(1, "alpha", "alpha", null, "C1", "swarm-alpha");
        var r = Json(slots.Clear(1));
        Assert.Equal(JsonValueKind.Null, r.GetProperty("goal").ValueKind);
        Assert.Equal(JsonValueKind.Null, r.GetProperty("persona_name").ValueKind);
        Assert.Equal("C1", r.GetProperty("channel_id").GetString());
        slots.Assign(2, "alpha", "alpha", null, null, null); // freed, so the goal may go elsewhere
    }

    [Fact]
    public void Bad_slots_and_goals_are_refused()
    {
        Assert.Throws<ArgumentException>(() => slots.Assign(0, "alpha", null, null, null, null).Wait());
        Assert.Throws<ArgumentException>(() => slots.Assign(11, "alpha", null, null, null, null).Wait());
        Assert.Throws<ArgumentException>(() => slots.Clear(11).Wait());
        Assert.Throws<ArgumentException>(() => slots.Assign(1, "nope", null, null, null, null).Wait());
        slots.Assign(1, "alpha", null, null, null, null);
        var e = Assert.Throws<ArgumentException>(() => slots.Assign(2, "ALPHA", null, null, null, null).Wait());
        Assert.Contains("already in slot 1", e.Message);
    }
}
