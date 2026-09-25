using System.Diagnostics;
using System.Text;
using System.Text.Json;
using AgentDesk.Core;

namespace AgentDesk.Tests;

/// <summary>Headless sessions on a pseudoconsole, with cmd standing in for claude.</summary>
public sealed class SessionsTests : IDisposable
{
    const string Shell = "cmd /d /q /k"; // stays up, reads keys, draws no banner
    readonly Sessions sessions = new();
    readonly string name = $"t{Guid.NewGuid():N}"[..9];
    readonly CancellationTokenSource gone = new();

    public void Dispose()
    {
        gone.Cancel();
        try { sessions.Stop(name).Wait(); } catch (AggregateException) { } // already stopped or never started
    }

    sealed class Viewer
    {
        readonly StringBuilder seen = new();
        public readonly TaskCompletionSource Exited = new();
        public string? First; // the first chunk pushed: the replay, when there was output before this viewer

        public Task Push(string e)
        {
            var ev = JsonDocument.Parse(e).RootElement;
            if (ev.GetProperty("event").GetString() == "session.exited") Exited.TrySetResult();
            else
                lock (seen)
                {
                    var text = Encoding.UTF8.GetString(ev.GetProperty("data").GetBytesFromBase64());
                    First ??= text;
                    seen.Append(text);
                }
            return Task.CompletedTask;
        }

        public async Task Sees(string text)
        {
            for (var sw = Stopwatch.StartNew(); sw.Elapsed < TimeSpan.FromSeconds(15); await Task.Delay(50))
                lock (seen) if (seen.ToString().Contains(text)) return;
            lock (seen) Assert.Fail($"never saw '{text}' in: {seen}");
        }
    }

    async Task<Viewer> Attach()
    {
        var v = new Viewer();
        Assert.Contains("\"attached\"", await sessions.Attach(name, 100, 30, v.Push, gone.Token));
        return v;
    }

    [Fact]
    public async Task Output_reaches_an_attached_viewer()
    {
        await sessions.Start(name, Path.GetTempPath(), $"{Shell} ping -n 2 127.0.0.1 >nul & echo hello-viewer");
        var v = await Attach();
        await v.Sees("hello-viewer");
        Assert.Contains(name, await sessions.List());
    }

    [Fact]
    public async Task A_late_attacher_gets_the_replay()
    {
        await sessions.Start(name, Path.GetTempPath(), $"{Shell} echo hello-replay");
        await (await Attach()).Sees("hello-replay");
        var late = await Attach(); // attached after it was printed, and before the redraw: the ring, not the screen
        Assert.Contains("hello-replay", late.First);
    }

    [Fact]
    public async Task Input_round_trips()
    {
        await sessions.Start(name, Path.GetTempPath(), Shell);
        var v = await Attach();
        await sessions.Input(name, "set /a 1234+4321\r");
        await v.Sees("5555");
    }

    [Fact]
    public async Task Stop_kills_the_process()
    {
        var pid = JsonDocument.Parse(await sessions.Start(name, Path.GetTempPath(), Shell)).RootElement.GetProperty("pid").GetInt32();
        var v = await Attach();
        Assert.Contains("\"stopped\"", await sessions.Stop(name));
        await v.Exited.Task.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Throws<ArgumentException>(() => Process.GetProcessById(pid));
        Assert.DoesNotContain(name, await sessions.List());
    }
}
