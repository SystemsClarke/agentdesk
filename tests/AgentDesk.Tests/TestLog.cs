using System.Runtime.CompilerServices;
using AgentDesk.Core;

namespace AgentDesk.Tests;

static class TestLog
{
    /// <summary>Before any test runs, point the core's log at a temp file: Log.Path defaults to the live
    /// %LOCALAPPDATA%\AgentDesk\core.log, and test runs must not write their wakes and PR checks into it.</summary>
    [ModuleInitializer]
    internal static void Init() => Log.Path = Path.Combine(Path.GetTempPath(), $"agentdesk-tests-{Environment.ProcessId}.log");
}
