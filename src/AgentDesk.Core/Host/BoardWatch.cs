using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core.Host;

/// <summary>
/// Tells subscribed windows the board changed, whoever wrote it: other processes write the database too, so it
/// watches SQLite's PRAGMA data_version on one connection every 250 ms, and only while a subscriber is connected.
/// </summary>
public sealed class BoardWatch(BoardStore store)
{
    const string Changed = """{"event":"board.changed"}""";
    readonly HashSet<Func<string, Task>> subscribers = [];
    readonly Lock gate = new();
    bool watching;

    public string Subscribe(Func<string, Task> push, CancellationToken gone)
    {
        lock (gate)
        {
            subscribers.Add(push);
            if (!watching) { watching = true; _ = Watch(); } // runs to its first await: the baseline predates this reply
        }
        gone.Register(() => { lock (gate) subscribers.Remove(push); });
        return new JsonObject { ["ok"] = true }.ToJsonString(Wire.Indented);
    }

    async Task Watch()
    {
        try
        {
            using var db = store.Open();
            var seen = db.Scalar("PRAGMA data_version");
            while (true)
            {
                await Task.Delay(250);
                Func<string, Task>[] now;
                lock (gate)
                {
                    if (subscribers.Count == 0) { watching = false; return; }
                    now = [.. subscribers];
                }
                var version = db.Scalar("PRAGMA data_version"); // moves on every commit made through another connection
                if (Equals(version, seen)) continue;
                seen = version;
                await Task.WhenAll(now.Select(Push));
            }
        }
        catch (Exception e) { Log.Warn($"board watch stopped: {e}"); lock (gate) watching = false; }
    }

    static async Task Push(Func<string, Task> push)
    {
        try { await push(Changed); }
        catch (Exception e) when (e is IOException or ObjectDisposedException) { } // that window just closed
    }
}
