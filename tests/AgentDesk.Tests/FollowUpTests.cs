using System.Text.Json.Nodes;
using AgentDesk.Core.Board;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>The #373 problem: the agent that asked posts again after John answered. The tray toasts it and the window's
/// LAST WORD marks it "done" or "follow-up"; both read the same query.</summary>
public sealed class FollowUpTests : IDisposable
{
    readonly string path = Path.Combine(Path.GetTempPath(), $"fu-{Guid.NewGuid():N}.db");
    readonly BoardStore store;
    readonly BoardDb db;
    readonly long thread;

    public FollowUpTests()
    {
        store = new BoardStore(path);
        store.Init();
        db = store.Open();
        thread = db.StartThread("question", "May I cancel the job?", "builder", "agent", "One job on bld-193 blocks the sync. Cancel it?");
    }

    public void Dispose()
    {
        db.Dispose();
        foreach (var f in new[] { path, path + "-wal", path + "-shm" }) try { File.Delete(f); } catch (IOException) { }
    }

    List<JsonObject> All() => db.FollowUps(0, db.MaxMessageId());
    string? Mark() => (string?)db.ListThreads("question", null, 10).Single()["follow_up"];

    [Fact]
    public void Nothing_before_john_answers()
    {
        db.Reply(thread, "builder", "agent", "Still blocked, by the way.");
        Assert.Empty(All());
        Assert.Null(Mark());
    }

    [Fact]
    public void Acks_receipts_and_other_agents_are_not_follow_ups()
    {
        db.JohnReplies(thread, "Yes, go ahead");
        db.DeliverPendingAcks("builder", "Acknowledged - your reply has been picked up.");
        db.Reply(thread, "builder", "agent", "builder picked this thread up.", meta: new JsonObject { ["kind"] = "read-receipt" });
        db.Reply(thread, "compile", "agent", "I'm blocked on this too.");
        Assert.Empty(All());
        Assert.Null(Mark());
    }

    [Fact]
    public void The_openers_post_after_johns_reply_is_one_and_says_done()
    {
        db.JohnReplies(thread, "Yes, go ahead");
        var id = db.Reply(thread, "builder", "agent", "**Done.** I cancelled the stage.\nThe other 8 jobs had passed.");
        var f = Assert.Single(All());
        Assert.Equal(id, (long)f["message_id"]!);
        Assert.Equal("done", (string)f["mark"]!);
        Assert.Equal("done", Mark());
        Assert.Equal($"#{thread} May I cancel the job?: **Done.** I cancelled the stage.", Tray.FollowUpText(f));
    }

    [Fact]
    public void Anything_else_is_a_follow_up_and_johns_next_reply_settles_it()
    {
        db.JohnReplies(thread, "Yes");
        db.Reply(thread, "builder", "agent", "Doneness check failed: one more question, may I also retry #90?");
        Assert.Equal("follow-up", (string)Assert.Single(All())["mark"]!);
        Assert.Equal("follow-up", Mark());

        db.JohnReplies(thread, "yes");
        Assert.Null(Mark());
    }

    [Fact]
    public void The_tray_window_sees_only_new_messages()
    {
        db.JohnReplies(thread, "Yes");
        var before = db.MaxMessageId();
        var first = db.Reply(thread, "builder", "agent", "Done.");
        var top = db.MaxMessageId();
        db.Reply(thread, "builder", "agent", "Also: #90 scheduled.");
        Assert.Equal(first, (long)Assert.Single(db.FollowUps(before, top))["message_id"]!);
        Assert.Equal("follow-up", (string)Assert.Single(db.FollowUps(top, db.MaxMessageId()))["mark"]!);
    }
}
