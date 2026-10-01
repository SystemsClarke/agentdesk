using System.Globalization;

namespace AgentDesk.Core;

/// <summary>What <see cref="Learner"/> found: how well each model predicted the newest hours it had not trained on, whether the learned
/// one earned its place, and what one more session of each tier costs per hour in weekly %.</summary>
public sealed record Learned(bool Ok, int Hours, double RmseNet, double RmseLinear, double RmseMean, double Haiku, double Sonnet, double Opus, string Note)
{
    /// <summary>The cost of one more session, weighted by the tiers running (sonnet when none are known).</summary>
    public double Blend(UsageSample latest)
    {
        var (h, s, o) = Learner.Tiers(latest);
        return h + s + o == 0 ? Sonnet : (h * Haiku + s * Sonnet + o * Opus) / (h + s + o);
    }
}

/// <summary>
/// A tiny network that learns, from the governor's own readings, how fast the weekly % burns per hour given how many haiku, sonnet and
/// opus sessions were running and the hour of the day: a linear part plus four tanh units, trained by backpropagation (plain SGD, a
/// fixed seed so it is repeatable). It retrains whenever a new reading arrives, which is the loop: predict, see the real spend, adjust.
/// It is trusted only if, on the newest quarter of the hours it never trained on, it beats the plain average by a clear margin
/// (<see cref="Margin"/>); until then <see cref="Learned.Ok"/> is false and the governor keeps its old per-session cost.
/// </summary>
public static class Learner
{
    const int Hidden = 4, Inputs = 5, MinHours = 48, MaxHours = 500, Budget = 24_000;
    /// <summary>The learned model must have this fraction of the plain average's holdout error, or less.</summary>
    public const double Margin = 0.9;
    const double Scale = 8; // sessions are fed in as count / 8
    static (DateTimeOffset Ts, int N, Learned? Result) cache;

    public static (int Haiku, int Sonnet, int Opus) Tiers(UsageSample x) =>
        x.Haiku + x.Sonnet + x.Opus > 0 ? (x.Haiku, x.Sonnet, x.Opus) : (0, x.SwarmSessions, 0); // readings from before the tiers were stored: call them sonnet

    static bool SameWeek(UsageSample a, UsageSample b) => a.WeeklyReset is not { } ra || b.WeeklyReset is not { } rb || Math.Abs((ra - rb).TotalHours) < 1;

    /// <summary>One row per hour with at least half an hour of coverage: the burn (weekly % per hour), the average sessions of each tier, and the hour of day.</summary>
    static List<(double Y, double[] X)> Rows(IReadOnlyList<UsageSample> samples)
    {
        var hours = new SortedDictionary<long, (double Delta, double Dt, double H, double S, double O)>();
        for (var i = 1; i < samples.Count; i++)
        {
            var (a, b) = (samples[i - 1], samples[i]);
            var dt = (b.Ts - a.Ts).TotalHours;
            if (dt <= 0 || dt > 1 || b.WeeklyPct < a.WeeklyPct || !SameWeek(a, b)) continue;
            var (ta, tb) = (Tiers(a), Tiers(b));
            var key = a.Ts.ToUnixTimeSeconds() / 3600;
            hours.TryGetValue(key, out var h);
            hours[key] = (h.Delta + b.WeeklyPct - a.WeeklyPct, h.Dt + dt, h.H + (ta.Haiku + tb.Haiku) / 2.0 * dt, h.S + (ta.Sonnet + tb.Sonnet) / 2.0 * dt, h.O + (ta.Opus + tb.Opus) / 2.0 * dt);
        }
        return [.. hours.Where(kv => kv.Value.Dt >= 0.5).Select(kv =>
        {
            var (key, h) = (kv.Key, kv.Value);
            var hod = DateTimeOffset.FromUnixTimeSeconds(key * 3600).UtcDateTime.Hour;
            return (h.Delta / h.Dt, Features(h.H / h.Dt, h.S / h.Dt, h.O / h.Dt, hod));
        })];
    }

    static double[] Features(double haiku, double sonnet, double opus, int hourOfDay) =>
        [haiku / Scale, sonnet / Scale, opus / Scale, Math.Sin(2 * Math.PI * hourOfDay / 24), Math.Cos(2 * Math.PI * hourOfDay / 24)];

    sealed class Net(int hidden, int seed)
    {
        public readonly double[] W = new double[Inputs], V = new double[hidden], C = new double[hidden];
        public readonly double[][] U = [.. Enumerable.Range(0, hidden).Select(_ => new double[Inputs])];
        public double B;

        public void Init()
        {
            var rng = new Random(seed);
            foreach (var u in U) for (var i = 0; i < Inputs; i++) u[i] = (rng.NextDouble() - 0.5) * 0.6;
            for (var k = 0; k < hidden; k++) V[k] = (rng.NextDouble() - 0.5) * 0.6;
        }

