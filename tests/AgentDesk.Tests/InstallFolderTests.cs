using System.Diagnostics;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>Why "Restart to update" kept landing on the old version: a process whose working directory is inside current\ stops
/// Windows replacing that folder. Here ping stands in for the window.</summary>
public sealed class InstallFolderTests : IDisposable
{
    readonly string dir = Directory.CreateTempSubdirectory("install-").FullName;
    readonly List<Process> started = [];

    public void Dispose()
    {
        foreach (var p in started)
            try { if (!p.HasExited) p.Kill(); } catch (InvalidOperationException) { }
        try { Directory.Delete(dir, true); } catch (IOException) { }
    }

    Process Ping(string cwd)
    {
        var p = Process.Start(new ProcessStartInfo("ping", "-t 127.0.0.1") { WorkingDirectory = cwd, UseShellExecute = false, CreateNoWindow = true })!;
        started.Add(p);
        // Started is not settled: the process opens its working directory itself, a moment after CreateProcess returns. Until it has,
        // the folder can still be moved, so wait for the process to report the directory it was given.
        for (var i = 0; i < 100 && Norm(InstallFolder.CwdOf(p.Id)) != Norm(cwd); i++) Thread.Sleep(50);
        return p;
    }

    static string Norm(string? p) => (p ?? "").TrimEnd('\\');

    [Fact]
    public void A_process_working_directory_is_read_from_the_process_itself()
    {
        Assert.Equal(Norm(Environment.CurrentDirectory), Norm(InstallFolder.CwdOf(Environment.ProcessId)));
        var inside = Directory.CreateDirectory(Path.Combine(dir, "current")).FullName;
        var p = Ping(inside);
        Assert.Equal(Norm(inside), Norm(InstallFolder.CwdOf(p.Id)));
        Assert.Null(InstallFolder.CwdOf(int.MaxValue));
    }

    [Fact]
    public void A_process_running_inside_the_folder_blocks_replacing_it_and_release_frees_it()
    {
        var current = Directory.CreateDirectory(Path.Combine(dir, "current")).FullName;
        var elsewhere = Directory.CreateDirectory(Path.Combine(dir, "project")).FullName;
        var pinned = Ping(current);
        var bystander = Ping(elsewhere); // an MCP relay in a project folder: never touched

        Assert.False(pinned.HasExited, "the process that should pin the folder is running");
        Assert.Equal(Norm(current), Norm(InstallFolder.CwdOf(pinned.Id)));
        // (That Windows refuses to move this folder now is checked by hand, not here: under some test hosts the move goes through.)
        Assert.Contains(InstallFolder.Holders(current), h => h.EndsWith($"(pid {pinned.Id})"));
        Assert.DoesNotContain(InstallFolder.Holders(current), h => h.EndsWith($"(pid {bystander.Id})"));

        Assert.False(InstallFolder.Release(current, "ping")); // no window among them
        Assert.True(pinned.WaitForExit(5000), "the process that pinned the folder is closed");
        Assert.False(bystander.HasExited, "a process working elsewhere is left alone");

        Directory.Move(current, Path.Combine(dir, "current_old")); // the fix: now the swap can happen
        Assert.True(Directory.Exists(Path.Combine(dir, "current_old")));
    }

    [Fact]
    public void Release_only_looks_at_the_names_it_is_given()
    {
        var current = Directory.CreateDirectory(Path.Combine(dir, "current")).FullName;
        var p = Ping(current);
        Assert.False(InstallFolder.Release(current, "AgentDesk.App", "agentdesk"));
        Assert.False(p.HasExited);
    }
}
