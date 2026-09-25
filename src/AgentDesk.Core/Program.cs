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
var feed = Path.Combine(data, "claude_usage.json");
_ = Usage.KeepFresh(feed, watch, () => Governor.Record(store, feed));
var prs = new PrChecker(store);
var sessions = new Sessions();
var identities = new Identities(store, sessions, data, Environment.GetEnvironmentVariable("AGENTDESK_CLAUDE") ?? "claude"); // a stand-in, for tests
var goals = new Goals(store, identities, sessions);
var slots = new Slots(store);
var concierge = new Concierge(store, goals, python); // its lead works from the AgentDesk checkout, as the crew did
_ = prs.Run(TimeSpan.FromSeconds(5));
identities.Resume();
using var bridge = SlackBridge.For(data, python); // the Slack bridge lives and dies with the core
var started = DateTimeOffset.UtcNow;
try { Tray.WebUrl = await Web.Start(data, WebCall); }
catch (Exception e) { Log.Warn($"ops console not started: {e.Message}"); }
_ = goals.Run(TimeSpan.FromSeconds(double.TryParse(Environment.GetEnvironmentVariable("AGENTDESK_GOAL_TICK"), out var tick) ? tick : 15));

Log.Info($"core starting (pid {Environment.ProcessId})");
await PipeServer.Run((req, push, gone) => req.Tool switch
{
    "hook:stop" => identities.AfterTurn(req.Args, hooks.Run("stop", req.Args)), // Phoenix: a handoff restarts the identity after the turn
    ['h', 'o', 'o', 'k', ':', .. var hookEvent] => hooks.Run(hookEvent, req.Args),
    "wait" => hooks.Wait(req.Args.GetInt32(), gone),
    "ui:update" => Setup.Update(new Args(req.Args), $"pipe (pid {req.Caller.Pid}, {req.Caller.EnvAuthor ?? req.Caller.Harness ?? "no author"})"),
    ['u', 'i', ':', .. var op] => Ui(op, new Args(req.Args), req.Caller, push, gone),
    "pass_the_torch" => identities.Torch(req.Caller, Tools.Dispatch(board, req.Caller, req.Tool, req.Args)),
    "goal_propose" or "experiment_start" or "experiment_done" or "member_spawn" or "member_done" => GoalTool(req.Tool, req.Caller, new Args(req.Args)),
    _ => Tools.Dispatch(board, req.Caller, req.Tool, req.Args),
}, CancellationToken.None);

