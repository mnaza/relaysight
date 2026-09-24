//! Turning a stream of telemetry into a history somebody can read.
//!
//! The dashboard shows now. An incident says a camera is down. Neither says
//! it has been down four times this week, which is the question that decides
//! whether somebody drives out to the site.
//!
//! What is counted is the time *between* reports, attributed to the state the
//! camera was in over it. So a gateway reporting every twenty seconds and one
//! reporting every five give the same answer, and a gateway that was itself
//! offline leaves a gap rather than inventing uptime it never saw.

use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use vms_domain::{CameraTelemetry, CameraTelemetryBatch, HealthStatus};

/// The longest stretch one report may speak for. A gateway that went quiet
/// for three hours and came back saying "healthy" is not three hours of
/// healthy; it is one interval of healthy and a gap nobody was watching.
pub const MAX_GAP_SECONDS: i64 = 150;

/// One camera's time in one state, inside one hour.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthSample {
    pub camera_id: String,
    /// The start of the hour this belongs to, UTC.
    pub hour: DateTime<Utc>,
    pub status: HealthStatus,
    pub seconds: i64,
    /// Reconnects since the previous report, never negative: a gateway that
    /// restarted counts from zero again and that is not minus forty.
    pub reconnects: i64,
    pub fps: Option<f32>,
    pub bitrate_kbps: Option<u32>,
    pub packet_loss: u64,
}

/// What to fold in, given the batch before this one from the same gateway.
///
/// The first batch after a restart produces nothing: with no previous report
/// there is no interval to attribute, and guessing one would be inventing
/// history at exactly the moment the process has none.
pub fn samples(
    previous: Option<&CameraTelemetryBatch>,
    batch: &CameraTelemetryBatch,
) -> Vec<HealthSample> {
    let mut out = Vec::new();
    let Some(previous) = previous else {
        return out;
    };
    for camera in &batch.cameras {
        let Some(before) = previous
            .cameras
            .iter()
            .find(|other| other.camera_id == camera.camera_id)
        else {
            // A camera that has just appeared has no interval behind it.
            continue;
        };
        let from = before.last_seen.min(camera.last_seen);
        let to = camera.last_seen;
        let gap = (to - from).num_seconds();
        if gap <= 0 {
            // The same report again, or a clock that went backwards. Either
            // way there is no new time to attribute.
            continue;
        }
        let counted = gap.min(MAX_GAP_SECONDS);
        let from = to - TimeDelta::seconds(counted);
        let reconnects = i64::from(camera.reconnects).saturating_sub(i64::from(before.reconnects));
        out.extend(split_by_hour(camera, from, to, reconnects.max(0)));
    }
    out
}

/// One interval, cut at the hour boundaries it crosses.
fn split_by_hour(
    camera: &CameraTelemetry,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    reconnects: i64,
) -> Vec<HealthSample> {
    let mut samples = Vec::new();
    let mut at = from;
    // Reconnects belong to the hour the report landed in: they happened
    // somewhere in the interval and splitting them would invent precision.
    let reconnect_hour = hour_of(to);
    while at < to {
        let hour = hour_of(at);
        let next = (hour + TimeDelta::hours(1)).min(to);
        samples.push(HealthSample {
            camera_id: camera.camera_id.clone(),
            hour,
            status: camera.status.clone(),
            seconds: (next - at).num_seconds().max(0),
            reconnects: if hour == reconnect_hour {
                reconnects
            } else {
                0
            },
            fps: camera.fps,
            bitrate_kbps: camera.bitrate_kbps,
            packet_loss: camera.packet_loss,
        });
        at = next;
    }
    samples
}

