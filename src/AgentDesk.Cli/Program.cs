// agentdesk.exe: everything a Claude Code session runs, relayed to AgentDesk.Core over the pipe.
//   agentdesk                    MCP server on stdio (how every session reaches the board)
//   agentdesk hook <event>       Claude Code hook: stdin JSON in, hook JSON out (never fails the session)
//   agentdesk wait <thread-id>   block until John replies on the thread, then print his reply
//   agentdesk sessions           list the Claude Code sessions the core hosts headless
//   agentdesk start <name> <folder> [command...]   start one (command defaults to claude); stop <name> ends it
//   agentdesk attach <name>      use one from this console, like tmux attach; Ctrl+] detaches
using System.Text.Json;
using AgentDesk.Cli;
using AgentDesk.Contracts;

using var core = await CoreConnection.Connect(new Caller(
    Environment.GetEnvironmentVariable("CLAUDE_CODE_SESSION_ID") ?? Environment.GetEnvironmentVariable("AGENTDESK_SESSION"),
    Environment.GetEnvironmentVariable("AGENTDESK_AUTHOR"),
    Environment.CurrentDirectory,
    Environment.GetEnvironmentVariable("CLAUDECODE") is null ? null : "claude-code",
    Environment.ProcessId));
switch (args)
{
    case ["hook", var hookEvent]:
        try
        {
            var input = JsonDocument.Parse((await Console.In.ReadToEndAsync()).TrimStart('\uFEFF') is { Length: > 0 } s ? s : "{}").RootElement;
            Console.Write(await core.Call($"hook:{hookEvent}", input));
        }
        catch (Exception) { } // a hook must never break a session
        break;
    case ["wait", var thread]:
        Console.WriteLine(await core.Call("wait", JsonSerializer.SerializeToElement(int.Parse(thread), CliJson.Default.Int32)));
        break;
    case ["sessions"]:
        Console.WriteLine(await core.Call("ui:session_list"));
        break;
    case ["start", var name, var folder, .. var command]:
        Console.WriteLine(await core.Call("ui:session_start", Attach.Json(new()
            { ["name"] = name, ["folder"] = Path.GetFullPath(folder), ["command"] = command.Length > 0 ? string.Join(' ', command) : null })));
        break;
    case ["stop", var name]:
        Console.WriteLine(await core.Call("ui:session_stop", Attach.Json(new() { ["name"] = name })));
        break;
    case ["attach", var name]:
        return await Attach.Run(core, name);
    default:
        await Mcp.Serve(core);
        break;
}
return 0;

namespace AgentDesk.Cli
{
    [System.Text.Json.Serialization.JsonSerializable(typeof(int))]
    [System.Text.Json.Serialization.JsonSerializable(typeof(IDictionary<string, JsonElement>))]
    sealed partial class CliJson : System.Text.Json.Serialization.JsonSerializerContext;
}
