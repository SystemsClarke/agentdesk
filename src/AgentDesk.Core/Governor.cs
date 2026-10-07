using System.Globalization;
using System.Text.Json.Nodes;
using AgentDesk.Core.Board;

namespace AgentDesk.Core;

/// <summary>One `/usage` reading: weekly and 5-hour percentages, their resets, and how many identity sessions ran then.</summary>
public sealed record UsageSample(DateTimeOffset Ts, double WeeklyPct, DateTimeOffset? WeeklyReset, double? FiveHourPct, DateTimeOffset? FiveHourReset, int SwarmSessions,
    int Haiku = 0, int Sonnet = 0, int Opus = 0);

/// <summary>settings.json's governor_* keys (governor_ramp and governor_floor shape the week: see <see cref="Governor.Pace"/>). The defaults are conservative: they assume John is busy and sessions are expensive
/// until the samples say otherwise.</summary>
public sealed record GovernorSettings(double K = 2, double Margin = 2, double DefaultRate = 0.3, double DefaultSigma = 0.5,
    double DefaultSessionRate = 3, int MaxSessions = 12, int MaxMembers = 4, int MaxSwarms = 10, bool Enforce = false, double Ramp = 1, double Floor = 0.6)
{
    public static GovernorSettings From(JsonObject? s)
    {
        double D(string key, double d) => double.TryParse(s?[key]?.ToString(), NumberStyles.Float, CultureInfo.InvariantCulture, out var v) && v >= 0 ? v : d;
        int I(string key, int d) => int.TryParse(s?[key]?.ToString(), out var v) && v >= 0 ? v : d;
        var g = new GovernorSettings();
        return new(D("governor_k", g.K), D("governor_margin", g.Margin), D("governor_default_rate", g.DefaultRate), D("governor_default_sigma", g.DefaultSigma),
            D("governor_default_session_rate", g.DefaultSessionRate), I("governor_max_sessions", g.MaxSessions), I("governor_max_members", g.MaxMembers),
            I("governor_max_swarms", g.MaxSwarms), s?["governor_enforce"]?.ToString().Equals("true", StringComparison.OrdinalIgnoreCase) == true,
            D("governor_ramp", g.Ramp), Math.Min(1, D("governor_floor", g.Floor)));
    }
}

/// <summary>John's own burn in weekly-% per hour: an EWMA per hour of week (UTC), a global EWMA and variance behind it, and
/// the measured burn of one identity session on top of John. When swarms run constantly there are few session-free hours, so a
/// long-memory EWMA over every hour (the Est* fields) estimates John as total rate minus sessions x the per-session burn.</summary>
public sealed class BurnModel
{
    public readonly double[] Mean = new double[168], Var = new double[168];
    public readonly int[] N = new int[168];
    public double GlobalMean, GlobalVar, SessionRate, EstMean, EstVar, Slope;
    public int GlobalN, SessionN, BaselineHours, EstN;
    /// <summary>The session count varied enough over the long memory to regress the hourly rate on it (<see cref="Slope"/>).</summary>
    public bool SlopeOk;

    /// <summary>A bucket needs two observed hours before it outweighs the global rate, which needs six before it outweighs the
    /// starvation estimate, which needs six before it outweighs the default. The global rate and the estimate start as plain
    /// means of the first hours seen, so a young one is shrunk toward the default (<see cref="PriorHours"/> hours of it): a
    /// first day that happens to be John's busiest must not become his rate for the whole week.</summary>
    public double Rate(int hourOfWeek, GovernorSettings s) =>
        N[hourOfWeek] >= 2 ? Mean[hourOfWeek] : GlobalN >= 6 ? Shrunk(GlobalMean, GlobalN, s) : EstN >= 6 ? Shrunk(EstMean, EstN, s) : s.DefaultRate;

    /// <summary>The weight, in observed hours, of the default rate a young global rate or estimate is shrunk toward.</summary>
    public const double PriorHours = 24;

    /// <summary>The least one identity session is taken to cost, in weekly-% per session-hour. John's own samples put a session far under
    /// the old 0.25 floor (a fleet of idle long-lived sessions burned about 0.08%/h each at the very most), and a floor above the real
    /// cost made the week's allowance round down to no sessions at all.</summary>
    public const double MinSessionRate = 0.05;

    static double Shrunk(double mean, int n, GovernorSettings s) => (n * mean + PriorHours * s.DefaultRate) / (n + PriorHours);

