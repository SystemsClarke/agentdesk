using System.IO.Pipes;
using System.Text.Json;
using System.Windows;
using System.Windows.Documents;
using System.Windows.Input;
using AgentDesk.App;
using AgentDesk.Contracts;
using AgentDesk.Core.Host;

namespace AgentDesk.Tests;

/// <summary>A board on the sample data whose reads and writes fail on demand, as a core that is down, restarting or answering badly does.</summary>
sealed class FaultyBoard : IBoard
{
    readonly SampleBoard inner = new();
    public Func<Exception?> Reads = () => null, Writes = () => null, Status = () => null;
    public Func<int, Task<ThreadDetail?>>? Thread;
    public int ThreadCalls;

    public event EventHandler? Changed { add => inner.Changed += value; remove => inner.Changed -= value; }
    async Task<T> Read<T>(Task<T> ok) => Reads() is { } e ? throw e : await ok;
    async Task Write(Task ok) { if (Writes() is { } e) throw e; await ok; }
    async Task<T> Write<T>(Task<T> ok) => Writes() is { } e ? throw e : await ok;
    public Task<IReadOnlyList<ThreadRow>> ListThreadsAsync(string channel) => Read(inner.ListThreadsAsync(channel));
    public Task<ThreadDetail?> ReadThreadAsync(int id) { ThreadCalls++; return Thread is { } t ? t(id) : inner.ReadThreadAsync(id); }
    public Task<IReadOnlyList<ThreadRow>> OpenQuestionsAsync() => Read(inner.OpenQuestionsAsync());
    public Task ReplyAsync(int id, string body) => Write(inner.ReplyAsync(id, body));
    public Task<bool> CloseAsync(int id) => Write(inner.CloseAsync(id));
    public Task<bool> UnarchiveAsync(int id) => Write(inner.UnarchiveAsync(id));
    public Task<int> PostAsync(string channel, string subject, string body) => inner.PostAsync(channel, subject, body);
    public async Task<BoardStatus> StatusAsync() => Status() is { } e ? throw e : await inner.StatusAsync();
    public Task<IReadOnlyList<Identity>> IdentitiesAsync() => inner.IdentitiesAsync();
    public Task<IReadOnlyList<Adoptable>> AdoptableAsync() => inner.AdoptableAsync();
    public Task<IReadOnlyList<Slot>> SlotsAsync() => inner.SlotsAsync();
    public Task<GoalDetail?> GoalAsync(string name) => inner.GoalAsync(name);
    public Task<string?> WebUrlAsync() => inner.WebUrlAsync();
    public Task<string?> ActAsync(string request, object? args = null) => Writes() is { } e ? throw e : inner.ActAsync(request, args);
    public Task<DictationState> DictateAsync(string action) => inner.DictateAsync(action);
    public Task<IReadOnlyList<FolderPick>> FoldersAsync() => inner.FoldersAsync();
}

/// <summary>The window and its connection survive a core that is down, restarting or answering badly.</summary>
[Collection("window")] // one WPF Application per process: window tests run one after another
public sealed class CoreDownTests
{
    static string Text(MainWindow w) => new TextRange(w.Doc.ContentStart, w.Doc.ContentEnd).Text;
    static string Bar(MainWindow w) => string.Concat(w.BarLine(200).Select(s => s.Text));

