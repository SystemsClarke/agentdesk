using System.Windows;
using System.Windows.Threading;
using AgentDesk.App;

namespace AgentDesk.Tests;

/// <summary>Ctrl+D in the real window, on a scripted board: words appear in the box at the cursor as they are heard, the box is
/// read-only meanwhile, a second press keeps them, Esc restores the box, and a failure leaves it as it was.</summary>
public sealed class DictateWindowTests
{
    static void Pump(int ms)
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
        _ = Application.Current ?? new Application();
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
    }
}
