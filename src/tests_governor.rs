//! The usage governor: the formula, the model it trains, what the board stores and shows, and a backtest that follows its caps through
//! synthetic weeks. Ported from the .NET GovernorTests; the numbers are the same.

use crate::governor::*;
use crate::tests_identities::Env;
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use serde_json::json;

type Time = DateTime<Utc>;

fn mon() -> Time {
    Utc.with_ymd_and_hms(2026, 9, 21, 0, 0, 0).unwrap() // a Monday, 00:00 UTC
}

fn s() -> Settings {
    Settings::default()
}

fn smp(ts: Time, pct: f64, reset: Option<Time>, five: Option<f64>, five_reset: Option<Time>, sessions: i64) -> Sample {
    Sample::new(ts, pct, reset, five, five_reset, sessions)
}

fn hrs(h: f64) -> Duration {
    Duration::milliseconds((h * 3_600_000.0) as i64)
}

fn hours_between_t(a: Time, b: Time) -> f64 {
    (b - a).num_seconds() as f64 / 3600.0
}

fn is_sorted(v: &[f64]) -> bool {
    v.windows(2).all(|w| w[0] <= w[1])
}

/// A small deterministic generator (xorshift): the tests need noise, not good noise.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[test]
fn hour_of_week_starts_monday_utc() {
    assert_eq!(hour_of_week(mon()), 0);
    assert_eq!(hour_of_week(mon() + Duration::days(2) + Duration::hours(10) + Duration::minutes(59)), 24 * 2 + 10);
    assert_eq!(hour_of_week(mon() - Duration::hours(1)), 167);
}

#[test]
fn the_allowance_ramps_up_through_the_week_so_the_cushion_is_spent_at_the_end() {
    let st = s();
    assert!((pace(0.0, st.ramp, st.floor) - st.floor).abs() < 1e-6);
    assert!((pace(0.999999, st.ramp, st.floor) - 1.0).abs() < 0.01);
    let paces: Vec<f64> = [0.0, 0.1, 0.3, 0.5, 0.7, 0.9, 0.99].iter().map(|t| pace(*t, st.ramp, st.floor)).collect();
    assert!(is_sorted(&paces), "never eases off");
    assert_eq!(pace(0.2, 0.0, st.floor), 1.0, "a ramp of 0 is the old flat spread");
    let reset = mon() + Duration::days(7);
    let model = BurnModel::default();
    let rate = |h: f64| advise(&model, &smp(mon() + hrs(h), 20.0, Some(reset), Some(0.0), None, 0), 0, mon() + hrs(h), &st).allowed_rate;
    assert!(rate(2.0) < rate(80.0) && rate(80.0) < rate(160.0), "the allowance rises through the week");
}

#[test]
fn the_plan_path_runs_to_the_reset_and_lands_near_the_top_of_the_plan() {
    let reset = mon() + Duration::days(7);
    let path = plan_path(&BurnModel::default(), &smp(mon(), 0.0, Some(reset), Some(0.0), None, 0), mon(), &s());
    assert_eq!(path.len(), 168);
    assert!(is_sorted(&path), "only goes up");
    assert!((90.0..=100.0).contains(path.last().unwrap()), "it tries to use the week, less the margin: {}", path.last().unwrap());
    assert!(status(98.0, Some(70.0)).contains("behind"));
    assert!(status(98.0, Some(97.0)).contains("on pace"));
    assert!(status(98.0, Some(100.0)).contains("limit"));
}

#[test]
fn reserve_shrinks_to_nothing_as_the_reset_nears() {
    let reserves: Vec<f64> = [168.0, 72.0, 24.0, 4.0, 1.0, 0.1, 0.0].iter().map(|h| 60.0 - spendable(60.0, 5.0, 1.0, *h, 2.0)).collect();
    for i in 1..reserves.len() {
        assert!(reserves[i] < reserves[i - 1], "reserve grew at step {i}: {reserves:?}");
    }
    assert!((reserves[reserves.len() - 1] - 5.0).abs() < 1e-6, "only the forecast is left at the reset");
    assert!((2.0 * 24f64.sqrt() - (60.0 - 5.0 - spendable(60.0, 5.0, 1.0, 24.0, 2.0))).abs() < 1e-6);
}