    /// <summary>Per-hour sigma, from the global variance: it includes the daily pattern, so it errs high (a bigger reserve).</summary>
    public double Sigma(GovernorSettings s) => GlobalN >= 6 ? Math.Max(0.1, Math.Sqrt(GlobalVar)) : EstN >= 6 ? Math.Max(0.1, Math.Sqrt(EstVar)) : s.DefaultSigma;

    /// <summary>Where John's rate comes from when a bucket is thin: measured session-free hours, the starvation estimate, or the default.</summary>
    public string Source => GlobalN >= 6 ? "measured" : EstN >= 6 ? "estimated" : "default";

    /// <summary>Measured against John's session-free hours; with too few of those (starved), the regression slope when the session
    /// count varied, since (rate - an assumed baseline) would only echo the default rate back.</summary>
    public double PerSession(GovernorSettings s) => GlobalN < 6 && SlopeOk ? Math.Clamp(Slope, MinSessionRate, 20)
        : SessionN >= 3 ? Math.Max(MinSessionRate, SessionRate) : s.DefaultSessionRate;
}

/// <summary>What the governor recommends now. Identities enforces it when settings.json's governor_enforce is true (milestone 7).</summary>
public sealed record Advice(double Used, double Remaining, double ResetInHours, double Baseline, double Sigma, double Reserve, double Spendable,
    double AllowedRate, double SessionRate, int Running, int TotalSessions, int NewSessions, int Swarms, int MembersPerSwarm, double FiveHourPct,
    bool StepDown, string LeadModel, string MemberModel, double ProjectedEnd, string Reason, double Pace = 1, double WeekElapsed = 0);

/// <summary>
/// The usage governor (docs/GOAL.md milestone 3, advisory): records `/usage` samples, forecasts John's own burn until the weekly
/// reset, and turns what is left into recommended caps with spendable = remaining - E[baseline] - k*sigma*sqrt(T). The reserve
/// shrinks as the reset nears, which is the ramp-up: cautious early, flat out at the end, finishing near 100%.
/// </summary>
public static class Governor
{
    const double BucketAlpha = 0.5, GlobalAlpha = 0.02, SessionAlpha = 0.2;
    /// <summary>The starvation estimate's EWMA: a 4-week half-life counted in observed hours (672).</summary>
    public static readonly double LongAlpha = 1 - Math.Pow(0.5, 1.0 / 672);
    /// <summary>A sample older than this, or a failing /usage, allows no new swarm sessions (fail closed).</summary>
    public const double StaleMinutes = 30;
    /// <summary>The most the reserve for John's noise holds back, in weekly %: a quarter of the plan.</summary>
    public const double MaxReserve = 25;
    static readonly CultureInfo Inv = CultureInfo.InvariantCulture;

    public const string Schema = "CREATE TABLE IF NOT EXISTS usage_samples (id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL UNIQUE, weekly_pct REAL NOT NULL,"
        + " weekly_reset_ts TEXT, five_hour_pct REAL, five_hour_reset_ts TEXT, swarm_sessions INTEGER NOT NULL DEFAULT 0);";

    /// <summary>usage_samples, plus the per-tier session counts (the model each running identity was launched at) that let
    /// per-tier burn be measured later.</summary>
    public static void Migrate(BoardDb db)
    {
        db.Exec(Schema);
        var cols = db.Rows("PRAGMA table_info(usage_samples)").Select(r => r["name"]!.ToString()).ToHashSet();
        foreach (var m in Models)
            if (!cols.Contains(m + "_sessions")) db.Exec($"ALTER TABLE usage_samples ADD COLUMN {m}_sessions INTEGER NOT NULL DEFAULT 0");
    }

    public static string[] Models => ["haiku", "sonnet", "opus"];

    /// <summary>The formula, floored at 0.</summary>
    public static double Spendable(double remaining, double baseline, double sigma, double hours, double k) =>
        Math.Max(0, remaining - baseline - k * sigma * Math.Sqrt(Math.Max(0, hours)));

