using System.Diagnostics;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Text.Json.Nodes;
using AgentDesk.Core.Board;

namespace AgentDesk.Core.Host;

/// <summary>
/// Keeps one child process running: starts <c>command</c> in <c>folder</c> and restarts it whenever it exits, waiting
/// 5 s before the first restart and doubling to 5 min, back to 5 s once a run has lasted 10 min. Children are put in a
/// kill-on-close Job Object, so they die with the core however it ends; <see cref="Dispose"/> stops the child at once.
/// </summary>
public sealed partial class Supervisor : IDisposable
{
    readonly string name, exe, args, folder;
    readonly TimeSpan first, max, resetAfter;
    readonly CancellationTokenSource stop = new();
    Process? child;

    public int Restarts { get; private set; }
    public int? LastExit { get; private set; }
    public DateTimeOffset? LastExitAt { get; private set; }
    /// <summary>The wait before the latest restart.</summary>
    public TimeSpan Backoff { get; private set; }
    public int? Pid => child is { HasExited: false } c ? c.Id : null;

    public Supervisor(string name, string command, string folder, TimeSpan? first = null, TimeSpan? max = null, TimeSpan? resetAfter = null)
    {
        (this.name, this.folder) = (name, folder);
        (exe, args) = Split(command.Trim());
        (this.first, this.max, this.resetAfter) = (first ?? TimeSpan.FromSeconds(5), max ?? TimeSpan.FromMinutes(5), resetAfter ?? TimeSpan.FromMinutes(10));
        _ = Run();
    }

    async Task Run()
    {
        var delay = first;
        while (!stop.IsCancellationRequested)
        {
            var began = DateTimeOffset.UtcNow;
            int? code = null;
            try
            {
                var p = Process.Start(new ProcessStartInfo(exe, args) { WorkingDirectory = folder, UseShellExecute = false, CreateNoWindow = true })!;
                if (AssignProcessToJobObject(Job.Value, p.Handle) == 0) Log.Warn($"{name}: pid {p.Id} is not in the core's job ({Marshal.GetLastPInvokeError()}); it may outlive the core");
                child = p;
                Log.Info($"{name}: started pid {p.Id}: {exe} {args}");
                await p.WaitForExitAsync(stop.Token);
                code = p.ExitCode;
            }
            catch (OperationCanceledException) { break; }
            catch (Exception e) when (e is System.ComponentModel.Win32Exception or IOException or InvalidOperationException) { Log.Warn($"{name}: could not start {exe}: {e.Message}"); }
            var up = DateTimeOffset.UtcNow - began;
            (LastExit, LastExitAt) = (code, DateTimeOffset.UtcNow);
            if (up >= resetAfter) delay = first;
            Log.Info($"{name}: exited with code {code?.ToString(CultureInfo.InvariantCulture) ?? "none"} after {up.TotalSeconds:0}s; restarting in {delay.TotalSeconds:0.#}s");
            try { await Task.Delay(delay, stop.Token); } catch (OperationCanceledException) { break; }
            (Backoff, delay) = (delay, delay * 2 > max ? max : delay * 2);
            Restarts++;
        }
        Kill(); // Dispose can land between the start and `child` being set
    }

    public void Dispose()
    {
        stop.Cancel();
        Kill();
    }

    void Kill()
    {
        if (child is not { HasExited: false } c) return;
        Log.Info($"{name}: stopping pid {c.Id}");
        try { c.Kill(entireProcessTree: true); } catch (Exception e) when (e is InvalidOperationException or System.ComponentModel.Win32Exception) { }
    }

    /// <summary><c>"C:\x y\a.exe" b c</c> or <c>a b c</c> into the program and its arguments.</summary>
    static (string, string) Split(string command)
    {
        if (command.StartsWith('"') && command.IndexOf('"', 1) is var q and > 0) return (command[1..q], command[(q + 1)..].Trim());
        return command.IndexOf(' ') is var s and > 0 ? (command[..s], command[(s + 1)..].Trim()) : (command, "");
    }

