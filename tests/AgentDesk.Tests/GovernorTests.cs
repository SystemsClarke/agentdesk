using System.Text.Json;
using AgentDesk.Core;
using AgentDesk.Core.Board;
using Xunit.Abstractions;

namespace AgentDesk.Tests;

/// <summary>The usage governor (docs/GOAL.md milestone 3): the formula, the model it trains, what the core stores and shows, and a
/// backtest that follows its caps through synthetic weeks.</summary>
public sealed class GovernorTests(ITestOutputHelper output)
{
    static readonly GovernorSettings S = new();
    static readonly DateTimeOffset Mon = new(2026, 9, 21, 0, 0, 0, TimeSpan.Zero); // a Monday, 00:00 UTC

    [Fact]
    public void Hour_of_week_starts_monday_utc()
    {
        Assert.Equal(0, Governor.HourOfWeek(Mon));
        Assert.Equal(24 * 2 + 10, Governor.HourOfWeek(Mon.AddDays(2).AddHours(10).AddMinutes(59)));
        Assert.Equal(167, Governor.HourOfWeek(Mon.AddHours(-1)));
    }

    [Fact]
    public void Reserve_shrinks_to_nothing_as_the_reset_nears()
    {
        var reserves = new[] { 168.0, 72, 24, 4, 1, 0.1, 0 }.Select(h => 60 - Governor.Spendable(60, 5, 1, h, 2)).ToList();
        for (var i = 1; i < reserves.Count; i++) Assert.True(reserves[i] < reserves[i - 1], $"reserve grew at step {i}: {string.Join(", ", reserves)}");
        Assert.Equal(5, reserves[^1], 6); // only the forecast is left at the reset
        Assert.Equal(2 * 1 * Math.Sqrt(24), 60 - 5 - Governor.Spendable(60, 5, 1, 24, 2), 6);
    }

    [Fact]
    public void Spendable_is_never_negative()
    {
        var rng = new Random(7);
        for (var i = 0; i < 10_000; i++)
            Assert.True(Governor.Spendable(rng.NextDouble() * 120 - 10, rng.NextDouble() * 150, rng.NextDouble() * 5, rng.NextDouble() * 200, rng.NextDouble() * 4) >= 0);
        Assert.Equal(0, Governor.Spendable(10, 50, 1, 24, 2));
        var model = new BurnModel();
        var advice = Governor.Advise(model, new(Mon, 99.5, Mon.AddDays(6), 0, null, 0), 3, Mon, S); // less left than the margin
        Assert.Equal(0, advice.Spendable);
        Assert.Equal((0, 0, 0), (advice.TotalSessions, advice.NewSessions, advice.Swarms));
    }

    [Fact]
    public void The_five_hour_guard_allows_no_new_sessions()
    {
        var model = new BurnModel();
        UsageSample At(double five, DateTimeOffset? fiveReset) => new(Mon, 10, Mon.AddHours(2), five, fiveReset, 2);
        var open = Governor.Advise(model, At(89, Mon.AddHours(1)), 2, Mon, S);
        Assert.True(open.NewSessions > 0, open.Reason);
        Assert.Equal(S.MaxSessions, open.TotalSessions); // 89% spendable over 2 h funds more than the ceiling
        var guarded = Governor.Advise(model, At(90, Mon.AddHours(1)), 2, Mon, S);
        Assert.Equal((0, 2), (guarded.NewSessions, guarded.TotalSessions));
        Assert.Contains("5-hour window", guarded.Reason);
        Assert.True(guarded.StepDown);
        Assert.Equal(0, Governor.Advise(model, At(95, Mon.AddHours(1)), 0, Mon, S).TotalSessions);
        Assert.True(Governor.Advise(model, At(95, Mon.AddMinutes(-1)), 2, Mon, S).NewSessions > 0); // that window already reset
    }

    [Fact]
    public void Tight_budgets_step_members_down_a_tier()
    {
        var model = new BurnModel();
        var tight = Governor.Advise(model, new(Mon, 40, Mon.AddDays(6), 5, null, 0), 0, Mon, S);
        Assert.True(tight.StepDown);
        Assert.Equal(("haiku", "sonnet"), (tight.MemberModel, tight.LeadModel));
        var loose = Governor.Advise(model, new(Mon, 10, Mon.AddHours(12), 5, null, 0), 0, Mon, S);
        Assert.False(loose.StepDown);
        Assert.Equal(("sonnet", "opus"), (loose.MemberModel, loose.LeadModel));
        Assert.Equal(loose.TotalSessions, loose.Swarms * (1 + loose.MembersPerSwarm) + loose.TotalSessions % (1 + loose.MembersPerSwarm));
    }

