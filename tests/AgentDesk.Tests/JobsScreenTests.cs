using System.Windows.Input;
using AgentDesk.App;
using Xunit;

namespace AgentDesk.Tests;

/// <summary>The Recurring jobs screen: R from the menu opens it, and its keys reach the core's job_* requests.</summary>
[Collection("window")]
public class JobsScreenTests
{
    [Fact]
    public void The_jobs_screen_runs_toggles_and_deletes_the_job_under_the_cursor()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            w.Press(Key.R);
            Assert.Equal("jobs", w.screen);
            DictateWindowTests.Pump(300); // the screen's own read lands

            w.Press(Key.E); // the first sample job is on: E turns it off at once
            var (req, args) = Assert.Single(board.Acts);
            Assert.Equal("ui:job_enable", req);
            Assert.Contains("on = False", args!.ToString());

            board.Acts.Clear();
            w.Press(Key.R); // run now asks first
            Assert.Empty(board.Acts);
            w.Press(Key.Y);
            Assert.Equal("ui:job_run", Assert.Single(board.Acts).Request);

            board.Acts.Clear();
            w.Press(Key.X);
            w.Press(Key.N); // anything but Y is no
            Assert.Empty(board.Acts);
            w.Press(Key.X);
            w.Press(Key.Y);
            Assert.Equal("ui:job_delete", Assert.Single(board.Acts).Request);

            w.Press(Key.Escape);
            Assert.Equal("main", w.screen);
        });
    }

    [Fact]
    public void A_schedule_reads_as_a_time_and_days_or_an_interval()
    {
        var (daily, every) = (new JobRow("a", "f", "p", "sonnet", "08:00", "weekdays", null, true, null, null, null, 0),
            new JobRow("b", "f", "p", "sonnet", null, "daily", 120, true, null, null, null, 0));
        Assert.Equal("08:00 weekdays", MainWindow.Schedule(daily));
        Assert.Equal("every 120 min", MainWindow.Schedule(every));
    }
}
