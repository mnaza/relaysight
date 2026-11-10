//! Which stretches of the ring a schedule says to keep, and when.
//!
//! A schedule rule is a window of the week in the gateway's own local time —
//! "weekdays, 08:00 to 18:00" — and keeping it means uploading that stretch of
//! the ring once it has passed. This file is the arithmetic, with no clock and
//! no disk in it, because a window that lands an hour out is the kind of bug
//! that only shows up on the day the clocks change.
//! See docs/superpowers/specs/2026-09-23-recording-policies-design.md.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use vms_domain::KeepRule;

/// Clips are cut this long at most. An eight-hour window is still eight hours
/// of video; it just arrives as a series of recordings a player can open.
pub const MAX_CLIP: Duration = Duration::minutes(10);

/// How far behind now to stop. A segment is only on disk once it has closed,
/// so asking for the last few seconds asks for video that does not exist yet.
pub const LAG: Duration = Duration::seconds(30);

/// How a gateway cuts the windows it keeps.
#[derive(Debug, Clone, Copy)]
pub struct Cutting {
    pub max_clip: Duration,
    pub lag: Duration,
}

impl Default for Cutting {
    fn default() -> Self {
        Self {
            max_clip: MAX_CLIP,
            lag: LAG,
        }
    }
}

/// How far back a gateway that was off will reach when it comes back. Beyond
/// this the ring will not have the video anyway, and trying is a way to spend
/// a morning uploading yesterday.
pub const MAX_CATCH_UP: Duration = Duration::hours(6);

/// The stretches to keep, in order, given what was already kept.
///
/// `watermark` is the instant everything before which has been dealt with.
/// `zone` is the gateway's local time zone, because a schedule is written in
/// the time of the site it watches.
pub fn windows_to_keep<Z: TimeZone>(
    rules: &[KeepRule],
    watermark: DateTime<Utc>,
    now: DateTime<Utc>,
    zone: &Z,
    cutting: Cutting,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let cutoff = now - cutting.lag;
    let from = watermark.max(cutoff - MAX_CATCH_UP);
    if cutoff <= from {
        return Vec::new();
    }

    let mut windows: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for rule in rules {
        let KeepRule::Schedule {
            days,
            from_minute,
            to_minute,
        } = rule
        else {
            continue;
        };
        windows.extend(instances(
            *days,
            *from_minute,
            *to_minute,
            from,
            cutoff,
            zone,
        ));
    }
    if windows.is_empty() {
        return windows;
    }

    // Two rules may overlap — "weekdays 08:00-18:00" and "every day
    // 17:00-19:00" — and uploading the overlap twice would bill twice for the
    // same video.
    windows.sort_by_key(|(start, _)| *start);
    let mut merged: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for (start, end) in windows {
        match merged.last_mut() {
            Some((_, last_end)) if start <= *last_end => {
                if end > *last_end {
                    *last_end = end;
                }
            }
            _ => merged.push((start, end)),
        }
    }

    // Long windows come out as a series of clips rather than one enormous
    // one. A window that is still running keeps its remainder back: cutting
    // whatever has accumulated on every pass would turn a morning into a
    // hundred clips the length of the poll interval.
    let mut chunks = Vec::new();
    for (start, end) in merged {
        let still_running = end >= cutoff;
        let mut at = start;
        while at + cutting.max_clip <= end {
            chunks.push((at, at + cutting.max_clip));
            at += cutting.max_clip;
        }
        if at < end && !still_running {
            chunks.push((at, end));
        }
    }
    chunks
}

