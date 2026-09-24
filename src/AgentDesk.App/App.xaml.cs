using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Windows;

namespace AgentDesk.App;

/// <summary>One window per user: a second launch hands its --thread to the first over a named pipe and exits.</summary>
public partial class App : Application
{
    static readonly string PipeName = "AgentDesk.App." + Environment.UserName;
    Mutex? mutex;

    protected override async void OnStartup(StartupEventArgs e)
    {
        base.OnStartup(e);
        var i = Array.IndexOf(e.Args, "--thread");
        int? tid = i >= 0 && i + 1 < e.Args.Length && int.TryParse(e.Args[i + 1], out var n) ? n : null;
        var sample = e.Args.Contains("--sample"); // a sample window runs beside the real one instead of handing over to it
        var first = true;
        mutex = sample ? null : new Mutex(true, @"Local\" + PipeName, out first);
        if (!first)
        {
            Forward(tid);
            Shutdown();
            return;
        }
        IBoard board;
        try
        {
            board = sample ? new SampleBoard() : await CoreBoard.Connect();
        }
        catch (Exception ex) when (ex is IOException or TimeoutException or System.ComponentModel.Win32Exception)
        {
            MessageBox.Show($"AgentDesk couldn't reach its core: {ex.Message}", "AgentDesk");
            Shutdown();
            return;
        }
        var window = new MainWindow(board, tid);
        DispatcherUnhandledException += (_, ex) =>
        {
            ex.Handled = true; // as the Tk app's @guarded: report it on the bar, keep the window up
            window.Flash($"Something broke: {ex.Exception.Message}", "pk b");
        };
        window.Show();
        if (!sample)
            _ = ListenAsync(window);
    }

    protected override void OnExit(ExitEventArgs e)
    {
        mutex?.Dispose();
        base.OnExit(e);
    }

    static void Forward(int? tid)
    {
        try
        {
            _ = AllowSetForegroundWindow(-1); // let the running window take the foreground this launch was given
            using var pipe = new NamedPipeClientStream(".", PipeName, PipeDirection.Out);
            pipe.Connect(2000);
            using var writer = new StreamWriter(pipe);
            writer.Write(tid?.ToString() ?? "");
        }
        catch (Exception ex) when (ex is IOException or TimeoutException)
        {
            // The first instance is exiting; nothing to hand over to.
        }
    }

    static async Task ListenAsync(MainWindow window)
    {
        while (true)
        {
            await using var pipe = new NamedPipeServerStream(PipeName, PipeDirection.In, 1, PipeTransmissionMode.Byte,
                PipeOptions.Asynchronous | PipeOptions.CurrentUserOnly);
            await pipe.WaitForConnectionAsync();
            var text = await new StreamReader(pipe).ReadToEndAsync();
            window.Summon(int.TryParse(text, out var tid) ? tid : null);
        }
    }

    [DllImport("user32.dll")]
    static extern int AllowSetForegroundWindow(int processId);
}
