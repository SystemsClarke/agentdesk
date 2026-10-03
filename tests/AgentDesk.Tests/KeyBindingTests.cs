using System.Text.Json;
using System.Windows.Documents;
using System.Windows.Input;
using AgentDesk.App;

namespace AgentDesk.Tests;

/// <summary>Every advertised key does what it says, and no key changes anything by accident.</summary>
[Collection("window")] // one WPF Application per process: window tests run one after another
public sealed class KeyBindingTests
{
    static string Bar(MainWindow w) => string.Concat(w.BarLine(200).Select(s => s.Text));
    static string Hints(MainWindow w) => string.Concat(w.Hints().Select(s => s.Text));
    static string Text(MainWindow w) => new TextRange(w.Doc.ContentStart, w.Doc.ContentEnd).Text;
    static string Args(object? a) => JsonSerializer.Serialize(a);

    static bool Box(MainWindow w, Key key, bool ctrl = false, bool alt = false, bool shift = false)
    {
        var handled = w.BoxKey(key, ctrl, alt, shift);
        DictateWindowTests.Pump(100);
        return handled;
    }

    /// <summary>The Agents screen, with the cursor on the first goal row.</summary>
    static void OnGoalRow(MainWindow w)
    {
        DictateWindowTests.Pump(300);
        w.Press(Key.A);
        DictateWindowTests.Pump(300);
        Assert.Equal("agents", w.screen);
    }