    [Fact]
    public void Train_learns_johns_hours_and_what_a_session_costs()
    {
        // Two weeks: John burns 1%/h on Mondays 10:00-11:00 and nothing else; two sessions run Tuesdays 09:00-12:00 at 1.5%/h each.
        var samples = new List<UsageSample>();
        var pct = 0.0;
        for (var t = Mon; t < Mon.AddDays(14); t = t.AddMinutes(5))
        {
            var how = Governor.HourOfWeek(t);
            var sessions = how is >= 24 + 9 and < 24 + 12 ? 2 : 0;
            samples.Add(new(t, pct, Mon.AddDays(t < Mon.AddDays(7) ? 7 : 14), 0, null, sessions));
            pct = t.AddMinutes(5) == Mon.AddDays(7) ? 0 : pct + ((how == 10 ? 1 : 0) + sessions * 1.5) / 12;
        }
        var m = Governor.Train(samples, S);
        Assert.Equal(1, m.Rate(10, S), 6);
        Assert.Equal(0, m.Rate(11, S), 6);
        Assert.Equal(1.5, m.PerSession(S), 1); // the hour a session ends counts it half
        Assert.Equal(2, m.N[10]);
        Assert.Equal(1.0, Governor.Baseline(m, Mon.AddHours(9.5), Mon.AddHours(11.5), S), 6);
    }

    [Fact]
    public async Task Samples_are_recorded_once_and_shown_in_status()
    {
        var data = Directory.CreateTempSubdirectory("governor-").FullName;
        var store = new BoardStore(Path.Combine(data, "agentdesk.db"));
        store.Init();
        var board = new AgentBoard(store, null!, "wait {0}");
        Assert.Equal("governor: no usage samples yet", Status(await board.Heartbeats(data)));
        var feed = Path.Combine(data, "claude_usage.json");
        Assert.False(Governor.Record(store, feed));
        var reset = DateTimeOffset.UtcNow.AddDays(3);
        File.WriteAllText(feed, $$$"""{"captured_ts": "{{{DateTimeOffset.UtcNow:yyyy-MM-ddTHH:mm:ss}}}+00:00", "five_hour": {"used": 12, "resets_at": null}, "seven_day": {"used": 40, "resets_at": "{{{reset.UtcDateTime:yyyy-MM-ddTHH:mm:ss}}}+00:00"}}""");
        using (var db = store.Open())
        {
            db.Exec("INSERT INTO identities (name, folder, state, created_ts, updated_ts) VALUES ('a', 'C:\\', 'running', 'x', 'x')");
            Assert.Equal("sonnet", db.Scalar("SELECT model FROM identities WHERE name='a'"));
        }
        Assert.True(Governor.Record(store, feed));
        Assert.True(Governor.Record(store, feed)); // the same captured_ts: still one sample
        using (var db = store.Open()) Assert.Equal("1|40.0|12.0|1", db.Scalar("SELECT COUNT(*) || '|' || MAX(weekly_pct) || '|' || MAX(five_hour_pct) || '|' || MAX(swarm_sessions) FROM usage_samples"));
        var g = JsonDocument.Parse(await Governor.Ui(store, data)).RootElement;
        Assert.Equal(1, g.GetProperty("samples").GetInt32());
        Assert.Equal(60, g.GetProperty("remaining").GetDouble());
        Assert.InRange(g.GetProperty("reset_in_hours").GetDouble(), 71.9, 72);
        Assert.Equal(1, g.GetProperty("caps").GetProperty("running").GetInt32());
        foreach (var key in new[] { "baseline", "spendable", "projected_end_pct", "reason" }) Assert.True(g.TryGetProperty(key, out _), key);
        Assert.StartsWith("governor: ", Status(await board.Heartbeats(data)));
    }

    static string Status(string heartbeats) => JsonDocument.Parse(heartbeats).RootElement.GetProperty("governor").GetProperty("summary").GetString()!;

    // ---- the backtest

    /// <summary>John is awake 12:00-03:00 UTC (8am-11pm Eastern).</summary>
    static double Steady(DateTimeOffset t) => t.Hour >= 12 || t.Hour < 3 ? 0.35 : 0.03;
    static double Bursty(DateTimeOffset t) => Steady(t) + (t.DayOfWeek == DayOfWeek.Wednesday && t.Hour is >= 14 and < 20 ? 3 : 0);
    static double Light(DateTimeOffset t) => 0.3 * Steady(t);

