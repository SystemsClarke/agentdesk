using System.Globalization;
using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// Permission rules for Claude Code, proposed by agents and approved by John. When Claude Code's auto-mode classifier refuses something John
/// asked for, the agent calls propose_permission_rule; that records a pending proposal and rings John with a question thread. Nothing is written
/// to settings.json until John approves in the window (Permission rules, X on the main menu): an agent can propose, never approve. An approval
/// backs the file up, adds the rule to <c>permissions.allow</c> (once), leaves every other key as it was, and says so on the thread; a revert
/// takes it out again. Every proposal stays in <c>permission_proposals</c> with who asked, why, and when it was decided, as the audit log.
/// </summary>
public sealed class Permissions(BoardStore store, string data, string userSettings)
{
    static readonly Regex RuleSyntax = new(@"^[A-Za-z][A-Za-z0-9_\-:.*]*(\([^\r\n]+\))?$", RegexOptions.Compiled);
    static readonly Regex Broad = new(@"^[A-Za-z][A-Za-z0-9_\-:.*]*(\(\s*:?\*?\s*\))?$", RegexOptions.Compiled); // "Bash", "Bash(*)", "Bash(:*)": everything
    static readonly JsonSerializerOptions Pretty = new() { WriteIndented = true, Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping };

    readonly Lock gate = new();

    /// <summary>Where a scope's rules live: the user's settings.json, or the project's own under .claude.</summary>
    string SettingsFor(string scope, string? projectDir) =>
        scope == "user" ? userSettings : Path.Combine(projectDir ?? throw new ArgumentException("a project rule needs a project_dir"), ".claude", "settings.json");

    public static bool IsBroad(string rule) => Broad.IsMatch(rule);

    /// <summary>An agent's proposal. Idempotent: the same pending rule is not proposed twice, and a rule already allowed says so.</summary>
    public Task<string> Propose(Caller c, string rule, string? scope, string reason, string? blocked, string? requestedBy, string? projectDir)
    {
        rule = (rule ?? "").Trim();
        if (rule.Length is 0 or > 300 || !RuleSyntax.IsMatch(rule))
            throw new ArgumentException("rule must be one Claude Code permission rule such as Bash(python tools/run.py:*) or mcp__server__tool, on one line");
        scope = string.IsNullOrWhiteSpace(scope) ? "user" : scope.Trim().ToLowerInvariant();
        if (scope is not ("user" or "project")) throw new ArgumentException("scope is user or project");
        if (string.IsNullOrWhiteSpace(reason)) throw new ArgumentException("reason is required: what John asked for that the classifier blocked");
        var dir = scope == "project" ? projectDir ?? c.Cwd : null;
        if (scope == "project" && (dir is null || !Directory.Exists(dir))) throw new ArgumentException($"no such project_dir: {dir}");
        var who = Identity.Resolve(requestedBy, c);
        lock (gate)
        {
            using var db = store.Open();
            var path = SettingsFor(scope, dir);
            if (Allowed(path).Contains(rule, StringComparer.Ordinal))
                return Task.FromResult(new JsonObject { ["already_allowed"] = true, ["rule"] = rule, ["note"] = $"{path} already allows it: retry the action." }.ToJsonString(Wire.Indented));
            if (db.Rows("SELECT id, thread_id FROM permission_proposals WHERE rule=$r AND scope=$s AND status='pending'", ("r", rule), ("s", scope)) is [var pending])
                return Task.FromResult(new JsonObject { ["id"] = pending["id"]!.DeepClone(), ["thread_id"] = pending["thread_id"]?.DeepClone(), ["status"] = "pending",
                    ["note"] = "already proposed and waiting for John" }.ToJsonString(Wire.Indented));
            var body = $"**Rule:** `{rule}` ({scope})\n**Asked by:** {who}\n**Why:** {Cut(reason, 500)}"
                + (string.IsNullOrWhiteSpace(blocked) ? "" : $"\n**Blocked action:** `{Cut(blocked, 300)}`")
                + (IsBroad(rule) ? "\n**Careful: this rule is broad, it allows every use of the tool.**" : "")
                + "\n\nApprove it? Permission rules (X on the AgentDesk main menu) has Approve and Reject; nothing is written until you do.";
            var tid = db.StartThread("question", $"permission rule: {rule}", who, BoardDb.Agent, body);
            db.Exec("INSERT INTO permission_proposals (rule, scope, project_dir, reason, blocked, requested_by, thread_id, status, created_ts) VALUES ($r,$s,$d,$why,$b,$w,$t,'pending',$ts)",
                ("r", rule), ("s", scope), ("d", dir), ("why", reason.Trim()), ("b", blocked), ("w", who), ("t", tid), ("ts", db.NowIso()));
            var id = db.Scalar("SELECT MAX(id) FROM permission_proposals");
            return Task.FromResult(new JsonObject { ["id"] = Convert.ToInt64(id, CultureInfo.InvariantCulture), ["thread_id"] = tid, ["status"] = "pending",
                ["note"] = "John has been asked. Nothing is written to settings.json until he approves in AgentDesk; carry on with other work and retry the action once his answer is on the thread." }.ToJsonString(Wire.Indented));
        }
    }

