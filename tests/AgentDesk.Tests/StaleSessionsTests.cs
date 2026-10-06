using System.Diagnostics;
using AgentDesk.Core;
using AgentDesk.Core.Host;
using Xunit;

namespace AgentDesk.Tests;

public sealed class StaleSessionsTests
{
    [Theory]
    [InlineData("claude --resume 11111111-2222-3333-4444-555555555555 --model opus", true)]
    [InlineData("claude --session-id 11111111-2222-3333-4444-555555555555", true)]
    [InlineData("claude --resume 11111111-2222-3333-4444-555555555555", true)]
    [InlineData("CLAUDE --RESUME 11111111-2222-3333-4444-555555555555 x", true)]
    [InlineData("claude --resume 11111111-2222-3333-4444-5555555555556 --model opus", false)] // the start of a longer id
    [InlineData("claude --resume 99999999-2222-3333-4444-555555555555", false)]               // another conversation
    [InlineData("claude --append-system-prompt 11111111-2222-3333-4444-555555555555", false)] // the id is only mentioned
    [InlineData("claude", false)]
    public void A_command_line_carries_a_conversation_only_as_the_whole_id_after_resume_or_session_id(string line, bool expected) =>
        Assert.Equal(expected, StaleSessions.Carries(line, "11111111-2222-3333-4444-555555555555"));

    static Process Hold(string id) => Process.Start(new ProcessStartInfo("cmd.exe", $"/c \"ping -n 60 127.0.0.1 >nul & rem --resume {id}\"") { UseShellExecute = false, CreateNoWindow = true })!;

    [Fact]
    public void Only_the_process_on_that_conversation_is_stopped_and_one_this_core_hosts_is_left_alone()
    {
        var (mine, other, hosted) = (Guid.NewGuid().ToString(), Guid.NewGuid().ToString(), Guid.NewGuid().ToString());
        using var stale = Hold(mine);
        using var unrelated = Hold(other);
        using var ours = Hold(hosted);
        try
        {
            Thread.Sleep(500); // let cmd start
            Assert.Equal([stale.Id], StaleSessions.Find(mine, new HashSet<int>(), "cmd"));
            Assert.Empty(StaleSessions.Find(hosted, new HashSet<int> { ours.Id }, "cmd")); // a session this core started is never "stale"
            Assert.Empty(StaleSessions.Find("not-a-conversation-id", new HashSet<int>(), "cmd")); // a fragment is never matched
            Assert.Equal([stale.Id], StaleSessions.Stop(mine, new HashSet<int>(), "cmd"));
            Assert.True(stale.WaitForExit(5000), "the duplicate ended");
            Assert.False(unrelated.HasExited);
            Assert.False(ours.HasExited);
            Assert.Empty(StaleSessions.Stop(mine, new HashSet<int>(), "claude")); // wrong name: even a matching command line is left alone
        }
        finally
        {
            foreach (var p in new[] { stale, unrelated, ours }) try { if (!p.HasExited) p.Kill(true); } catch (InvalidOperationException) { }
        }
    }

    [Fact]
    public async Task A_session_the_core_starts_is_in_its_job_and_an_ordinary_process_is_not()
    {
        var sessions = new Sessions();
        var started = sessions.Launch("jobtest", Path.GetTempPath(), "cmd.exe /c ping -n 30 127.0.0.1 >nul", new Dictionary<string, string?>());
        try
        {
            Assert.True(Supervisor.IsInCoreJob(started), "the session ends when the core does");
            using var plain = Process.Start(new ProcessStartInfo("cmd.exe", "/c ping -n 5 127.0.0.1 >nul") { UseShellExecute = false, CreateNoWindow = true })!;
            Assert.False(Supervisor.IsInCoreJob(plain.Id));
            plain.Kill(true);
        }
        finally { await sessions.Stop("jobtest"); }
    }
}
