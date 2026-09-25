using System.Collections.Concurrent;
using System.Diagnostics;
using System.Globalization;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// Goals (docs/GOAL.md): a hypothesis, a measure the core runs itself, a success line, a budget and an experiment log.
/// The lead identity (&lt;goal&gt;-lead) proposes the test and John approves it. While it runs, the core wakes the lead every
/// cadence and after each measure, and the lead spawns member identities (&lt;goal&gt;-&lt;name&gt;) within max_members. The loop
/// ends when a measure crosses the success line, when the budget's hours are spent, or when John stops it.
/// </summary>
public sealed partial class Goals
{
    const string Author = "agentdesk";
    const string LeadCharter = """
        You lead an AgentDesk goal: a hypothesis tested by a measure that the core runs itself. You think and dispatch;
        members do the work. Keep each member's task small (one experiment) and pick the cheapest capable model
        (haiku for mechanical work, sonnet by default, opus only for hard reasoning).
        - member_spawn {goal, name, task, model} starts a member (within the goal's max_members); it retires with member_done.
        - experiment_start {goal, change} before a change, experiment_done {goal, n} once it is in place: the core runs the
          measure, records the value and posts the verdict on the goal thread. Never report a measured value yourself.
        - The core wakes you with the goal's status every cadence and after every measure. Between wakes, stop.
        """;
    static readonly TimeSpan Settle = TimeSpan.FromSeconds(1);
    readonly BoardStore store;
    readonly Identities ids;
    readonly Sessions sessions;
    readonly Lock gate = new();
    readonly ConcurrentDictionary<string, SemaphoreSlim> measuring = new(StringComparer.OrdinalIgnoreCase);
    public TimeSpan MeasureTimeout = TimeSpan.FromMinutes(10), InlineWait = TimeSpan.FromSeconds(20);

    public Goals(BoardStore store, Identities ids, Sessions sessions)
    {
        (this.store, this.ids, this.sessions) = (store, ids, sessions);
        ids.Context = Context;
    }

    /// <summary>John: the window, the CLI in his own terminal, or Slack. Never an agent's session.</summary>
    public static bool John(Caller c) => c.Identity is null && c.SessionId is null && c.Harness != "claude-code";

    public async Task<string> Create(string name, string objective, string folder)
    {
        if (!NameRe().IsMatch(name)) throw new ArgumentException("a goal name is letters, digits and dashes (at most 32), starting with a letter");
        if (string.IsNullOrWhiteSpace(objective)) throw new ArgumentException("objective is required");
        if (!Directory.Exists(folder)) throw new ArgumentException($"no such folder: {folder}");
        var lead = name + "-lead";
        using (var db = store.Open())
            if (Get(db, name) is not null) throw new ArgumentException($"goal already exists: {name}");
        await ids.Create(lead, folder, LeadCharter, null, model: "opus");
        using (var db = store.Open())
        {
            var thread = db.StartThread("discussion", $"goal: {name}", Author, BoardDb.Agent,
                $"**Goal {name}** (draft): {objective}\n\nLead: {lead}, in `{folder}`. It proposes a hypothesis, a measure and a success line here; "
                + $"John approves with `agentdesk goal approve {name}`.");
            db.Exec("INSERT INTO goals (name, objective, measure_folder, lead, thread_id, created_ts, updated_ts) VALUES ($n,$o,$f,$l,$t,$ts,$ts)",
                ("n", name), ("o", objective), ("f", folder), ("l", lead), ("t", thread), ("ts", db.NowIso()));
        }
        await ids.Start(lead, $"You lead goal {name}. Objective: {objective}\n\nPropose how to test it: call goal_propose with name={name}, a hypothesis, "
            + $"a measure_cmd (the core runs it in {folder}; the metric is the last number it prints, or exit code 0 means pass) and a success line "
            + "(value < N, value <= N, value > N, value >= N, or pass). Then stop: John approves it, and the core wakes you when the goal runs.");
        return await Status(name);
    }