// AgentDesk's window (docs/ui-api.md).
Task<string> Ui(string op, Args a, Caller caller, Func<string, Task> push, CancellationToken gone) => op switch
{
    "reply" => board.JohnReplies(a.Int("thread_id"), a.String("body")),
    "thread" => board.PeekThread(a.Int("thread_id")),
    "close" => board.CloseQuestion(a.Int("thread_id")),
    "post" => board.JohnPosts(a.String("channel"), a.String("subject", ""), a.String("body")),
    "unarchive" => board.Unarchive(a.Int("thread_id")),
    "status" => Status(),
    "governor" => Governor.Ui(store, data),
    "check_prs" => Task.FromResult(prs.Poke()),
    "concierge" => concierge.Toggle(caller, a.BoolOrNull("on")),
    "wake" => identities.Wake(a.Int("thread_id")),
    "subscribe" => Task.FromResult(watch.Subscribe(push, gone)),
    "session_start" => sessions.Start(a.String("name"), a.String("folder"), a.StringOrNull("command")),
    "session_list" => sessions.List(),
    "session_stop" => sessions.Stop(a.String("name")),
    "attach" => sessions.Attach(a.String("name"), a.Int("cols", 120), a.Int("rows", 30), push, gone),
    "input" => sessions.Input(a.String("name"), a.String("data")),
    "resize" => sessions.Resize(a.String("name"), a.Int("cols"), a.Int("rows")),
    "identity_create" => identities.Create(a.String("name"), a.String("folder"), a.StringOrNull("charter"), a.StringOrNull("host"), a.Bool("autostart", false), model: a.StringOrNull("model")),
    "identity_list" => identities.List(),
    "identity_start" => identities.Start(a.String("name")),
    "identity_stop" => identities.Stop(a.String("name")),
    "identity_forget" => identities.Forget(a.String("name")),
    "adoptable" => Identities.Adoptable(),
    "adopt" => identities.Adopt(a.String("session_id"), a.String("name")),
    "log_tail" => Task.FromResult(new JsonObject { ["lines"] = new JsonArray([.. Log.Tail(a.Int("lines", 100)).Select(l => (JsonNode)l)]) }.ToJsonString(Wire.Indented)),
    "web_url" => Task.FromResult(new JsonObject { ["url"] = Tray.WebUrl }.ToJsonString(Wire.Indented)),
    "goal_create" => goals.Create(a.String("name"), a.String("objective"), a.String("folder")),
    "goal_propose" => GoalTool("goal_propose", caller, a),
    "goal_approve" => goals.Approve(caller, a.String("name"), a.IntOrNull("max_members"), a.DoubleOrNull("max_hours"), a.DoubleOrNull("cadence_minutes")),
    "goal_stop" => goals.Stop(caller, a.String("name")),
    "goal_list" => goals.List(),
    "goal_status" => goals.Status(a.String("name")),
    "slot_list" => slots.List(),
    "slot_assign" => slots.Assign(a.Int("n"), a.StringOrNull("goal"), a.StringOrNull("persona"), a.StringOrNull("persona_icon"), a.StringOrNull("channel_id"), a.StringOrNull("channel_name")),
    "slot_clear" => slots.Clear(a.Int("n")),
    _ => Task.FromResult(Tools.Error($"unknown request: ui:{op}")),
};

// The ops console (Host/Web.cs): the requests its page needs, answered by the same objects as the window's.
Task<string> WebCall(string op, JsonElement args) => op switch
{
    "core" => Task.FromResult(new JsonObject { ["pid"] = Environment.ProcessId, ["started"] = started.ToString("yyyy-MM-ddTHH:mm:ssZ"), ["update"] = Setup.State() }.ToJsonString(Wire.Indented)),
    "update" => Setup.Update(new Args(args), "web"),
    "open_questions" => board.OpenQuestions(new Caller(null, null, null, "web", 0), false, null),
    "status" or "concierge" or "session_list" or "log_tail" or "identity_list" or "identity_create" or "identity_start" or "identity_stop" or "identity_forget"
        => Ui(op, new Args(args), new Caller(null, null, null, "web", 0), _ => Task.CompletedTask, CancellationToken.None),
    _ => Task.FromResult(Tools.Error($"unknown request: {op}")),
};

async Task<string> Status()
{
    var s = JsonNode.Parse(await board.Heartbeats(data))!;
    s["bridge"] = bridge?.Status() ?? new JsonObject { ["enabled"] = false };
    s["goals"] = goals.Summaries();
    s["concierge"] = concierge.State();
    s["sessions"] = identities.Counts();
    return s.ToJsonString(Wire.Indented);
}

// The goal tools agents call (Tools.g.cs lists them); the caller decides what each may do.
Task<string> GoalTool(string tool, Caller c, Args a) => tool switch
{
    "goal_propose" => goals.Propose(c, a.String("name"), a.String("hypothesis"), a.String("measure_cmd"), a.String("success"), a.Int("samples", 1)),
    "experiment_start" => goals.ExperimentStart(c, a.String("goal"), a.String("change")),
    "experiment_done" => goals.ExperimentDone(c, a.String("goal"), a.Int("n")),
    "member_spawn" => goals.Spawn(c, a.String("goal"), a.String("name"), a.String("task"), a.StringOrNull("model"), a.IntOrNull("work_id")),
    _ => goals.MemberDone(c, a.String("summary")),
};
