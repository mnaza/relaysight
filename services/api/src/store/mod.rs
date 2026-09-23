//! The persistence boundary. Handlers speak these methods; SQL lives in the
//! implementations. See docs/superpowers/specs/2026-09-06-persistent-fleet-store-design.md.

mod sqlite;

pub use sqlite::SqliteStore;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use vms_domain::{
    AuditView, CameraTelemetryBatch, EnrollmentRequest, GatewayEnrollmentRequest, GatewayView,
    IncidentView, RecordingManifest, RecordingPolicy, VideoSource,
};
use vms_plugin_sdk::FleetEvent;

/// An event waiting for one sink.
#[derive(Debug, Clone)]
pub struct DueDelivery {
    pub event: FleetEvent,
    pub plugin_id: String,
    pub attempts: i64,
}

/// What the dashboard shows: an event and how each sink got on with it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventView {
    #[serde(flatten)]
    pub event: FleetEvent,
    pub deliveries: Vec<DeliveryView>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeliveryView {
    pub plugin_id: String,
    pub attempts: i64,
    pub delivered_at: Option<DateTime<Utc>>,
    pub declined: bool,
    pub last_error: Option<String>,
    /// `None` with no delivery means it was given up on.
    pub next_attempt_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct CameraRecord {
    pub id: String,
    pub gateway_id: String,
    pub site_id: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    // Stored for every camera and pinned by tests; no handler surfaces it yet.
    #[allow(dead_code)]
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SiteRecord {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub city: String,
    pub cameras: Vec<CameraRecord>,
}

#[derive(Debug, Clone)]
pub struct OrganizationRecord {
    pub id: String,
    pub name: String,
    pub sites: Vec<SiteRecord>,
}

/// SHA-256 of a token, lowercase hex. What the database sees instead of tokens.
pub fn token_hash(token: &str) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn create_enrollment(
        &self,
        token: &str,
        request: &EnrollmentRequest,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Validity check without burning the token (entitlement resolution sits
    /// between look and claim, and an entitlement outage must not burn it).
    async fn enrollment_request(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError>;

    /// Atomic: exactly one caller ever gets Ok for a given token.
    async fn claim_enrollment(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError>;

    /// Organization + site + gateway in one transaction. Idempotent; a
    /// repeated enrollment for the same gateway rotates its token.
    /// A fresh enrollment also clears a revocation — the admin-issued token is the un-revoke.
    async fn enroll_gateway(
        &self,
        request: &EnrollmentRequest,
        enroll: &GatewayEnrollmentRequest,
        gateway_token: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn verify_gateway_token(&self, gateway_id: &str, token: &str)
    -> Result<bool, StoreError>;

    /// True when the token belongs to any enrolled gateway. The plugin
    /// endpoints the edge calls carry no gateway id in the path, so this is
    /// the check they get.
    async fn verify_any_gateway_token(&self, token: &str) -> Result<bool, StoreError>;

    /// Org + site + gateway placeholder + camera rows from one telemetry
    /// batch. Advances `last_seen`, preserves `first_seen`, never touches
    /// `gateways.token_hash`.
    async fn upsert_fleet_identity(
        &self,
        batch: &CameraTelemetryBatch,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// All cameras, ordered by name then id.
    async fn fleet_cameras(&self) -> Result<Vec<CameraRecord>, StoreError>;

    /// Orgs → sites → cameras, all sorted by id.
    async fn fleet_identity(&self) -> Result<Vec<OrganizationRecord>, StoreError>;

    /// Upsert by `recording_id`.
    async fn save_recording(&self, manifest: &RecordingManifest) -> Result<(), StoreError>;

    async fn recording(&self, recording_id: &str) -> Result<RecordingManifest, StoreError>;

    /// Newest `started_at` first.
    async fn camera_recordings(
        &self,
        camera_id: &str,
    ) -> Result<Vec<RecordingManifest>, StoreError>;

    /// `delete_after` set and `<= now`.
    async fn expired_recordings(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<RecordingManifest>, StoreError>;

    /// Deleting an absent row is Ok, not NotFound — the retention loop may
    /// race itself.
    async fn delete_recording(&self, recording_id: &str) -> Result<(), StoreError>;

    /// The admin credential as an argon2id PHC string, if one has been seeded.
    async fn admin_password_hash(&self) -> Result<Option<String>, StoreError>;

    /// Insert or replace the single credential row.
    async fn set_admin_password_hash(
        &self,
        phc: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn create_session(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// True only for a stored, unexpired session.
    async fn session_is_valid(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError>;

    /// Deleting an absent session is Ok — logging out twice is not an error.
    async fn delete_session(&self, session_id: &str) -> Result<(), StoreError>;

    /// Password change and forced reset revoke everything at once.
    async fn delete_all_sessions(&self) -> Result<(), StoreError>;

    /// Called from the retention loop.
    async fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// Open a disconnect incident. Idempotent: at most one open incident per
    /// camera, enforced by the database. `true` when this call is what opened
    /// it — the difference between an outage starting and an outage being
    /// reported again.
    async fn open_incident(
        &self,
        camera_id: &str,
        started_at: DateTime<Utc>,
        detail: Option<&str>,
    ) -> Result<bool, StoreError>;

    /// Close the camera's open incident, if any. Closing nothing is Ok — the
    /// reconciler closes unconditionally — and `true` means this call is the
    /// one that closed it.
    async fn close_incident(
        &self,
        camera_id: &str,
        ended_at: DateTime<Utc>,
    ) -> Result<bool, StoreError>;

    /// Open incidents first (newest-started first), then closed ones
    /// newest-first — an open incident can never be paged out by the limit.
    async fn incidents(&self, limit: i64) -> Result<Vec<IncidentView>, StoreError>;

    /// Prune closed incidents that ended before the cutoff. Open incidents
    /// are never pruned.
    async fn delete_closed_incidents_before(&self, cutoff: DateTime<Utc>)
    -> Result<(), StoreError>;

    /// Set the tombstone and clear the token: every bearer credential for
    /// this id is refused from now on, the bootstrap secret included.
    /// NotFound when the id has never been in the roster.
    async fn revoke_gateway(&self, gateway_id: &str, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// False for unknown ids.
    async fn gateway_revoked(&self, gateway_id: &str) -> Result<bool, StoreError>;

    /// Whether the roster has this gateway at all. `gateway_revoked` answers
    /// false for an id it has never seen, which is the right answer there and
    /// the wrong one for anything that must tell a typo from a live gateway.
    async fn gateway_exists(&self, gateway_id: &str) -> Result<bool, StoreError>;

    /// Take a gateway's cameras out of the roster, returning the ids stamped.
    /// Already-retired cameras are left alone, so a second call returns none.
    /// A camera comes back on its own when a gateway reports it again.
    async fn retire_gateway_cameras(
        &self,
        gateway_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, StoreError>;

    /// The store's half of the gateways screen: every known gateway with
    /// joined names and flags. `online` and `heartbeat` are the handler's to
    /// fill — the store never claims liveness.
    async fn gateway_views(&self) -> Result<Vec<GatewayView>, StoreError>;

    /// Add a source. Its gateway must exist.
    async fn add_video_source(&self, source: &VideoSource) -> Result<(), StoreError>;

    /// Every source, for the dashboard.
    async fn video_sources(&self) -> Result<Vec<VideoSource>, StoreError>;

    /// One gateway's sources, in the order they were added — what its poll answers.
    async fn gateway_video_sources(&self, gateway_id: &str)
    -> Result<Vec<VideoSource>, StoreError>;

    /// `NotFound` when there is nothing to remove.
    async fn delete_video_source(&self, id: &str) -> Result<(), StoreError>;

    /// Set how a camera is recorded, replacing whatever it had.
    async fn set_recording_policy(&self, policy: &RecordingPolicy) -> Result<(), StoreError>;

    /// One camera's policy, or `None` while it has never been given one —
    /// which means off.
    async fn recording_policy(
        &self,
        camera_id: &str,
    ) -> Result<Option<RecordingPolicy>, StoreError>;

    /// One gateway's policies: what its poll answers.
    async fn gateway_recording_policies(
        &self,
        gateway_id: &str,
    ) -> Result<Vec<RecordingPolicy>, StoreError>;

    /// Write an event down and queue it for each sink named. Writing it
    /// first is the point: a sink that is restarting must not be able to lose
    /// it.
    async fn record_event(
        &self,
        event: &FleetEvent,
        sinks: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Deliveries that are due, oldest event first, with the event to send.
    async fn due_deliveries(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<DueDelivery>, StoreError>;

    /// A sink took it, or decided it was not for it. Either way it is done.
    async fn delivery_succeeded(
        &self,
        event_id: &str,
        plugin_id: &str,
        declined: bool,
        at: DateTime<Utc>,
        detail: Option<&str>,
    ) -> Result<(), StoreError>;

    /// A sink did not take it. `next_attempt_at` of `None` means giving up,
    /// and the reason stays on the row where the dashboard can show it.
    async fn delivery_failed(
        &self,
        event_id: &str,
        plugin_id: &str,
        error: &str,
        next_attempt_at: Option<DateTime<Utc>>,
    ) -> Result<(), StoreError>;

    /// The recent events with how each sink got on, newest first.
    async fn recent_events(&self, limit: i64) -> Result<Vec<EventView>, StoreError>;

    /// Drop events older than the cutoff, deliveries and all.
    async fn delete_events_before(&self, cutoff: DateTime<Utc>) -> Result<(), StoreError>;

    /// One audit row. Callers treat failure as loggable, never fatal.
    async fn record_audit(
        &self,
        at: DateTime<Utc>,
        actor: &str,
        action: &str,
        subject: &str,
        detail: Option<&str>,
    ) -> Result<(), StoreError>;

    /// Newest first.
    async fn audit_entries(&self, limit: i64) -> Result<Vec<AuditView>, StoreError>;

    /// Pruning, only ever called with AUDIT_RETENTION_DAYS > 0.
    async fn delete_audit_before(&self, cutoff: DateTime<Utc>) -> Result<(), StoreError>;
}

#[derive(Debug)]
pub enum StoreError {
    /// The row does not exist.
    NotFound,
    /// The row exists but is no longer usable (claimed or expired enrollment).
    Gone,
    Internal(anyhow::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NotFound => write!(f, "not found"),
            StoreError::Gone => write!(f, "gone"),
            StoreError::Internal(err) => write!(f, "store error: {err}"),
        }
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        StoreError::Internal(err.into())
    }
}

/// Fixed-width RFC 3339 with microseconds and a `Z` suffix, so that SQL string
/// comparison of two stored timestamps is chronological comparison.
pub(crate) fn ts(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub(crate) fn parse_ts(value: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| {
            StoreError::Internal(anyhow::anyhow!("bad stored timestamp {value:?}: {err}"))
        })
}