    /// <summary>How much of an even spread of the spendable % the governor allows per hour at <paramref name="t"/> (0 the week's start,
    /// 1 its reset): <paramref name="floor"/> at the start, climbing to 1 at the reset. Spendable is recomputed from what is really
    /// left every time, so what early hours did not spend (the cushion) is spent later, harder: the end-of-week push. A ramp of 0 is the old
    /// flat spread; a bigger one holds back longer. m = (q+1) t^q (1-t) / (1-t^(q+1)) is 0 at the start and 1 at the reset.</summary>
    public static double Pace(double t, double ramp, double floor)
    {
        t = Math.Clamp(t, 0, 0.999);
        var m = ramp <= 0 ? 1 : (ramp + 1) * Math.Pow(t, ramp) * (1 - t) / (1 - Math.Pow(t, ramp + 1));
        return floor + (1 - floor) * Math.Min(1, m);
    }

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
        // Baseline starvation. First the long-memory regression of rate on sessions (exponential weights), which separates a session
        // from John whenever the session count varies; then every hour, oldest first, as John's rate = total rate - sessions x per session.
        double sw = 0, sx = 0, sy = 0, sxx = 0, sxy = 0, keep = 1 - LongAlpha;
        foreach (var (_, h) in hours)
            if (h.Dt >= 0.5)
            {
                var (x, y) = (h.SessionHours / h.Dt, h.Delta / h.Dt);
                (sw, sx, sy, sxx, sxy) = (keep * sw + 1, keep * sx + x, keep * sy + y, keep * sxx + x * x, keep * sxy + x * y);
            }
        var varX = sw > 0 ? sxx / sw - sx / sw * (sx / sw) : 0;
        (m.SlopeOk, m.Slope) = (hours.Count >= 6 && varX >= 0.25, varX > 0 ? (sxy / sw - sx / sw * (sy / sw)) / varX : 0);
        foreach (var (_, h) in hours)
            if (h.Dt >= 0.5)
                Ewma(ref m.EstMean, ref m.EstVar, ref m.EstN, Math.Max(0, h.Delta / h.Dt - h.SessionHours / h.Dt * m.PerSession(s)), LongAlpha);
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
        // John cannot burn more than what is left of the plan, and the reserve for his noise is never more than MaxReserve.
        var baseline = Math.Min(Baseline(m, now, reset, s), remaining);
        var sigma = m.Sigma(s);
        var reserve = Math.Min(s.K * sigma * Math.Sqrt(hours), Math.Min(MaxReserve, remaining));
        var spendable = Math.Max(0, remaining - s.Margin - baseline - reserve);
        var elapsed = Math.Clamp((now - reset.AddDays(-7)).TotalHours / 168, 0, 1);
        var pace = Pace(elapsed, s.Ramp, s.Floor);
        var rate = spendable / hours * pace;
        var per = m.PerSession(s);
        var five = latest.FiveHourReset is { } f && f <= now ? 0 : latest.FiveHourPct ?? 0;
        var affordable = rate / per;
        var total = (int)Math.Min(s.MaxSessions, Math.Floor(affordable + 1e-9));
        var newSessions = Math.Max(0, total - running);
        string reason;
        if (five >= 90) { (total, newSessions) = (0, 0); reason = $"the 5-hour window is at {five:0}%: no swarm sessions (shed them) until it resets"; }
        else if (spendable <= 0) reason = $"nothing spendable: John's forecast {baseline:0.#}% plus a {reserve:0.#}% reserve covers the {remaining:0.#}% left";
        else if (total == 0) reason = $"{spendable:0.#}% spendable over {Usage.Span(hours * 3600)} ({pace * 100:0}% of an even spread this far into the week) funds {affordable:0.00} sessions (about {affordable * 24:0} session-hours a day) at {per:0.##}%/session-hour; the reserve shrinks as the reset nears";
        else reason = $"{spendable:0.#}% spendable over {Usage.Span(hours * 3600)}, {pace * 100:0}% of an even spread this far into the week: {rate:0.00}%/h funds {total} sessions at {per:0.##}%/session-hour"
            + (total == s.MaxSessions && affordable >= s.MaxSessions + 1 ? " (held at governor_max_sessions)" : "");
        var members = total <= 1 ? 0 : Math.Min(s.MaxMembers, total - 1);
        var swarms = total == 0 ? 0 : Math.Min(s.MaxSwarms, total / (1 + members));
        var stepDown = affordable < 1 || five >= 75; // tight: members drop to haiku and leads to sonnet
        return new(used, remaining, hours, baseline, sigma, reserve, spendable, rate, per, running, total, newSessions, swarms, members, five,
            stepDown, stepDown ? "sonnet" : "opus", stepDown ? "haiku" : "sonnet", Math.Min(100, used + baseline + Math.Min(spendable, total * per * hours)), reason, pace, elapsed);
    }

    // ---- the board

    static string Iso(DateTimeOffset t) => t.UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss'+00:00'", Inv);

    static DateTimeOffset? Time(JsonNode? n) => n?.ToString() is { Length: > 0 } s && DateTimeOffset.TryParse(s, Inv, DateTimeStyles.AssumeUniversal, out var t) ? t : null;

    public static void Insert(BoardDb db, UsageSample x, (int Haiku, int Sonnet, int Opus) tiers = default) =>
        db.Exec("INSERT OR IGNORE INTO usage_samples (ts, weekly_pct, weekly_reset_ts, five_hour_pct, five_hour_reset_ts, swarm_sessions, haiku_sessions, sonnet_sessions, opus_sessions)"
                + " VALUES ($ts,$w,$wr,$f,$fr,$n,$h,$s,$o)",
            ("ts", Iso(x.Ts)), ("w", x.WeeklyPct), ("wr", x.WeeklyReset is { } wr ? Iso(wr) : null), ("f", x.FiveHourPct),
            ("fr", x.FiveHourReset is { } fr ? Iso(fr) : null), ("n", x.SwarmSessions), ("h", tiers.Haiku), ("s", tiers.Sonnet), ("o", tiers.Opus));

    public static List<UsageSample> Load(BoardDb db, DateTimeOffset since) =>
        [.. db.Rows("SELECT * FROM usage_samples WHERE ts >= $since ORDER BY ts", ("since", Iso(since))).Select(Sample)];

    static UsageSample Sample(JsonObject r) => new(
        Time(r["ts"])!.Value, r["weekly_pct"]!.GetValue<double>(), Time(r["weekly_reset_ts"]), r["five_hour_pct"]?.GetValue<double>(),
        Time(r["five_hour_reset_ts"]), (int)r["swarm_sessions"]!.GetValue<long>(), (int)r["haiku_sessions"]!.GetValue<long>(),
        (int)r["sonnet_sessions"]!.GetValue<long>(), (int)r["opus_sessions"]!.GetValue<long>());

    static int Running(BoardDb db) => Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM identities WHERE state='running'"), Inv);

    /// <summary>The feed as it stands (claude_usage.json), with the identity sessions running now. Once per captured_ts; false if
    /// the feed has no weekly figure.</summary>
    public static bool Record(BoardStore store, string feed)
    {
        if (AgentBoard.Load(feed) is not { } d || Usage.Ts(d["captured_ts"]) is not { } ts || !Usage.Num(d["seven_day"]?["used"], out var weekly)) return false;
        double? five = Usage.Num(d["five_hour"]?["used"], out var f) ? f : null;
        using var db = store.Open();
        int Tier(string m) => Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM identities WHERE state='running' AND COALESCE(running_model, model)=$m", ("m", m)), Inv);
        Insert(db, new(ts, weekly, Usage.Ts(d["seven_day"]?["resets_at"]), five, Usage.Ts(d["five_hour"]?["resets_at"]), Running(db)), (Tier("haiku"), Tier("sonnet"), Tier("opus")));
        return true;
    }

    // ---- enforcement (milestone 7)

    /// <summary>The governor's answer at one moment: its advice (the plan's total_sessions, which <see cref="Budget"/> turns into
    /// the pool's ceiling) and the launch tier. <see cref="Fresh"/> is false when there is no sample, the latest is older than
    /// <see cref="StaleMinutes"/>, or /usage is failing: then no new swarm session starts, and nothing is shed (there is nothing
    /// to shed on).</summary>
    public sealed record Verdict(GovernorSettings Settings, Advice? Advice, bool Fresh, string Why)
    {
        public bool Enforce => Settings.Enforce;

        /// <summary>The tier to launch at: the recommended one when stepping down and it is cheaper than the stored one, else the stored one.</summary>
        public string Model(string stored, bool lead) => Advice is { StepDown: true } a && Array.IndexOf(Models, lead ? a.LeadModel : a.MemberModel) is var r and >= 0
            && Array.IndexOf(Models, stored) is var had && had > r ? Models[r] : stored;

    }

    /// <summary>settings.json's governor_enforce, read now.</summary>
    public static bool Enforcing(string data) => GovernorSettings.From(AgentBoard.Load(Path.Combine(data, "settings.json"))).Enforce;

    public static Verdict Judge(BoardDb db, string data, DateTimeOffset now, bool usageFailing)
    {
        var s = GovernorSettings.From(AgentBoard.Load(Path.Combine(data, "settings.json")));
        if (Trained(db, s, now) is not { } trained) return new(s, null, false, "no usage samples yet: no new swarm sessions (fail closed)");
        var (_, latest, model) = trained;
        var a = Advise(model, latest, Running(db), now, s);
        var age = (now - latest.Ts).TotalMinutes;
        return usageFailing ? new(s, a, false, "/usage is failing: no new swarm sessions (fail closed)")
            : age > StaleMinutes ? new(s, a, false, $"the latest usage sample is {age:0} minutes old: no new swarm sessions (fail closed)")
            : new(s, a, true, a.Reason);
    }

    static readonly Lock TrainGate = new();
    static (string Key, List<UsageSample> Samples, BurnModel Model)? lastTrained;

    /// <summary>The last five weeks of samples and the burn model trained on them. Training (the EWMAs and the per-session regression) took about 100 ms
    /// and ran for every verdict: every ui:status, every launch decision, every tick. The model depends only on the samples and the settings, so
    /// it is kept until a sample is added, the settings change or the hour turns (the window is five weeks back from the hour); what changes
    /// call to call (now, the sessions running) goes into <see cref="Advise"/>, which is cheap. Null when there are no samples.
    /// The samples are never changed after they are loaded: callers only read them.</summary>
    static (List<UsageSample> Samples, UsageSample Latest, BurnModel Model)? Trained(BoardDb db, GovernorSettings s, DateTimeOffset now)
    {
        var hour = now.ToUnixTimeSeconds() / 3600;
        var since = DateTimeOffset.FromUnixTimeSeconds(hour * 3600).AddDays(-35);
        var sig = db.Rows("SELECT COUNT(*) AS n, MAX(id) AS mx, MIN(ts) AS t0, MAX(ts) AS t, SUM(weekly_pct) AS w, SUM(five_hour_pct) AS f, SUM(swarm_sessions) AS s,"
            + " SUM(haiku_sessions) AS h, SUM(sonnet_sessions) AS so, SUM(opus_sessions) AS o FROM usage_samples WHERE ts >= $since", ("since", Iso(since)))[0];
        if (sig["n"]!.GetValue<long>() == 0) return null;
        // The newest sample is what a verdict is about (the 5-hour window, the sessions then): always read fresh, never from the kept list.
        var latest = Sample(db.Rows("SELECT * FROM usage_samples WHERE ts >= $since ORDER BY ts DESC LIMIT 1", ("since", Iso(since)))[0]);
        var key = string.Join('|', sig.Select(kv => kv.Value?.ToString()).Append(hour.ToString()).Append(s.ToString()));
        lock (TrainGate)
        {
            if (lastTrained is { } c && c.Key == key) return (c.Samples, latest, c.Model);
            var samples = Load(db, since);
            var model = Train(samples, s);
            lastTrained = (key, samples, model);
            return (samples, latest, model);
        }
    }

    /// <summary>ui:governor_enforce: settings.json's governor_enforce, keeping every other key.</summary>
    public static void SetEnforce(string data, bool on)
    {
        var file = Path.Combine(data, "settings.json");
        var d = AgentBoard.Load(file) ?? [];
        d["governor_enforce"] = on;
        Atomic.Write(file, d.ToJsonString(AgentDesk.Contracts.Wire.Indented));
    }

    /// <summary>ui:governor, and ui:status's governor section: the last five weeks of samples (the EWMAs have forgotten older ones).</summary>
    /// <summary><paramref name="enforcement"/> (Identities: would_queue, would_shed, held) is merged in when given.</summary>
    public static JsonObject Report(BoardDb db, string data, DateTimeOffset now, bool usageFailing = false, JsonObject? enforcement = null)
    {
        var s = GovernorSettings.From(AgentBoard.Load(Path.Combine(data, "settings.json")));
        var v = Judge(db, data, now, usageFailing);
        var o = Build(db, s, now);
        (o["enforcing"], o["advisory"], o["fresh"]) = (s.Enforce, !s.Enforce, v.Fresh);
        if (!v.Fresh) o["fail_closed"] = v.Why;
        if (v.Advice is not null) o["summary"] += (v.Fresh ? "" : " · fail closed: " + v.Why) + (s.Enforce ? " · enforcing" : " · advisory");
        foreach (var (k, x) in enforcement ?? []) o[k] = x?.DeepClone();
        return o;
    }

    static JsonObject Build(BoardDb db, GovernorSettings s, DateTimeOffset now)
    {
        if (Trained(db, s, now) is not { } trained)
            return new() { ["samples"] = 0, ["reason"] = "no usage samples yet", ["summary"] = "governor: no usage samples yet" };
        var (samples, latest, m) = trained;
        var a = Advise(m, latest, Running(db), now, s);
        static double R(double v) => Math.Round(v, 2);
        var path = PlanPath(m, latest, now, s);
        var trend = TrendEnd(samples, now, a);
        var plan = path.Count > 0 ? path[^1] : Math.Min(100, a.Used + a.Baseline + a.Spendable);
        return new()
        {
            ["plan_end_pct"] = R(plan), ["trend_end_pct"] = trend is { } te ? R(te) : null, ["pace"] = R(a.Pace), ["week_elapsed_pct"] = R(a.WeekElapsed * 100),
            ["unused_pct"] = R(Math.Max(0, 100 - Math.Max(plan, trend ?? 0))), ["status"] = Status(plan, trend),
            ["forecast"] = new JsonArray([.. path.Select(x => (JsonNode?)R(x))]),
            ["samples"] = samples.Count, ["baseline_hours"] = m.BaselineHours, ["session_hours"] = m.SessionN,
            ["estimated_hours"] = m.EstN, ["baseline_source"] = m.Source,
            ["sample_age_minutes"] = R((now - latest.Ts).TotalMinutes),
            ["used"] = R(a.Used), ["remaining"] = R(a.Remaining), ["reset_in_hours"] = R(a.ResetInHours), ["baseline"] = R(a.Baseline),
            ["sigma"] = R(a.Sigma), ["reserve"] = R(a.Reserve), ["k"] = s.K, ["spendable"] = R(a.Spendable), ["allowed_rate"] = R(a.AllowedRate),
            ["session_rate"] = R(a.SessionRate), ["projected_end_pct"] = R(a.ProjectedEnd), ["five_hour_pct"] = R(a.FiveHourPct),
            ["caps"] = new JsonObject { ["swarms"] = a.Swarms, ["members_per_swarm"] = a.MembersPerSwarm, ["total_sessions"] = a.TotalSessions,
                ["running"] = a.Running, ["new_sessions"] = a.NewSessions },
            ["models"] = new JsonObject { ["step_down"] = a.StepDown, ["lead"] = a.LeadModel, ["member"] = a.MemberModel },
            ["reason"] = a.Reason,
            ["series"] = Series(samples, now),
            ["summary"] = $"governor: {a.Spendable:0.#}% spendable of {a.Remaining:0.#}% left, resets in {Usage.Span(a.ResetInHours * 3600)}"
                + $" · up to {a.TotalSessions} sessions ({a.Swarms} swarm{(a.Swarms == 1 ? "" : "s")} x {a.MembersPerSwarm} members), {a.NewSessions} new"
                + $" · members {a.MemberModel}{(a.StepDown ? " (step down)" : "")}" + (a.FiveHourPct >= 90 ? " · 5h guard" : ""),
        };
    }

    /// <summary>The plan, hour by hour to the reset: John's expected burn plus the governor's allowed rate, with the allowance recomputed
    /// from what would then be left. It lands near 100% less the margin because the cushion is released as the reset nears.</summary>
    public static List<double> PlanPath(BurnModel m, UsageSample latest, DateTimeOffset now, GovernorSettings s)
    {
        var (used, reset) = (latest.WeeklyPct, latest.WeeklyReset ?? now.AddDays(7));
        while (reset <= now) (used, reset) = (0, reset.AddDays(7));
        var path = new List<double>();
        for (var t = now; t < reset && path.Count < 170; t = t.AddHours(1))
        {
            var a = Advise(m, new UsageSample(t, used, reset, null, null, 0), 0, t, s);
            used = Math.Min(100, used + Math.Min(1, (reset - t).TotalHours) * (m.Rate(HourOfWeek(t), s) + a.AllowedRate)); // the last step may be a part hour
            path.Add(used);
        }
        return path;
    }

    /// <summary>Where the week ends if the last six hours' real pace simply continued; null with under an hour of readings in this week.</summary>
    public static double? TrendEnd(IReadOnlyList<UsageSample> samples, DateTimeOffset now, Advice a)
    {
        var last = samples[^1];
        var first = samples.FirstOrDefault(x => x.Ts >= last.Ts.AddHours(-6) && SameWeek(x, last) && x.WeeklyPct <= last.WeeklyPct);
        if (first is null || (last.Ts - first.Ts).TotalHours < 1) return null;
        return Math.Min(100, a.Used + (last.WeeklyPct - first.WeeklyPct) / (last.Ts - first.Ts).TotalHours * a.ResetInHours);
    }

    /// <summary>One line on how the week is going.</summary>
    public static string Status(double plan, double? trend) =>
        trend is not { } t ? "too few readings to tell yet"
        : t >= 100 ? $"on course to hit the limit before the reset ({t:0}%)"
        : t >= plan - 2 ? $"on pace: ends near {t:0}%"
        : $"behind: at this pace the week ends near {t:0}% and {Math.Max(0, 100 - t):0}% goes unused; the work queue should spin up";

    /// <summary>ui:governor's "series", for the window's budget chart: weekly % over the last 7 days, the last reading of each hour, oldest first.</summary>
    public static JsonArray Series(IEnumerable<UsageSample> samples, DateTimeOffset now) =>
        [.. samples.Where(x => x.Ts > now.AddDays(-7)).GroupBy(x => x.Ts.ToUnixTimeSeconds() / 3600).Select(h => (JsonNode?)Math.Round(h.Last().WeeklyPct, 2))];

    public static Task<string> Ui(BoardStore store, string data, bool usageFailing = false, JsonObject? enforcement = null)
    {
        using var db = store.Open();
        return Task.FromResult(Report(db, data, DateTimeOffset.UtcNow, usageFailing, enforcement).ToJsonString(AgentDesk.Contracts.Wire.Indented));
    }
}

