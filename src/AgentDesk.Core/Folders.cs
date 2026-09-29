using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>ui:folders: the folders John works in, most likely first, for the New agent and New goal dialogs. Gathered from
/// what is already running (identities, goals, agent sessions) and from every Claude Code conversation of the last 90 days,
/// then ranked by use that decays with age, so what he opened yesterday beats what he opened once in the spring.</summary>
public static class Folders
{
    /// <summary>One time something worked in <paramref name="Path"/>. <paramref name="Weight"/> says how much it counts;
    /// <paramref name="Who"/> names the agent or goal when there is one.</summary>
    public readonly record struct Use(string Path, DateTimeOffset When, double Weight, string? Who = null);

    /// <summary>A suggestion: who works there (up to 3 names), how many uses in all, and the last one.</summary>
    public sealed record Pick(string Path, IReadOnlyList<string> Who, int Uses, DateTimeOffset Last);

    const double HalfLifeDays = 21;
    const int History = 300;
    static readonly TimeSpan Window = TimeSpan.FromDays(90);

    /// <summary>The same folder however it is spelled (case, trailing separator, slash direction) is one folder. Ranked by
    /// decayed weight, then recency; folders that are gone are dropped.</summary>
    public static List<Pick> Rank(IEnumerable<Use> uses, DateTimeOffset now, Func<string, bool> exists, int take = 30) =>
        [.. uses.Where(u => !string.IsNullOrWhiteSpace(u.Path))
            .GroupBy(u => u.Path.Trim().Replace('/', '\\').TrimEnd('\\'), StringComparer.OrdinalIgnoreCase)
            .Where(g => g.Key.Length > 0 && exists(g.Key))
            .Select(g => (Path: g.Key, Uses: g.ToList(), Score: g.Sum(u => u.Weight * Math.Pow(0.5, Math.Max(0, (now - u.When).TotalDays) / HalfLifeDays))))
            .OrderByDescending(f => f.Score).ThenByDescending(f => f.Uses.Max(u => u.When))
            .Take(take)
            .Select(f => new Pick(f.Path,
                [.. f.Uses.Where(u => u.Who is not null).OrderByDescending(u => u.When).Select(u => u.Who!).Distinct(StringComparer.OrdinalIgnoreCase).Take(3)],
                f.Uses.Count, f.Uses.Max(u => u.When)))];

    /// <summary>Every use the machine knows of: identities and goals count for more than a session, since an agent lives there.</summary>
    public static List<Use> Gather(BoardStore store, string projects, DateTimeOffset now)
    {
        List<Use> uses = [];
        using (var db = store.Open())
        {
            foreach (var r in db.Rows("SELECT name, folder, updated_ts FROM identities WHERE host='windows'"))
                uses.Add(new(r["folder"]!.ToString()!, Time(r["updated_ts"], now), 3, r["name"]!.ToString()));
            foreach (var r in db.Rows("SELECT name, measure_folder, updated_ts FROM goals"))
                uses.Add(new(r["measure_folder"]!.ToString()!, Time(r["updated_ts"], now), 3, r["name"]!.ToString()));
            foreach (var r in db.Rows("SELECT cwd, seen_ts FROM sessions WHERE cwd IS NOT NULL"))
                uses.Add(new(r["cwd"]!.ToString()!, Time(r["seen_ts"], now), 1));
        }
        if (Directory.Exists(projects))
            foreach (var f in Directory.EnumerateDirectories(projects).SelectMany(d => Directory.EnumerateFiles(d, "*.jsonl")).Select(f => new FileInfo(f))
                         .Where(f => now - f.LastWriteTimeUtc < Window).OrderByDescending(f => f.LastWriteTimeUtc).Take(History))
                if (FolderOf(f.FullName) is { } folder)
                    uses.Add(new(folder, f.LastWriteTimeUtc, 1));
        return uses;
    }

    /// <summary>What the dialog offers: <see cref="Gather"/> and <see cref="Rank"/> over what exists, without the folders nobody
    /// chooses to work in (scratch runs of claude -p in the temp folder, the Claude app's scratch workspaces under AppData).</summary>
    public static List<Pick> Suggest(BoardStore store, string projects, DateTimeOffset now)
    {
        var (profile, temp) = (Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), Path.GetTempPath());
        return Rank(Gather(store, projects, now).Where(u => !Noise(u.Path, profile, temp)), now, Directory.Exists);
    }

    public static bool Noise(string path, string profile, string temp) => Under(path, Path.Combine(profile, "AppData")) || Under(path, temp);

    static bool Under(string path, string root)
    {
        (path, root) = (path.Trim().Replace('/', '\\').TrimEnd('\\'), root.Replace('/', '\\').TrimEnd('\\'));
        return path.Equals(root, StringComparison.OrdinalIgnoreCase) || path.StartsWith(root + "\\", StringComparison.OrdinalIgnoreCase);
    }

    public static string Json(IEnumerable<Pick> picks) => new JsonObject
    {
        ["folders"] = new JsonArray([.. picks.Select(p => (JsonNode)new JsonObject
        {
            ["path"] = p.Path, ["who"] = new JsonArray([.. p.Who.Select(w => (JsonNode)w)]), ["uses"] = p.Uses, ["last"] = p.Last.ToString("yyyy-MM-ddTHH:mm:ssZ"),
        })]),
    }.ToJsonString(Wire.Indented);

    static DateTimeOffset Time(JsonNode? ts, DateTimeOffset fallback) =>
        DateTimeOffset.TryParse(ts?.ToString(), System.Globalization.CultureInfo.InvariantCulture, System.Globalization.DateTimeStyles.AssumeUniversal, out var t) ? t : fallback;

    /// <summary>A transcript's folder: the first "cwd" in its first few lines. Claude may be writing the file, so share it.</summary>
    static string? FolderOf(string file)
    {
        try
        {
            using var reader = new StreamReader(new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete));
            for (var i = 0; i < 20 && reader.ReadLine() is { } line; i++)
                try
                {
                    if (JsonDocument.Parse(line).RootElement.TryGetProperty("cwd", out var cwd) && cwd.GetString() is { Length: > 0 } folder) return folder;
                }
                catch (JsonException) { } // a line cut off mid-write
        }
        catch (IOException) { }
        return null;
    }
}
