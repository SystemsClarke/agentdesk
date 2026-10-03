using System.Text.Json;
using System.Text.Json.Nodes;
using System.Windows.Documents;
using System.Windows.Input;
using AgentDesk.App;

namespace AgentDesk.Tests;

/// <summary>Keys and clicks act on the row you see, and what you typed is not lost or sent somewhere else.</summary>
[Collection("window")] // one WPF Application per process: window tests run one after another
public sealed class WrongTargetTests
{
    static string Text(MainWindow w) => new TextRange(w.Doc.ContentStart, w.Doc.ContentEnd).Text;
    static string Bar(MainWindow w) => string.Concat(w.BarLine(200).Select(s => s.Text));
    static string Hints(MainWindow w) => string.Concat(w.Hints().Select(s => s.Text));

    /// <summary>A key in a box, as OnKey hands it over.</summary>
    static bool Box(MainWindow w, Key key, bool ctrl = false, bool alt = false)
    {
        var handled = w.BoxKey(key, ctrl, alt);
        DictateWindowTests.Pump(100);
        return handled;
    }

    static void OpenFirstQuestion(MainWindow w)
    {
        w.Press(Key.Q);
        DictateWindowTests.Pump(300);
        w.Press(Key.Enter);
        Assert.Equal("read", w.screen);
    }

    [Fact]
    public void A_reply_draft_stays_with_its_thread_and_is_never_sent_to_the_next_one()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OpenFirstQuestion(w);
            w.Reply.Text = "draft for the first";
            Assert.True(Box(w, Key.N, alt: true));
            Assert.Equal("", w.Reply.Text); // the other thread starts empty, not with the first one's words
            w.Reply.Text = "draft for the second";
            Box(w, Key.Enter, ctrl: true);
            Assert.Contains("Sent to #", Bar(w));
            Assert.Equal("", w.Reply.Text); // sent: gone from the box
            Box(w, Key.P, alt: true);
            Assert.Equal("draft for the first", w.Reply.Text); // and the first one's draft was kept for it

