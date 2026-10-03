using System.Windows;
using System.Windows.Input;
using System.Windows.Threading;
using AgentDesk.App;

namespace AgentDesk.Tests;

/// <summary>A board that behaves as the sample one does and remembers every action the window asked the core to carry out.</summary>
sealed class RecordingBoard : IBoard
{
    readonly SampleBoard inner = new();
    public List<(string Request, object? Args)> Acts { get; } = [];
    /// <summary>Scripts for the reads a scenario wants to hold back or change; null is the sample board's own answer.</summary>
    public Func<string, Task<GoalDetail?>>? Goal;
    public Func<Task<IReadOnlyList<Identity>>>? Identities;
    EventHandler? pushed;

    public event EventHandler? Changed { add { inner.Changed += value; pushed += value; } remove { inner.Changed -= value; pushed -= value; } }
    /// <summary>The core pushing board.changed with nothing the sample board did itself.</summary>
    public void Push() => pushed?.Invoke(this, EventArgs.Empty);
    /// <summary>How many times each read was asked for.</summary>
    public int ListCalls, ThreadReads, StatusCalls, IdentityCalls, SlotCalls, WebUrlCalls;
    public Task<IReadOnlyList<ThreadRow>> ListThreadsAsync(string channel) { ListCalls++; return inner.ListThreadsAsync(channel); }
    public Task<ThreadDetail?> ReadThreadAsync(int id) { ThreadReads++; return inner.ReadThreadAsync(id); }
    public Task<IReadOnlyList<ThreadRow>> OpenQuestionsAsync() => inner.OpenQuestionsAsync();
    public Task ReplyAsync(int id, string body) => inner.ReplyAsync(id, body);
    public Task<bool> CloseAsync(int id) => inner.CloseAsync(id);
    public Task<bool> UnarchiveAsync(int id) => inner.UnarchiveAsync(id);
    /// <summary>Holds back or counts posts: null is the sample board's own answer.</summary>
    public Func<string, string, string, Task<int>>? Posting;
    public Task<int> PostAsync(string channel, string subject, string body) => Posting?.Invoke(channel, subject, body) ?? inner.PostAsync(channel, subject, body);
    public Task<BoardStatus> StatusAsync() { StatusCalls++; return inner.StatusAsync(); }
    public Task<IReadOnlyList<Identity>> IdentitiesAsync() { IdentityCalls++; return Identities?.Invoke() ?? inner.IdentitiesAsync(); }
    public Task<IReadOnlyList<Adoptable>> AdoptableAsync() => inner.AdoptableAsync();
    public Task<IReadOnlyList<Slot>> SlotsAsync() { SlotCalls++; return inner.SlotsAsync(); }
    public Task<GoalDetail?> GoalAsync(string name) => Goal?.Invoke(name) ?? inner.GoalAsync(name);
    public Task<GoalDetail?> SampleGoal(string name) => inner.GoalAsync(name);
    public IReadOnlyList<Identity> SampleIdentities() => inner.IdentitiesAsync().Result;
    public Task<string?> WebUrlAsync() { WebUrlCalls++; return inner.WebUrlAsync(); }
    /// <summary>Holds every action until it completes, as a core a pipe away does: null answers at once.</summary>
    public Task? Hold;
    public async Task<string?> ActAsync(string request, object? args = null)
    {
        Acts.Add((request, args));
        if (Hold is not null)
            await Hold;
        return await inner.ActAsync(request, args);
    }
    public Task<DictationState> DictateAsync(string action) => inner.DictateAsync(action);
    public Task<IReadOnlyList<FolderPick>> FoldersAsync() => inner.FoldersAsync();
}

/// <summary>Runs a scenario on an STA thread against a shown MainWindow, and hands it keys without a keyboard.</summary>
static class KeyHarness
{
    public static void Run(IBoard board, Action<MainWindow> scenario)
    {
        Exception? failure = null;
        var thread = new Thread(() =>
        {
            try
            {
                _ = Application.Current ?? new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
                SynchronizationContext.SetSynchronizationContext(new DispatcherSynchronizationContext(Dispatcher.CurrentDispatcher)); // what the real message loop gives the window
                var window = new MainWindow(board, null) { hide = () => { } };
                window.Show();
                DictateWindowTests.Pump(200);
                try { scenario(window); }
                finally { window.Close(); }
            }
            catch (Exception e) { failure = e; }
            finally { Dispatcher.CurrentDispatcher.InvokeShutdown(); }
        });
        thread.SetApartmentState(ApartmentState.STA);
        thread.Start();
        thread.Join();
        if (failure is not null) throw new Exception("window scenario failed", failure);
    }

    /// <summary>One key on the screen itself, then a moment for what it started to land.</summary>
    public static bool Press(this MainWindow w, Key key, ModifierKeys mods = ModifierKeys.None)
    {
        var handled = w.ScreenKey(key, mods);
        DictateWindowTests.Pump(100);
        return handled;
    }
}
