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
