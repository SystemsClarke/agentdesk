using System.Diagnostics;
using System.Runtime.InteropServices;

namespace AgentDesk.Core.Host;

/// <summary>
/// A claude.exe the core did not start (or no longer tracks) that is running the very conversation an identity is about to resume: two
/// processes writing one transcript, which is how it gets corrupted. It happens when a core goes away without taking its sessions with it
/// (before sessions were put in the core's job object), and the next core then resumes them a second time. Matched on the session id in the
/// command line (<c>--resume ID</c> or <c>--session-id ID</c>) and on the process name, never on the name alone.
/// </summary>
public static partial class StaleSessions
{
    /// <summary>The pids of processes called <paramref name="processName"/> whose command line carries this conversation's id, other than
    /// <paramref name="except"/> (the sessions this core hosts).</summary>
    public static List<int> Find(string sessionId, IReadOnlySet<int> except, string processName = "claude")
    {
        var found = new List<int>();
        if (sessionId.Length != 36) return found; // a conversation id, not a fragment that would match something else
        foreach (var p in Process.GetProcessesByName(processName))
            using (p)
                if (!except.Contains(p.Id) && CommandLine(p.Id) is { } cmd && Carries(cmd, sessionId))
                    found.Add(p.Id);
        return found;
    }

    /// <summary>Stops them, and what they started. Returns the pids it stopped.</summary>
    public static List<int> Stop(string sessionId, IReadOnlySet<int> except, string processName = "claude")
    {
        var stopped = new List<int>();
        foreach (var pid in Find(sessionId, except, processName))
            try
            {
                using var p = Process.GetProcessById(pid);
                if (!string.Equals(p.ProcessName, processName, StringComparison.OrdinalIgnoreCase)) continue; // the pid was reused since
                p.Kill(entireProcessTree: true);
                stopped.Add(pid);
                Log.Warn($"stopped pid {pid}: a second {processName} on conversation {sessionId}, which an identity is about to resume");
            }
            catch (Exception e) when (e is ArgumentException or InvalidOperationException or System.ComponentModel.Win32Exception) { } // already gone
        return stopped;
    }

    /// <summary>True when the command line has <c>--resume ID</c> or <c>--session-id ID</c> for exactly this id.</summary>
    public static bool Carries(string commandLine, string id)
    {
        foreach (var flag in new[] { "--resume ", "--session-id " })
            for (var at = commandLine.IndexOf(flag + id, StringComparison.OrdinalIgnoreCase); at >= 0; at = commandLine.IndexOf(flag + id, at + 1, StringComparison.OrdinalIgnoreCase))
            {
                var end = at + flag.Length + id.Length;
                if (end == commandLine.Length || !Uri.IsHexDigit(commandLine[end]) && commandLine[end] != '-') return true; // the whole id, not the start of a longer one
            }
        return false;
    }

    /// <summary>A process's command line (ProcessCommandLineInformation): null when it cannot be read.</summary>
    static string? CommandLine(int pid)
    {
        var h = OpenProcess(0x1000 /* PROCESS_QUERY_LIMITED_INFORMATION */, 0, pid);
        if (h == 0) return null;
        try
        {
            NtQueryInformationProcess(h, 60, 0, 0, out var need);
            if (need < 16 || need > 1 << 20) return null;
            var buf = Marshal.AllocHGlobal((nint)need);
            try
            {
                if (NtQueryInformationProcess(h, 60, buf, need, out _) != 0) return null;
                var bytes = (ushort)Marshal.ReadInt16(buf); // UNICODE_STRING: Length, MaximumLength, then (x64) the Buffer pointer at offset 8
                var text = Marshal.ReadIntPtr(buf, 8);
                return text == 0 ? null : Marshal.PtrToStringUni(text, bytes / 2);
            }
            finally { Marshal.FreeHGlobal(buf); }
        }
        finally { CloseHandle(h); }
    }

    [LibraryImport("ntdll.dll")] private static partial int NtQueryInformationProcess(nint process, int cls, nint buffer, uint length, out uint returned);
    [LibraryImport("kernel32.dll", SetLastError = true)] private static partial nint OpenProcess(uint access, int inherit, int pid);
    [LibraryImport("kernel32.dll")] private static partial int CloseHandle(nint h);
}
