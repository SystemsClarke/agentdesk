using System.Text.Json;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>The requests AgentDesk's window sends (docs/ui-api.md).</summary>
public sealed class UiTests : IDisposable
{
    readonly string path = Path.Combine(Path.GetTempPath(), $"ui-{Guid.NewGuid():N}.db");
    readonly BoardStore store;
    readonly AgentBoard board;
    readonly long thread;

    public UiTests()
    {
        store = new BoardStore(path);
        store.Init();
        board = new AgentBoard(store, null!, "wait {0}");
        using var db = store.Open();
        thread = db.StartThread("question", "Which branch?", "builder", "agent", "main or dev?");
    }

    [Fact]
    public async Task Reply_answers_the_question_and_queues_the_askers_ack()
    {
        var mid = JsonDocument.Parse(await board.JohnReplies((int)thread, "dev")).RootElement.GetProperty("message_id").GetInt64();
        using var db = store.Open();
        Assert.Equal("answered", db.Scalar("SELECT status FROM threads WHERE id=$t", ("t", thread)));
        Assert.Equal("builder", db.Scalar("SELECT agent FROM acks WHERE message_id=$m AND state='pending'", ("m", mid)));
        Assert.Contains("\"error\"", await board.JohnReplies((int)thread, "  "));
    }

    [Fact]
    public async Task Thread_is_read_without_a_receipt()
    {
        Assert.Contains("main or dev?", await board.PeekThread((int)thread));
        using var db = store.Open();
        Assert.Equal(1L, db.Scalar("SELECT COUNT(*) FROM messages"));
        Assert.Equal(0L, db.Scalar("SELECT (SELECT COUNT(*) FROM receipts) + (SELECT COUNT(*) FROM sessions)"));
    }

    [Fact]
    public async Task List_threads_carries_the_last_word_and_how_johns_reply_landed()
    {
        await board.JohnReplies((int)thread, "dev");
        using (var db = store.Open()) db.DeliverPendingAcks("builder", "ack");
        var row = JsonDocument.Parse(await board.ListThreads("question", null, 10, true)).RootElement.GetProperty("threads")[0];
        Assert.Equal("john", row.GetProperty("last_author").GetString());
        Assert.StartsWith("picked-up|", row.GetProperty("delivery").GetString());
    }

    [Fact]
    public async Task Post_starts_a_thread_as_john()
    {
        var tid = JsonDocument.Parse(await board.JohnPosts("discussion", "Hello", "hi all")).RootElement.GetProperty("thread_id").GetInt64();
        using var db = store.Open();
        Assert.Equal("john|human|fyi", db.Scalar("SELECT m.author || '|' || m.author_kind || '|' || t.status FROM messages m JOIN threads t ON t.id=m.thread_id WHERE t.id=$t", ("t", tid)));
        Assert.Contains("\"error\"", await board.JohnPosts("discussion", "x", " "));
        Assert.Contains("\"error\"", await board.JohnPosts("nope", "x", "y"));
    }

    [Fact]
    public async Task Unarchive_restores_the_settled_status_and_holds_it_off_the_sweep()
    {
        using (var db = store.Open()) db.Exec("UPDATE threads SET status='archived', meta='{\"archived_from\": \"closed\"}' WHERE id=$t", ("t", thread));
        Assert.Contains("\"unarchived\": true", await board.Unarchive((int)thread));
        using (var db = store.Open())
            Assert.Equal("closed|1|", db.Scalar("SELECT status || '|' || json_extract(meta,'$.archive_hold') || '|' || COALESCE(json_extract(meta,'$.archived_from'),'') FROM threads WHERE id=$t", ("t", thread)));
        Assert.Contains("\"unarchived\": false", await board.Unarchive((int)thread));
    }

