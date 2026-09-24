using System.Text.Json;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>John's replies must reach the session that asked: injected once, blocking a stop until seen.</summary>
public sealed class HooksTests : IDisposable
{
    readonly string path = Path.Combine(Path.GetTempPath(), $"hooks-{Guid.NewGuid():N}.db");
    readonly BoardStore store;
    readonly Hooks hooks;
    readonly long thread;

    public HooksTests()
    {
        store = new BoardStore(path);
        store.Init();
        hooks = new Hooks(store);
        using var db = store.Open();
        db.RecordSession("builder", "S1", null, 1);
        db.RecordSession("other", "S2", null, 2);
        thread = db.StartThread("question", "Which branch?", "builder", "agent", "main or dev?");
    }

    static JsonElement Input(string sid, bool stopActive = false) =>
        JsonDocument.Parse($$"""{"session_id":"{{sid}}","stop_hook_active":{{(stopActive ? "true" : "false")}}}""").RootElement;

    void JohnReplies(string body) { using var db = store.Open(); db.Reply(thread, "john", "human", body); }

    [Fact]
    public async Task Reply_is_injected_once_into_the_asking_session_only()
    {
        JohnReplies("dev, please");
        Assert.Equal("", await hooks.Run("context", Input("S2")));          // not their thread
        Assert.Contains("dev, please", await hooks.Run("context", Input("S1")));
        Assert.Equal("", await hooks.Run("context", Input("S1")));          // shown once
    }

    [Fact]
    public async Task Stop_is_blocked_until_the_reply_is_seen_even_after_a_prior_block()
    {
        JohnReplies("use dev");
        var stop = JsonDocument.Parse(await hooks.Run("stop", Input("S1", stopActive: true))).RootElement;
        Assert.Equal("block", stop.GetProperty("decision").GetString());
        Assert.Contains("use dev", stop.GetProperty("reason").GetString());
        Assert.Equal("", await hooks.Run("stop", Input("S1", stopActive: true)));
    }

    [Fact]
    public async Task Wait_returns_as_soon_as_John_replies()
    {
        var waiting = hooks.Wait((int)thread, default);
        await Task.Delay(300);
        Assert.False(waiting.IsCompleted);
        JohnReplies("go");
        var text = await waiting.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Contains("go", text);
        Assert.Equal("", await hooks.Run("context", Input("S1")));          // the watcher already delivered it
    }

    public void Dispose()
    {
        Microsoft.Data.Sqlite.SqliteConnection.ClearAllPools();
        foreach (var f in Directory.GetFiles(Path.GetTempPath(), Path.GetFileName(path) + "*")) File.Delete(f);
    }
}
