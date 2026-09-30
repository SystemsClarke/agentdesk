using System.Globalization;
using System.Text.Json.Nodes;
using AgentDesk.Contracts;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>
/// Recurring jobs: a prompt that runs itself on a schedule (every morning at 08:00, weekdays, every two hours), so a chore that
/// used to wait for someone to type "go" happens whether or not anyone is at the keyboard. Each run is a fresh session of the
/// job's identity (same name, new conversation) that does the work and retires. The core knows nothing about what a job does:
/// the prompt is opaque, so it can be a chore of any kind, or "read this file and follow it".
/// A job is either daily-style (<c>at</c> HH:mm local time on <c>days</c>: daily, weekdays, weekends, or a list such as mon,thu)
/// or interval-style (<c>every_minutes</c>). A run that is due while the previous one is still going is skipped, not stacked. A run
/// missed because the core was down fires once on the way back up if it is under <c>catch_up_minutes</c> late; older, it is skipped.
/// </summary>
public sealed class Jobs(BoardStore store, Identities ids)
{
    const string Suffix = "\n\nThis is a scheduled run of the recurring job `{0}`: nobody is at the keyboard. Do the work above, "
        + "post what you found or changed to the board as usual, then call retire so the slot is free for the next run.";
    static readonly string[] Days = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

    readonly Lock gate = new();
    /// <summary>Local time zone; a test may pin one.</summary>
    public TimeZoneInfo Zone = TimeZoneInfo.Local;
    public Func<DateTimeOffset> Now = () => DateTimeOffset.UtcNow;

    /// <summary>The first run strictly after <paramref name="after"/>.</summary>
    public static DateTimeOffset Next(DateTimeOffset after, string? at, string days, double? everyMinutes, TimeZoneInfo zone)
    {
        if (everyMinutes is { } m) return after.AddMinutes(m);
        var (h, mi) = ParseAt(at ?? throw new ArgumentException("give either at (HH:mm) or every_minutes"));
        var allowed = ParseDays(days);
        var local = TimeZoneInfo.ConvertTime(after, zone);
        for (var d = 0; d <= 8; d++)
        {
            var day = local.Date.AddDays(d);
            if (!allowed.Contains(day.DayOfWeek)) continue;
            var wall = day.AddHours(h).AddMinutes(mi);
            if (zone.IsInvalidTime(wall)) wall = wall.AddHours(1); // the spring-forward gap: run just after it
            var when = new DateTimeOffset(wall, zone.GetUtcOffset(wall));
            if (when > after) return when.ToUniversalTime();
        }
        throw new ArgumentException($"days matches no day: {days}");
    }

    static (int H, int M) ParseAt(string at) =>
        TimeOnly.TryParseExact(at.Trim(), ["H:mm", "HH:mm"], CultureInfo.InvariantCulture, DateTimeStyles.None, out var t) ? (t.Hour, t.Minute)
            : throw new ArgumentException($"at must be HH:mm (24-hour, local time), got {at}");

