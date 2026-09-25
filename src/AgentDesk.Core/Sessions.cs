using System.Text;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Host;

namespace AgentDesk.Core;

/// <summary>
/// Named Claude Code sessions the core hosts on pseudoconsoles, with no window: John attaches from any terminal
/// (agentdesk attach), like tmux. Each keeps its last 256 KB of output, replayed to whoever attaches. In memory:
/// the sessions end with the core.
/// </summary>
public sealed class Sessions
{
    const int Keep = 256 * 1024;
    readonly Dictionary<string, Session> all = new(StringComparer.OrdinalIgnoreCase);
    readonly Lock gate = new();

    sealed class Session(string name, string folder, string command, Pty pty)
    {
        public readonly string Name = name, Folder = folder, Command = command;
        public readonly Pty Pty = pty;
        public readonly DateTimeOffset Started = DateTimeOffset.UtcNow;
        public readonly byte[] Ring = new byte[Keep];
        public long Written; // total bytes ever; the ring holds the last min(Written, Keep)
        public readonly List<Func<string, Task>> Viewers = [];
        public readonly SemaphoreSlim Out = new(1, 1); // one chunk at a time, so every viewer sees output in order
        public readonly TaskCompletionSource Gone = new(TaskCreationOptions.RunContinuationsAsynchronously); // its name is free again
        public bool Restarting; // viewers are told session.restarted, not session.exited, and reattach to the successor
    }

    /// <summary>Raised with a session's name and pid once its process has ended, however it ended.</summary>
    public event Action<string, int>? Ended;

    public Task<string> Start(string name, string folder, string? command) =>
        Ok(new JsonObject { ["name"] = name, ["pid"] = Launch(name, folder, command, new Dictionary<string, string?>()) });

    /// <summary>Starts a session with <paramref name="extra"/> laid over its environment, and returns its pid.</summary>
    public int Launch(string name, string folder, string? command, IReadOnlyDictionary<string, string?> extra)
    {
        if (string.IsNullOrWhiteSpace(name)) throw new ArgumentException("name is required");
        if (!Directory.Exists(folder)) throw new ArgumentException($"no such folder: {folder}");
        command = string.IsNullOrWhiteSpace(command) ? "claude" : command;
        lock (gate)
        {
            if (all.ContainsKey(name)) throw new ArgumentException($"session already running: {name}");
            // The core may have been started from inside a Claude Code session; its session's identity is not this one's.
            var env = Environment.GetEnvironmentVariables().Keys.Cast<string>()
                .Where(k => k.StartsWith("CLAUDE", StringComparison.OrdinalIgnoreCase) || k is "AGENTDESK_SESSION" or "AGENTDESK_AUTHOR")
                .ToDictionary(k => k, string? (_) => null);
            env["AGENTDESK_HEADLESS"] = name;
            foreach (var (k, v) in extra) env[k] = v;
            var s = all[name] = new Session(name, folder, command, Pty.Start(command, folder, env, 120, 30));
            _ = Task.Run(() => Pump(s)); // reads block: never on this thread
            Log.Info($"session {name} started: {command} in {folder} (pid {s.Pty.Pid})");
            return s.Pty.Pid;
        }
    }

    public Task<string> List()
    {
        lock (gate)
            return Ok(new JsonObject
            {
                ["sessions"] = new JsonArray([.. all.Values.Select(s => (JsonNode)new JsonObject
                {
                    ["name"] = s.Name, ["folder"] = s.Folder, ["command"] = s.Command, ["pid"] = s.Pty.Pid,
                    ["started"] = s.Started.ToString("yyyy-MM-ddTHH:mm:ssZ"), ["viewers"] = s.Viewers.Count,
                })]),
            });
    }

    /// <summary>Ends it and waits until its name is free; <paramref name="restart"/> tells its viewers a successor of the same name follows.</summary>
    public async Task<string> Stop(string name, bool restart = false)
    {
        var s = Find(name);
        s.Restarting = restart;
        s.Pty.Kill();
        await s.Gone.Task.WaitAsync(TimeSpan.FromSeconds(10));
        return await Ok(new JsonObject { ["stopped"] = name });
    }