/// <summary>
/// The one session pool: every running identity counts, John's own included, and a Phoenix successor takes its predecessor's
/// place rather than a new one. The ceiling is max_sessions (Options' "Sessions at once"), and, while the governor enforces, never
/// more than the plan's total_sessions (advisory, the core only logs what that lower ceiling would have done). John's identities
/// are never gated by the plan or shed; they only shrink the swarm's part, the ceiling minus them. Goals, the Concierge among them,
/// split that part: each gets its lead first, then members round-robin, the goal waiting longest first, and a goal's max_members
/// bounds its members within the pool. Identities.Drain asks <see cref="CanStart"/> before every start (identity_start,
/// member_spawn, the Concierge's dispatch, lead wakes, the core's resume), and its tick sheds against <see cref="Governed"/>.
/// </summary>
public sealed record Budget
{
    /// <summary>A swarm identity that is running or queued: its goal, whether it leads it, and since when it has waited.</summary>
    public sealed record Want(string Name, string Goal, bool Lead, bool Running, string Since);

    /// <summary>max_sessions, John's hard maximum.</summary>
    public int Hard { get; init; }
    /// <summary>The governor's total_sessions, or null when it fails closed (no swarm session may start).</summary>
    public int? Plan { get; init; }
    public bool Enforce { get; init; }
    public string PlanWhy { get; init; } = "";
    public int Running { get; init; }
    /// <summary>John's own identities running (any identity that is neither a goal's lead nor a member).</summary>
    public int John { get; init; }
    public IReadOnlyList<Want> Wants { get; init; } = [];
    public IReadOnlyDictionary<string, int> MaxMembers { get; init; } = new Dictionary<string, int>();

