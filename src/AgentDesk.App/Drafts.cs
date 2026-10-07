using System.IO;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace AgentDesk.App;

/// <summary>Unsent replies and the unsent new post, kept in a file so an update's restart, a crash or a reboot does not eat what John was typing.</summary>
static class DraftStore
{
    /// <summary>The file's text for these drafts; empty drafts leave no entry, and nothing at all gives an empty string.</summary>
    public static string Serialize(IReadOnlyDictionary<int, string> replies, (string Subject, string Body)? compose)
    {
        var kept = replies.Where(r => r.Value.Length > 0).ToList();
        if (kept.Count == 0 && compose is null) return "";
        var o = new JsonObject { ["replies"] = new JsonObject(kept.Select(r => KeyValuePair.Create(r.Key.ToString(), (JsonNode?)r.Value))) };
        if (compose is { } c) o["compose"] = new JsonObject { ["subject"] = c.Subject, ["body"] = c.Body };
        return o.ToJsonString();
    }

    /// <summary>Writes beside the file and moves it in, so a crash mid-write leaves the old drafts rather than half of the new; no drafts removes the file.</summary>
    public static void Save(string path, string text)
    {
        try
        {
            if (text.Length == 0)
            {
                File.Delete(path);
                return;
            }
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            var tmp = path + ".tmp";
            File.WriteAllText(tmp, text);
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
        } // a draft that cannot be saved is no worse than before the file existed
    }

    /// <summary>What <see cref="Serialize"/> wrote; a missing or damaged file is no drafts.</summary>
    public static (Dictionary<int, string> Replies, (string Subject, string Body)? Compose) Load(string path)
    {
        var replies = new Dictionary<int, string>();
        (string, string)? compose = null;
        try
        {
            if (JsonNode.Parse(File.ReadAllText(path)) is JsonObject o)
            {
                foreach (var (k, v) in o["replies"] as JsonObject ?? [])
                    if (int.TryParse(k, out var id) && (string?)v is { Length: > 0 } text) replies[id] = text;
                if (o["compose"] is JsonObject c)
                    compose = ((string?)c["subject"] ?? "", (string?)c["body"] ?? "");
            }
        }
        catch (Exception e) when (e is IOException or JsonException or InvalidCastException or UnauthorizedAccessException)
        {
        }
        return (replies, compose);
    }
}
