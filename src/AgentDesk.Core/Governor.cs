using System.Globalization;
using System.Text.Json.Nodes;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>One `/usage` reading: weekly and 5-hour percentages, their resets, and how many identity sessions ran then.</summary>
public sealed record UsageSample(DateTimeOffset Ts, double WeeklyPct, DateTimeOffset? WeeklyReset, double? FiveHourPct, DateTimeOffset? FiveHourReset, int SwarmSessions);

/// <summary>settings.json's governor_* keys. The defaults are conservative: they assume John is busy and sessions are expensive
/// until the samples say otherwise.</summary>
public sealed record GovernorSettings(double K = 2, double Margin = 2, double DefaultRate = 0.3, double DefaultSigma = 0.5,
    double DefaultSessionRate = 3, int MaxSessions = 12, int MaxMembers = 4, int MaxSwarms = 10)
{
    public static GovernorSettings From(JsonObject? s)
    {
        double D(string key, double d) => double.TryParse(s?[key]?.ToString(), NumberStyles.Float, CultureInfo.InvariantCulture, out var v) && v >= 0 ? v : d;
        int I(string key, int d) => int.TryParse(s?[key]?.ToString(), out var v) && v >= 0 ? v : d;
        var g = new GovernorSettings();
        return new(D("governor_k", g.K), D("governor_margin", g.Margin), D("governor_default_rate", g.DefaultRate), D("governor_default_sigma", g.DefaultSigma),
            D("governor_default_session_rate", g.DefaultSessionRate), I("governor_max_sessions", g.MaxSessions), I("governor_max_members", g.MaxMembers),
            I("governor_max_swarms", g.MaxSwarms));
    }
}

/// <summary>John's own burn in weekly-% per hour: an EWMA per hour of week (UTC), a global EWMA and variance behind it, and
/// the measured burn of one identity session on top of John.</summary>
public sealed class BurnModel
{
    public readonly double[] Mean = new double[168], Var = new double[168];
    public readonly int[] N = new int[168];
    public double GlobalMean, GlobalVar, SessionRate;
    public int GlobalN, SessionN, BaselineHours;

    /// <summary>A bucket needs two observed hours before it outweighs the global rate, which needs six before it outweighs the default.</summary>
    public double Rate(int hourOfWeek, GovernorSettings s) => N[hourOfWeek] >= 2 ? Mean[hourOfWeek] : GlobalN >= 6 ? GlobalMean : s.DefaultRate;

    /// <summary>Per-hour sigma, from the global variance: it includes the daily pattern, so it errs high (a bigger reserve).</summary>
    public double Sigma(GovernorSettings s) => GlobalN >= 6 ? Math.Max(0.1, Math.Sqrt(GlobalVar)) : s.DefaultSigma;

    public double PerSession(GovernorSettings s) => SessionN >= 3 ? Math.Max(0.25, SessionRate) : s.DefaultSessionRate;
}

/// <summary>What the governor recommends now. Advisory: nothing reads it to enforce anything yet (milestone 7).</summary>
public sealed record Advice(double Used, double Remaining, double ResetInHours, double Baseline, double Sigma, double Reserve, double Spendable,
    double AllowedRate, double SessionRate, int Running, int TotalSessions, int NewSessions, int Swarms, int MembersPerSwarm, double FiveHourPct,
    bool StepDown, string LeadModel, string MemberModel, double ProjectedEnd, string Reason);

/// <summary>
/// The usage governor (docs/GOAL.md milestone 3, advisory): records `/usage` samples, forecasts John's own burn until the weekly
/// reset, and turns what is left into recommended caps with spendable = remaining - E[baseline] - k*sigma*sqrt(T). The reserve
/// shrinks as the reset nears, which is the ramp-up: cautious early, flat out at the end, finishing near 100%.
/// </summary>
public static class Governor
{
    const double BucketAlpha = 0.5, GlobalAlpha = 0.02, SessionAlpha = 0.2;
    static readonly CultureInfo Inv = CultureInfo.InvariantCulture;

