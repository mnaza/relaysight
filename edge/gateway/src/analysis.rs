//! Looking at a camera every so often, and keeping what it saw.
//!
//! A policy can say "ask this plugin every thirty seconds, and when it is sure
//! enough about something, keep the video around that moment". The gateway has
//! no analysis of its own and is not getting any: what it has is a snapshot it
//! can take and a plugin somebody else wrote.
//!
//! The pacing and the threshold live here, with no network in them, because
//! the failure that matters is calling a paid plugin far more often than the
//! policy said.
//! See docs/superpowers/specs/2026-09-23-recording-policies-design.md.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use vms_domain::AiDetectionResult;

/// When each camera was last looked at.
#[derive(Default)]
pub struct Pacing {
    last: HashMap<String, DateTime<Utc>>,
}

impl Pacing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this camera is due, marking it looked at if so. The first call
    /// for a camera is due: a policy that was just turned on should not wait
    /// out an interval before it does anything.
    pub fn due(&mut self, camera_id: &str, every_seconds: u16, now: DateTime<Utc>) -> bool {
        let every = Duration::seconds(i64::from(every_seconds.max(1)));
        match self.last.get(camera_id) {
            Some(last) if now - *last < every => false,
            _ => {
                self.last.insert(camera_id.to_owned(), now);
                true
            }
        }
    }

    pub fn forget_missing(&mut self, present: &HashSet<String>) {
        self.last.retain(|camera_id, _| present.contains(camera_id));
    }
}

/// The detection that crossed the threshold, if any did. Ties count: a policy
/// written as "0.8" means 0.8 is enough.
pub fn crossed(detections: &[AiDetectionResult], threshold: f32) -> Option<&AiDetectionResult> {
    detections
        .iter()
        .filter(|detection| detection.confidence >= threshold)
        .max_by(|a, b| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("a timestamp")
    }

    fn detection(label: &str, confidence: f32) -> AiDetectionResult {
        AiDetectionResult {
            label: label.into(),
            confidence,
            bbox: None,
            attributes: Default::default(),
        }
    }

    #[test]
    fn a_camera_is_looked_at_on_the_interval_and_not_more_often() {
        let mut pacing = Pacing::new();
        assert!(pacing.due("cam-1", 30, at(0)), "the first look is now");
        assert!(!pacing.due("cam-1", 30, at(10)));
        assert!(!pacing.due("cam-1", 30, at(29)));
        assert!(pacing.due("cam-1", 30, at(30)));
        // Another camera keeps its own clock.
        assert!(pacing.due("cam-2", 30, at(31)));
    }

    #[test]
    fn an_interval_of_zero_does_not_call_a_plugin_in_a_loop() {
        // A policy the API should have refused, arriving from an older
        // control plane. Once a second is still wrong, but it is not a bill.
        let mut pacing = Pacing::new();
        assert!(pacing.due("cam-1", 0, at(0)));
        assert!(!pacing.due("cam-1", 0, at(0)));
        assert!(pacing.due("cam-1", 0, at(1)));
    }

    #[test]
    fn a_camera_nobody_reports_is_forgotten() {
        let mut pacing = Pacing::new();
        pacing.due("cam-1", 30, at(0));
        pacing.forget_missing(&HashSet::new());
        assert!(pacing.due("cam-1", 30, at(1)), "as if it were new");
    }

    #[test]
    fn the_strongest_detection_over_the_threshold_is_the_one_that_counts() {
        let detections = [
            detection("cat", 0.4),
            detection("person", 0.91),
            detection("van", 0.77),
        ];
        let crossed = crossed(&detections, 0.75).expect("two crossed");
        assert_eq!(crossed.label, "person");
        assert!(crossed.confidence > 0.9);
    }

    #[test]
    fn nothing_over_the_threshold_keeps_nothing() {
        let detections = [detection("cat", 0.4), detection("leaf", 0.2)];
        assert!(crossed(&detections, 0.75).is_none());
        assert!(crossed(&[], 0.0).is_none(), "no detections at all");
    }

    #[test]
    fn a_threshold_a_detection_exactly_meets_counts() {
        // "0.8" in a policy means 0.8 is enough, not "more than 0.8".
        let detections = [detection("person", 0.8)];
        assert!(crossed(&detections, 0.8).is_some());
    }
}
