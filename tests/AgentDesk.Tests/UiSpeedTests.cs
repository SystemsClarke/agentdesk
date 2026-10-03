using System.Windows.Documents;
using System.Windows.Input;
using AgentDesk.App;
using AgentDesk.Contracts;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>Fewer calls and less layout per refresh: each claim is a count of calls or of layouts, not a feeling.</summary>
[Collection("window")] // one WPF Application per process: window tests run one after another
public sealed class UiSpeedTests
{
    static void Settle() => DictateWindowTests.Pump(1200); // past the 750 ms change debounce
    static string Text(IEnumerable<Seg> line) => string.Concat(line.Select(s => s.Text));

    [Fact]
    public void The_consoles_address_is_asked_for_once_and_status_on_each_full_refresh()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            Assert.Equal(1, board.WebUrlCalls);
            w.Press(Key.Q);
            var status = board.StatusCalls;
            w.Press(Key.H); // a full refresh (archived questions)
            DictateWindowTests.Pump(300);
            w.Press(Key.H);
            DictateWindowTests.Pump(300);
            Assert.True(board.StatusCalls >= status + 2);
            Assert.Equal(1, board.WebUrlCalls);
        });
    }

    [Theory]
    [InlineData(Key.Q, true)]  // a message base: a push re-reads the threads only
    [InlineData(Key.S, false)] // SysOp shows the status data: a push re-reads it all
    public void A_change_push_on_a_list_skips_the_status_reads_and_elsewhere_does_not(Key key, bool skips)
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            w.Press(key);
            DictateWindowTests.Pump(300);
            var (lists, status, ids, slots) = (board.ListCalls, board.StatusCalls, board.IdentityCalls, board.SlotCalls);
            board.Push();
            Settle();
            Assert.True(board.ListCalls > lists, "the push refreshed");
            if (skips)
                Assert.Equal((status, ids, slots), (board.StatusCalls, board.IdentityCalls, board.SlotCalls));
            else
                Assert.True(board.StatusCalls > status && board.IdentityCalls > ids && board.SlotCalls > slots);
        });
    }

    [Fact]
    public void A_refresh_while_a_thread_is_open_lays_each_message_out_once()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            w.Press(Key.Q);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter);
            DictateWindowTests.Pump(300);
            Assert.Equal("read", w.screen);
            var (runs, reads) = (w.MarkdownRuns, board.ThreadReads);
            Assert.True(runs > 0);
            for (var i = 0; i < 3; i++)
            {
                board.Push();
                Settle();
            }
            Assert.True(board.ThreadReads >= reads + 3, "each push re-read the thread");
            Assert.Equal(runs, w.MarkdownRuns);
        });
    }

    [Fact]
    public void Dragging_the_window_edge_lays_out_when_it_settles_and_measures_the_glyph_once()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            var (screens, glyphs) = (w.ScreenMeasures, w.GlyphMeasures);
            for (var i = 0; i < 30; i++)
            {
                w.Width = 900 + i * 7;
                DictateWindowTests.Pump(10);
            }
            DictateWindowTests.Pump(300);
            Assert.InRange(w.ScreenMeasures - screens, 1, 8); // 30 resizes, a handful of layouts
            Assert.Equal(glyphs, w.GlyphMeasures); // the glyph width was already known
            var cols = w.cols;
            w.Press(Key.OemPlus, ModifierKeys.Control); // zoom stays immediate and re-measures
            Assert.NotEqual(glyphs, w.GlyphMeasures);
            Assert.NotEqual(cols, w.cols);
        });
    }

    [Fact]
    public void Prs_and_callers_are_built_once_per_status()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            Assert.Same(w.Prs, w.Prs);
            Assert.Same(w.Callers, w.Callers);
            var open = w.Prs;
            w.Press(Key.P);
            w.Press(Key.H); // settled PRs too: a different list
            Assert.True(w.Prs.Count >= open.Count);
            Assert.Same(w.Prs, w.Prs);
        });
    }

    [Fact]
    public void A_footer_wider_than_the_window_drops_hints_from_the_right_and_keeps_Esc_and_F()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            w.Press(Key.A);
            DictateWindowTests.Pump(300);
            for (var i = 0; i < 12 && !Text(w.Hints()).Contains("forget"); i++)
                w.Press(Key.Down);
            var full = w.Hints();
            Assert.Contains("forget", Text(full));
            var text = Text(MainWindow.Shorten(full, 64));
            Assert.True(text.Length <= 64, text);
            Assert.Contains("Esc", text);
            Assert.Contains("forget", text);
            Assert.Equal(full.Count, MainWindow.Shorten(full, 1000).Count);
        });
    }

    [Fact]
    public void Up_and_down_scroll_a_screen_with_no_rows_and_the_cursor_row_stays_in_a_short_window()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            w.Height = w.MinHeight;
            w.Press(Key.S); // SysOp: nothing to move over
            DictateWindowTests.Pump(300);
            Assert.True(w.Body.ExtentHeight > w.Body.ViewportHeight, "the screen runs past the window");
            Assert.Equal(0, w.Body.VerticalOffset);
            w.Press(Key.PageDown);
            Assert.True(w.Body.VerticalOffset > 0);
            w.Press(Key.PageUp);
            Assert.Equal(0, w.Body.VerticalOffset);
            w.Press(Key.Escape);

            w.Press(Key.A); // Agents: rows, a cursor, and a budget box below them
            DictateWindowTests.Pump(300);
            for (var i = 0; i < 30; i++)
                w.Press(Key.Down);
            DictateWindowTests.Pump(300);
            var at = w.clickMap.First(kv => kv.Value == w.Sel).Key;
            var rect = w.Doc.Blocks.ElementAt(at).ContentStart.GetCharacterRect(LogicalDirection.Forward);
            Assert.InRange(rect.Top, 0, w.Body.ViewportHeight);
        });
    }

    /// <summary>Counts what the core is asked, through the real pipe and the real CoreBoard.</summary>
    [Fact]
    public async Task One_refresh_asks_the_core_for_the_discussion_channel_once()
    {
        var asked = new List<string>();
        Environment.SetEnvironmentVariable("AGENTDESK_DATA", Path.GetTempPath());
        Environment.SetEnvironmentVariable("AGENTDESK_PIPE", $"agentdesk-test-{Guid.NewGuid():N}");
        using var stop = new CancellationTokenSource();
        _ = PipeServer.Run((req, _, _) =>
        {
            lock (asked)
                asked.Add(req.Tool + " " + req.Args);
            return Task.FromResult(req.Tool switch
            {
                "ui:threads" => """{"threads": [{"id": 4, "channel": "discussion", "subject": "bio: builder"}]}""",
                "recent_messages" => """{"messages": []}""",
                _ => "{}",
            });
        }, stop.Token);
        using var board = await AgentDesk.App.CoreBoard.Connect();
        var listed = await board.ListThreadsAsync("discussion");
        var status = await board.StatusAsync();
        Assert.Single(listed);
        Assert.Equal(4, status.Bios["builder"]);
        lock (asked)
            Assert.Single(asked, a => a.StartsWith("ui:threads") && a.Contains("discussion"));

        await Task.Delay(3200); // past what a refresh shares: a later status asks again
        await board.StatusAsync();
        lock (asked)
            Assert.Equal(2, asked.Count(a => a.StartsWith("ui:threads") && a.Contains("discussion")));
        stop.Cancel();
    }
}