    public const string Schema = "CREATE TABLE IF NOT EXISTS usage_samples (id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL UNIQUE, weekly_pct REAL NOT NULL,"
        + " weekly_reset_ts TEXT, five_hour_pct REAL, five_hour_reset_ts TEXT, swarm_sessions INTEGER NOT NULL DEFAULT 0);";

    public static string[] Models => ["haiku", "sonnet", "opus"];

    /// <summary>The formula, floored at 0.</summary>
    public static double Spendable(double remaining, double baseline, double sigma, double hours, double k) =>
        Math.Max(0, remaining - baseline - k * sigma * Math.Sqrt(Math.Max(0, hours)));

    /// <summary>Monday 00:00 UTC is 0.</summary>
    public static int HourOfWeek(DateTimeOffset t) => (int)(((t.ToUnixTimeSeconds() / 3600 + 72) % 168 + 168) % 168);

    static void Ewma(ref double mean, ref double var, ref int n, double x, double alpha)
    {
        var a = Math.Max(alpha, 1.0 / ++n); // a running mean until 1/alpha observations, so an early variance is not near zero
        var d = x - mean;
        mean += a * d;
        var = (1 - a) * (var + a * d * d);
    }

    static bool SameWeek(UsageSample a, UsageSample b) => a.WeeklyReset is not { } ra || b.WeeklyReset is not { } rb || Math.Abs((ra - rb).TotalHours) < 1;

    /// <summary>Samples become hours (weekly-% per hour, needing half an hour of coverage). Hours with no identity session running
    /// train John's baseline; hours with some measure a session as (rate - baseline) / sessions.</summary>
    public static BurnModel Train(IReadOnlyList<UsageSample> samples, GovernorSettings s)
    {
        var m = new BurnModel();
        var hours = new SortedDictionary<long, (double Delta, double Dt, double SessionHours, bool Swarm)>();
        for (var i = 1; i < samples.Count; i++)
        {
            var (a, b) = (samples[i - 1], samples[i]);
            var dt = (b.Ts - a.Ts).TotalHours;
            if (dt <= 0 || dt > 1 || b.WeeklyPct < a.WeeklyPct || !SameWeek(a, b)) continue; // a gap or a reset says nothing about a rate
            var key = a.Ts.ToUnixTimeSeconds() / 3600;
            hours.TryGetValue(key, out var h);
            hours[key] = (h.Delta + b.WeeklyPct - a.WeeklyPct, h.Dt + dt, h.SessionHours + (a.SwarmSessions + b.SwarmSessions) / 2.0 * dt,
                h.Swarm || a.SwarmSessions + b.SwarmSessions > 0);
        }
        var busy = new List<(int How, double Rate, double Sessions)>();
        foreach (var (key, h) in hours)
        {
            if (h.Dt < 0.5) continue;
            var (rate, how) = (h.Delta / h.Dt, HourOfWeek(DateTimeOffset.FromUnixTimeSeconds(key * 3600)));
            if (h.Swarm) { busy.Add((how, rate, h.SessionHours / h.Dt)); continue; }
            m.BaselineHours++;
            Ewma(ref m.Mean[how], ref m.Var[how], ref m.N[how], rate, BucketAlpha);
            Ewma(ref m.GlobalMean, ref m.GlobalVar, ref m.GlobalN, rate, GlobalAlpha);
        }
        foreach (var (how, rate, sessions) in busy)
        {
            if (sessions < 0.5) continue;
            var unused = 0.0;
            Ewma(ref m.SessionRate, ref unused, ref m.SessionN, Math.Clamp((rate - m.Rate(how, s)) / sessions, 0, 20), SessionAlpha);
        }
        return m;
    }

    /// <summary>E[John's burn] from now until the reset, hour bucket by hour bucket.</summary>
    public static double Baseline(BurnModel m, DateTimeOffset now, DateTimeOffset reset, GovernorSettings s)
    {
        var sum = 0.0;
        for (var t = now; t < reset;)
        {
            var next = DateTimeOffset.FromUnixTimeSeconds((t.ToUnixTimeSeconds() / 3600 + 1) * 3600);
            var end = next < reset ? next : reset;
            sum += m.Rate(HourOfWeek(t), s) * (end - t).TotalHours;
            t = end;
        }
        return sum;
    }