    /// <summary>One job for the core's lifetime, never closed: Windows closes it when the core exits and kills what is in it.</summary>
    static readonly Lazy<nint> Job = new(() =>
    {
        var job = CreateJobObjectW(0, 0);
        var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION { LimitFlags = 0x2000 }; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if (job == 0 || SetInformationJobObject(job, 9, ref info, (uint)Marshal.SizeOf<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()) == 0) // JobObjectExtendedLimitInformation
            Log.Warn($"could not create the core's job object ({Marshal.GetLastPInvokeError()})");
        return job;
    });

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public nuint MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public nuint Affinity;
        public uint PriorityClass, SchedulingClass;
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount, ReadTransferCount, WriteTransferCount, OtherTransferCount;
        public nuint ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }

    [LibraryImport("kernel32.dll", SetLastError = true)] private static partial nint CreateJobObjectW(nint sa, nint name);
    [LibraryImport("kernel32.dll", SetLastError = true)] private static partial int SetInformationJobObject(nint job, int cls, ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION info, uint len);
    [LibraryImport("kernel32.dll", SetLastError = true)] private static partial int AssignProcessToJobObject(nint job, nint process);
}

/// <summary>
/// The Slack bridge (scripts/slack_bridge.py) under a <see cref="Supervisor"/>. A bridge the core did not start is left
/// alone while its heartbeat (slack_bridge.state: its pid and <c>ts</c>) is fresh, and adopted once it goes stale.
/// </summary>
public sealed class SlackBridge : IDisposable
{
    static readonly TimeSpan Fresh = TimeSpan.FromSeconds(90);
    readonly string state, command, folder;
    readonly TimeSpan poll;
    readonly CancellationTokenSource stop = new();
    public Supervisor? Supervisor { get; private set; }

    public SlackBridge(string data, string command, string folder, TimeSpan? poll = null)
    {
        (state, this.command, this.folder, this.poll) = (Path.Combine(data, "slack_bridge.state"), command, folder, poll ?? TimeSpan.FromSeconds(30));
        _ = Watch();
    }

    /// <summary>The bridge for this core, or null when settings.json's <c>slack_bridge</c> is false. AGENTDESK_BRIDGE_CMD
    /// replaces the command; a core on a temp data folder (AGENTDESK_DATA) never starts the real bridge, which would
    /// post to John's Slack.</summary>
    public static SlackBridge? For(string data, string python)
    {
        if (AgentBoard.Load(Path.Combine(data, "settings.json"))?["slack_bridge"] is JsonValue v && v.TryGetValue(out bool on) && !on)
        {
            Log.Info("slack bridge: off in settings.json");
            return null;
        }
        if (Environment.GetEnvironmentVariable("AGENTDESK_BRIDGE_CMD") is { Length: > 0 } cmd) return new(data, cmd, data);
        if (Environment.GetEnvironmentVariable("AGENTDESK_DATA") is { Length: > 0 })
        {
            Log.Info("slack bridge: not supervised on a custom AGENTDESK_DATA without AGENTDESK_BRIDGE_CMD");
            return null;
        }
        return new(data, $"\"{Path.Combine(python, ".venv", "Scripts", "pythonw.exe")}\" scripts\\slack_bridge.py", python);
    }

    /// <summary>A bridge is running when its heartbeat's pid is alive and its <c>ts</c> is under 90 s old.</summary>
    public static bool Running(string stateFile, DateTimeOffset now) =>
        AgentBoard.Load(stateFile) is { } s && s["pid"] is JsonValue p && p.TryGetValue(out int pid) && AgentBoard.Alive(pid)
        && DateTimeOffset.TryParse(s["ts"]?.ToString(), CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var ts) && now - ts < Fresh;

    async Task Watch()
    {
        try
        {
            if (Running(state, DateTimeOffset.UtcNow)) Log.Info("slack bridge: one is already running; supervising once its heartbeat goes stale");
            while (Running(state, DateTimeOffset.UtcNow)) await Task.Delay(poll, stop.Token);
            lock (stop) if (!stop.IsCancellationRequested) Supervisor = new Supervisor("slack bridge", command, folder);
        }
        catch (OperationCanceledException) { }
    }

    public JsonObject Status() => new()
    {
        ["enabled"] = true, ["supervised"] = Supervisor is not null, ["pid"] = Supervisor?.Pid, ["restarts"] = Supervisor?.Restarts ?? 0,
        ["last_exit"] = Supervisor?.LastExit, ["last_exit_ts"] = Supervisor?.LastExitAt?.ToString("yyyy-MM-ddTHH:mm:sszzz", CultureInfo.InvariantCulture),
    };

    public void Dispose()
    {
        lock (stop) stop.Cancel();
        Supervisor?.Dispose();
    }
}
