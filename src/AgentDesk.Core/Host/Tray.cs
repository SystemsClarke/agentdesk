using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text.Json.Nodes;
using AgentDesk.Core.Board;

namespace AgentDesk.Core.Host;

/// <summary>
/// The core's face: a notification-area icon whose tooltip (its accessible name) counts the questions waiting on John,
/// and a balloon, which Windows shows as a toast, for each new one and hourly while any stay open. Win32 only, so
/// the core stays AOT-clean. A hidden window rather than a message-only one: only a top-level window hears
/// TaskbarCreated, which is how the icon comes back after Explorer restarts.
/// </summary>
public static unsafe partial class Tray
{
    const uint WM_CONTEXTMENU = 0x7B, WM_LBUTTONDBLCLK = 0x203, Callback = 0x8001, NIN_KEYSELECT = 0x401, NIN_BALLOONUSERCLICK = 0x405;
    const uint NIM_ADD = 0, NIM_MODIFY = 1, NIM_DELETE = 2, NIM_SETVERSION = 4, NIF_MESSAGE = 1, NIF_ICON = 2, NIF_TIP = 4, NIF_INFO = 0x10;
    static readonly TimeSpan Poll = TimeSpan.FromSeconds(3), Remind = TimeSpan.FromHours(1); // the Python window's poll; its hourly summary
    static nint hwnd, icon;
    static uint taskbarCreated;
    static string tip = "AgentDesk"; // Windows prefixes the app name to the accessible name, so the count stands alone
    static long clicked; // the thread the latest toast was about: what clicking it opens
    static long lastLaunch;
    /// <summary>Set once Velopack has downloaded an update; the menu then offers to restart into it.</summary>
    public static Action? ApplyUpdate { get; set; }
    /// <summary>The ops console's URL with its key (Web.cs); the menu opens it.</summary>
    public static string? WebUrl { get; set; }

    /// <summary>A test or temp core (AGENTDESK_DATA set, or AGENTDESK_NO_TRAY=1) shows no icon and no toasts: they cluttered
    /// John's taskbar, and quitting one by mistake looked like quitting the real one.</summary>
    public static bool Hidden(Func<string, string?> env) => env("AGENTDESK_DATA") is { Length: > 0 } || env("AGENTDESK_NO_TRAY") == "1";

    public static void Start(BoardStore store)
    {
        if (Hidden(Environment.GetEnvironmentVariable))
            Log.Info("tray: none on a temp core (AGENTDESK_DATA or AGENTDESK_NO_TRAY)");
        else
            new Thread(() => Pump(store)) { IsBackground = true, Name = "tray" }.Start();
    }

    static void Pump(BoardStore store)
    {
        fixed (char* cls = "AgentDeskTray")
        {
            var wc = new WNDCLASSW { lpfnWndProc = &WndProc, hInstance = GetModuleHandleW(null), lpszClassName = cls };
            RegisterClassW(&wc);
            hwnd = CreateWindowExW(0, cls, cls, 0, 0, 0, 0, 0, 0, 0, wc.hInstance, 0);
        }
        taskbarCreated = RegisterWindowMessageW("TaskbarCreated");
        nint small = 0;
        icon = ExtractIconExW(Environment.ProcessPath!, 0, null, &small, 1) > 0 ? small : LoadIconW(0, 32512);
        Add();
        _ = Task.Run(() => Watch(store));
        MSG msg;
        while (GetMessageW(&msg, 0, 0, 0) > 0) DispatchMessageW(&msg);
    }

    static void Add() { Shell(NIM_ADD, NIF_MESSAGE | NIF_ICON | NIF_TIP); Shell(NIM_SETVERSION, 0); } // v3: NIN_KEYSELECT and WM_CONTEXTMENU from the keyboard

    [UnmanagedCallersOnly]
    static nint WndProc(nint h, uint msg, nint w, nint l)
    {
        if (msg == Callback)
            switch ((uint)l)
            {
                case WM_LBUTTONDBLCLK or NIN_KEYSELECT: Launch(0); break;
                case NIN_BALLOONUSERCLICK: Launch(clicked); break;
                case WM_CONTEXTMENU: Menu(h); break;
            }
        else if (msg == taskbarCreated) Add();
        return DefWindowProcW(h, msg, w, l);
    }