#[test]
fn spendable_is_never_negative() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for _ in 0..10_000 {
        let v = spendable(rng.next() * 120.0 - 10.0, rng.next() * 150.0, rng.next() * 5.0, rng.next() * 200.0, rng.next() * 4.0);
        assert!(v >= 0.0);
    }
    assert_eq!(spendable(10.0, 50.0, 1.0, 24.0, 2.0), 0.0);
    let a = advise(&BurnModel::default(), &smp(mon(), 99.5, Some(mon() + Duration::days(6)), Some(0.0), None, 0), 3, mon(), &s()); // less left than the margin
    assert_eq!(a.spendable, 0.0);
    assert_eq!((a.total_sessions, a.new_sessions, a.swarms), (0, 0, 0));
}

#[test]
fn the_five_hour_guard_allows_no_swarm_sessions() {
    let model = BurnModel::default();
    let at = |five: f64, reset: Time| smp(mon(), 10.0, Some(mon() + Duration::hours(2)), Some(five), Some(reset), 2);
    let open = advise(&model, &at(89.0, mon() + Duration::hours(1)), 2, mon(), &s());
    assert!(open.new_sessions > 0, "{}", open.reason);
    assert_eq!(open.total_sessions, s().max_sessions); // 89% spendable over 2 h funds more than the ceiling
    let guarded = advise(&model, &at(90.0, mon() + Duration::hours(1)), 2, mon(), &s());
    assert_eq!((guarded.new_sessions, guarded.total_sessions), (0, 0));
    assert!(guarded.reason.contains("5-hour window"));
    assert!(guarded.step_down);
    assert_eq!(advise(&model, &at(95.0, mon() + Duration::hours(1)), 0, mon(), &s()).total_sessions, 0);
    assert!(advise(&model, &at(95.0, mon() - Duration::minutes(1)), 2, mon(), &s()).new_sessions > 0, "that window already reset");
}

#[test]
fn tight_budgets_step_members_down_a_tier() {
    let model = BurnModel::default();
    let tight = advise(&model, &smp(mon(), 40.0, Some(mon() + Duration::days(6)), Some(5.0), None, 0), 0, mon(), &s());
    assert!(tight.step_down);
    assert_eq!((tight.member_model, tight.lead_model), ("haiku", "sonnet"));
    let loose = advise(&model, &smp(mon(), 10.0, Some(mon() + Duration::hours(12)), Some(5.0), None, 0), 0, mon(), &s());
    assert!(!loose.step_down);
    assert_eq!((loose.member_model, loose.lead_model), ("sonnet", "opus"));
    assert_eq!(loose.total_sessions, loose.swarms * (1 + loose.members_per_swarm) + loose.total_sessions % (1 + loose.members_per_swarm));
}

#[test]
fn train_learns_johns_hours_and_what_a_session_costs() {
    // Two weeks: John burns 1%/h on Mondays 10:00-11:00 and nothing else; two sessions run Tuesdays 09:00-12:00 at 1.5%/h each.
    let (mut samples, mut pct, mut t) = (Vec::new(), 0.0, mon());
    while t < mon() + Duration::days(14) {
        let how = hour_of_week(t);
        let sessions = if (24 + 9..24 + 12).contains(&how) { 2 } else { 0 };
        samples.push(smp(t, pct, Some(mon() + Duration::days(if t < mon() + Duration::days(7) { 7 } else { 14 })), Some(0.0), None, sessions));
        pct = if t + Duration::minutes(5) == mon() + Duration::days(7) { 0.0 } else { pct + ((if how == 10 { 1.0 } else { 0.0 }) + sessions as f64 * 1.5) / 12.0 };
        t += Duration::minutes(5);
    }
    let m = train(&samples, &s());
    assert!((m.rate(10, &s()) - 1.0).abs() < 1e-6);
    assert!(m.rate(11, &s()).abs() < 1e-6);
    assert!((m.per_session(&s()) - 1.5).abs() < 0.1, "the hour a session ends counts it half: {}", m.per_session(&s()));
    assert_eq!(m.n[10], 2);
    assert!((baseline(&m, mon() + hrs(9.5), mon() + hrs(11.5), &s()) - 1.0).abs() < 1e-6);
}

