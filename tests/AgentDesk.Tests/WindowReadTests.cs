using System.Text.Json.Nodes;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>The window's cheaper reads: ui:threads leaves out each thread's last body, and ui:status asks Heartbeats for no governor
/// (it computes its own, with enforcement merged in).</summary>
public sealed class WindowReadTests : IDisposable
{
    readonly string data = Directory.CreateTempSubdirectory("window-read-").FullName;
    readonly BoardStore store;

    public WindowReadTests()
    {
        store = new BoardStore(Path.Combine(data, "agentdesk.db"));
        store.Init();
        using var db = store.Open();
        db.StartThread("discussion", "a subject", "builder", "agent", "the first and last body");
    }

    public void Dispose() { try { Directory.Delete(data, true); } catch (IOException) { } }

    [Fact]
    public void Brief_rows_drop_last_body_and_keep_the_rest()
    {
        using var db = store.Open();
        var full = db.ListThreads("discussion", null, 10).Single();
        var brief = db.ListThreads("discussion", null, 10, lastBody: false).Single();
        Assert.Equal("the first and last body", (string?)full["last_body"]);
        Assert.False(brief.ContainsKey("last_body"));
        Assert.Equal(full.Where(p => p.Key != "last_body").Select(p => p.Key + "=" + p.Value), brief.Select(p => p.Key + "=" + p.Value));
    }

    [Fact]
    public async Task Heartbeats_skip_the_governor_when_asked()
    {
        var board = new AgentBoard(store, null!, "wait {0}");
        Assert.NotNull(JsonNode.Parse(await board.Heartbeats(data))!["governor"]);
        Assert.Null(JsonNode.Parse(await board.Heartbeats(data, governor: false))!["governor"]);
    }
}
