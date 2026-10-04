namespace AgentDesk.Core;

/// <summary>Writes a file so that a reader (or a crash, or another writer) never sees half of it: the text goes to a temp file of its own
/// next to the target, then replaces it. Two writers cannot collide on the temp name, and a replace that is refused for a moment (a reader
/// has the target open) is tried again, up to twenty times.</summary>
public static class Atomic
{
    public static void Write(string path, string text)
    {
        var tmp = $"{path}.{Environment.ProcessId}.{Guid.NewGuid():N}.tmp";
        try
        {
            File.WriteAllText(tmp, text);
            for (var attempt = 1; ; attempt++)
                try { File.Move(tmp, path, true); return; }
                catch (Exception e) when (e is IOException or UnauthorizedAccessException && attempt < 20) { Thread.Sleep(10 * attempt); } // Windows says "access denied" while a reader has the target open
        }
        catch
        {
            try { File.Delete(tmp); } catch (IOException) { }
            throw;
        }
    }
}