#[test]
fn cheap_sessions_early_in_a_fresh_week_are_allowed_not_rounded_down_to_none() {
    // Long-lived sessions that mostly idle burn 0.05%/h each over John's 0.3%/h, and the new week has just begun.
    let (mut samples, mut pct, mut t) = (Vec::new(), 0.0, mon());
    while t < mon() + Duration::days(14) {
        let sessions = 1 + (hours_between_t(mon(), t) / 3.0) as i64 % 3 * 8; // 1, 9, 17 sessions in 3-hour blocks
        let week_start = mon() + Duration::days(7 * (hours_between_t(mon(), t) / 168.0) as i64);
        if t == week_start {
            pct = 0.0;
        }
        samples.push(smp(t, pct, Some(week_start + Duration::days(7)), Some(0.0), None, sessions));
        pct += (0.3 + sessions as f64 * 0.05) / 12.0;
        t += Duration::minutes(5);
    }
    let m = train(&samples, &s());
    assert!((MIN_SESSION_RATE..=0.15).contains(&m.per_session(&s())), "the measured cost, not the old 0.25 floor: {}", m.per_session(&s()));
    let week_one = mon() + Duration::days(7) + Duration::hours(6);
    let latest = smp(week_one, 4.0, Some(mon() + Duration::days(14)), Some(3.0), Some(week_one + Duration::hours(2)), 20);
    let a = advise(&m, &latest, 20, week_one, &s());
    assert!(a.total_sessions >= 2, "allowance {} sessions: {}", a.total_sessions, a.reason);
}

#[test]
fn starved_of_session_free_hours_johns_baseline_is_estimated() {
    // Three weeks in which the swarm never stops: 1 to 3 sessions in 3-hour blocks, each burning 1.2%/h, over John's 0.4%/h.
    let (mut samples, mut pct, mut t) = (Vec::new(), 0.0, mon());
    while t < mon() + Duration::days(21) {
        let sessions = 1 + (hours_between_t(mon(), t) / 3.0) as i64 % 3;
        let week_start = mon() + Duration::days(7 * (hours_between_t(mon(), t) / 168.0) as i64);
        if t == week_start {
            pct = 0.0;
        }
        samples.push(smp(t, pct, Some(week_start + Duration::days(7)), Some(0.0), None, sessions));
        pct += (0.4 + sessions as f64 * 1.2) / 12.0;
        t += Duration::minutes(5);
    }
    let m = train(&samples, &s());
    assert_eq!(m.baseline_hours, 0);
    assert_eq!(m.source(), "estimated");
    assert!(m.slope_ok);
    assert!((m.per_session(&s()) - 1.2).abs() < 0.1, "{}", m.per_session(&s()));
    assert!((0.3..=0.5).contains(&m.rate(10, &s())), "within the sampling error of a 5-minute feed whose session count changes mid-pair: {}", m.rate(10, &s()));
    let b = baseline(&m, mon(), mon() + Duration::days(1), &s());
    assert!((0.3 * 24.0..=0.5 * 24.0).contains(&b), "{b}");
    // A swarm that never changes size cannot be told apart from John: the per-session default stands, and so does the estimate it implies.
    let flat_samples: Vec<Sample> = samples.iter().map(|x| Sample { sessions: 2, ..x.clone() }).collect();
    let flat = train(&flat_samples, &s());
    assert!(!flat.slope_ok);
    assert_eq!(flat.per_session(&s()), MIN_SESSION_RATE.max(flat.session_rate));
    assert!(flat.rate(10, &s()).is_finite());
}