    [Fact]
    public void A_modifier_key_does_not_answer_a_question_and_Ctrl_W_and_R_wait_for_it()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OnGoalRow(w);
            w.Press(Key.X);
            Assert.Contains("Stop build-speed?", Hints(w));
            foreach (var key in new[] { Key.LeftShift, Key.RightShift, Key.LeftCtrl, Key.LeftAlt, Key.LWin, Key.CapsLock })
                w.Press(key, key is Key.LeftShift or Key.RightShift ? ModifierKeys.Shift : ModifierKeys.None);
            Assert.Contains("Stop build-speed?", Hints(w)); // still asking
            Assert.True(w.Press(Key.W, ModifierKeys.Control));
            Assert.True(w.Press(Key.R, ModifierKeys.Control));
            Assert.Contains("Stop build-speed?", Hints(w));
            Assert.Empty(board.Acts); // neither the Concierge nor a wake ran under the question
            w.Press(Key.N);
            Assert.Contains("Never mind", Bar(w));
            w.Press(Key.X);
            w.Press(Key.Y);
            DictateWindowTests.Pump(300);
            Assert.Contains(board.Acts, a => a.Request == "ui:goal_stop" && Args(a.Args).Contains("build-speed"));
        });
    }

    [Fact]
    public void Alt_chords_that_are_not_ours_go_to_the_system_but_the_readers_stay()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            Assert.False(w.ScreenKey(Key.Space, ModifierKeys.Alt)); // the system menu
            Assert.False(w.ScreenKey(Key.F4, ModifierKeys.Alt));
            Assert.False(w.ScreenKey(Key.G, ModifierKeys.Alt));
            Assert.Equal("main", w.screen);
            w.Press(Key.Q);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter);
            Assert.Equal("read", w.screen);
            Assert.True(w.ScreenKey(Key.N, ModifierKeys.Alt)); // Alt+N still steps the reader
        });
    }

    [Fact]
    public void A_held_key_acts_once_where_acting_twice_does_harm()
    {
        Assert.True(MainWindow.HeldKeyIgnored(Key.Enter, ctrl: false, inBox: false, inSubject: false)); // a row's Enter
        Assert.True(MainWindow.HeldKeyIgnored(Key.Space, ctrl: false, inBox: false, inSubject: false));
        Assert.True(MainWindow.HeldKeyIgnored(Key.Enter, ctrl: true, inBox: true, inSubject: false)); // Ctrl+Enter sends
        Assert.True(MainWindow.HeldKeyIgnored(Key.Enter, ctrl: false, inBox: true, inSubject: true)); // the subject's Enter moves on
        Assert.False(MainWindow.HeldKeyIgnored(Key.Enter, ctrl: false, inBox: true, inSubject: false)); // a new line in the body
        Assert.False(MainWindow.HeldKeyIgnored(Key.Space, ctrl: false, inBox: true, inSubject: false)); // typing
        Assert.False(MainWindow.HeldKeyIgnored(Key.Down, ctrl: false, inBox: false, inSubject: false)); // scrolling relies on repeat
        Assert.False(MainWindow.HeldKeyIgnored(Key.PageDown, ctrl: false, inBox: false, inSubject: false));
    }

    [Fact]
    public void Ctrl_Enter_twice_while_a_post_is_in_flight_posts_once()
    {
        var board = new RecordingBoard();
        var posts = 0;
        var release = new TaskCompletionSource<int>();
        board.Posting = (_, _, _) =>
        {
            posts++;
            return release.Task;
        };
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.D);
            w.Press(Key.N);
            Assert.Equal("compose", w.screen);
            w.Reply.Text = "one body";
            w.BoxKey(Key.Enter, true, false);
            w.BoxKey(Key.Enter, true, false);
            DictateWindowTests.Pump(100);
            Assert.Equal(1, posts);
            release.SetResult(900);
            DictateWindowTests.Pump(300);
            Assert.Equal(1, posts);
        });
    }

    [Fact]
    public void An_empty_subject_posts_as_no_subject_and_an_empty_body_posts_nothing()
    {
        var board = new RecordingBoard();
        var sent = new List<string>();
        board.Posting = (_, subject, _) =>
        {
            sent.Add(subject);
            return Task.FromResult(901);
        };
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.D);
            w.Press(Key.N);
            Box(w, Key.Enter, ctrl: true);
            Assert.Contains("Nothing to send", Bar(w));
            Assert.Empty(sent);
            w.Reply.Text = "just a body";
            Box(w, Key.Enter, ctrl: true);
            Assert.Equal(["(no subject)"], sent);
        });
    }

    [Fact]
    public void G_hangs_up_only_on_the_main_menu()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            var hidden = 0;
            w.hide = () => hidden++;
            DictateWindowTests.Pump(300);
            foreach (var screen in new[] { Key.Q, Key.P, Key.S, Key.B, Key.O })
            {
                w.Press(screen);
                w.Press(Key.G);
                DictateWindowTests.Pump(500);
                Assert.Equal(0, hidden);
                w.Press(Key.M);
                Assert.Equal("main", w.screen);
            }
            w.Press(Key.G);
            DictateWindowTests.Pump(600);
            Assert.Equal(1, hidden);
        });
    }

    [Fact]
    public void Ctrl_W_asks_before_it_turns_the_Concierge_off_and_the_Options_row_ignores_the_arrows()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300); // the sample board has the Concierge on
            Assert.True(w.Press(Key.W, ModifierKeys.Control));
            Assert.Contains("Turn the Concierge off?", Hints(w));
            Assert.Empty(board.Acts);
            w.Press(Key.N);
            Assert.Empty(board.Acts);
            w.Press(Key.W, ModifierKeys.Control);
            w.Press(Key.Y);
            DictateWindowTests.Pump(600);
            Assert.Contains(board.Acts, a => a.Request == "ui:concierge" && Args(a.Args).Contains("false"));

            board.Acts.Clear();
            w.Press(Key.O);
            w.Press(Key.Down); // Concierge is the second row
            w.Press(Key.Right);
            w.Press(Key.Left);
            Assert.DoesNotContain("Turn the Concierge off?", Hints(w));
            Assert.Empty(board.Acts);
            w.Press(Key.Enter);
            Assert.Contains("Turn the Concierge off?", Hints(w));
        });
    }

    [Fact]
    public void Ctrl_W_in_a_text_box_does_nothing()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.D);
            w.Press(Key.N);
            Assert.False(Box(w, Key.W, ctrl: true));
            Assert.DoesNotContain("Concierge", Hints(w));
            Assert.Empty(board.Acts);
        });
    }

    [Fact]
    public void Esc_on_a_post_with_words_needs_a_second_Esc_and_a_page_goes_back_to_Whos_on()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.D);
            w.Press(Key.N);
            Box(w, Key.Escape); // nothing typed: straight back
            Assert.Equal("list", w.screen);
            w.Press(Key.N);
            w.Subject.Text = "half a thought";
            Box(w, Key.Escape);
            Assert.Equal("compose", w.screen);
            Assert.Contains("Esc again", Bar(w));
            Assert.Equal("half a thought", w.Subject.Text);
            w.Subject.Text = "half a thought, and more"; // typing since: the next Esc asks again
            Box(w, Key.Escape);
            Assert.Equal("compose", w.screen);
            Box(w, Key.Escape);
            Assert.Equal("list", w.screen);
            Assert.Equal("", w.Subject.Text);

            w.Press(Key.B);
            DictateWindowTests.Pump(300);
            w.Press(Key.P);
            Assert.Equal("compose", w.screen);
            Assert.Contains("twice", Hints(w));
            Box(w, Key.Escape);
            Box(w, Key.Escape);
            Assert.Equal("who", w.screen);
        });
    }

    [Fact]
    public void Tab_in_the_reply_box_keeps_the_keyboard_and_a_key_on_the_screen_returns_to_the_box()
    {
        KeyHarness.Run(new RecordingBoard(), w =>
        {
            w.Press(Key.Q);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter);
            Assert.True(w.Reply.IsKeyboardFocused);
            Assert.True(Box(w, Key.Tab));
            Assert.True(w.Reply.IsKeyboardFocused);

            Box(w, Key.Escape);
            w.Press(Key.N);
            Assert.Equal("compose", w.screen);
            w.Reply.Focus();
            Assert.True(Box(w, Key.Tab, shift: true));
            Assert.True(w.Subject.IsKeyboardFocused); // Shift+Tab goes back to the subject
            w.Subject.Text = "a subject";
            w.Body.Focus(); // a click on the screen took the keyboard
            w.Press(Key.Enter);
            Assert.True(w.Reply.IsKeyboardFocused); // not a menu letter, not lost
            Assert.Equal("compose", w.screen);
        });
    }

    [Fact]
    public void Agent_keys_on_a_goal_row_and_goal_keys_on_an_agent_row_say_why_they_do_nothing()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OnGoalRow(w);
            Assert.Contains("members", Hints(w));
            Assert.DoesNotContain("forget", Hints(w));
            foreach (var (key, said) in new[] { (Key.F, "F forgets an agent"), (Key.S, "S starts or stops an agent") })
            {
                w.Press(key);
                Assert.Contains(said, Bar(w));
            }
            w.Press(Key.A); // build-speed is running
            Assert.Contains("already running", Bar(w));
            Assert.Empty(board.Acts);

            w.Press(Key.End); // an agent's row
            Assert.Contains("forget", Hints(w));
            Assert.DoesNotContain("stop goal", Hints(w));
            foreach (var (key, said) in new[] { (Key.X, "X stops a goal"), (Key.L, "L attaches to a goal's lead"), (Key.OemPlus, "set a goal's members") })
            {
                w.Press(key);
                Assert.Contains(said, Bar(w));
            }
            Assert.Empty(board.Acts);
        });
    }

    [Fact]
    public void Plus_and_minus_move_a_goals_members_by_one_and_Shift_by_ten_with_no_ceiling_and_once_per_press()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OnGoalRow(w); // build-speed: 3 members at most
            w.Press(Key.OemPlus);
            w.Press(Key.Add);
            w.Press(Key.OemPlus, ModifierKeys.Shift);
            w.Press(Key.OemMinus, ModifierKeys.Shift);
            w.Press(Key.Subtract);
            var wanted = board.Acts.Where(a => a.Request == "ui:goal_budget")
                .Select(a => JsonSerializer.Deserialize<JsonElement>(Args(a.Args)).GetProperty("max_members").GetInt32()).ToList();
            Assert.Equal([4, 4, 13, 1, 2], wanted);

            board.Acts.Clear();
            var release = new TaskCompletionSource();
            board.Hold = release.Task;
            w.ScreenKey(Key.OemPlus, ModifierKeys.None); // two before the first has landed
            w.ScreenKey(Key.OemPlus, ModifierKeys.None);
            release.SetResult();
            DictateWindowTests.Pump(300);
            Assert.Single(board.Acts, a => a.Request == "ui:goal_budget");
        });
    }

    [Fact]
    public void S_on_a_running_agent_is_not_sent_twice_by_a_held_key()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            OnGoalRow(w);
            w.Press(Key.End);
            var release = new TaskCompletionSource();
            board.Hold = release.Task;
            w.ScreenKey(Key.S, ModifierKeys.None);
            w.ScreenKey(Key.S, ModifierKeys.None);
            release.SetResult();
            DictateWindowTests.Pump(300);
            Assert.Single(board.Acts, a => a.Request is "ui:identity_stop" or "ui:identity_start");
        });
    }

    [Fact]
    public void The_footers_offer_only_keys_that_work_on_that_screen()
    {
        var board = new RecordingBoard();
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            Assert.Contains("[Q,D,W,J,P,S,B,O,A,G]", Text(w)); // the menu prompt no longer lists an E that does nothing
            w.Press(Key.S);
            Assert.DoesNotContain("reload", Hints(w));
            w.Press(Key.R);
            Assert.DoesNotContain("Nothing to reload", Bar(w)); // R is nothing here, and says nothing
            Assert.Empty(board.Acts);
            w.Press(Key.M);
            w.Press(Key.P);
            Assert.Contains("↵ O", Hints(w)); // the alias is on the footer
        });
    }
}