    public static Advice Advise(BurnModel m, UsageSample latest, int running, DateTimeOffset now, GovernorSettings s)
    {
        var (used, reset) = (latest.WeeklyPct, latest.WeeklyReset ?? now.AddDays(7)); // no reset time: assume the longest week
        while (reset <= now) (used, reset) = (0, reset.AddDays(7)); // the week turned over since the sample
        var hours = Math.Max((reset - now).TotalHours, 1 / 60.0);
        var remaining = Math.Max(0, 100 - used);
        var baseline = Baseline(m, now, reset, s);
        var sigma = m.Sigma(s);
        var reserve = s.K * sigma * Math.Sqrt(hours);
        var spendable = Spendable(remaining - s.Margin, baseline, sigma, hours, s.K);
        var rate = spendable / hours;
        var per = m.PerSession(s);
        var five = latest.FiveHourReset is { } f && f <= now ? 0 : latest.FiveHourPct ?? 0;
        var affordable = rate / per;
        var total = (int)Math.Min(s.MaxSessions, Math.Floor(affordable + 1e-9));
        var newSessions = Math.Max(0, total - running);
        string reason;
        if (five >= 90) { (total, newSessions) = (Math.Min(total, running), 0); reason = $"the 5-hour window is at {five:0}%: no new sessions until it resets"; }
        else if (spendable <= 0) reason = $"nothing spendable: John's forecast {baseline:0.#}% plus a {reserve:0.#}% reserve covers the {remaining:0.#}% left";
        else if (total == 0) reason = $"{spendable:0.#}% spendable over {Usage.Span(hours * 3600)} funds {affordable:0.00} sessions at {per:0.##}%/session-hour; the reserve shrinks as the reset nears";
        else reason = $"{spendable:0.#}% spendable over {Usage.Span(hours * 3600)}: {rate:0.00}%/h funds {total} sessions at {per:0.##}%/session-hour"
            + (total == s.MaxSessions && affordable >= s.MaxSessions + 1 ? " (held at governor_max_sessions)" : "");
        var members = total <= 1 ? 0 : Math.Min(s.MaxMembers, total - 1);
        var swarms = total == 0 ? 0 : Math.Min(s.MaxSwarms, total / (1 + members));
        var stepDown = affordable < 1 || five >= 75; // tight: members drop to haiku and leads to sonnet
        return new(used, remaining, hours, baseline, sigma, reserve, spendable, rate, per, running, total, newSessions, swarms, members, five,
            stepDown, stepDown ? "sonnet" : "opus", stepDown ? "haiku" : "sonnet", Math.Min(100, used + baseline + Math.Min(spendable, total * per * hours)), reason);
    }

    // ---- the board

    static string Iso(DateTimeOffset t) => t.UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'", Inv);

    static DateTimeOffset? Time(JsonNode? n) => n?.ToString() is { Length: > 0 } s && DateTimeOffset.TryParse(s, Inv, DateTimeStyles.AssumeUniversal, out var t) ? t : null;

    public static void Insert(BoardDb db, UsageSample x) =>
        db.Exec("INSERT OR IGNORE INTO usage_samples (ts, weekly_pct, weekly_reset_ts, five_hour_pct, five_hour_reset_ts, swarm_sessions) VALUES ($ts,$w,$wr,$f,$fr,$n)",
            ("ts", Iso(x.Ts)), ("w", x.WeeklyPct), ("wr", x.WeeklyReset is { } wr ? Iso(wr) : null), ("f", x.FiveHourPct),
            ("fr", x.FiveHourReset is { } fr ? Iso(fr) : null), ("n", x.SwarmSessions));

