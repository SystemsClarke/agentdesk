using AgentDesk.App;
using Xunit;

namespace AgentDesk.Tests;

/// <summary>The SysOp screen's vault backup line.</summary>
public class VaultHealthTests
{
    static readonly DateTimeOffset Now = new(2026, 10, 7, 12, 0, 0, TimeSpan.Zero);

    [Fact]
    public void A_recent_push_reads_green_with_its_age()
    {
        var s = MainWindow.VaultSegs(new(Now.AddMinutes(-12), true, 0, null), Now);
        Assert.Equal("pushed to GitHub 12 min ago", s.Text);
        Assert.Equal("gr", s.Tags);
    }

    [Fact]
    public void A_failed_push_says_why_and_how_much_is_not_on_github()
    {
        var s = MainWindow.VaultSegs(new(Now.AddMinutes(-5), false, 3, "push failed: could not resolve host\nmore"), Now);
        Assert.Contains("push failed: could not resolve host", s.Text);
        Assert.DoesNotContain("more", s.Text);
        Assert.Contains("3 commit(s) not on GitHub", s.Text);
        Assert.Equal("pk", s.Tags);
    }

    [Fact]
    public void A_backup_that_has_gone_quiet_is_flagged_even_if_its_last_run_was_fine()
    {
        Assert.Contains("last ran 5 h ago", MainWindow.VaultSegs(new(Now.AddHours(-5), true, 0, null), Now).Text);
        Assert.Contains("no report yet", MainWindow.VaultSegs(null, Now).Text);
    }
}
