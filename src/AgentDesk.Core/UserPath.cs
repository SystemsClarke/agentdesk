// agentdesk on PATH: install and update put the install's current folder on the user PATH (HKCU\Environment\Path),
// uninstall takes it back off. Only that one entry is ever added or removed; everything else in the value is kept
// exactly as it was, variables unexpanded.
using System.Runtime.InteropServices;
using Microsoft.Win32;

namespace AgentDesk.Core;

public static partial class UserPath
{
    /// <summary>
    /// The PATH value with <paramref name="dir"/> on it (<paramref name="on"/>) or off it, or null when nothing changes.
    /// An entry matches when it names the same folder: case, a trailing backslash, surrounding spaces or quotes, and
    /// %VARIABLES% do not matter. On appends once at the end; off removes every match and leaves the rest alone.
    /// </summary>
    public static string? With(string? path, string dir, bool on)
    {
        var entries = (path ?? "").Split(';');
        var matches = entries.Count(e => Same(e, dir));
        if (on)
        {
            if (matches > 0) return null;
            var kept = (path ?? "").TrimEnd(';');
            return kept.Length == 0 ? dir : $"{kept};{dir}";
        }
        if (matches == 0) return null;
        return string.Join(';', entries.Where(e => !Same(e, dir)));
    }

    static bool Same(string entry, string dir) => Norm(entry) is { Length: > 0 } e && e.Equals(Norm(dir), StringComparison.OrdinalIgnoreCase);

    static string Norm(string s) => Environment.ExpandEnvironmentVariables(s.Trim().Trim('"')).TrimEnd('\\', '/');

    /// <summary>Reads the PATH through <paramref name="read"/> and writes it back through <paramref name="write"/> only if
    /// it changes; true when it did. The registry is one pair of these; tests pass fakes.</summary>
    public static bool Update(Func<string?> read, Action<string> write, string dir, bool on)
    {
        if (With(read(), dir, on) is not { } next) return false;
        write(next);
        return true;
    }

    /// <summary>Puts <paramref name="dir"/> on (or off) John's user PATH as REG_EXPAND_SZ, and tells running programs
    /// (Explorer, new consoles) the environment changed. Only Setup's install, update and uninstall hooks call this.</summary>
    public static void Apply(string dir, bool on)
    {
        using var env = Registry.CurrentUser.CreateSubKey("Environment");
        var changed = Update(
            () => env.GetValue("Path", null, RegistryValueOptions.DoNotExpandEnvironmentNames) as string,
            next => env.SetValue("Path", next, RegistryValueKind.ExpandString),
            dir, on);
        if (!changed) return;
        Log.Info($"user PATH: {(on ? "added" : "removed")} {dir}");
        SendMessageTimeout(0xFFFF, 0x001A, 0, "Environment", 0x0002, 5000, out _); // HWND_BROADCAST, WM_SETTINGCHANGE, SMTO_ABORTIFHUNG
    }

    [LibraryImport("user32.dll", EntryPoint = "SendMessageTimeoutW", StringMarshalling = StringMarshalling.Utf16)]
    private static partial nint SendMessageTimeout(nint hwnd, uint msg, nint wParam, string lParam, uint flags, uint timeout, out nint result);
}
