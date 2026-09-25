using System.Diagnostics;
using System.Text.Json;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>The Concierge (a standing goal over Work to Hire) and the goal machinery it needs: the internal open_work measure,
/// standing goals, work items handed from the lead to a member. A PowerShell script stands in for claude, as in GoalsTests.</summary>
public sealed class ConciergeTests : IDisposable
{
    readonly string dir = Directory.CreateTempSubdirectory("concierge-").FullName;
    readonly BoardStore store;
    readonly Sessions sessions = new();
    readonly Identities ids;
    readonly Goals goals;
    readonly Concierge concierge;
    readonly AgentBoard board;
    readonly Caller john;

    public ConciergeTests()
    {
        File.WriteAllText(Path.Combine(dir, "settings.json"), """{"max_sessions": 6}""");
        var script = Path.Combine(dir, "standin.ps1");
        File.WriteAllText(script, "\"ready $env:AGENTDESK_AUTHOR\"\nwhile ($null -ne ($l = [Console]::In.ReadLine())) { \"got: $l\" }\n");
        store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
        ids = new Identities(store, sessions, dir, $"powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{script}\"");
        goals = new Goals(store, ids, sessions);
        concierge = new Concierge(store, goals, dir);
        board = new AgentBoard(store, null!, "wait {0}");
        john = new Caller(null, null, dir, null, 1);
    }

    public void Dispose()
    {
        using (var db = store.Open()) db.Exec("DELETE FROM identities");
        foreach (var n in Json(sessions.List()).GetProperty("sessions").EnumerateArray())
            try { sessions.Stop(n.GetProperty("name").GetString()!).Wait(); } catch (AggregateException) { }
    }

    static JsonElement Json(Task<string> t) => JsonDocument.Parse(t.Result).RootElement;

    JsonElement? Identity(string name) => Json(ids.List()).GetProperty("identities").EnumerateArray().Cast<JsonElement?>().SingleOrDefault(r => r!.Value.GetProperty("name").GetString() == name);

    string State(string name) => Identity(name)?.GetProperty("state").GetString() ?? "gone";

    Caller As(string name) => new(Identity(name)!.Value.GetProperty("claude_session_id").GetString(), name, dir, "claude-code", 1, name);

    long Post(string subject, string claim = "auto") => JsonDocument.Parse(board.PostWork(new Caller("s1", "poster", dir, "claude-code", 1), subject, "please do " + subject, null, claim).Result)
        .RootElement.GetProperty("thread_id").GetInt64();

    string? Scalar(string sql, params (string, object?)[] args) { using var db = store.Open(); return db.Scalar(sql, args)?.ToString(); }

    static async Task Until(Func<bool> ok, string what)
    {
        for (var sw = Stopwatch.StartNew(); !ok(); await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(20), $"never: {what}");
    }

    [Fact]
    public async Task Toggling_on_creates_and_approves_the_goal_and_off_stops_it()
    {
        var off = Json(concierge.Toggle(john, null));
        Assert.Equal((false, "off"), (off.GetProperty("on").GetBoolean(), off.GetProperty("state").GetString()));
        var agent = new Caller(Guid.NewGuid().ToString(), "someone", dir, "claude-code", 1, "someone");
        await Assert.ThrowsAsync<ArgumentException>(() => concierge.Toggle(agent, true)); // only John turns it on
        Assert.Null(Scalar("SELECT name FROM goals"));

        var on = Json(concierge.Toggle(john, true));
        Assert.True(on.GetProperty("on").GetBoolean());
        var g = Json(goals.Status("concierge"));
        Assert.Equal(("running", 1, "internal:open_work", "value <= 0", "concierge-lead"), (g.GetProperty("state").GetString(), g.GetProperty("standing").GetInt32(),
            g.GetProperty("measure_cmd").GetString(), g.GetProperty("success").GetString(), g.GetProperty("lead").GetString()));
        Assert.Equal(("stopped", "sonnet"), (State("concierge-lead"), Identity("concierge-lead")!.Value.GetProperty("model").GetString())); // nothing to do: not launched
        Assert.Contains("keeps Work to Hire", Identity("concierge-lead")!.Value.GetProperty("charter").GetString());
        await concierge.Toggle(john, true); // on again: no second goal, no error
        Assert.Equal("1", Scalar("SELECT COUNT(*) FROM goals"));

        Post("tidy the readme");
        await goals.Tick(); // an open item: the lead is started with the goal's status
        Assert.Equal("running", State("concierge-lead"));
        var lead = As("concierge-lead");
        var item = Post("fix the build");
        Assert.True(Json(board.ClaimWork(lead, (int)item, null)).GetProperty("claimed").GetBoolean());
        await goals.Spawn(lead, "concierge", "w" + item, "fix the build", "haiku", item);
        Assert.Equal("running", State($"concierge-w{item}"));

        var stopped = Json(concierge.Toggle(john, false));
        Assert.Equal((false, "stopped"), (stopped.GetProperty("on").GetBoolean(), stopped.GetProperty("state").GetString()));
        await Until(() => State($"concierge-w{item}") == "gone" && State("concierge-lead") == "stopped", "members forgotten, the lead stopped");
        Assert.Equal("open", Scalar("SELECT status FROM threads WHERE id=$i", ("i", item))); // what it held is back on the queue
        Assert.Null(Scalar("SELECT json_extract(meta, '$.assignee') FROM threads WHERE id=$i", ("i", item)));

        Assert.True(Json(concierge.Toggle(john, true)).GetProperty("on").GetBoolean()); // and on again later
        Assert.Equal("running", Scalar("SELECT state FROM goals WHERE name='concierge'"));
    }

