//! Keeping the minutes around a camera going quiet.
//!
//! The gateway is the thing that notices: it is dialling the camera, so it
//! knows before the cloud does. What an investigation wants is not the outage
//! — that is a line in a log — but the video from just before it, which is
//! exactly what a ring buffer has and nothing else does.
//!
//! The pre-roll can be kept the moment the camera goes quiet, because that
//! video already exists. The post-roll cannot: it has not happened yet, so it
//! is remembered and kept once it has.
//! See docs/superpowers/specs/2026-09-23-recording-policies-design.md.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use vms_domain::{HealthStatus, KeepRule};

/// A window of a camera's ring to upload, and when it can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keep {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// The earliest moment the ring will have all of it.
    pub ready_at: DateTime<Utc>,
}

/// What this gateway has seen each camera doing, and what it still owes.
#[derive(Default)]
pub struct Incidents {
    seen: HashMap<String, HealthStatus>,
    owed: Vec<(String, Keep)>,
}

impl Incidents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take note of how a camera looks now, and say what should be kept
    /// because of it. Anything that cannot be kept yet is remembered.
    pub fn observe(
        &mut self,
        camera_id: &str,
        status: &HealthStatus,
        rules: &[KeepRule],
        now: DateTime<Utc>,
    ) {
        let previous = self.seen.insert(camera_id.to_owned(), status.clone());
        let Some(previous) = previous else {
            // First sight of this camera. A gateway that has just started has
            // no ring to speak of and no idea whether this is new, so the
            // honest thing is to learn the state and keep nothing.
            return;
        };
        for rule in rules {
            let KeepRule::OnIncident {
                pre_roll_seconds,
                post_roll_seconds,
            } = rule
            else {
                continue;
            };
            let pre = Duration::seconds(i64::from(*pre_roll_seconds));
            let post = Duration::seconds(i64::from(*post_roll_seconds));

            if went_quiet(&previous, status) && pre > Duration::zero() {
                // Already on disk: the only wait is for the segment being
                // written when the camera stopped.
                self.owed.push((
                    camera_id.to_owned(),
                    Keep {
                        from: now - pre,
                        to: now,
                        ready_at: now,
                    },
                ));
            }
            if came_back(&previous, status) && post > Duration::zero() {
                self.owed.push((
                    camera_id.to_owned(),
                    Keep {
                        from: now,
                        to: now + post,
                        // Nothing to cut until the video exists.
                        ready_at: now + post,
                    },
                ));
            }
        }
    }

    /// A keep somebody else decided on — an analysis result, for instance.
    /// It waits in the same queue, because the question is the same one: has
    /// the ring got all of it yet.
    pub fn remember(&mut self, camera_id: &str, keep: Keep) {
        self.owed.push((camera_id.to_owned(), keep));
    }

    /// The windows whose video the ring should now have. They are handed over
    /// once; what the caller does with them is the caller's business.
    pub fn due(&mut self, now: DateTime<Utc>) -> Vec<(String, Keep)> {
        let mut due = Vec::new();
        self.owed.retain(|(camera_id, keep)| {
            if keep.ready_at <= now {
                due.push((camera_id.clone(), keep.clone()));
                false
            } else {
                true
            }
        });
        due
    }

    /// A camera nobody reports any more is not worth remembering.
    pub fn forget_missing(&mut self, present: &std::collections::HashSet<String>) {
        self.seen.retain(|camera_id, _| present.contains(camera_id));
        self.owed
            .retain(|(camera_id, _)| present.contains(camera_id));
    }
}

/// Offline is the only status worth cutting video for. Warning means the
/// camera is answering badly, which is a different conversation.
fn went_quiet(previous: &HealthStatus, now: &HealthStatus) -> bool {
    *previous != HealthStatus::Offline && *now == HealthStatus::Offline
}

