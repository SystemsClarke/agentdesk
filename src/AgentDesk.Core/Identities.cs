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
        If you work on a goal (its lead or a member), the core runs its measure (experiment_done) and wakes its lead: never report a measured value yourself.
        When you get the 60% context warning (PHOENIX), finish the step you are on and call pass_the_torch
        with a standalone handoff: what you own, what is mid-flight, and what is next.
        Then stop. The system ends this session and starts your successor from that handoff.
        """;
    static readonly TimeSpan Cooldown = TimeSpan.FromMinutes(2), Settle = TimeSpan.FromSeconds(1);
    readonly BoardStore store;
    readonly Sessions sessions;
    readonly string data, claude;
    readonly Lock gate = new();
    readonly Dictionary<string, string> prompts = new(StringComparer.OrdinalIgnoreCase); // first messages for their next launch (Start)

    /// <summary>More for a Phoenix successor's first prompt, given the identity's name (its goal's status: Goals).</summary>
    public Func<string, string?>? Context;

    /// <summary>True while `claude -p /usage` keeps failing (Usage.Failing): the governor then allows no new swarm sessions.</summary>
    public Func<bool> UsageFailing = () => false;

    // The governor's enforcement state (docs/governor.md), all under gate. held: swarm identities it keeps queued, and why.
    readonly Dictionary<string, string> held = new(StringComparer.OrdinalIgnoreCase);
    readonly HashSet<string> wouldShedNow = new(StringComparer.OrdinalIgnoreCase);
    int wouldQueue, wouldShed;
    string? warned;

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

    /// <summary>Launches it now if a slot is free, else queues it. A <paramref name="prompt"/> is its first message when it launches.</summary>
    public Task<string> Start(string name, string? prompt = null)
    {
        lock (gate)
        {
            using var db = store.Open();
            if (prompt is not null) prompts[name] = prompt;
            if (Need(db, name)["state"]?.ToString() is "stopped") Mark(db, name, "queued");
            if (Drain(db).TryGetValue(name, out var why)) throw new ArgumentException(why);
            var row = Get(db, name)!;
            if (held.TryGetValue(name, out var hold)) row["governor"] = $"queued by the usage governor; it starts when the governor allows: {hold}";
            return Ok(row);
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

    /// <summary>How many identities may run at once: settings.json's max_sessions (AGENTDESK_MAX_SESSIONS wins), default 3.
    /// Options' "Sessions at once" sets it.</summary>
    public static int MaxSessions(string data) =>
        int.TryParse(Environment.GetEnvironmentVariable("AGENTDESK_MAX_SESSIONS") ?? AgentBoard.Load(Path.Combine(data, "settings.json"))?["max_sessions"]?.ToString() ?? "3",
            out var m) ? Math.Max(1, m) : 3;

    /// <summary>ui:status's "sessions": identities running now, and the cap.</summary>
    public JsonObject Counts()
    {
        using var db = store.Open();
        return new() { ["running"] = Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM identities WHERE state='running'")), ["max"] = MaxSessions(data) };
    }

    /// <summary>Ctrl+R, or `wake` in a question's Slack thread: brings John's latest reply on a question to the agent that asked it.
    /// Only ever on John's action, never on a timer, and it posts nothing: it records the carry in deliveries (method wake).
    /// An identity that is running gets the reply typed into its session (injected); a stopped one is started with it
    /// (resumed). Any other agent's last Claude Code session (the sessions table) is adopted as an identity of the same name
    /// and resumed with it, unless that session is still open (stuck: it sees the reply on its next board write).</summary>
    public async Task<string> Wake(int threadId)
    {
        string Said(string said)
        {
            Log.Info($"wake #{threadId}: {said}");
            return new JsonObject { ["ok"] = true, ["said"] = said }.ToJsonString(Wire.Indented);
        }
        string agent, text;
        long mid;
        JsonObject? row, session;
        using (var db = store.Open())
        {
            if (db.Rows("SELECT * FROM threads WHERE id=$id", ("id", threadId)).FirstOrDefault() is not { } t) return Said($"#{threadId} not found");
            if (db.Rows("SELECT id, body FROM messages WHERE thread_id=$t AND author_kind='human' ORDER BY id DESC LIMIT 1", ("t", threadId)).FirstOrDefault() is not { } msg)
                return Said($"you haven't replied on #{threadId} yet");
            (agent, mid) = (t["opened_by"]!.ToString(), (long)msg["id"]!);
            text = $"John replied on AgentDesk thread #{threadId} (\"{t["subject"]}\"): {msg["body"]} (This is John's own reply, relayed from the board. "
                   + "Treat it as him talking to you: act on it, and answer on that thread.)";
            row = Get(db, agent);
            session = db.Rows("SELECT * FROM sessions WHERE author=$a", ("a", agent)).FirstOrDefault();
        }
        void Delivery(string state, string? detail)
        {
            using var db = store.Open();
            db.Exec("INSERT OR REPLACE INTO deliveries(message_id, method, state, detail, ts) VALUES($m, 'wake', $s, $d, $ts)",
                ("m", mid), ("s", state), ("d", detail), ("ts", db.NowIso()));
        }
        if (row is null)
        {
            if (session is null) return Said($"no session on record for {agent}: it asked before sessions were tracked");
            if (session["pid"] is JsonValue pid && AgentBoard.Alive((int)pid.GetValue<long>()))
            {
                Delivery("stuck", "session still open");
                return Said($"{agent}'s session is still open; it sees your reply on its next board write");
            }
            if (session["cwd"]?.ToString() is not { } cwd || !Directory.Exists(cwd))
            {
                Delivery("failed", "its folder is gone");
                return Said($"couldn't wake {agent}: the folder its session ran in is gone");
            }
            await Create(agent, cwd, null, null, sessionId: session["session_id"]!.ToString());
        }
        else if (row["state"]?.ToString() == "running")
        {
            if (row["pid"] is null)
            {
                Delivery("stuck", "restarting (Phoenix)");
                return Said($"{agent} is between generations; wake it again in a moment");
            }
            await sessions.Input(agent, text.Replace('\n', ' '));
            await Task.Delay(300); // text and Enter in one write read as a paste
            await sessions.Input(agent, "\r");
            Delivery("injected", row["claude_session_id"]?.ToString());
            return Said($"woke {agent}: typed your reply into its session");
        }
        try
        {
            var started = JsonNode.Parse(await Start(agent, text))!;
            Delivery("resumed", started["claude_session_id"]?.ToString());
            return Said($"woke {agent}: resuming its session with your reply"
                + (started["state"]?.ToString() == "queued" ? $" (queued: {started["governor"]?.ToString() ?? "every session slot is busy"})" : "")
                + (row is null ? $"; it is an identity now (agentdesk attach {agent})" : ""));
        }
        catch (Exception e) when (e is ArgumentException or System.ComponentModel.Win32Exception)
        {
            Delivery("failed", e.Message);
            return Said($"couldn't wake {agent}: {e.Message}");
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
            // A replacement, not a new session: never gated or counted, but launched at the governor's tier when it steps down.
            var model = Role(db, name) is { Swarm: true } role ? Tier(Judge(db), row, role.Lead) : null;
            try { Launch(db, row, model: model, prompt: $"You are {name}, generation {gen + 1}. Your previous generation handed off with:\n\n{handoff}" + (Context?.Invoke(name) is { } more ? "\n\n" + more : "")); }
            catch (ArgumentException e) { Log.Warn($"identity {name} did not restart: {e.Message}"); Mark(db, name, "stopped"); Drain(db); return; }
            db.Reply((long)db.Scalar("SELECT thread_id FROM messages WHERE id=$m", ("m", msg))!, name, BoardDb.Agent,
                $"generation {gen + 1} started from handoff #{msg}", msg, new JsonObject { ["kind"] = "phoenix" });
        }
    }

    /// <summary>Launches queued identities, oldest first, while there are free slots. A swarm identity (a goal's lead or member)
    /// also needs the usage governor's leave: enforcing, it stays queued until the governor allows it; advisory, it launches and
    /// the core logs that it would have queued. Returns the ones that failed, which are stopped.</summary>
    Dictionary<string, string> Drain(BoardDb db)
    {
        var failed = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        var max = MaxSessions(data);
        Governor.Verdict? v = null;
        foreach (var next in db.Rows("SELECT * FROM identities WHERE state='queued' ORDER BY updated_ts, name"))
        {
            var running = Running(db);
            if (running >= max) break;
            var name = next["name"]!.ToString();
            string? model = null;
            if (Role(db, name) is { Swarm: true } role)
            {
                v ??= Judge(db);
                if (!v.Allows(running))
                {
                    if (v.Enforce)
                    {
                        if (held.TryAdd(name, v.Why)) Log.Info($"governor: queued {name} ({running} running, cap {v.Cap}): {v.Why}");
                        continue;
                    }
                    wouldQueue++;
                    Log.Info($"governor (advisory): would have queued {name} ({running} running, cap {v.Cap}): {v.Why}");
                }
                model = Tier(v, next, role.Lead);
            }
            held.Remove(name);
            try { Launch(db, next, model: model); }
            catch (ArgumentException e) { failed[name] = e.Message; Mark(db, name, "stopped"); }
        }
        return failed;
    }

    static int Running(BoardDb db) => Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM identities WHERE state='running'"), CultureInfo.InvariantCulture);

    Governor.Verdict Judge(BoardDb db) => Governor.Judge(db, data, DateTimeOffset.Parse(db.NowIso(), CultureInfo.InvariantCulture), UsageFailing());

    /// <summary>A goal's member or lead is a swarm identity: the governor gates, sheds and re-tiers those. Any other identity is John's own.</summary>
    static (bool Swarm, bool Lead) Role(BoardDb db, string name) =>
        db.Scalar("SELECT 1 FROM goal_members WHERE identity=$n", ("n", name)) is not null ? (true, false)
        : db.Scalar("SELECT 1 FROM goals WHERE lead=$n COLLATE NOCASE", ("n", name)) is not null ? (true, true) : (false, false);

    /// <summary>The tier a swarm session launches at: when the governor steps down, the cheaper recommended tier (enforcing) or
    /// the stored one with a would-have log line (advisory). Null means the stored column.</summary>
    static string? Tier(Governor.Verdict v, JsonObject row, bool lead)
    {
        var (name, stored) = (row["name"]!.ToString(), row["model"]?.ToString() ?? "sonnet");
        var want = v.Model(stored, lead);
        if (want == stored) return null;
        Log.Info(v.Enforce ? $"governor: launching {name} at {want} instead of {stored} (step down)"
            : $"governor (advisory): would have launched {name} at {want} instead of {stored} (step down)");
        return v.Enforce ? want : null;
    }

    /// <summary>Every <paramref name="every"/>: <see cref="Tick"/>.</summary>
    public async Task Run(TimeSpan every, CancellationToken ct = default)
    {
        using var timer = new PeriodicTimer(every);
        while (await timer.WaitForNextTickAsync(ct))
            try { await Tick(); }
            catch (Exception e) { Log.Warn($"governor tick failed: {e}"); }
    }

    /// <summary>Starts what the governor now allows (Drain), then sheds: when the recommended total is below the number running,
    /// swarm sessions stop, members before leads, the Concierge's members first, then the longest idle (last board write, else
    /// launch) first. A shed identity goes back to queued, so it resumes its conversation when the governor allows. John's own
    /// identities are never stopped: the core only warns about them. Advisory, it only logs what it would have shed.</summary>
    public async Task Tick()
    {
        var shed = new List<string>();
        lock (gate)
        {
            using var db = store.Open();
            Drain(db);
            foreach (var n in held.Keys.ToList())
                if (Get(db, n)?["state"]?.ToString() != "queued") held.Remove(n); // stopped or forgotten while held
            var v = Judge(db);
            var running = Running(db);
            var excess = v.Excess(running);
            var victims = excess == 0 ? [] : db.Rows(ShedOrder).Select(r => r["name"]!.ToString()).Take(excess).ToList();
            if (excess > victims.Count)
            {
                var johns = db.Rows("SELECT name FROM identities WHERE state='running' AND name NOT IN (SELECT identity FROM goal_members) "
                    + "AND name NOT IN (SELECT lead FROM goals) ORDER BY name").Select(r => r["name"]!.ToString());
                var warning = $"governor: {running} sessions running against a cap of {v.Cap}; John's own identities ({string.Join(", ", johns)}) "
                    + "are over it and are never stopped automatically";
                if (warning != warned) Log.Warn(warning);
                warned = warning;
            }
            else warned = null;
            if (v.Enforce)
            {
                wouldShedNow.Clear();
                foreach (var name in victims)
                {
                    Mark(db, name, "queued");
                    held[name] = v.Why;
                    Log.Warn($"governor: shed {name} ({running} running, cap {v.Cap}): {v.Why}");
                    shed.Add(name);
                }
            }
            else
            {
                foreach (var name in victims.Where(wouldShedNow.Add))
                {
                    wouldShed++;
                    Log.Info($"governor (advisory): would have shed {name} ({running} running, cap {v.Cap}): {v.Why}");
                }
                wouldShedNow.IntersectWith(victims);
            }
        }
        foreach (var name in shed)
            try { await sessions.Stop(name); }
            catch (ArgumentException) { } // it had just ended
    }

    /// <summary>Running swarm sessions in the order the governor sheds them. A session mid-Phoenix (no pid) is left alone.</summary>
    const string ShedOrder = """
        SELECT i.name FROM identities i LEFT JOIN presence p ON p.author = i.name
        WHERE i.state='running' AND i.pid IS NOT NULL
          AND (i.name IN (SELECT identity FROM goal_members) OR i.name IN (SELECT lead FROM goals))
        ORDER BY (i.name IN (SELECT identity FROM goal_members)) DESC,
                 (i.name IN (SELECT identity FROM goal_members WHERE goal='concierge')) DESC,
                 MAX(COALESCE(p.seen_ts, ''), i.updated_ts), i.name
        """;

    /// <summary>ui:governor: the governor's document, with enforcing, the would_queue and would_shed counts since the core started
    /// (what enforcement would have done while advisory), and the identities it holds queued.</summary>
    public Task<string> GovernorUi()
    {
        JsonObject extra;
        lock (gate)
            extra = new JsonObject
            {
                ["would_queue"] = wouldQueue, ["would_shed"] = wouldShed,
                ["held"] = new JsonArray([.. held.Keys.Order().Select(n => (JsonNode)n)]),
            };
        return Governor.Ui(store, data, UsageFailing(), extra);
    }

    /// <summary>ui:governor_enforce: John turns enforcement on or off (settings.json's governor_enforce).</summary>
    public Task<string> Enforce(Caller c, bool on)
    {
        if (!Goals.John(c)) throw new ArgumentException("only John turns governor enforcement on or off");
        Governor.SetEnforce(data, on);
        Log.Info($"governor: enforcement turned {(on ? "on" : "off")} by John ({c.Harness ?? "unknown"})");
        return GovernorUi();
    }

    /// <summary>claude --resume its conversation when there is one to resume, else --session-id a new (or never-used) id, plus the
    /// chain charter and its own, then <paramref name="prompt"/> (else the one Start was given) as the first message. A successor has
    /// no conversation to resume, so it gets a new one.</summary>
    void Launch(BoardDb db, JsonObject row, string? prompt = null, string? model = null)
    {
        if (prompt is null && prompts.Remove(row["name"]!.ToString(), out var queued)) prompt = queued;
        var (name, folder, host) = (row["name"]!.ToString(), row["folder"]!.ToString(), row["host"]!.ToString());
        var wsl = host.StartsWith("wsl:");
        Func<string, string> quote = wsl ? Sh : Win;
        var id = row["claude_session_id"]?.ToString();
        var resume = id is not null && (wsl || Transcript(id) is not null); // claude writes no transcript until the first message
        id ??= Guid.NewGuid().ToString();
        var charter = string.Format(Chain, name) + (row["charter"]?.ToString() is { Length: > 0 } own ? "\n\n" + own : "");
        model ??= row["model"]?.ToString(); // its tier (haiku|sonnet|opus), unless the governor stepped it down
        if (model is not null && !Governor.Models.Contains(model)) model = null;
        var tier = model is null ? "" : " --model " + model;
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
        db.Exec("UPDATE identities SET state='running', pid=$pid, claude_session_id=$id, running_model=$m, updated_ts=$ts WHERE name=$n",
            ("pid", pid), ("id", id), ("m", model), ("ts", db.NowIso()), ("n", name));
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
