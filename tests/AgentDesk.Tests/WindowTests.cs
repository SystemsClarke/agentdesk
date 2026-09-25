using AgentDesk.App;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>The window's Agents and Adopt rows, the ops console line, and the tray guard for temp cores.</summary>
public sealed class WindowTests
{
    static string Text(List<Seg> line) => string.Concat(line.Select(s => s.Text));

    [Fact]
    public void An_agent_row_lines_up_under_the_header_and_shows_a_dash_for_no_model()
    {
        var row = MainWindow.AgentRow(new Identity("builder", "queued", 4, "wsl:Ubuntu", "/home/john/src"), 20);
        Assert.Equal("  builder             queued   4    wsl:Ubuntu   —       /home/john/src      ", Text(row));
        Assert.Equal(57 + 20, Text(row).Length);
        Assert.Contains(row, s => s.Text.StartsWith("queued") && s.Tags == "ye");
    }

    [Fact]
    public void An_adoptable_row_is_cut_to_the_width()
    {
        var row = MainWindow.AdoptRow(new Adoptable("id", @"C:\a\very\long\folder\name\indeed", DateTimeOffset.Now, "Fix the build please, it has been red all day"), 12, 20);
        Assert.Equal(2 + 12 + 1 + 11 + 20, Text(row).Length);
        Assert.StartsWith(@"  C:\a\very\l…", Text(row));
        Assert.EndsWith("Fix the build pleas…", Text(row));
    }

    [Fact]
    public void The_home_folder_shows_as_a_tilde()
    {
        var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        Assert.Equal(@"~\srcepo", MainWindow.Tilde(home + @"\srcepo"));
        Assert.Equal(@"D:\src", MainWindow.Tilde(@"D:\src"));
        Assert.Equal(home + "x", MainWindow.Tilde(home + "x"));
    }

    [Fact]
    public void The_ops_console_line_hides_the_key() =>
        Assert.Equal("http://127.0.0.1:47811/", MainWindow.Unkeyed("http://127.0.0.1:47811/?k=secret"));

    [Fact]
    public void A_temp_core_shows_no_tray()
    {
        Assert.False(Tray.Hidden(_ => null));
        Assert.True(Tray.Hidden(n => n == "AGENTDESK_DATA" ? @"C:\tmp\core" : null));
        Assert.True(Tray.Hidden(n => n == "AGENTDESK_NO_TRAY" ? "1" : null));
        Assert.False(Tray.Hidden(n => n == "AGENTDESK_NO_TRAY" ? "0" : null));
    }
}
