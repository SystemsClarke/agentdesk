using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using AgentDesk.Contracts;

namespace AgentDesk.Cli;

/// <summary>
/// agentdesk attach &lt;name&gt;: this console becomes a raw VT window onto a session the core hosts. Keys go to the
/// session as typed, its output is drawn as sent, and the window's size follows this console. Ctrl+] detaches.
/// </summary>
static partial class Attach
{
    const uint ENABLE_PROCESSED_INPUT = 1, ENABLE_LINE_INPUT = 2, ENABLE_ECHO_INPUT = 4, ENABLE_VIRTUAL_TERMINAL_INPUT = 0x200;
    const uint ENABLE_VIRTUAL_TERMINAL_PROCESSING = 4;

    public static async Task<int> Run(CoreConnection core, string name)
    {
        nint hin = GetStdHandle(-10), hout = GetStdHandle(-11);
        if (GetConsoleMode(hin, out var inMode) == 0 || GetConsoleMode(hout, out var outMode) == 0)
        {
            Console.Error.WriteLine("agentdesk attach needs a console");
            return 1;
        }
        var stdout = Console.OpenStandardOutput();
        var done = new TaskCompletionSource<string>(TaskCreationOptions.RunContinuationsAsynchronously);
        core.Pushed += e => // in order, on the connection's reader
        {
            var ev = JsonDocument.Parse(e).RootElement;
            if (!ev.TryGetProperty("name", out var n) || n.GetString() != name) return;
            if (ev.GetProperty("event").GetString() == "session.exited") done.TrySetResult($"{name} ended");
            else if (ev.TryGetProperty("data", out var data)) { stdout.Write(data.GetBytesFromBase64()); stdout.Flush(); }
        };
        var (inCp, outCp) = (Console.InputEncoding, Console.OutputEncoding);
        try
        {
            Console.InputEncoding = Console.OutputEncoding = new UTF8Encoding(false);
            SetConsoleMode(hin, (inMode & ~(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT);
            SetConsoleMode(hout, outMode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            var size = (Console.WindowWidth, Console.WindowHeight);
            var reply = await core.Call("ui:attach", Json(new() { ["name"] = name, ["cols"] = size.Item1, ["rows"] = size.Item2 }));
            if (JsonDocument.Parse(reply).RootElement.TryGetProperty("error", out var err)) return Fail(err.GetString());
            _ = Task.Run(async () => // there is no resize event without reading console input records: poll
            {
                while (!done.Task.IsCompleted)
                {
                    await Task.Delay(250);
                    var now = (Console.WindowWidth, Console.WindowHeight);
                    if (now == size) continue;
                    size = now;
                    await core.Call("ui:resize", Json(new() { ["name"] = name, ["cols"] = now.Item1, ["rows"] = now.Item2 }));
                }
            });
            _ = Task.Run(async () =>
            {
                using var stdin = Console.OpenStandardInput();
                var (buf, decoder) = (new byte[4096], Encoding.UTF8.GetDecoder());
                var chars = new char[buf.Length + 1];
                while (stdin.Read(buf) is var n and > 0)
                {
                    var keys = new string(chars, 0, decoder.GetChars(buf, 0, n, chars, 0));
                    var stop = Detach().Match(keys);
                    if (stop.Success) keys = keys[..stop.Index];
                    // One at a time: the core runs a connection's requests concurrently, and keys must not overtake each other.
                    if (keys.Length > 0) await core.Call("ui:input", Json(new() { ["name"] = name, ["data"] = keys }));
                    if (stop.Success) break;
                }
                done.TrySetResult($"detached from {name}");
            });
            Console.Error.Write($"\x1b[0m\r\n[agentdesk: {await done.Task}]\r\n");
            return 0;
        }
        catch (Exception e) when (e is IOException) { return Fail(e.Message); }
        finally
        {
            SetConsoleMode(hin, inMode);
            SetConsoleMode(hout, outMode);
            (Console.InputEncoding, Console.OutputEncoding) = (inCp, outCp);
        }
    }

    /// <summary>Ctrl+]: a raw GS, or its key-down in win32-input-mode (ESC[Vk;Sc;Uc;Kd;Cs;Rc_), which a session's own
    /// pseudoconsole asks this console for, and which this console then sends as typed.</summary>
    [GeneratedRegex(@"\x1d|\x1b\[\d+;\d+;29;1;\d+(;\d+)?_")] private static partial Regex Detach();

    static int Fail(string? why) { Console.Error.WriteLine($"agentdesk: {why}"); return 1; }

    public static JsonElement Json(JsonObject o) => JsonDocument.Parse(o.ToJsonString()).RootElement;

    [LibraryImport("kernel32.dll")] private static partial nint GetStdHandle(int which);
    [LibraryImport("kernel32.dll")] private static partial int GetConsoleMode(nint h, out uint mode);
    [LibraryImport("kernel32.dll")] private static partial int SetConsoleMode(nint h, uint mode);
}