    /// <summary>Weeks back to back, resetting Fridays 08:00 UTC, following the governor's caps every 5 minutes: each session burns
    /// 1.2%/h of the week, John burns his trace with noise, and /usage reports whole percentages, rounded down. A 5-hour window
    /// holds a quarter of a week. Returns each week's final % and the highest weekly and 5-hour % seen.</summary>
    static (List<double> Ends, double MaxWeekly, double MaxFive) Simulate(IReadOnlyList<Func<DateTimeOffset, double>> weeks, int seed, List<UsageSample>? history = null)
    {
        const double SessionBurn = 1.2, FiveRatio = 4;
        var rng = new Random(seed);
        var samples = history ?? [];
        var start = new DateTimeOffset(2026, 9, 4, 8, 0, 0, TimeSpan.Zero);
        var (ends, maxWeekly, maxFive) = (new List<double>(), 0.0, 0.0);
        var (pct, five, sessions) = (0.0, 0.0, 0);
        DateTimeOffset? fiveReset = null;
        var model = Governor.Train(samples, S);
        for (var w = 0; w < weeks.Count; w++)
        {
            var reset = start.AddDays(7 * (w + 1));
            for (var t = start.AddDays(7 * w); t < reset; t = t.AddMinutes(5))
            {
                if (t.Minute == 0) model = Governor.Train(samples, S);
                if (fiveReset <= t) (five, fiveReset) = (0, null);
                var seen = new UsageSample(t, Math.Floor(pct), reset, Math.Floor(five), fiveReset, sessions);
                sessions = Governor.Advise(model, seen, sessions, t, S).TotalSessions;
                samples.Add(seen with { SwarmSessions = sessions });
                var burn = (weeks[w](t) * (0.5 + rng.NextDouble()) + sessions * SessionBurn * (0.5 + rng.NextDouble())) / 12;
                if (burn > 0 && fiveReset is null) fiveReset = t.AddHours(5);
                (pct, five) = (pct + burn, five + burn * FiveRatio);
                (maxWeekly, maxFive) = (Math.Max(maxWeekly, pct), Math.Max(maxFive, five));
            }
            ends.Add(pct);
            pct = 0;
        }
        return (ends, maxWeekly, maxFive);
    }

    public static TheoryData<string> Scenarios => ["steady", "bursty", "light"];

    [Theory]
    [MemberData(nameof(Scenarios))]
    public void Backtest_following_the_caps_ends_the_week_between_85_and_100(string scenario)
    {
        Func<DateTimeOffset, double> week = scenario switch { "steady" => Steady, "bursty" => Bursty, _ => Light };
        for (var seed = 1; seed <= 10; seed++)
        {
            // Two ordinary weeks first, from no history at all (the defaults), then the week under test.
            var (ends, maxWeekly, maxFive) = Simulate([Steady, Steady, week], seed);
            output.WriteLine($"{scenario} seed {seed}: weeks end at {string.Join(", ", ends.Select(e => e.ToString("0.0")))}%; max weekly {maxWeekly:0.0}%, max 5-hour {maxFive:0.0}%");
            Assert.True(maxWeekly <= 100, $"went over: {maxWeekly}");
            Assert.InRange(ends[^1], 85, 100);
            Assert.InRange(ends[1], 85, 100);
        }
    }

    /// <summary>Opt-in: AGENTDESK_BACKTEST_DB names a COPY of a board (never the live one) whose usage_samples replay as John's own
    /// hourly burn, with the simulated swarm following the caps on top. See docs/governor.md.</summary>
    [Fact]
    public void Backtest_on_recorded_samples()
    {
        if (Environment.GetEnvironmentVariable("AGENTDESK_BACKTEST_DB") is not { Length: > 0 } copy) return;
        List<UsageSample> recorded;
        using (var db = new BoardStore(copy).Open()) recorded = Governor.Load(db, DateTimeOffset.MinValue);
        var hourly = new Dictionary<long, double>(); // John's recorded %/h by hour since the epoch, from swarm-free pairs
        for (var i = 1; i < recorded.Count; i++)
        {
            var (a, b) = (recorded[i - 1], recorded[i]);
            var dt = (b.Ts - a.Ts).TotalHours;
            if (dt is > 0 and <= 1 && b.WeeklyPct >= a.WeeklyPct && a.SwarmSessions + b.SwarmSessions == 0)
                hourly[a.Ts.ToUnixTimeSeconds() / 3600] = hourly.GetValueOrDefault(a.Ts.ToUnixTimeSeconds() / 3600) + (b.WeeklyPct - a.WeeklyPct);
        }
        var first = recorded.Count > 0 ? recorded[0].Ts : DateTimeOffset.UtcNow;
        var weeks = Math.Max(1, (int)Math.Ceiling((recorded.Count > 0 ? (recorded[^1].Ts - first).TotalDays : 0) / 7));
        // Shift the recorded hours onto the simulator's weeks, hour of week for hour of week.
        var offset = (long)Math.Round((new DateTimeOffset(2026, 9, 4, 8, 0, 0, TimeSpan.Zero) - first).TotalHours / 168) * 168;
        double John(DateTimeOffset t) => hourly.GetValueOrDefault(t.ToUnixTimeSeconds() / 3600 - offset);
        var (ends, maxWeekly, maxFive) = Simulate([.. Enumerable.Repeat<Func<DateTimeOffset, double>>(John, weeks)], 1);
        output.WriteLine($"{recorded.Count} samples, {hourly.Count} swarm-free hours: weeks end at {string.Join(", ", ends.Select(e => e.ToString("0.0")))}%; max weekly {maxWeekly:0.0}%, max 5-hour {maxFive:0.0}%");
        Assert.True(maxWeekly <= 100, $"went over: {maxWeekly}");
    }
}
