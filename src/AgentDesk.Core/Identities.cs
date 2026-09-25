using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// Agents that outlive the core: an identity is a name, a folder, a charter and one Claude Code conversation, kept in the
/// board's identities table. Running, it is the headless session of the same name (agentdesk attach &lt;name&gt;). At most
/// max_sessions run at once and the rest queue; when the core starts it resumes the ones that were running. When its session
/// hands off (pass_the_torch), the end of that turn restarts it: a fresh conversation, the next generation, started from the handoff.
/// </summary>
public sealed partial class Identities
{
    /// <summary>Prepended to every identity's system prompt; {0} is its name.</summary>
    const string Chain = """
        You are one generation of {0}, a long-lived AgentDesk agent. The identity {0} continues past this session.
        Your successor starts automatically from your handoff, and nothing you did not write down reaches it.
        When you get the 60% context warning (PHOENIX), finish the step you are on and call pass_the_torch
        with a standalone handoff: what you own, what is mid-flight, and what is next.
        Then stop. The system ends this session and starts your successor from that handoff.
        """;
    static readonly TimeSpan Cooldown = TimeSpan.FromMinutes(2), Settle = TimeSpan.FromSeconds(1);
    readonly BoardStore store;
    readonly Sessions sessions;
    readonly string data, claude;
    readonly Lock gate = new();

    /// <summary><paramref name="claude"/> is the command line that stands for claude (tests pass a harmless one).</summary>
    public Identities(BoardStore store, Sessions sessions, string data, string claude = "claude")
    {
        (this.store, this.sessions, this.data, this.claude) = (store, sessions, data, claude);
        store.Init();
        sessions.Ended += (name, pid) =>
        {
            lock (gate)
            {
                using var db = store.Open();
                db.Exec("UPDATE identities SET state='stopped', pid=NULL, updated_ts=$ts WHERE name=$n AND pid=$pid AND state='running'",
                    ("ts", db.NowIso()), ("n", name), ("pid", pid));
                Drain(db);
            }
        };
    }

