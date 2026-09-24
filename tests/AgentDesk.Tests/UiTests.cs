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
