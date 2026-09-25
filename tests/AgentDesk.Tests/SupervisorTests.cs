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

    void Beat(int pid, DateTimeOffset ts) =>
        File.WriteAllText(State, $$"""{"pid": {{pid}}, "ts": "{{ts:yyyy-MM-ddTHH:mm:sszzz}}", "poll_s": 15, "last_relay": null}""");

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
        var now = DateTimeOffset.UtcNow;
        Assert.False(SlackBridge.Running(State, now)); // no file
        Beat(Environment.ProcessId, now.AddSeconds(-10));
        Assert.True(SlackBridge.Running(State, now));
        Beat(Environment.ProcessId, now.AddSeconds(-120));
        Assert.False(SlackBridge.Running(State, now)); // stale
        Beat(DeadPid(), now);
        Assert.False(SlackBridge.Running(State, now)); // dead
        File.WriteAllText(State, "not json");
        Assert.False(SlackBridge.Running(State, now));
    }

    [Fact]
    public async Task A_healthy_unsupervised_bridge_is_left_alone_and_adopted_once_its_heartbeat_goes_stale()
    {
        Beat(Environment.ProcessId, DateTimeOffset.UtcNow); // someone else's bridge, alive and fresh
        using var bridge = new SlackBridge(data, "cmd /d /c ping -n 60 127.0.0.1 >nul", data, TimeSpan.FromMilliseconds(100));
        await Task.Delay(500);
        Assert.Null(bridge.Supervisor);
        Assert.False(bridge.Status()["supervised"]!.GetValue<bool>());

        Beat(Environment.ProcessId, DateTimeOffset.UtcNow.AddMinutes(-5)); // it stops beating
        await Until(() => bridge.Supervisor?.Pid is not null);
        Assert.True(bridge.Status()["supervised"]!.GetValue<bool>());
        Assert.Equal(0, bridge.Status()["restarts"]!.GetValue<int>());
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
