using System.Windows;
using System.Windows.Threading;
using AgentDesk.App;

namespace AgentDesk.Tests;

/// <summary>Ctrl+D in the real window, on a scripted board: words appear in the box at the cursor as they are heard, the box is
/// read-only meanwhile, a second press keeps them, Esc restores the box, and a failure leaves it as it was.</summary>
public sealed class DictateWindowTests
{
    internal static void Pump(int ms)
    {
        var frame = new DispatcherFrame();
        var t = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(ms) };
        t.Tick += (_, _) => { t.Stop(); frame.Continue = false; };
        t.Start();
        Dispatcher.PushFrame(frame);
    }

    static void Until(Func<bool> done, string what)
    {
        for (var i = 0; i < 100 && !done(); i++) Pump(50);
        Assert.True(done(), what);
    }

    [Fact]
    public void Dictation_types_into_the_box_and_ends_on_the_second_press_escape_or_an_error()
    {
        Exception? failure = null;
        var thread = new Thread(() =>
        {
            try { Scenario(); }
            catch (Exception e) { failure = e; }
            finally { Dispatcher.CurrentDispatcher.InvokeShutdown(); }
        });
        thread.SetApartmentState(ApartmentState.STA);
        thread.Start();
        thread.Join();
        if (failure is not null) throw new Exception("window scenario failed", failure);
    }

    static void Scenario()
    {
        _ = Application.Current ?? new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown }; // closing a window must not end the app
        var heard = "";
        var done = false;
        var board = new SampleBoard { Dictate = action => new(done ? "done" : action == "start" ? "loading" : "listening", heard, null, 0) };
        var window = new MainWindow(board, null);
        window.Show();
        Pump(200);
        var box = window.Reply;

        box.Text = "existing";
        box.SelectionStart = box.Text.Length;
        _ = window.StartDictation(box);
        Until(() => window.Dictating && box.IsReadOnly, "the box goes read-only while listening");
        heard = "hello";
        Until(() => box.Text == "existing hello", "the first words land after the cursor, with a space");
        heard = "hello world";
        Until(() => box.Text == "existing hello world", "the words are replaced by the longer transcript, not appended twice");

        (heard, done) = ("hello world today", true); // the plugin finishes after stop
        _ = window.StopDictation();
        Until(() => !window.Dictating, "the second press ends it");
        Assert.Equal("existing hello world today", box.Text);
        Assert.False(box.IsReadOnly);

        (heard, done) = ("", false); // Esc: the box goes back
        box.Text = "keep me";
        box.SelectionStart = 0;
        box.SelectionLength = 0;
        _ = window.StartDictation(box);
        heard = "dropped";
        Until(() => box.Text.Contains("dropped"), "heard words appear");
        window.CancelDictation();
        Assert.Equal("keep me", box.Text);
        Assert.False(box.IsReadOnly);
        Assert.False(window.Dictating);

        var broken = new SampleBoard { Dictate = _ => new("done", "", "PortAudioError('no microphone')", 0) }; // a failure: the box is left alone
        window.Close();
        var window2 = new MainWindow(broken, null);
        window2.Show();
        Pump(200);
        window2.Reply.Text = "untouched";
        window2.Reply.SelectionStart = 9;
        _ = window2.StartDictation(window2.Reply);
        Until(() => !window2.Dictating, "an error ends dictation");
        Assert.Equal("untouched", window2.Reply.Text);
        Assert.False(window2.Reply.IsReadOnly);
        window2.Close();

        // The pre-roll: a focused box arms the mic once, losing focus lets it go, and with the option off nothing is armed.
        var actions = new List<string>();
        var window4 = new MainWindow(new SampleBoard { Dictate = a => { lock (actions) actions.Add(a); return new("idle", "", null, 0); } }, null);
        window4.Show();
        Pump(200);
        window4.ArmMic(false);
        window4.DisarmMic();
        Pump(100);
        lock (actions) Assert.Empty(actions);
        window4.ArmMic(true);
        window4.ArmMic(true); // the 2-minute heartbeat: the plugin's arm is idempotent
        window4.DisarmMic();
        window4.DisarmMic(); // nothing armed, nothing to let go of
        Until(() => { lock (actions) return actions.Count == 3; }, "arm, arm again, disarm");
        lock (actions) Assert.Equal(["arm", "arm", "disarm"], actions);
        window4.Close();

        // New agent: the folder step lists what he uses (the sample board's two), ↓ moves the pick, Tab copies it into the box,
        // and typing narrows the list. (One Application per process, so this shares the dictation scenario's thread.)
        var window3 = new MainWindow(new SampleBoard(), null);
        window3.Show();
        Pump(200);
        window3.NewAgent();
        Assert.False(window3.OnFolderStep); // the name comes first
        window3.Subject.Text = "alpha";
        _ = window3.AskNext();
        Until(() => window3.OnFolderStep && window3.folderPicks.Count == 2, "the folder step shows the suggestions");
        var shown = string.Concat(window3.FolderLines(120).SelectMany(l => l).Select(s => s.Text));
        Assert.Contains("agentdesk-terminal", shown);
        Assert.Contains("fastbuild", shown);
        Assert.True(window3.FolderKey(System.Windows.Input.Key.Down));
        Assert.Equal(1, window3.folderSel);
        Assert.True(window3.FolderKey(System.Windows.Input.Key.Tab));
        Assert.Equal(window3.folderPicks[1].Path, window3.Subject.Text);
        window3.Subject.Text = "agent";
        Until(() => !string.Concat(window3.FolderLines(120).SelectMany(l => l).Select(s => s.Text)).Contains("fastbuild"), "typing narrows the list");
        Assert.Equal(0, window3.folderSel);
        window3.Close();

        // Agents & goals is one list: goals first, then the agents that are not a goal's lead.
        var window5 = new MainWindow(new SampleBoard(), null);
        window5.Show();
        Pump(300);
        var list = window5.AgentsScreen(110).Select(l => string.Concat(l.Select(s => s.Text))).ToList();
        window5.Close();
        Assert.Contains(list, l => l.Contains("GOAL") && l.Contains("LEAD"));
        Assert.Contains(list, l => l.Contains("AGENT") && l.Contains("MODEL"));
        Assert.Contains(list, l => l.Contains("build-speed") && l.Contains("running"));
        Assert.Contains(list, l => l.Contains("app-dev") && l.Contains("sonnet"));
        Assert.DoesNotContain(list, l => l.TrimStart().StartsWith("build-speed-lead") && l.Contains("windows")); // its lead is on the goal's row
    }
}
