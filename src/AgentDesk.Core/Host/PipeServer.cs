using System.IO.Pipes;
using System.Security.AccessControl;
using System.Security.Principal;
using AgentDesk.Contracts;

namespace AgentDesk.Core.Host;

/// <summary>
/// Serves the core on \\.\pipe\agentdesk-&lt;SID&gt;. Only this Windows account may connect; nothing
/// listens on the network. Requests on one connection run concurrently; replies carry their id.
/// </summary>
public static class PipeServer
{
    public static async Task Run(Func<Request, CancellationToken, Task<string>> handle, CancellationToken stop)
    {
        var acl = new PipeSecurity();
        acl.AddAccessRule(new PipeAccessRule(WindowsIdentity.GetCurrent().User!, PipeAccessRights.FullControl, AccessControlType.Allow));
        Log.Info($"listening on {PipeNames.Board}");
        while (!stop.IsCancellationRequested)
        {
            var pipe = NamedPipeServerStreamAcl.Create(PipeNames.Board, PipeDirection.InOut, NamedPipeServerStream.MaxAllowedServerInstances,
                                                       PipeTransmissionMode.Byte, PipeOptions.Asynchronous, 0, 0, acl);
            await pipe.WaitForConnectionAsync(stop);
            _ = Serve(handle, pipe, stop);
        }
    }

    static async Task Serve(Func<Request, CancellationToken, Task<string>> handle, NamedPipeServerStream pipe, CancellationToken stop)
    {
        await using var owned = pipe;
        using var reader = new StreamReader(pipe);
        await using var writer = new StreamWriter(pipe) { AutoFlush = false };
        var gate = new SemaphoreSlim(1, 1);
        try
        {
            while (await Wire.Read(reader, WireJson.Default.Request, stop) is { } req)
                _ = Task.Run(async () =>
                {
                    string text;
                    try { text = await handle(req, stop); }
                    catch (Exception e) { Log.Warn($"{req.Tool} failed: {e}"); text = Tools.Error($"internal error: {e.Message}"); }
                    await Wire.Write(writer, new Response(req.Id, text), WireJson.Default.Response, gate);
                }, stop);
        }
        catch (Exception e) when (e is IOException or OperationCanceledException) { } // the agent session ended
    }
}


