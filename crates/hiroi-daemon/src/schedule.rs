//! When scheduler rules hold.
//!
//! Everything here is a pure function of a [`Moment`] (weekday and minute
//! of the day, local time), so the rules can be tested without a clock.
//! The service asks once a minute what the rules want and acts only on
//! changes, which keeps manual commands in force until the next change.

use std::collections::BTreeMap;

use hiroi_proto::{Schedule, ScheduleAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    /// 0 is Monday.
    pub weekday: u8,
    /// Minutes after midnight.
    pub minute: u16,
}

impl Moment {
    pub fn now() -> Self {
        let now = jiff::ZonedDateTime::now();
        Self {
            weekday: now.weekday().to_monday_zero_offset() as u8,
            minute: now.hour() as u16 * 60 + now.minute() as u16,
        }
    }
}

/// What the window rules want at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Desired {
    /// For every queue that some window rule controls: should it run?
    pub queues: BTreeMap<String, bool>,
    /// Lowest limit of the speed windows active now, if any.
    pub limit: Option<u64>,
}

pub fn desired(rules: &[Schedule], at: Moment) -> Desired {
    let mut desired = Desired::default();
    for rule in rules.iter().filter(|rule| rule.to_minute.is_some()) {
        let on = active(rule, at);
        match &rule.action {
            ScheduleAction::RunQueue { queue } => {
                *desired.queues.entry(queue.clone()).or_default() |= on;
            }
            ScheduleAction::SpeedLimit { limit } if on => {
                desired.limit = Some(desired.limit.map_or(*limit, |current| current.min(*limit)));
            }
            ScheduleAction::SpeedLimit { .. } => {}
        }
    }
    desired
}

/// One-shot rules (no end time) that fire at `at`.
pub fn firing(rules: &[Schedule], at: Moment) -> impl Iterator<Item = &Schedule> {
    rules.iter().filter(move |rule| {
        rule.to_minute.is_none() && starts_on(rule, at.weekday) && rule.from_minute == at.minute
    })
}

/// Whether a window rule holds at `at`. A window that crosses midnight
/// belongs to the day it starts on.
pub fn active(rule: &Schedule, at: Moment) -> bool {
    let Some(to) = rule.to_minute else {
        return false;
    };
    let from = rule.from_minute;
    let yesterday = (at.weekday + 6) % 7;
    if from <= to {
        starts_on(rule, at.weekday) && at.minute >= from && at.minute < to
    } else {
        (starts_on(rule, at.weekday) && at.minute >= from)
            || (starts_on(rule, yesterday) && at.minute < to)
    }
}

fn starts_on(rule: &Schedule, weekday: u8) -> bool {
    rule.days & (1 << weekday) != 0
}

/// Parses `HH:MM`.
pub fn parse_time(text: &str) -> Option<u16> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let (hours, minutes): (u16, u16) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

/// Parses `daily`, `weekdays`, `weekends`, or lists and ranges of day
/// names such as `mon-fri` or `sat,sun`.
pub fn parse_days(text: &str) -> Option<u8> {
    const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let day = |name: &str| {
        NAMES
            .iter()
            .position(|day| name.trim().to_ascii_lowercase().starts_with(day))
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "daily" | "every day" | "all" => return Some(hiroi_proto::EVERY_DAY),
        "weekdays" => return Some(0b001_1111),
        "weekends" => return Some(0b110_0000),
        _ => {}
    }
    let mut days = 0u8;
    for part in text.split(',') {
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (day(first)?, day(last)?);
                let mut current = first;
                loop {
                    days |= 1 << current;
                    if current == last {
                        break;
                    }
                    current = (current + 1) % 7;
                }
            }
            None => days |= 1 << day(part)?,
        }
    }
    (days != 0).then_some(days)
}