/// The 161% forecast from the live core.log (2026-09-27), rebuilt from a series shaped like the recorded one: only four session-free
/// hours, so the starvation estimate stands in, and it used to be spread flat over the 137 hours to the reset with no cap.
#[test]
fn a_young_estimate_never_forecasts_more_than_is_left() {
    let start = Utc.with_ymd_and_hms(2026, 9, 26, 15, 0, 0).unwrap();
    let reset = Utc.with_ymd_and_hms(2026, 10, 3, 8, 0, 0).unwrap();
    let hours: [(f64, i64); 24] = [
        (1.7, 0),
        (3.0, 0),
        (2.2, 0),
        (3.0, 0),
        (4.0, 1),
        (2.0, 1),
        (2.0, 1),
        (2.0, 1),
        (2.1, 3),
        (1.0, 4),
        (1.0, 4),
        (4.4, 4),
        (1.0, 4),
        (0.0, 4),
        (0.0, 4),
        (0.0, 4),
        (5.4, 4),
        (0.0, 4),
        (0.0, 4),
        (0.0, 4),
        (0.0, 4),
        (1.0, 4),
        (1.0, 4),
        (1.0, 4),
    ];
    let (mut samples, mut pct, mut t) = (Vec::new(), 9.0f64, start);
    while t < start + Duration::hours(hours.len() as i64) {
        let (rate, sessions) = hours[hours_between_t(start, t) as usize];
        let wobble = if t.minute() % 10 == 0 { -1 } else { 0 };
        samples.push(smp(t, pct.floor(), Some(reset + Duration::minutes(wobble)), Some(10.0), None, sessions)); // the feed's reset wobbles by a minute
        pct += rate / 12.0;
        t += Duration::minutes(5);
    }
    let now = start + Duration::hours(hours.len() as i64) - Duration::minutes(2);
    let m = train(&samples, &s());
    let a = advise(&m, samples.last().unwrap(), 4, now, &s());
    assert_eq!((m.source(), m.global_n < 6), ("estimated", true), "too few session-free hours to measure John");
    assert!(m.slope < 0.0 && m.per_session(&s()) == MIN_SESSION_RATE, "slope {}, per session {}", m.slope, m.per_session(&s()));
    assert!(m.est_mean * a.reset_in_hours > 100.0, "the unshrunk estimate forecasts {:.0}%: the bug needs more than the whole plan", m.est_mean * a.reset_in_hours);
    assert!(a.baseline <= a.remaining, "forecast {} of {} left", a.baseline, a.remaining);
    assert!(a.used + a.baseline <= 100.0);
    assert!((0.0..=MAX_RESERVE).contains(&a.reserve));
    assert!(a.projected_end <= 100.0);
    assert!((s().default_rate..=m.est_mean).contains(&m.rate(0, &s())), "shrunk toward the default until it has a day of its own");
}

#[test]
fn the_verdict_fails_closed_and_steps_tiers_down_only() {
    let e = Env::new();
    let now = Utc::now();
    assert!(!judge(&e.db, now, false).fresh, "no sample at all");
    insert(&e.db, &smp(now - Duration::minutes(29), 10.0, Some(now + Duration::hours(2)), Some(80.0), Some(now + Duration::hours(1)), 0), (0, 0, 0)).unwrap();
    let v = judge(&e.db, now, false);
    assert!(v.fresh);
    assert!(!v.enforce(), "off by default");
    assert!(Budget::read(&e.db, 99, &v).governed() > 3, "the pool's ceiling comes from a fresh plan");
    assert_eq!(Budget::read(&e.db, 99, &judge(&e.db, now, true)).governed(), 0, "fail closed: no swarm slot");
    let stale = judge(&e.db, now + Duration::minutes(2), false);
    assert_eq!((stale.fresh, Budget::read(&e.db, 99, &stale).governed()), (false, 0), "stale: no swarm slot, and nothing to shed on");
    assert_eq!(
        (v.model("opus", true), v.model("sonnet", false), v.model("haiku", false), v.model("sonnet", true)),
        ("sonnet".to_string(), "haiku".to_string(), "haiku".to_string(), "sonnet".to_string())
    );
}

#[test]
fn samples_are_recorded_once_and_shown_in_the_report() {
    let e = Env::new();
    assert_eq!(report(&e.db, Utc::now(), false, None)["summary"], "governor: no usage samples yet");
    assert!(!record(&e.db));
    let reset = Utc::now() + Duration::days(3);
    std::fs::create_dir_all(e.data()).unwrap();
    std::fs::write(
        e.data().join("claude_usage.json"),
        json!({"captured_ts": Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string(), "five_hour": {"used": 12, "resets_at": null},
               "seven_day": {"used": 40, "resets_at": reset.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()}})
        .to_string(),
    )
    .unwrap();
    e.db.exec("INSERT INTO identities (name, folder, state, created_ts, updated_ts) VALUES ('a', 'C:\\', 'running', 'x', 'x')", &[]).unwrap();
    assert!(record(&e.db));
    assert!(record(&e.db), "the same captured_ts: still one sample");
    let r = e.db.row("SELECT COUNT(*) AS n, MAX(weekly_pct) AS w, MAX(five_hour_pct) AS f, MAX(swarm_sessions) AS s FROM usage_samples", &[]).unwrap().unwrap();
    assert_eq!((r["n"].as_i64(), r["w"].as_f64(), r["f"].as_f64(), r["s"].as_i64()), (Some(1), Some(40.0), Some(12.0), Some(1)));
    let g = report(&e.db, Utc::now(), false, None);
    assert_eq!(g["samples"], 1);
    assert_eq!(g["remaining"], 60.0);
    assert!((71.9..=72.0).contains(&g["reset_in_hours"].as_f64().unwrap()));
    assert_eq!(g["caps"]["running"], 1);
    assert_eq!(g["series"][0], 40.0);
    for key in ["baseline", "spendable", "projected_end_pct", "reason"] {
        assert!(g.get(key).is_some(), "{key}");
    }
    assert!(g["summary"].as_str().unwrap().starts_with("governor: "));
}