    public Task<string> Propose(Caller c, string name, string hypothesis, string measureCmd, string success, int samples)
    {
        using var db = store.Open();
        var g = Lead(c, Need(db, name));
        if (Str(g, "state") != "draft") throw new ArgumentException($"goal {name} is {Str(g, "state")}: proposals are for drafts");
        ParseLine(success);
        if (string.IsNullOrWhiteSpace(hypothesis) || string.IsNullOrWhiteSpace(measureCmd)) throw new ArgumentException("hypothesis and measure_cmd are required");
        db.Exec("UPDATE goals SET hypothesis=$h, measure_cmd=$m, success=$s, samples=$k, updated_ts=$ts WHERE name=$n",
            ("h", hypothesis), ("m", measureCmd), ("s", success.Trim()), ("k", Math.Clamp(samples, 1, 9)), ("ts", db.NowIso()), ("n", name));
        db.Reply(Long(g, "thread_id"), Str(g, "lead")!, BoardDb.Agent,
            $"**Proposal.** Hypothesis: {hypothesis}\n\nMeasure: `{measureCmd}` in `{Str(g, "measure_folder")}`, success when {success.Trim()}.\n\n"
            + $"Waiting for John: `agentdesk goal approve {name}`.");
        return Ok(Need(db, name));
    }

    public Task<string> Approve(Caller c, string name, int? maxMembers, double? maxHours, double? cadence)
    {
        if (!John(c)) throw new ArgumentException("only John approves a goal");
        using var db = store.Open();
        var g = Need(db, name);
        if (Str(g, "measure_cmd") is null) throw new ArgumentException($"goal {name} has no proposal yet (goal_propose)");
        if (Str(g, "state") is "running" or "succeeded") throw new ArgumentException($"goal {name} is already {Str(g, "state")}");
        db.Exec("UPDATE goals SET state='running', started_ts=$ts, woke_ts=NULL, updated_ts=$ts, max_members=COALESCE($m, max_members), "
                + "max_hours=COALESCE($h, max_hours), cadence_minutes=COALESCE($c, cadence_minutes) WHERE name=$n",
            ("ts", db.NowIso()), ("m", maxMembers), ("h", maxHours), ("c", cadence), ("n", name));
        g = Need(db, name);
        db.Reply(Long(g, "thread_id"), Author, BoardDb.Agent, $"**Approved by John.** Budget: {g["max_members"]} members, {g["max_hours"]} h; the lead wakes every {g["cadence_minutes"]} min.");
        return Ok(g); // the loop's next tick wakes the lead
    }

    public Task<string> Stop(Caller c, string name)
    {
        using var db = store.Open();
        var g = Lead(c, Need(db, name));
        End(db, g, "stopped", John(c) ? "stopped by John" : $"stopped by {c.Identity}");
        return Ok(Need(db, name));
    }

    public Task<string> List() => Ok(new JsonObject { ["goals"] = Summaries() });

    const string ListSql = """
        SELECT g.name, g.state, g.objective, g.lead, g.thread_id, g.success,
          (SELECT COUNT(*) FROM experiments e WHERE e.goal=g.name) AS experiments,
          (SELECT value FROM experiments e WHERE e.goal=g.name AND e.measured_ts IS NOT NULL ORDER BY e.n DESC LIMIT 1) AS last_value,
          (SELECT COUNT(*) FROM goal_members m WHERE m.goal=g.name) AS members, g.max_members
        FROM goals g ORDER BY g.updated_ts DESC
        """;

    /// <summary>The goal, its experiments, the metric history (measured values, oldest first), its members, and the summary text agents get.</summary>
    public Task<string> Status(string name)
    {
        using var db = store.Open();
        var g = Need(db, name);
        g["experiments"] = new JsonArray([.. db.Rows("SELECT * FROM experiments WHERE goal=$g ORDER BY n", ("g", name))]);
        g["history"] = new JsonArray([.. db.Rows("SELECT value FROM experiments WHERE goal=$g AND value IS NOT NULL ORDER BY n", ("g", name)).Select(r => r["value"]!.DeepClone())]);
        g["members"] = new JsonArray([.. db.Rows("SELECT identity, task, created_ts FROM goal_members WHERE goal=$g ORDER BY created_ts", ("g", name))]);
        g["summary"] = Summary(db, name);
        return Ok(g);
    }

    /// <summary>ui:status's "goals": ui:goal_list's rows.</summary>
    public JsonArray Summaries()
    {
        using var db = store.Open();
        return new JsonArray([.. db.Rows(ListSql)]);
    }

