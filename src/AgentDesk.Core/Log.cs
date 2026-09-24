namespace AgentDesk.Core;

/// <summary>One line per event in %LOCALAPPDATA%\AgentDesk\core.log. The core has no console.</summary>
public static class Log
{
    static readonly Lock Gate = new();
    public static string Path { get; set; } =
        System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AgentDesk", "core.log");

    public static void Info(string msg) => Write("INFO", msg);
    public static void Warn(string msg) => Write("WARN", msg);

    static void Write(string level, string msg)
    {
        var line = $"{DateTimeOffset.UtcNow:yyyy-MM-ddTHH:mm:ssZ} {level} {msg}{Environment.NewLine}";
        lock (Gate)
            try { File.AppendAllText(Path, line); } catch (IOException) { } // logging must never take the core down
    }
}
