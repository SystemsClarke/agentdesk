namespace AgentDesk.Core;

/// <summary>One line per event in %LOCALAPPDATA%\AgentDesk\core.log. The core has no console.</summary>
public static class Log
{
    static readonly Lock Gate = new();
    public static string Path { get; set; } =
        System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AgentDesk", "core.log");

    public static void Info(string msg) => Write("INFO", msg);
    public static void Warn(string msg) => Write("WARN", msg);

    /// <summary>ui:log_tail: the last <paramref name="lines"/> lines (1 to 1000), from the file's last 256 KB.</summary>
    public static string[] Tail(int lines)
    {
        lock (Gate)
            try
            {
                using var f = new FileStream(Path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
                var skip = f.Length > 256 * 1024 ? 1 : 0; // the first line read is cut
                f.Seek(-Math.Min(f.Length, 256 * 1024), SeekOrigin.End);
                var all = new StreamReader(f).ReadToEnd().Split(Environment.NewLine, StringSplitOptions.RemoveEmptyEntries).Skip(skip).ToArray();
                return all[Math.Max(0, all.Length - Math.Clamp(lines, 1, 1000))..];
            }
            catch (IOException) { return []; }
    }

    static void Write(string level, string msg)
    {
        var line = $"{DateTimeOffset.UtcNow:yyyy-MM-ddTHH:mm:ssZ} {level} {msg}{Environment.NewLine}";
        lock (Gate)
            try { File.AppendAllText(Path, line); } catch (IOException) { } // logging must never take the core down
    }
}