/// `HH:MM` for display.
pub fn format_time(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// `mon,wed-fri` style for display.
pub fn format_days(days: u8) -> String {
    const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    match days {
        hiroi_proto::EVERY_DAY => "daily".into(),
        0b001_1111 => "weekdays".into(),
        0b110_0000 => "weekends".into(),
        _ => (0..7)
            .filter(|day| days & (1 << day) != 0)
            .map(|day| NAMES[day])
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiroi_proto::EVERY_DAY;

    fn at(weekday: u8, time: &str) -> Moment {
        Moment {
            weekday,
            minute: parse_time(time).unwrap(),
        }
    }

    fn window(days: u8, from: &str, to: &str, action: ScheduleAction) -> Schedule {
        Schedule {
            id: 1,
            days,
            from_minute: parse_time(from).unwrap(),
            to_minute: Some(parse_time(to).unwrap()),
            action,
        }
    }

    fn night() -> ScheduleAction {
        ScheduleAction::RunQueue {
            queue: "night".into(),
        }
    }

    #[test]
    fn windows_within_a_day() {
        let rule = window(0b001_1111, "09:00", "17:00", night());
        assert!(!active(&rule, at(0, "08:59")));
        assert!(active(&rule, at(0, "09:00")));
        assert!(active(&rule, at(4, "16:59")));
        assert!(!active(&rule, at(4, "17:00")));
        assert!(!active(&rule, at(5, "12:00")), "Saturday is not a weekday");
    }

    #[test]
    fn windows_across_midnight_belong_to_their_start_day() {
        // Friday 23:00 to Saturday 06:00, Fridays only.
        let rule = window(1 << 4, "23:00", "06:00", night());
        assert!(active(&rule, at(4, "23:30")));
        assert!(active(&rule, at(5, "05:59")));
        assert!(
            !active(&rule, at(5, "23:30")),
            "Saturday does not start a window"
        );
        assert!(
            !active(&rule, at(4, "05:00")),
            "Thursday did not start one either"
        );
    }

    #[test]
    fn overlapping_limits_take_the_lowest() {
        let rules = [
            window(
                EVERY_DAY,
                "08:00",
                "20:00",
                ScheduleAction::SpeedLimit { limit: 5_000_000 },
            ),
            window(
                EVERY_DAY,
                "12:00",
                "13:00",
                ScheduleAction::SpeedLimit { limit: 1_000_000 },
            ),
            window(EVERY_DAY, "01:00", "07:00", night()),
        ];
        let noon = desired(&rules, at(2, "12:30"));
        assert_eq!(noon.limit, Some(1_000_000));
        assert_eq!(noon.queues.get("night"), Some(&false));
        let early = desired(&rules, at(2, "02:00"));
        assert_eq!(
            (early.limit, early.queues.get("night")),
            (None, Some(&true))
        );
    }

    #[test]
    fn one_shot_rules_fire_on_their_minute_only() {
        let rule = Schedule {
            id: 1,
            days: EVERY_DAY,
            from_minute: 120,
            to_minute: None,
            action: night(),
        };
        assert_eq!(
            firing(std::slice::from_ref(&rule), at(3, "02:00")).count(),
            1
        );
        assert_eq!(
            firing(std::slice::from_ref(&rule), at(3, "02:01")).count(),
            0
        );
        assert!(!active(&rule, at(3, "02:00")));
    }

    #[test]
    fn parses_days_and_times() {
        assert_eq!(parse_days("mon-fri"), Some(0b001_1111));
        assert_eq!(parse_days("sat,sun"), Some(0b110_0000));
        assert_eq!(parse_days("fri-mon"), Some(0b111_0001));
        assert_eq!(parse_days("Monday"), Some(1));
        assert_eq!(parse_days("daily"), Some(EVERY_DAY));
        assert_eq!(parse_days("someday"), None);
        assert_eq!(parse_time("07:30"), Some(450));
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(format_days(0b000_0101), "mon,wed");
        assert_eq!(format_time(450), "07:30");
    }
}
