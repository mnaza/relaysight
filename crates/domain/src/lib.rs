use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Warning,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraSummary {
    pub id: String,
    pub name: String,
    pub site_id: String,
    pub status: HealthStatus,
    pub fps: Option<f32>,
    pub bitrate_kbps: Option<u32>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteSummary {
    pub id: String,
    pub customer_id: String,
    pub name: String,
    pub city: String,
    pub cameras: Vec<CameraSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerSummary {
    pub id: String,
    pub name: String,
    pub sites: Vec<SiteSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub generated_at: DateTime<Utc>,
    pub source: FleetSource,
    pub customers: Vec<CustomerSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetSource {
    Live,
    Demo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayHeartbeat {
    pub gateway_id: String,
    pub site_id: String,
    pub hostname: String,
    pub version: String,
    pub uptime_seconds: u64,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub cameras_seen: u32,
    pub healthy_cameras: u32,
    pub warning_cameras: u32,
    pub offline_cameras: u32,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraTelemetry {
    pub camera_id: String,
    pub gateway_id: String,
    pub site_id: String,
    pub name: String,
    pub status: HealthStatus,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub profile_name: Option<String>,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    pub bitrate_kbps: Option<u32>,
    pub packet_loss: u64,
    pub reconnects: u32,
    pub rtsp_endpoint: Option<String>,
    pub last_seen: DateTime<Utc>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraTelemetryBatch {
    pub gateway_id: String,
    pub customer_id: String,
    pub customer_name: String,
    pub site_id: String,
    pub site_name: String,
    pub city: String,
    pub sent_at: DateTime<Utc>,
    pub cameras: Vec<CameraTelemetry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentRequest {
    pub customer_id: String,
    pub customer_name: String,
    pub site_id: String,
    pub site_name: String,
    pub city: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentCreated {
    pub enrollment_token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayEnrollmentRequest {
    pub enrollment_token: String,
    pub gateway_id: String,
    pub hostname: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayEnrollmentResponse {
    pub gateway_token: String,
    pub entitlement: EditionEntitlement,
    pub customer_id: String,
    pub customer_name: String,
    pub site_id: String,
    pub site_name: String,
    pub city: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditionKind {
    Community,
    Commercial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditionEntitlement {
    pub edition: EditionKind,
    pub plan: String,
    pub self_hosted: bool,
    pub managed: bool,
    /// `None` means unlimited cameras. Hosted commercial/free plans may return a limit.
    pub camera_limit: Option<usize>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayCommandStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayCommandKind {
    Record {
        camera_id: String,
        duration_seconds: u32,
        segment_seconds: u32,
        storage_plugin_id: String,
    },
    Live {
        camera_id: String,
        offer_sdp: String,
        offer_type: String,
        session_seconds: u32,
        ice_servers: Vec<RtcIceServerConfig>,
    },
    Analyze {
        camera_id: String,
        ai_plugin_id: String,
        storage_plugin_id: String,
        tasks: Vec<String>,
    },
    /// Keep what has already happened: the last `seconds` out of the
    /// gateway's ring buffer. Nothing is dialled — the video is already on the
    /// gateway's disk or it is gone.
    SaveClip {
        camera_id: String,
        seconds: u32,
        storage_plugin_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayCommand {
    pub id: String,
    pub gateway_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub kind: GatewayCommandKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingObject {
    pub storage_plugin_id: String,
    pub object_ref: String,
    pub object_key: String,
    pub content_type: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSegment {
    pub id: String,
    pub sequence: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub keyframe: bool,
    pub object: RecordingObject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingManifest {
    pub recording_id: String,
    pub camera_id: String,
    pub gateway_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub init: RecordingObject,
    pub segments: Vec<RecordingSegment>,
    pub delete_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayCommandResult {
    pub command_id: String,
    pub gateway_id: String,
    pub status: GatewayCommandStatus,
    pub completed_at: DateTime<Utc>,
    pub error: Option<String>,
    pub recording: Option<RecordingManifest>,
    pub live: Option<LiveSessionAnswer>,
    pub analysis: Option<AiAnalysisResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayCommandView {
    pub command: GatewayCommand,
    pub status: GatewayCommandStatus,
    pub result: Option<GatewayCommandResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcIceServerConfig {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtcConfigResponse {
    pub ice_servers: Vec<RtcIceServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveSessionRequest {
    pub offer_sdp: String,
    #[serde(default = "default_offer_type")]
    pub offer_type: String,
    #[serde(default = "default_live_session_seconds")]
    pub session_seconds: u32,
}

fn default_offer_type() -> String {
    "offer".into()
}
fn default_live_session_seconds() -> u32 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveSessionAnswer {
    pub session_id: String,
    pub sdp: String,
    pub sdp_type: String,
    pub codec: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiAnalysisRequest {
    pub ai_plugin_id: Option<String>,
    pub storage_plugin_id: Option<String>,
    #[serde(default)]
    pub tasks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiBoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiDetectionResult {
    pub label: String,
    pub confidence: f32,
    pub bbox: Option<AiBoundingBox>,
    #[serde(default)]
    pub attributes: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiAnalysisResult {
    pub camera_id: String,
    pub captured_at: DateTime<Utc>,
    pub ai_plugin_id: String,
    pub model: Option<String>,
    pub detections: Vec<AiDetectionResult>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub snapshot: RecordingObject,
}

/// Whether a camera is being recorded all the time, or only when asked.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    /// The default. Nothing is recorded until a command asks for it, and
    /// nothing can be saved after the fact.
    Off,
    /// The gateway keeps a ring buffer on its own disk, so the minutes before
    /// something happened can still be kept.
    Continuous,
}

impl RecordingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecordingMode::Off => "off",
            RecordingMode::Continuous => "continuous",
        }
    }
}

/// When a stretch of the ring is worth uploading.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeepRule {
    /// A window of the week, in the gateway's own local time. `to` before
    /// `from` means the window crosses midnight.
    Schedule {
        /// Monday is bit 0. 0 means every day.
        days: u8,
        from_minute: u16,
        to_minute: u16,
    },
    /// The source stopped answering: keep what led up to it.
    OnIncident {
        pre_roll_seconds: u16,
        post_roll_seconds: u16,
    },
    /// An AI plugin's result crossed a threshold.
    OnAnalysis {
        plugin_id: String,
        every_seconds: u16,
        threshold: f32,
        pre_roll_seconds: u16,
        post_roll_seconds: u16,
    },
}

/// How one camera is recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordingPolicy {
    pub camera_id: String,
    pub gateway_id: String,
    pub mode: RecordingMode,
    #[serde(default)]
    pub keep: Vec<KeepRule>,
    /// How long a kept clip lives in the cloud. 0 means forever.
    #[serde(default)]
    pub retention_days: u16,
    /// Where a clip this policy keeps should go. The control plane fills it
    /// in: a gateway has no way to know which storage plugin a customer uses.
    #[serde(default)]
    pub storage_plugin_id: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingPolicyRequest {
    pub mode: RecordingMode,
    #[serde(default)]
    pub keep: Vec<KeepRule>,
    #[serde(default)]
    pub retention_days: u16,
}

/// What somebody is allowed to do. Three, not thirty: a permission per
/// endpoint is a matrix nobody maintains and everybody ends up granting in
/// full.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Everything, including users and plugins.
    Owner,
    /// Runs the fleet: gateways, sources, policies, recordings, live.
    Technician,
    /// Reads.
    Viewer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Technician => "technician",
            Role::Viewer => "viewer",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "owner" => Some(Role::Owner),
            "technician" => Some(Role::Technician),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    /// Whoever may change the system itself.
    pub fn is_owner(&self) -> bool {
        matches!(self, Role::Owner)
    }

    /// Whoever may operate the fleet.
    pub fn can_operate(&self) -> bool {
        matches!(self, Role::Owner | Role::Technician)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserView {
    pub id: String,
    pub email: String,
    pub role: Role,
    /// Set for a customer login: this user sees that customer and nothing
    /// else.
    pub customer_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub password: String,
    pub role: Role,
    #[serde(default)]
    pub customer_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub customer_id: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipRequest {
    /// How far back to reach. The ring decides whether it still has it.
    #[serde(default = "default_clip_seconds")]
    pub seconds: u32,
    pub storage_plugin_id: Option<String>,
}

fn default_clip_seconds() -> u32 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingRequest {
    #[serde(default = "default_record_duration")]
    pub duration_seconds: u32,
    #[serde(default = "default_segment_duration")]
    pub segment_seconds: u32,
    pub storage_plugin_id: Option<String>,
}

fn default_record_duration() -> u32 {
    10
}
fn default_segment_duration() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandAccepted {
    pub command_id: String,
    pub status: GatewayCommandStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingTimeline {
    pub camera_id: String,
    pub recordings: Vec<RecordingManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentView {
    pub camera_id: String,
    pub camera_name: String,
    pub site_id: String,
    pub site_name: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub detail: Option<String>,
}

/// How a video source reaches the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// The gateway pulls it: an NVR, an encoder, another VMS.
    Rtsp,
    /// Pushed to the gateway, identified by a stream key.
    Rtmp,
    /// The same, over SRT.
    Srt,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::Rtsp => "rtsp",
            SourceKind::Rtmp => "rtmp",
            SourceKind::Srt => "srt",
        }
    }
}

/// Video the gateway carries that is not a camera it discovered. Never holds a
/// credential: a source's password lives on the gateway, as a camera's does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoSource {
    pub id: String,
    pub gateway_id: String,
    pub name: String,
    pub kind: SourceKind,
    /// A URL for `rtsp`, a stream key for what is pushed.
    pub address: String,
    pub added_at: DateTime<Utc>,
}

/// What the dashboard sends to add one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoSourceRequest {
    pub gateway_id: String,
    pub name: String,
    pub kind: SourceKind,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayView {
    pub gateway_id: String,
    pub site_id: String,
    pub site_name: String,
    pub customer_name: String,
    pub hostname: Option<String>,
    pub version: Option<String>,
    /// Holds a working token right now. Revoked gateways are not enrolled.
    pub enrolled: bool,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub online: bool,
    /// The live report, when the gateway has heartbeated this process.
    pub heartbeat: Option<GatewayHeartbeat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditView {
    pub at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub subject: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackSegment {
    pub id: String,
    pub sequence: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub url: String,
    pub headers: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackManifest {
    pub recording_id: String,
    pub camera_id: String,
    pub codec: String,
    pub mime_type: String,
    pub init_url: String,
    pub init_headers: std::collections::BTreeMap<String, String>,
    pub segments: Vec<PlaybackSegment>,
}
