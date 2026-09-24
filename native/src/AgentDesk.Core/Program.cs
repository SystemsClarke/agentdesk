// AgentDesk.Core: the one per-user process that owns the board. Started by the first agent's MCP
// shim (or at login), it serves the board over a named pipe and calls Python where Python is best.
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
var wait = $"\"{Path.Combine(python, ".venv", "Scripts", "python.exe")}\" \"{Path.Combine(python, "agentdesk", "wait.py")}\" {{0}}";

Log.Info($"core starting (pid {Environment.ProcessId})");
var board = new AgentBoard(new BoardStore(Path.Combine(data, "agentdesk.db")), new PythonPlugins(python), wait);
await PipeServer.Run(board, CancellationToken.None);