    static HashSet<DayOfWeek> ParseDays(string days)
    {
        var set = days.Trim().ToLowerInvariant() switch
        {
            "" or "daily" or "everyday" => Days.Select((_, i) => (DayOfWeek)i).ToHashSet(),
            "weekdays" => [DayOfWeek.Monday, DayOfWeek.Tuesday, DayOfWeek.Wednesday, DayOfWeek.Thursday, DayOfWeek.Friday],
            "weekends" => [DayOfWeek.Saturday, DayOfWeek.Sunday],
            var list => list.Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries)
                .Select(d => Array.IndexOf(Days, d.Length >= 3 ? d[..3] : d) is var i and >= 0 ? (DayOfWeek)i
                    : throw new ArgumentException($"unknown day: {d} (use daily, weekdays, weekends or mon,tue,...)"))
                .ToHashSet(),
        };
        return set.Count > 0 ? set : throw new ArgumentException("days matches no day");
    }

    public Task<string> Create(string name, string folder, string prompt, string? at, string? days, double? everyMinutes, string? model, double? catchUpMinutes)
    {
        if (string.IsNullOrWhiteSpace(name) || name.Any(c => !(char.IsLetterOrDigit(c) || c is '-' or '_' or '.'))) throw new ArgumentException("name is required: letters, digits, - _ .");
        if (string.IsNullOrWhiteSpace(prompt)) throw new ArgumentException("prompt is required");
        if (!Directory.Exists(folder)) throw new ArgumentException($"no such folder: {folder}");
        if ((at is null) == (everyMinutes is null)) throw new ArgumentException("give exactly one of at (HH:mm local time) or every_minutes");
        if (everyMinutes is < 1) throw new ArgumentException("every_minutes must be at least 1");
        model = string.IsNullOrWhiteSpace(model) ? "sonnet" : model.Trim().ToLowerInvariant();
        if (!Governor.Models.Contains(model)) throw new ArgumentException($"model must be haiku, sonnet or opus, got {model}");
        days = string.IsNullOrWhiteSpace(days) ? "daily" : days.Trim().ToLowerInvariant();
        var next = Next(Now(), at, days, everyMinutes, Zone); // validates at and days
        lock (gate)
        {
            using var db = store.Open();
            if (db.Scalar("SELECT 1 FROM recurring_jobs WHERE name=$n", ("n", name)) is not null) throw new ArgumentException($"job already exists: {name}");
            db.Exec("INSERT INTO recurring_jobs (name, folder, prompt, model, at, days, every_minutes, catch_up_minutes, next_run_ts, created_ts) VALUES ($n,$f,$p,$m,$a,$d,$e,$c,$x,$ts)",
                ("n", name), ("f", folder), ("p", prompt), ("m", model), ("a", at), ("d", days), ("e", everyMinutes), ("c", catchUpMinutes ?? 240), ("x", Iso(next)), ("ts", db.NowIso()));
            return Task.FromResult(Need(db, name).ToJsonString(Wire.Indented));
        }
    }

    public Task<string> List()
    {
        using var db = store.Open();
        return Task.FromResult(new JsonObject { ["jobs"] = new JsonArray([.. db.Rows("SELECT * FROM recurring_jobs ORDER BY name")]) }.ToJsonString(Wire.Indented));
    }

    public Task<string> Enable(string name, bool on)
    {
        lock (gate)
        {
            using var db = store.Open();
            var job = Need(db, name);
            // Turning it on schedules from now: it does not "catch up" the days it was off.
            var next = on ? Iso(Next(Now(), job["at"]?.ToString(), job["days"]!.ToString(), Every(job), Zone)) : null;
            db.Exec("UPDATE recurring_jobs SET enabled=$e, next_run_ts=$x WHERE name=$n", ("e", on ? 1 : 0), ("x", next), ("n", name));
            return Task.FromResult(Need(db, name).ToJsonString(Wire.Indented));
        }
    }

    public Task<string> Delete(string name)
    {
        lock (gate)
        {
            using var db = store.Open();
            Need(db, name);
            db.Exec("DELETE FROM recurring_jobs WHERE name=$n", ("n", name));
            return Task.FromResult(new JsonObject { ["deleted"] = name }.ToJsonString(Wire.Indented));
        }
    }

    /// <summary>Runs it now, off schedule (the next scheduled run is unchanged). Returns the job with the outcome in last_status.</summary>
    public Task<string> RunNow(string name)
    {
        Fire(name, "manual");
        using var db = store.Open();
        return Task.FromResult(Need(db, name).ToJsonString(Wire.Indented));
    }

    /// <summary>Ticks at once (a job due while the core was down is caught up), then every <paramref name="every"/>.</summary>
    public async Task Run(TimeSpan every, CancellationToken ct = default)
    {
        using var timer = new PeriodicTimer(every);
        do
            try { Tick(); }
            catch (Exception e) { Log.Warn($"jobs tick failed: {e}"); }
        while (await timer.WaitForNextTickAsync(ct));
    }

    /// <summary>Fires every enabled job that is due, and moves each to its next run.</summary>
    public void Tick()
    {
        var now = Now();
        var fire = new List<string>();
        lock (gate)
        {
            using var db = store.Open();
            foreach (var job in db.Rows("SELECT * FROM recurring_jobs WHERE enabled=1 AND next_run_ts IS NOT NULL AND next_run_ts <= $now", ("now", Iso(now))))
            {
                var (n, when) = (job["name"]!.ToString(), DateTimeOffset.Parse(job["next_run_ts"]!.ToString(), CultureInfo.InvariantCulture));
                var late = (now - when).TotalMinutes;
                var next = Next(now, job["at"]?.ToString(), job["days"]!.ToString(), Every(job), Zone);
                db.Exec("UPDATE recurring_jobs SET next_run_ts=$x WHERE name=$n", ("x", Iso(next)), ("n", n));
                if (late > (Dbl(job["catch_up_minutes"]) ?? 240))
                    Status(db, n, $"missed: due {when:u}, {late:0} min late (core was down?); not run");
                else fire.Add(n);
            }
        }
        foreach (var n in fire) Fire(n, "schedule");
    }

    void Fire(string name, string why)
    {
        lock (gate)
        {
            using var db = store.Open();
            var job = Need(db, name);
            try
            {
                var state = db.Scalar("SELECT state FROM identities WHERE name=$n", ("n", name)) as string;
                if (state is "running" or "queued") { Status(db, name, $"skipped ({why}): previous run still {state}"); return; }
                if (state is null)
                    ids.Create(name, job["folder"]!.ToString(), $"Recurring job {name}.", null, false, model: job["model"]!.ToString()).GetAwaiter().GetResult();
                else db.Exec("UPDATE identities SET claude_session_id=NULL, generation=1, model=$m, folder=$f WHERE name=$n", // a fresh conversation every run
                    ("m", job["model"]!.ToString()), ("f", job["folder"]!.ToString()), ("n", name));
                ids.Start(name, job["prompt"] + string.Format(Suffix, name)).GetAwaiter().GetResult();
                db.Exec("UPDATE recurring_jobs SET last_run_ts=$ts, runs=runs+1 WHERE name=$n", ("ts", db.NowIso()), ("n", name));
                Status(db, name, $"started ({why})");
                Log.Info($"job {name}: started ({why})");
            }
            catch (Exception e) when (e is ArgumentException or IOException or InvalidOperationException)
            {
                Status(db, name, $"failed to start ({why}): {e.Message}");
                Log.Warn($"job {name}: failed to start: {e.Message}");
            }
        }
    }

    static void Status(BoardDb db, string name, string status) => db.Exec("UPDATE recurring_jobs SET last_status=$s WHERE name=$n", ("s", status), ("n", name));

    static double? Every(JsonObject job) => Dbl(job["every_minutes"]);

    static double? Dbl(JsonNode? n) => n is null ? null : Convert.ToDouble(n.GetValue<object>(), CultureInfo.InvariantCulture); // sqlite hands back long or double

    static string Iso(DateTimeOffset t) => t.UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'", CultureInfo.InvariantCulture);

    static JsonObject Need(BoardDb db, string name) =>
        db.Rows("SELECT * FROM recurring_jobs WHERE name=$n", ("n", name)) is [var row] ? row : throw new ArgumentException($"no such job: {name}");
}
