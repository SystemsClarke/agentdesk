using System.Diagnostics;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>The core's child-process supervisor, and the Slack bridge under it, with cmd standing in for the bridge.</summary>
public sealed class SupervisorTests : IDisposable
{
    readonly string data = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), $"sup-{Guid.NewGuid():N}")).FullName;
    string State => Path.Combine(data, "slack_bridge.state");

    public void Dispose() { try { Directory.Delete(data, true); } catch (IOException) { } }

    static async Task Until(Func<bool> ok, int seconds = 10)
    {
        var sw = Stopwatch.StartNew();
        while (!ok()) { Assert.True(sw.Elapsed.TotalSeconds < seconds, "timed out"); await Task.Delay(50); }
    }

    /// <summary>Writes the heartbeat as the bridge does: a temp file moved over the old one. Writing it in place let the
    /// bridge's watcher read a half-written file as no bridge at all, which is what made the healthy-bridge test flaky.</summary>
    void Beat(int pid, DateTimeOffset ts)
    {
        var tmp = State + ".tmp";
        File.WriteAllText(tmp, $$"""{"pid": {{pid}}, "ts": "{{ts:yyyy-MM-ddTHH:mm:ss.fffzzz}}", "poll_s": 15, "last_relay": null}""");
        for (var i = 0; ; i++)
            try { File.Move(tmp, State, true); return; }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException && i < 20) { Thread.Sleep(10); } // the watcher is reading it
    }

    /// <summary>Someone else's bridge: a process this test owns, alive until it is ended.</summary>
    static Process StandIn() => Process.Start(new ProcessStartInfo("cmd", "/d /c ping -n 120 127.0.0.1 >nul") { UseShellExecute = false, CreateNoWindow = true })!;

    static void End(Process p)
    {
        try { p.Kill(true); } catch (InvalidOperationException) { }
        p.WaitForExit(5000);
    }

    [Fact]
    public async Task A_command_that_exits_is_restarted_with_doubling_backoff_up_to_the_cap()
    {
        using var s = new Supervisor("t", "cmd /d /c exit 1", data, TimeSpan.FromMilliseconds(100), TimeSpan.FromMilliseconds(400), TimeSpan.FromMinutes(1));
        await Until(() => s.Restarts >= 1);
        Assert.Equal(TimeSpan.FromMilliseconds(100), s.Backoff);
        await Until(() => s.Restarts >= 2);
        Assert.Equal(TimeSpan.FromMilliseconds(200), s.Backoff);
        await Until(() => s.Restarts >= 4);
        Assert.Equal(TimeSpan.FromMilliseconds(400), s.Backoff); // 100, 200, 400, 400: capped
        Assert.Equal(1, s.LastExit);
        Assert.NotNull(s.LastExitAt);
    }

    [Fact]
    public async Task A_run_longer_than_the_reset_time_restarts_at_the_first_delay_again()
    {
        using var s = new Supervisor("t", "cmd /d /c ping -n 2 127.0.0.1 >nul", data, TimeSpan.FromMilliseconds(100), TimeSpan.FromSeconds(5), TimeSpan.FromMilliseconds(300));
        await Until(() => s.Restarts >= 2, 20);
        Assert.Equal(TimeSpan.FromMilliseconds(100), s.Backoff); // each run lasts ~1 s, over the 300 ms reset
        Assert.Equal(0, s.LastExit);
    }

    [Fact]
    public async Task Dispose_stops_the_child_and_its_children()
    {
        var s = new Supervisor("t", "cmd /d /c ping -n 60 127.0.0.1 >nul", data);
        await Until(() => s.Pid is not null);
        using var child = Process.GetProcessById(s.Pid!.Value);
        s.Dispose();
        Assert.True(child.WaitForExit(5000), "the child outlived Dispose");
        await Task.Delay(300);
        Assert.Null(s.Pid);
        Assert.Equal(0, s.Restarts);
    }

    [Fact]
    public void Running_needs_a_live_pid_and_a_fresh_heartbeat()
    {
        var now = DateTimeOffset.UtcNow; // beats are stamped now and read later: a pid must have started before its beat
        Assert.False(SlackBridge.Running(State, now)); // no file
        Beat(Environment.ProcessId, now);
        Assert.True(SlackBridge.Running(State, now.AddSeconds(10)));
        Assert.False(SlackBridge.Running(State, now.AddSeconds(120))); // stale
        Beat(DeadPid(), now);
        Assert.False(SlackBridge.Running(State, now)); // dead
        Beat(Environment.ProcessId, DateTimeOffset.UtcNow.AddYears(-30));
        Assert.Null(SlackBridge.Heartbeat(State, now).Pid); // beat before the process started: a reused pid, not the bridge
        File.WriteAllText(State, "not json");
        Assert.False(SlackBridge.Running(State, now));
    }

    [Fact]
    public async Task A_healthy_unsupervised_bridge_is_left_alone_and_adopted_once_it_is_gone()
    {
        var other = StandIn();
        try
        {
            Beat(other.Id, DateTimeOffset.UtcNow.AddSeconds(1)); // someone else's bridge, alive and beating
            using var bridge = new SlackBridge(data, "cmd /d /c ping -n 60 127.0.0.1 >nul", data, TimeSpan.FromMilliseconds(100));
            await Until(() => bridge.State == "external");
            await Task.Delay(300); // several more polls
            Assert.Null(bridge.Supervisor);
            Assert.False(bridge.Status()["supervised"]!.GetValue<bool>());

            End(other); // it goes away
            await Until(() => bridge.Supervisor?.Pid is not null);
            Assert.Equal("supervised", (string)bridge.Status()["state"]!);
            Assert.Equal(0, bridge.Status()["restarts"]!.GetValue<int>());
        }
        finally { End(other); }
    }

    [Fact]
    public async Task An_unsupervised_bridge_alive_but_silent_is_a_stale_orphan_and_no_second_bridge_starts()
    {
        var other = StandIn();
        try
        {
            Beat(other.Id, DateTimeOffset.UtcNow.AddSeconds(1)); // then it never beats again
            using var bridge = new SlackBridge(data, "cmd /d /c ping -n 60 127.0.0.1 >nul", data, TimeSpan.FromMilliseconds(100), hung: TimeSpan.FromSeconds(2));
            await Until(() => bridge.State == "stale-orphan");
            await Task.Delay(300);
            var status = bridge.Status();
            Assert.Equal("stale-orphan", (string)status["state"]!);
            Assert.Equal(other.Id, status["orphan_pid"]!.GetValue<int>());
            Assert.Null(bridge.Supervisor); // a second bridge would double-post

            End(other); // John kills it: the core takes over
            await Until(() => bridge.Supervisor?.Pid is not null);
            Assert.Null(bridge.Status()["orphan_pid"]);
        }
        finally { End(other); }
    }

    [Fact]
    public async Task A_supervised_bridge_that_stops_beating_is_killed_and_restarted()
    {
        // The stand-in never beats by itself; the test beats for it, then stops.
        using var bridge = new SlackBridge(data, "cmd /d /c ping -n 120 127.0.0.1 >nul", data, TimeSpan.FromMilliseconds(100), hung: TimeSpan.FromSeconds(1));
        await Until(() => bridge.Supervisor?.Pid is not null);
        var first = bridge.Supervisor!.Pid!.Value;
        using var child = Process.GetProcessById(first);
        for (var sw = Stopwatch.StartNew(); sw.Elapsed < TimeSpan.FromSeconds(2.5); await Task.Delay(100))
            Beat(Environment.ProcessId, DateTimeOffset.UtcNow); // any pid's beat counts (pythonw runs the interpreter as a child)
        Assert.Equal(0, bridge.HungRestarts); // up past the hung time, but beating

        await Until(() => bridge.HungRestarts == 1); // it stops beating
        Assert.True(child.WaitForExit(5000), "the hung bridge was not killed");
        await Until(() => bridge.Supervisor.Pid is { } pid && pid != first, 15); // restarted after the usual 5 s
        Assert.Equal(1, bridge.Status()["hung_restarts"]!.GetValue<int>());
    }

    [Fact]
    public async Task With_no_bridge_running_it_is_supervised_at_once()
    {
        using var bridge = new SlackBridge(data, "cmd /d /c exit 3", data);
        await Until(() => bridge.Supervisor?.LastExit is not null);
        Assert.Equal(3, bridge.Status()["last_exit"]!.GetValue<int>());
    }

    [Fact]
    public void The_settings_file_turns_the_bridge_off()
    {
        File.WriteAllText(Path.Combine(data, "settings.json"), """{"slack_bridge": false}""");
        Assert.Null(SlackBridge.For(data, data));
    }

    static int DeadPid()
    {
        using var p = Process.Start(new ProcessStartInfo("cmd", "/d /c exit 0") { UseShellExecute = false, CreateNoWindow = true })!;
        p.WaitForExit();
        return p.Id;
    }
}
