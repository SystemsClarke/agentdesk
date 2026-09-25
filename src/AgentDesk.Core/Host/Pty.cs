using System.Diagnostics;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace AgentDesk.Core.Host;

/// <summary>
/// A process on a pseudoconsole (ConPTY) with no window: what it draws arrives on <see cref="Output"/> as VT bytes,
/// and what is written to <see cref="Input"/> reaches it as typed keys. Win32 only, so the core stays AOT-clean.
/// </summary>
public sealed unsafe partial class Pty : IDisposable
{
    const uint EXTENDED_STARTUPINFO_PRESENT = 0x80000, CREATE_UNICODE_ENVIRONMENT = 0x400, STARTF_USESTDHANDLES = 0x100;
    const nint PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = 0x20016;
    nint hpc;
    public Stream Input { get; }
    public Stream Output { get; }
    public Task Exited { get; }
    public int Pid { get; }

    Pty(nint hpc, FileStream input, FileStream output, int pid, Task exited) =>
        (this.hpc, Input, Output, Pid, Exited) = (hpc, input, output, pid, exited);

    /// <summary>Runs <paramref name="commandLine"/> in <paramref name="folder"/>, with <paramref name="env"/> laid over
    /// this process's environment (a null value removes the variable).</summary>
    public static Pty Start(string commandLine, string folder, IReadOnlyDictionary<string, string?> env, int cols, int rows)
    {
        if (CreatePipe(out var inRead, out var inWrite, 0, 0) == 0 || CreatePipe(out var outRead, out var outWrite, 0, 0) == 0)
            throw new IOException($"CreatePipe failed ({Marshal.GetLastPInvokeError()})");
        var hr = CreatePseudoConsole(Size(cols, rows), inRead, outWrite, 0, out var hpc);
        if (hr != 0) throw new IOException($"CreatePseudoConsole failed (0x{hr:X8})");

        nint bytes = 0;
        InitializeProcThreadAttributeList(0, 1, 0, ref bytes);
        var list = Marshal.AllocHGlobal(bytes);
        try
        {
            if (InitializeProcThreadAttributeList(list, 1, 0, ref bytes) == 0 ||
                UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, hpc, sizeof(nint), 0, 0) == 0)
                throw new IOException($"attribute list failed ({Marshal.GetLastPInvokeError()})");
            // No std handles of our own: else a child of a redirected parent writes to the parent's pipes, not the pseudoconsole.
            var si = new StartupInfoEx { cb = (uint)sizeof(StartupInfoEx), dwFlags = STARTF_USESTDHANDLES, lpAttributeList = list };
            var vars = Environment.GetEnvironmentVariables().Cast<System.Collections.DictionaryEntry>()
                .ToDictionary(e => (string)e.Key, e => (string?)e.Value, StringComparer.OrdinalIgnoreCase);
            foreach (var (k, v) in env) vars[k] = v;
            var block = string.Concat(vars.Where(v => v.Value is not null).OrderBy(v => v.Key, StringComparer.OrdinalIgnoreCase)
                                          .Select(v => $"{v.Key}={v.Value}\0")) + "\0";
            ProcessInfo pi;
            fixed (char* cmd = commandLine + "\0", cwd = folder, envp = block) // CreateProcessW may write to its command line
                if (CreateProcessW(null, cmd, 0, 0, 0, EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT, envp, cwd, &si, &pi) == 0)
                {
                    var err = Marshal.GetLastPInvokeError();
                    ClosePseudoConsole(hpc);
                    CloseHandle(inWrite);
                    CloseHandle(outRead);
                    throw new ArgumentException($"could not start '{commandLine}' in {folder} (error {err})");
                }
            CloseHandle(pi.hThread);
            var wait = new ManualResetEvent(false) { SafeWaitHandle = new SafeWaitHandle(pi.hProcess, true) };
            var exited = new TaskCompletionSource();
            ThreadPool.RegisterWaitForSingleObject(wait, (_, _) => { exited.TrySetResult(); wait.Dispose(); }, null, -1, true);
            var pty = new Pty(hpc, new FileStream(new SafeFileHandle(inWrite, true), FileAccess.Write, 0),
                              new FileStream(new SafeFileHandle(outRead, true), FileAccess.Read, 0), (int)pi.dwProcessId, exited.Task);
            _ = exited.Task.ContinueWith(_ => pty.Close()); // the pseudoconsole holds Output open until it is closed
            return pty;
        }
        finally
        {
            DeleteProcThreadAttributeList(list);
            Marshal.FreeHGlobal(list);
            CloseHandle(inRead); // the pseudoconsole has its own copies
            CloseHandle(outWrite);
        }
    }

    public void Resize(int cols, int rows)
    {
        var h = hpc;
        if (h != 0) ResizePseudoConsole(h, Size(cols, rows));
    }

    /// <summary>Ends the process and everything it started.</summary>
    public void Kill()
    {
        try { using var p = Process.GetProcessById(Pid); p.Kill(entireProcessTree: true); }
        catch (Exception e) when (e is ArgumentException or InvalidOperationException or System.ComponentModel.Win32Exception) { } // already gone
        Close();
    }

    // Off the caller's thread: on older Windows ClosePseudoConsole waits until Output has been drained.
    void Close() { if (Interlocked.Exchange(ref hpc, 0) is var h and not 0) Task.Run(() => ClosePseudoConsole(h)); }

    public void Dispose() { Kill(); Input.Dispose(); }

    static Coord Size(int cols, int rows) => new() { X = (short)Math.Clamp(cols, 1, 1000), Y = (short)Math.Clamp(rows, 1, 1000) };

    struct Coord { public short X, Y; }
    struct StartupInfoEx
    {
        public uint cb; public nint lpReserved, lpDesktop, lpTitle;
        public uint dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public ushort wShowWindow, cbReserved2; public nint lpReserved2, hStdInput, hStdOutput, hStdError, lpAttributeList;
    }
    struct ProcessInfo { public nint hProcess, hThread; public uint dwProcessId, dwThreadId; }

    [LibraryImport("kernel32.dll", SetLastError = true)] private static partial int CreatePipe(out nint read, out nint write, nint sa, uint size);
    [LibraryImport("kernel32.dll")] private static partial int CreatePseudoConsole(Coord size, nint input, nint output, uint flags, out nint hpc);
    [LibraryImport("kernel32.dll")] private static partial int ResizePseudoConsole(nint hpc, Coord size);
    [LibraryImport("kernel32.dll")] private static partial void ClosePseudoConsole(nint hpc);
    [LibraryImport("kernel32.dll", SetLastError = true)]
    private static partial int InitializeProcThreadAttributeList(nint list, int count, uint flags, ref nint size);
    [LibraryImport("kernel32.dll", SetLastError = true)]
    private static partial int UpdateProcThreadAttribute(nint list, uint flags, nint attr, nint value, nint size, nint prev, nint ret);
    [LibraryImport("kernel32.dll")] private static partial void DeleteProcThreadAttributeList(nint list);
    [LibraryImport("kernel32.dll", SetLastError = true)]
    private static partial int CreateProcessW(char* app, char* cmd, nint pa, nint ta, int inherit, uint flags, char* env, char* cwd,
                                              StartupInfoEx* si, ProcessInfo* pi);
    [LibraryImport("kernel32.dll")] private static partial int CloseHandle(nint h);
}
