using System.Diagnostics;
using System.Text;
using System.Text.Json;
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>Identities over headless sessions, with cmd standing in for claude: it echoes its author and arguments, then stays up.</summary>
public sealed class IdentitiesTests : IDisposable
{
    const string Claude = "cmd /d /q /k echo author=%AGENTDESK_AUTHOR%";
    readonly string dir = Directory.CreateTempSubdirectory("identities-").FullName;
    readonly string projects;
    readonly BoardStore store;
    readonly List<Sessions> cores = [];
    readonly CancellationTokenSource gone = new();

    public IdentitiesTests()
    {
        File.WriteAllText(Path.Combine(dir, "settings.json"), """{"max_sessions": 2}""");
        projects = Directory.CreateDirectory(Path.Combine(dir, "projects")).FullName;
        Environment.SetEnvironmentVariable("AGENTDESK_CLAUDE_PROJECTS", projects);
        store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
    }

    public void Dispose()
    {
        gone.Cancel();
        using (var db = store.Open()) db.Exec("DELETE FROM identities"); // else a stop frees a slot and launches the next queued one
        foreach (var s in cores)
            foreach (var n in JsonDocument.Parse(s.List().Result).RootElement.GetProperty("sessions").EnumerateArray())
                try { s.Stop(n.GetProperty("name").GetString()!).Wait(); } catch (AggregateException) { }
        Environment.SetEnvironmentVariable("AGENTDESK_CLAUDE_PROJECTS", null);
    }

    /// <summary>A core: its own sessions, and identities over the shared board.</summary>
    (Sessions, Identities) Core()
    {
        var s = new Sessions();
        cores.Add(s);
        return (s, new Identities(store, s, dir, Claude));
    }

    static JsonElement Json(Task<string> t) => JsonDocument.Parse(t.Result).RootElement;

    Dictionary<string, string> States(Identities ids) =>
        Json(ids.List()).GetProperty("identities").EnumerateArray().ToDictionary(r => r.GetProperty("name").GetString()!, r => r.GetProperty("state").GetString()!);

    async Task Sees(Sessions s, string name, string text)
    {
        var seen = new StringBuilder();
        await s.Attach(name, 200, 30, e =>
        {
            if (JsonDocument.Parse(e).RootElement.TryGetProperty("data", out var d)) lock (seen) seen.Append(Encoding.UTF8.GetString(d.GetBytesFromBase64()));
            return Task.CompletedTask;
        }, gone.Token);
        for (var sw = Stopwatch.StartNew(); sw.Elapsed < TimeSpan.FromSeconds(15); await Task.Delay(50))
            lock (seen) if (seen.ToString().Contains(text)) return;
        lock (seen) Assert.Fail($"never saw '{text}' in: {seen}");
    }

    [Fact]
    public async Task Create_start_list_forget()
    {
        var (s, ids) = Core();
        await Assert.ThrowsAsync<ArgumentException>(() => ids.Create("alpha", dir, null, null, model: "gpt"));
        await ids.Create("alpha", dir, "be brief", null, model: "Opus");
        Assert.Equal("stopped", States(ids)["alpha"]);
        var row = Json(ids.Start("alpha"));
        Assert.Equal("running", row.GetProperty("state").GetString());
        var id = row.GetProperty("claude_session_id").GetString()!;
        await Sees(s, "alpha", $"author=alpha --session-id {id} --model opus --add-dir ");
        Assert.Contains(" --append-system-prompt \"You are one generation of alpha, a long-lived AgentDesk agent.", Command(s));
        Assert.EndsWith("\n\nbe brief\"", Command(s)); // the identity's own charter is last, after the state file paragraph
        var pid = row.GetProperty("pid").GetInt32();
        Assert.Contains("\"forgotten\"", await ids.Forget("alpha"));
        Assert.Empty(States(ids));
        Assert.Throws<ArgumentException>(() => Process.GetProcessById(pid));
    }

    [Fact]
    public async Task Starts_past_the_cap_queue_until_a_slot_frees()
    {
        var (_, ids) = Core();
        foreach (var n in new[] { "a", "b", "c" }) { await ids.Create(n, dir, null, null); await ids.Start(n); }
        Assert.Equal(["queued", "running", "running"], States(ids).Values.Order());
        var running = States(ids).First(p => p.Value == "running").Key;
        await ids.Stop(running);
        Assert.Equal("stopped", States(ids)[running]);
        Assert.Equal(2, States(ids).Values.Count(v => v == "running"));
    }

