//! John's meeting rules, enforced in code (docs/question-types.md, "Meetings and the calendar"). Outlook's availability search is not
//! limited to working hours, so an agent passes what it found through `filter_meeting_slots` before offering John any time. The rules
//! are in `settings.json` (`meeting_start`, `meeting_end`, `lunch_start`, `lunch_end`, `meeting_weekdays_only`); the defaults are his:
//! 10:00 to 16:30, no meetings 13:00 to 14:00, weekdays. Times are wall-clock in his mailbox time zone, which is how Outlook returns them.

use crate::paths;
use chrono::{Datelike, NaiveDateTime, NaiveTime, Weekday};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Rules {
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub lunch: Option<(NaiveTime, NaiveTime)>,
    pub weekdays_only: bool,
}

fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

fn time_of(v: &Value, key: &str) -> Option<NaiveTime> {
    let s = v.get(key)?.as_str()?.trim();
    NaiveTime::parse_from_str(s, "%H:%M").ok()
}

impl Rules {
    /// John's rules unless `settings` says otherwise. An unreadable value falls back to the default, never to "no rule".
    pub fn from_settings(settings: &Value) -> Rules {
        let (start, end) = (time_of(settings, "meeting_start").unwrap_or(hm(10, 0)), time_of(settings, "meeting_end").unwrap_or(hm(16, 30)));
        let lunch = match (time_of(settings, "lunch_start"), time_of(settings, "lunch_end")) {
            (Some(a), Some(b)) if a < b => Some((a, b)),
            _ if settings.get("lunch_start").is_some_and(|v| v.is_null()) => None,
            _ => Some((hm(13, 0), hm(14, 0))),
        };
        let weekdays_only = settings.get("meeting_weekdays_only").and_then(|v| v.as_bool()).unwrap_or(true);
        // A start that is not before the end would drop every slot; keep the defaults rather than silently refusing everything.
        if start >= end {
            return Rules { start: hm(10, 0), end: hm(16, 30), lunch, weekdays_only };
        }
        Rules { start, end, lunch, weekdays_only }
    }

    pub fn load() -> Rules {
        Rules::from_settings(&paths::settings())
    }

    pub fn describe(&self) -> Value {
        json!({
            "meetings": format!("{} to {}", self.start.format("%H:%M"), self.end.format("%H:%M")),
            "lunch": self.lunch.map(|(a, b)| format!("{} to {}", a.format("%H:%M"), b.format("%H:%M"))),
            "weekdays_only": self.weekdays_only,
        })
    }

    /// Why a meeting from `start` to `end` breaks the rules, or `Ok(())` when it does not.
    pub fn check(&self, start: NaiveDateTime, end: NaiveDateTime) -> Result<(), String> {
        if end <= start {
            return Err("it ends before it starts".into());
        }
        if start.date() != end.date() {
            return Err("it runs past midnight".into());
        }
        if self.weekdays_only && matches!(start.weekday(), Weekday::Sat | Weekday::Sun) {
            return Err(format!("it is on a {}", if start.weekday() == Weekday::Sat { "Saturday" } else { "Sunday" }));
        }
        if start.time() < self.start {
            return Err(format!("it starts before {}", self.start.format("%H:%M")));
        }
        if end.time() > self.end {
            return Err(format!("it ends after {}", self.end.format("%H:%M")));
        }
        if let Some((a, b)) = self.lunch {
            if start.time() < b && end.time() > a {
                return Err(format!("it overlaps lunch, {} to {}", a.format("%H:%M"), b.format("%H:%M")));
            }
        }
        Ok(())
    }
}

/// A wall-clock time from `"2026-10-20T10:30"`, `"2026-10-20T10:30:00"` (fractions allowed) or Outlook's `{dateTime, timeZone}`.
/// A time with `Z` or an offset is refused: it is not the wall-clock time in John's zone, and guessing would book the wrong hour.
fn wall_clock(v: &Value) -> Result<NaiveDateTime, String> {
    let s = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Object(o) => o.get("dateTime").and_then(|d| d.as_str()).unwrap_or("").trim().to_string(),
        _ => String::new(),
    };
    let tail = s.rsplit('T').next().unwrap_or("");
    if s.ends_with('Z') || (s.contains('T') && (tail.contains('+') || tail.contains('-'))) {
        return Err(format!("'{s}' has a Z or an offset: pass the wall-clock time in John's mailbox zone, as Outlook returns it"));
    }
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(&s, f).ok())
        .ok_or_else(|| format!("'{s}' is not a date and time like 2026-10-20T10:30"))
}

