using System.IO.Pipes;
using System.Security.AccessControl;
using System.Security.Principal;
using AgentDesk.Contracts;

namespace AgentDesk.Core.Host;

/// <summary>Answers one request. <paramref name="push"/> sends this connection an event (a response with id 0);
/// <paramref name="gone"/> is cancelled when the connection closes.</summary>
public delegate Task<string> Handler(Request request, Func<string, Task> push, CancellationToken gone);

/// <summary>
/// Serves the core on \\.\pipe\agentdesk-&lt;SID&gt;. Only this Windows account may connect; nothing
/// listens on the network. Requests on one connection run concurrently; replies carry their id.
/// </summary>
public static class PipeServer
{
    public static async Task Run(Handler handle, CancellationToken stop)
    {
        var acl = new PipeSecurity();
        acl.AddAccessRule(new PipeAccessRule(WindowsIdentity.GetCurrent().User!, PipeAccessRights.FullControl, AccessControlType.Allow));
        Log.Info($"listening on {PipeNames.Board}");
        while (!stop.IsCancellationRequested)
        {
            var pipe = NamedPipeServerStreamAcl.Create(PipeNames.Board, PipeDirection.InOut, NamedPipeServerStream.MaxAllowedServerInstances,
                                                       PipeTransmissionMode.Byte, PipeOptions.Asynchronous, 0, 0, acl);
            try { await pipe.WaitForConnectionAsync(stop); }
            catch { await pipe.DisposeAsync(); throw; } // else a client can still connect to it and be answered by nobody
            _ = Serve(handle, pipe, stop);
        }
    }

    static async Task Serve(Handler handle, NamedPipeServerStream pipe, CancellationToken stop)
    {
        await using var owned = pipe;
        using var reader = new StreamReader(pipe);
        await using var writer = new StreamWriter(pipe) { AutoFlush = false };
        using var gone = CancellationTokenSource.CreateLinkedTokenSource(stop);
        var gate = new SemaphoreSlim(1, 1);
        Task Send(int id, string text) => Wire.Write(writer, new Response(id, text), WireJson.Default.Response, gate);
        try
        {
            while (await Wire.Read(reader, WireJson.Default.Request, gone.Token) is { } req)
                _ = Task.Run(async () =>
                {
                    string text;
                    try { text = await handle(req, e => Send(0, e), gone.Token); }
                    catch (OperationCanceledException) when (gone.IsCancellationRequested) { return; } // nobody left to answer
                    catch (ArgumentException e) { text = Tools.Error(e.Message); }
                    catch (Exception e) { Log.Warn($"{req.Tool} failed: {e}"); text = Tools.Error($"internal error: {e.Message}"); }
                    await Send(req.Id, text);
                }, gone.Token);
        }
        catch (Exception e) when (e is IOException or OperationCanceledException) { } // the session or window ended
        finally { gone.Cancel(); }
    }
}
