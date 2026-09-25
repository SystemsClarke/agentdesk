using System.Text;
using System.Threading.Channels;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Host;

namespace AgentDesk.Core;

/// <summary>
/// Named Claude Code sessions the core hosts on pseudoconsoles, with no window: John attaches from any terminal
/// (agentdesk attach), like tmux. Each keeps its last 256 KB of output, replayed to whoever attaches. In memory:
/// the sessions end with the core. Each viewer has its own bounded queue, so a stalled one never slows the others or
/// the reader: one that falls <paramref name="viewerQueue"/> chunks behind is dropped with {"event":"session.overflow"},
/// and reattaching gets the ring again.
/// </summary>
public sealed class Sessions(int viewerQueue = 256)
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
        public readonly List<Viewer> Viewers = []; // guarded by lock (this), with Ring and Written, so every viewer sees output in order
        public readonly TaskCompletionSource Gone = new(TaskCreationOptions.RunContinuationsAsynchronously); // its name is free again
        public bool Restarting; // viewers are told session.restarted, not session.exited, and reattach to the successor
    }

    /// <summary>One attached connection: its pushes, in order, from its own queue.</summary>
    sealed class Viewer
    {
        public required Func<string, Task> Push;
        public required Channel<string> Queue;
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
        var v = new Viewer { Push = push, Queue = Channel.CreateBounded<string>(new BoundedChannelOptions(viewerQueue + 1) { SingleWriter = false }) };
        lock (s)
        {
            var replay = Tail(s);
            if (replay.Length > 0) v.Queue.Writer.TryWrite(Output(s.Name, replay, replay.Length));
            s.Viewers.Add(v);
        }
        _ = Drain(v, gone);
        gone.Register(() => { lock (s) s.Viewers.Remove(v); v.Queue.Writer.TryComplete(); });
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
                var e = Output(s.Name, buf, n);
                lock (s)
                {
                    for (var i = 0; i < n; i++) s.Ring[(s.Written + i) % Keep] = buf[i];
                    s.Written += n;
                    foreach (var v in s.Viewers.ToArray()) Enqueue(s, v, e);
                }
            }
        }
        catch (Exception e) when (e is IOException or ObjectDisposedException) { } // the pseudoconsole closed
        await s.Pty.Exited;
        lock (gate) all.Remove(s.Name);
        var ended = new JsonObject { ["event"] = s.Restarting ? "session.restarted" : "session.exited", ["name"] = s.Name }.ToJsonString();
        lock (s)
            foreach (var v in s.Viewers)
            {
                v.Queue.Writer.TryWrite(ended); // the spare slot: a full queue still gets its ending
                v.Queue.Writer.TryComplete();
            }
        s.Pty.Dispose();
        Log.Info($"session {s.Name} ended");
        try { Ended?.Invoke(s.Name, s.Pty.Pid); }
        finally { s.Gone.TrySetResult(); }
    }

    /// <summary>Queues one push for a viewer, under lock (s). A viewer whose queue is full is dropped: it is told
    /// session.overflow after what it already has, in the spare slot, and gets nothing more.</summary>
    void Enqueue(Session s, Viewer v, string e)
    {
        if (v.Queue.Reader.Count < viewerQueue && v.Queue.Writer.TryWrite(e)) return;
        s.Viewers.Remove(v);
        v.Queue.Writer.TryWrite(new JsonObject { ["event"] = "session.overflow", ["name"] = s.Name }.ToJsonString());
        v.Queue.Writer.TryComplete();
        Log.Warn($"session {s.Name}: a viewer fell {viewerQueue} chunks behind and was dropped");
    }

    /// <summary>Sends a viewer its queue, one push at a time, until the queue ends or the connection goes.</summary>
    static async Task Drain(Viewer v, CancellationToken gone)
    {
        try
        {
            await foreach (var e in v.Queue.Reader.ReadAllAsync(gone))
                if (!await Send(v.Push, e)) return;
        }
        catch (OperationCanceledException) { } // the connection closed
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

    static async Task<bool> Send(Func<string, Task> push, string e)
    {
        try { await push(e); return true; }
        catch (Exception x) when (x is IOException or ObjectDisposedException or OperationCanceledException) { return false; } // that viewer just left
    }

    static Task<string> Ok(JsonObject o) => Task.FromResult(o.ToJsonString(Wire.Indented));
}
