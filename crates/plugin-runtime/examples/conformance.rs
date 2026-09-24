//! Does your plugin speak the protocol?
//!
//! The contract is written down in `docs/PLUGIN-SDK.md`, and until now the
//! only way to find out whether an implementation matched it was to register
//! it and watch the dashboard. This asks the endpoint directly and exercises
//! every capability it declares.
//!
//! ```text
//! cargo run -p vms-plugin-runtime --example conformance -- http://localhost:9002
//! PLUGIN_TOKEN_ENV=STORAGE_PLUGIN_TOKEN cargo run … -- http://localhost:9002
//! ```
//!
//! It signs and asks; it never uploads bytes and never deletes anything.

use std::process::ExitCode;

use vms_plugin_runtime::PluginRegistry;
use vms_plugin_sdk::{
    AiAnalyzeRequest, EventDeliveryRequest, EventSeverity, FleetEvent, FleetEventKind, MediaInput,
    PluginCapability, PluginPlacement, PluginRegistration, StorageDownloadRequest,
    StorageUploadRequest, TransferAudience,
};

/// A one-pixel JPEG, so an inference plugin is asked for something real
/// without this file carrying a photograph around.
const PIXEL_JPEG_BASE64: &str = "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AKp//2Q==";

#[tokio::main]
async fn main() -> ExitCode {
    let Some(endpoint) = std::env::args().nth(1) else {
        eprintln!("usage: conformance <plugin endpoint> — e.g. http://localhost:9002");
        return ExitCode::from(2);
    };
    let registration = PluginRegistration {
        endpoint: endpoint.clone(),
        placement: PluginPlacement::ControlPlane,
        enabled: true,
        token_env: std::env::var("PLUGIN_TOKEN_ENV").ok(),
        manifest: None,
    };

    let mut failures = 0;
    println!("checking {endpoint}");

    let registry = match PluginRegistry::from_registrations(vec![registration.clone()]).await {
        Ok(registry) => registry,
        Err(error) => {
            println!("  ✗ could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    let manifest = match registry.describe(&registration).await {
        Ok(manifest) => {
            println!(
                "  ✓ manifest: {} {} (protocol {})",
                manifest.id, manifest.version, manifest.protocol_version
            );
            manifest
        }
        Err(error) => {
            // Everything else needs to know who this is, so there is nothing
            // to carry on with.
            println!("  ✗ manifest: {error}");
            return ExitCode::FAILURE;
        }
    };
    if manifest.capabilities.is_empty() {
        println!("  ✗ capabilities: none declared, so nothing would ever call this");
        failures += 1;
    }

    match registry.health(&manifest.id).await {
        Ok(health) => println!("  ✓ health: {}", health.status),
        Err(error) => {
            println!("  ✗ health: {error}");
            failures += 1;
        }
    }

    for capability in &manifest.capabilities {
        match capability {
            PluginCapability::AiAnalyze => {
                let request = AiAnalyzeRequest {
                    context: Default::default(),
                    camera_id: "conformance".into(),
                    captured_at: chrono::Utc::now(),
                    input: MediaInput::InlineBase64 {
                        content_type: "image/jpeg".into(),
                        data_base64: PIXEL_JPEG_BASE64.into(),
                    },
                    tasks: vec!["detect".into()],
                    parameters: serde_json::json!({}),
                };
                match registry.ai_analyze(&manifest.id, &request).await {
                    Ok(answer) => println!(
                        "  ✓ ai_analyze: {} detection(s) from {}",
                        answer.detections.len(),
                        answer.model.unwrap_or_else(|| "an unnamed model".into())
                    ),
                    Err(error) => {
                        println!("  ✗ ai_analyze: {error}");
                        failures += 1;
                    }
                }
            }
            PluginCapability::StorageBlob => {
                let upload = StorageUploadRequest {
                    context: Default::default(),
                    namespace: "conformance".into(),
                    object_key: "check.txt".into(),
                    content_type: "text/plain".into(),
                    content_length: Some(3),
                    expires_seconds: 60,
                    audience: TransferAudience::Service,
                    metadata: Default::default(),
                };
                match registry.storage_upload(&manifest.id, &upload).await {
                    Ok(transfer) => {
                        println!(
                            "  ✓ storage upload: signed {} {}",
                            transfer.method,
                            redact(&transfer.url)
                        );
                        let download = StorageDownloadRequest {
                            context: Default::default(),
                            object_ref: transfer.object_ref.clone(),
                            expires_seconds: 60,
                            audience: TransferAudience::Service,
                        };
                        match registry.storage_download(&manifest.id, &download).await {
                            Ok(transfer) => println!(
                                "  ✓ storage download: signed {} {}",
                                transfer.method,
                                redact(&transfer.url)
                            ),
                            Err(error) => {
                                println!("  ✗ storage download: {error}");
                                failures += 1;
                            }
                        }
                    }
                    Err(error) => {
                        println!("  ✗ storage upload: {error}");
                        failures += 1;
                    }
                }
                println!("  · storage delete: not exercised, it would delete something");
            }
            PluginCapability::EventSink => {
                let request = EventDeliveryRequest {
                    context: Default::default(),
                    event: FleetEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        kind: FleetEventKind::Test,
                        severity: EventSeverity::Info,
                        occurred_at: chrono::Utc::now(),
                        customer_id: String::new(),
                        site_id: String::new(),
                        site_name: String::new(),
                        gateway_id: None,
                        camera_id: None,
                        title: "Conformance check".into(),
                        detail: Some("Nothing is wrong. Something is being tested.".into()),
                        metadata: serde_json::json!({}),
                    },
                };
                match registry.deliver_event(&manifest.id, &request).await {
                    // Declining is a correct answer: a sink may filter.
                    Ok(answer) => println!(
                        "  ✓ event_sink: delivered={} {}",
                        answer.delivered,
                        answer.detail.unwrap_or_default()
                    ),
                    Err(error) => {
                        println!("  ✗ event_sink: {error}");
                        failures += 1;
                    }
                }
            }
        }
    }

    if failures == 0 {
        println!(
            "all good: this plugin speaks protocol {}",
            manifest.protocol_version
        );
        ExitCode::SUCCESS
    } else {
        println!("{failures} check(s) failed");
        ExitCode::FAILURE
    }
}

/// Signed URLs carry credentials in the query string, and this prints to a
/// terminal somebody may well paste into an issue.
fn redact(url: &str) -> String {
    match url.split_once('?') {
        Some((head, _)) => format!("{head}?…"),
        None => url.to_owned(),
    }
}