/// Every occurrence of one weekly window that overlaps `[from, to)`.
fn instances<Z: TimeZone>(
    days: u8,
    from_minute: u16,
    to_minute: u16,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    zone: &Z,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let length = window_length(from_minute, to_minute);
    if length == 0 {
        return Vec::new();
    }
    let mut found = Vec::new();
    let local_from = from.with_timezone(zone);
    // Start a day early: a window that began yesterday evening may still be
    // running now.
    let mut day = local_from.date_naive() - chrono::Days::new(1);
    let last_day = to.with_timezone(zone).date_naive();
    while day <= last_day {
        if wanted_day(days, day.weekday().num_days_from_monday() as u8) {
            let midnight = day.and_hms_opt(0, 0, 0).expect("midnight exists");
            // Both ends are wall-clock times, worked out on the clock on the
            // wall and only then turned into instants. Adding the minutes to
            // an instant instead is the bug this file's opening line warns
            // about: on the Sunday the clocks go back, "09:00 to 17:00"
            // quietly becomes 08:00 to 16:00, and the last hour of the day
            // the operator asked for is never kept.
            let local_start = midnight + Duration::minutes(i64::from(from_minute));
            let local_end = local_start + Duration::minutes(i64::from(length));
            if let (Some(start), Some(end)) = (resolve(zone, local_start), resolve(zone, local_end))
            {
                let clipped = (start.max(from), end.min(to));
                if clipped.0 < clipped.1 {
                    found.push(clipped);
                }
            }
        }
        day = day.succ_opt().expect("a next day exists");
    }
    found
}

