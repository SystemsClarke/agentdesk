using AgentDesk.Core;
using AgentDesk.Core.Board;
using Xunit;

namespace AgentDesk.Tests;

public sealed class DbSpeedTests : IDisposable
{
    readonly string dir = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "agentdesk-db-" + Guid.NewGuid().ToString("N"))).FullName;
    string Db => Path.Combine(dir, "agentdesk.db");

    public void Dispose()
    {
        try { Directory.Delete(dir, true); } catch (IOException) { }
    }

    [Fact]
    public void A_page_of_threads_is_the_newest_with_ties_broken_by_id_and_counts_every_message()
    {
        var store = new BoardStore(Db, () => new DateTimeOffset(2026, 10, 4, 12, 0, 0, TimeSpan.Zero)); // every thread gets the same updated_ts
        store.Init();
        using var db = store.Open();
        var ids = Enumerable.Range(0, 6).Select(i => db.StartThread("discussion", $"t{i}", "a", "agent", "first")).ToList();
        db.Reply(ids[1], "b", "agent", "second");
        db.Reply(ids[1], "b", "agent", "third");
        var page = db.ListThreads("discussion", null, 4);
        Assert.Equal(ids.AsEnumerable().Reverse().Take(4).Select(i => (long)i), page.Select(r => (long)r["id"]!)); // newest id first on a tie: the same page every time
        Assert.Equal(4, db.ListThreads(null, null, 4).Count);
        Assert.Equal(6, db.ListThreads(null, null, 50).Count);
        var again = db.ListThreads("discussion", null, 50);
        Assert.Equal(3L, (long)again.Single(r => (long)r["id"]! == ids[1])["message_count"]!);
        Assert.Equal("third", (string?)again.Single(r => (long)r["id"]! == ids[1])["last_body"]);
        Assert.False(db.ListThreads("discussion", null, 50, lastBody: false)[0].ContainsKey("last_body"));
        Assert.Empty(db.ListThreads("question", null, 50)); // the channel filter applies before the page, not after it
    }

    [Fact]
    public void A_board_at_the_current_schema_version_is_not_migrated_again_and_an_unversioned_one_is()
    {
        var store = new BoardStore(Db);
        store.Init();
        using (var db = store.Open())
        {
            var version = (long)db.Scalar("PRAGMA user_version")!;
            Assert.NotEqual(0, version);
            db.Exec("DROP VIEW open_questions");
            store.Init(); // at the current version: left alone, the view stays dropped (proves InitDb returned early)
            Assert.Null(db.Scalar("SELECT 1 FROM sqlite_master WHERE name='open_questions'"));
            db.Exec("PRAGMA user_version=0"); // a board from before versions, or from another build
        }
        store.Init();
        using var after = store.Open();
        Assert.NotNull(after.Scalar("SELECT 1 FROM sqlite_master WHERE name='open_questions'")); // migrated again, view back
        Assert.NotEqual(0, (long)after.Scalar("PRAGMA user_version")!);
    }

    [Fact]
    public void The_anchor_keeps_the_write_ahead_log_between_calls_and_blocks_nobody()
    {
        using var store = new BoardStore(Db);
        store.Init();
        store.Anchor();
        store.Anchor(); // once is enough
        for (var i = 0; i < 3; i++)
        {
            using var db = store.Open();
            db.StartThread("discussion", $"t{i}", "a", "agent", "x");
        }
        Assert.True(File.Exists(Db + "-wal"), "with a connection held open, closing the others does not delete the log");
        using var read = store.Open();
        Assert.Equal(3, read.ListThreads(null, null, 10).Count);
        using var write = store.Open();
        write.StartThread("discussion", "after", "a", "agent", "x"); // the anchor holds no lock
    }

    [Fact]
    public void A_write_that_fails_part_way_leaves_nothing_behind()
    {
        var store = new BoardStore(Db);
        store.Init();
        using var db = store.Open();
        var id = db.StartThread("discussion", "t", "a", "agent", "x");
        Assert.ThrowsAny<Exception>(() => db.Reply(id + 99, "a", "agent", "to a thread that is not there")); // the foreign key refuses it
        Assert.Equal(1L, (long)db.Scalar("SELECT COUNT(*) FROM messages")!);
        db.Reply(id, "a", "agent", "fine"); // and the connection is usable: the failed transaction was rolled back, not left open
        Assert.Equal(2L, (long)db.Scalar("SELECT COUNT(*) FROM messages")!);
    }

    [Fact]
    public void The_governors_trained_model_is_kept_between_verdicts_but_never_outlives_a_new_sample_or_a_settings_change()
    {
        var store = new BoardStore(Db);
        store.Init();
        var now = new DateTimeOffset(2026, 10, 4, 12, 0, 0, TimeSpan.Zero);
        var reset = now.AddDays(5);
        using var db = store.Open();
        for (var i = 40; i >= 1; i--) Governor.Insert(db, new(now.AddMinutes(-5 * i), 10 + (40 - i) * 0.05, reset, 1, null, 2));
        Governor.Insert(db, new(now, 12.0, reset, 1, null, 2));
        var first = Governor.Judge(db, dir, now, false);
        Assert.Equal(12.0, first.Advice!.Used);
        Assert.Equal(first.Advice.Reason, Governor.Judge(db, dir, now, false).Advice!.Reason); // the second is the kept model, the same answer
        Governor.Insert(db, new(now.AddMinutes(5), 30.0, reset, 1, null, 2)); // a new sample: the next verdict sees it
        Assert.Equal(30.0, Governor.Judge(db, dir, now.AddMinutes(5), false).Advice!.Used);
        Assert.False(Governor.Judge(db, dir, now.AddMinutes(5), false).Enforce);
        File.WriteAllText(Path.Combine(dir, "settings.json"), "{\"governor_enforce\": true, \"governor_max_sessions\": 1}"); // a settings change: seen at once
        var after = Governor.Judge(db, dir, now.AddMinutes(5), false);
        Assert.True(after.Enforce);
        Assert.True(after.Advice!.TotalSessions <= 1);
        var emptyStore = new BoardStore(Path.Combine(dir, "empty.db"));
        emptyStore.Init();
        using var empty = emptyStore.Open();
        Assert.Null(Governor.Judge(empty, dir, now, false).Advice); // no samples: still the fail-closed answer
    }
}