    [Fact]
    public async Task Requeue_is_instant_and_launches_nothing_and_DrainQueued_then_starts_Johns_own_first()
    {
        var (_, ids) = Core();
        await ids.Create("swarm", dir, null, null);
        await ids.Create("mine", dir, null, null);
        await ids.Create("mine2", dir, null, null);
        using (var db = store.Open())
        {
            db.Exec("INSERT INTO goals (name, objective, measure_folder, lead, created_ts, updated_ts) VALUES ('g', 'o', $d, 'g-lead', '2026-01-01', '2026-01-01')", ("d", dir));
            db.Exec("INSERT INTO goal_members (identity, goal, task, created_ts) VALUES ('swarm', 'g', 't', '2026-01-01')");
            db.Exec("UPDATE identities SET state='running', pid=1 WHERE 1=1");
            db.Exec("UPDATE identities SET updated_ts='2026-01-01T00:00:00+00:00' WHERE name='swarm'"); // the oldest: first in line unless John's come first
        }

        var (_, restarted) = Core();
        restarted.Requeue();
        Assert.All(States(restarted), kv => Assert.Equal("queued", kv.Value)); // nothing launched, and nothing left claiming to run
        restarted.DrainQueued(); // max_sessions is 2
        var after = States(restarted);
        Assert.Equal("running", after["mine"]);
        Assert.Equal("running", after["mine2"]);
        Assert.Equal("queued", after["swarm"]);
    }

    [Fact]
    public async Task A_restarted_core_resumes_what_was_running()
    {
        var (_, ids) = Core();
        await ids.Create("keep", dir, null, null);
        await ids.Create("auto", dir, null, null, autostart: true);
        var before = Json(ids.Start("keep"));
        var id = before.GetProperty("claude_session_id").GetString()!;
        File.WriteAllText(Path.Combine(Directory.CreateDirectory(Path.Combine(projects, "C--x")).FullName, $"{id}.jsonl"), "{}\n"); // it has a conversation now

        var (s2, restarted) = Core(); // the old core's processes are still up here; its sessions are not this core's
        restarted.Resume();
        var after = Json(restarted.List()).GetProperty("identities").EnumerateArray().ToDictionary(r => r.GetProperty("name").GetString()!);
        Assert.Equal("running", after["keep"].GetProperty("state").GetString());
        Assert.NotEqual(before.GetProperty("pid").GetInt32(), after["keep"].GetProperty("pid").GetInt32());
        Assert.Equal("running", after["auto"].GetProperty("state").GetString());
        await Sees(s2, "keep", $"author=keep --resume {id}");
    }

    static string Command(Sessions s) => Assert.Single(Json(s.List()).GetProperty("sessions").EnumerateArray()).GetProperty("command").GetString()!;

    static JsonElement Hook(string sid) => JsonDocument.Parse(JsonSerializer.Serialize(new Dictionary<string, string> { ["session_id"] = sid })).RootElement;

    JsonElement Row(Identities ids, string name) => Json(ids.List()).GetProperty("identities").EnumerateArray().Single(r => r.GetProperty("name").GetString() == name);

    /// <summary>One handoff, with the state file as the last generation left it (or none): the first launch's and the successor's command lines.</summary>
    async Task<(string First, string Successor)> HandOff(string name, string? stateFile, string? settings = null)
    {
        if (settings is not null) File.WriteAllText(Path.Combine(dir, "settings.json"), settings);
        var (s, ids) = Core();
        var board = new AgentBoard(store, null!, "wait {0}");
        await ids.Create(name, dir, null, null);
        var sid = Json(ids.Start(name)).GetProperty("claude_session_id").GetString()!;
        var first = Command(s);
        if (stateFile is not null)
        {
            Directory.CreateDirectory(Path.GetDirectoryName(Identities.StatePath(dir, name))!);
            File.WriteAllText(Identities.StatePath(dir, name), stateFile);
        }
        var me = new Caller(sid, name, dir, "claude-code", 1, name);
        await ids.Torch(me, board.PassTheTorch(me, "Owns the parser.", null));
        await ids.AfterTurn(Hook(sid), Task.FromResult(""));
        for (var sw = Stopwatch.StartNew(); Row(ids, name).GetProperty("generation").GetInt32() != 2 || Row(ids, name).GetProperty("pid").ValueKind == JsonValueKind.Null; await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(15), "never restarted");
        return (first, Command(s));
    }

