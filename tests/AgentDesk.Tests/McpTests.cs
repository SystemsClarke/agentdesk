using System.Text.Json;
using AgentDesk.Cli;
using AgentDesk.Contracts;
using AgentDesk.Core.Host;
using Xunit;

namespace AgentDesk.Tests;

/// <summary>The hand-written MCP server (Cli/Mcp.cs), against a fake core on a test pipe: what every Claude Code session talks to.</summary>
[Collection("window")] // sets AGENTDESK_PIPE for the process
public class McpTests
{
    static async Task<Dictionary<string, JsonElement>> Talk(List<string> calls, params string[] lines)
    {
        Environment.SetEnvironmentVariable("AGENTDESK_PIPE", $"agentdesk-test-{Guid.NewGuid():N}");
        using var stop = new CancellationTokenSource();
        _ = PipeServer.Run((req, _, _) =>
        {
            lock (calls) calls.Add($"{req.Tool} {req.Args.GetRawText()}");
            return Task.FromResult("""{"echo": true}""");
        }, stop.Token);
        using var core = await CoreConnection.Connect(new Caller(null, "tester", null, "claude-code", Environment.ProcessId));
        var output = new StringWriter();
        await Mcp.Serve(core, new StringReader(string.Join("\n", lines) + "\n"), output);
        stop.Cancel();
        return output.ToString().Split('\n', StringSplitOptions.RemoveEmptyEntries)
            .Select(l => JsonDocument.Parse(l).RootElement.Clone())
            .ToDictionary(r => r.GetProperty("id").ValueKind == JsonValueKind.Null ? "null" : r.GetProperty("id").GetRawText());
    }

    [Fact]
    public async Task It_answers_initialize_ping_and_lists_every_tool_with_its_schema()
    {
        var calls = new List<string>();
        var r = await Talk(calls,
            """{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}""",
            """{"jsonrpc":"2.0","method":"notifications/initialized"}""", // a notification: no answer
            """{"jsonrpc":"2.0","id":2,"method":"ping"}""",
            """{"jsonrpc":"2.0","id":3,"method":"tools/list"}""");
        Assert.Equal(3, r.Count); // the notification got none
        var init = r["1"].GetProperty("result");
        Assert.Equal("2025-03-26", init.GetProperty("protocolVersion").GetString()); // the client's version, which this server speaks
        Assert.Equal("agentdesk", init.GetProperty("serverInfo").GetProperty("name").GetString());
        Assert.Equal(Tools.Instructions, init.GetProperty("instructions").GetString());
        Assert.True(init.GetProperty("capabilities").TryGetProperty("tools", out _));
        Assert.Equal(JsonValueKind.Object, r["2"].GetProperty("result").ValueKind);
        var tools = r["3"].GetProperty("result").GetProperty("tools");
        Assert.Equal(Tools.All.Select(t => t.Name), tools.EnumerateArray().Select(t => t.GetProperty("name").GetString()));
        foreach (var (info, tool) in Tools.All.Zip(tools.EnumerateArray()))
        {
            Assert.Equal(info.Description, tool.GetProperty("description").GetString());
            Assert.Equal(info.InputSchema.Trim(), tool.GetProperty("inputSchema").GetRawText()); // the schema is passed through as written
            Assert.Equal("object", tool.GetProperty("inputSchema").GetProperty("type").GetString());
        }
        Assert.Empty(calls);
    }

    [Fact]
    public async Task A_tool_call_reaches_the_core_with_its_arguments_and_returns_its_text_under_the_callers_id()
    {
        var calls = new List<string>();
        var name = Tools.All.First().Name;
        var r = await Talk(calls,
            """{"jsonrpc":"2.0","id":"abc","method":"tools/call","params":{"name":"NAME","arguments":{"x":1,"y":"two"}}}""".Replace("NAME", name),
            """{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"NAME"}}""".Replace("NAME", name));
        Assert.Equal("""{"echo": true}""", r["\"abc\""].GetProperty("result").GetProperty("content")[0].GetProperty("text").GetString()); // a string id is echoed as a string
        Assert.Equal("text", r["\"abc\""].GetProperty("result").GetProperty("content")[0].GetProperty("type").GetString());
        Assert.False(r["\"abc\""].GetProperty("result").TryGetProperty("isError", out _));
        Assert.Contains(r.Keys, k => k == "7");
        lock (calls)
        {
            Assert.Contains(calls, c => c == name + " {\"x\":1,\"y\":\"two\"}");
            Assert.Contains(calls, c => c == name + " {}");
        }
    }

    [Fact]
    public async Task Bad_input_gets_json_rpc_errors_and_never_reaches_the_core()
    {
        var calls = new List<string>();
        var r = await Talk(calls,
            """{"jsonrpc":"2.0","id":4,"method":"nope/nothing"}""",
            """{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"no_such_tool"}}""",
            """{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{}}""",
            """{not json""");
        Assert.Equal(-32601, r["4"].GetProperty("error").GetProperty("code").GetInt32()); // method not found
        Assert.Equal(-32602, r["5"].GetProperty("error").GetProperty("code").GetInt32());
        Assert.Equal(-32602, r["6"].GetProperty("error").GetProperty("code").GetInt32());
        Assert.Equal(-32700, r["null"].GetProperty("error").GetProperty("code").GetInt32()); // parse error, id null
        Assert.Empty(calls); // none of those reached the core
    }
}
