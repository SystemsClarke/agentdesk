using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// The Concierge (docs/GOAL.md, docs/DISPATCH.md): the standing goal <c>concierge</c> that keeps Work to Hire drained. The core's
/// <see cref="Dispatcher"/> claims each ordinary open item and starts a worker for it, in priority order; the lead,
/// concierge-lead, is woken (internal:open_triage) only for items it must split. A worker completes its item with a report on its thread. Off until John turns it on (Ctrl+W, Slack
/// `concierge on`, the ops console), and that is its approval: no per-item one.
/// </summary>
public sealed class Concierge(BoardStore store, Goals goals, string folder)
{
    public const string Name = "concierge", Lead = Name + "-lead";
    /// <summary>Workers at once while the week is on pace; the Dispatcher lifts it while the week is behind.</summary>
    public const int BaseMembers = 3;

    const string Charter = """
        You are the AgentDesk Concierge's lead. The core's dispatcher already starts a worker for every ordinary open Work to Hire item,
        by priority; you only get the ones it cannot route: items marked triage (vague or too large for one worker) and items that
        three workers failed. The core wakes you while any wait. For each, oldest first:
        1. list_work status=open, read_thread the item, then claim_work it. Never pass `author`: you post as concierge-lead.
        2. Split it into small, self-contained items with post_work (priority 0-4, model haiku for mechanical work, `after` for order),
           or if it can be done directly, do it. A decision only John can make goes to ask_human, never a guess.
        3. complete_work the original with a note saying what became of it (the new item ids).
        Between wakes, stop.
        """;

    readonly Goals.StandingGoal spec = new(Name, "Keep Work to Hire drained: every open item claimed, worked by a small swarm, and completed with a report on its thread.",
        folder, Charter, "opus", "The core dispatches every ordinary open item itself, and the lead splits the rest, so the queue empties.", "internal:open_triage", "value <= 0",
        MaxMembers: BaseMembers, CadenceMinutes: 10);

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
