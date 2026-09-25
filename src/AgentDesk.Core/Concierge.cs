using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// The Concierge (docs/GOAL.md): the standing goal <c>concierge</c> that keeps Work to Hire drained. The core measures the open
/// items itself (internal:open_work) and wakes the lead, concierge-lead, while any wait; the lead claims each one and hands it to
/// a member (member_spawn with work_id), which completes it with a report on its thread. Off until John turns it on (Ctrl+W, Slack
/// `concierge on`, the ops console), and that is its approval: no per-item one.
/// </summary>
public sealed class Concierge(BoardStore store, Goals goals, string folder)
{
    public const string Name = "concierge", Lead = Name + "-lead";

    const string Charter = """
        You are the AgentDesk Concierge: the standing lead that keeps Work to Hire (the board's `work` channel) drained. You triage
        and dispatch; members do the work. The core wakes you while open items wait, and member_done wakes you too. For each open
        item, oldest first:
        1. list_work status=open, read_thread the item, then claim_work it. Never pass `author`: you post as concierge-lead.
           Items posted with claim=anyone are reserved for an agent to take deliberately: leave them alone.
        2. Decide whether it needs a swarm. Most items need one member; split only work that is genuinely parallel.
        3. member_spawn goal=concierge, name=w<item id> (w<id>-b, ... for more), task=the item restated so it stands alone,
           work_id=<item id> (this hands your claim to that member; one member holds it), and model=sonnet, or haiku for
           mechanical work (renames, lookups, summaries, formatting). Members without work_id report through member_done.
        4. The member completes the item with complete_work (its report lands on the item's thread) and retires with member_done.
           One that retires without completing hands the item back to you: re-dispatch it, or complete_work it yourself saying
           why it could not be done.
        Stay within the goal's max_members: when it is full, stop; member_done wakes you. A decision only John can make goes to
        ask_human, never a guess. Between wakes, stop.
        """;

    readonly Goals.StandingGoal spec = new(Name, "Keep Work to Hire drained: every open item claimed, worked by a small swarm, and completed with a report on its thread.",
        folder, Charter, "sonnet", "A lead that triages each open item into a small swarm keeps the queue empty.", "internal:open_work", "value <= 0",
        MaxMembers: 3, CadenceMinutes: 10);

    /// <summary>ui:concierge. With <paramref name="on"/> (John only) it turns the Concierge on (creating or approving the goal) or
    /// off (the goal stops: members forgotten, the lead stopped, held items back on the queue). Returns the state either way.</summary>
    public async Task<string> Toggle(Caller c, bool? on)
    {
        if (on is { } want)
        {
            if (!Goals.John(c)) throw new ArgumentException("only John turns the Concierge on or off");
            if (want) await goals.Ensure(c, spec);
            else if (Running()) await goals.Stop(c, Name);
        }
        return State().ToJsonString(Wire.Indented);
    }

    bool Running()
    {
        using var db = store.Open();
        return db.Scalar("SELECT state FROM goals WHERE name=$n", ("n", Name)) as string == "running";
    }

    /// <summary>ui:status's "concierge": on, the goal's state, the lead's, the open count the measure sees, the members, and
    /// the work items the Concierge holds (newest first).</summary>
    public JsonObject State()
    {
        using var db = store.Open();
        var g = db.Rows("SELECT state, thread_id FROM goals WHERE name=$n", ("n", Name)).FirstOrDefault();
        var members = db.Rows("SELECT identity, task, work_id FROM goal_members WHERE goal=$n ORDER BY created_ts", ("n", Name));
        var mine = members.Select(m => m["identity"]!.ToString()).Append(Lead).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var held = db.Rows("SELECT id, meta FROM threads WHERE channel='work' AND status='claimed' ORDER BY updated_ts DESC")
            .Where(t => Goals.Assignee(t) is { } who && mine.Contains(who)).Select(t => (JsonNode?)(long)t["id"]!);
        return new()
        {
            ["on"] = g?["state"]?.ToString() == "running",
            ["state"] = g?["state"]?.ToString() ?? "off",
            ["lead"] = Lead,
            ["lead_state"] = db.Scalar("SELECT state FROM identities WHERE name=$n", ("n", Lead)) as string,
            ["thread_id"] = g?["thread_id"]?.DeepClone(),
            ["open"] = Goals.MeasureInternal(db, "internal:open_work"),
            ["members"] = new JsonArray([.. members]),
            ["held"] = new JsonArray([.. held]),
        };
    }
}
