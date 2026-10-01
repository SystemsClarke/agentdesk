using AgentDesk.Core;

namespace AgentDesk.Tests;

/// <summary>The small network that learns what a session of each tier costs from the governor's own readings, and the gate that keeps it
/// from being trusted before it has earned it.</summary>
public sealed class LearnerTests
{
    static readonly DateTimeOffset Start = new(2026, 9, 21, 0, 0, 0, TimeSpan.Zero);

    /// <summary>Readings every ten minutes through <paramref name="hours"/> hours of one week; the sessions of each tier change hourly and the
    /// burn is <paramref name="burn"/> of them plus noise.</summary>
    static List<UsageSample> Week(int hours, Func<int, int, int, double> burn, int seed = 3)
    {
        var rng = new Random(seed);
        var (list, pct, reset) = (new List<UsageSample>(), 0.0, Start.AddDays(7));
        for (var h = 0; h < hours; h++)
        {
            var (hi, so, op) = (rng.Next(0, 7), rng.Next(0, 7), rng.Next(0, 4));
            for (var m = 0; m < 6; m++)
            {
                list.Add(new(Start.AddHours(h).AddMinutes(m * 10), pct, reset, 0, null, hi + so + op, hi, so, op));
                pct += Math.Max(0, burn(hi, so, op) + (rng.NextDouble() - 0.5) * 0.02) / 6;
            }
        }
        return list;
    }

    [Fact]
    public void It_learns_what_each_tier_costs_when_the_readings_carry_the_signal()
    {
        var learned = Learner.Compute(Week(150, (h, s, o) => 0.03 + 0.01 * h + 0.045 * s + 0.12 * o))!;
        Assert.True(learned.Ok, learned.Note);
        Assert.InRange(learned.Opus, 0.12 * 0.75, 0.12 * 1.25);
        Assert.InRange(learned.Sonnet, 0.045 * 0.75, 0.045 * 1.25);
        Assert.True(learned.Opus > learned.Sonnet && learned.Sonnet > learned.Haiku);
        Assert.True(learned.RmseNet < 0.5 * learned.RmseMean, "it predicts the newest hours far better than the plain average");
    }

    [Fact]
    public void It_is_not_trusted_when_the_burn_has_nothing_to_do_with_the_sessions()
    {
        var rng = new Random(11);
        var learned = Learner.Compute(Week(150, (_, _, _) => 0.2 + (rng.NextDouble() - 0.5) * 0.3))!;
        Assert.False(learned.Ok, learned.Note);
        Assert.Contains("not trusted", learned.Note);
    }

    [Fact]
    public void Too_few_hours_are_not_enough_to_try()
    {
        var learned = Learner.Compute(Week(20, (h, s, o) => 0.03 + 0.12 * o))!;
        Assert.False(learned.Ok);
        Assert.Contains("needs", learned.Note);
        Assert.Null(Learner.Compute([]));
    }

    [Fact]
    public void A_trusted_model_sets_the_per_session_cost_the_governor_uses_and_an_untrusted_one_does_not()
    {
        var samples = Week(150, (h, s, o) => 0.03 + 0.01 * h + 0.045 * s + 0.12 * o);
        var settings = new GovernorSettings();
        var model = Governor.Train(samples, settings);
        Assert.True(model.Learned!.Ok);
        var all = new UsageSample(Start.AddDays(3), 40, Start.AddDays(7), 0, null, 4, 0, 0, 4); // four opus
        var none = all with { SwarmSessions = 4, Opus = 0, Sonnet = 4 };
        var opus = Governor.Advise(model, all, 4, all.Ts, settings).SessionRate;
        Assert.True(opus > Governor.Advise(model, none, 4, all.Ts, settings).SessionRate, "opus sessions cost more than sonnet ones");
        model.Learned = model.Learned with { Ok = false };
        Assert.Equal(model.PerSession(settings), Governor.Advise(model, all, 4, all.Ts, settings).SessionRate);
    }
}