    [Fact]
    public async Task The_state_file_is_in_the_charter_and_the_successor_is_given_it_verbatim()
    {
        var (firstLaunch, successor) = await HandOff("keeper", "DECISIONS\n- use the queue, because the lock was held for 30 s\nKEY FACTS\n- the id is 11111111-2222-3333-4444-555555555555");
        var path = Identities.StatePath(dir, "keeper");
        Assert.Contains($"Your state file is {path}", firstLaunch); // told where it is, from the first generation on
        Assert.Contains(" --add-dir ", firstLaunch);
        Assert.Contains("exactly as your previous generation left it", successor);
        Assert.Contains("use the queue, because the lock was held for 30 s", successor); // verbatim, not paraphrased
        Assert.Contains("11111111-2222-3333-4444-555555555555", successor);
        Assert.Contains("Owns the parser.", successor); // the handoff is still there
    }

    [Fact]
    public async Task A_successor_without_a_state_file_gets_just_the_handoff()
    {
        var (_, none) = await HandOff("bare", null);
        Assert.DoesNotContain("exactly as your previous generation left it", none);
        Assert.Contains("Owns the parser.", none);
    }

    [Fact]
    public async Task A_huge_state_file_is_cut_so_the_successor_still_starts()
    {
        // phoenix_state_chars is small here: the stand-in for claude runs under cmd.exe, whose command line is limited to 8,191 characters.
        var (_, huge) = await HandOff("big", new string('x', 5_000) + "TAIL-MARKER", """{"max_sessions": 2, "phoenix_state_chars": 1000}""");
        Assert.Contains("cut here", huge);
        Assert.DoesNotContain("TAIL-MARKER", huge);
        Assert.Contains(new string('x', 1000), huge);
        Assert.DoesNotContain(new string('x', 1001), huge); // cut at the setting
    }

    [Fact]
    public async Task Switched_off_in_settings_there_is_no_state_file_text_and_no_added_folder()
    {
        var (first, successor) = await HandOff("quiet", "DECISIONS\n- secret", """{"max_sessions": 2, "phoenix_state_file": false}""");
        Assert.DoesNotContain("Your state file", first + successor);
        Assert.DoesNotContain("--add-dir", first + successor);
        Assert.DoesNotContain("secret", successor);
    }

    [Fact]
    public async Task A_core_that_went_down_between_the_handoff_and_the_restart_still_gives_the_successor_the_handoff()
    {
        var (s, ids) = Core();
        var board = new AgentBoard(store, null!, "wait {0}");
        await ids.Create("crashy", dir, null, null);
        var sid = Json(ids.Start("crashy")).GetProperty("claude_session_id").GetString()!;
        var me = new Caller(sid, "crashy", dir, "claude-code", 1, "crashy");
        await ids.Torch(me, board.PassTheTorch(me, "Owns the lexer. Next: fuzz it.", null));
        await s.Stop("crashy");
        using (var db = store.Open()) // Phoenix's bookkeeping done, the launch not: the state the core leaves when it dies in between
        {
            var msg = (long)db.Scalar("SELECT phoenix_msg FROM identities WHERE name='crashy'")!;
            db.Exec("INSERT INTO phoenix_chain (identity, generation, claude_session_id, handoff_msg, ts) VALUES ('crashy',1,$s,$m,$ts)", ("s", sid), ("m", msg), ("ts", db.NowIso()));
            db.Exec("UPDATE identities SET state='stopped', pid=NULL, claude_session_id=NULL, generation=2, phoenix_msg=NULL WHERE name='crashy'");
        }
        await ids.Start("crashy");
        Assert.EndsWith("\"You are crashy, generation 2. Your previous generation handed off with:\n\nOwns the lexer. Next: fuzz it.\"", Command(s));
    }

