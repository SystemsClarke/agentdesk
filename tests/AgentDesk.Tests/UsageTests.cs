using AgentDesk.Core;

namespace AgentDesk.Tests;

/// <summary>The 5-minute /usage probe.</summary>
public sealed class UsageTests
{
    [Fact]
    public void The_probe_leaves_no_transcript_behind() // 288 runs a day each wrote one under ~/.claude/projects
    {
        Assert.Equal(["-p", "/usage"], Usage.ProbeArgs[..2]);
        Assert.Contains("--no-session-persistence", Usage.ProbeArgs);
    }

    [Fact]
    public void The_probe_runs_only_when_something_reads_it()
    {
        var now = DateTime.UtcNow;
        Assert.False(Usage.Wanted(false, false, now, 0)); // nobody enforces, nobody waits, nobody looked
        Assert.True(Usage.Wanted(true, false, now, 0)); // enforcing
        Assert.True(Usage.Wanted(false, true, now, 0)); // a swarm session is queued: fail closed on a real reading
        Assert.True(Usage.Wanted(false, false, now, now.AddMinutes(-9).Ticks)); // the governor screen was open
        Assert.False(Usage.Wanted(false, false, now, now.AddMinutes(-11).Ticks));
    }
}