/// One wall-clock time as an instant.
///
/// The hour that happens twice takes the earlier of the two: the first 02:30
/// is the one somebody means by 02:30. The hour that never happens takes the
/// first instant that does, because a window that starts at 02:30 on that
/// morning starts as soon as 02:30 would have — and a window that simply
/// vanished on the day the clocks went forward would be a morning nobody
/// kept.
fn resolve<Z: TimeZone>(zone: &Z, local: chrono::NaiveDateTime) -> Option<DateTime<Utc>> {
    match zone.from_local_datetime(&local) {
        chrono::LocalResult::Single(at) => Some(at.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
        // No zone skips more than a couple of hours; walking forward a minute
        // at a time finds the far side of any of them.
        chrono::LocalResult::None => (1..=180).find_map(|minutes| {
            zone.from_local_datetime(&(local + Duration::minutes(minutes)))
                .earliest()
                .map(|at| at.with_timezone(&Utc))
        }),
    }
}

/// How long the window runs, in minutes, wrapping past midnight.
fn window_length(from_minute: u16, to_minute: u16) -> u16 {
    if to_minute > from_minute {
        to_minute - from_minute
    } else {
        // Crosses midnight: "22:00 to 06:00" is eight hours.
        1440 - from_minute + to_minute
    }
}

/// Monday is bit 0; no bits set means every day.
fn wanted_day(days: u8, weekday_from_monday: u8) -> bool {
    days == 0 || days & (1 << weekday_from_monday) != 0
}

/// Where a camera's schedule has got to, kept beside its ring so a restart
/// does not upload the same morning twice.
pub fn read_watermark(ring_dir: &std::path::Path) -> Option<DateTime<Utc>> {
    let raw = std::fs::read_to_string(ring_dir.join("kept-until")).ok()?;
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

pub fn write_watermark(ring_dir: &std::path::Path, at: DateTime<Utc>) -> anyhow::Result<()> {
    let path = ring_dir.join("kept-until");
    let temporary = path.with_extension("partial");
    std::fs::write(&temporary, at.to_rfc3339())?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Timelike};
    use chrono_tz::Europe::Madrid;

    /// Windows a rule produces over a stretch, as local wall-clock strings —
    /// which is how the operator wrote the rule and the only way to read
    /// these that shows a DST bug.
    fn local_windows(rule: KeepRule, from: &str, to: &str) -> Vec<(String, String)> {
        instances_for(rule, at(from), at(to))
            .into_iter()
            .map(|(start, end)| {
                (
                    start
                        .with_timezone(&Madrid)
                        .format("%Y-%m-%d %H:%M %Z")
                        .to_string(),
                    end.with_timezone(&Madrid)
                        .format("%Y-%m-%d %H:%M %Z")
                        .to_string(),
                )
            })
            .collect()
    }

    fn instances_for(
        rule: KeepRule,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        let KeepRule::Schedule {
            days,
            from_minute,
            to_minute,
        } = rule
        else {
            panic!("a schedule rule");
        };
        instances(days, from_minute, to_minute, from, to, &Madrid)
    }

    fn schedule(from_minute: u16, to_minute: u16) -> KeepRule {
        KeepRule::Schedule {
            days: 0,
            from_minute,
            to_minute,
        }
    }

    /// The same, on this machine's own time zone database rather than the
    /// copy compiled into `chrono-tz`, through the same `chrono::Local` the
    /// gateway itself uses.
    ///
    /// Ignored because it needs `TZ` set for the whole process, which would
    /// reach the other tests. `make check-dst` runs it on its own.
    #[test]
    #[ignore = "needs TZ set for the process; see make check-dst"]
    fn this_box_keeps_nine_to_five_across_its_own_daylight_saving_change() {
        let zone = chrono::Local::now().timezone();
        let named = std::env::var("TZ").unwrap_or_default();
        assert!(
            named.contains('/'),
            "set TZ to a named zone with a daylight-saving change, e.g. TZ=Europe/Madrid"
        );
        // The Sunday the clocks go back, wherever this box thinks it is.
        let (from, to) = (at("2026-10-24T00:00:00Z"), at("2026-10-26T00:00:00Z"));
        let windows = instances(0, 9 * 60, 17 * 60, from, to, &zone);
        for (start, end) in &windows {
            println!(
                "   {} to {}",
                start.with_timezone(&zone).format("%Y-%m-%d %H:%M %Z"),
                end.with_timezone(&zone).format("%Y-%m-%d %H:%M %Z")
            );
        }
        for (start, end) in &windows {
            let local_start = start.with_timezone(&zone);
            let local_end = end.with_timezone(&zone);
            assert_eq!(
                (local_start.hour(), local_start.minute()),
                (9, 0),
                "a window started at the wrong time on {local_start}"
            );
            assert_eq!(
                (local_end.hour(), local_end.minute()),
                (17, 0),
                "a window ended at the wrong time on {local_end}"
            );
        }
        assert_eq!(windows.len(), 2, "two days, two windows: {windows:?}");
    }

    /// Europe/Madrid, 2026-10-25: 03:00 CEST becomes 02:00 CET and that day
    /// is twenty-five hours long.
    #[test]
    fn a_morning_window_stays_at_nine_on_the_day_the_clocks_go_back() {
        let windows = local_windows(
            schedule(9 * 60, 17 * 60),
            "2026-10-24T00:00:00Z",
            "2026-10-26T00:00:00Z",
        );
        let sunday: Vec<_> = windows
            .iter()
            .filter(|(start, _)| start.starts_with("2026-10-25"))
            .collect();
        assert_eq!(
            sunday,
            vec![&(
                "2026-10-25 09:00 CET".to_string(),
                "2026-10-25 17:00 CET".to_string()
            )],
            "nine to five is nine to five, whatever the clocks did that night: {windows:?}"
        );
    }

    #[test]
    fn a_window_over_the_repeated_hour_is_the_longer_night_it_really_was() {
        // 01:00 CEST to 04:00 CET is four hours on the ground: the hour
        // between 02:00 and 03:00 happens twice.
        let windows = instances_for(
            schedule(60, 4 * 60),
            at("2026-10-24T00:00:00Z"),
            at("2026-10-26T00:00:00Z"),
        );
        let sunday: Vec<_> = windows
            .iter()
            .filter(|(start, _)| start.with_timezone(&Madrid).day() == 25)
            .collect();
        assert_eq!(sunday.len(), 1, "{windows:?}");
        let (start, end) = sunday[0];
        assert_eq!(*start, at("2026-10-24T23:00:00Z"), "01:00 CEST");
        assert_eq!(*end, at("2026-10-25T03:00:00Z"), "04:00 CET");
    }

    #[test]
    fn a_window_over_the_missing_hour_is_the_shorter_night_it_really_was() {
        // Europe/Madrid, 2027-03-28: 02:00 CET becomes 03:00 CEST. 01:00 to
        // 04:00 is two hours on the ground.
        let windows = instances_for(
            schedule(60, 4 * 60),
            at("2027-03-27T00:00:00Z"),
            at("2027-03-29T00:00:00Z"),
        );
        let sunday: Vec<_> = windows
            .iter()
            .filter(|(start, _)| start.with_timezone(&Madrid).day() == 28)
            .collect();
        assert_eq!(sunday.len(), 1, "{windows:?}");
        let (start, end) = sunday[0];
        assert_eq!(*start, at("2027-03-28T00:00:00Z"), "01:00 CET");
        assert_eq!(*end, at("2027-03-28T02:00:00Z"), "04:00 CEST");
    }

    #[test]
    fn a_window_that_starts_in_the_hour_that_never_happened_starts_when_it_can() {
        // 02:30 does not exist on 2027-03-28 in Madrid. The first instant
        // that does is 03:00, and that is when the keep starts.
        let windows = instances_for(
            schedule(2 * 60 + 30, 4 * 60),
            at("2027-03-27T00:00:00Z"),
            at("2027-03-29T00:00:00Z"),
        );
        let sunday: Vec<_> = windows
            .iter()
            .filter(|(start, _)| start.with_timezone(&Madrid).day() == 28)
            .collect();
        assert_eq!(sunday.len(), 1, "{windows:?}");
        assert_eq!(sunday[0].0, at("2027-03-28T01:00:00Z"), "03:00 CEST");
        assert_eq!(sunday[0].1, at("2027-03-28T02:00:00Z"), "04:00 CEST");
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("a timestamp")
            .with_timezone(&Utc)
    }

    fn weekdays_9_to_5() -> KeepRule {
        KeepRule::Schedule {
            // Monday to Friday.
            days: 0b0001_1111,
            from_minute: 9 * 60,
            to_minute: 17 * 60,
        }
    }

    #[test]
    fn a_window_that_has_passed_is_kept_in_clip_sized_pieces() {
        let zone = Utc;
        // 2026-09-23 is a Wednesday.
        let windows = windows_to_keep(
            &[weekdays_9_to_5()],
            at("2026-09-23T09:00:00Z"),
            at("2026-09-23T09:35:00Z"),
            &zone,
            Cutting::default(),
        );
        assert_eq!(windows.len(), 3, "ten-minute clips, minus the lag");
        assert_eq!(windows[0].0, at("2026-09-23T09:00:00Z"));
        assert_eq!(windows[0].1, at("2026-09-23T09:10:00Z"));
        assert_eq!(windows[2].1, at("2026-09-23T09:30:00Z"));
        assert!(
            windows.last().unwrap().1 <= at("2026-09-23T09:34:30Z"),
            "nothing is asked for that the ring has not written yet"
        );
    }

    #[test]
    fn a_day_the_schedule_does_not_name_keeps_nothing() {
        // 2026-09-26 is a Saturday.
        let windows = windows_to_keep(
            &[weekdays_9_to_5()],
            at("2026-09-26T09:00:00Z"),
            at("2026-09-26T17:00:00Z"),
            &Utc,
            Cutting::default(),
        );
        assert!(windows.is_empty(), "{windows:?}");
    }

    #[test]
    fn a_schedule_is_read_in_the_gateway_s_own_time() {
        // The same window, at a site three hours east: 09:00 local is 06:00 UTC.
        let zone = FixedOffset::east_opt(3 * 3600).unwrap();
        let windows = windows_to_keep(
            &[weekdays_9_to_5()],
            at("2026-09-23T06:00:00Z"),
            at("2026-09-23T06:15:00Z"),
            &zone,
            Cutting::default(),
        );
        assert_eq!(
            windows.first().map(|w| w.0),
            Some(at("2026-09-23T06:00:00Z"))
        );
        assert!(
            !windows.is_empty(),
            "the window is open at the site, whatever UTC says"
        );
    }

    #[test]
    fn a_window_across_midnight_is_one_window() {
        let rule = KeepRule::Schedule {
            days: 0,
            from_minute: 22 * 60,
            to_minute: 6 * 60,
        };
        let windows = windows_to_keep(
            &[rule],
            at("2026-09-23T23:50:00Z"),
            at("2026-09-24T00:10:00Z"),
            &Utc,
            Cutting::default(),
        );
        assert!(!windows.is_empty());
        assert_eq!(windows[0].0, at("2026-09-23T23:50:00Z"));
        assert!(
            windows.last().unwrap().1 >= at("2026-09-24T00:00:00Z"),
            "the window does not stop at midnight: {windows:?}"
        );
    }

    #[test]
    fn overlapping_rules_do_not_keep_the_same_video_twice() {
        let evening = KeepRule::Schedule {
            days: 0,
            from_minute: 16 * 60,
            to_minute: 19 * 60,
        };
        let windows = windows_to_keep(
            &[weekdays_9_to_5(), evening],
            at("2026-09-23T16:00:00Z"),
            at("2026-09-23T16:25:00Z"),
            &Utc,
            Cutting::default(),
        );
        let total: i64 = windows.iter().map(|(a, b)| (*b - *a).num_seconds()).sum();
        // Two full clips. The rest of the window is still running, so its
        // remainder waits rather than going out as a scrap.
        assert_eq!(total, 1_200, "the overlap was uploaded twice: {windows:?}");
        assert!(
            windows.windows(2).all(|pair| pair[0].1 <= pair[1].0),
            "clips overlap each other: {windows:?}"
        );
    }

    #[test]
    fn a_window_that_is_still_running_keeps_its_remainder_back() {
        // Cutting whatever has piled up on every pass would turn a morning
        // into a hundred clips the length of the poll interval.
        let windows = windows_to_keep(
            &[weekdays_9_to_5()],
            at("2026-09-23T09:00:00Z"),
            at("2026-09-23T09:04:00Z"),
            &Utc,
            Cutting::default(),
        );
        assert!(windows.is_empty(), "nothing whole yet: {windows:?}");

        // Once the window closes, the tail goes out with everything else.
        let windows = windows_to_keep(
            &[weekdays_9_to_5()],
            at("2026-09-23T16:55:00Z"),
            at("2026-09-23T17:30:00Z"),
            &Utc,
            Cutting::default(),
        );
        assert_eq!(
            windows.last().map(|window| window.1),
            Some(at("2026-09-23T17:00:00Z")),
            "the window ends where the schedule says: {windows:?}"
        );
    }

    #[test]
    fn a_gateway_that_was_off_for_a_week_does_not_upload_the_week() {
        // It could not anyway: the ring is hours deep. Trying is how a morning
        // is spent uploading last Tuesday.
        let windows = windows_to_keep(
            &[KeepRule::Schedule {
                days: 0,
                from_minute: 0,
                to_minute: 1439,
            }],
            at("2026-09-16T09:00:00Z"),
            at("2026-09-23T09:00:00Z"),
            &Utc,
            Cutting::default(),
        );
        let earliest = windows.first().expect("something is kept").0;
        assert!(
            earliest >= at("2026-09-23T02:00:00Z"),
            "reached back to {earliest}"
        );
    }

    #[test]
    fn rules_that_are_not_schedules_are_left_to_whoever_handles_them() {
        let windows = windows_to_keep(
            &[KeepRule::OnIncident {
                pre_roll_seconds: 30,
                post_roll_seconds: 30,
            }],
            at("2026-09-23T09:00:00Z"),
            at("2026-09-23T10:00:00Z"),
            &Utc,
            Cutting::default(),
        );
        assert!(windows.is_empty());
    }

    #[test]
    fn the_watermark_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_watermark(dir.path()).is_none(), "nothing kept yet");
        let moment = at("2026-09-23T09:30:00Z");
        write_watermark(dir.path(), moment).unwrap();
        assert_eq!(read_watermark(dir.path()), Some(moment));
    }
}