    /// <summary>Where Claude Code keeps transcripts: ~/.claude/projects/&lt;encoded folder&gt;/&lt;session id&gt;.jsonl.</summary>
    static string Projects => Environment.GetEnvironmentVariable("AGENTDESK_CLAUDE_PROJECTS")
        ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".claude", "projects");

    public Task<string> Create(string name, string folder, string? charter, string? host, bool autostart = false, string? sessionId = null, string? model = null)
    {
        if (string.IsNullOrWhiteSpace(name)) throw new ArgumentException("name is required");
        model = string.IsNullOrWhiteSpace(model) ? "sonnet" : model.Trim().ToLowerInvariant();
        if (!Governor.Models.Contains(model)) throw new ArgumentException($"model must be haiku, sonnet or opus, got {model}");
        host = string.IsNullOrWhiteSpace(host) ? "windows" : host;
        if (host != "windows" && !(host.StartsWith("wsl:") && host.Length > 4)) throw new ArgumentException($"host must be windows or wsl:<distro>, got {host}");
        if (host == "windows" && !Directory.Exists(folder)) throw new ArgumentException($"no such folder: {folder}");
        lock (gate)
        {
            using var db = store.Open();
            if (Get(db, name) is not null) throw new ArgumentException($"identity already exists: {name}");
            db.Exec("INSERT INTO identities (name, folder, charter, host, claude_session_id, autostart, model, created_ts, updated_ts) VALUES ($n,$f,$c,$h,$s,$a,$m,$ts,$ts)",
                ("n", name), ("f", folder), ("c", charter), ("h", host), ("s", sessionId), ("a", autostart ? 1 : 0), ("m", model), ("ts", db.NowIso()));
            return Ok(Get(db, name)!);
        }
    }

    public Task<string> List()
    {
        using var db = store.Open();
        return Ok(new JsonObject { ["identities"] = new JsonArray([.. db.Rows("SELECT * FROM identities ORDER BY name")]) });
    }

    /// <summary>Launches it now if a slot is free, else queues it.</summary>
    public Task<string> Start(string name)
    {
        lock (gate)
        {
            using var db = store.Open();
            if (Need(db, name)["state"]?.ToString() is "stopped") Mark(db, name, "queued");
            if (Drain(db).TryGetValue(name, out var why)) throw new ArgumentException(why);
            return Ok(Get(db, name)!);
        }
    }

    /// <summary>Stops an identity, or a plain session of that name (agentdesk stop, step 1).</summary>
    public async Task<string> Stop(string name)
    {
        string? was;
        lock (gate)
        {
            using var db = store.Open();
            if (Get(db, name) is not { } row) was = null;
            else { was = row["state"]?.ToString(); Mark(db, name, "stopped"); }
        }
        if (was is null) return await sessions.Stop(name);
        if (was == "running")
            try { await sessions.Stop(name); }
            catch (ArgumentException) { } // it had just ended
        lock (gate)
        {
            using var db = store.Open();
            Drain(db);
            return Get(db, name)!.ToJsonString(Wire.Indented);
        }
    }

    public async Task<string> Forget(string name)
    {
        using (var db = store.Open()) Need(db, name);
        await Stop(name);
        lock (gate)
        {
            using var db = store.Open();
            db.Exec("DELETE FROM identities WHERE name=$n", ("n", name));
        }
        return await Ok(new JsonObject { ["forgotten"] = name });
    }

    /// <summary>At core start: everything that was running or queued, and every autostart identity, is queued again and
    /// launched as slots allow, resuming its conversation.</summary>
    public void Resume()
    {
        lock (gate)
        {
            using var db = store.Open();
            db.Exec("UPDATE identities SET state='queued', pid=NULL WHERE state IN ('running', 'queued') OR autostart=1");
            foreach (var (name, why) in Drain(db)) Log.Warn($"identity {name} did not start: {why}");
        }
    }

    /// <summary>Claude Code sessions touched in the last 24 hours that someone typed in, newest first: candidates for <see cref="Adopt"/>.</summary>
    public static Task<string> Adoptable()
    {
        var since = DateTime.UtcNow.AddHours(-24);
        var files = Directory.Exists(Projects) ? Directory.EnumerateDirectories(Projects).SelectMany(d => Directory.EnumerateFiles(d, "*.jsonl")) : [];
        return Ok(new JsonObject
        {
            ["sessions"] = new JsonArray([.. files.Select(f => new FileInfo(f)).Where(f => f.LastWriteTimeUtc > since).OrderByDescending(f => f.LastWriteTimeUtc)
                .Select(f => (f, Peek(f.FullName))).Where(p => p.Item2.First is not null) // nobody typed in it: claude -p /usage, scheduled tasks
                .Select(p => (JsonNode)new JsonObject
                {
                    ["session_id"] = Path.GetFileNameWithoutExtension(p.f.Name), ["folder"] = p.Item2.Folder,
                    ["last_activity"] = p.f.LastWriteTimeUtc.ToString("yyyy-MM-ddTHH:mm:ssZ"), ["first_message"] = p.Item2.First,
                })]),
        });
    }

    /// <summary>Makes an existing Claude Code conversation an identity, in its own folder, and resumes it.</summary>
    public async Task<string> Adopt(string sessionId, string name)
    {
        if (!Guid.TryParse(sessionId, out _)) throw new ArgumentException($"not a session id: {sessionId}");
        var file = Transcript(sessionId) ?? throw new ArgumentException($"no transcript for {sessionId} under {Projects}");
        var folder = Peek(file).Folder ?? throw new ArgumentException($"{file} does not say which folder it ran in");
        await Create(name, folder, null, null, sessionId: sessionId);
        var row = JsonNode.Parse(await Start(name))!.AsObject();
        row["note"] = "If this conversation is still open in the Claude desktop app, two processes are now writing to one conversation: close it there. (Next time, close it in the app before adopting.)";
        return row.ToJsonString(Wire.Indented);
    }

    /// <summary>pass_the_torch from an identity's own session (its AGENTDESK_IDENTITY and claude session id): marks it
    /// phoenix-pending with the handoff message, so the end of this turn restarts it (<see cref="AfterTurn"/>).</summary>
    public async Task<string> Torch(Caller caller, Task<string> call)
    {
        var result = await call;
        if (caller is not { Identity: { } name, SessionId: { } sid } || JsonNode.Parse(result) is not { } r || r["bio_thread_id"] is null) return result;
        lock (gate)
        {
            using var db = store.Open();
            db.Exec("UPDATE identities SET phoenix_msg=(SELECT MAX(id) FROM messages WHERE thread_id=$t AND author=$a) WHERE name=$n AND claude_session_id=$s AND state='running'",
                ("t", (long)r["bio_thread_id"]!), ("a", r["name"]!.ToString()), ("n", name), ("s", sid));
        }
        return result;
    }

    /// <summary>The Stop hook. Once the turn really ends (the hook did not block it), a session that handed off is restarted,
    /// after the hook has answered.</summary>
    public async Task<string> AfterTurn(JsonElement input, Task<string> hook)
    {
        var result = await hook;
        if (!result.Contains("\"block\"") && input.TryGetProperty("session_id", out var s) && s.GetString() is { Length: > 0 } sid)
            _ = Task.Run(() => Phoenix(sid)).ContinueWith(t => Log.Warn($"phoenix for session {sid} failed: {t.Exception?.InnerException?.Message}"), TaskContinuationOptions.OnlyOnFaulted);
        return result;
    }

    /// <summary>Records the generation in phoenix_chain, ends the session, and launches its successor in the same slot:
    /// a new conversation whose first prompt is the handoff. At most one per identity per <see cref="Cooldown"/>.</summary>
    async Task Phoenix(string sid)
    {
        string name, handoff;
        long gen, msg;
        lock (gate)
        {
            using var db = store.Open();
            if (db.Rows("SELECT i.name, i.generation, i.phoenix_msg, m.body FROM identities i JOIN messages m ON m.id=i.phoenix_msg WHERE i.claude_session_id=$s AND i.state='running'",
                    ("s", sid)) is not [var row]) return;
            (name, gen, msg, handoff) = (row["name"]!.ToString(), (long)row["generation"]!, (long)row["phoenix_msg"]!, row["body"]!.ToString());
            handoff = handoff[(handoff.IndexOf("\n\n") + 2)..]; // after pass_the_torch's "**Handoff recorded** (ts)." line
            db.Exec("UPDATE identities SET phoenix_msg=NULL WHERE name=$n", ("n", name));
            var now = DateTimeOffset.Parse(db.NowIso(), CultureInfo.InvariantCulture);
            if (db.Scalar("SELECT MAX(ts) FROM phoenix_chain WHERE identity=$n", ("n", name)) is string last && now - DateTimeOffset.Parse(last, CultureInfo.InvariantCulture) < Cooldown)
            {
                Log.Warn($"identity {name} handed off again within {Cooldown.TotalMinutes} minutes of its last restart: not restarting it");
                return;
            }
            db.Exec("INSERT INTO phoenix_chain (identity, generation, claude_session_id, handoff_msg, ts) VALUES ($n,$g,$s,$m,$ts)",
                ("n", name), ("g", gen), ("s", sid), ("m", msg), ("ts", db.NowIso()));
            // Still running but with no pid: its end frees no slot, so nothing queued takes the one its successor fills.
            db.Exec("UPDATE identities SET pid=NULL, claude_session_id=NULL, generation=$g, updated_ts=$ts WHERE name=$n",
                ("g", gen + 1), ("ts", db.NowIso()), ("n", name));
        }
        await Task.Delay(Settle); // the hook's answer reaches claude before claude goes
        try { await sessions.Stop(name, restart: true); }
        catch (ArgumentException) { } // it had just ended
        lock (gate)
        {
            using var db = store.Open();
            if (Get(db, name) is not { } row || row["state"]?.ToString() != "running") return; // stopped or forgotten meanwhile
            try { Launch(db, row, $"You are {name}, generation {gen + 1}. Your previous generation handed off with:\n\n{handoff}"); }
            catch (ArgumentException e) { Log.Warn($"identity {name} did not restart: {e.Message}"); Mark(db, name, "stopped"); Drain(db); return; }
            db.Reply((long)db.Scalar("SELECT thread_id FROM messages WHERE id=$m", ("m", msg))!, name, BoardDb.Agent,
                $"generation {gen + 1} started from handoff #{msg}", msg, new JsonObject { ["kind"] = "phoenix" });
        }
    }

    /// <summary>Launches queued identities, oldest first, while there are free slots. Returns the ones that failed, which are stopped.</summary>
    Dictionary<string, string> Drain(BoardDb db)
    {
        var failed = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        var max = Crew.MaxSessions(AgentBoard.Load(Path.Combine(data, "settings.json")));
        while (Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM identities WHERE state='running'")) < max
               && db.Rows("SELECT * FROM identities WHERE state='queued' ORDER BY updated_ts, name LIMIT 1") is [var next])
        {
            var name = next["name"]!.ToString();
            try { Launch(db, next); }
            catch (ArgumentException e) { failed[name] = e.Message; Mark(db, name, "stopped"); }
        }
        return failed;
    }

    /// <summary>claude --resume its conversation when there is one to resume, else --session-id a new (or never-used) id, plus the
    /// chain charter and its own; a successor gets a new conversation with <paramref name="prompt"/> as its first message.</summary>
    void Launch(BoardDb db, JsonObject row, string? prompt = null)
    {
        var (name, folder, host) = (row["name"]!.ToString(), row["folder"]!.ToString(), row["host"]!.ToString());
        var wsl = host.StartsWith("wsl:");
        Func<string, string> quote = wsl ? Sh : Win;
        var id = row["claude_session_id"]?.ToString();
        var resume = prompt is null && id is not null && (wsl || Transcript(id) is not null); // claude writes no transcript until the first message
        id ??= Guid.NewGuid().ToString();
        var charter = string.Format(Chain, name) + (row["charter"]?.ToString() is { Length: > 0 } own ? "\n\n" + own : "");
        var tier = row["model"]?.ToString() is { } model && Governor.Models.Contains(model) ? " --model " + model : ""; // its tier (haiku|sonnet|opus)
        var args = (resume ? "--resume " : "--session-id ") + id + tier + " --append-system-prompt " + quote(charter) + (prompt is null ? "" : " " + quote(prompt));
        var env = new Dictionary<string, string?> { ["AGENTDESK_IDENTITY"] = name, ["AGENTDESK_AUTHOR"] = name };
        int pid;
        if (wsl)
        {
            env["WSLENV"] = string.Join(':', new[] { Environment.GetEnvironmentVariable("WSLENV"), "AGENTDESK_IDENTITY", "AGENTDESK_AUTHOR" }.Where(v => !string.IsNullOrEmpty(v)));
            pid = sessions.Launch(name, Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
                $"wsl.exe -d {Win(host[4..])} --cd {Win(folder)} -- {claude} {args}", env);
        }
        else pid = sessions.Launch(name, folder, $"{claude} {args}", env);
        db.Exec("UPDATE identities SET state='running', pid=$pid, claude_session_id=$id, updated_ts=$ts WHERE name=$n",
            ("pid", pid), ("id", id), ("ts", db.NowIso()), ("n", name));
    }

    static string? Transcript(string id) =>
        Directory.Exists(Projects) ? Directory.EnumerateDirectories(Projects).Select(d => Path.Combine(d, id + ".jsonl")).FirstOrDefault(File.Exists) : null;

    /// <summary>A transcript's folder (the first "cwd") and its first typed user message, trimmed to 80 characters.</summary>
    static (string? Folder, string? First) Peek(string file)
    {
        string? folder = null, first = null;
        try
        {
            using var reader = new StreamReader(new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete)); // claude may be writing it
            while ((folder is null || first is null) && reader.ReadLine() is { } line)
                try
                {
                    var e = JsonDocument.Parse(line).RootElement;
                    if (folder is null && e.TryGetProperty("cwd", out var cwd)) folder = cwd.GetString();
                    if (first is not null || !e.TryGetProperty("type", out var t) || t.GetString() != "user" || (e.TryGetProperty("isMeta", out var meta) && meta.ValueKind == JsonValueKind.True)
                        || !e.TryGetProperty("message", out var m) || !m.TryGetProperty("content", out var c)) continue;
                    var text = c.ValueKind == JsonValueKind.String ? c.GetString()
                        : c.ValueKind == JsonValueKind.Array ? string.Join(' ', c.EnumerateArray().Where(p => p.TryGetProperty("type", out var pt) && pt.GetString() == "text").Select(p => p.GetProperty("text").GetString())) : null;
                    text = Spaces().Replace(Tags().Replace(text ?? "", ""), " ").Trim(); // <system-reminder>, <command-name> and friends were not typed
                    if (text.Length > 0) first = text.Length > 80 ? text[..80] : text;
                }
                catch (JsonException) { } // a line cut off mid-write
        }
        catch (IOException) { }
        return (folder, first);
    }

    static JsonObject? Get(BoardDb db, string name) => db.Rows("SELECT * FROM identities WHERE name=$n", ("n", name)) is [var row] ? row : null;

    static JsonObject Need(BoardDb db, string name) => Get(db, name) ?? throw new ArgumentException($"no such identity: {name}");

    static void Mark(BoardDb db, string name, string state) =>
        db.Exec("UPDATE identities SET state=$s, pid=NULL, phoenix_msg=NULL, updated_ts=$ts WHERE name=$n", ("s", state), ("ts", db.NowIso()), ("n", name));

    /// <summary>One argument, quoted for CreateProcess's parser.</summary>
    static string Win(string s) => s.Length > 0 && !s.Any(c => c is ' ' or '\t' or '\n' or '"')
        ? s : "\"" + Trailing().Replace(Backslashes().Replace(s, "$1$1\\\""), "$1$1") + "\"";

    /// <summary>One argument, quoted for the shell wsl.exe hands the rest of its command line to.</summary>
    static string Sh(string s) => "'" + s.Replace("'", "'\\''") + "'";

    [GeneratedRegex(@"\s+")] private static partial Regex Spaces();
    [GeneratedRegex(@"<([\w-]+)[^>]*>.*?</\1>", RegexOptions.Singleline)] private static partial Regex Tags();
    [GeneratedRegex(@"(\\*)""")] private static partial Regex Backslashes();
    [GeneratedRegex(@"(\\+)$")] private static partial Regex Trailing();

    static Task<string> Ok(JsonObject o) => Task.FromResult(o.ToJsonString(Wire.Indented));
}