    [Fact]
    public async Task A_handoff_restarts_the_identity_from_it_once_the_turn_ends()
    {
        var (s, ids) = Core();
        var board = new AgentBoard(store, null!, "wait {0}");
        await ids.Create("phx", dir, null, null);
        var sid = Json(ids.Start("phx")).GetProperty("claude_session_id").GetString()!;
        var events = new List<string>();
        await s.Attach("phx", 120, 30, e => { lock (events) events.Add(e); return Task.CompletedTask; }, gone.Token);

        var me = new Caller(sid, "phx", dir, "claude-code", 1, "phx");
        await ids.Torch(me, board.PassTheTorch(me, "Owns the parser. Next: its tests.", null));
        Assert.Equal("", await ids.AfterTurn(Hook(sid), Task.FromResult(""))); // the hook answers at once; the restart follows
        JsonElement row;
        for (var sw = Stopwatch.StartNew(); (row = Row(ids, "phx")).GetProperty("pid").ValueKind == JsonValueKind.Null || row.GetProperty("generation").GetInt32() != 2; await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(15), "never restarted");

        var next = row.GetProperty("claude_session_id").GetString()!;
        Assert.NotEqual(sid, next);
        Assert.Equal("running", row.GetProperty("state").GetString());
        Assert.StartsWith($"{Claude} --session-id {next} --model sonnet --add-dir ", Command(s)); // the state folder, so the session may edit its state file
        Assert.Contains(" --append-system-prompt ", Command(s));
        Assert.EndsWith("\"You are phx, generation 2. Your previous generation handed off with:\n\nOwns the parser. Next: its tests.\"", Command(s));
        lock (events) Assert.Contains(events, e => e.Contains("session.restarted"));
        using (var db = store.Open())
        {
            var chain = Assert.Single(db.Rows("SELECT * FROM phoenix_chain"));
            var handoff = (long)chain["handoff_msg"]!;
            Assert.Equal(("phx", 1L, sid), ((string)chain["identity"]!, (long)chain["generation"]!, (string)chain["claude_session_id"]!));
            Assert.Equal($"generation 2 started from handoff #{handoff}", db.Scalar("SELECT body FROM messages ORDER BY id DESC LIMIT 1"));
        }

        // The successor hands off straight away: within two minutes of the last restart, nothing happens.
        var successor = me with { SessionId = next };
        await ids.Torch(successor, board.PassTheTorch(successor, "Again, already.", null));
        await ids.AfterTurn(Hook(next), Task.FromResult(""));
        await Task.Delay(2500);
        Assert.Equal(next, Row(ids, "phx").GetProperty("claude_session_id").GetString());
        Assert.Equal(2, Row(ids, "phx").GetProperty("generation").GetInt32());
        using (var db = store.Open()) Assert.Single(db.Rows("SELECT * FROM phoenix_chain"));
    }

    [Fact]
    public async Task A_handoff_from_any_other_session_restarts_nothing()
    {
        var (s, ids) = Core();
        var board = new AgentBoard(store, null!, "wait {0}");
        await ids.Create("phx", dir, null, null);
        var before = Json(ids.Start("phx"));
        var sid = before.GetProperty("claude_session_id").GetString()!;
        var other = Guid.NewGuid().ToString();
        foreach (var caller in new[] { new Caller(other, null, dir, "claude-code", 1), new Caller(other, "phx", dir, "claude-code", 1, "phx"), new Caller(sid, "phx", dir, "claude-code", 1) })
        {
            await ids.Torch(caller, board.PassTheTorch(caller, "Not mine to hand off.", null));
            await ids.AfterTurn(Hook(caller.SessionId!), Task.FromResult(""));
        }
        await ids.AfterTurn(Hook(sid), Task.FromResult(""));
        await Task.Delay(2500);
        var after = Row(ids, "phx");
        Assert.Equal(JsonValueKind.Null, after.GetProperty("phoenix_msg").ValueKind);
        Assert.Equal((sid, 1, before.GetProperty("pid").GetInt32()),
            (after.GetProperty("claude_session_id").GetString(), after.GetProperty("generation").GetInt32(), after.GetProperty("pid").GetInt32()));
        using var db = store.Open();
        Assert.Empty(db.Rows("SELECT * FROM phoenix_chain"));
    }

    [Fact]
    public async Task Retire_stops_the_identity_at_once_and_frees_the_slot()
    {
        var (_, ids) = Core();
        await ids.Create("done", dir, null, null);
        await ids.Create("waiting", dir, null, null);
        var sid = Json(ids.Start("done")).GetProperty("claude_session_id").GetString()!;
        await ids.Start("waiting");
        var me = new Caller(sid, "done", dir, "claude-code", 1, "done");

        Assert.Throws<ArgumentException>(() => { _ = ids.Retire(me with { Identity = null }); }); // no identity, no headless session
        Assert.Throws<ArgumentException>(() => { _ = ids.Retire(me with { Identity = "waiting-not" }); }); // not an identity

        await ids.Retire(me); // no turn end needed: it is killed within a second
        for (var sw = Stopwatch.StartNew(); States(ids)["done"] != "stopped"; await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(5), "never retired");
        Assert.Equal("running", States(ids)["waiting"]);
        Assert.Equal(sid, Row(ids, "done").GetProperty("claude_session_id").GetString()); // stopped, not forgotten: it resumes this conversation
    }