    public Task<string> ExperimentStart(Caller c, string goal, string change)
    {
        lock (gate)
        {
            using var db = store.Open();
            var g = Running(db, goal);
            var owner = Member(db, c, g);
            var n = (long)db.Scalar("SELECT COALESCE(MAX(n),0)+1 FROM experiments WHERE goal=$g", ("g", goal))!;
            db.Exec("INSERT INTO experiments (goal, n, change, owner, started_ts) VALUES ($g,$n,$c,$o,$ts)", ("g", goal), ("n", n), ("c", change), ("o", owner), ("ts", db.NowIso()));
            return Ok(new JsonObject { ["goal"] = goal, ["n"] = n, ["note"] = "Make the change, then call experiment_done with this n: the core runs the measure." });
        }
    }

    /// <summary>Runs the measure for experiment n. Returns the recorded result if it finishes within <see cref="InlineWait"/>, else says it is measuring.</summary>
    public async Task<string> ExperimentDone(Caller c, string goal, int n)
    {
        using (var db = store.Open())
        {
            Member(db, c, Running(db, goal));
            if (db.Rows("SELECT measured_ts FROM experiments WHERE goal=$g AND n=$n", ("g", goal), ("n", n)) is not [var e]) throw new ArgumentException($"no experiment {n} on goal {goal}");
            if (e["measured_ts"] is not null) throw new ArgumentException($"experiment {n} was already measured");
        }
        var run = Measure(goal, n);
        return await Task.WhenAny(run, Task.Delay(InlineWait)) == run ? await run
            : await Ok(new JsonObject { ["goal"] = goal, ["n"] = n, ["measuring"] = true, ["note"] = "The verdict is posted on the goal thread and the lead is woken with it." });
    }

    public async Task<string> Spawn(Caller c, string goal, string name, string task, string? model)
    {
        model = string.IsNullOrWhiteSpace(model) ? "sonnet" : model.ToLowerInvariant();
        if (!Governor.Models.Contains(model)) throw new ArgumentException("model is haiku, sonnet or opus");
        if (!NameRe().IsMatch(name)) throw new ArgumentException("a member name is letters, digits and dashes");
        var id = $"{goal}-{name}";
        JsonObject g;
        lock (gate)
        {
            using var db = store.Open();
            g = Lead(c, Running(db, goal));
            var count = Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM goal_members WHERE goal=$g", ("g", goal)));
            if (count >= Long(g, "max_members")) throw new ArgumentException($"budget: goal {goal} has {count} of {g["max_members"]} members; wait for one to call member_done");
            db.Exec("INSERT INTO goal_members (identity, goal, task, created_ts) VALUES ($i,$g,$t,$ts)", ("i", id), ("g", goal), ("t", task), ("ts", db.NowIso()));
        }
        try
        {
            await ids.Create(id, Str(g, "measure_folder")!, $"""
                You are a member of the AgentDesk goal {goal}, dispatched by its lead {g["lead"]}. Objective: {g["objective"]}
                Hypothesis: {g["hypothesis"]}
                Your task: {task}
                Call experiment_start {goal}, change before you change anything, and experiment_done with its n once the change is in place:
                the core runs the measure and posts the verdict on thread #{g["thread_id"]}. Never report a measured value yourself.
                When the task is done, call member_done with a short summary; that retires you.
                """, null, model: model);
        }
        catch { using var db = store.Open(); db.Exec("DELETE FROM goal_members WHERE identity=$i", ("i", id)); throw; }
        return await ids.Start(id, $"Your task for goal {goal}: {task}"); // queued past max_sessions, launched when a slot frees
    }

    /// <summary>A member retires: its summary goes on the goal thread, then its identity is forgotten and the lead is woken.</summary>
    public Task<string> MemberDone(Caller c, string summary)
    {
        string goal, id = c.Identity ?? throw new ArgumentException("member_done is for goal members");
        lock (gate)
        {
            using var db = store.Open();
            goal = db.Scalar("SELECT goal FROM goal_members WHERE identity=$i", ("i", id)) as string ?? throw new ArgumentException($"{id} is not a goal member");
            db.Reply(Long(Need(db, goal), "thread_id"), id, BoardDb.Agent, $"**Done.** {summary}");
            db.Exec("DELETE FROM goal_members WHERE identity=$i", ("i", id));
        }
        _ = Later(async () => { await ids.Forget(id); await Wake(goal); });
        return Ok(new JsonObject { ["retired"] = id });
    }

