// AgentDesk.Core: the one per-user process that owns the board. Started by the first agent session's
// agentdesk.exe (or at login), it serves the board, hooks and waits over a named pipe, and calls Python
// only where Python is best.
using System.Text.Json;
using System.Text.Json.Nodes;
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
_ = Usage.KeepFresh(Path.Combine(data, "claude_usage.json"), watch);
var prs = new PrChecker(store);
var sessions = new Sessions();
var identities = new Identities(store, sessions, data);
_ = prs.Run(TimeSpan.FromSeconds(5));
identities.Resume();
var started = DateTimeOffset.UtcNow;
try { Tray.WebUrl = await Web.Start(data, WebCall); }
catch (Exception e) { Log.Warn($"ops console not started: {e.Message}"); }

Log.Info($"core starting (pid {Environment.ProcessId})");
await PipeServer.Run((req, push, gone) => req.Tool switch
{
    "hook:stop" => identities.AfterTurn(req.Args, hooks.Run("stop", req.Args)), // Phoenix: a handoff restarts the identity after the turn
    ['h', 'o', 'o', 'k', ':', .. var hookEvent] => hooks.Run(hookEvent, req.Args),
    "wait" => hooks.Wait(req.Args.GetInt32(), gone),
    "ui:update" => Setup.Update(new Args(req.Args), $"pipe (pid {req.Caller.Pid}, {req.Caller.EnvAuthor ?? req.Caller.Harness ?? "no author"})"),
    ['u', 'i', ':', .. var op] => Ui(op, new Args(req.Args), push, gone),
    "pass_the_torch" => identities.Torch(req.Caller, Tools.Dispatch(board, req.Caller, req.Tool, req.Args)),
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
    "check_prs" => Task.FromResult(prs.Poke()),
    "fresh" => board.FreshStart(a.String("name")),
    "worker" => board.ToggleWorker(data, python),
    "wake" => board.Wake(a.Int("thread_id"), data, python),
    "subscribe" => Task.FromResult(watch.Subscribe(push, gone)),
    "session_start" => sessions.Start(a.String("name"), a.String("folder"), a.StringOrNull("command")),
    "session_list" => sessions.List(),
    "session_stop" => sessions.Stop(a.String("name")),
    "attach" => sessions.Attach(a.String("name"), a.Int("cols", 120), a.Int("rows", 30), push, gone),
    "input" => sessions.Input(a.String("name"), a.String("data")),
    "resize" => sessions.Resize(a.String("name"), a.Int("cols"), a.Int("rows")),
    "identity_create" => identities.Create(a.String("name"), a.String("folder"), a.StringOrNull("charter"), a.StringOrNull("host"), a.Bool("autostart", false)),
    "identity_list" => identities.List(),
    "identity_start" => identities.Start(a.String("name")),
    "identity_stop" => identities.Stop(a.String("name")),
    "identity_forget" => identities.Forget(a.String("name")),
    "adoptable" => Identities.Adoptable(),
    "adopt" => identities.Adopt(a.String("session_id"), a.String("name")),
    "log_tail" => Task.FromResult(new JsonObject { ["lines"] = new JsonArray([.. Log.Tail(a.Int("lines", 100)).Select(l => (JsonNode)l)]) }.ToJsonString(Wire.Indented)),
    "web_url" => Task.FromResult(new JsonObject { ["url"] = Tray.WebUrl }.ToJsonString(Wire.Indented)),
    _ => Task.FromResult(Tools.Error($"unknown request: ui:{op}")),
};

// The ops console (Host/Web.cs): the requests its page needs, answered by the same objects as the window's.
Task<string> WebCall(string op, JsonElement args) => op switch
{
    "core" => Task.FromResult(new JsonObject { ["pid"] = Environment.ProcessId, ["started"] = started.ToString("yyyy-MM-ddTHH:mm:ssZ"), ["update"] = Setup.State() }.ToJsonString(Wire.Indented)),
    "update" => Setup.Update(new Args(args), "web"),
    "open_questions" => board.OpenQuestions(new Caller(null, null, null, "web", 0), false, null),
    "status" or "worker" or "session_list" or "log_tail" or "identity_list" or "identity_create" or "identity_start" or "identity_stop" or "identity_forget"
        => Ui(op, new Args(args), _ => Task.CompletedTask, CancellationToken.None),
    _ => Task.FromResult(Tools.Error($"unknown request: {op}")),
};