    /// <summary>The ceiling the governor would set: max_sessions, or the plan when that is lower, and 0 when it fails closed.</summary>
    public int Governed => Math.Min(Hard, Plan ?? 0);

    /// <summary>The ceiling that applies now: <see cref="Governed"/> while the governor enforces, else max_sessions.</summary>
    public int Ceiling => Enforce ? Governed : Hard;

    /// <summary>Why the ceiling is what it is: "max_sessions", "plan" or "fail closed".</summary>
    public string Why => !Enforce || Governed >= Hard ? "max_sessions" : Plan is null ? "fail closed" : "plan";

    public static Budget Read(BoardDb db, int hard, Governor.Verdict v)
    {
        var wants = db.Rows("""
            SELECT i.name, i.state, i.updated_ts, COALESCE(m.goal, g.name) AS goal, m.identity IS NULL AS lead
            FROM identities i LEFT JOIN goal_members m ON m.identity = i.name
              LEFT JOIN goals g ON m.identity IS NULL AND g.lead = i.name COLLATE NOCASE
            WHERE i.state IN ('running', 'queued') AND COALESCE(m.goal, g.name) IS NOT NULL
            """).Select(r => new Want(r["name"]!.ToString(), r["goal"]!.ToString(), (long)r["lead"]! != 0, r["state"]!.ToString() == "running",
                r["updated_ts"]?.ToString() ?? "")).ToList();
        var running = Convert.ToInt32(db.Scalar("SELECT COUNT(*) FROM identities WHERE state='running'"), CultureInfo.InvariantCulture);
        return new()
        {
            Hard = hard, Plan = v.Fresh ? v.Advice?.TotalSessions : null, Enforce = v.Enforce, PlanWhy = v.Why, Running = running,
            John = running - wants.Count(w => w.Running), Wants = wants,
            MaxMembers = db.Rows("SELECT name, max_members FROM goals").ToDictionary(r => r["name"]!.ToString(), r => (int)(long)r["max_members"]!, StringComparer.OrdinalIgnoreCase),
        };
    }

