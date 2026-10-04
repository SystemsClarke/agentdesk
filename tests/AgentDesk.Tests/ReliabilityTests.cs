using System.Text.Json;
using AgentDesk.Core;
using AgentDesk.Core.Board;
using Xunit;

namespace AgentDesk.Tests;

public sealed class ReliabilityTests : IDisposable
{
    readonly string dir = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "agentdesk-rel-" + Guid.NewGuid().ToString("N"))).FullName;

    public void Dispose()
    {
        try { Directory.Delete(dir, true); } catch (IOException) { }
    }

    [Fact]
    public async Task Atomic_writes_never_show_a_reader_half_a_file_and_leave_no_temp_files()
    {
        var file = Path.Combine(dir, "claude_usage.json");
        Atomic.Write(file, "{}");
        var big = JsonSerializer.Serialize(new { pad = new string('x', 200_000) });
        using var stop = new CancellationTokenSource();
        var torn = 0;
        var reader = Task.Run(() =>
        {
            while (!stop.IsCancellationRequested)
                try
                {
                    using var f = new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete); // how a reader should open a file that gets replaced
                    using var doc = JsonDocument.Parse(f);
                }
                catch (JsonException) { Interlocked.Increment(ref torn); }
                catch (IOException) { } // the replace is in flight for a moment
                catch (UnauthorizedAccessException) { }
                finally { Thread.Sleep(5); } // a reader that never lets go would starve any writer
        });
        Parallel.For(0, 3, w =>
        {
            for (var i = 0; i < 30; i++)
                Atomic.Write(file, i % 2 == 0 ? big : "{}"); // three writers, no common temp name
        });
        stop.Cancel();
        await reader;
        Assert.Equal(0, torn);
        Assert.Empty(Directory.GetFiles(dir, "*.tmp"));
    }

    [Fact]
    public async Task A_write_that_a_reader_blocks_for_a_moment_waits_and_lands()
    {
        var file = Path.Combine(dir, "settings.json");
        Atomic.Write(file, "{\"a\":1}");
        var reader = new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.Read); // an ordinary ReadAllText holds it like this
        var release = Task.Run(async () => { await Task.Delay(200); reader.Dispose(); });
        Atomic.Write(file, "{\"a\":2}"); // refused ("access denied") until the reader lets go
        await release;
        Assert.Equal("{\"a\":2}", File.ReadAllText(file));
        Assert.Empty(Directory.GetFiles(dir, "*.tmp"));
    }

    [Fact]
    public async Task The_stop_hook_reads_a_transcript_claude_is_still_writing()
    {
        var store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
        store.Init();
        var transcript = Path.Combine(dir, "session.jsonl");
        // claude has the file open for writing: File.ReadLines (FileShare.Read) is refused with IO_SharingViolation.
        using var writer = new FileStream(transcript, FileMode.Create, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete);
        var line = """{"message":{"content":[{"type":"tool_use","name":"Edit"}]}}""" + "\n";
        writer.Write(System.Text.Encoding.UTF8.GetBytes(line));
        writer.Flush();
        var input = JsonSerializer.SerializeToElement(new { session_id = "S9", stop_hook_active = false, transcript_path = transcript });
        var reply = await new Hooks(store).Run("stop", input);
        Assert.Contains("you changed files this session", reply); // read, and found the edit with no post
    }

    [Fact]
    public void The_log_keeps_one_older_file_and_does_not_grow_without_end()
    {
        var was = Log.Path;
        try
        {
            Log.Path = Path.Combine(dir, "core.log");
            var line = new string('y', 1000);
            for (var i = 0; i < 2600; i++) Log.Info(line); // about 2.6 MB
            Assert.True(File.Exists(Log.Path + ".1"), "the full log was moved aside");
            Assert.True(new FileInfo(Log.Path).Length < 2 * 1024 * 1024);
        }
        finally { Log.Path = was; }
    }
}
