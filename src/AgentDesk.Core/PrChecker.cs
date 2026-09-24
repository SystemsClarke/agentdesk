using System.ComponentModel;
using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>A check that could not be completed. Never means, and never implies, merged.</summary>
public sealed class GhError(string message) : Exception(message);

/// <summary>The merge list's watcher (agentdesk/prs.py): asks GitHub, through `gh`, whether each open PR is settled. It only
/// ever takes a row off the list because GitHub said so; a failed check leaves the row open and records why.</summary>
public sealed class PrChecker(BoardStore store, Func<string, JsonObject>? gh = null)
{
    readonly SemaphoreSlim poke = new(0, 1);
    readonly Func<string, JsonObject> gh = gh ?? Gh;

    /// <summary>ui:check_prs: run a pass now rather than at the next minute.</summary>
    public string Poke()
    {
        try { poke.Release(); } catch (SemaphoreFullException) { } // already asked
        return new JsonObject { ["ok"] = true }.ToJsonString(Wire.Indented);
    }

    /// <summary>Checks the open PRs, the first time after <paramref name="first"/>, then every minute or when poked. Never ends.</summary>
    public async Task Run(TimeSpan first)
    {
        for (var wait = first; ; wait = TimeSpan.FromMinutes(1))
        {
            await poke.WaitAsync(wait);
            try { await Task.Run(CheckDue); }
            catch (Exception e) { Log.Warn($"pr check pass failed: {e}"); } // a locked board or a misbehaving gh costs one pass, not the feature
        }
    }

    void CheckDue()
    {
        using var db = store.Open();
        var counted = new SortedDictionary<string, int>(StringComparer.Ordinal);
        foreach (var status in db.Rows("SELECT * FROM pull_requests WHERE state='open' ORDER BY (checked_ts IS NOT NULL), checked_ts, id").Select(r => CheckOne(db, r)))
            counted[status] = counted.GetValueOrDefault(status) + 1;
        if (counted.Keys.Any(k => k != "still-open"))
            Log.Info($"pr check: {counted.Values.Sum()} checked, " + string.Join(", ", counted.Select(c => $"{c.Value} {c.Key}")));
    }

    string CheckOne(BoardDb db, JsonObject row)
    {
        var id = (long)row["id"]!;
        void Checked(string? error) => db.Exec("UPDATE pull_requests SET checked_ts=$ts, last_error=$e WHERE id=$id", ("ts", db.NowIso()), ("e", error), ("id", id));
        JsonObject data;
        try { data = gh((string)row["url"]!); }
        catch (GhError e) { Checked(e.Message); return "error"; }
        var state = (data["state"]?.ToString() ?? "").ToUpperInvariant();
        if (state == "OPEN") { Checked(null); return "still-open"; } // clears a stale error: the row is known-good now
        if (state is not ("MERGED" or "CLOSED")) { Checked($"gh reported a state this does not understand: {Py.Repr(state)}"); return "error"; }
        var settled = state.ToLowerInvariant();
        db.Exec("UPDATE pull_requests SET state=$s, settled_ts=$ts, checked_ts=$ts, last_error=NULL WHERE id=$id", ("s", settled), ("ts", db.NowIso()), ("id", id));
        var title = (data["title"]?.ToString() is { Length: > 0 } t ? t : row["title"]?.ToString() ?? "").Trim();
        if (row["thread_id"] is not JsonValue thread)
            Log.Info($"pr {row["url"]} settled {settled} with no thread to notify");
        else if (db.Exec("UPDATE pull_requests SET notified_ts=$ts WHERE id=$id AND notified_ts IS NULL", ("ts", db.NowIso()), ("id", id)) == 1)
        {
            var (where, who) = ($"{row["repo"]}#{row["number"]} - {row["url"]}", Identity.Describe(row["requested_by"]?.ToString()));
            try
            {
                db.Reply((long)thread, "agentdesk", BoardDb.Agent, settled == "merged"
                    ? $"Merged: {title}\n\n{where}\n\n{who}: your pull request is merged, and off John's merge list. "
                        + "Pull the base branch before building on it, and post on this thread if there is follow-up."
                    : $"Closed without merging: {title}\n\n{where}\n\n{who}: your pull request was closed rather than merged, so it is off "
                        + "John's merge list. Don't reopen it on your own; ask him on this thread if the change is still wanted.",
                    meta: new JsonObject { ["kind"] = $"pr-{settled}" });
            }
            catch { db.Exec("UPDATE pull_requests SET notified_ts=NULL WHERE id=$id", ("id", id)); throw; } // claimed and never sent is worse than twice
        }
        return settled;
    }

    static JsonObject Gh(string url)
    {
        var psi = new ProcessStartInfo("gh") { UseShellExecute = false, CreateNoWindow = true, RedirectStandardOutput = true,
            RedirectStandardError = true, StandardOutputEncoding = Encoding.UTF8, StandardErrorEncoding = Encoding.UTF8 };
        foreach (var a in new[] { "pr", "view", url, "--json", "state,title,mergedAt" }) psi.ArgumentList.Add(a);
        Process p;
        try { p = Process.Start(psi)!; }
        catch (Win32Exception e) { throw new GhError(e.NativeErrorCode == 2 ? "the GitHub CLI (gh) is not on PATH" : $"gh could not be run: {e.Message}"); }
        using (p)
        {
            var (stdout, stderr) = (p.StandardOutput.ReadToEndAsync(), p.StandardError.ReadToEndAsync());
            if (!p.WaitForExit(30_000)) { p.Kill(true); throw new GhError("gh did not answer within 30s"); }
            if (p.ExitCode != 0) // gh's first line is the useful one; the rest is advice
                throw new GhError((stderr.Result.Length > 0 ? stderr.Result : stdout.Result).Split('\n').Select(l => l.TrimEnd('\r'))
                    .FirstOrDefault(l => l.Trim().Length > 0) ?? $"gh exited {p.ExitCode}");
            try { return JsonNode.Parse(stdout.Result) as JsonObject ?? throw new JsonException(); }
            catch (JsonException) { throw new GhError("gh returned something that is not JSON"); }
        }
    }
}