    static void Menu(nint h)
    {
        var m = CreatePopupMenu();
        AppendMenuW(m, 0, 1, "&Open AgentDesk");
        if (WebUrl is not null) AppendMenuW(m, 0, 4, "Open o&ps console");
        if (ApplyUpdate is not null) AppendMenuW(m, 0, 2, "&Restart to update");
        AppendMenuW(m, 0, 3, "&Quit");
        SetMenuDefaultItem(m, 1, 0);
        var xy = stackalloc int[2];
        GetCursorPos(xy);
        SetForegroundWindow(h); // or the menu never closes when John clicks elsewhere
        var cmd = TrackPopupMenu(m, 0x100 | 0x2, xy[0], xy[1], 0, h, 0); // TPM_RETURNCMD | TPM_RIGHTBUTTON
        PostMessageW(h, 0, 0, 0); // WM_NULL, so the next right-click opens the menu first time
        DestroyMenu(m);
        if (cmd == 1) Launch(0);
        try { if (cmd == 4) Process.Start(new ProcessStartInfo(WebUrl!) { UseShellExecute = true }); }
        catch (Exception e) { Log.Warn($"could not open the ops console: {e.Message}"); }
        if (cmd is 2 or 3) Shell(NIM_DELETE, 0);
        try { if (cmd == 2) ApplyUpdate!(); } catch (Exception e) { Log.Warn($"update failed: {e.Message}"); Add(); return; }
        if (cmd == 3) { Log.Info("core quit from the tray"); Environment.Exit(0); }
    }

    /// <summary>Opens AgentDesk.App from beside the core, else from the installed current\ folder.</summary>
    public static void Launch(long thread)
    {
        if (Environment.TickCount64 - Interlocked.Exchange(ref lastLaunch, Environment.TickCount64) < 1000) return; // Enter sends NIN_KEYSELECT twice
        var exe = new[] { AppContext.BaseDirectory, Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AgentDeskApp", "current") }
            .Select(d => Path.Combine(d, "AgentDesk.App.exe")).FirstOrDefault(File.Exists);
        try { if (exe is null) Log.Warn("AgentDesk.App.exe not found"); else Process.Start(exe, thread > 0 ? $"--thread {thread}" : ""); }
        catch (Exception e) { Log.Warn($"could not open AgentDesk: {e.Message}"); }
    }

    /// <summary>Takes the icon down before a restart, so no dead one lingers.</summary>
    public static void Hide() { if (hwnd != 0) Shell(NIM_DELETE, 0); }

    static void Toast(long thread, string title, string text)
    {
        clicked = thread;
        if (Shell(NIM_MODIFY, NIF_INFO, title, text) == 0) Log.Warn($"toast not shown: {title}");
    }

    static int Shell(uint op, uint flags, string title = "", string text = "")
    {
        var d = new NOTIFYICONDATAW { cbSize = (uint)sizeof(NOTIFYICONDATAW), hWnd = hwnd, uFlags = flags, uCallbackMessage = Callback, hIcon = icon, uVersion = 3, dwInfoFlags = 4 }; // NIIF_USER: our icon
        Copy(tip, d.szTip, 128);
        Copy(title, d.szInfoTitle, 64);
        Copy(text, d.szInfo, 256);
        return Shell_NotifyIconW(op, &d);
    }

    static void Copy(string s, char* to, int size) => (s.Length < size ? s : s[..(size - 2)] + "…").CopyTo(new Span<char>(to, size));

    struct WNDCLASSW { public uint style; public delegate* unmanaged<nint, uint, nint, nint, nint> lpfnWndProc; public int cbClsExtra, cbWndExtra; public nint hInstance, hIcon, hCursor, hbrBackground; public char* lpszMenuName, lpszClassName; }
    struct MSG { public nint hwnd; public uint message; public nint wParam, lParam; public uint time; public int x, y, lPrivate; }
    struct NOTIFYICONDATAW
    {
        public uint cbSize; public nint hWnd; public uint uID, uFlags, uCallbackMessage; public nint hIcon;
        public fixed char szTip[128]; public uint dwState, dwStateMask; public fixed char szInfo[256]; public uint uVersion;
        public fixed char szInfoTitle[64]; public uint dwInfoFlags; public Guid guidItem; public nint hBalloonIcon;
    }

    [LibraryImport("user32.dll")] private static partial ushort RegisterClassW(WNDCLASSW* wc);
    [LibraryImport("user32.dll")] private static partial nint CreateWindowExW(uint ex, char* cls, char* name, uint style, int x, int y, int w, int h, nint parent, nint menu, nint inst, nint param);
    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)] private static partial uint RegisterWindowMessageW(string name);
    [LibraryImport("user32.dll")] private static partial int GetMessageW(MSG* msg, nint hwnd, uint min, uint max);
    [LibraryImport("user32.dll")] private static partial nint DispatchMessageW(MSG* msg);
    [LibraryImport("user32.dll")] private static partial nint DefWindowProcW(nint h, uint msg, nint w, nint l);
    [LibraryImport("user32.dll")] private static partial nint LoadIconW(nint inst, nint id);
    [LibraryImport("user32.dll")] private static partial nint CreatePopupMenu();
    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)] private static partial int AppendMenuW(nint m, uint flags, nint id, string text);
    [LibraryImport("user32.dll")] private static partial int SetMenuDefaultItem(nint m, uint item, uint byPos);
    [LibraryImport("user32.dll")] private static partial int GetCursorPos(int* xy);
    [LibraryImport("user32.dll")] private static partial int SetForegroundWindow(nint h);
    [LibraryImport("user32.dll")] private static partial int TrackPopupMenu(nint m, uint flags, int x, int y, int reserved, nint h, nint rect);
    [LibraryImport("user32.dll")] private static partial int DestroyMenu(nint m);
    [LibraryImport("user32.dll")] private static partial int PostMessageW(nint h, uint msg, nint w, nint l);
    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16)] private static partial nint GetModuleHandleW(string? name);
    [LibraryImport("shell32.dll", StringMarshalling = StringMarshalling.Utf16)] private static partial uint ExtractIconExW(string file, int index, nint* large, nint* small, uint n);
    [LibraryImport("shell32.dll")] private static partial int Shell_NotifyIconW(uint op, NOTIFYICONDATAW* data);
}

