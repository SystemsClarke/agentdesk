// agentdesk.exe: everything a Claude Code session runs, relayed to AgentDesk.Core over the pipe.
//   agentdesk                    MCP server on stdio (how every session reaches the board)
//   agentdesk hook <event>       Claude Code hook: stdin JSON in, hook JSON out (never fails the session)
//   agentdesk wait <thread-id>   block until John replies on the thread, then print his reply
//   agentdesk sessions           list the Claude Code sessions the core hosts headless
//   agentdesk start <name> <folder> [command...]   start one (command defaults to claude); stop <name> ends it
//   agentdesk attach <name>      use one from this console, like tmux attach; Ctrl+] detaches
//   agentdesk new <name> <folder> [--charter text] [--host wsl:<distro>] [--autostart]   an identity: a session that outlives the core
//   agentdesk start|stop|forget <name>, agentdesk list   run, stop, delete and list identities
//   agentdesk adoptable, agentdesk adopt <session-id> <name>   make a recent Claude Code conversation an identity
//   agentdesk update [--apply]   check for (and download) a newer release; --apply restarts the core into it and waits for it
using System.Text.Json;
using AgentDesk.Cli;
using AgentDesk.Contracts;

using var core = await CoreConnection.Connect(new Caller(
    Environment.GetEnvironmentVariable("CLAUDE_CODE_SESSION_ID") ?? Environment.GetEnvironmentVariable("AGENTDESK_SESSION"),
    Environment.GetEnvironmentVariable("AGENTDESK_AUTHOR"),
    Environment.CurrentDirectory,
    Environment.GetEnvironmentVariable("CLAUDECODE") is null ? null : "claude-code",
    Environment.ProcessId,
    Environment.GetEnvironmentVariable("AGENTDESK_IDENTITY")));
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
    case [("stop" or "forget" or "start") and var verb, var name]:
        Console.WriteLine(await core.Call($"ui:identity_{verb}", Attach.Json(new() { ["name"] = name }))); // stop also ends a plain session
        break;
    case ["new", var name, var folder, .. var opts]:
        var host = Opt(opts, "--host");
        Console.WriteLine(await core.Call("ui:identity_create", Attach.Json(new()
        {
            ["name"] = name, ["folder"] = host is null or "windows" ? Path.GetFullPath(folder) : folder, ["charter"] = Opt(opts, "--charter"),
            ["host"] = host, ["autostart"] = opts.Contains("--autostart"),
        })));
        break;
    case ["list"]:
        Console.WriteLine(await core.Call("ui:identity_list"));
        break;
    case ["adoptable"]:
        Console.WriteLine(await core.Call("ui:adoptable"));
        break;
    case ["adopt", var id, var name]:
        Console.WriteLine(await core.Call("ui:adopt", Attach.Json(new() { ["session_id"] = id, ["name"] = name })));
        break;
    case ["update", .. var opts]:
        if (opts is not ([] or ["--apply"])) { Console.Error.WriteLine("usage: agentdesk update [--apply]"); return 2; }
        var reply = await core.Call("ui:update", Attach.Json(new() { ["apply"] = opts.Length > 0 }));
        Console.WriteLine(reply);
        if (!reply.Contains("\"restarting\": true")) break;
        var pipe = $@"\\.\pipe\{PipeNames.Board}";
        for (var t = 0; t < 120 && File.Exists(pipe); t++) await Task.Delay(500); // the old core going (a minute at most)
        for (var t = 0; t < 240 && !File.Exists(pipe); t++) await Task.Delay(500); // the new one listening (two minutes at most)
        Console.WriteLine(File.Exists(pipe) ? await core.Call("ui:update") : "The core has not come back after two minutes; see core.log.");
        break;
    case ["attach", var name]:
        return await Attach.Run(core, name);
    default:
        await Mcp.Serve(core);
        break;
}
return 0;

static string? Opt(string[] opts, string flag) => Array.IndexOf(opts, flag) is var i and >= 0 && i + 1 < opts.Length ? opts[i + 1] : null;

namespace AgentDesk.Cli
{
    [System.Text.Json.Serialization.JsonSerializable(typeof(int))]
    [System.Text.Json.Serialization.JsonSerializable(typeof(IDictionary<string, JsonElement>))]
    sealed partial class CliJson : System.Text.Json.Serialization.JsonSerializerContext;
}
