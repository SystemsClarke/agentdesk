// Install, update and uninstall, via Velopack. Setup.exe runs the installed core with a hook argument;
// the hook points Claude Code and the login Run key at the installed exes, or takes them back out.
// The data in %LOCALAPPDATA%\AgentDesk is never touched.
using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Host;
using Microsoft.Win32;
using Velopack;
using Velopack.Sources;

namespace AgentDesk.Core;

public static class Setup
{
    const string Repo = "https://github.com/SystemsClarke/agentdesk", RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    static readonly string Home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
    static readonly string Cli = Path.Combine(AppContext.BaseDirectory, "agentdesk.exe").Replace('\\', '/');
    static readonly (string Event, string Arg, int Timeout, string Status)[] ClaudeHooks =
    [
        ("SessionStart", "session-start", 15, "AgentDesk briefing"),
        ("Stop", "stop", 10, "AgentDesk: did you tell the swarm?"),
        ("PostToolUse", "context", 10, "Phoenix context check"),
    ];

    /// <summary>Runs a Velopack hook and exits if this launch is one. A downloaded update waits for John (see KeepUpdated).</summary>
    public static void Run() => VelopackApp.Build()
        .SetAutoApplyOnStartup(false)
        .OnAfterInstallFastCallback(_ => Register(true))
        .OnAfterUpdateFastCallback(_ => Register(true))
        .OnBeforeUninstallFastCallback(_ => Register(false))
        .Run();

    /// <summary>Downloads new GitHub releases now and every 30 minutes (2 of GitHub's 60 unauthenticated calls an hour), and hands <paramref name="ready"/> the restart that
    /// applies one. Only John runs it (the tray's Restart to update): a restart drops every agent's pipe to the core.</summary>
    public static async Task KeepUpdated(Action<Action> ready)
    {
        offer = ready;
        if (!Updates.IsInstalled) return; // a dev build
        if (Updates.UpdatePendingRestart is { } pending) ready(() => Updates.ApplyUpdatesAndRestart(pending));
        using var timer = new PeriodicTimer(TimeSpan.FromMinutes(30));
        do
            try { await Check(); }
            catch (Exception e) { Log.Warn($"update check failed: {e.Message}"); }
        while (await timer.WaitForNextTickAsync());
    }

    static UpdateManager? updates;
    static UpdateManager Updates => updates ??= new(new GithubSource(Repo, Environment.GetEnvironmentVariable("AGENTDESK_GITHUB_TOKEN"), false));
    static readonly SemaphoreSlim Checking = new(1, 1);
    static Action<Action> offer = _ => { };
    static string? latest;
    static VelopackAsset? staged;

    /// <summary>Checks GitHub now and downloads a newer release, if there is one; Velopack never offers an older one.</summary>
    static async Task<bool> Check()
    {
        await Checking.WaitAsync();
        try
        {
            var next = await Updates.CheckForUpdatesAsync();
            latest = next?.TargetFullRelease.Version.ToString() ?? Updates.CurrentVersion?.ToString();
            if (next is null) return false;
            await Updates.DownloadUpdatesAsync(next);
            var asset = staged = next.TargetFullRelease;
            offer(() => Updates.ApplyUpdatesAndRestart(asset));
            return true;
        }
        finally { Checking.Release(); }
    }

    /// <summary>ui:update: checks and downloads now, and with <paramref name="apply"/> restarts into a ready update after replying.
    /// A dev build (not installed) only reports that.</summary>
    public static async Task<string> Update(Args args, string source)
    {
        var apply = args.Bool("apply", false);
        Log.Info($"update{(apply ? " and restart" : "")} requested by {source}");
        var downloaded = false;
        try { downloaded = Updates.IsInstalled && await Check(); }
        catch (Exception e) { Log.Warn($"update check failed: {e.Message}"); return Tools.Error($"update check failed: {e.Message}"); }
        var state = State();
        var restart = apply && Pending() is not null;
        if (restart)
            _ = Task.Run(async () =>
            {
                await Task.Delay(1000); // the reply goes out first
                Log.Info($"restarting into {Pending()!.Version} for {source}");
                Tray.Hide();
                Updates.ApplyUpdatesAndRestart(Pending(), ["--background"]);
            });
        state["downloaded"] = downloaded;
        state["restarting"] = restart;
        return state.ToJsonString(Wire.Indented);
    }

    static VelopackAsset? Pending() => Updates.IsInstalled ? staged ?? Updates.UpdatePendingRestart : null;

    /// <summary>What the core runs and what it could restart into; <c>latest</c> is as of the last check (null before one).</summary>
    public static JsonObject State() => new()
    {
        ["installed"] = Updates.IsInstalled,
        ["current"] = Updates.CurrentVersion?.ToString() ?? typeof(Setup).Assembly.GetName().Version?.ToString(3),
        ["latest"] = latest,
        ["pending"] = Pending()?.Version.ToString(),
    };

    static void Register(bool on)
    {
        using (var run = Registry.CurrentUser.CreateSubKey(RunKey))
            if (on) run.SetValue("AgentDesk", $"\"{Environment.ProcessPath}\" --background"); else run.DeleteValue("AgentDesk", false);

        Edit(Path.Combine(Home, ".claude.json"), root =>
        {
            var servers = (root["mcpServers"] ??= new JsonObject()).AsObject();
            servers.Remove("agentdesk");
            if (on) servers["agentdesk"] = new JsonObject { ["type"] = "stdio", ["command"] = Cli, ["args"] = new JsonArray(), ["env"] = new JsonObject() };
        });

        Edit(Path.Combine(Home, ".claude", "settings.json"), root =>
        {
            var hooks = (root["hooks"] ??= new JsonObject()).AsObject();
            foreach (var (ev, groups) in hooks.ToList()) // drop every agentdesk hook, wherever its exe lived
            {
                foreach (var group in groups!.AsArray().ToList())
                {
                    var list = group!["hooks"]!.AsArray();
                    foreach (var h in list.Where(h => h!["command"]?.GetValue<string>().Contains("agentdesk.exe\" hook ") == true).ToList()) list.Remove(h);
                    if (list.Count == 0) groups.AsArray().Remove(group);
                }
                if (groups.AsArray().Count == 0) hooks.Remove(ev);
            }
            if (on)
                foreach (var (ev, arg, timeout, status) in ClaudeHooks)
                    (hooks[ev] ??= new JsonArray()).AsArray().Add((JsonNode)new JsonObject
                    {
                        ["hooks"] = new JsonArray(new JsonObject { ["type"] = "command", ["command"] = $"\"{Cli}\" hook {arg}", ["timeout"] = timeout, ["statusMessage"] = status }),
                    });
        });
    }

    static void Edit(string path, Action<JsonObject> change)
    {
        var root = File.Exists(path) ? JsonNode.Parse(File.ReadAllText(path))!.AsObject() : new JsonObject();
        change(root);
        File.WriteAllText(path, root.ToJsonString(new JsonSerializerOptions { WriteIndented = true, Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping }));
    }
}
