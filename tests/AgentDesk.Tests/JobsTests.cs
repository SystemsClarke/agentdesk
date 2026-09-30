using System.Diagnostics;
using System.Text.Json;
using AgentDesk.Core;
using AgentDesk.Core.Board;

namespace AgentDesk.Tests;

/// <summary>Recurring jobs (Jobs.cs): when a schedule is next due, and that the core fires, skips and catches up as documented.
/// A PowerShell script stands in for claude, as in ConciergeTests.</summary>
public sealed class JobsTests : IDisposable
{
    static readonly TimeZoneInfo Utc = TimeZoneInfo.Utc;
    static DateTimeOffset T(string s) => DateTimeOffset.Parse(s + "Z", System.Globalization.CultureInfo.InvariantCulture);

    readonly string dir = Directory.CreateTempSubdirectory("jobs-").FullName;
    readonly BoardStore store;
    readonly Sessions sessions = new();
    readonly Identities ids;
    readonly Jobs jobs;
    DateTimeOffset now = T("2026-09-30T06:00:00"); // a Wednesday

    public JobsTests()
    {
        File.WriteAllText(Path.Combine(dir, "settings.json"), """{"max_sessions": 6}""");
        var script = Path.Combine(dir, "standin.ps1");
        File.WriteAllText(script, "\"ready $env:AGENTDESK_AUTHOR\"\nwhile ($null -ne ($l = [Console]::In.ReadLine())) { \"got: $l\" }\n");
        store = new BoardStore(Path.Combine(dir, "agentdesk.db"));
        ids = new Identities(store, sessions, dir, $"powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{script}\"");
        jobs = new Jobs(store, ids) { Zone = Utc, Now = () => now };
    }

    public void Dispose()
    {
        using (var db = store.Open()) db.Exec("DELETE FROM identities");
        foreach (var n in Json(sessions.List()).GetProperty("sessions").EnumerateArray())
            try { sessions.Stop(n.GetProperty("name").GetString()!).Wait(); } catch (AggregateException) { }
    }

    static JsonElement Json(Task<string> t) => JsonDocument.Parse(t.Result).RootElement;

    JsonElement Job(string name) => Json(jobs.List()).GetProperty("jobs").EnumerateArray().Single(j => j.GetProperty("name").GetString() == name);

    string? Scalar(string sql, params (string, object?)[] args) { using var db = store.Open(); return db.Scalar(sql, args)?.ToString(); }

    static async Task Until(Func<bool> ok, string what)
    {
        for (var sw = Stopwatch.StartNew(); !ok(); await Task.Delay(50))
            Assert.True(sw.Elapsed < TimeSpan.FromSeconds(20), $"never: {what}");
    }

    [Fact]
    public void Daily_at_is_the_next_occurrence_strictly_after_now()
    {
        Assert.Equal(T("2026-09-30T08:00:00"), Jobs.Next(T("2026-09-30T06:00:00"), "08:00", "daily", null, Utc));
        Assert.Equal(T("2026-10-01T08:00:00"), Jobs.Next(T("2026-09-30T08:00:00"), "08:00", "daily", null, Utc)); // exactly at: tomorrow's
        Assert.Equal(T("2026-10-01T08:00:00"), Jobs.Next(T("2026-09-30T09:30:00"), "8:00", "daily", null, Utc));
    }

    [Fact]
    public void Weekdays_skip_the_weekend_and_named_days_are_honoured()
    {
        Assert.Equal(T("2026-10-05T08:00:00"), Jobs.Next(T("2026-10-02T09:00:00"), "08:00", "weekdays", null, Utc)); // Fri after 08:00 -> Mon
    }

    [Fact]
    public void Named_days_pick_the_nearest()
    {
        Assert.Equal(T("2026-10-02T07:15:00"), Jobs.Next(T("2026-09-30T09:00:00"), "07:15", "mon,fri", null, Utc));
        Assert.Equal(T("2026-10-03T12:00:00"), Jobs.Next(T("2026-09-30T09:00:00"), "12:00", "weekends", null, Utc));
    }

    [Fact]
    public void An_interval_is_that_long_after_now()
    {
        Assert.Equal(T("2026-09-30T08:00:00"), Jobs.Next(T("2026-09-30T06:00:00"), null, "daily", 120, Utc));
    }

    [Fact]
    public void Local_time_is_used_for_at()
    {
        var eastern = TimeZoneInfo.CreateCustomTimeZone("t", TimeSpan.FromHours(-4), "t", "t");
        Assert.Equal(T("2026-09-30T12:00:00"), Jobs.Next(T("2026-09-30T06:00:00"), "08:00", "daily", null, eastern));
    }

    [Theory]
    [InlineData("25:00", "daily"), InlineData("8am", "daily"), InlineData("08:00", "someday"), InlineData("08:00", "")]
    public void A_bad_spec_is_refused(string at, string days)
    {
        if (days == "") { Assert.Equal(T("2026-09-30T08:00:00"), Jobs.Next(T("2026-09-30T06:00:00"), at, days, null, Utc)); return; } // empty means daily
        Assert.Throws<ArgumentException>(() => Jobs.Next(T("2026-09-30T06:00:00"), at, days, null, Utc));
    }

    [Fact]
    public async Task Create_needs_exactly_one_of_at_and_every_and_a_real_folder()
    {
        await Assert.ThrowsAsync<ArgumentException>(() => jobs.Create("a", dir, "p", null, null, null, null, null));
        await Assert.ThrowsAsync<ArgumentException>(() => jobs.Create("a", dir, "p", "08:00", null, 60, null, null));
        await Assert.ThrowsAsync<ArgumentException>(() => jobs.Create("a", Path.Combine(dir, "nope"), "p", "08:00", null, null, null, null));
        await Assert.ThrowsAsync<ArgumentException>(() => jobs.Create("a b", dir, "p", "08:00", null, null, null, null));
    }

