// AgentDesk.Core: the one per-user process that owns the board. Started by the first agent session's
// agentdesk.exe (or at login), it serves the board, hooks and waits over a named pipe, and calls Python
// only where Python is best.
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;
using AgentDesk.Core.Host;
using AgentDesk.Core.Plugins;

using var single = new Mutex(true, @"Local\AgentDesk.Core", out var first);
if (!first) return; // already running

var data = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AgentDesk");
Directory.CreateDirectory(data);
var python = Environment.GetEnvironmentVariable("AGENTDESK_PYTHON")
             ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "NoOneDrive", "AgentDesk");
var store = new BoardStore(Path.Combine(data, "agentdesk.db"));
var board = new AgentBoard(store, new PythonPlugins(python), $"\"{Path.Combine(AppContext.BaseDirectory, "agentdesk.exe")}\" wait {{0}}");
var hooks = new Hooks(store);

Log.Info($"core starting (pid {Environment.ProcessId})");
await PipeServer.Run((req, ct) => req.Tool switch
{
    ['h', 'o', 'o', 'k', ':', .. var hookEvent] => hooks.Run(hookEvent, req.Args),
    "wait" => hooks.Wait(req.Args.GetInt32(), ct),
    _ => Tools.Dispatch(board, req.Caller, req.Tool, req.Args),
}, CancellationToken.None);
