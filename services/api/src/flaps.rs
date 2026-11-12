//! A camera that keeps going up and down sends one message out, and one
//! when it has settled. See
//! docs/superpowers/specs/2026-11-10-flapping-camera-design.md.
//!
//! This is not a rule engine. It is one rule, for the one case that makes
//! people mute alerts, and it lives where the events are raised.

use chrono::{DateTime, Duration, Utc};

use crate::store::{CameraFlap, Store, StoreError};

/// How many transitions in how long make a flap, and how long one state has
/// to hold before it is over. Read from the environment next to
/// `STALE_CAMERA_SECONDS`.
#[derive(Debug, Clone, PartialEq)]
pub struct FlapSettings {
    /// Transitions inside the window that make a flap. Zero or less turns
    /// the whole thing off: every transition goes out as itself.
    pub transitions: i64,
    pub window: Duration,
    /// One state held this long ends the flap.
    pub settle: Duration,
}

impl Default for FlapSettings {
    fn default() -> Self {
        Self {
            transitions: 3,
            window: Duration::seconds(600),
            settle: Duration::seconds(600),
        }
    }
}

impl FlapSettings {
    /// `FLAP_TRANSITIONS`, `FLAP_WINDOW_SECONDS`, `FLAP_SETTLE_SECONDS`; a
    /// value that does not parse is the default, as every other setting here.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        let seconds = |name: &str, default: Duration| {
            std::env::var(name)
                .ok()
                .and_then(|raw| raw.parse::<i64>().ok())
                .map(Duration::seconds)
                .unwrap_or(default)
        };
        Self {
            transitions: std::env::var("FLAP_TRANSITIONS")
                .ok()
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(defaults.transitions),
            window: seconds("FLAP_WINDOW_SECONDS", defaults.window),
            settle: seconds("FLAP_SETTLE_SECONDS", defaults.settle),
        }
    }

    pub fn enabled(&self) -> bool {
        self.transitions > 0
    }

    pub fn window_minutes(&self) -> i64 {
        self.window.num_minutes()
    }
}

/// What to do with the transition that was just recorded.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Not flapping: the transition goes out as itself.
    Deliver,
    /// Flapping already: record it, deliver nothing.
    Held,
    /// This transition crossed the threshold: hold it, and send
    /// `camera_flapping` in its place.
    Started { changes: i64 },
}

/// Called after the transition's incident has been opened or closed, so the
/// count includes it. Writes the flap row itself: a bump for `Held`, a start
/// for `Started`.
pub async fn verdict(
    store: &dyn Store,
    settings: &FlapSettings,
    camera_id: &str,
    now: DateTime<Utc>,
) -> Result<Verdict, StoreError> {
    if !settings.enabled() {
        return Ok(Verdict::Deliver);
    }
    let flap = store.camera_flap(camera_id).await?;
    if let Some(flap) = &flap
        && flap.settled_at.is_none()
    {
        store.bump_camera_flap(camera_id, now).await?;
        return Ok(Verdict::Held);
    }
    // Transitions that were already reported as part of a flap that ended
    // do not count toward the next one; otherwise a settle window shorter
    // than the flap window would re-open the flap on the first transition
    // after "settled", on the strength of transitions the channel has
    // already heard about.
    let mut since = now - settings.window;
    if let Some(settled_at) = flap.and_then(|flap| flap.settled_at)
        && settled_at > since
    {
        since = settled_at;
    }
    let changes = store.transitions_since(camera_id, since).await?;
    if changes >= settings.transitions {
        store.start_camera_flap(camera_id, now, changes).await?;
        return Ok(Verdict::Started { changes });
    }
    Ok(Verdict::Deliver)
}

/// An open flap whose camera has held one state for the settle window.
pub fn has_settled(flap: &CameraFlap, settings: &FlapSettings, now: DateTime<Utc>) -> bool {
    flap.settled_at.is_none() && now - flap.last_transition_at >= settings.settle
}

pub fn flapping_detail(settings: &FlapSettings, changes: i64) -> String {
    format!(
        "{changes} changes in {} minutes; single changes are held until it settles",
        settings.window_minutes()
    )
}