    [Fact]
    public async Task The_internal_open_work_measure_counts_unclaimed_items()
    {
        using (var db = store.Open()) Assert.Equal(0, Goals.MeasureInternal(db, "internal:open_work"));
        Post("one");
        Post("two");
        Post("reserved for a deliberate claim", "anyone");
        var claimed = Post("claimed");
        var done = Post("done");
        using (var db = store.Open())
        {
            db.ClaimTask(claimed, "x");
            db.ClaimTask(done, "x");
            db.CompleteTask(done, "x");
            db.StartThread("discussion", "not work", "x", "agent", "hi");
            Assert.Equal(2, Goals.MeasureInternal(db, "internal:open_work"));
            Assert.Throws<ArgumentException>(() => Goals.MeasureInternal(db, "internal:nope"));
        }
        await Assert.ThrowsAsync<ArgumentException>(() => goals.Create("draftish", "x", dir).ContinueWith(_ =>
            goals.Propose(john, "draftish", "h", "internal:nope", "value <= 0", 1)).Unwrap()); // a typo is refused at proposal
        Assert.Equal(2.0, Json(concierge.Toggle(john, null)).GetProperty("open").GetDouble());
    }

    [Fact]
    public async Task A_standing_goal_idles_at_its_line_and_does_not_stop()
    {
        await concierge.Toggle(john, true);
        await goals.Tick();
        await goals.Tick();
        Assert.Equal(("running", "stopped"), (Scalar("SELECT state FROM goals"), State("concierge-lead"))); // met (0 open): idle, and the lead never woke
        Assert.NotNull(Scalar("SELECT woke_ts FROM goals")); // re-checked

        // A measure that meets the line through experiment_done does not end it either.
        var n = Json(goals.ExperimentStart(john, "concierge", "nothing to do")).GetProperty("n").GetInt32();
        Assert.Equal("met", Json(goals.ExperimentDone(john, "concierge", n)).GetProperty("verdict").GetString());
        Assert.Equal("running", Scalar("SELECT state FROM goals"));
        Assert.DoesNotContain("**Goal succeeded**", Scalar("SELECT group_concat(body) FROM messages"));

        // Off its line the lead is woken, once per change of value rather than every tick.
        using (var db = store.Open()) db.Exec("UPDATE goals SET started_ts='2000-01-01T00:00:00+00:00'"); // a normal goal would be exhausted by now
        Post("first");
        await goals.Tick();
        Assert.Equal("running", State("concierge-lead"));
        var woke = Scalar("SELECT woke_ts FROM goals");
        await Task.Delay(1100);
        await goals.Tick(); // same value, cadence not due: no second wake
        Assert.Equal(woke, Scalar("SELECT woke_ts FROM goals"));
        Post("second");
        await goals.Tick(); // the value changed: woken again
        Assert.NotEqual(woke, Scalar("SELECT woke_ts FROM goals"));
        Assert.Equal("running", Scalar("SELECT state FROM goals"));
        Assert.Contains("now 2", Json(goals.Status("concierge")).GetProperty("summary").GetString());
    }

    [Fact]
    public async Task A_work_item_is_claimed_handed_to_a_member_and_completed_with_a_report()
    {
        await concierge.Toggle(john, true);
        var item = Post("rename the tabs");
        await goals.Tick();
        var lead = As("concierge-lead"); // the stand-in lead, started because an item was open

        // The lead claims it and dispatches one member with the item.
        await Assert.ThrowsAsync<ArgumentException>(() => goals.Spawn(lead, "concierge", "w1", "rename the tabs", "haiku", item)); // not claimed yet
        Assert.True(Json(board.ClaimWork(lead, (int)item, null)).GetProperty("claimed").GetBoolean());
        await goals.Spawn(lead, "concierge", "w1", "rename the tabs", "haiku", item);
        Assert.Equal("concierge-w1", Scalar("SELECT json_extract(meta, '$.assignee') FROM threads WHERE id=$i", ("i", item)));
        Assert.Contains($"complete_work thread_id={item}", Identity("concierge-w1")!.Value.GetProperty("charter").GetString());
        var state = Json(concierge.Toggle(john, null));
        Assert.Equal(item, state.GetProperty("held")[0].GetInt64());
        Assert.Equal(item, state.GetProperty("members")[0].GetProperty("work_id").GetInt64());

        // The member completes it with its report on the item's thread, then retires.
        var member = As("concierge-w1");
        Assert.False(Json(board.CompleteWork(lead, (int)item, "not mine any more", null)).GetProperty("completed").GetBoolean());
        Assert.True(Json(board.CompleteWork(member, (int)item, "Renamed the three tabs; screenshots on the PR.", null)).GetProperty("completed").GetBoolean());
        await goals.MemberDone(member, "renamed the tabs");
        await Until(() => State("concierge-w1") == "gone", "the member is forgotten");
        Assert.Equal("done", Scalar("SELECT status FROM threads WHERE id=$i", ("i", item)));
        Assert.Equal("concierge-w1|Renamed the three tabs; screenshots on the PR.",
            Scalar("SELECT author || '|' || body FROM messages WHERE thread_id=$i ORDER BY id DESC LIMIT 1", ("i", item)));
        Assert.Equal("running", Scalar("SELECT state FROM goals"));

        // A member that retires without completing hands the item back to the lead.
        var next = Post("write the changelog");
        await board.ClaimWork(lead, (int)next, null);
        await goals.Spawn(lead, "concierge", "w2", "write the changelog", null, next);
        await goals.MemberDone(As("concierge-w2"), "could not find the release notes");
        Assert.Equal("claimed|concierge-lead", Scalar("SELECT status || '|' || json_extract(meta, '$.assignee') FROM threads WHERE id=$i", ("i", next)));
        Assert.Contains($"Work item #{next} was not completed: it is back with concierge-lead.", Scalar("SELECT group_concat(body) FROM messages"));
    }
}