    [Fact]
    public async Task Status_reads_the_slack_and_worker_heartbeats()
    {
        var data = Directory.CreateDirectory(path + ".data").FullName;
        File.WriteAllText(Path.Combine(data, "slack_bridge.state"), """{"ts": "2026-09-24T17:53:04+00:00", "poll_s": 15, "last_relay": {"ts": "2026-09-24T17:50:48+00:00", "thread_id": 3}}""");
        File.WriteAllText(Path.Combine(data, "worker.state"), $$"""{"pid": {{Environment.ProcessId}}, "item": null}""");
        long work;
        using (var db = store.Open())
        {
            work = db.StartThread("work", "Do it", "builder", "agent", "please");
            db.ClaimTask(work, "crew");
            db.Exec("INSERT INTO work_events (work_id, ts, kind, body) VALUES ($w, '2026-09-24T17:00:00+00:00', 'start', 'claimed')", ("w", work));
        }
        var doc = JsonDocument.Parse(await board.Heartbeats(data)).RootElement;
        var w = doc.GetProperty("worker");
        Assert.True(w.GetProperty("running").GetBoolean());
        Assert.Equal(work, w.GetProperty("held").GetInt64());
        Assert.Equal("start", w.GetProperty("events")[0].GetProperty("kind").GetString());
        Assert.Equal(3, doc.GetProperty("slack").GetProperty("last_relay").GetProperty("thread_id").GetInt32());
        File.Delete(Path.Combine(data, "worker.state"));
        File.Delete(Path.Combine(data, "slack_bridge.state"));
        doc = JsonDocument.Parse(await board.Heartbeats(data)).RootElement;
        Assert.False(doc.GetProperty("worker").GetProperty("running").GetBoolean());
        Assert.Equal(JsonValueKind.Null, doc.GetProperty("slack").ValueKind);
        Directory.Delete(data);
    }

    [Fact]
    public async Task Status_carries_the_usage_meter()
    {
        var data = Directory.CreateDirectory(path + ".usage").FullName;
        var feed = Path.Combine(data, "claude_usage.json");
        var now = DateTimeOffset.Parse("2026-09-24T17:00:00+00:00");
        Assert.Equal("no feed yet (add scripts/claude_usage_feed.py to your status line)", Usage.Report(feed, now)["summary"]!.GetValue<string>());
        File.WriteAllText(feed, """{"extra": {"spent": 1099.99, "limit": 1100, "captured_ts": "2026-09-24T14:50:00+00:00"}}""");
        Assert.True(Usage.Apply(feed, "Current session: 72% used · resets Sep 25, 11:30pm (America/New_York)\nCurrent week (all models): 80% used", now));
        var left = Usage.Span((new DateTimeOffset(new DateTime(2026, 9, 25, 23, 30, 0, DateTimeKind.Local)) - now.AddMinutes(1)).TotalSeconds);
        var r = Usage.Report(feed, now.AddMinutes(1));
        Assert.Equal([$"time left: {left} in your 5-hour window, 72% used", "time left: unknown on the week, 80% used, getting close",
            "monthly spend: $1,099.99 of $1,100 (100%) · nearly capped, as of 2h 11m ago"], r["lines"]!.AsArray().Select(l => l!.GetValue<string>()));
        Assert.Equal($"5h 72%, resets in {left} · week 80% · spend $1,099.99 of $1,100 (100%) · nearly capped, as of 2h 11m ago · reported 1m ago",
            r["summary"]!.GetValue<string>());
        Assert.StartsWith("5h 72%", JsonDocument.Parse(await board.Heartbeats(data)).RootElement.GetProperty("usage").GetProperty("summary").GetString());
        Directory.Delete(data, true);
    }

    [Fact]
    public async Task A_subscriber_is_pushed_a_write_made_elsewhere()
    {
        Log.Path = path + ".log";
        Environment.SetEnvironmentVariable("AGENTDESK_DATA", Path.GetTempPath()); // were the core ever auto-started, not the live board
        Environment.SetEnvironmentVariable("AGENTDESK_PIPE", $"agentdesk-test-{Guid.NewGuid():N}");
        var watch = new BoardWatch(store);
        using var stop = new CancellationTokenSource();
        _ = PipeServer.Run((_, push, gone) => Task.FromResult(watch.Subscribe(push, gone)), stop.Token);
        var core = await CoreConnection.Connect(new Caller(null, null, null, "ui", Environment.ProcessId));
        var pushed = new TaskCompletionSource<string>();
        core.Pushed += e => pushed.TrySetResult(e);
        await core.Call("ui:subscribe");
        using (var db = store.Open()) db.Reply(thread, "other", "agent", "written by another process");
        Assert.Equal("""{"event":"board.changed"}""", await pushed.Task.WaitAsync(TimeSpan.FromSeconds(5)));
        core.Dispose();
        stop.Cancel();
        await Task.Delay(600); // the watcher sees nobody left and closes its connection
    }

    public void Dispose()
    {
        foreach (var f in Directory.GetFiles(Path.GetTempPath(), Path.GetFileName(path) + "*")) File.Delete(f);
    }
}
