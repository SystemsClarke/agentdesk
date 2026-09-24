using System.Text.Json;
using AgentDesk.Contracts;
using ModelContextProtocol.Protocol;
using ModelContextProtocol.Server;

namespace AgentDesk.Cli;

/// <summary>The agentdesk MCP server: lists the board's tools and relays each call to the core.</summary>
static class Mcp
{
    public static async Task Serve(CoreConnection core)
    {
        var tools = Tools.All.Select(t => new Tool
        {
            Name = t.Name, Description = t.Description, InputSchema = JsonDocument.Parse(t.InputSchema).RootElement,
        }).ToList();
        var options = new McpServerOptions
        {
            ServerInfo = new() { Name = "agentdesk", Version = "2.0" },
            ServerInstructions = Tools.Instructions,
            Handlers = new()
            {
                ListToolsHandler = (_, _) => ValueTask.FromResult(new ListToolsResult { Tools = tools }),
                CallToolHandler = async (ctx, ct) =>
                {
                    var args = ctx.Params?.Arguments is { } a ? JsonSerializer.SerializeToElement(a, CliJson.Default.IDictionaryStringJsonElement) : (JsonElement?)null;
                    var text = await core.Call(ctx.Params!.Name, args, ct);
                    return new CallToolResult { Content = [new TextContentBlock { Text = text }] };
                },
            },
        };
        await using var server = McpServer.Create(new StdioServerTransport("agentdesk"), options);
        await server.RunAsync();
    }
}
