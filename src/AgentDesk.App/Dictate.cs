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
