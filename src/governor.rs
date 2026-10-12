//! The usage governor: records `/usage` samples, forecasts John's own burn until the weekly reset, and turns what is left into
//! recommended caps with spendable = remaining - E[baseline] - k*sigma*sqrt(T). The reserve shrinks as the reset nears, which is the
//! ramp-up: cautious early, flat out at the end, finishing near 100%. A port of the .NET `Governor.cs`; the numbers match.
//!
//! `Budget` is the one session pool: John's own identities answer only to `max_sessions`, a goal's lead or member also to the pool's
//! ceiling (the plan, while `governor_enforce` is on), its goal's fair share and its `max_members`.

use crate::db::Db;
use crate::paths;
use anyhow::Result;
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;

type Time = DateTime<Utc>;

const BUCKET_ALPHA: f64 = 0.5;
const GLOBAL_ALPHA: f64 = 0.02;
const SESSION_ALPHA: f64 = 0.2;
/// A sample older than this, or a failing /usage, allows no new swarm sessions (fail closed).
pub const STALE_MINUTES: f64 = 30.0;
/// The most the reserve for John's noise holds back, in weekly %: a quarter of the plan.
pub const MAX_RESERVE: f64 = 25.0;
/// The weight, in observed hours, of the default rate a young global rate or estimate is shrunk toward.
const PRIOR_HOURS: f64 = 24.0;
/// The least one identity session is taken to cost, in weekly-% per session-hour.
pub const MIN_SESSION_RATE: f64 = 0.05;
pub const MODELS: [&str; 3] = ["haiku", "sonnet", "opus"];

fn long_alpha() -> f64 {
    1.0 - 0.5f64.powf(1.0 / 672.0) // a 4-week half-life counted in observed hours
}

/// One `/usage` reading: weekly and 5-hour percentages, their resets, and how many identity sessions ran then.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub ts: Option<Time>,
    pub weekly_pct: f64,
    pub weekly_reset: Option<Time>,
    pub five_pct: Option<f64>,
    pub five_reset: Option<Time>,
    pub sessions: i64,
}

impl Sample {
    pub fn new(ts: Time, weekly: f64, weekly_reset: Option<Time>, five: Option<f64>, five_reset: Option<Time>, sessions: i64) -> Sample {
        Sample { ts: Some(ts), weekly_pct: weekly, weekly_reset, five_pct: five, five_reset, sessions }
    }
    fn t(&self) -> Time {
        self.ts.unwrap_or_else(Utc::now)
    }
}

/// settings.json's governor_* keys. The defaults are conservative: they assume John is busy and sessions are expensive until the
/// samples say otherwise.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub k: f64,
    pub margin: f64,
    pub default_rate: f64,
    pub default_sigma: f64,
    pub default_session_rate: f64,
    pub max_sessions: i64,
    pub max_members: i64,
    pub max_swarms: i64,
    pub enforce: bool,
    pub ramp: f64,
    pub floor: f64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            k: 2.0,
            margin: 2.0,
            default_rate: 0.3,
            default_sigma: 0.5,
            default_session_rate: 3.0,
            max_sessions: 12,
            max_members: 4,
            max_swarms: 10,
            enforce: false,
            ramp: 1.0,
            floor: 0.6,
        }
    }
}

impl Settings {
    pub fn from(s: &Value) -> Settings {
        let g = Settings::default();
        let d = |key: &str, dflt: f64| s[key].as_f64().or_else(|| s[key].as_str().and_then(|t| t.trim().parse().ok())).filter(|v| *v >= 0.0).unwrap_or(dflt);
        let i = |key: &str, dflt: i64| s[key].as_i64().or_else(|| s[key].as_str().and_then(|t| t.trim().parse().ok())).filter(|v| *v >= 0).unwrap_or(dflt);
        Settings {
            k: d("governor_k", g.k),
            margin: d("governor_margin", g.margin),
            default_rate: d("governor_default_rate", g.default_rate),
            default_sigma: d("governor_default_sigma", g.default_sigma),
            default_session_rate: d("governor_default_session_rate", g.default_session_rate),
            max_sessions: i("governor_max_sessions", g.max_sessions),
            max_members: i("governor_max_members", g.max_members),
            max_swarms: i("governor_max_swarms", g.max_swarms),
            enforce: s["governor_enforce"].as_bool().unwrap_or_else(|| s["governor_enforce"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("true"))),
            ramp: d("governor_ramp", g.ramp),
            floor: d("governor_floor", g.floor).min(1.0),
        }
    }

    pub fn read() -> Settings {
        Settings::from(&settings_json())
    }
}

