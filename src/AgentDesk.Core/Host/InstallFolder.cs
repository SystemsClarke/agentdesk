using System.Diagnostics;
using System.Text;

namespace AgentDesk.Core.Host;

/// <summary>
/// An update swaps the install's current\ folder, and Windows will not move a folder that any process has as its working
/// directory. The window used to inherit the core's (current\) and outlives the core's restart on purpose, so it pinned the
/// folder: the updater silently kept the old version and relaunched it. Before applying, anything of ours started from there
/// is closed. MCP relays are never touched: Claude Code starts them in the project's folder, not in ours.
/// </summary>
public static unsafe partial class InstallFolder
{
    /// <summary>The working directory of a process of this user, or null when it cannot be read (gone, or not ours).</summary>
    public static string? CwdOf(int pid)
    {
        var h = OpenProcess(0x0410, 0, pid); // PROCESS_QUERY_INFORMATION | PROCESS_VM_READ
        if (h == 0) return null;
        try
        {
            Pbi pbi = default;
            if (NtQueryInformationProcess(h, 0, ref pbi, sizeof(Pbi), out _) != 0) return null;
            nint parameters = 0;
            if (ReadProcessMemory(h, pbi.Peb + 0x20, &parameters, sizeof(nint), out _) == 0 || parameters == 0) return null; // PEB.ProcessParameters
            UnicodeString path = default;
            if (ReadProcessMemory(h, parameters + 0x38, &path, sizeof(UnicodeString), out _) == 0 || path.Length == 0) return null; // .CurrentDirectory.DosPath
            var text = new byte[path.Length];
            fixed (byte* p = text)
                if (ReadProcessMemory(h, path.Buffer, p, text.Length, out _) == 0) return null;
            return Encoding.Unicode.GetString(text);
        }
        finally { CloseHandle(h); }
    }

    static bool Inside(string? cwd, string root) => cwd is not null && (cwd.TrimEnd('\\') + "\\").StartsWith(root, StringComparison.OrdinalIgnoreCase);

    static string Slashed(string root) => Path.GetFullPath(root).TrimEnd('\\') + "\\";

    /// <summary>Closes (then, after 3 seconds, ends) every process called one of <paramref name="names"/> whose working directory
    /// is inside <paramref name="root"/>, except this one. True if one was the window (AgentDesk.App), so the restart can bring it back.</summary>
    public static bool Release(string root, params string[] names)
    {
        root = Slashed(root);
        var window = false;
        foreach (var name in names)
            foreach (var p in Process.GetProcessesByName(name))
                using (p)
                {
                    if (p.Id == Environment.ProcessId || !Inside(CwdOf(p.Id), root)) continue;
                    window |= name == "AgentDesk.App";
                    Log.Info($"update: closing {name} (pid {p.Id}): its working directory is inside {root}, and Windows will not replace a folder that is in use");
                    try
                    {
                        p.CloseMainWindow();
                        if (!p.WaitForExit(3000)) { p.Kill(); p.WaitForExit(3000); }
                    }
                    catch (Exception e) when (e is InvalidOperationException or System.ComponentModel.Win32Exception) { } // it had just ended
                }
        return window;
    }

    /// <summary>Every process, of any name, still using <paramref name="root"/> as its working directory: what to blame when an update did not apply.</summary>
    public static List<string> Holders(string root)
    {
        root = Slashed(root);
        List<string> found = [];
        foreach (var p in Process.GetProcesses())
            using (p)
                if (p.Id != Environment.ProcessId && Inside(CwdOf(p.Id), root)) found.Add($"{p.ProcessName} (pid {p.Id})");
        return found;
    }

    [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
    struct Pbi { public nint Exit, Peb, Affinity, Priority, Pid, Parent; }

    [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
    struct UnicodeString { public ushort Length, MaxLength; public nint Buffer; }

    [System.Runtime.InteropServices.LibraryImport("ntdll.dll")] private static partial int NtQueryInformationProcess(nint h, int cls, ref Pbi pbi, int len, out int returned);
    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")] private static partial nint OpenProcess(int access, int inherit, int pid);
    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")] private static partial int ReadProcessMemory(nint h, nint address, void* buffer, nint size, out nint read);
    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")] private static partial int CloseHandle(nint h);
}