    int Bound(string goal) => MaxMembers.GetValueOrDefault(goal);

    /// <summary>Each goal's share of the swarm's part of <paramref name="ceiling"/>: leads first, then members round-robin, the goal
    /// that has waited longest (its oldest queued identity) first; a goal never gets more than it wants, nor members past max_members.</summary>
    public Dictionary<string, int> Shares(int ceiling)
    {
        var free = Math.Max(0, ceiling - John);
        var goals = Wants.GroupBy(w => w.Goal, StringComparer.OrdinalIgnoreCase)
            .Select(g => (Goal: g.Key, Lead: g.Any(w => w.Lead), Members: Math.Min(g.Count(w => !w.Lead), Bound(g.Key)),
                Since: g.Where(w => !w.Running).Select(w => w.Since).DefaultIfEmpty("ï¿¿").Min(StringComparer.Ordinal)!))
            .OrderBy(g => g.Since, StringComparer.Ordinal).ThenBy(g => g.Goal, StringComparer.OrdinalIgnoreCase).ToList();
        var share = goals.ToDictionary(g => g.Goal, _ => 0, StringComparer.OrdinalIgnoreCase);
        foreach (var g in goals.Where(g => g.Lead))
            if (free > 0) (share[g.Goal], free) = (1, free - 1);
        for (var more = true; more && free > 0;)
        {
            more = false;
            foreach (var g in goals)
                if (free > 0 && share[g.Goal] - (g.Lead ? 1 : 0) < g.Members) (share[g.Goal], free, more) = (share[g.Goal] + 1, free - 1, true);
        }
        return share;
    }