    /// <summary>Every <paramref name="every"/>: ends goals whose hours are spent and wakes leads whose cadence is due.</summary>
    public async Task Run(TimeSpan every, CancellationToken ct = default)
    {
        using var timer = new PeriodicTimer(every);
        while (await timer.WaitForNextTickAsync(ct))
            try { await Tick(); }
            catch (Exception e) { Log.Warn($"goals tick failed: {e}"); }
    }

    public async Task Tick()
    {
        var due = new List<string>();
        using (var db = store.Open())
        {
            var now = Time(db.NowIso());
            foreach (var g in db.Rows("SELECT * FROM goals WHERE state='running'"))
                if ((now - Time(Str(g, "started_ts")!)).TotalHours >= (double)g["max_hours"]!)
                    End(db, g, "exhausted", $"budget spent: {g["max_hours"]} h");
                else if (Str(g, "woke_ts") is not { } woke || (now - Time(woke)).TotalMinutes >= (double)g["cadence_minutes"]!)
                    due.Add(Str(g, "name")!);
        }
        foreach (var name in due) await Wake(name);
    }

    /// <summary>Types the goal's status into the lead's session and presses Enter, or starts the lead with it as its prompt.</summary>
    public async Task Wake(string name)
    {
        string lead, text;
        JsonObject? who;
        using (var db = store.Open())
        {
            if (Get(db, name) is not { } g || Str(g, "state") != "running") return;
            db.Exec("UPDATE goals SET woke_ts=$ts WHERE name=$n", ("ts", db.NowIso()), ("n", name));
            lead = Str(g, "lead")!;
            text = $"[AgentDesk goal wake] {Summary(db, name).Replace("\n", " / ")} / Next: decide the next experiment and dispatch it "
                   + "(member_spawn), or run it yourself (experiment_start, experiment_done). Then stop until the next wake.";
            who = db.Rows("SELECT state, pid FROM identities WHERE name=$n", ("n", lead)).FirstOrDefault();
        }
        try
        {
            if (who?["state"]?.ToString() != "running") { await ids.Start(lead, text); return; } // stopped or queued: text is its first prompt
            if (who["pid"] is null) return; // mid-Phoenix: its successor starts with the goal's status
            await sessions.Input(lead, text);
            await Task.Delay(300); // text and Enter in one write read as a paste
            await sessions.Input(lead, "\r");
        }
        catch (ArgumentException e) { Log.Warn($"goal {name}: could not wake {lead}: {e.Message}"); } // just ended, or forgotten
    }

    async Task<string> Measure(string goal, int n)
    {
        var one = measuring.GetOrAdd(goal, _ => new SemaphoreSlim(1, 1));
        await one.WaitAsync();
        try
        {
            JsonObject g;
            using (var db = store.Open()) g = Need(db, goal);
            var line = ParseLine(Str(g, "success")!);
            var values = new List<double>();
            string? error = null;
            for (var i = 0; i < Long(g, "samples") && error is null; i++)
            {
                var (value, err) = await RunOnce(Str(g, "measure_cmd")!, Str(g, "measure_folder")!, line.Op == "pass");
                if (value is { } v) values.Add(v); else error = err;
            }
            using (var db = store.Open())
            {
                double? value = error is null ? values.Order().ElementAt(values.Count / 2) : null;
                var prior = db.Rows("SELECT value FROM experiments WHERE goal=$g AND value IS NOT NULL", ("g", goal)).Select(r => (double)r["value"]!).ToList();
                var verdict = value is not { } v ? "error: " + error
                    : line.Met(v) ? "met"
                    : prior.Count > 0 && (line.Op[0] == '<' ? v < prior.Min() : line.Op[0] == '>' && v > prior.Max()) ? "improved" : "no gain";
                db.Exec("UPDATE experiments SET measured_ts=$ts, value=$v, verdict=$d WHERE goal=$g AND n=$n",
                    ("ts", db.NowIso()), ("v", value), ("d", verdict), ("g", goal), ("n", n));
                var e = db.Rows("SELECT * FROM experiments WHERE goal=$g AND n=$n", ("g", goal), ("n", n))[0];
                db.Reply(Long(g, "thread_id"), Author, BoardDb.Agent,
                    $"Experiment #{n} by {e["owner"]} ({e["change"]}): **{value?.ToString(CultureInfo.InvariantCulture) ?? "no value"}**, {verdict}.");
                if (verdict == "met") End(db, Need(db, goal), "succeeded", $"experiment #{n} measured {value} ({g["success"]})");
                else _ = Later(() => Wake(goal));
                return e.ToJsonString(Wire.Indented);
            }
        }
        finally { one.Release(); }
    }

