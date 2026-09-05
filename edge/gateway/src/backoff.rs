//! How long to wait before probing a camera that just failed.
//!
//! The probe loop runs on a fixed interval, which is right while things work and
//! wasteful when they do not. A camera unreachable for fifteen minutes was
//! probed twenty-five times, each attempt waiting out an eight-second timeout,
//! and wrote twenty-five identical lines to the log. None of them could have
//! succeeded and none of them told anybody anything the first one had not.
//!
//! So a camera that fails is probed less often, doubling each time, until it
//! answers again.
//!
//! # What this costs
//!
//! Recovery is noticed later. That is the whole trade and it is why the delay is
//! capped: at the ceiling, a camera that comes back is seen within one cap
//! rather than within one interval. Making the cap large would make the log
//! quiet and the dashboard slow to tell the truth, which is the wrong way round.
//!
//! # What it does not do
//!
//! A camera in backoff still reports. It is not skipped from telemetry, it is
//! skipped from *dialling* — it stays on the dashboard as offline, with the
//! error that put it there, because a camera that vanishes from the fleet while
//! it is broken is the opposite of what an operator needs.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Longest wait between attempts. Fifteen-minute outages were observed on a
/// camera reached over the public internet, so a cap far above this would leave
/// the dashboard wrong for most of a recovery.
pub const DEFAULT_CAP: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
struct Entry {
    failures: u32,
    next_attempt: Instant,
    last_error: String,
}

#[derive(Debug)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    state: HashMap<String, Entry>,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            state: HashMap::new(),
        }
    }

    /// The error to keep reporting, when this camera should not be dialled yet.
    /// `None` means probe it.
    pub fn skip_reason(&self, camera_id: &str, now: Instant) -> Option<String> {
        let entry = self.state.get(camera_id)?;
        (now < entry.next_attempt).then(|| entry.last_error.clone())
    }

    pub fn record_failure(&mut self, camera_id: &str, error: &str, now: Instant) {
        let entry = self.state.entry(camera_id.to_string()).or_insert(Entry {
            failures: 0,
            next_attempt: now,
            last_error: String::new(),
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.last_error = error.to_string();
        entry.next_attempt = now + delay_for(self.base, self.cap, entry.failures);
    }

    pub fn record_success(&mut self, camera_id: &str) {
        self.state.remove(camera_id);
    }

    /// How many consecutive failures this camera has. Zero once it answers.
    pub fn failures(&self, camera_id: &str) -> u32 {
        self.state.get(camera_id).map_or(0, |e| e.failures)
    }
}

/// `base * 2^(failures - 1)`, capped. Saturating rather than wrapping: a camera
/// down for a week should not have its delay overflow back to nothing.
fn delay_for(base: Duration, cap: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let shift = (failures - 1).min(32);
    let factor = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
    base.saturating_mul(factor.min(u32::MAX as u64) as u32)
        .min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Duration {
        Duration::from_secs(30)
    }

    #[test]
    fn the_delay_doubles_and_then_stops_doubling() {
        let cap = Duration::from_secs(300);
        assert_eq!(delay_for(base(), cap, 1), Duration::from_secs(30));
        assert_eq!(delay_for(base(), cap, 2), Duration::from_secs(60));
        assert_eq!(delay_for(base(), cap, 3), Duration::from_secs(120));
        assert_eq!(delay_for(base(), cap, 4), Duration::from_secs(240));
        assert_eq!(delay_for(base(), cap, 5), cap);
        assert_eq!(delay_for(base(), cap, 6), cap);
    }

    #[test]
    fn a_camera_down_for_a_week_does_not_overflow_back_to_no_delay() {
        // 2^600 does not fit anywhere. The arithmetic has to saturate, not wrap,
        // or a long outage ends with the loop hammering the camera again.
        let cap = Duration::from_secs(300);
        assert_eq!(delay_for(base(), cap, 600), cap);
        assert_eq!(delay_for(base(), cap, u32::MAX), cap);
    }

    #[test]
    fn a_camera_that_has_never_failed_is_probed() {
        let b = Backoff::new(base(), DEFAULT_CAP);
        assert_eq!(b.skip_reason("cam", Instant::now()), None);
    }

    #[test]
    fn a_failure_holds_the_camera_off_and_keeps_its_error() {
        let now = Instant::now();
        let mut b = Backoff::new(base(), DEFAULT_CAP);
        b.record_failure("cam", "RTSP DESCRIBE timeout", now);

        assert_eq!(
            b.skip_reason("cam", now + Duration::from_secs(5)).as_deref(),
            Some("RTSP DESCRIBE timeout"),
            "the reason it is offline has to survive the wait, or the dashboard forgets why"
        );
        assert_eq!(b.skip_reason("cam", now + Duration::from_secs(31)), None);
    }

    #[test]
    fn success_clears_the_delay_completely() {
        // Not halved, not decayed. A camera that answers is a working camera,
        // and the next failure should start from one interval again.
        let now = Instant::now();
        let mut b = Backoff::new(base(), DEFAULT_CAP);
        for _ in 0..5 {
            b.record_failure("cam", "down", now);
        }
        assert_eq!(b.failures("cam"), 5);

        b.record_success("cam");
        assert_eq!(b.failures("cam"), 0);
        assert_eq!(b.skip_reason("cam", now), None);

        b.record_failure("cam", "down", now);
        assert_eq!(b.skip_reason("cam", now + Duration::from_secs(31)), None);
    }

    #[test]
    fn one_bad_camera_does_not_delay_a_good_one() {
        // The state is per camera on purpose. A site with one dead camera should
        // not have the other eleven probed at the dead one's pace.
        let now = Instant::now();
        let mut b = Backoff::new(base(), DEFAULT_CAP);
        for _ in 0..6 {
            b.record_failure("broken", "down", now);
        }
        assert!(b.skip_reason("broken", now + Duration::from_secs(120)).is_some());
        assert_eq!(b.skip_reason("working", now + Duration::from_secs(120)), None);
    }
}
