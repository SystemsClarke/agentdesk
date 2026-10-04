using System.Diagnostics;
using System.Text.Json;
using AgentDesk.Core;
using AgentDesk.Core.Board;
using Xunit;
using Xunit.Abstractions;

namespace AgentDesk.Tests;

/// <summary>Opt-in: AGENTDESK_BENCH_DB names a COPY of a board (never the live one). Prints p50/p95 of the calls every window refresh and every
/// agent tool call make, so a change to the database layer can be shown to help (or not) on John's real data.</summary>
public class DbBenchTests(ITestOutputHelper output)
{
    static (double P50, double P95) Time(int n, Action run)
    {
        run(); // warm
        var t = new List<double>();
        for (var i = 0; i < n; i++) { var sw = Stopwatch.StartNew(); run(); t.Add(sw.Elapsed.TotalMilliseconds); }
        t.Sort();
        return (t[n / 2], t[(int)(n * 0.95)]);
    }

    [Fact]
    public void Bench_on_a_copy_of_a_real_board()
    {
        if (Environment.GetEnvironmentVariable("AGENTDESK_BENCH_DB") is not { Length: > 0 } copy) return;
        var store = new BoardStore(copy);
        if (Environment.GetEnvironmentVariable("AGENTDESK_BENCH_ANCHOR") == "1") { store.GetType().GetMethod("Anchor")?.Invoke(store, null); output.WriteLine("(anchored, as the core runs)"); }
        void Row(string name, (double P50, double P95) r) => output.WriteLine($"{name,-34} p50 {r.P50,7:0.0} ms   p95 {r.P95,7:0.0} ms");
        Row("open+close a connection", Time(200, () => { using var db = store.Open(); }));
        Row("list_threads(all, 50)", Time(60, () => { using var db = store.Open(); db.ListThreads(null, null, 50); }));
        Row("ui:threads (brief, 50, active)", Time(60, () => { using var db = store.Open(); db.ListThreads("discussion", null, 50, includeArchived: false, lastBody: false); }));
        Row("ui:threads (brief, 300, active)", Time(30, () => { using var db = store.Open(); db.ListThreads("discussion", null, 300, includeArchived: false, lastBody: false); }));
        string sid;
        using (var db = store.Open()) sid = db.Rows("SELECT session_id FROM sessions LIMIT 1").FirstOrDefault()?["session_id"]?.ToString() ?? "none";
        var input = JsonSerializer.SerializeToElement(new { session_id = sid });
        var hooks = new Hooks(store);
        Row("hook:context (Replies, per tool)", Time(60, () => hooks.Run("context", input).GetAwaiter().GetResult()));
        var n = 0;
        Row("post_message (one write)", Time(30, () => { using var db = store.Open(); db.StartThread("discussion", "bench " + n++, "bench", "agent", "x"); }));
    }
}