/// Splits `slots` (objects with `start` and `end`) into the ones that fit the rules, earliest first and at most `limit`, and the ones
/// that do not, each with the reason.
pub fn filter(rules: &Rules, slots: &[Value], limit: usize) -> Value {
    let mut allowed: Vec<(NaiveDateTime, Value)> = vec![];
    let mut dropped: Vec<Value> = vec![];
    for slot in slots {
        let verdict = wall_clock(&slot["start"]).and_then(|s| wall_clock(&slot["end"]).map(|e| (s, e))).and_then(|(s, e)| rules.check(s, e).map(|_| s));
        match verdict {
            Ok(start) => allowed.push((start, slot.clone())),
            Err(why) => dropped.push(json!({"slot": slot, "why": why})),
        }
    }
    allowed.sort_by_key(|(s, _)| *s);
    let over = allowed.len().saturating_sub(limit);
    let allowed: Vec<Value> = allowed.into_iter().take(limit).map(|(_, v)| v).collect();
    json!({"allowed": allowed, "dropped": dropped, "not_shown_over_limit": over, "rules": rules.describe()})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        Rules::from_settings(&Value::Null)
    }

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").unwrap()
    }

    fn slot(a: &str, b: &str) -> Value {
        json!({"start": a, "end": b})
    }

    #[test]
    fn the_defaults_are_johns() {
        let r = rules();
        assert_eq!((r.start, r.end, r.lunch, r.weekdays_only), (hm(10, 0), hm(16, 30), Some((hm(13, 0), hm(14, 0))), true));
    }

    #[test]
    fn a_morning_and_an_afternoon_slot_fit() {
        // 2026-10-20 is a Tuesday
        assert_eq!(rules().check(at("2026-10-20T10:30"), at("2026-10-20T11:00")), Ok(()));
        assert_eq!(rules().check(at("2026-10-20T14:00"), at("2026-10-20T14:30")), Ok(()));
        assert_eq!(rules().check(at("2026-10-20T10:00"), at("2026-10-20T10:30")), Ok(()), "starting exactly at 10:00 is fine");
        assert_eq!(rules().check(at("2026-10-20T16:00"), at("2026-10-20T16:30")), Ok(()), "ending exactly at 16:30 is fine");
        assert_eq!(rules().check(at("2026-10-20T12:30"), at("2026-10-20T13:00")), Ok(()), "ending as lunch starts is fine");
        assert_eq!(rules().check(at("2026-10-20T14:00"), at("2026-10-20T14:15")), Ok(()), "starting as lunch ends is fine");
    }

    #[test]
    fn each_way_to_break_the_rules_is_named() {
        let r = rules();
        let why = |a: &str, b: &str| r.check(at(a), at(b)).unwrap_err();
        assert_eq!(why("2026-10-20T09:30", "2026-10-20T10:00"), "it starts before 10:00");
        assert_eq!(why("2026-10-20T16:15", "2026-10-20T16:45"), "it ends after 16:30");
        assert_eq!(why("2026-10-20T13:00", "2026-10-20T13:30"), "it overlaps lunch, 13:00 to 14:00");
        assert_eq!(why("2026-10-20T12:45", "2026-10-20T13:15"), "it overlaps lunch, 13:00 to 14:00");
        assert_eq!(why("2026-10-20T13:45", "2026-10-20T14:15"), "it overlaps lunch, 13:00 to 14:00");
        assert_eq!(why("2026-10-20T12:00", "2026-10-20T15:00"), "it overlaps lunch, 13:00 to 14:00", "a long meeting that swallows lunch");
        assert_eq!(why("2026-10-24T11:00", "2026-10-24T11:30"), "it is on a Saturday");
        assert_eq!(why("2026-10-25T11:00", "2026-10-25T11:30"), "it is on a Sunday");
        assert_eq!(why("2026-10-20T11:00", "2026-10-21T00:30"), "it runs past midnight");
        assert_eq!(why("2026-10-20T11:30", "2026-10-20T11:00"), "it ends before it starts");
    }

    #[test]
    fn settings_change_the_rules_and_a_bad_value_never_means_no_rule() {
        let r = Rules::from_settings(&json!({"meeting_start": "09:00", "meeting_end": "17:00", "lunch_start": "12:00", "lunch_end": "13:00", "meeting_weekdays_only": false}));
        assert_eq!((r.start, r.end, r.lunch, r.weekdays_only), (hm(9, 0), hm(17, 0), Some((hm(12, 0), hm(13, 0))), false));
        assert_eq!(r.check(at("2026-10-24T09:00"), at("2026-10-24T09:30")), Ok(()), "weekends allowed when asked");
        let bad = Rules::from_settings(&json!({"meeting_start": "soon", "meeting_end": "later", "lunch_start": "noon", "lunch_end": 5}));
        assert_eq!(bad, rules(), "unreadable values fall back to John's defaults");
        let backwards = Rules::from_settings(&json!({"meeting_start": "17:00", "meeting_end": "09:00"}));
        assert_eq!((backwards.start, backwards.end), (hm(10, 0), hm(16, 30)), "a start after the end would refuse everything");
        let no_lunch = Rules::from_settings(&json!({"lunch_start": null}));
        assert_eq!(no_lunch.lunch, None, "null lunch_start turns lunch off on purpose");
    }

    #[test]
    fn filtering_keeps_the_earliest_allowed_and_says_why_for_the_rest() {
        let slots = vec![
            slot("2026-10-21T15:30:00", "2026-10-21T16:00:00"),
            slot("2026-10-20T13:00:00", "2026-10-20T13:30:00"),
            json!({"start": {"dateTime": "2026-10-20T10:30:00.0000000", "timeZone": "Eastern Standard Time"}, "end": {"dateTime": "2026-10-20T11:00:00.0000000", "timeZone": "Eastern Standard Time"}}),
            slot("2026-10-20T16:45:00", "2026-10-20T17:15:00"),
            slot("2026-10-22T14:00:00", "2026-10-22T14:30:00"),
            slot("2026-10-23T11:00:00", "2026-10-23T11:30:00"),
        ];
        let r = filter(&rules(), &slots, 3);
        let starts: Vec<String> = r["allowed"].as_array().unwrap().iter().map(|s| s["start"].to_string()).collect();
        assert_eq!(r["allowed"].as_array().unwrap().len(), 3);
        assert!(starts[0].contains("2026-10-20T10:30"), "earliest first: {starts:?}");
        assert!(starts[2].contains("2026-10-22T14:00"), "{starts:?}");
        assert_eq!(r["not_shown_over_limit"], 1, "four fit, three shown");
        let why: Vec<String> = r["dropped"].as_array().unwrap().iter().map(|d| d["why"].as_str().unwrap().to_string()).collect();
        assert_eq!(why, vec!["it overlaps lunch, 13:00 to 14:00", "it ends after 16:30"]);
        assert_eq!(r["rules"]["meetings"], "10:00 to 16:30");
    }

    #[test]
    fn a_time_with_a_zone_marker_or_junk_is_dropped_not_guessed() {
        let r = filter(
            &rules(),
            &[
                slot("2026-10-20T15:00:00Z", "2026-10-20T15:30:00Z"),
                slot("2026-10-20T10:00:00-04:00", "2026-10-20T10:30:00-04:00"),
                slot("tomorrow", "later"),
                json!({"start": "2026-10-20T11:00"}),
                json!(5),
            ],
            3,
        );
        assert_eq!(r["allowed"].as_array().unwrap().len(), 0);
        let dropped = r["dropped"].as_array().unwrap();
        assert_eq!(dropped.len(), 5);
        assert!(dropped[0]["why"].as_str().unwrap().contains("wall-clock time in John's mailbox zone"));
        assert!(dropped[1]["why"].as_str().unwrap().contains("Z or an offset"));
        assert!(dropped[2]["why"].as_str().unwrap().contains("not a date and time"));
    }
}