fn settings_json() -> Value {
    std::fs::read_to_string(paths::data_dir().join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// John's own burn in weekly-% per hour: an EWMA per hour of week (UTC), a global EWMA and variance behind it, and the measured burn of
/// one identity session on top of John. When swarms run constantly there are few session-free hours, so a long-memory EWMA over every
/// hour (the `est_*` fields) estimates John as total rate minus sessions x the per-session burn.
pub struct BurnModel {
    pub mean: [f64; 168],
    pub var: [f64; 168],
    pub n: [i32; 168],
    pub global_mean: f64,
    pub global_var: f64,
    pub session_rate: f64,
    pub est_mean: f64,
    pub est_var: f64,
    pub slope: f64,
    pub global_n: i32,
    pub session_n: i32,
    pub baseline_hours: i32,
    pub est_n: i32,
    /// The session count varied enough over the long memory to regress the hourly rate on it (`slope`).
    pub slope_ok: bool,
}

impl Default for BurnModel {
    fn default() -> BurnModel {
        BurnModel {
            mean: [0.0; 168],
            var: [0.0; 168],
            n: [0; 168],
            global_mean: 0.0,
            global_var: 0.0,
            session_rate: 0.0,
            est_mean: 0.0,
            est_var: 0.0,
            slope: 0.0,
            global_n: 0,
            session_n: 0,
            baseline_hours: 0,
            est_n: 0,
            slope_ok: false,
        }
    }
}

fn shrunk(mean: f64, n: i32, s: &Settings) -> f64 {
    (f64::from(n) * mean + PRIOR_HOURS * s.default_rate) / (f64::from(n) + PRIOR_HOURS)
}

impl BurnModel {
    /// A bucket needs two observed hours before it outweighs the global rate, which needs six before it outweighs the starvation
    /// estimate, which needs six before it outweighs the default. A young global rate or estimate is shrunk toward the default.
    pub fn rate(&self, hour_of_week: usize, s: &Settings) -> f64 {
        if self.n[hour_of_week] >= 2 {
            self.mean[hour_of_week]
        } else if self.global_n >= 6 {
            shrunk(self.global_mean, self.global_n, s)
        } else if self.est_n >= 6 {
            shrunk(self.est_mean, self.est_n, s)
        } else {
            s.default_rate
        }
    }

    /// Per-hour sigma, from the global variance: it includes the daily pattern, so it errs high (a bigger reserve).
    pub fn sigma(&self, s: &Settings) -> f64 {
        if self.global_n >= 6 {
            self.global_var.sqrt().max(0.1)
        } else if self.est_n >= 6 {
            self.est_var.sqrt().max(0.1)
        } else {
            s.default_sigma
        }
    }

    /// Where John's rate comes from when a bucket is thin: measured session-free hours, the starvation estimate, or the default.
    pub fn source(&self) -> &'static str {
        if self.global_n >= 6 {
            "measured"
        } else if self.est_n >= 6 {
            "estimated"
        } else {
            "default"
        }
    }

    /// Measured against John's session-free hours; with too few of those (starved), the regression slope when the session count varied.
    pub fn per_session(&self, s: &Settings) -> f64 {
        if self.global_n < 6 && self.slope_ok {
            self.slope.clamp(MIN_SESSION_RATE, 20.0)
        } else if self.session_n >= 3 {
            MIN_SESSION_RATE.max(self.session_rate)
        } else {
            s.default_session_rate
        }
    }
}

/// What the governor recommends now.
#[derive(Clone, Debug)]
pub struct Advice {
    pub used: f64,
    pub remaining: f64,
    pub reset_in_hours: f64,
    pub baseline: f64,
    pub sigma: f64,
    pub reserve: f64,
    pub spendable: f64,
    pub allowed_rate: f64,
    pub session_rate: f64,
    pub running: i64,
    pub total_sessions: i64,
    pub new_sessions: i64,
    pub swarms: i64,
    pub members_per_swarm: i64,
    pub five_hour_pct: f64,
    pub step_down: bool,
    pub lead_model: &'static str,
    pub member_model: &'static str,
    pub projected_end: f64,
    pub reason: String,
    pub pace: f64,
    pub week_elapsed: f64,
}

/// The formula, floored at 0.
#[cfg_attr(not(test), allow(dead_code))] // the formula as documented; `advise` inlines it with the margin
pub fn spendable(remaining: f64, baseline: f64, sigma: f64, hours: f64, k: f64) -> f64 {
    (remaining - baseline - k * sigma * hours.max(0.0).sqrt()).max(0.0)
}

/// How much of an even spread of the spendable % the governor allows per hour at `t` (0 the week's start, 1 its reset): `floor` at the
/// start, climbing to 1 at the reset. A ramp of 0 is the flat spread; a bigger one holds back longer.
pub fn pace(t: f64, ramp: f64, floor: f64) -> f64 {
    let t = t.clamp(0.0, 0.999);
    let m = if ramp <= 0.0 { 1.0 } else { (ramp + 1.0) * t.powf(ramp) * (1.0 - t) / (1.0 - t.powf(ramp + 1.0)) };
    floor + (1.0 - floor) * m.min(1.0)
}

/// Monday 00:00 UTC is 0.
pub fn hour_of_week(t: Time) -> usize {
    ((t.timestamp().div_euclid(3600) + 72).rem_euclid(168)) as usize
}

fn ewma(mean: &mut f64, var: &mut f64, n: &mut i32, x: f64, alpha: f64) {
    *n += 1;
    let a = alpha.max(1.0 / f64::from(*n)); // a running mean until 1/alpha observations, so an early variance is not near zero
    let d = x - *mean;
    *mean += a * d;
    *var = (1.0 - a) * (*var + a * d * d);
}

fn same_week(a: &Sample, b: &Sample) -> bool {
    match (a.weekly_reset, b.weekly_reset) {
        (Some(ra), Some(rb)) => (ra - rb).num_seconds().abs() < 3600,
        _ => true,
    }
}

fn hours_between(a: Time, b: Time) -> f64 {
    (b - a).num_milliseconds() as f64 / 3_600_000.0
}

/// Samples become hours (weekly-% per hour, needing half an hour of coverage). Hours with no identity session running train John's
/// baseline; hours with some measure a session as (rate - baseline) / sessions.
pub fn train(samples: &[Sample], s: &Settings) -> BurnModel {
    let mut m = BurnModel::default();
    // (delta, dt, session hours, swarm)
    let mut hours: BTreeMap<i64, (f64, f64, f64, bool)> = BTreeMap::new();
    for w in samples.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let dt = hours_between(a.t(), b.t());
        if dt <= 0.0 || dt > 1.0 || b.weekly_pct < a.weekly_pct || !same_week(a, b) {
            continue; // a gap or a reset says nothing about a rate
        }
        let key = a.t().timestamp().div_euclid(3600);
        let h = hours.entry(key).or_insert((0.0, 0.0, 0.0, false));
        h.0 += b.weekly_pct - a.weekly_pct;
        h.1 += dt;
        h.2 += (a.sessions + b.sessions) as f64 / 2.0 * dt;
        h.3 = h.3 || a.sessions + b.sessions > 0;
    }
    let mut busy: Vec<(usize, f64, f64)> = Vec::new();
    for (key, h) in &hours {
        if h.1 < 0.5 {
            continue;
        }
        let (rate, how) = (h.0 / h.1, hour_of_week(Utc.timestamp_opt(key * 3600, 0).unwrap()));
        if h.3 {
            busy.push((how, rate, h.2 / h.1));
            continue;
        }
        m.baseline_hours += 1;
        ewma(&mut m.mean[how], &mut m.var[how], &mut m.n[how], rate, BUCKET_ALPHA);
        ewma(&mut m.global_mean, &mut m.global_var, &mut m.global_n, rate, GLOBAL_ALPHA);
    }
    for (how, rate, sessions) in busy {
        if sessions < 0.5 {
            continue;
        }
        let x = ((rate - m.rate(how, s)) / sessions).clamp(0.0, 20.0);
        let mut unused = 0.0;
        ewma(&mut m.session_rate, &mut unused, &mut m.session_n, x, SESSION_ALPHA);
    }
    // Baseline starvation. First the long-memory regression of rate on sessions (exponential weights), which separates a session from
    // John whenever the session count varies; then every hour, oldest first, as John's rate = total rate - sessions x per session.
    let (mut sw, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let keep = 1.0 - long_alpha();
    for h in hours.values().filter(|h| h.1 >= 0.5) {
        let (x, y) = (h.2 / h.1, h.0 / h.1);
        (sw, sx, sy, sxx, sxy) = (keep * sw + 1.0, keep * sx + x, keep * sy + y, keep * sxx + x * x, keep * sxy + x * y);
    }
    let var_x = if sw > 0.0 { sxx / sw - sx / sw * (sx / sw) } else { 0.0 };
    m.slope_ok = hours.len() >= 6 && var_x >= 0.25;
    m.slope = if var_x > 0.0 { (sxy / sw - sx / sw * (sy / sw)) / var_x } else { 0.0 };
    for h in hours.values().filter(|h| h.1 >= 0.5) {
        let x = (h.0 / h.1 - h.2 / h.1 * m.per_session(s)).max(0.0);
        ewma(&mut m.est_mean, &mut m.est_var, &mut m.est_n, x, long_alpha());
    }
    m
}

/// E[John's burn] from now until the reset, hour bucket by hour bucket.
pub fn baseline(m: &BurnModel, now: Time, reset: Time, s: &Settings) -> f64 {
    let (mut sum, mut t) = (0.0, now);
    while t < reset {
        let next = Utc.timestamp_opt((t.timestamp().div_euclid(3600) + 1) * 3600, 0).unwrap();
        let end = next.min(reset);
        sum += m.rate(hour_of_week(t), s) * hours_between(t, end);
        t = end;
    }
    sum
}

/// The week's reset after `now`, and what has been used of the week that is current at `now`.
fn current_week(latest: &Sample, now: Time) -> (f64, Time) {
    let (mut used, mut reset) = (latest.weekly_pct, latest.weekly_reset.unwrap_or(now + Duration::days(7))); // no reset time: the longest week
    while reset <= now {
        used = 0.0; // the week turned over since the sample
        reset += Duration::days(7);
    }
    (used, reset)
}

pub fn advise(m: &BurnModel, latest: &Sample, running: i64, now: Time, s: &Settings) -> Advice {
    let (used, reset) = current_week(latest, now);
    let hours = hours_between(now, reset).max(1.0 / 60.0);
    let remaining = (100.0 - used).max(0.0);
    // John cannot burn more than what is left of the plan, and the reserve for his noise is never more than MAX_RESERVE.
    let base = baseline(m, now, reset, s).min(remaining);
    let sigma = m.sigma(s);
    let reserve = (s.k * sigma * hours.sqrt()).min(MAX_RESERVE.min(remaining));
    let spend = (remaining - s.margin - base - reserve).max(0.0);
    let elapsed = (hours_between(reset - Duration::days(7), now) / 168.0).clamp(0.0, 1.0);
    let pace = pace(elapsed, s.ramp, s.floor);
    let rate = spend / hours * pace;
    let per = m.per_session(s);
    let five = match (latest.five_reset, latest.five_pct) {
        (Some(f), _) if f <= now => 0.0,
        (_, p) => p.unwrap_or(0.0),
    };
    let affordable = rate / per;
    let mut total = (s.max_sessions as f64).min((affordable + 1e-9).floor()) as i64;
    let mut new_sessions = (total - running).max(0);
    #[allow(clippy::needless_late_init)] // the first branch also sets the totals
    let reason;
    if five >= 90.0 {
        (total, new_sessions) = (0, 0);
        reason = format!("the 5-hour window is at {five:.0}%: no swarm sessions (shed them) until it resets");
    } else if spend <= 0.0 {
        reason = format!("nothing spendable: John's forecast {}% plus a {}% reserve covers the {}% left", f1(base), f1(reserve), f1(remaining));
    } else if total == 0 {
        reason = format!(
            "{}% spendable over {} ({:.0}% of an even spread this far into the week) funds {affordable:.2} sessions (about {:.0} session-hours a day) at {}%/session-hour; the reserve shrinks as the reset nears",
            f1(spend),
            span(hours * 3600.0),
            pace * 100.0,
            affordable * 24.0,
            f2(per)
        );
    } else {
        reason = format!(
            "{}% spendable over {}, {:.0}% of an even spread this far into the week: {rate:.2}%/h funds {total} sessions at {}%/session-hour{}",
            f1(spend),
            span(hours * 3600.0),
            pace * 100.0,
            f2(per),
            if total == s.max_sessions && affordable >= (s.max_sessions + 1) as f64 { " (held at governor_max_sessions)" } else { "" }
        );
    }
    let members = if total <= 1 { 0 } else { s.max_members.min(total - 1) };
    let swarms = if total == 0 { 0 } else { s.max_swarms.min(total / (1 + members)) };
    let step_down = affordable < 1.0 || five >= 75.0; // tight: members drop to haiku and leads to sonnet
    Advice {
        used,
        remaining,
        reset_in_hours: hours,
        baseline: base,
        sigma,
        reserve,
        spendable: spend,
        allowed_rate: rate,
        session_rate: per,
        running,
        total_sessions: total,
        new_sessions,
        swarms,
        members_per_swarm: members,
        five_hour_pct: five,
        step_down,
        lead_model: if step_down { "sonnet" } else { "opus" },
        member_model: if step_down { "haiku" } else { "sonnet" },
        projected_end: (used + base + spend.min(total as f64 * per * hours)).min(100.0),
        reason,
        pace,
        week_elapsed: elapsed,
    }
}

/// C#'s "0.#": up to one decimal, no trailing zero.
fn f1(v: f64) -> String {
    trimmed(v, 1)
}

/// C#'s "0.##".
fn f2(v: f64) -> String {
    trimmed(v, 2)
}

fn trimmed(v: f64, places: usize) -> String {
    let s = format!("{v:.places$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" {
        "0".into()
    } else {
        s
    }
}

/// "2d 3h", "4h 05m", "12m".
pub fn span(seconds: f64) -> String {
    let t = seconds.max(0.0) as i64;
    let (d, h, m) = (t / 86400, t % 86400 / 3600, t % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

/// The plan, hour by hour to the reset: John's expected burn plus the governor's allowed rate, with the allowance recomputed from what
/// would then be left.
pub fn plan_path(m: &BurnModel, latest: &Sample, now: Time, s: &Settings) -> Vec<f64> {
    let (mut used, reset) = current_week(latest, now);
    let (mut path, mut t) = (Vec::new(), now);
    while t < reset && path.len() < 170 {
        let a = advise(m, &Sample::new(t, used, Some(reset), None, None, 0), 0, t, s);
        used = (used + hours_between(t, reset).min(1.0) * (m.rate(hour_of_week(t), s) + a.allowed_rate)).min(100.0); // the last step may be a part hour
        path.push(used);
        t += Duration::hours(1);
    }
    path
}

/// Where the week ends if the last six hours' real pace simply continued; None with under an hour of readings in this week.
pub fn trend_end(samples: &[Sample], a: &Advice) -> Option<f64> {
    let last = samples.last()?;
    let first = samples.iter().find(|x| x.t() >= last.t() - Duration::hours(6) && same_week(x, last) && x.weekly_pct <= last.weekly_pct)?;
    let dt = hours_between(first.t(), last.t());
    if dt < 1.0 {
        return None;
    }
    Some((a.used + (last.weekly_pct - first.weekly_pct) / dt * a.reset_in_hours).min(100.0))
}

/// One line on how the week is going.
pub fn status(plan: f64, trend: Option<f64>) -> String {
    match trend {
        None => "too few readings to tell yet".into(),
        Some(t) if t >= 100.0 => format!("on course to hit the limit before the reset ({t:.0}%)"),
        Some(t) if t >= plan - 2.0 => format!("on pace: ends near {t:.0}%"),
        Some(t) => format!("behind: at this pace the week ends near {t:.0}% and {:.0}% goes unused; the work queue should spin up", (100.0 - t).max(0.0)),
    }
}

/// The weekly % over the last 7 days, the last reading of each hour, oldest first.
pub fn series(samples: &[Sample], now: Time) -> Vec<f64> {
    let mut out: Vec<(i64, f64)> = Vec::new();
    for x in samples.iter().filter(|x| x.t() > now - Duration::days(7)) {
        let hour = x.t().timestamp().div_euclid(3600);
        match out.last_mut() {
            Some(last) if last.0 == hour => last.1 = x.weekly_pct,
            _ => out.push((hour, x.weekly_pct)),
        }
    }
    out.into_iter().map(|(_, v)| (v * 100.0).round() / 100.0).collect()
}

// ---- the board

fn iso(t: Time) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, false)
}

fn parse_time(v: &Value) -> Option<Time> {
    if let Some(n) = v.as_f64() {
        return Utc.timestamp_millis_opt((if n > 1e11 { n } else { n * 1000.0 }) as i64).single();
    }
    let s = v.as_str().filter(|s| !s.is_empty())?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok().map(|n| Utc.from_utc_datetime(&n)))
}

fn sample_of(r: &Value) -> Option<Sample> {
    Some(Sample {
        ts: parse_time(&r["ts"]),
        weekly_pct: r["weekly_pct"].as_f64()?,
        weekly_reset: parse_time(&r["weekly_reset_ts"]),
        five_pct: r["five_hour_pct"].as_f64(),
        five_reset: parse_time(&r["five_hour_reset_ts"]),
        sessions: r["swarm_sessions"].as_i64().unwrap_or(0),
    })
}

/// Stores a sample (once per timestamp), with the sessions running at each tier.
pub fn insert(db: &Db, x: &Sample, tiers: (i64, i64, i64)) -> Result<()> {
    db.exec(
        "INSERT OR IGNORE INTO usage_samples (ts, weekly_pct, weekly_reset_ts, five_hour_pct, five_hour_reset_ts, swarm_sessions, haiku_sessions, sonnet_sessions, opus_sessions) \
         VALUES (:ts,:w,:wr,:f,:fr,:n,:h,:s,:o)",
        &[
            (":ts", &iso(x.t())),
            (":w", &x.weekly_pct),
            (":wr", &x.weekly_reset.map(iso)),
            (":f", &x.five_pct),
            (":fr", &x.five_reset.map(iso)),
            (":n", &x.sessions),
            (":h", &tiers.0),
            (":s", &tiers.1),
            (":o", &tiers.2),
        ],
    )?;
    Ok(())
}

pub fn load(db: &Db, since: Time) -> Result<Vec<Sample>> {
    Ok(db.rows("SELECT * FROM usage_samples WHERE ts >= :since ORDER BY ts", &[(":since", &iso(since))])?.iter().filter_map(sample_of).collect())
}

fn running(db: &Db) -> i64 {
    db.scalar_i64("SELECT COUNT(*) FROM identities WHERE state='running'", &[]).ok().flatten().unwrap_or(0)
}

/// The feed as it stands (claude_usage.json), with the identity sessions running now. Once per timestamp; false if the feed has no
/// weekly figure. Reads both the status line's own shape (`used_percentage`, `ts`) and the .NET build's (`used`, `captured_ts`).
pub fn record(db: &Db) -> bool {
    let Ok(text) = std::fs::read_to_string(paths::usage_file()) else { return false };
    let Ok(d) = serde_json::from_str::<Value>(&text) else { return false };
    let pct = |w: &Value| w["used"].as_f64().or_else(|| w["used_percentage"].as_f64()).or_else(|| w["used"].as_str().and_then(|s| s.trim_end_matches('%').parse().ok()));
    let Some(weekly) = pct(&d["seven_day"]) else { return false };
    let ts = parse_time(&d["captured_ts"]).or_else(|| parse_time(&d["ts"])).unwrap_or_else(Utc::now);
    let tier = |m: &str| db.scalar_i64("SELECT COUNT(*) FROM identities WHERE state='running' AND COALESCE(running_model, model)=:m", &[(":m", &m)]).ok().flatten().unwrap_or(0);
    let x = Sample::new(ts, weekly, parse_time(&d["seven_day"]["resets_at"]), pct(&d["five_hour"]), parse_time(&d["five_hour"]["resets_at"]), running(db));
    insert(db, &x, (tier("haiku"), tier("sonnet"), tier("opus"))).is_ok()
}

// ---- enforcement

/// The governor's answer at one moment: its advice (the plan's total_sessions, which `Budget` turns into the pool's ceiling) and whether it
/// is `fresh`: false when there is no sample, the latest is older than `STALE_MINUTES`, or /usage is failing. Then no new swarm session
/// starts, and nothing is shed (there is nothing to shed on).
pub struct Verdict {
    pub settings: Settings,
    pub advice: Option<Advice>,
    pub fresh: bool,
    pub why: String,
}

impl Verdict {
    pub fn enforce(&self) -> bool {
        self.settings.enforce
    }

    /// The tier to launch at: the recommended one when stepping down and it is cheaper than the stored one, else the stored one.
    pub fn model(&self, stored: &str, lead: bool) -> String {
        if let Some(a) = self.advice.as_ref().filter(|a| a.step_down) {
            let want = if lead { a.lead_model } else { a.member_model };
            if let (Some(r), Some(had)) = (MODELS.iter().position(|m| *m == want), MODELS.iter().position(|m| *m == stored)) {
                if had > r {
                    return MODELS[r].to_string();
                }
            }
        }
        stored.to_string()
    }
}

pub fn judge(db: &Db, now: Time, usage_failing: bool) -> Verdict {
    let s = Settings::read();
    let Some((samples, latest)) = trained_inputs(db, now) else {
        return Verdict { settings: s, advice: None, fresh: false, why: "no usage samples yet: no new swarm sessions (fail closed)".into() };
    };
    let model = train(&samples, &s);
    let a = advise(&model, &latest, running(db), now, &s);
    let age = hours_between(latest.t(), now) * 60.0;
    if usage_failing {
        Verdict { settings: s, advice: Some(a), fresh: false, why: "/usage is failing: no new swarm sessions (fail closed)".into() }
    } else if age > STALE_MINUTES {
        Verdict { settings: s, advice: Some(a), fresh: false, why: format!("the latest usage sample is {age:.0} minutes old: no new swarm sessions (fail closed)") }
    } else {
        let why = a.reason.clone();
        Verdict { settings: s, advice: Some(a), fresh: true, why }
    }
}

/// The last five weeks of samples (the EWMAs have forgotten older ones), and the newest one.
fn trained_inputs(db: &Db, now: Time) -> Option<(Vec<Sample>, Sample)> {
    let since = Utc.timestamp_opt(now.timestamp().div_euclid(3600) * 3600, 0).unwrap() - Duration::days(35);
    let samples = load(db, since).ok()?;
    let latest = samples.last()?.clone();
    Some((samples, latest))
}

/// settings.json's governor_enforce, keeping every other key.
pub fn set_enforce(on: bool) -> Result<()> {
    let file = paths::data_dir().join("settings.json");
    let mut d = settings_json();
    if !d.is_object() {
        d = json!({});
    }
    d["governor_enforce"] = json!(on);
    std::fs::create_dir_all(paths::data_dir())?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&d)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The governor's document (the Options/SysOp budget screen and `ui:governor`): `enforcement` (would_queue, would_shed, held) is merged in when given.
pub fn report(db: &Db, now: Time, usage_failing: bool, enforcement: Option<&Value>) -> Value {
    let v = judge(db, now, usage_failing);
    let s = &v.settings;
    let mut o = build(db, s, now);
    o["enforcing"] = json!(s.enforce);
    o["advisory"] = json!(!s.enforce);
    o["fresh"] = json!(v.fresh);
    if !v.fresh {
        o["fail_closed"] = json!(v.why);
    }
    if v.advice.is_some() {
        let tail = format!("{}{}", if v.fresh { String::new() } else { format!(" · fail closed: {}", v.why) }, if s.enforce { " · enforcing" } else { " · advisory" });
        o["summary"] = json!(format!("{}{tail}", o["summary"].as_str().unwrap_or("")));
    }
    o["pool"] = Budget::read(db, crate::identities::max_sessions() as i64, &v).report();
    if let Some(Value::Object(e)) = enforcement {
        for (k, x) in e {
            o[k] = x.clone();
        }
    }
    o
}

fn build(db: &Db, s: &Settings, now: Time) -> Value {
    let Some((samples, latest)) = trained_inputs(db, now) else {
        return json!({"samples": 0, "reason": "no usage samples yet", "summary": "governor: no usage samples yet"});
    };
    let m = train(&samples, s);
    let a = advise(&m, &latest, running(db), now, s);
    let path = plan_path(&m, &latest, now, s);
    let trend = trend_end(&samples, &a);
    let plan = path.last().copied().unwrap_or_else(|| (a.used + a.baseline + a.spendable).min(100.0));
    json!({
        "plan_end_pct": r2(plan), "trend_end_pct": trend.map(r2), "pace": r2(a.pace), "week_elapsed_pct": r2(a.week_elapsed * 100.0),
        "unused_pct": r2((100.0 - plan.max(trend.unwrap_or(0.0))).max(0.0)), "status": status(plan, trend),
        "forecast": path.iter().map(|x| r2(*x)).collect::<Vec<_>>(),
        "samples": samples.len(), "baseline_hours": m.baseline_hours, "session_hours": m.session_n,
        "estimated_hours": m.est_n, "baseline_source": m.source(),
        "sample_age_minutes": r2(hours_between(latest.t(), now) * 60.0),
        "used": r2(a.used), "remaining": r2(a.remaining), "reset_in_hours": r2(a.reset_in_hours), "baseline": r2(a.baseline),
        "sigma": r2(a.sigma), "reserve": r2(a.reserve), "k": s.k, "spendable": r2(a.spendable), "allowed_rate": r2(a.allowed_rate),
        "session_rate": r2(a.session_rate), "projected_end_pct": r2(a.projected_end), "five_hour_pct": r2(a.five_hour_pct),
        "caps": {"swarms": a.swarms, "members_per_swarm": a.members_per_swarm, "total_sessions": a.total_sessions, "running": a.running, "new_sessions": a.new_sessions},
        "models": {"step_down": a.step_down, "lead": a.lead_model, "member": a.member_model},
        "reason": a.reason,
        "series": series(&samples, now),
        "summary": format!(
            "governor: {}% spendable of {}% left, resets in {} · up to {} sessions ({} swarm{} x {} members), {} new · members {}{}{}",
            f1(a.spendable), f1(a.remaining), span(a.reset_in_hours * 3600.0), a.total_sessions, a.swarms, if a.swarms == 1 { "" } else { "s" },
            a.members_per_swarm, a.new_sessions, a.member_model, if a.step_down { " (step down)" } else { "" },
            if a.five_hour_pct >= 90.0 { " · 5h guard" } else { "" }
        ),
    })
}

// ---- the session pool

/// A swarm identity that is running or queued: its goal, whether it leads it, and since when it has waited.
#[derive(Clone, Debug)]
pub struct Want {
    #[allow(dead_code)]
    pub name: String,
    pub goal: String,
    pub lead: bool,
    pub running: bool,
    pub since: String,
}

/// The one session pool: every running identity counts, John's own included, and a Phoenix successor takes its predecessor's place
/// rather than a new one. The ceiling is max_sessions, and, while the governor enforces, never more than the plan's total_sessions.
/// John's identities are never gated by the plan or shed; they only shrink the swarm's part, the ceiling minus them. Goals, the
/// Concierge among them, split that part: each gets its lead first, then members round-robin, the goal waiting longest first.
pub struct Budget {
    /// max_sessions, John's hard maximum.
    pub hard: i64,
    /// The governor's total_sessions, or None when it fails closed (no swarm session may start).
    pub plan: Option<i64>,
    pub enforce: bool,
    pub plan_why: String,
    pub running: i64,
    /// John's own identities running (any identity that is neither a goal's lead nor a member).
    pub john: i64,
    pub wants: Vec<Want>,
    pub max_members: BTreeMap<String, i64>,
}

impl Budget {
    pub fn read(db: &Db, hard: i64, v: &Verdict) -> Budget {
        let wants: Vec<Want> = db
            .rows(
                "SELECT i.name, i.state, i.updated_ts, COALESCE(m.goal, g.name) AS goal, m.identity IS NULL AS lead \
                 FROM identities i LEFT JOIN goal_members m ON m.identity = i.name \
                 LEFT JOIN goals g ON m.identity IS NULL AND g.lead = i.name COLLATE NOCASE \
                 WHERE i.state IN ('running', 'queued') AND COALESCE(m.goal, g.name) IS NOT NULL",
                &[],
            )
            .unwrap_or_default()
            .iter()
            .map(|r| Want {
                name: r["name"].as_str().unwrap_or("").into(),
                goal: r["goal"].as_str().unwrap_or("").into(),
                lead: r["lead"].as_i64() == Some(1),
                running: r["state"].as_str() == Some("running"),
                since: r["updated_ts"].as_str().unwrap_or("").into(),
            })
            .collect();
        let running = running(db);
        let max_members = db
            .rows("SELECT name, max_members FROM goals", &[])
            .unwrap_or_default()
            .iter()
            .map(|r| (r["name"].as_str().unwrap_or("").to_lowercase(), r["max_members"].as_i64().unwrap_or(0)))
            .collect();
        Budget {
            hard,
            plan: if v.fresh { v.advice.as_ref().map(|a| a.total_sessions) } else { None },
            enforce: v.enforce(),
            plan_why: v.why.clone(),
            running,
            john: running - wants.iter().filter(|w| w.running).count() as i64,
            wants,
            max_members,
        }
    }

    /// The ceiling the governor would set: max_sessions, or the plan when that is lower, and 0 when it fails closed.
    pub fn governed(&self) -> i64 {
        self.hard.min(self.plan.unwrap_or(0))
    }

    /// The ceiling that applies now: `governed` while the governor enforces, else max_sessions.
    pub fn ceiling(&self) -> i64 {
        if self.enforce {
            self.governed()
        } else {
            self.hard
        }
    }

    /// Why the ceiling is what it is: "max_sessions", "plan" or "fail closed".
    pub fn why(&self) -> &'static str {
        if !self.enforce || self.governed() >= self.hard {
            "max_sessions"
        } else if self.plan.is_none() {
            "fail closed"
        } else {
            "plan"
        }
    }

    fn bound(&self, goal: &str) -> i64 {
        self.max_members.get(&goal.to_lowercase()).copied().unwrap_or(0)
    }

    /// Each goal's share of the swarm's part of `ceiling`: leads first, then members round-robin, the goal that has waited longest (its
    /// oldest queued identity) first; a goal never gets more than it wants, nor members past max_members.
    pub fn shares(&self, ceiling: i64) -> BTreeMap<String, i64> {
        let mut free = (ceiling - self.john).max(0);
        // (goal, has a lead, members wanted, waiting since)
        let mut goals: Vec<(String, bool, i64, String)> = Vec::new();
        for w in &self.wants {
            let key = w.goal.to_lowercase();
            if !goals.iter().any(|g| g.0.to_lowercase() == key) {
                let mine: Vec<&Want> = self.wants.iter().filter(|x| x.goal.to_lowercase() == key).collect();
                let members = mine.iter().filter(|x| !x.lead).count() as i64;
                let since = mine.iter().filter(|x| !x.running).map(|x| x.since.clone()).min().unwrap_or_else(|| "\u{ffff}".into());
                goals.push((w.goal.clone(), mine.iter().any(|x| x.lead), members.min(self.bound(&w.goal)), since));
            }
        }
        goals.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        let mut share: BTreeMap<String, i64> = goals.iter().map(|g| (g.0.to_lowercase(), 0)).collect();
        for g in goals.iter().filter(|g| g.1) {
            if free > 0 {
                share.insert(g.0.to_lowercase(), 1);
                free -= 1;
            }
        }
        let mut more = true;
        while more && free > 0 {
            more = false;
            for g in &goals {
                let k = g.0.to_lowercase();
                if free > 0 && share[&k] - i64::from(g.1) < g.2 {
                    *share.get_mut(&k).unwrap() += 1;
                    free -= 1;
                    more = true;
                }
            }
        }
        share
    }

    /// May one more session start under `ceiling`? None means yes, else why not. John's own identities (`goal` None) answer only to
    /// max_sessions; a goal's lead or member also needs a free slot under the ceiling, its goal under its fair share, and a member its
    /// goal under max_members.
    pub fn can_start(&self, goal: Option<&str>, lead: bool, ceiling: i64) -> Option<String> {
        let Some(goal) = goal else {
            return if self.running < self.hard { None } else { Some(format!("{} sessions running: max_sessions is {}", self.running, self.hard)) };
        };
        if self.running >= ceiling {
            return Some(format!("the pool is full: {} running, ceiling {ceiling}", self.running));
        }
        let key = goal.to_lowercase();
        let mine: Vec<&Want> = self.wants.iter().filter(|w| w.running && w.goal.to_lowercase() == key).collect();
        let members = mine.iter().filter(|w| !w.lead).count() as i64;
        if !lead && members >= self.bound(goal) {
            return Some(format!("goal {goal} has {members} of its max_members {} running", self.bound(goal)));
        }
        let share = self.shares(ceiling).get(&key).copied().unwrap_or(0);
        if (mine.len() as i64) < share {
            None
        } else {
            Some(format!("goal {goal} has its share of the pool running ({} of {share}) while other goals wait", mine.len()))
        }
    }

    /// The status screen's "sessions": the ceiling and why, running by owner (John, each goal, the Concierge), queued by goal, and each
    /// goal's share. "max" is the ceiling.
    pub fn report(&self) -> Value {
        let ceiling = self.ceiling();
        let shares = self.shares(ceiling);
        let mut owners = vec![json!({"owner": "John", "running": self.john})];
        let mut line = vec![format!("John {}", self.john)];
        let mut names: Vec<String> = Vec::new();
        for w in &self.wants {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&w.goal)) {
                names.push(w.goal.clone());
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        for g in names {
            let mine: Vec<&Want> = self.wants.iter().filter(|w| w.goal.eq_ignore_ascii_case(&g)).collect();
            let (run, queued) = (mine.iter().filter(|w| w.running).count(), mine.iter().filter(|w| !w.running).count());
            let share = shares.get(&g.to_lowercase()).copied().unwrap_or(0);
            let owner = if g.eq_ignore_ascii_case("concierge") { "Concierge".to_string() } else { g.clone() };
            owners.push(json!({"owner": owner, "goal": g, "running": run, "queued": queued, "share": share, "bound": 1 + self.bound(&g)}));
            line.push(format!("{owner} {run}/{share}{}", if queued > 0 { format!(", {queued} queued") } else { String::new() }));
        }
        let why = self.why();
        let reason = format!(
            "{}{}",
            match why {
                "plan" => format!("the usage plan: {}", self.plan_why),
                "fail closed" => self.plan_why.clone(),
                _ => "max_sessions (Sessions at once)".to_string(),
            },
            if !self.enforce && self.governed() < self.hard { format!("; advisory: the plan would hold it at {}", self.governed()) } else { String::new() }
        );
        json!({
            "running": self.running, "max": ceiling, "ceiling": ceiling, "why": why, "reason": reason, "max_sessions": self.hard,
            "plan": self.plan, "governed": self.governed(), "enforcing": self.enforce, "john": self.john, "swarm": (ceiling - self.john).max(0),
            "queued": self.wants.iter().filter(|w| !w.running).count(), "owners": owners,
            "summary": format!("{} of {ceiling} sessions ({why}) · {}", self.running, line.join(" · ")),
        })
    }
}