fn came_back(previous: &HealthStatus, now: &HealthStatus) -> bool {
    *previous == HealthStatus::Offline && *now != HealthStatus::Offline
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> KeepRule {
        KeepRule::OnIncident {
            pre_roll_seconds: 60,
            post_roll_seconds: 30,
        }
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("a timestamp")
    }

    #[test]
    fn the_minutes_before_a_camera_went_quiet_are_kept() {
        let mut incidents = Incidents::new();
        incidents.observe("cam-1", &HealthStatus::Healthy, &[rule()], at(0));
        assert!(incidents.due(at(0)).is_empty(), "nothing has happened yet");

        incidents.observe("cam-1", &HealthStatus::Offline, &[rule()], at(100));
        let due = incidents.due(at(100));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "cam-1");
        assert_eq!(due[0].1.from, at(40), "a minute before the silence");
        assert_eq!(due[0].1.to, at(100));
        assert!(incidents.due(at(100)).is_empty(), "handed over once");
    }

    #[test]
    fn what_happens_after_it_comes_back_waits_until_it_has_happened() {
        let mut incidents = Incidents::new();
        incidents.observe("cam-1", &HealthStatus::Offline, &[rule()], at(0));
        incidents.observe("cam-1", &HealthStatus::Healthy, &[rule()], at(200));

        assert!(
            incidents.due(at(200)).is_empty(),
            "the ring cannot hand over video from the future"
        );
        assert!(incidents.due(at(220)).is_empty(), "still not all of it");
        let due = incidents.due(at(230));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1.from, at(200));
        assert_eq!(due[0].1.to, at(230));
    }

    #[test]
    fn a_camera_that_stays_broken_is_not_cut_again_every_pass() {
        // Backoff reports an offline camera every interval. One incident is
        // one incident, however many times it is reported.
        let mut incidents = Incidents::new();
        incidents.observe("cam-1", &HealthStatus::Healthy, &[rule()], at(0));
        incidents.observe("cam-1", &HealthStatus::Offline, &[rule()], at(10));
        assert_eq!(incidents.due(at(10)).len(), 1);
        for pass in 1..10 {
            incidents.observe(
                "cam-1",
                &HealthStatus::Offline,
                &[rule()],
                at(10 + pass * 10),
            );
            assert!(incidents.due(at(10 + pass * 10)).is_empty(), "pass {pass}");
        }
    }

    #[test]
    fn a_camera_answering_badly_is_not_an_incident() {
        let mut incidents = Incidents::new();
        incidents.observe("cam-1", &HealthStatus::Healthy, &[rule()], at(0));
        incidents.observe("cam-1", &HealthStatus::Warning, &[rule()], at(10));
        assert!(
            incidents.due(at(10)).is_empty(),
            "packet loss is not a camera going dark"
        );
    }

    #[test]
    fn the_first_sight_of_a_camera_keeps_nothing() {
        // A gateway that has just started sees every camera for the first
        // time, and has no ring behind it to cut anything out of anyway.
        let mut incidents = Incidents::new();
        incidents.observe("cam-1", &HealthStatus::Offline, &[rule()], at(0));
        assert!(incidents.due(at(0)).is_empty());
    }

    #[test]
    fn a_policy_without_an_incident_rule_keeps_nothing() {
        let mut incidents = Incidents::new();
        let schedule = KeepRule::Schedule {
            days: 0,
            from_minute: 0,
            to_minute: 60,
        };
        let rules = std::slice::from_ref(&schedule);
        incidents.observe("cam-1", &HealthStatus::Healthy, rules, at(0));
        incidents.observe("cam-1", &HealthStatus::Offline, rules, at(10));
        assert!(incidents.due(at(10)).is_empty());
    }

    #[test]
    fn a_camera_taken_off_the_roster_is_forgotten() {
        let mut incidents = Incidents::new();
        incidents.observe("cam-1", &HealthStatus::Healthy, &[rule()], at(0));
        incidents.observe("cam-1", &HealthStatus::Offline, &[rule()], at(10));
        incidents.forget_missing(&std::collections::HashSet::new());
        assert!(
            incidents.due(at(10)).is_empty(),
            "a camera that is gone owes nothing"
        );
        // And it is not remembered as offline either: if it comes back it is
        // a new camera as far as this is concerned.
        incidents.observe("cam-1", &HealthStatus::Healthy, &[rule()], at(20));
        assert!(incidents.due(at(20)).is_empty());
    }
}
