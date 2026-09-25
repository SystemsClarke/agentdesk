using System.Diagnostics;
using System.Globalization;
using System.Text;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;
using AgentDesk.Core.Host;

namespace AgentDesk.Core;

/// <summary>Claude plan usage (agentdesk/usage.py): claude_usage.json in the data folder holds the 5-hour and weekly
/// windows (from `claude -p /usage`) and the monthly spend (from the status-line feeder).</summary>
public static partial class Usage
{
    static readonly CultureInfo Inv = CultureInfo.InvariantCulture;
    static readonly string[] Months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

    [GeneratedRegex(@"^\s*(\w{3}) (\d{1,2}), (\d{1,2})(?::(\d{2}))?\s*([ap]m)")] private static partial Regex ResetRe();

    public static string Span(double seconds)
    {
        var t = TimeSpan.FromSeconds(Math.Max(0, (long)seconds));
        return t.Days > 0 ? $"{t.Days}d {t.Hours}h" : t.Hours > 0 ? $"{t.Hours}h {t.Minutes:00}m" : $"{t.Minutes}m";
    }

    internal static DateTimeOffset? Ts(JsonNode? v) => v is not JsonValue j ? null
        : j.TryGetValue(out double n) ? DateTimeOffset.FromUnixTimeMilliseconds((long)(n > 1e11 ? n : n * 1000))
        : DateTimeOffset.TryParse(j.ToString(), Inv, DateTimeStyles.AssumeUniversal, out var t) ? t : null;

    internal static bool Num(JsonNode? n, out double v) { v = 0; return n is JsonValue j && double.TryParse(j.ToString(), NumberStyles.Float, Inv, out v); }

    /// <summary>What the window shows: "lines" rotate on the main menu prompt, "summary" is SysOp's Claude plan row.</summary>
    public static JsonObject Report(string file, DateTimeOffset now)
    {
        if (AgentBoard.Load(file) is not { Count: > 0 } d)
            return new() { ["lines"] = new JsonArray(), ["summary"] = "no feed yet (add scripts/claude_usage_feed.py to your status line)" };
        string In(DateTimeOffset r) => Span((r - now).TotalSeconds);
        var (five, week) = (d["five_hour"] as JsonObject, d["seven_day"] as JsonObject);
        var captured = Ts(d["captured_ts"]);
        var stale = captured is { } c && (now - c).TotalMinutes > 30 ? $", as of {Span((now - c).TotalSeconds)} ago" : "";
        var lines = new List<string>();
        var reset = Ts(five?["resets_at"]);
        if (reset <= now) lines.Add("time left: fresh 5-hour window, the meter just reset");
        else if (five is { Count: > 0 }) lines.Add($"time left: {(reset is { } r ? In(r) : "unknown")} in your 5-hour window, {five["used"]?.ToString() ?? "?"}% used{stale}");
        var wreset = Ts(week?["resets_at"]);
        if (Num(week?["used"], out var wused) && wused >= 75 && !(wreset <= now))
            lines.Add($"time left: {(wreset is { } w ? In(w) : "unknown")} on the week, {week!["used"]}% used, getting close{stale}");
        var spend = "";
        if (d["extra"] is JsonObject x && Num(x["spent"], out var spent) && Num(x["limit"], out var limit))
        {
            var pct = limit != 0 ? (int)Math.Round(100 * spent / limit) : 0;
            var age = Ts(x["captured_ts"]) is { } a && (now - a).TotalSeconds > 3600 ? $", as of {Span((now - a).TotalSeconds)} ago" : "";
            spend = $"monthly spend: ${spent.ToString("N2", Inv)} of ${limit.ToString("N0", Inv)} ({pct}%){(pct >= 90 ? " · nearly capped" : "")}{age}";
            lines.Add(spend);
        }
        var bits = new List<string>();
        foreach (var (win, label) in new[] { (five, "5h"), (week, "week") })
            if (win is { Count: > 0 })
                bits.Add($"{label} {win["used"]?.ToString() ?? "?"}%" + (Ts(win["resets_at"]) is { } r && r > now ? $", resets in {In(r)}" : ""));
        if (spend.Length > 0) bits.Add(spend.Replace("monthly spend: ", "spend "));
        if (captured is { } cap) bits.Add($"reported {Span((now - cap).TotalSeconds)} ago");
        return new() { ["lines"] = new JsonArray([.. lines.Select(l => (JsonNode?)l)]), ["summary"] = bits.Count > 0 ? string.Join(" · ", bits) : "feed has no plan limits in it" };
    }

