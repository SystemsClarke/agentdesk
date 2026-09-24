// AgentDesk.Core: the one per-user process that owns the board. Started by the first agent session's
// agentdesk.exe (or at login), it serves the board, hooks and waits over a named pipe, and calls Python
// only where Python is best.
using AgentDesk.Contracts;
using AgentDesk.Core;
using AgentDesk.Core.Board;
using AgentDesk.Core.Host;
using AgentDesk.Core.Plugins;

Setup.Run(); // Velopack: install/uninstall hooks exit here
using var single = new Mutex(true, $@"Local\{PipeNames.Board}", out var first); // one core per pipe
if (!args.Contains("--background")) Tray.Launch(0); // John started it (Start menu): show the window
if (!first) return; // already running
_ = Setup.KeepUpdated(apply => Tray.ApplyUpdate = apply);

var data = Environment.GetEnvironmentVariable("AGENTDESK_DATA")
           ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AgentDesk");
Directory.CreateDirectory(data);
Log.Path = Path.Combine(data, "core.log");
var python = Environment.GetEnvironmentVariable("AGENTDESK_PYTHON")
             ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "NoOneDrive", "AgentDesk");
var store = new BoardStore(Path.Combine(data, "agentdesk.db"));
var board = new AgentBoard(store, new PythonPlugins(python), $"\"{Path.Combine(AppContext.BaseDirectory, "agentdesk.exe")}\" wait {{0}}");
var hooks = new Hooks(store);
var watch = new BoardWatch(store);
Tray.Start(store);

Log.Info($"core starting (pid {Environment.ProcessId})");
await PipeServer.Run((req, push, gone) => req.Tool switch
{
    ['h', 'o', 'o', 'k', ':', .. var hookEvent] => hooks.Run(hookEvent, req.Args),
    "wait" => hooks.Wait(req.Args.GetInt32(), gone),
    ['u', 'i', ':', .. var op] => Ui(op, new Args(req.Args), push, gone),
    _ => Tools.Dispatch(board, req.Caller, req.Tool, req.Args),
}, CancellationToken.None);

// AgentDesk's window (docs/ui-api.md).
Task<string> Ui(string op, Args a, Func<string, Task> push, CancellationToken gone) => op switch
{
    "reply" => board.JohnReplies(a.Int("thread_id"), a.String("body")),
    "thread" => board.PeekThread(a.Int("thread_id")),
    "close" => board.CloseQuestion(a.Int("thread_id")),
    "post" => board.JohnPosts(a.String("channel"), a.String("subject", ""), a.String("body")),
    "unarchive" => board.Unarchive(a.Int("thread_id")),
    "status" => board.Heartbeats(data),
    "subscribe" => Task.FromResult(watch.Subscribe(push, gone)),
    _ => Task.FromResult(Tools.Error($"unknown request: ui:{op}")),
};
