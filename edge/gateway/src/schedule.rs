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
            // A local midnight that does not exist (clocks going forward in
            // some zones) or happens twice: take the earliest real instant,
            // which is what an operator means by "that day".
            if let Some(start) = zone
                .from_local_datetime(&midnight)
                .earliest()
                .map(|start| start + Duration::minutes(i64::from(from_minute)))
            {
                let start = start.with_timezone(&Utc);
                let end = start + Duration::minutes(i64::from(length));
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
    use chrono::FixedOffset;

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