    /// <summary>The claude CLI, as shutil.which finds it, else its default install path.</summary>
    public static string? ClaudeExe() => (Environment.GetEnvironmentVariable("PATH") ?? "").Split(';', StringSplitOptions.RemoveEmptyEntries)
        .SelectMany(d => new[] { Path.Combine(d, "claude.exe"), Path.Combine(d, "claude.cmd") })
        .Append(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".local", "bin", "claude.exe")).FirstOrDefault(File.Exists);

    /// <summary>Every 5 minutes, window or not, because the governor samples every reading (<paramref name="sampled"/>);
    /// a subscribed window is told when it lands.</summary>
    public static async Task KeepFresh(string file, BoardWatch watch, Action? sampled = null)
    {
        var last = DateTime.MinValue;
        while (true)
        {
            await Task.Delay(TimeSpan.FromSeconds(5));
            if (DateTime.UtcNow - last < TimeSpan.FromMinutes(5)) continue;
            last = DateTime.UtcNow;
            try
            {
                var fresh = await Task.Run(() => Refresh(file));
                sampled?.Invoke(); // the status-line feeder may have written a reading even when this one failed
                if (fresh) await watch.Notify();
            }
            catch (Exception e) { Log.Warn($"usage refresh failed: {e}"); }
        }
    }

    /// <summary>Ask the claude CLI for plan usage (no hooks, no model call, ~10 s). True if the feed was updated.</summary>
    static bool Refresh(string file)
    {
        if (ClaudeExe() is not { } exe) return false;
        var psi = new ProcessStartInfo(exe) { WorkingDirectory = Path.GetTempPath(), UseShellExecute = false, CreateNoWindow = true,
            RedirectStandardOutput = true, StandardOutputEncoding = Encoding.UTF8 };
        foreach (var a in new[] { "-p", "/usage", "--setting-sources", "project" }) psi.ArgumentList.Add(a);
        using var p = Process.Start(psi)!;
        var output = p.StandardOutput.ReadToEndAsync();
        if (!p.WaitForExit(60_000)) { p.Kill(true); return false; }
        return Apply(file, output.Result, DateTimeOffset.UtcNow);
    }

    /// <summary>Fold `/usage` output into the feed, keeping what else it holds (the spend). False if it named no window.</summary>
    public static bool Apply(string file, string output, DateTimeOffset now)
    {
        var d = AgentBoard.Load(file) ?? [];
        var found = false;
        foreach (var (key, label) in new[] { ("five_hour", "Current session"), ("seven_day", "Current week (all models)") })
            if (Regex.Match(output, Regex.Escape(label) + @": (\d+)% used(?: · resets (.+))?") is { Success: true } m && (found = true))
                d[key] = new JsonObject { ["used"] = int.Parse(m.Groups[1].Value, Inv), ["resets_at"] = ParseReset(m.Groups[2].Value, now) };
        if (!found) return false;
        d["source"] = "claude -p /usage";
        d["captured_ts"] = now.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'", Inv);
        File.WriteAllText(file + ".tmp", d.ToJsonString(Wire.Indented));
        File.Move(file + ".tmp", file, true);
        return true;
    }

    /// <summary>"Sep 26, 3:59am (America/New_York)" or "Sep 26, 4am", read as this machine's local time, to ISO UTC.</summary>
    static string? ParseReset(string text, DateTimeOffset now)
    {
        var m = ResetRe().Match(text);
        var month = Array.IndexOf(Months, m.Groups[1].Value) + 1;
        if (!m.Success || month == 0) return null;
        var hour = int.Parse(m.Groups[3].Value, Inv) % 12 + (m.Groups[5].Value == "pm" ? 12 : 0);
        var local = new DateTime(now.Year, month, int.Parse(m.Groups[2].Value, Inv), hour, m.Groups[4].Success ? int.Parse(m.Groups[4].Value, Inv) : 0, 0, DateTimeKind.Local);
        if ((local - now.LocalDateTime).TotalDays < -30) local = local.AddYears(1); // "Jan 2" read in late December
        return new DateTimeOffset(local).ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'", Inv);
    }
}