    /// <summary>measure_cmd through cmd in the goal's folder: the last number on stdout (a non-zero exit is an error), or for
    /// a pass line, exit 0 as 1 and anything else as 0.</summary>
    async Task<(double?, string?)> RunOnce(string cmd, string folder, bool pass)
    {
        var psi = new ProcessStartInfo("cmd.exe", $"/d /s /c \"{cmd}\"")
        { WorkingDirectory = folder, UseShellExecute = false, CreateNoWindow = true, RedirectStandardOutput = true, RedirectStandardError = true };
        using var p = Process.Start(psi)!;
        var stdout = p.StandardOutput.ReadToEndAsync();
        _ = p.StandardError.ReadToEndAsync();
        using var timeout = new CancellationTokenSource(MeasureTimeout);
        try { await p.WaitForExitAsync(timeout.Token); }
        catch (OperationCanceledException) { p.Kill(true); return (null, $"timed out after {MeasureTimeout.TotalSeconds:0} s"); }
        if (pass) return (p.ExitCode == 0 ? 1 : 0, null);
        if (p.ExitCode != 0) return (null, $"exit code {p.ExitCode}");
        return NumberRe().Matches(await stdout) is { Count: > 0 } m ? (double.Parse(m[^1].Value, CultureInfo.InvariantCulture), null) : (null, "no number on stdout");
    }

    /// <summary>Marks a goal ended, posts it on the goal thread, forgets its members and stops its lead.</summary>
    void End(BoardDb db, JsonObject g, string state, string note)
    {
        var name = Str(g, "name")!;
        if (db.Exec("UPDATE goals SET state=$s, updated_ts=$ts WHERE name=$n AND state IN ('draft','running')", ("s", state), ("ts", db.NowIso()), ("n", name)) == 0) return;
        db.Reply(Long(g, "thread_id"), Author, BoardDb.Agent, $"**Goal {state}**: {note}.\n\n{Summary(db, name)}");
        var members = db.Rows("SELECT identity FROM goal_members WHERE goal=$g", ("g", name)).Select(r => Str(r, "identity")!).ToList();
        db.Exec("DELETE FROM goal_members WHERE goal=$g", ("g", name));
        _ = Later(async () =>
        {
            foreach (var m in members) await ids.Forget(m);
            await ids.Stop(Str(g, "lead")!);
        });
    }

    /// <summary>For a Phoenix successor's first prompt: the status of the goal its identity leads or works on.</summary>
    string? Context(string identity)
    {
        using var db = store.Open();
        return db.Scalar("SELECT name FROM goals WHERE lead=$i UNION SELECT goal FROM goal_members WHERE identity=$i LIMIT 1", ("i", identity)) is string goal
            ? "Your goal's status now, from the core:\n" + Summary(db, goal) : null;
    }