static partial class Tray // safe, so it can await
{
    /// <summary>"#&lt;id&gt; &lt;subject&gt;: &lt;first line&gt;" for an agent's follow-up on a question John answered.</summary>
    public static string FollowUpText(JsonObject f)
    {
        var first = ((string?)f["body"] ?? "").Split('\n').Select(l => l.Trim()).FirstOrDefault(l => l.Length > 0) ?? "";
        return $"#{f["thread_id"]} {f["subject"]}: {first}";
    }

    static async Task Watch(BoardStore store)
    {
        HashSet<long>? seen = null; // questions open at start are not news (the Python rule)
        long followed = -1; // the newest message looked at for follow-ups; nothing before the core started is news
        var reminded = DateTime.UtcNow;
        using var timer = new PeriodicTimer(Poll);
        do
            try
            {
                if (seen is null) store.Init(); // a fresh board has no open_questions view yet
                using var db = store.Open();
                var top = db.MaxMessageId();
                var follows = followed < 0 ? [] : db.FollowUps(followed, top);
                followed = top;
                var open = db.OpenQuestions(false); // newest first
                var ids = open.Select(q => (long)q["thread_id"]!).ToHashSet();
                var now = $"{(open.Count == 0 ? "No" : open.Count)} open question{(open.Count == 1 ? "" : "s")}";
                if (now != tip) { tip = now; Shell(NIM_MODIFY, NIF_TIP); }
                var followedUp = follows.Select(f => (long)f["thread_id"]!).ToHashSet();
                List<JsonObject> fresh = seen is null ? [] : open.Where(q => !seen.Contains((long)q["thread_id"]!) && !followedUp.Contains((long)q["thread_id"]!)).Reverse().ToList();
                seen = ids;
                foreach (var f in follows) Toast((long)f["thread_id"]!, $"{((string)f["mark"]! == "done" ? "Done" : "Follow-up")} from {f["author"]}", FollowUpText(f));
                foreach (var q in fresh) Toast((long)q["thread_id"]!, $"New question from {q["opened_by"]}", (string)q["subject"]!);
                if (fresh.Count > 0 || open.Count == 0) reminded = DateTime.UtcNow;
                else if (DateTime.UtcNow - reminded >= Remind)
                {
                    reminded = DateTime.UtcNow;
                    var subjects = open.Select(q => (string)q["subject"]!).ToList();
                    Toast((long)open[0]["thread_id"]!, $"AgentDesk: {tip}", subjects.Count == 1 ? subjects[0]
                        : string.Join("; ", subjects.Take(3).Select(s => s.Length > 60 ? s[..60] : s)) + (subjects.Count > 3 ? $" and {subjects.Count - 3} more" : ""));
                }
            }
            catch (Exception e) { Log.Warn($"tray poll failed: {e.Message}"); }
        while (await timer.WaitForNextTickAsync());
    }
}
