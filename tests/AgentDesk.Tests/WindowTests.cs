using AgentDesk.App;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>The window's Agents, Adopt and Goals rows, the goal reader and budget panel, the ops console line, and the tray guard for temp cores.</summary>
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
        Assert.Equal(@"~\src
epo", MainWindow.Tilde(home + @"\src
epo"));
        Assert.Equal(@"D:\src", MainWindow.Tilde(@"D:\src"));
        Assert.Equal(home + "x", MainWindow.Tilde(home + "x"));
    }

    [Fact]
    public void The_ops_console_line_hides_the_key() =>
        Assert.Equal("http://127.0.0.1:47811/", MainWindow.Unkeyed("http://127.0.0.1:47811/?k=secret"));

    static readonly GoalRow Speed = new("build-speed", "running", "Get the compile under 25 minutes", "build-speed-lead", "value < 25", 12, 29.4, 2, 3);

    [Fact]
    public void A_goal_row_shows_its_value_against_the_line_and_the_leads_generation()
    {
        var row = MainWindow.GoalLine(Speed, 1, new Identity("build-speed-lead", "running", 2, "windows", @"C:\src", "opus"), 30);
        Assert.Equal("  build-speed       running   1    29.4 · < 25         12   2/3    build-speed-lead · gen 2      ", Text(row));
        Assert.Equal(67 + 30, Text(row).Length);
        var standing = MainWindow.GoalLine(Speed with { Name = "concierge", Standing = true, LastValue = null, Success = null }, null, null, 20);
        Assert.Contains(standing, s => s.Text.StartsWith("standing") && s.Tags == "gr");
        Assert.Contains("—    — · no line yet", Text(standing));
    }

    [Fact]
    public void The_goal_reader_shows_the_last_10_experiments_a_sparkline_and_the_lead()
    {
        List<Experiment> log = [.. Enumerable.Range(1, 12).Select(n => new Experiment(n, $"change {n}", "build-speed-lead", n < 12 ? 40 - n : null, n < 12 ? "improved" : null))];
        var d = new GoalDetail(Speed, "Caching cuts it", "measure.cmd", 24, 30, DateTimeOffset.Now.AddHours(-2), log, [.. log.Where(e => e.Value != null).Select(e => e.Value!.Value)],
            [new("build-speed-cache", "try the cache", 7)], "Goal build-speed (running)\nNext: experiment 12");
        var text = MainWindow.GoalReader(d, new Identity("build-speed-lead", "running", 3, "windows", @"C:\src"), 1, 96).Select(Text).ToList();
        Assert.All(text.Where(l => l.StartsWith('╔') || l.StartsWith('┌')), l => Assert.Equal(96, l.Length));
        Assert.StartsWith("╔═ Goal build-speed ═ running in slot 1 ═", text[0]);
        Assert.Contains(text, l => l.Contains("Experiments · last 10 of 12"));
        Assert.DoesNotContain(text, l => l.Contains("#2   change 2 "));
        Assert.Contains(text, l => l.Contains("#12  change 12") && l.Contains("measuring"));
        Assert.Contains(text, l => l.StartsWith("  History     █▇▆▅") && l.EndsWith("29–39, last 29"));
        Assert.Contains(text, l => l.Contains("2 of 3 members · 2h 00m of 24h · a wake every 30 min"));
        Assert.Contains(text, l => l.Contains("build-speed-cache") && l.Contains("#7"));
        Assert.Contains(text, l => l.Contains("Next: experiment 12"));
        Assert.Equal(" L attaches to build-speed-lead (generation 3, running): agentdesk attach build-speed-lead", text[^1]);
    }

    [Fact]
    public void The_budget_panel_shows_the_week_the_allowance_the_mode_and_the_series()
    {
        var b = new Budget(900, 18, 30, 97.4, 5, 2, 2, "why", "summary", [.. Enumerable.Range(0, 168).Select(i => (double)i / 2)], "advisory");
        var text = MainWindow.BudgetLines(b, 92).Select(Text).ToList();
        Assert.Equal("18% of the week left · resets in 1d 6h · forecast 97.4% at the reset    advisory ", text[0]);
        Assert.Equal("Today's allowance: 5 sessions · 2 swarms x 2 members", text[1]);
        Assert.Equal("why", text[2]);
        Assert.StartsWith("This week  ▁", text[3]);
        Assert.Equal(11 + 56 + "  0–83.5%, last 83.5%".Length, text[3].Length); // 168 hours squeezed to the width
        Assert.Single(MainWindow.BudgetLines(b with { Samples = 0 }, 92));
        Assert.Equal(3, MainWindow.BudgetLines(b with { Series = [], Mode = null }, 92).Count);
        Assert.Equal("18% left · resets in 1d 6h · forecast 97.4% · 5 sessions (2x2) · advisory", MainWindow.GovLine(b));
        Assert.StartsWith("no usage samples yet", MainWindow.GovLine(null));
    }

    [Fact]
    public void A_temp_core_shows_no_tray()
    {
        Assert.False(Tray.Hidden(_ => null));
        Assert.True(Tray.Hidden(n => n == "AGENTDESK_DATA" ? @"C:\tmp\core" : null));
        Assert.True(Tray.Hidden(n => n == "AGENTDESK_NO_TRAY" ? "1" : null));
        Assert.False(Tray.Hidden(n => n == "AGENTDESK_NO_TRAY" ? "0" : null));
    }
}
