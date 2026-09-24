using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO.Pipes;
using System.Text.Json;

namespace AgentDesk.Contracts;

/// <summary>
/// A connection to AgentDesk.Core, shared by agentdesk.exe and AgentDesk's window. Starts the core if it isn't running.
/// Replies arrive tagged with their request id, in any order; a reply with id 0 is an event the core pushed.
/// </summary>
public sealed class CoreConnection : IDisposable
{
    static readonly JsonElement NoArgs = JsonDocument.Parse("{}").RootElement;
    readonly NamedPipeClientStream pipe;
    readonly StreamWriter writer;
    readonly SemaphoreSlim gate = new(1, 1);
    readonly ConcurrentDictionary<int, TaskCompletionSource<string>> waiting = new();
    readonly Caller caller;
    int nextId;

    /// <summary>An event the core pushed, such as {"event":"board.changed"}. Raised on a pool thread.</summary>
    public event Action<string>? Pushed;

    CoreConnection(NamedPipeClientStream pipe, Caller caller)
    {
        (this.pipe, this.caller, writer) = (pipe, caller, new StreamWriter(pipe));
        var reader = new StreamReader(pipe);
        _ = Task.Run(async () =>
        {
            try
            {
                while (await Wire.Read(reader, WireJson.Default.Response, default) is { } r)
                    if (r.Id == 0) Pushed?.Invoke(r.Text);
                    else if (waiting.TryRemove(r.Id, out var t)) t.SetResult(r.Text);
            }
            catch (Exception e) when (e is IOException or ObjectDisposedException) { }
            foreach (var t in waiting.Values) t.TrySetException(new IOException("AgentDesk core went away"));
        });
    }

    public static async Task<CoreConnection> Connect(Caller caller)
    {
        var pipe = new NamedPipeClientStream(".", PipeNames.Board, PipeDirection.InOut, PipeOptions.Asynchronous);
        try { await pipe.ConnectAsync(500); }
        catch (TimeoutException)
        {
            Process.Start(new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "AgentDesk.Core.exe")) { UseShellExecute = false });
            await pipe.ConnectAsync(15_000); // the core keeps itself to one instance
        }
        return new CoreConnection(pipe, caller);
    }

    public async Task<string> Call(string tool, JsonElement? args = null, CancellationToken ct = default)
    {
        var id = Interlocked.Increment(ref nextId);
        var reply = waiting[id] = new(TaskCreationOptions.RunContinuationsAsynchronously);
        await Wire.Write(writer, new Request(id, tool, args ?? NoArgs, caller), WireJson.Default.Request, gate);
        return await reply.Task.WaitAsync(ct);
    }

    public void Dispose() => pipe.Dispose();
}