fn hour_of(at: DateTime<Utc>) -> DateTime<Utc> {
    at.duration_trunc(TimeDelta::hours(1)).unwrap_or(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("a timestamp")
    }

    fn camera(id: &str, status: HealthStatus, last_seen: DateTime<Utc>) -> CameraTelemetry {
        CameraTelemetry {
            camera_id: id.into(),
            gateway_id: "gw-1".into(),
            site_id: "site-1".into(),
            name: id.into(),
            status,
            manufacturer: None,
            model: None,
            firmware: None,
            profile_name: None,
            codec: None,
            width: None,
            height: None,
            fps: Some(25.0),
            bitrate_kbps: Some(2_000),
            packet_loss: 0,
            reconnects: 0,
            rtsp_endpoint: None,
            last_seen,
            last_error: None,
        }
    }

    fn batch(cameras: Vec<CameraTelemetry>) -> CameraTelemetryBatch {
        CameraTelemetryBatch {
            gateway_id: "gw-1".into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Town".into(),
            sent_at: Utc::now(),
            cameras,
        }
    }

    #[test]
    fn the_first_batch_invents_no_history() {
        let now = batch(vec![camera("cam-1", HealthStatus::Healthy, at(0))]);
        assert!(samples(None, &now).is_empty());
    }

    #[test]
    fn the_time_between_reports_is_what_is_counted() {
        let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(0))]);
        let now = batch(vec![camera("cam-1", HealthStatus::Healthy, at(20))]);
        let folded = samples(Some(&before), &now);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].seconds, 20);
        assert_eq!(folded[0].status, HealthStatus::Healthy);
        assert_eq!(folded[0].camera_id, "cam-1");
    }

    #[test]
    fn how_often_a_gateway_reports_does_not_change_the_answer() {
        // Twenty seconds of healthy is twenty seconds of healthy, whether it
        // arrived as one report or four.
        let mut total_fast = 0;
        for step in 0..4 {
            let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(step * 5))]);
            let now = batch(vec![camera(
                "cam-1",
                HealthStatus::Healthy,
                at(step * 5 + 5),
            )]);
            total_fast += samples(Some(&before), &now)[0].seconds;
        }
        let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(0))]);
        let now = batch(vec![camera("cam-1", HealthStatus::Healthy, at(20))]);
        assert_eq!(total_fast, samples(Some(&before), &now)[0].seconds);
    }

    #[test]
    fn a_gateway_that_was_gone_for_hours_does_not_bring_back_hours_of_uptime() {
        let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(0))]);
        let now = batch(vec![camera("cam-1", HealthStatus::Healthy, at(10_800))]);
        let folded = samples(Some(&before), &now);
        let counted: i64 = folded.iter().map(|sample| sample.seconds).sum();
        assert_eq!(
            counted, MAX_GAP_SECONDS,
            "a gap is not uptime, whatever the camera says on its way back"
        );
    }

    #[test]
    fn an_interval_across_an_hour_lands_in_both_hours() {
        // 1_800_000_000 lands exactly on an hour, so 3_590 to 3_610 seconds
        // later straddles the next one.
        let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(3_590))]);
        let now = batch(vec![camera("cam-1", HealthStatus::Healthy, at(3_610))]);
        let folded = samples(Some(&before), &now);
        assert_eq!(folded.len(), 2, "{folded:?}");
        assert_eq!(folded[0].seconds + folded[1].seconds, 20);
        assert_eq!(folded[1].hour - folded[0].hour, TimeDelta::hours(1));
        assert!(
            folded
                .iter()
                .all(|sample| sample.hour.timestamp() % 3600 == 0)
        );
    }

    #[test]
    fn an_outage_is_counted_as_the_outage_it_was() {
        let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(0))]);
        let now = batch(vec![camera("cam-1", HealthStatus::Offline, at(30))]);
        let folded = samples(Some(&before), &now);
        assert_eq!(folded[0].status, HealthStatus::Offline);
        assert_eq!(folded[0].seconds, 30);
    }

    #[test]
    fn a_gateway_that_restarted_does_not_report_minus_forty_reconnects() {
        let mut old = camera("cam-1", HealthStatus::Healthy, at(0));
        old.reconnects = 40;
        let mut new = camera("cam-1", HealthStatus::Healthy, at(20));
        new.reconnects = 0;
        let folded = samples(Some(&batch(vec![old])), &batch(vec![new]));
        assert_eq!(folded[0].reconnects, 0);
    }

    #[test]
    fn reconnects_between_two_reports_are_counted_once() {
        let mut old = camera("cam-1", HealthStatus::Healthy, at(0));
        old.reconnects = 3;
        let mut new = camera("cam-1", HealthStatus::Healthy, at(3_610));
        new.reconnects = 5;
        let before = batch(vec![old]);
        let folded = samples(Some(&before), &batch(vec![new]));
        let total: i64 = folded.iter().map(|sample| sample.reconnects).sum();
        assert_eq!(total, 2, "two reconnects, however many hours it spans");
    }

    #[test]
    fn a_camera_that_has_just_appeared_waits_for_its_second_report() {
        let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(0))]);
        let now = batch(vec![
            camera("cam-1", HealthStatus::Healthy, at(20)),
            camera("cam-2", HealthStatus::Healthy, at(20)),
        ]);
        let folded = samples(Some(&before), &now);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].camera_id, "cam-1");
    }

    #[test]
    fn a_repeated_report_adds_nothing() {
        let before = batch(vec![camera("cam-1", HealthStatus::Healthy, at(20))]);
        let now = batch(vec![camera("cam-1", HealthStatus::Healthy, at(20))]);
        assert!(samples(Some(&before), &now).is_empty());
        // And a clock that went backwards is not negative uptime.
        let back = batch(vec![camera("cam-1", HealthStatus::Healthy, at(10))]);
        assert!(samples(Some(&before), &back).is_empty());
    }
}
