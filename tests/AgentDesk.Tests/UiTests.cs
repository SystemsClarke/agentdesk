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
    public async Task Check_prs_settles_only_what_github_reports()
    {
        const string merged = "https://github.com/o/r/pull/1", failing = "https://github.com/o/r/pull/2";
        using (var db = store.Open())
        {
            db.RegisterPr(merged, "o/r", 1, "one", "claude:proj#ab12", thread);
            db.RegisterPr(failing, "o/r", 2, "two", "builder", null);
        }
        var checker = new PrChecker(store, url => url == merged ? new() { ["state"] = "MERGED", ["title"] = "One!" } : throw new GhError("gh auth login"));
        _ = checker.Run(TimeSpan.FromHours(1));
        Assert.Contains("\"ok\": true", checker.Poke());
        string? Row(string url) { using var db = store.Open(); return db.Scalar("SELECT state || '|' || COALESCE(last_error, '') FROM pull_requests WHERE url=$u AND checked_ts IS NOT NULL", ("u", url)) as string; }
        for (var i = 0; i < 100 && Row(failing) is null; i++) await Task.Delay(50);
        Assert.Equal("merged|", Row(merged));
        Assert.Equal("open|gh auth login", Row(failing));
        using (var db = store.Open())
            Assert.StartsWith("agentdesk|pr-merged|Merged: One!\n\no/r#1 - https://github.com/o/r/pull/1\n\nThis was the pull request claude in proj, session ab12 asked",
                db.Scalar("SELECT author || '|' || json_extract(meta, '$.kind') || '|' || body FROM messages WHERE thread_id=$t ORDER BY id DESC LIMIT 1", ("t", thread)) as string);
        var data = Directory.CreateDirectory(path + ".prs").FullName;
        Assert.Equal(2, JsonDocument.Parse(await board.Heartbeats(data)).RootElement.GetProperty("prs").GetArrayLength());
        Directory.Delete(data);
    }

    [Fact]
    public async Task Status_carries_the_crew_and_fresh_queues_a_fresh_start()
    {
        var data = Directory.CreateDirectory(path + ".crew").FullName;
        Directory.CreateDirectory(Path.Combine(data, "live-sessions"));
        Directory.CreateDirectory(Path.Combine(data, "sessions"));
        var me = Environment.ProcessId;
        File.WriteAllText(Path.Combine(data, "live-sessions", $"{me}.json"),
            $$"""{"ts": 5, "answered": "local", "live": {"builder": {"pid": {{me}}, "started": 1790000000, "provider": "claude", "resume": true}, "gone": {"pid": 0} } }""");
        File.WriteAllText(Path.Combine(data, "live-sessions", "0.json"), "{}");
        File.WriteAllText(Path.Combine(data, "sessions", "builder.json"), """{"id": "7f3a91c2e4", "items": 4}""");
        Environment.SetEnvironmentVariable("CLAUDE_PROVIDERS_FILE", Path.Combine(data, "providers.json"));
        File.WriteAllText(Path.Combine(data, "providers.json"), """{"order": ["claude", "local"], "profiles": {"claude": {"model": "claude-sonnet-5"}, "local": {"base_url": "http://localhost:11434", "model": "qwen"}}}""");
        Assert.Contains("\"ok\": true", await board.FreshStart("verifier"));
        var crew = JsonDocument.Parse(await board.Heartbeats(data)).RootElement.GetProperty("crew");
        Assert.Equal(1, crew.GetProperty("live").GetInt32());
        Assert.Equal("Claude subscription, model=claude-sonnet-5 (all ANTHROPIC_* overrides removed)  ·  last run fell back to local", crew.GetProperty("note").GetString());
        Assert.Equal("claude,local", string.Join(",", crew.GetProperty("backends").EnumerateArray().Select(b => b.GetString())));
        var (builder, verifier) = (crew.GetProperty("roles")[0], crew.GetProperty("roles")[1]);
        Assert.Equal("builder|claude|True|7f3a91c2e4|4|False|2026-09-21T14:13:20.0000000+00:00", string.Join("|", builder.GetProperty("name"), builder.GetProperty("provider"),
            builder.GetProperty("resumed"), builder.GetProperty("session_id"), builder.GetProperty("items"), builder.GetProperty("fresh_due"), builder.GetProperty("running_since")));
        Assert.True(verifier.GetProperty("fresh_due").GetBoolean());
        Assert.False(File.Exists(Path.Combine(data, "live-sessions", "0.json"))); // a dead process's file is swept
        Environment.SetEnvironmentVariable("CLAUDE_PROVIDERS_FILE", null);
        Directory.Delete(data, true);
    }

    [Fact]
    public async Task Worker_asks_a_running_crew_to_stop()
    {
        var data = Directory.CreateDirectory(path + ".worker").FullName;
        File.WriteAllText(Path.Combine(data, "worker.state"), $$"""{"pid": {{Environment.ProcessId}}, "item": null}""");
        Assert.Contains("\"stop_requested\": true", await board.ToggleWorker(data, "no-python-here")); // never starts one while it runs
        Assert.True(File.Exists(Path.Combine(data, "worker.stop")));
        Directory.Delete(data, true);
    }

    [Fact]
    public async Task Wake_says_why_it_cannot_and_posts_nothing()
    {
        var data = Directory.CreateDirectory(path + ".wake").FullName;
        async Task<string?> Said(int tid) => JsonDocument.Parse(await board.Wake(tid, data, "no-python-here")).RootElement.GetProperty("said").GetString();
        Assert.Equal("#999 not found", await Said(999));
        Assert.Equal($"you haven't replied on #{thread} yet", await Said((int)thread));
        await board.JohnReplies((int)thread, "dev");
        Assert.Equal("no session on record for builder: it asked before sessions were tracked", await Said((int)thread));
        using (var db = store.Open()) db.RecordSession("builder", "sess-1", data, Environment.ProcessId);
        Assert.Equal("builder's session is still open; it sees your reply on its next board write", await Said((int)thread));
        using (var db = store.Open())
        {
            Assert.Equal("wake|stuck", db.Scalar("SELECT method || '|' || state FROM deliveries"));
            Assert.Equal(2L, db.Scalar("SELECT COUNT(*) FROM messages")); // the question and John's reply: a wake posts nothing
        }
        Directory.Delete(data, true);
    }

    [Fact]
    public async Task A_subscriber_is_pushed_a_write_made_elsewhere()
    {
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