            Box(w, Key.Escape);
            Assert.Equal("list", w.screen);
            w.Press(Key.N);
            Assert.Equal("compose", w.screen);
            Assert.Equal("", w.Reply.Text); // a new post does not start from a reply draft
        });
    }

    [Fact]
    public void Ctrl_Enter_on_another_thread_with_an_empty_box_sends_nothing()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OpenFirstQuestion(w);
            var before = board.ReadThreadAsync(41).Result!.Messages.Count;
            w.Reply.Text = "meant for the first";
            Box(w, Key.N, alt: true);
            Box(w, Key.Enter, ctrl: true);
            Assert.Contains("Nothing to send", Bar(w));
            Assert.Equal(before, board.ReadThreadAsync(41).Result!.Messages.Count);
        });
    }

    [Fact]
    public void Alt_chords_belong_to_the_reader_and_do_nothing_in_compose()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OpenFirstQuestion(w);
            Box(w, Key.Escape);
            w.Press(Key.N);
            Assert.Equal("compose", w.screen);
            w.Reply.Focus();
            w.Reply.Text = "an unsent post";
            Assert.True(w.Reply.IsKeyboardFocused, "the scenario needs the Reply box to have the keyboard");
            foreach (var key in new[] { Key.C, Key.U, Key.N, Key.P })
                Assert.False(Box(w, key, alt: true)); // not claimed: they are not the reader's
            Assert.Equal("compose", w.screen);
            Assert.DoesNotContain("yes", Hints(w)); // no Close & archive confirm for the thread read last
            Assert.Equal("an unsent post", w.Reply.Text);
        });
    }

    [Fact]
    public void An_unsent_post_survives_a_summon_and_page_does_not_overwrite_it()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.D);
            w.Press(Key.N);
            Assert.Equal("compose", w.screen);
            (w.Subject.Text, w.Reply.Text) = ("half a subject", "half a body");
            w.Summon(38);
            Assert.Equal("read", w.screen);
            Assert.Equal("", w.Reply.Text); // the summoned thread has no draft of this post's
            Box(w, Key.Escape);
            w.Press(Key.B);
            DictateWindowTests.Pump(100);
            w.Press(Key.P);
            Assert.Equal("compose", w.screen);
            Assert.Equal(("half a subject", "half a body"), (w.Subject.Text, w.Reply.Text));
            Assert.Contains("unsent post", Bar(w));
        });
    }

    [Fact]
    public void Alt_U_is_offered_only_for_an_archived_thread()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OpenFirstQuestion(w);
            Assert.DoesNotContain("Alt+U", Hints(w)); // a live question has nothing to bring back
            Assert.True(w.Press(Key.U));
            Assert.Contains("Nothing to bring back", Bar(w));
            w.Press(Key.Escape);
            w.Press(Key.H); // the archive
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter);
            Assert.Equal("read", w.screen);
            Assert.Contains("Alt+U", Hints(w));
            w.Press(Key.U);
            DictateWindowTests.Pump(300);
            Assert.Contains("is back on the desk", Bar(w));
        });
    }

    [Fact]
    public void Closing_a_thread_that_is_not_an_open_question_says_there_was_nothing_to_do()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            w.Press(Key.Q);
            w.Press(Key.H);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter); // #30 is archived
            w.Press(Key.C);
            w.Press(Key.Y);
            DictateWindowTests.Pump(300);
            Assert.Contains("Nothing to close", Bar(w));
        });
    }

    [Fact]
    public void A_goal_detail_that_arrives_late_does_not_land_on_another_goal()
    {
        var board = new RecordingBoard();
        var slow = new TaskCompletionSource<GoalDetail?>();
        var slowToo = new TaskCompletionSource<GoalDetail?>();
        board.Goal = name => name == "build-speed" ? slow.Task : slowToo.Task;
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.A);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter); // build-speed, whose log is slow
            Assert.Equal("goal", w.screen);
            w.Press(Key.Escape);
            w.Press(Key.Down);
            w.Press(Key.Enter); // concierge, while the first read is still out
            Assert.Equal("goal", w.screen);
            slow.SetResult(board.SampleGoal("build-speed").Result);
            DictateWindowTests.Pump(500);
            Assert.DoesNotContain("Goal build-speed", Text(w)); // the answer was for the goal the window had left
            slowToo.SetResult(board.SampleGoal("concierge").Result);
            DictateWindowTests.Pump(500);
            Assert.Contains("Goal concierge", Text(w));
        });
    }

    [Fact]
    public void On_a_goal_whose_log_has_not_arrived_A_is_not_adopt_and_plus_changes_nothing()
    {
        var board = new RecordingBoard { Goal = _ => new TaskCompletionSource<GoalDetail?>().Task };
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.A);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter);
            Assert.Equal("goal", w.screen);
            foreach (var key in new[] { Key.A, Key.X, Key.L, Key.OemPlus, Key.OemMinus })
            {
                Assert.True(w.Press(key));
                Assert.Equal("goal", w.screen);
                Assert.Contains("Still reading", Bar(w));
            }
            Assert.Empty(board.Acts);
        });
    }

    [Fact]
    public void The_Agents_cursor_stays_on_the_agent_it_was_on_when_the_list_reorders()
    {
        var board = new RecordingBoard();
        var identities = board.SampleIdentities();
        board.Identities = () => Task.FromResult<IReadOnlyList<Identity>>(identities);
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.A);
            DictateWindowTests.Pump(300);
            for (var i = 0; i < 30 && !Text(w).Contains("▶builder"); i++)
                w.Press(Key.Down);
            Assert.Contains("▶builder", Text(w));
            // app-dev, which sits above builder, signs off: every row below it moves up one.
            identities = [.. identities.Where(i => i.Name != "app-dev")];
            board.Push();
            DictateWindowTests.Pump(1500);
            Assert.Contains("▶builder", Text(w));
            w.Press(Key.S);
            DictateWindowTests.Pump(300);
            Assert.Contains(board.Acts, a => a.Request == "ui:identity_stop" && JsonSerializer.Serialize(a.Args).Contains("\"builder\""));
        });
    }

    [Fact]
    public void A_new_agent_is_selected_by_name_not_by_its_place_in_the_agents_list()
    {
        var board = new RecordingBoard();
        var identities = board.SampleIdentities();
        board.Identities = () => Task.FromResult<IReadOnlyList<Identity>>(identities);
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.A);
            DictateWindowTests.Pump(300);
            identities = [.. identities, new Identity("aaa-new", "stopped", 1, "windows", @"C:\src")];
            w.NewAgent();
            w.Subject.Text = "aaa-new";
            _ = w.AskNext();
            DictateWindowTests.Pump(200);
            w.Subject.Text = Environment.CurrentDirectory;
            _ = w.AskNext();
            DictateWindowTests.Pump(200);
            w.Subject.Text = "";
            _ = w.AskNext();
            DictateWindowTests.Pump(600);
            Assert.Equal("agents", w.screen);
            Assert.Contains("▶aaa-new", Text(w));
        });
    }

    [Fact]
    public void Who_s_on_rows_click_to_the_caller_they_show()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.B);
            DictateWindowTests.Pump(300);
            var rows = w.clickMap.OrderBy(k => k.Key).Select(k => k.Value).ToList();
            Assert.True(rows.Count > 1);
            Assert.Equal(Enumerable.Range(0, rows.Count), rows); // the SysOp line shifts every row; none may take another's place
        });
    }

    [Fact]
    public void A_setting_is_merged_into_the_file_as_it_is_now_so_the_cores_keys_survive()
    {
        var dir = Path.Combine(Path.GetTempPath(), "adk-prefs-" + Guid.NewGuid().ToString("N"));
        var file = Path.Combine(dir, "settings.json");
        try
        {
            MainWindow.WritePref(file, "theme", JsonValue.Create("grayscale")); // creates the folder and the file
            File.WriteAllText(file, """{"theme":"grayscale","governor_enforce":true}"""); // the core edits its own key meanwhile
            MainWindow.WritePref(file, "font_size", JsonValue.Create(14));
            var now = JsonNode.Parse(File.ReadAllText(file))!;
            Assert.Equal(true, (bool?)now["governor_enforce"]);
            Assert.Equal(14, (int?)now["font_size"]);
            Assert.Equal("grayscale", (string?)now["theme"]);
            Assert.False(File.Exists(file + ".tmp"));
        }
        finally
        {
            if (Directory.Exists(dir))
                Directory.Delete(dir, true);
        }
    }
}