        public double Predict(double[] x) => Forward(x, new double[hidden]);

        double Forward(double[] x, double[] z)
        {
            var y = B;
            for (var i = 0; i < Inputs; i++) y += W[i] * x[i];
            for (var k = 0; k < hidden; k++)
            {
                var a = C[k];
                for (var i = 0; i < Inputs; i++) a += U[k][i] * x[i];
                y += V[k] * (z[k] = Math.Tanh(a));
            }
            return y;
        }

        /// <summary>Stochastic gradient descent on squared error with a little weight decay, the learning rate easing off over the epochs.</summary>
        public void Train(IReadOnlyList<(double Y, double[] X)> rows, double mean)
        {
            var epochs = Math.Clamp(Budget / Math.Max(1, rows.Count), 20, 400); // about the same work however many hours there are
            Init();
            B = mean;
            var z = new double[hidden];
            for (var epoch = 0; epoch < epochs; epoch++)
            {
                var lr = 0.05 * (1 - epoch / (double)epochs) + 0.002;
                foreach (var (y, x) in rows)
                {
                    var e = Math.Clamp(Forward(x, z) - y, -5, 5); // a wild reading must not throw the weights
                    B -= lr * e;
                    for (var i = 0; i < Inputs; i++) W[i] -= lr * (e * x[i] + 1e-4 * W[i]);
                    for (var k = 0; k < hidden; k++)
                    {
                        var dz = e * V[k] * (1 - z[k] * z[k]);
                        V[k] -= lr * (e * z[k] + 1e-4 * V[k]);
                        C[k] -= lr * dz;
                        for (var i = 0; i < Inputs; i++) U[k][i] -= lr * (dz * x[i] + 1e-4 * U[k][i]);
                    }
                }
            }
        }
    }

    static double Rmse(Func<double[], double> f, IEnumerable<(double Y, double[] X)> rows) =>
        Math.Sqrt(rows.Average(r => Math.Pow(f(r.X) - r.Y, 2)));

    /// <summary>Trains on the older three quarters of the hours, scores on the newest quarter, then refits on everything. Null with too few hours.</summary>
    public static Learned? Fit(IReadOnlyList<UsageSample> samples)
    {
        if (samples.Count == 0) return null;
        var key = (samples[^1].Ts, samples.Count);
        lock (typeof(Learner)) if (cache.Result is not null && (cache.Ts, cache.N) == key) return cache.Result;
        var learned = Compute(samples);
        lock (typeof(Learner)) cache = (key.Ts, key.Count, learned);
        return learned;
    }

    public static Learned? Compute(IReadOnlyList<UsageSample> samples)
    {
        var rows = Rows(samples);
        if (rows.Count > MaxHours) rows = rows[^MaxHours..]; // the recent past is what the next hours look like
        if (rows.Count < MinHours) return rows.Count == 0 ? null : new(false, rows.Count, 0, 0, 0, 0, 0, 0, $"{rows.Count} hours so far; it needs {MinHours} before it tries");
        var split = rows.Count * 3 / 4;
        var (train, test) = (rows[..split], rows[split..]);
        var mean = train.Average(r => r.Y);
        var linear = new Net(0, 1);
        linear.Train(train, mean);
        var net = new Net(Hidden, 1);
        net.Train(train, mean);
        var (rMean, rLinear, rNet) = (Rmse(_ => mean, test), Rmse(linear.Predict, test), Rmse(net.Predict, test));
        var useNet = rNet < rLinear;
        var best = Math.Min(rNet, rLinear);
        var final = new Net(useNet ? Hidden : 0, 1);
        final.Train(rows, rows.Average(r => r.Y));
        var recent = rows[^Math.Min(24, rows.Count)..];
        double[] Ref() => [.. Enumerable.Range(0, Inputs).Select(i => recent.Average(r => r.X[i]))];
        double Cost(int tier)
        {
            var (a, b) = (Ref(), Ref());
            b[tier] += 1 / Scale;
            return Math.Clamp(final.Predict(b) - final.Predict(a), 0, 20);
        }
        var (h, s, o) = (Cost(0), Cost(1), Cost(2));
        var ok = best < Margin * rMean && h + s + o > 0 && rows.Count >= MinHours;
        var note = ok ? $"learned from {rows.Count} hours: a session-hour costs haiku {h:0.##}%, sonnet {s:0.##}%, opus {o:0.##}% (error {best:0.##}%/h against {rMean:0.##} for the plain average)"
            : $"not trusted yet: on the newest hours it is off by {best:0.##}%/h against {rMean:0.##} for the plain average, so the governor keeps its own estimate";
        return new(ok, rows.Count, rNet, rLinear, rMean, h, s, o, note);
    }
}