    [Fact]
    public void A_core_that_is_down_at_launch_leaves_the_window_focused_and_working_with_one_quiet_line()
    {
        var board = new FaultyBoard { Reads = () => new IOException("core restarted; try again") };
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            Assert.Contains("Starting the AgentDesk core", Bar(w)); // never read yet: a core that is starting, not one that went away
            Assert.DoesNotContain("Something broke", Bar(w));
            Assert.Same(w.Body, FocusManager.GetFocusedElement(w)); // the first refresh failed and the window still took focus
            Assert.True(w.Press(Key.P));
            Assert.Equal("prs", w.screen);
        });
    }

    [Fact]
    public void A_failing_optional_read_keeps_the_thread_lists_painting()
    {
        var board = new FaultyBoard { Status = () => new InvalidOperationException("internal error") };
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.Q);
            DictateWindowTests.Pump(300);
            Assert.Contains("gocd-agent-docker", Text(w));
            Assert.DoesNotContain("Something broke", Bar(w));
            Assert.DoesNotContain("isn't answering", Bar(w));
        });
    }

    [Fact]
    public void A_thread_that_fails_to_load_says_so_and_asks_once_while_it_is_pending()
    {
        var board = new FaultyBoard { Thread = async _ => { await Task.Delay(400); throw new IOException("core restarted"); } };
        KeyHarness.Run(board, w =>
        {
            w.Press(Key.Q);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter);
            DictateWindowTests.Pump(150);
            Assert.Equal("read", w.screen);
            Assert.Contains("Loading", Text(w)); // not "no longer on the board" while the call is out
            Assert.Equal(1, board.ThreadCalls);
            DictateWindowTests.Pump(600);
            Assert.Contains("Couldn't read", Bar(w));
        });
    }

    [Fact]
    public void A_write_that_fails_flashes_the_reason_whatever_the_exception()
    {
        var board = new FaultyBoard();
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            board.Writes = () => new KeyNotFoundException("boom"); // not one the old filters knew
            w.Press(Key.P);
            w.Press(Key.C);
            DictateWindowTests.Pump(200);
            Assert.Contains("Not checked: boom", Bar(w)); // check_prs is no longer fire-and-forget

            w.Press(Key.Q);
            DictateWindowTests.Pump(300);
            w.Press(Key.Enter);
            DictateWindowTests.Pump(300);
            w.Press(Key.C); // close & archive
            w.Press(Key.Y);
            DictateWindowTests.Pump(300);
            Assert.Contains("Not closed: boom", Bar(w));
        });
    }

    [Fact]
    public void Forgetting_an_agent_that_fails_flashes_the_reason()
    {
        var board = new FaultyBoard();
        KeyHarness.Run(board, w =>
        {
            DictateWindowTests.Pump(300);
            w.Press(Key.A);
            DictateWindowTests.Pump(300);
            board.Writes = () => new TimeoutException("boom");
            for (var i = 0; i < 12 && !Bar(w).Contains("Forget"); i++) // F means nothing on a goal's row: walk down to an agent
            {
                w.Press(Key.F);
                if (!Bar(w).Contains("Forget"))
                    w.Press(Key.Down);
            }
            Assert.Contains("Forget", Bar(w));
            w.Press(Key.Y);
            DictateWindowTests.Pump(300);
            Assert.Contains("Not forgotten: boom", Bar(w));
        });
    }

    /// <summary>The reader task used to catch only IOException: a frame that was not JSON faulted it silently, and every call after
    /// waited forever. Both a killed pipe and a garbled frame must fail the call in flight, and the next call must reconnect.</summary>
    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task A_pipe_that_dies_or_garbles_mid_call_fails_that_call_and_the_next_one_reconnects(bool garble)
    {
        var name = $"agentdesk-test-{Guid.NewGuid():N}";
        Environment.SetEnvironmentVariable("AGENTDESK_DATA", Path.GetTempPath()); // were the core ever auto-started, not the live board
        Environment.SetEnvironmentVariable("AGENTDESK_PIPE", name);
        using var first = new NamedPipeServerStream(name, PipeDirection.InOut, 2, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        var connecting = CoreConnection.Connect(new Caller(null, null, null, "ui", Environment.ProcessId));
        await first.WaitForConnectionAsync();
        using var core = await connecting;
        using var second = new NamedPipeServerStream(name, PipeDirection.InOut, 2, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);

        var call = core.Call("x");
        var reader = new StreamReader(first);
        await reader.ReadLineAsync(); // the request arrived
        if (garble)
        {
            var w = new StreamWriter(first) { AutoFlush = true };
            await w.WriteLineAsync("this is not json");
        }
        else
            first.Dispose(); // the core was killed with the call in flight
        await Assert.ThrowsAsync<IOException>(() => call.WaitAsync(TimeSpan.FromSeconds(5)));

        var serve = Task.Run(async () =>
        {
            await second.WaitForConnectionAsync();
            var line = await new StreamReader(second).ReadLineAsync();
            var id = JsonDocument.Parse(line!).RootElement.GetProperty("id").GetInt32();
            await new StreamWriter(second) { AutoFlush = true }.WriteLineAsync($$"""{"id":{{id}},"text":"two"}""");
        });
        Assert.Equal("two", await core.Call("x").WaitAsync(TimeSpan.FromSeconds(5)));
        await serve;
    }

    static async Task<AgentDesk.App.CoreBoard> BoardAnswering(Func<string, string> answer, CancellationTokenSource stop)
    {
        Environment.SetEnvironmentVariable("AGENTDESK_DATA", Path.GetTempPath());
        Environment.SetEnvironmentVariable("AGENTDESK_PIPE", $"agentdesk-test-{Guid.NewGuid():N}");
        _ = PipeServer.Run((req, _, _) => Task.FromResult(req.Tool == "ui:subscribe" ? """{"ok": true}""" : answer(req.Tool)), stop.Token);
        return await AgentDesk.App.CoreBoard.Connect();
    }

    [Fact]
    public async Task Only_no_such_thread_and_no_such_goal_mean_missing_any_other_core_error_is_a_failure()
    {
        using var stop = new CancellationTokenSource();
        using var board = await BoardAnswering(tool => tool switch
        {
            "ui:thread" => """{"error": "no such thread: 9"}""",
            "ui:goal_status" => """{"error": "no such goal: g"}""",
            _ => """{"error": "database error: database is locked"}""",
        }, stop);
        Assert.Null(await board.ReadThreadAsync(9));
        Assert.Null(await board.GoalAsync("g"));
        stop.Cancel();

        using var stop2 = new CancellationTokenSource();
        using var locked = await BoardAnswering(_ => """{"error": "database error: database is locked"}""", stop2);
        await Assert.ThrowsAsync<InvalidOperationException>(() => locked.ReadThreadAsync(9));
        await Assert.ThrowsAsync<InvalidOperationException>(() => locked.GoalAsync("g"));
        stop2.Cancel();
    }

    [Fact]
    public async Task A_status_reply_missing_its_members_reads_as_defaults_instead_of_failing()
    {
        using var stop = new CancellationTokenSource();
        using var board = await BoardAnswering(tool => tool switch
        {
            "ui:threads" => """{"threads": []}""",
            "recent_messages" => """{"messages": []}""",
            _ => "{}",
        }, stop);
        var s = await board.StatusAsync();
        Assert.Empty(s.Prs);
        Assert.False(s.ConciergeOn);
        stop.Cancel();
    }
}
