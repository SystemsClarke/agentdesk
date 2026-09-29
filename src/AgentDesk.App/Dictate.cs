using System.IO;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Threading;

namespace AgentDesk.App;

/// <summary>Ctrl+D in any text box (the reply box, the subject line, every step of a New agent or New goal dialog): local
/// speech-to-text, typed into the box at the cursor as it is heard. Ctrl+D again stops, Esc cancels. The box is read-only while
/// listening, so nothing typed can be overwritten by the next words.</summary>
public partial class MainWindow
{
    readonly DispatcherTimer dictTimer = new() { Interval = TimeSpan.FromMilliseconds(150) };
    TextBox? dictBox;
    string dictOriginal = "", dictBefore = "", dictAfter = "", dictHeard = "", dictShown = "";
    bool dictStopping, dictPolling;

    internal bool Dictating => dictBox is not null;

    // The pre-roll: while a box has focus (and Options' pre-roll is on) the mic keeps a 2-second rolling buffer in RAM, so
    // Ctrl+D catches the words just before the press. The plugin host exits when idle, so a focused box re-arms every 2 minutes.
    readonly DispatcherTimer micTimer = new() { Interval = TimeSpan.FromMinutes(2) };
    bool micArmed;

    void WireMic()
    {
        foreach (var box in new[] { Reply, Subject })
        {
            box.GotKeyboardFocus += (_, _) => ArmMic();
            box.LostKeyboardFocus += (_, _) => DisarmMic();
        }
        micTimer.Tick += (_, _) => ArmMic();
        Deactivated += (_, _) => DisarmMic();
        Activated += (_, _) => { if (Reply.IsKeyboardFocused || Subject.IsKeyboardFocused) ArmMic(); };
        Closed += (_, _) => DisarmMic();
    }

    internal void ArmMic() => ArmMic(Pref("preroll", true));

    internal void ArmMic(bool on)
    {
        if (!on || Dictating)
            return;
        micArmed = true;
        micTimer.Stop(); // a fresh 2 minutes from this arm
        micTimer.Start();
        _ = Mic("arm");
    }

    internal void DisarmMic()
    {
        if (!micArmed)
            return;
        micArmed = false;
        micTimer.Stop();
        _ = Mic("disarm");
    }

    /// <summary>A missing microphone or a plugin that is down is not worth a message here: Ctrl+D says so when it matters.</summary>
    async Task Mic(string action)
    {
        try
        {
            await board.DictateAsync(action);
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
        }
    }

    internal void ToggleDictation()
    {
        if (Dictating)
            _ = StopDictation();
        else if (Keyboard.FocusedElement is TextBox box)
            _ = StartDictation(box);
        else
            Flash("Put the cursor in a box first, then press Ctrl+D.", "ye");
    }

    internal async Task StartDictation(TextBox box)
    {
        (dictBox, dictOriginal, dictHeard, dictShown, dictStopping) = (box, box.Text, "", "", false);
        dictBefore = box.Text[..box.SelectionStart];
        dictAfter = box.Text[(box.SelectionStart + box.SelectionLength)..];
        if (dictBefore.Length > 0 && !char.IsWhiteSpace(dictBefore[^1]))
            dictBefore += " ";
        box.IsReadOnly = true;
        Say("Listening. Ctrl+D stops, Esc cancels.", "gr b");
        await Talk("start");
        if (Dictating)
            dictTimer.Start();
    }

    internal Task StopDictation()
    {
        if (dictStopping)
            return Task.CompletedTask;
        dictStopping = true;
        return Talk("stop");
    }

    /// <summary>Esc: stop listening and put the box back as it was.</summary>
    internal void CancelDictation()
    {
        if (dictBox is not { } box)
            return;
        dictTimer.Stop();
        (box.IsReadOnly, dictBox) = (false, null);
        box.Text = dictOriginal;
        box.CaretIndex = dictBefore.Length;
        _ = board.DictateAsync("stop").ContinueWith(_ => { }); // the heard words are dropped; a failed stop has nothing left to say
        Flash("Dictation cancelled.", "mu");
    }

    async Task DictationTick()
    {
        if (dictPolling || !Dictating)
            return;
        dictPolling = true;
        try
        {
            await Talk("poll");
        }
        finally
        {
            dictPolling = false;
        }
    }

    async Task Talk(string action)
    {
        try
        {
            Hear(await board.DictateAsync(action));
        }
        catch (Exception e) when (e is InvalidOperationException or IOException)
        {
            Finish("Dictation isn't available: " + e.Message);
        }
    }

    void Hear(DictationState s)
    {
        if (!Dictating)
            return; // cancelled while this answer was on its way
        if (s.Error is { } error)
        {
            Finish("Dictation failed: " + error);
            return;
        }
        switch (s.State)
        {
            case "downloading":
                Say($"Downloading the speech model, once: {(int)(s.Progress * 10) * 10}%.", "ye");
                break;
            case "loading":
                Say("Loading the speech model. Start talking; it is listening.", "mu");
                break;
            case "listening":
                Say(dictStopping ? "Finishing." : "Listening. Ctrl+D stops, Esc cancels.", dictStopping ? "mu" : "gr b");
                break;
        }
        if (s.Text != dictHeard)
        {
            dictHeard = s.Text;
            dictBox!.Text = dictBefore + dictHeard + dictAfter;
            dictBox.CaretIndex = dictBefore.Length + dictHeard.Length;
            dictBox.ScrollToLine(dictBox.GetLineIndexFromCharacterIndex(dictBox.CaretIndex));
        }
        if (s.State == "done")
            Finish(null);
    }

    /// <summary>A status line, spoken by a screen reader once: it repeats only when the words change, not on every poll.</summary>
    void Say(string text, string tags)
    {
        if (text == dictShown)
            return;
        dictShown = text;
        Flash(text, tags);
        flashTimer.Stop(); // stays up until dictation ends
    }

    void Finish(string? problem)
    {
        dictTimer.Stop();
        if (dictBox is { } box)
        {
            box.IsReadOnly = false;
            box.Focus();
            box.CaretIndex = dictBefore.Length + dictHeard.Length;
        }
        dictBox = null;
        if (problem is not null)
            Flash(problem, "pk b");
        else
            Flash(dictHeard.Length > 0 ? "Dictated." : "I didn't catch anything.", dictHeard.Length > 0 ? "gr" : "ye");
    }
}