    public static List<UsageSample> Load(BoardDb db, DateTimeOffset since) =>
        [.. db.Rows("SELECT * FROM usage_samples WHERE ts >= $since ORDER BY ts", ("since", Iso(since))).Select(r => new UsageSample(
            Time(r["ts"])!.Value, r["weekly_pct"]!.GetValue<double>(), Time(r["weekly_reset_ts"]), r["five_hour_pct"]?.GetValue<double>(),
            Time(r["five_hour_reset_ts"]), (int)r["swarm_sessions"]!.GetValue<long>()))];

    static int Running(BoardDb db) => Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM identities WHERE state='running'"), Inv);

    /// <summary>The feed as it stands (claude_usage.json), with the identity sessions running now. Once per captured_ts; false if
    /// the feed has no weekly figure.</summary>
    public static bool Record(BoardStore store, string feed)
    {
        if (AgentBoard.Load(feed) is not { } d || Usage.Ts(d["captured_ts"]) is not { } ts || !Usage.Num(d["seven_day"]?["used"], out var weekly)) return false;
        double? five = Usage.Num(d["five_hour"]?["used"], out var f) ? f : null;
        using var db = store.Open();
        Insert(db, new(ts, weekly, Usage.Ts(d["seven_day"]?["resets_at"]), five, Usage.Ts(d["five_hour"]?["resets_at"]), Running(db)));
        return true;
    }

    /// <summary>ui:governor, and ui:status's governor section: the last five weeks of samples (the EWMAs have forgotten older ones).</summary>
    public static JsonObject Report(BoardDb db, string data, DateTimeOffset now)
    {
        var s = GovernorSettings.From(AgentBoard.Load(Path.Combine(data, "settings.json")));
        var samples = Load(db, now.AddDays(-35));
        if (samples.Count == 0)
            return new() { ["samples"] = 0, ["advisory"] = true, ["reason"] = "no usage samples yet", ["summary"] = "governor: no usage samples yet" };
        var m = Train(samples, s);
        var a = Advise(m, samples[^1], Running(db), now, s);
        static double R(double v) => Math.Round(v, 2);
        return new()
        {
            ["advisory"] = true, ["samples"] = samples.Count, ["baseline_hours"] = m.BaselineHours, ["session_hours"] = m.SessionN,
            ["sample_age_minutes"] = R((now - samples[^1].Ts).TotalMinutes),
            ["used"] = R(a.Used), ["remaining"] = R(a.Remaining), ["reset_in_hours"] = R(a.ResetInHours), ["baseline"] = R(a.Baseline),
            ["sigma"] = R(a.Sigma), ["reserve"] = R(a.Reserve), ["k"] = s.K, ["spendable"] = R(a.Spendable), ["allowed_rate"] = R(a.AllowedRate),
            ["session_rate"] = R(a.SessionRate), ["projected_end_pct"] = R(a.ProjectedEnd), ["five_hour_pct"] = R(a.FiveHourPct),
            ["caps"] = new JsonObject { ["swarms"] = a.Swarms, ["members_per_swarm"] = a.MembersPerSwarm, ["total_sessions"] = a.TotalSessions,
                ["running"] = a.Running, ["new_sessions"] = a.NewSessions },
            ["models"] = new JsonObject { ["step_down"] = a.StepDown, ["lead"] = a.LeadModel, ["member"] = a.MemberModel },
            ["reason"] = a.Reason,
            ["summary"] = $"governor: {a.Spendable:0.#}% spendable of {a.Remaining:0.#}% left, resets in {Usage.Span(a.ResetInHours * 3600)}"
                + $" · up to {a.TotalSessions} sessions ({a.Swarms} swarm{(a.Swarms == 1 ? "" : "s")} x {a.MembersPerSwarm} members), {a.NewSessions} new"
                + $" · members {a.MemberModel}{(a.StepDown ? " (step down)" : "")}" + (a.FiveHourPct >= 90 ? " · 5h guard" : ""),
        };
    }

    public static Task<string> Ui(BoardStore store, string data)
    {
        using var db = store.Open();
        return Task.FromResult(Report(db, data, DateTimeOffset.UtcNow).ToJsonString(AgentDesk.Contracts.Wire.Indented));
    }
}
