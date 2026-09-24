using System.Text.Json.Nodes;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>The crew as Options shows it (sessions.status, crew.load_session, providers.describe): the agent sessions running
/// across processes, each crew role's session, and the backend profile from ~/.claude/providers.json.</summary>
public static class Crew
{
    static readonly string[] Roles = ["builder", "verifier", "researcher"];

    static bool Pid(JsonNode? n) => n is JsonValue v && v.TryGetValue(out double pid) && AgentBoard.Alive((int)pid);

    public static JsonObject Status(string data, BoardDb db)
    {
        var live = new JsonObject();
        var (answered, newest) = ("", 0.0);
        foreach (var f in Directory.Exists(Path.Combine(data, "live-sessions")) ? Directory.GetFiles(Path.Combine(data, "live-sessions"), "*.json") : [])
        {
            if (!int.TryParse(Path.GetFileNameWithoutExtension(f), out var pid) || !AgentBoard.Alive(pid)) { try { File.Delete(f); } catch (IOException) { } continue; }
            if (AgentBoard.Load(f) is not { } s) continue;
            foreach (var (who, run) in (s["live"] as JsonObject ?? []).Where(r => Pid(r.Value?["pid"])).ToList())
                live[who] = run!.DeepClone();
            if (s["answered"]?.ToString() is { Length: > 0 } a && s["ts"] is JsonValue t && t.TryGetValue(out double ts) && ts > newest) (answered, newest) = (a, ts);
        }
        var settings = AgentBoard.Load(Path.Combine(data, "settings.json"));
        var provider = settings?["provider"]?.ToString() ?? "claude";
        var max = int.TryParse(Environment.GetEnvironmentVariable("AGENTDESK_MAX_SESSIONS") ?? settings?["max_sessions"]?.ToString() ?? "3", out var m) ? Math.Max(1, m) : 3;
        var file = Environment.GetEnvironmentVariable("CLAUDE_PROVIDERS_FILE")
            ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".claude", "providers.json");
        var cfg = AgentBoard.Load(file);
        var profiles = cfg?["profiles"] as JsonObject ?? [];
        var name = profiles.ContainsKey(provider) ? provider : "claude";
        var note = profiles[name] is not JsonObject spec ? $"{name} (no {Path.GetFileName(file)} entry for it -- running on the Claude subscription)"
            : spec["base_url"]?.ToString() is { Length: > 0 } url ? $"{url} model={spec["model"]?.ToString() ?? "None"}"
            : $"Claude subscription, model={spec["model"]?.ToString() ?? "None"} (all ANTHROPIC_* overrides removed)";
        var chain = (cfg?["order"] as JsonArray ?? []).Select(n => n?.ToString() ?? "").Concat(profiles.Select(p => p.Key)).Where(n => n.Length > 0).Distinct().ToList();
        return new()
        {
            ["live"] = live.Count,
            ["max"] = max,
            ["note"] = note + (answered.Length > 0 && answered != provider ? $"  ·  last run fell back to {answered}" : ""),
            ["backends"] = new JsonArray([.. (chain.Count > 0 ? chain : ["claude"]).Select(n => (JsonNode?)n)]),
            ["roles"] = new JsonArray([.. Roles.Select(r =>
            {
                var run = live[r];
                var session = AgentBoard.Load(Path.Combine(data, "sessions", $"{r}.json"));
                return (JsonNode)new JsonObject
                {
                    ["name"] = r,
                    ["running_since"] = run?["started"] is JsonValue st ? DateTimeOffset.FromUnixTimeMilliseconds((long)(st.GetValue<double>() * 1000)).ToString("O") : null,
                    ["provider"] = run?["provider"]?.ToString(), ["resumed"] = run?["resume"]?.GetValue<bool>() == true,
                    ["session_id"] = session?["id"]?.ToString(), ["items"] = (int)(session?["items"]?.GetValue<double>() ?? 0), ["fresh_due"] = db.TorchDue(r),
                };
            })]),
        };
    }
}