    /// <summary>ui:permission_list: the proposals, newest first, and what the user's settings.json allows now.</summary>
    public Task<string> List()
    {
        using var db = store.Open();
        var rows = db.Rows("SELECT * FROM permission_proposals ORDER BY id DESC LIMIT 200");
        foreach (var r in rows) r["broad"] = IsBroad(r["rule"]!.ToString()!);
        return Task.FromResult(new JsonObject { ["proposals"] = new JsonArray([.. rows]), ["allow"] = new JsonArray([.. Allowed(userSettings).Select(a => (JsonNode)a)]),
            ["settings"] = userSettings }.ToJsonString(Wire.Indented));
    }

    /// <summary>ui:permission_decide: John approves (writes the rule) or rejects. An agent's session is refused.</summary>
    public Task<string> Decide(Caller c, long id, bool approve)
    {
        John(c);
        lock (gate)
        {
            using var db = store.Open();
            var p = Need(db, id);
            if (p["status"]!.ToString() != "pending") throw new ArgumentException($"proposal {id} is already {p["status"]}");
            var (rule, scope, path) = (p["rule"]!.ToString()!, p["scope"]!.ToString()!, SettingsFor(p["scope"]!.ToString()!, p["project_dir"]?.ToString()));
            if (approve)
            {
                var backup = Edit(path, rule, add: true);
                db.Exec("UPDATE permission_proposals SET status='approved', decided_ts=$ts, settings_path=$p, backup=$b WHERE id=$id", ("ts", db.NowIso()), ("p", path), ("b", backup), ("id", id));
                Tell(db, p, $"Approved in AgentDesk: `{rule}` is now in `permissions.allow` of {path}. Retry the action.", "Approved in the Permission rules screen.");
                Log.Info($"permission rule approved: {rule} ({scope}) -> {path}");
            }
            else
            {
                db.Exec("UPDATE permission_proposals SET status='rejected', decided_ts=$ts WHERE id=$id", ("ts", db.NowIso()), ("id", id));
                Tell(db, p, $"Rejected in AgentDesk: `{rule}` was not added. Do not retry the action; ask John another way.", "Rejected in the Permission rules screen.");
            }
            return Task.FromResult(Need(db, id).ToJsonString(Wire.Indented));
        }
    }