    /// <summary>May one more session start under <paramref name="ceiling"/>? Null means yes, else why not. John's own identities
    /// (<paramref name="goal"/> null) answer only to max_sessions; a goal's lead or member also needs a free slot under the ceiling,
    /// its goal under its fair share, and a member its goal under max_members.</summary>
    public string? CanStart(string? goal, bool lead, int ceiling)
    {
        if (goal is null) return Running < Hard ? null : $"{Running} sessions running: max_sessions is {Hard}";
        if (Running >= ceiling) return $"the pool is full: {Running} running, ceiling {ceiling}";
        var mine = Wants.Where(w => w.Running && string.Equals(w.Goal, goal, StringComparison.OrdinalIgnoreCase)).ToList();
        if (!lead && mine.Count(w => !w.Lead) >= Bound(goal)) return $"goal {goal} has {mine.Count(w => !w.Lead)} of its max_members {Bound(goal)} running";
        var share = Shares(ceiling).GetValueOrDefault(goal);
        return mine.Count < share ? null : $"goal {goal} has its share of the pool running ({mine.Count} of {share}) while other goals wait";
    }

    static string Owner(string goal) => goal.Equals(Concierge.Name, StringComparison.OrdinalIgnoreCase) ? "Concierge" : goal;

    /// <summary>ui:status's "sessions" and ui:governor's "pool": the ceiling and why, running by owner (John, each goal, the
    /// Concierge), queued by goal, and each goal's share. "max" is the ceiling, as ui:status's "sessions" always had it.</summary>
    public JsonObject Report()
    {
        var shares = Shares(Ceiling);
        var owners = new JsonArray(new JsonObject { ["owner"] = "John", ["running"] = John });
        var line = new List<string> { $"John {John}" };
        foreach (var g in Wants.GroupBy(w => w.Goal, StringComparer.OrdinalIgnoreCase).OrderBy(g => g.Key, StringComparer.OrdinalIgnoreCase))
        {
            var (run, queued, share) = (g.Count(w => w.Running), g.Count(w => !w.Running), shares.GetValueOrDefault(g.Key));
            owners.Add((JsonNode)new JsonObject { ["owner"] = Owner(g.Key), ["goal"] = g.Key, ["running"] = run, ["queued"] = queued, ["share"] = share, ["bound"] = 1 + Bound(g.Key) });
            line.Add($"{Owner(g.Key)} {run}/{share}" + (queued > 0 ? $", {queued} queued" : ""));
        }
        var reason = Why switch { "plan" => $"the usage plan: {PlanWhy}", "fail closed" => PlanWhy, _ => "max_sessions (Sessions at once)" }
            + (!Enforce && Governed < Hard ? $"; advisory: the plan would hold it at {Governed}" : "");
        return new()
        {
            ["running"] = Running, ["max"] = Ceiling, ["ceiling"] = Ceiling, ["why"] = Why, ["reason"] = reason, ["max_sessions"] = Hard,
            ["plan"] = Plan, ["governed"] = Governed, ["enforcing"] = Enforce, ["john"] = John, ["swarm"] = Math.Max(0, Ceiling - John),
            ["queued"] = Wants.Count(w => !w.Running), ["owners"] = owners,
            ["summary"] = $"{Running} of {Ceiling} sessions ({Why}) · {string.Join(" · ", line)}",
        };
    }
}
