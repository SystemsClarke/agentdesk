using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// The 10 swarm slots (docs/GOAL.md): each a reusable Slack channel with a persona, hosting at most one goal. The core only
/// stores the mapping; the Slack bridge (agentdesk/swarm.py) makes every Slack call and writes what it did back here.
/// </summary>
public sealed class Slots
{
    public const int Count = 10;
    readonly BoardStore store;

    public Slots(BoardStore store) { this.store = store; store.Init(); }

    const string ListSql = """
        WITH RECURSIVE k(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM k WHERE n < 10)
        SELECT k.n, s.channel_id, s.channel_name, s.persona_name, s.persona_icon, s.goal, s.updated_ts,
          g.state, g.lead, g.thread_id, g.objective,
          (SELECT value FROM experiments e WHERE e.goal=g.name AND e.measured_ts IS NOT NULL ORDER BY e.n DESC LIMIT 1) AS last_value,
          (SELECT COUNT(*) FROM goal_members m WHERE m.goal=g.name) AS members
        FROM k LEFT JOIN slots s ON s.n=k.n LEFT JOIN goals g ON g.name=s.goal ORDER BY k.n
        """;

    /// <summary>All 10 slots, empty ones included, each with its goal's state, lead, thread, last measured value and member count.</summary>
    public Task<string> List()
    {
        using var db = store.Open();
        return Ok(new JsonObject { ["slots"] = new JsonArray([.. db.Rows(ListSql)]) });
    }

    /// <summary>Puts a goal (and its persona) in slot n, and records the channel the bridge made or renamed. Fields not given keep their value.</summary>
    public Task<string> Assign(int n, string? goal, string? persona, string? icon, string? channelId, string? channelName)
    {
        Check(n);
        using var db = store.Open();
        if (goal is not null)
        {
            if (db.Scalar("SELECT 1 FROM goals WHERE name=$g", ("g", goal)) is null) throw new ArgumentException($"no such goal: {goal}");
            if (db.Scalar("SELECT n FROM slots WHERE goal=$g AND n<>$n", ("g", goal), ("n", n)) is long other) throw new ArgumentException($"goal {goal} is already in slot {other}");
        }
        db.Exec("""
            INSERT INTO slots (n, channel_id, channel_name, persona_name, persona_icon, goal, updated_ts) VALUES ($n,$ci,$cn,$p,$i,$g,$ts)
            ON CONFLICT(n) DO UPDATE SET channel_id=COALESCE($ci, channel_id), channel_name=COALESCE($cn, channel_name),
              persona_name=COALESCE($p, persona_name), persona_icon=COALESCE($i, persona_icon), goal=COALESCE($g, goal), updated_ts=$ts
            """, ("n", n), ("ci", channelId), ("cn", channelName), ("p", persona), ("i", icon), ("g", goal), ("ts", db.NowIso()));
        return Row(db, n);
    }

    /// <summary>Frees slot n: no goal, no persona. Its channel stays, for the next swarm to rename and reuse.</summary>
    public Task<string> Clear(int n)
    {
        Check(n);
        using var db = store.Open();
        db.Exec("UPDATE slots SET goal=NULL, persona_name=NULL, persona_icon=NULL, updated_ts=$ts WHERE n=$n", ("n", n), ("ts", db.NowIso()));
        return Row(db, n);
    }

    static void Check(int n)
    {
        if (n is < 1 or > Count) throw new ArgumentException($"a slot is 1 to {Count}");
    }

    static Task<string> Row(BoardDb db, int n) => Ok(db.Rows(ListSql).Single(r => (long)r["n"]! == n));

    static Task<string> Ok(JsonObject o) => Task.FromResult(o.ToJsonString(Wire.Indented));
}