    /// <summary>ui:permission_revert: takes an approved rule back out of settings.json.</summary>
    public Task<string> Revert(Caller c, long id)
    {
        John(c);
        lock (gate)
        {
            using var db = store.Open();
            var p = Need(db, id);
            if (p["status"]!.ToString() != "approved") throw new ArgumentException($"proposal {id} is {p["status"]}, not approved: nothing to revert");
            var (rule, path) = (p["rule"]!.ToString()!, p["settings_path"]!.ToString()!);
            var backup = Edit(path, rule, add: false);
            db.Exec("UPDATE permission_proposals SET status='reverted', reverted_ts=$ts, backup=COALESCE($b, backup) WHERE id=$id", ("ts", db.NowIso()), ("b", backup), ("id", id));
            Tell(db, p, $"Reverted in AgentDesk: `{rule}` was taken back out of {path}.", null);
            Log.Info($"permission rule reverted: {rule} <- {path}");
            return Task.FromResult(Need(db, id).ToJsonString(Wire.Indented));
        }
    }

    static void John(Caller c)
    {
        if (!Goals.John(c)) throw new ArgumentException("only John approves, rejects or reverts permission rules");
    }

    static JsonObject Need(BoardDb db, long id) =>
        db.Rows("SELECT * FROM permission_proposals WHERE id=$i", ("i", id)) is [var row] ? row : throw new ArgumentException($"no such proposal: {id}");

    /// <summary>The result on the proposal's thread, and John's own line so the question stops ringing (only a human reply settles one).</summary>
    static void Tell(BoardDb db, JsonObject p, string result, string? johnLine)
    {
        if (p["thread_id"] is not { } t) return;
        var tid = Convert.ToInt64(t.GetValue<object>(), CultureInfo.InvariantCulture);
        db.Reply(tid, "AgentDesk", BoardDb.Agent, result);
        if (johnLine is not null) db.JohnReplies(tid, johnLine);
    }

    static string Cut(string s, int n) => s.Length > n ? s[..n] + "..." : s;

    /// <summary>permissions.allow of a settings file; empty when the file or the key is not there.</summary>
    static List<string> Allowed(string path)
    {
        try
        {
            if (!File.Exists(path)) return [];
            return JsonNode.Parse(File.ReadAllText(path)) is JsonObject o && o["permissions"] is JsonObject perms && perms["allow"] is JsonArray allow
                ? [.. allow.Select(a => (string?)a).OfType<string>()] : [];
        }
        catch (Exception e) when (e is IOException or JsonException) { return []; }
    }

    /// <summary>Adds or removes one rule in permissions.allow, after a backup copy of the file, keeping every other key. The file is written
    /// whole and atomically; one that is not valid JSON is refused untouched. Returns the backup's path (null when there was no file).</summary>
    string? Edit(string path, string rule, bool add)
    {
        JsonObject root;
        string? backup = null;
        if (File.Exists(path))
        {
            var text = File.ReadAllText(path);
            try { root = JsonNode.Parse(text) as JsonObject ?? throw new JsonException("not an object"); }
            catch (JsonException e) { throw new ArgumentException($"{path} is not valid JSON ({e.Message}): not touched. Fix it by hand, then approve again."); }
            Directory.CreateDirectory(Path.Combine(data, "permission_backups"));
            var origin = Path.GetFileName(Path.GetDirectoryName(path));
            backup = Path.Combine(data, "permission_backups", $"{DateTime.UtcNow:yyyyMMdd-HHmmss-fff}-{(string.IsNullOrEmpty(origin) ? "settings" : origin)}.json");
            File.WriteAllText(backup, text);
        }
        else if (!add) return null;
        else root = [];
        if (root["permissions"] is not JsonObject perms) root["permissions"] = perms = [];
        if (perms["allow"] is not JsonArray allow) perms["allow"] = allow = [];
        var have = allow.Select(a => (string?)a).ToList();
        if (add && !have.Contains(rule)) allow.Add((JsonNode?)JsonValue.Create(rule));
        else if (!add)
            for (var i = allow.Count - 1; i >= 0; i--)
                if ((string?)allow[i] == rule) allow.RemoveAt(i);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        Atomic.Write(path, root.ToJsonString(Pretty) + "\n");
        return backup;
    }
}
