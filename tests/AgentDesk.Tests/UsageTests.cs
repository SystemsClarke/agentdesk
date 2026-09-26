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
}
