use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    AiAnalyze,
    StorageBlob,
    EventSink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub vendor: Option<String>,
    pub description: Option<String>,
    pub capabilities: Vec<PluginCapability>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginPlacement {
    #[default]
    ControlPlane,
    Edge,
    Either,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRegistration {
    pub endpoint: String,
    #[serde(default)]
    pub placement: PluginPlacement,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Environment variable that contains a bearer token for the plugin.
    pub token_env: Option<String>,
    /// Optional embedded manifest lets the UI show the plugin even when it is temporarily offline.
    pub manifest: Option<PluginManifest>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredPlugin {
    pub endpoint: String,
    pub placement: PluginPlacement,
    pub enabled: bool,
    pub reachable: bool,
    pub manifest: PluginManifest,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHealth {
    pub status: String,
    pub plugin_id: String,
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginInvocationContext {
    pub organization_id: Option<String>,
    pub site_id: Option<String>,
    pub camera_id: Option<String>,
    /// Per-tenant/provider connection identifier. In Commercial this can map to a vault-backed connection.
    pub connection_id: Option<String>,
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaInput {
    Url {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    StorageObject {
        storage_plugin_id: String,
        object_ref: String,
    },
    /// Small media payload embedded directly in the invocation. Useful for snapshots and
    /// decouples AI plugins from storage/network topology. Keep large video in object storage.
    InlineBase64 {
        content_type: String,
        data_base64: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiAnalyzeRequest {
    #[serde(default)]
    pub context: PluginInvocationContext,
    pub camera_id: String,
    pub captured_at: DateTime<Utc>,
    pub input: MediaInput,
    #[serde(default)]
    pub tasks: Vec<String>,
    #[serde(default)]
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    pub label: String,
    pub confidence: f32,
    pub bbox: Option<BoundingBox>,
    #[serde(default)]
    pub attributes: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiAnalyzeResponse {
    pub plugin_id: String,
    pub model: Option<String>,
    pub detections: Vec<Detection>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferAudience {
    Browser,
    Edge,
    #[default]
    Service,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageUploadRequest {
    #[serde(default)]
    pub context: PluginInvocationContext,
    pub namespace: String,
    pub object_key: String,
    pub content_type: String,
    pub content_length: Option<u64>,
    pub expires_seconds: u32,
    #[serde(default)]
    pub audience: TransferAudience,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedTransfer {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub object_ref: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageDownloadRequest {
    #[serde(default)]
    pub context: PluginInvocationContext,
    pub object_ref: String,
    pub expires_seconds: u32,
    #[serde(default)]
    pub audience: TransferAudience,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageDeleteRequest {
    #[serde(default)]
    pub context: PluginInvocationContext,
    pub object_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageDeleteResponse {
    pub deleted: bool,
}