    /// <summary>Subscribes this connection: replays the ring, then streams output as {"event":"session.output"} pushes,
    /// then nudges the size so the program redraws for its new viewer.</summary>
    public async Task<string> Attach(string name, int cols, int rows, Func<string, Task> push, CancellationToken gone)
    {
        var s = Find(name);
        await s.Out.WaitAsync(gone);
        try
        {
            byte[] replay;
            lock (s) replay = Tail(s);
            if (replay.Length > 0) await push(Output(s.Name, replay, replay.Length));
            lock (s) s.Viewers.Add(push);
        }
        finally { s.Out.Release(); }
        gone.Register(() => { lock (s) s.Viewers.Remove(push); });
        s.Pty.Resize(cols + 1, rows);
        await Task.Delay(100); // two sizes in one breath can read as no change
        s.Pty.Resize(cols, rows);
        return await Ok(new JsonObject { ["attached"] = name, ["replayed"] = s.Written > 0 });
    }

    public async Task<string> Input(string name, string data)
    {
        var s = Find(name);
        var bytes = Encoding.UTF8.GetBytes(data);
        await s.Pty.Input.WriteAsync(bytes);
        await s.Pty.Input.FlushAsync();
        return await Ok(new JsonObject { ["ok"] = true });
    }

    public Task<string> Resize(string name, int cols, int rows)
    {
        Find(name).Pty.Resize(cols, rows);
        return Ok(new JsonObject { ["ok"] = true });
    }

    Session Find(string name)
    {
        lock (gate) return all.TryGetValue(name, out var s) ? s : throw new ArgumentException($"no such session: {name}");
    }

    async Task Pump(Session s)
    {
        var buf = new byte[16 * 1024];
        try
        {
            int n;
            while ((n = await s.Pty.Output.ReadAsync(buf)) > 0)
            {
                await s.Out.WaitAsync();
                try
                {
                    Func<string, Task>[] now;
                    lock (s)
                    {
                        for (var i = 0; i < n; i++) s.Ring[(s.Written + i) % Keep] = buf[i];
                        s.Written += n;
                        now = [.. s.Viewers];
                    }
                    var e = Output(s.Name, buf, n);
                    await Task.WhenAll(now.Select(v => Send(v, e)));
                }
                finally { s.Out.Release(); }
            }
        }
        catch (Exception e) when (e is IOException or ObjectDisposedException) { } // the pseudoconsole closed
        await s.Pty.Exited;
        lock (gate) all.Remove(s.Name);
        Func<string, Task>[] last;
        lock (s) last = [.. s.Viewers];
        var ended = new JsonObject { ["event"] = s.Restarting ? "session.restarted" : "session.exited", ["name"] = s.Name }.ToJsonString();
        await Task.WhenAll(last.Select(v => Send(v, ended)));
        s.Pty.Dispose();
        Log.Info($"session {s.Name} ended");
        try { Ended?.Invoke(s.Name, s.Pty.Pid); }
        finally { s.Gone.TrySetResult(); }
    }

    static byte[] Tail(Session s)
    {
        var n = (int)Math.Min(s.Written, Keep);
        var tail = new byte[n];
        for (var i = 0; i < n; i++) tail[i] = s.Ring[(s.Written - n + i) % Keep];
        return tail;
    }

    static string Output(string name, byte[] data, int n) =>
        new JsonObject { ["event"] = "session.output", ["name"] = name, ["data"] = Convert.ToBase64String(data, 0, n) }.ToJsonString();

    static async Task Send(Func<string, Task> push, string e)
    {
        try { await push(e); }
        catch (Exception x) when (x is IOException or ObjectDisposedException) { } // that viewer just left
    }

    static Task<string> Ok(JsonObject o) => Task.FromResult(o.ToJsonString(Wire.Indented));
}