#[test]
fn the_series_is_the_last_reading_of_each_hour_over_7_days() {
    let samples: Vec<Sample> = (0..8 * 24 * 12).map(|i| smp(mon() + Duration::minutes(5 * i), (i / 12 % 100) as f64, None, None, None, 0)).collect();
    let v = series(&samples, mon() + Duration::days(8));
    assert_eq!(v.len(), 7 * 24);
    assert_eq!(v[0], 24.0, "hour 24: the first day is older than 7 days");
    assert_eq!(v[v.len() - 1], 91.0, "hour 191, wrapped at 100");
}

// ---- the backtest

/// John is awake 12:00-03:00 UTC (8am-11pm Eastern).
fn steady(t: Time) -> f64 {
    if t.hour() >= 12 || t.hour() < 3 {
        0.35
    } else {
        0.03
    }
}
fn bursty(t: Time) -> f64 {
    steady(t) + if t.weekday() == chrono::Weekday::Wed && (14..20).contains(&t.hour()) { 3.0 } else { 0.0 }
}
fn light(t: Time) -> f64 {
    0.3 * steady(t)
}

/// Weeks back to back, resetting Fridays 08:00 UTC, following the governor's caps every 5 minutes: each session burns 1.2%/h of the
/// week, John burns his trace with noise, and /usage reports whole percentages, rounded down. A 5-hour window holds a quarter of a week.
fn simulate(weeks: &[fn(Time) -> f64], seed: u64) -> (Vec<f64>, f64, f64) {
    const SESSION_BURN: f64 = 1.2;
    const FIVE_RATIO: f64 = 4.0;
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let mut samples: Vec<Sample> = Vec::new();
    let start = Utc.with_ymd_and_hms(2026, 9, 4, 8, 0, 0).unwrap();
    let (mut ends, mut max_weekly, mut max_five) = (Vec::new(), 0.0f64, 0.0f64);
    let (mut pct, mut five, mut sessions) = (0.0f64, 0.0f64, 0i64);
    let mut five_reset: Option<Time> = None;
    let mut model = train(&samples, &s());
    for (w, john) in weeks.iter().enumerate() {
        let reset = start + Duration::days(7 * (w as i64 + 1));
        let mut t = start + Duration::days(7 * w as i64);
        while t < reset {
            if t.minute() == 0 {
                model = train(&samples, &s());
            }
            if five_reset.is_some_and(|f| f <= t) {
                (five, five_reset) = (0.0, None);
            }
            let seen = smp(t, pct.floor(), Some(reset), Some(five.floor()), five_reset, sessions);
            sessions = advise(&model, &seen, sessions, t, &s()).total_sessions;
            samples.push(Sample { sessions, ..seen });
            let burn = (john(t) * (0.5 + rng.next()) + sessions as f64 * SESSION_BURN * (0.5 + rng.next())) / 12.0;
            if burn > 0.0 && five_reset.is_none() {
                five_reset = Some(t + Duration::hours(5));
            }
            (pct, five) = (pct + burn, five + burn * FIVE_RATIO);
            (max_weekly, max_five) = (max_weekly.max(pct), max_five.max(five));
            t += Duration::minutes(5);
        }
        ends.push(pct);
        pct = 0.0;
    }
    (ends, max_weekly, max_five)
}

#[test]
fn backtest_following_the_caps_ends_the_week_between_85_and_100() {
    for (name, week) in [("steady", steady as fn(Time) -> f64), ("bursty", bursty), ("light", light)] {
        for seed in 1..=3 {
            // Two ordinary weeks first, from no history at all (the defaults), then the week under test.
            let (ends, max_weekly, max_five) = simulate(&[steady, steady, week], seed);
            let line = format!("{name} seed {seed}: weeks end at {ends:?}; max weekly {max_weekly:.1}%, max 5-hour {max_five:.1}%");
            assert!(max_weekly <= 100.0, "went over: {line}");
            assert!(max_five <= 90.0, "5-hour window over 90%: {line}");
            assert!((85.0..=100.0).contains(&ends[2]), "{line}");
            assert!((85.0..=100.0).contains(&ends[1]), "{line}");
        }
    }
}

