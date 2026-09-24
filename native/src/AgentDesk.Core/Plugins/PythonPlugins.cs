using System.Collections.Concurrent;
using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Core.Board;

namespace AgentDesk.Core.Plugins;

/// <summary>
/// Runs `python -m agentdesk.plugin` (the repo's venv) and calls it with newline-delimited JSON-RPC
/// on stdio. Started on first use, exits after a few idle minutes so it costs no RAM when unused,
/// and restarted on the next call. Its stderr goes to the core log.
/// </summary>
public sealed class PythonPlugins(string pythonRepo) : IPythonPlugins
{
    static readonly TimeSpan IdleExit = TimeSpan.FromMinutes(5);
    readonly Lock gate = new();
    readonly ConcurrentDictionary<int, TaskCompletionSource<JsonElement>> pending = new();
    Process? proc;
    int nextId;
    Timer? idle;

    public Task<JsonElement> Call(string method, JsonObject args)
    {
        var id = Interlocked.Increment(ref nextId);
        var done = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        pending[id] = done;
        var line = new JsonObject { ["jsonrpc"] = "2.0", ["id"] = id, ["method"] = method, ["params"] = args.DeepClone() }.ToJsonString();
        lock (gate)
        {
            if (proc is not { HasExited: false }) Start();
            idle!.Change(IdleExit, Timeout.InfiniteTimeSpan);
            proc!.StandardInput.WriteLine(line);
            proc.StandardInput.Flush();
        }
        return done.Task;
    }

    void Start()
    {
        var p = Process.Start(new ProcessStartInfo(Path.Combine(pythonRepo, ".venv", "Scripts", "python.exe"), "-m agentdesk.plugin")
        {
            WorkingDirectory = pythonRepo, UseShellExecute = false, CreateNoWindow = true,
            RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true,
        })!;
        p.ErrorDataReceived += (_, e) => { if (e.Data is { } l) Log.Info($"python: {l}"); };
        p.OutputDataReceived += (_, e) => { if (e.Data is { } l) Complete(l); };
        p.Exited += (_, _) => FailAll("python plugin host exited");
        p.EnableRaisingEvents = true;
        p.BeginErrorReadLine();
        p.BeginOutputReadLine();
        idle ??= new Timer(_ => { lock (gate) if (pending.IsEmpty && proc is { HasExited: false }) proc.Kill(); });
        proc = p;
        Log.Info($"started python plugin host (pid {p.Id})");
    }

    void Complete(string line)
    {
        using var doc = JsonDocument.Parse(line);
        var r = doc.RootElement;
        if (!r.TryGetProperty("id", out var idEl) || idEl.ValueKind != JsonValueKind.Number || !pending.TryRemove(idEl.GetInt32(), out var done)) return;
        if (r.TryGetProperty("error", out var err)) done.SetException(new PythonPluginException(err.GetProperty("message").GetString()!));
        else done.SetResult(r.GetProperty("result").Clone());
    }

    void FailAll(string why)
    {
        foreach (var id in pending.Keys)
            if (pending.TryRemove(id, out var t)) t.TrySetException(new IOException(why));
    }
}


