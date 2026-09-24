using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO.Pipes;
using System.Text.Json;
using AgentDesk.Contracts;

namespace AgentDesk.Cli;

/// <summary>This session's connection to AgentDesk.Core. Starts the core if it isn't running.</summary>
sealed class CoreClient
{
    readonly StreamWriter writer;
    readonly SemaphoreSlim gate = new(1, 1);
    readonly ConcurrentDictionary<int, TaskCompletionSource<string>> waiting = new();
    readonly Caller caller = new(
        Environment.GetEnvironmentVariable("CLAUDE_CODE_SESSION_ID") ?? Environment.GetEnvironmentVariable("AGENTDESK_SESSION"),
        Environment.GetEnvironmentVariable("AGENTDESK_AUTHOR"),
        Environment.CurrentDirectory,
        Environment.GetEnvironmentVariable("CLAUDECODE") is null ? null : "claude-code",
        Environment.ProcessId);
    int nextId;

    CoreClient(NamedPipeClientStream pipe)
    {
        writer = new StreamWriter(pipe);
        var reader = new StreamReader(pipe);
        _ = Task.Run(async () => // replies arrive tagged with their request id, in any order
        {
            try
            {
                while (await Wire.Read(reader, WireJson.Default.Response, default) is { } r)
                    if (waiting.TryRemove(r.Id, out var t)) t.SetResult(r.Text);
            }
            catch (IOException) { }
            foreach (var t in waiting.Values) t.TrySetException(new IOException("AgentDesk core went away"));
        });
    }

    public static async Task<CoreClient> Connect()
    {
        var pipe = new NamedPipeClientStream(".", PipeNames.Board, PipeDirection.InOut, PipeOptions.Asynchronous);
        try { await pipe.ConnectAsync(500); }
        catch (TimeoutException)
        {
            Process.Start(new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "AgentDesk.Core.exe")) { UseShellExecute = false });
            await pipe.ConnectAsync(15_000); // the core keeps itself to one instance
        }
        return new CoreClient(pipe);
    }

    public async Task<string> Call(string tool, JsonElement args, CancellationToken ct = default)
    {
        var id = Interlocked.Increment(ref nextId);
        var reply = waiting[id] = new(TaskCreationOptions.RunContinuationsAsynchronously);
        await Wire.Write(writer, new Request(id, tool, args, caller), WireJson.Default.Request, gate);
        return await reply.Task.WaitAsync(ct);
    }
}