#[test]
fn the_pool_gives_leads_first_then_members_and_holds_a_goal_at_its_max_members() {
    let e = Env::new();
    for (goal, lead, max) in [("alpha", "alpha-lead", 2i64), ("beta", "beta-lead", 1)] {
        crate::identities::create(&e.db, lead, e.folder(), None, None, false).unwrap();
        e.db.exec(
            "INSERT INTO goals (name, objective, measure_folder, max_members, state, lead, created_ts, updated_ts) VALUES (:n,'o',:f,:m,'running',:l,'x','x')",
            &[(":n", &goal), (":f", &e.folder()), (":m", &max), (":l", &lead)],
        )
        .unwrap();
        e.db.exec("UPDATE identities SET state='running' WHERE name=:l", &[(":l", &lead)]).unwrap();
        for i in 1..=max {
            let m = format!("{goal}-m{i}");
            crate::identities::create(&e.db, &m, e.folder(), None, None, false).unwrap();
            e.db.exec("INSERT INTO goal_members (identity, goal, task, created_ts) VALUES (:i,:g,'t','x')", &[(":i", &m), (":g", &goal)]).unwrap();
            e.db.exec("UPDATE identities SET state='queued' WHERE name=:i", &[(":i", &m)]).unwrap();
        }
    }
    let v = Verdict { settings: s(), advice: None, fresh: false, why: String::new() };
    let pool = Budget::read(&e.db, 4, &v);
    assert_eq!((pool.running, pool.john), (2, 0));
    let shares = pool.shares(4);
    assert_eq!(shares["alpha"] + shares["beta"], 4, "{shares:?}");
    assert!(pool.can_start(Some("alpha"), false, 4).is_none() || pool.can_start(Some("beta"), false, 4).is_none());
    assert!(pool.can_start(Some("alpha"), false, 2).unwrap().contains("pool is full"));
    assert!(pool.can_start(None, false, 2).is_none(), "John's own answer only to max_sessions");
    let r = pool.report();
    assert_eq!(r["ceiling"], 4);
    assert!(r["summary"].as_str().unwrap().starts_with("2 of 4 sessions"));
}

#[test]
fn the_supervisor_holds_and_sheds_swarm_sessions_when_the_governor_enforces_and_never_touches_johns() {
    let e = Env::new();
    let now = Utc::now();
    // the 5-hour window is at 95%: the plan allows no swarm sessions at all
    insert(&e.db, &smp(now, 10.0, Some(now + Duration::days(3)), Some(95.0), Some(now + Duration::hours(1)), 0), (0, 0, 0)).unwrap();
    crate::identities::create(&e.db, "g-lead", e.folder(), None, None, false).unwrap();
    crate::identities::create(&e.db, "johns", e.folder(), None, None, false).unwrap();
    e.db.exec("INSERT INTO goals (name, objective, measure_folder, state, lead, created_ts, updated_ts) VALUES ('g','o',:f,'running','g-lead','x','x')", &[(":f", &e.folder())])
        .unwrap();
    e.db.exec("UPDATE identities SET state='queued' WHERE name IN ('g-lead','johns')", &[]).unwrap();
    std::fs::write(e.data().join("settings.json"), r#"{"governor_enforce": true}"#).unwrap_or_else(|_| {
        std::fs::create_dir_all(e.data()).unwrap();
        std::fs::write(e.data().join("settings.json"), r#"{"governor_enforce": true}"#).unwrap();
    });
    let mut sup = crate::supervisor::Supervisor::new();
    sup.tick(&e.db).unwrap();
    let state = |n: &str| crate::identities::get(&e.db, n).unwrap().unwrap()["state"].as_str().unwrap().to_string();
    assert_eq!(state("johns"), "running", "John's own answer only to max_sessions");
    assert_eq!(state("g-lead"), "queued", "held by the plan");
    assert!(sup.held("g-lead").unwrap().contains("governor caps the pool"), "{:?}", sup.held("g-lead"));
    // advisory: the same plan holds nothing back
    std::fs::write(e.data().join("settings.json"), r#"{"governor_enforce": false}"#).unwrap();
    sup.tick(&e.db).unwrap();
    assert_eq!(state("g-lead"), "running");
    // enforcing again with the lead running: it is shed, John's identity is not
    std::fs::write(e.data().join("settings.json"), r#"{"governor_enforce": true}"#).unwrap();
    sup.tick(&e.db).unwrap();
    assert_eq!(state("g-lead"), "queued", "shed");
    assert_eq!(state("johns"), "running");
    sup.shutdown();
}
