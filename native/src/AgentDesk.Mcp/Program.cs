// The agentdesk MCP server every Claude Code session starts over stdio. It holds no logic: each
// tool call is relayed over the per-user pipe to AgentDesk.Core, which owns the board.
using System.Collections.Concurrent;
using System.IO.Pipes;
using System.Text.Json;
using AgentDesk.Contracts;
using ModelContextProtocol.Protocol;
using ModelContextProtocol.Server;

var pipe = new NamedPipeClientStream(".", PipeNames.Board, PipeDirection.InOut, PipeOptions.Asynchronous);
try { await pipe.ConnectAsync(1_000); }
catch (TimeoutException)
{
    // The core isn't running yet: start it (it keeps itself to one instance) and wait for it.
    System.Diagnostics.Process.Start(Path.Combine(AppContext.BaseDirectory, "AgentDesk.Core.exe"));
    await pipe.ConnectAsync(15_000);
}

var caller = new Caller(
    Environment.GetEnvironmentVariable("CLAUDE_CODE_SESSION_ID") ?? Environment.GetEnvironmentVariable("AGENTDESK_SESSION"),
    Environment.GetEnvironmentVariable("AGENTDESK_AUTHOR"),
    Environment.CurrentDirectory,
    Environment.GetEnvironmentVariable("CLAUDECODE") is null ? null : "claude-code",
    Environment.ProcessId);
var reader = new StreamReader(pipe);
var writer = new StreamWriter(pipe);
var gate = new SemaphoreSlim(1, 1);
var waiting = new ConcurrentDictionary<int, TaskCompletionSource<string>>();
var nextId = 0;

_ = Task.Run(async () =>   // replies come back tagged with their request id
{
    while (await Wire.Read(reader, WireJson.Default.Response, default) is { } r)
        if (waiting.TryRemove(r.Id, out var t)) t.SetResult(r.Text);
    foreach (var t in waiting.Values) t.TrySetException(new IOException("AgentDesk core went away"));
});

var tools = Tools.All.Select(t => new Tool { Name = t.Name, Description = t.Description,
                                              InputSchema = JsonDocument.Parse(t.InputSchema).RootElement }).ToList();
var options = new McpServerOptions
{
    ServerInfo = new() { Name = "agentdesk", Version = "2.0" },
    ServerInstructions = Tools.Instructions,
    Handlers = new()
    {
        ListToolsHandler = (_, _) => ValueTask.FromResult(new ListToolsResult { Tools = tools }),
        CallToolHandler = async (ctx, ct) =>
        {
            var id = Interlocked.Increment(ref nextId);
            var reply = waiting[id] = new(TaskCreationOptions.RunContinuationsAsynchronously);
            var args = ctx.Params?.Arguments is { } a ? JsonSerializer.SerializeToElement(a, ArgsJson.Default.IDictionaryStringJsonElement) : default;
            await Wire.Write(writer, new Request(id, ctx.Params!.Name, args, caller), WireJson.Default.Request, gate);
            return new CallToolResult { Content = [new TextContentBlock { Text = await reply.Task.WaitAsync(ct) }] };
        },
    },
};
await using var server = McpServer.Create(new StdioServerTransport("agentdesk"), options);
await server.RunAsync();

[System.Text.Json.Serialization.JsonSerializable(typeof(IDictionary<string, JsonElement>))]
sealed partial class ArgsJson : System.Text.Json.Serialization.JsonSerializerContext;
