// agentdesk.exe: everything a Claude Code session runs, relayed to AgentDesk.Core over the pipe.
//   agentdesk                    MCP server on stdio (how every session reaches the board)
//   agentdesk hook <event>       Claude Code hook: stdin JSON in, hook JSON out (never fails the session)
//   agentdesk wait <thread-id>   block until John replies on the thread, then print his reply
using System.Text.Json;
using AgentDesk.Cli;

var core = await CoreClient.Connect();
switch (args)
{
    case ["hook", var hookEvent]:
        try
        {
            var input = JsonDocument.Parse(await Console.In.ReadToEndAsync() is { Length: > 0 } s ? s : "{}").RootElement;
            Console.Write(await core.Call($"hook:{hookEvent}", input));
        }
        catch (Exception) { } // a hook must never break a session
        break;
    case ["wait", var thread]:
        Console.WriteLine(await core.Call("wait", JsonSerializer.SerializeToElement(int.Parse(thread), CliJson.Default.Int32)));
        break;
    default:
        await Mcp.Serve(core);
        break;
}

namespace AgentDesk.Cli
{
    [System.Text.Json.Serialization.JsonSerializable(typeof(int))]
    [System.Text.Json.Serialization.JsonSerializable(typeof(IDictionary<string, JsonElement>))]
    sealed partial class CliJson : System.Text.Json.Serialization.JsonSerializerContext;
}
