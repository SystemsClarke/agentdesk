using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO.Pipes;
using System.Text.Json;

namespace AgentDesk.Contracts;

/// <summary>
/// A connection to AgentDesk.Core, shared by agentdesk.exe and AgentDesk's window. Starts the core if it isn't running,
/// and reconnects (starting it again) when the core restarts, as it does on an update, so an agent's MCP server outlives it.
/// Replies arrive tagged with their request id, in any order; a reply with id 0 is an event the core pushed.
/// </summary>
public sealed class CoreConnection : IDisposable
{
    static readonly JsonElement NoArgs = JsonDocument.Parse("{}").RootElement;
    readonly SemaphoreSlim gate = new(1, 1), relink = new(1, 1);
    readonly ConcurrentDictionary<int, Pending> waiting = new();
    readonly Caller caller;
    Link? link;
    int nextId;

    /// <summary>A call waiting for its reply, and the link it was written to: when a link dies only its own calls fail, never one
    /// already resent on the link that replaced it. Link is null while the call is between links.</summary>
    sealed class Pending
    {
        public readonly TaskCompletionSource<string> Reply = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public volatile Link? Link;
    }

    sealed class Link(NamedPipeClientStream pipe)
    {
        public readonly NamedPipeClientStream Pipe = pipe;
        public readonly StreamWriter Writer = new(pipe);
        public volatile bool Dead;
    }

    /// <summary>An event the core pushed, such as {"event":"board.changed"}. Raised on a pool thread.</summary>
    public event Action<string>? Pushed;

    /// <summary>Raised on a pool thread after a core restart, once connected again. Subscribing also keeps the
    /// connection up while idle (the window needs that for its pushes); otherwise it reconnects on the next call.</summary>
    public event Action? Reconnected;

    CoreConnection(Caller caller) => this.caller = caller;

    public static async Task<CoreConnection> Connect(Caller caller)
    {
        var core = new CoreConnection(caller);
        await core.Live();
        return core;
    }

    public async Task<string> Call(string tool, JsonElement? args = null, CancellationToken ct = default)
    {
        var id = Interlocked.Increment(ref nextId);
        var pending = waiting[id] = new();
        var request = new Request(id, tool, args ?? NoArgs, caller);
        try
        {
            var l = pending.Link = await Live();
            try
            {
                if (l.Dead) throw new IOException("the link died before this call was written");
                await Wire.Write(l.Writer, request, WireJson.Default.Request, gate);
            }
            catch (IOException) // never sent: safe to resend, on the new link, which must be the one this call is waiting on
            {
                pending.Link = null; // untag first: the dying link's reader must not fail a call that is about to move
                l.Dead = true;
                l = pending.Link = await Live();
                await Wire.Write(l.Writer, request, WireJson.Default.Request, gate);
            }
            return await pending.Reply.Task.WaitAsync(ct);
        }
        finally { waiting.TryRemove(id, out _); } // a failed Live(), a failed write or a cancel must not leave it behind
    }

    async Task<Link> Live()
    {
        await relink.WaitAsync();
        try
        {
            if (link is { Dead: false }) return link;
            var again = link is not null;
            link = await Open();
            if (again) _ = Task.Run(() => Reconnected?.Invoke());
            return link;
        }
        finally { relink.Release(); }
    }

    async Task<Link> Open()
    {
        var pipe = new NamedPipeClientStream(".", PipeNames.Board, PipeDirection.InOut, PipeOptions.Asynchronous);
        try { await pipe.ConnectAsync(500); }
        catch (TimeoutException)
        {
            Process.Start(new ProcessStartInfo(Path.Combine(AppContext.BaseDirectory, "AgentDesk.Core.exe"), "--background") { UseShellExecute = false });
            await pipe.ConnectAsync(15_000); // the core keeps itself to one instance
        }
        var l = new Link(pipe);
        _ = Task.Run(async () =>
        {
            try
            {
                var reader = new StreamReader(pipe);
                while (await Wire.Read(reader, WireJson.Default.Response, default) is { } r)
                    if (r.Id == 0) { try { Pushed?.Invoke(r.Text); } catch { } } // one bad subscriber must not take the link down
                    else if (waiting.TryRemove(r.Id, out var p)) p.Reply.TrySetResult(r.Text);
            }
            catch { } // a closed pipe, or a frame that is not JSON: either way this link is finished, and the code below must run
            l.Dead = true;
            foreach (var (id, p) in waiting) if (p.Link == l && waiting.TryRemove(id, out _)) p.Reply.TrySetException(new IOException("AgentDesk core restarted; try again"));
            if (Reconnected is not null && !disposed)
                for (var wait = 1; !disposed; wait = Math.Min(wait * 2, 30)) // the window: come back as soon as the core does
                    try { await Task.Delay(wait * 1000); await Live(); break; } catch { } // core not back yet
        });
        return l;
    }

    volatile bool disposed;
    public void Dispose() { disposed = true; link?.Pipe.Dispose(); }
}
