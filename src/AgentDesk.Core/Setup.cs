// Install, update and uninstall, via Velopack. Setup.exe runs the installed core with a hook argument;
// the hook points Claude Code and the login Run key at the installed exes, or takes them back out.
// The data in %LOCALAPPDATA%\AgentDesk is never touched.
using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Win32;
using Velopack;
using Velopack.Sources;

namespace AgentDesk.Core;

static class Setup
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

    /// <summary>Runs a Velopack hook and exits if this launch is one; applies a downloaded update otherwise.</summary>
    public static void Run() => VelopackApp.Build()
        .OnAfterInstallFastCallback(_ => Register(true))
        .OnAfterUpdateFastCallback(_ => Register(true))
        .OnBeforeUninstallFastCallback(_ => Register(false))
        .Run();

    /// <summary>Downloads new GitHub releases now and every four hours; Run() applies them at the next start.</summary>
    public static async Task KeepUpdated()
    {
        var updates = new UpdateManager(new GithubSource(Repo, Environment.GetEnvironmentVariable("AGENTDESK_GITHUB_TOKEN"), false));
        if (!updates.IsInstalled) return; // a dev build
        using var timer = new PeriodicTimer(TimeSpan.FromHours(4));
        do
            try { if (await updates.CheckForUpdatesAsync() is { } next) await updates.DownloadUpdatesAsync(next); }
            catch (Exception e) { Log.Warn($"update check failed: {e.Message}"); }
        while (await timer.WaitForNextTickAsync());
    }

    static void Register(bool on)
    {
        using (var run = Registry.CurrentUser.CreateSubKey(RunKey))
            if (on) run.SetValue("AgentDesk", $"\"{Environment.ProcessPath}\""); else run.DeleteValue("AgentDesk", false);

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