/// "7 changes; quiet since 21:52 UTC" — or "down since", which is the same
/// instant: the last transition is when it last went down, by definition.
/// Both figures are measured. The spec's draft said "over 38 minutes", and
/// that number could only be a convention (the row does not know when the
/// first counted change was), which a reader would have taken for the span
/// of the flapping; this repository does not ship figures like that.
pub fn settled_detail(flap: &CameraFlap, down: bool) -> String {
    format!(
        "{} changes; {} since {} UTC",
        flap.changes,
        if down { "down" } else { "quiet" },
        flap.last_transition_at.format("%H:%M")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SqliteStore, Store};
    use chrono::{Duration, Utc};

    fn settings(transitions: i64, window: i64, settle: i64) -> FlapSettings {
        FlapSettings {
            transitions,
            window: Duration::seconds(window),
            settle: Duration::seconds(settle),
        }
    }

    /// down -9m, up -5m, down now: three transitions in a 10-minute window.
    async fn three_transitions(store: &SqliteStore, now: chrono::DateTime<Utc>) {
        store
            .open_incident("cam-1", now - Duration::minutes(9), None)
            .await
            .unwrap();
        store
            .close_incident("cam-1", now - Duration::minutes(5))
            .await
            .unwrap();
        store.open_incident("cam-1", now, None).await.unwrap();
    }

    #[tokio::test]
    async fn the_transition_that_crosses_the_threshold_starts_a_flap_and_the_next_is_held() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        three_transitions(&store, now).await;
        assert_eq!(
            verdict(&store, &FlapSettings::default(), "cam-1", now)
                .await
                .unwrap(),
            Verdict::Started { changes: 3 }
        );
        let flap = store.camera_flap("cam-1").await.unwrap().unwrap();
        assert_eq!((flap.changes, flap.settled_at), (3, None));
        assert_eq!(flap.started_at.timestamp(), now.timestamp());

        let later = now + Duration::minutes(1);
        store.close_incident("cam-1", later).await.unwrap();
        assert_eq!(
            verdict(&store, &FlapSettings::default(), "cam-1", later)
                .await
                .unwrap(),
            Verdict::Held
        );
        let flap = store.camera_flap("cam-1").await.unwrap().unwrap();
        assert_eq!(flap.changes, 4);
        assert_eq!(flap.last_transition_at.timestamp(), later.timestamp());
    }

    #[tokio::test]
    async fn two_transitions_in_the_window_deliver_as_themselves() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .open_incident("cam-1", now - Duration::minutes(5), None)
            .await
            .unwrap();
        store.close_incident("cam-1", now).await.unwrap();
        assert_eq!(
            verdict(&store, &FlapSettings::default(), "cam-1", now)
                .await
                .unwrap(),
            Verdict::Deliver
        );
        assert_eq!(store.camera_flap("cam-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn transitions_before_the_last_settle_do_not_count() {
        // Three transitions made a flap that settled five minutes later.
        // One more transition a minute after that, with a 10-minute window:
        // the old three were already reported as a flap, so this one goes
        // out as itself rather than re-opening the flap by itself.
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        three_transitions(&store, now).await;
        let settings = settings(3, 600, 300);
        assert_eq!(
            verdict(&store, &settings, "cam-1", now).await.unwrap(),
            Verdict::Started { changes: 3 }
        );
        store
            .settle_camera_flap("cam-1", now + Duration::minutes(5))
            .await
            .unwrap();
        let later = now + Duration::minutes(6);
        store.close_incident("cam-1", later).await.unwrap();
        assert_eq!(
            verdict(&store, &settings, "cam-1", later).await.unwrap(),
            Verdict::Deliver
        );
        assert!(
            store
                .camera_flap("cam-1")
                .await
                .unwrap()
                .unwrap()
                .settled_at
                .is_some(),
            "the settled row stays settled"
        );
    }

    #[tokio::test]
    async fn zero_or_negative_transitions_disables_holding() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        for minutes in [8, 6, 4, 2] {
            store
                .open_incident("cam-1", now - Duration::minutes(minutes), None)
                .await
                .unwrap();
            store
                .close_incident("cam-1", now - Duration::minutes(minutes - 1))
                .await
                .unwrap();
        }
        store.open_incident("cam-1", now, None).await.unwrap();
        for transitions in [0, -1] {
            assert_eq!(
                verdict(&store, &settings(transitions, 600, 600), "cam-1", now)
                    .await
                    .unwrap(),
                Verdict::Deliver
            );
        }
        assert_eq!(store.camera_flap("cam-1").await.unwrap(), None);
    }

    #[test]
    fn has_settled_is_inclusive_at_the_settle_window() {
        let now = Utc::now();
        let flap = crate::store::CameraFlap {
            camera_id: "cam-1".into(),
            started_at: now - Duration::minutes(20),
            changes: 3,
            last_transition_at: now - Duration::seconds(600),
            settled_at: None,
        };
        let settings = FlapSettings::default();
        assert!(has_settled(&flap, &settings, now));
        assert!(!has_settled(&flap, &settings, now - Duration::seconds(1)));
        let settled = crate::store::CameraFlap {
            settled_at: Some(now),
            ..flap
        };
        assert!(!has_settled(&settled, &settings, now + Duration::hours(1)));
    }

    #[test]
    fn the_details_read_as_the_spec_wrote_them() {
        assert_eq!(
            flapping_detail(&FlapSettings::default(), 3),
            "3 changes in 10 minutes; single changes are held until it settles"
        );
        let t = Utc::now();
        let last = t + Duration::minutes(28);
        let flap = crate::store::CameraFlap {
            camera_id: "cam-1".into(),
            started_at: t,
            changes: 7,
            last_transition_at: last,
            settled_at: None,
        };
        // Both figures are measured, not derived: the count, and the time of
        // the last change. "Over N minutes" was a convention a reader would
        // have taken for the span of the flapping, and this repository does
        // not ship numbers like that.
        assert_eq!(
            settled_detail(&flap, false),
            format!("7 changes; quiet since {} UTC", last.format("%H:%M"))
        );
        assert_eq!(
            settled_detail(&flap, true),
            format!("7 changes; down since {} UTC", last.format("%H:%M"))
        );
    }

    #[test]
    fn the_defaults_are_three_in_ten_minutes_and_ten_quiet_minutes() {
        let defaults = FlapSettings::default();
        assert_eq!(defaults.transitions, 3);
        assert_eq!(defaults.window, Duration::seconds(600));
        assert_eq!(defaults.settle, Duration::seconds(600));
        assert!(defaults.enabled());
        assert_eq!(defaults.window_minutes(), 10);
        assert!(!settings(0, 600, 600).enabled());
    }
}