    [Fact]
    public async Task An_idle_identity_of_johns_is_named_to_give_up_its_slot_and_a_busy_one_is_not()
    {
        var (_, ids) = Core();
        await ids.Create("idle", dir, null, null);
        await ids.Create("busy", dir, null, null);
        var idleSid = Json(ids.Start("idle")).GetProperty("claude_session_id").GetString()!;
        var busySid = Json(ids.Start("busy")).GetProperty("claude_session_id").GetString()!;
        var folder = Directory.CreateDirectory(Path.Combine(projects, "C--x")).FullName;
        File.WriteAllText(Path.Combine(folder, idleSid + ".jsonl"), "{}");
        File.WriteAllText(Path.Combine(folder, busySid + ".jsonl"), "{}");
        File.SetLastWriteTimeUtc(Path.Combine(folder, idleSid + ".jsonl"), DateTime.UtcNow.AddHours(-2)); // quiet for 2 hours
        ids.IdleAfter = TimeSpan.FromMinutes(30);
        using var db = store.Open();
        var later = DateTimeOffset.UtcNow.AddHours(1); // past both identities' launch times
        // busy wrote just now, so it is not idle even an hour on only if its transcript is newer than the threshold allows
        File.SetLastWriteTimeUtc(Path.Combine(folder, busySid + ".jsonl"), later.UtcDateTime.AddMinutes(-5));
        Assert.Equal(["idle"], ids.IdleJohns(db, later));
        Assert.Empty(ids.IdleJohns(db, DateTimeOffset.UtcNow)); // nothing is idle at launch
    }

    [Fact]
    public async Task Johns_reply_starts_a_stopped_asker_unless_it_already_has_it_or_is_no_identity()
    {
        var (_, ids) = Core();
        await ids.Create("asker", dir, null, null);
        await ids.Create("quiet", dir, null, null);
        long Reply(string agent)
        {
            using var db = store.Open();
            var tid = db.StartThread("question", $"from {agent}", agent, BoardDb.Agent, "which one?");
            var mid = db.JohnReplies(tid, "that one");
            if (agent == "quiet") db.Exec("INSERT INTO deliveries(message_id, method, state, ts) VALUES($m, 'watcher', 'woke', $ts)", ("m", mid), ("ts", db.NowIso()));
            return tid;
        }
        await ids.AutoWake((int)Reply("asker"));
        await ids.AutoWake((int)Reply("quiet")); // its wait already returned the reply
        await ids.AutoWake((int)Reply("stranger")); // not an identity: never adopted by a reply
        var states = States(ids);
        Assert.Equal(("running", "stopped", false), (states["asker"], states["quiet"], states.ContainsKey("stranger")));
    }

    [Fact]
    public async Task Adoptable_lists_recent_transcripts_and_adopt_resumes_one()
    {
        var id = Guid.NewGuid().ToString();
        var folder = Directory.CreateDirectory(Path.Combine(projects, "C--work")).FullName;
        var cwd = JsonSerializer.Serialize(dir);
        File.WriteAllLines(Path.Combine(folder, $"{id}.jsonl"),
        [
            """{"type":"summary","summary":"x"}""",
            $$$"""{"type":"user","isMeta":true,"cwd":{{{cwd}}},"message":{"role":"user","content":"caveat"}}""",
            $$$"""{"type":"user","cwd":{{{cwd}}},"message":{"role":"user","content":"<command-name>/clear</command-name>"}}""",
            $$$"""{"type":"user","cwd":{{{cwd}}},"message":{"role":"user","content":[{"type":"text","text":"<system-reminder>\nnot typed\n</system-reminder>Fix the\nflaky test in {{{new string('x', 100)}}}"}]}}""",
            """{"type":"assistant","message":{"role":"assistant","content":"ok"}}""",
        ]);
        var old = Path.Combine(folder, $"{Guid.NewGuid()}.jsonl");
        File.WriteAllText(old, "{}\n");
        File.SetLastWriteTimeUtc(old, DateTime.UtcNow.AddDays(-2));
        File.WriteAllText(Path.Combine(folder, $"{Guid.NewGuid()}.jsonl"), // claude -p /usage: nobody typed in it
            """{"type":"user","message":{"role":"user","content":"<command-name>/usage</command-name>"}}""" + "\n");

        var row = Assert.Single(Json(Identities.Adoptable()).GetProperty("sessions").EnumerateArray());
        Assert.Equal(id, row.GetProperty("session_id").GetString());
        Assert.Equal(dir, row.GetProperty("folder").GetString());
        var first = row.GetProperty("first_message").GetString()!;
        Assert.StartsWith("Fix the flaky test in xxx", first);
        Assert.Equal(80, first.Length);

        var (s, ids) = Core();
        var adopted = Json(ids.Adopt(id, "adoptee"));
        Assert.Equal("running", adopted.GetProperty("state").GetString());
        Assert.Equal(id, adopted.GetProperty("claude_session_id").GetString());
        Assert.Contains("desktop app", adopted.GetProperty("note").GetString());
        await Sees(s, "adoptee", $"author=adoptee --resume {id}");
    }
}