    [Fact]
    public async Task A_due_job_starts_a_fresh_session_once_and_moves_to_tomorrow()
    {
        await jobs.Create("morning", dir, "do the morning chore", "08:00", "daily", null, "haiku", null);
        Assert.Equal("2026-09-30T08:00:00+00:00", Job("morning").GetProperty("next_run_ts").GetString());

        jobs.Tick(); // 06:00: not yet
        Assert.Null(Scalar("SELECT state FROM identities WHERE name='morning'"));

        now = T("2026-09-30T08:00:30");
        jobs.Tick();
        await Until(() => Scalar("SELECT state FROM identities WHERE name='morning'") == "running", "the job's identity runs");
        var j = Job("morning");
        Assert.Equal("2026-10-01T08:00:00+00:00", j.GetProperty("next_run_ts").GetString());
        Assert.Equal(1, j.GetProperty("runs").GetInt32());
        Assert.StartsWith("started", j.GetProperty("last_status").GetString());
        Assert.Equal("haiku", Scalar("SELECT model FROM identities WHERE name='morning'"));

        jobs.Tick(); // nothing else is due
        Assert.Equal(1, Job("morning").GetProperty("runs").GetInt32());
    }

    [Fact]
    public async Task A_run_due_while_the_last_is_still_going_is_skipped_not_stacked()
    {
        await jobs.Create("patrol", dir, "patrol", null, null, 60, null, null);
        now = T("2026-09-30T07:00:01");
        jobs.Tick();
        await Until(() => Scalar("SELECT state FROM identities WHERE name='patrol'") == "running", "first run");
        var first = Scalar("SELECT claude_session_id FROM identities WHERE name='patrol'");

        now = T("2026-09-30T08:00:02");
        jobs.Tick();
        Assert.StartsWith("skipped", Job("patrol").GetProperty("last_status").GetString());
        Assert.Equal(1, Job("patrol").GetProperty("runs").GetInt32());
        Assert.Equal(first, Scalar("SELECT claude_session_id FROM identities WHERE name='patrol'"));
    }

    [Fact]
    public async Task The_next_run_is_a_new_conversation_not_a_resume()
    {
        await jobs.Create("daily", dir, "chore", "08:00", null, null, null, null);
        now = T("2026-09-30T08:00:01");
        jobs.Tick();
        await Until(() => Scalar("SELECT state FROM identities WHERE name='daily'") == "running", "first run");
        var first = Scalar("SELECT claude_session_id FROM identities WHERE name='daily'");
        await ids.Stop("daily");
        await Until(() => Scalar("SELECT state FROM identities WHERE name='daily'") == "stopped", "stopped");

        now = T("2026-10-01T08:00:01");
        jobs.Tick();
        await Until(() => Scalar("SELECT state FROM identities WHERE name='daily'") == "running", "second run");
        Assert.NotEqual(first, Scalar("SELECT claude_session_id FROM identities WHERE name='daily'"));
        Assert.Equal(2, Job("daily").GetProperty("runs").GetInt32());
    }

    [Fact]
    public async Task A_run_missed_by_more_than_the_catch_up_window_is_not_run()
    {
        await jobs.Create("late", dir, "chore", "08:00", null, null, null, 60);
        now = T("2026-09-30T11:00:00"); // the core was down until three hours after it was due
        jobs.Tick();
        Assert.Null(Scalar("SELECT state FROM identities WHERE name='late'"));
        Assert.StartsWith("missed", Job("late").GetProperty("last_status").GetString());
        Assert.Equal("2026-10-01T08:00:00+00:00", Job("late").GetProperty("next_run_ts").GetString());
    }

    [Fact]
    public async Task A_run_missed_inside_the_window_runs_once()
    {
        await jobs.Create("catchup", dir, "chore", "08:00", null, null, null, null);
        now = T("2026-09-30T09:00:00");
        jobs.Tick();
        await Until(() => Scalar("SELECT state FROM identities WHERE name='catchup'") == "running", "caught up");
        Assert.Equal(1, Job("catchup").GetProperty("runs").GetInt32());
    }

    [Fact]
    public async Task A_disabled_job_does_not_run_and_enabling_reschedules_from_now()
    {
        await jobs.Create("off", dir, "chore", "08:00", null, null, null, null);
        await jobs.Enable("off", false);
        now = T("2026-09-30T08:30:00");
        jobs.Tick();
        Assert.Null(Scalar("SELECT state FROM identities WHERE name='off'"));
        await jobs.Enable("off", true);
        Assert.Equal("2026-10-01T08:00:00+00:00", Job("off").GetProperty("next_run_ts").GetString()); // not the 08:00 it slept through
    }

    [Fact]
    public async Task Run_now_starts_it_without_moving_the_schedule()
    {
        await jobs.Create("manual", dir, "chore", "08:00", null, null, null, null);
        await jobs.RunNow("manual");
        await Until(() => Scalar("SELECT state FROM identities WHERE name='manual'") == "running", "manual run");
        Assert.Equal("2026-09-30T08:00:00+00:00", Job("manual").GetProperty("next_run_ts").GetString());
    }

    [Fact]
    public async Task Delete_removes_the_job_and_an_unknown_one_is_an_error()
    {
        await jobs.Create("gone", dir, "chore", "08:00", null, null, null, null);
        await jobs.Delete("gone");
        Assert.Empty(Json(jobs.List()).GetProperty("jobs").EnumerateArray());
        await Assert.ThrowsAsync<ArgumentException>(() => jobs.Delete("gone"));
    }
}