    /// <summary>What an agent needs about the goal: hypothesis, success line, budget, the last 5 experiments and the metric trend.</summary>
    static string Summary(BoardDb db, string name)
    {
        var g = Need(db, name);
        var members = db.Rows("SELECT identity FROM goal_members WHERE goal=$g", ("g", name)).Select(r => Str(r, "identity")).ToList();
        var last = db.Rows("SELECT * FROM (SELECT * FROM experiments WHERE goal=$g ORDER BY n DESC LIMIT 5) ORDER BY n", ("g", name));
        var trend = db.Rows("SELECT value FROM experiments WHERE goal=$g AND value IS NOT NULL ORDER BY n", ("g", name)).Select(r => r["value"]!.ToString());
        return $"""
            Goal {name} ({g["state"]}): {g["objective"]}
            Hypothesis: {g["hypothesis"] ?? "(none yet)"}
            Success: {g["success"] ?? "(none yet)"}, measured by `{g["measure_cmd"]}` in {g["measure_folder"]} ({g["samples"]} run(s), median)
            Budget: {members.Count} of {g["max_members"]} members ({string.Join(", ", members)}), {g["max_hours"]} h; wakes every {g["cadence_minutes"]} min. Thread #{g["thread_id"]}.
            Last experiments: {(last.Count == 0 ? "none" : string.Join("; ", last.Select(e => $"#{e["n"]} by {e["owner"]}, {e["change"]}: {e["value"]?.ToString() ?? "unmeasured"} ({e["verdict"] ?? "open"})")))}
            Metric trend: {(trend.Any() ? string.Join(" -> ", trend) : "no measurements yet")}
            """;
    }

    sealed record Line(string Op, double Target)
    {
        public bool Met(double v) => Op switch { "<" => v < Target, "<=" => v <= Target, ">" => v > Target, ">=" => v >= Target, _ => v == Target };
    }

    static Line ParseLine(string s)
    {
        if (s.Trim().Equals("pass", StringComparison.OrdinalIgnoreCase)) return new("pass", 1);
        var m = LineRe().Match(s);
        return m.Success ? new(m.Groups[1].Value, double.Parse(m.Groups[2].Value, CultureInfo.InvariantCulture))
            : throw new ArgumentException("success is 'value < N', 'value <= N', 'value > N', 'value >= N', 'value == N' or 'pass'");
    }

    /// <summary>The caller must be the goal's lead, or John.</summary>
    static JsonObject Lead(Caller c, JsonObject g) =>
        John(c) || string.Equals(c.Identity, Str(g, "lead"), StringComparison.OrdinalIgnoreCase) ? g : throw new ArgumentException($"only {g["lead"]} or John may do that");

    /// <summary>The caller must be the goal's lead or one of its members, or John; returns the owner name.</summary>
    static string Member(BoardDb db, Caller c, JsonObject g)
    {
        if (John(c)) return BoardDb.John;
        var name = Str(g, "name");
        return c.Identity is { } id && (string.Equals(id, Str(g, "lead"), StringComparison.OrdinalIgnoreCase)
            || db.Scalar("SELECT 1 FROM goal_members WHERE identity=$i AND goal=$g", ("i", id), ("g", name)) is not null)
            ? id : throw new ArgumentException($"only goal {name}'s lead and members may run its experiments");
    }

    static JsonObject Running(BoardDb db, string name) =>
        Need(db, name) is var g && Str(g, "state") == "running" ? g : throw new ArgumentException($"goal {name} is {g["state"]}, not running");

    static Task Later(Func<Task> work) => Task.Run(async () =>
    {
        await Task.Delay(Settle); // the tool's answer reaches the agent first
        try { await work(); }
        catch (Exception e) { Log.Warn($"goals: {e.Message}"); }
    });

    static JsonObject? Get(BoardDb db, string name) => db.Rows("SELECT * FROM goals WHERE name=$n", ("n", name)) is [var g] ? g : null;
    static JsonObject Need(BoardDb db, string name) => Get(db, name) ?? throw new ArgumentException($"no such goal: {name}");
    static string? Str(JsonObject o, string k) => o[k]?.ToString();
    static long Long(JsonObject o, string k) => (long)o[k]!;
    static DateTimeOffset Time(string iso) => DateTimeOffset.Parse(iso, CultureInfo.InvariantCulture);
    static Task<string> Ok(JsonObject o) => Task.FromResult(o.ToJsonString(Wire.Indented));

    [GeneratedRegex(@"^[A-Za-z][A-Za-z0-9-]{0,31}$")] private static partial Regex NameRe();
    [GeneratedRegex(@"^\s*value\s*(<=|>=|==|<|>)\s*(-?\d+(?:\.\d+)?)\s*$", RegexOptions.IgnoreCase)] private static partial Regex LineRe();
    [GeneratedRegex(@"-?\d+(?:\.\d+)?")] private static partial Regex NumberRe();
}
