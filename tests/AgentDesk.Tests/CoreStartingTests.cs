using System.Diagnostics;
using System.IO;
using System.Windows.Input;
using AgentDesk.App;
using AgentDesk.Contracts;
using Xunit;

namespace AgentDesk.Tests;

/// <summary>A core that is slow to start (the first launch after an update resumes dozens of sessions) must show up as a window that is
/// already usable and says it is starting, not as a dialog or a pile of 15-second dials.</summary>
[Collection("window")]
public class CoreStartingTests
{
    /// <summary>A stand-in for AgentDesk.Core.exe that only counts how many times it was started.</summary>
    static (string Exe, string Log) CountingCore()
    {
        var dir = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "agentdesk-start-" + Guid.NewGuid().ToString("N"))).FullName;
        var log = Path.Combine(dir, "starts.txt");
        var exe = Path.Combine(dir, "core.cmd");
        File.WriteAllText(exe, $"@echo off\r\necho x>>\"{log}\"\r\n");
        return (exe, log);
    }

    static void Isolate(string exe)
    {
        Environment.SetEnvironmentVariable("AGENTDESK_PIPE", $"agentdesk-test-{Guid.NewGuid():N}");
        Environment.SetEnvironmentVariable("AGENTDESK_CORE_EXE", exe);
        Environment.SetEnvironmentVariable("AGENTDESK_DATA", Path.GetTempPath());
    }

    [Fact]
    public async Task Calls_queued_behind_a_failed_dial_fail_at_once_and_the_core_is_started_once()
    {
        var (exe, log) = CountingCore();
        Isolate(exe);
        using var core = CoreConnection.Create(new Caller(null, null, null, "ui", Environment.ProcessId));
        core.Patience = TimeSpan.FromSeconds(1);
        var clock = Stopwatch.StartNew();
        var calls = Enumerable.Range(0, 12).Select(i => core.Call("ui:status")).ToList(); // the window's first refresh: a dozen reads at once
        foreach (var c in calls) await Assert.ThrowsAnyAsync<Exception>(() => c);
        Assert.True(clock.Elapsed < TimeSpan.FromSeconds(4), $"{clock.Elapsed.TotalSeconds:0.0} s for 12 calls: each one dialled for itself"); // was 12 x the patience
        Assert.Single(File.ReadAllLines(log)); // once, not once per call
    }

    [Fact]
    public void The_window_is_usable_at_once_and_says_the_core_is_starting_when_the_core_is_not_there()
    {
        var (exe, _) = CountingCore();
        Isolate(exe);
        using var board = CoreBoard.Create();
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(500);
            Assert.True(w.Body.IsKeyboardFocused || w.Body.IsFocused, "the window must take the keyboard before the core answers");
            w.Press(Key.Q); // a key works, with no core
            Assert.Equal("list", w.screen);
            Assert.Contains("Starting the AgentDesk core", string.Concat(w.BarLine(200).Select(s => s.Text)));
        });
    }
}
