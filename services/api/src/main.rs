mod entitlements;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, header::AUTHORIZATION},
    routing::{get, post},
};
use chrono::Utc;
use serde::Serialize;
use tokio::sync::RwLock;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing::info;
use uuid::Uuid;
use vms_domain::{
    AiAnalysisRequest as CameraAiAnalysisRequest, CameraSummary, CameraTelemetry,
    CameraTelemetryBatch, CommandAccepted, CustomerSummary, EditionEntitlement, EnrollmentCreated,
    EnrollmentRequest, FleetSnapshot, FleetSource, GatewayCommand, GatewayCommandKind,
    GatewayCommandResult, GatewayCommandStatus, GatewayCommandView, GatewayEnrollmentRequest,
    GatewayEnrollmentResponse, GatewayHeartbeat, HealthStatus, LiveSessionRequest,
    PlaybackManifest, PlaybackSegment, RecordingManifest, RecordingRequest, RecordingTimeline,
    RtcConfigResponse, RtcIceServerConfig, SiteSummary,
};
use vms_plugin_runtime::PluginRegistry;
use vms_plugin_sdk::{
    AiAnalyzeRequest, AiAnalyzeResponse, PluginHealth, RegisteredPlugin, SignedTransfer,
    StorageDeleteRequest, StorageDeleteResponse, StorageDownloadRequest, StorageUploadRequest,
    TransferAudience,
};

use crate::entitlements::EntitlementResolver;

#[derive(Clone)]
struct AppState {
    gateways: Arc<RwLock<HashMap<String, GatewayHeartbeat>>>,
    camera_batches: Arc<RwLock<HashMap<String, CameraTelemetryBatch>>>,
    enrollments: Arc<RwLock<HashMap<String, Enrollment>>>,
    gateway_tokens: Arc<RwLock<HashMap<String, String>>>,
    gateway_token: Arc<str>,
    stale_camera_seconds: i64,
    entitlements: EntitlementResolver,
    plugins: PluginRegistry,
    plugin_dir: Arc<PathBuf>,
    command_queues: Arc<RwLock<HashMap<String, VecDeque<String>>>>,
    commands: Arc<RwLock<HashMap<String, GatewayCommandView>>>,
    recordings: Arc<RwLock<HashMap<String, RecordingManifest>>>,
    default_storage_plugin: Arc<str>,
    default_ai_plugin: Arc<str>,
    rtc_ice_servers: Arc<Vec<RtcIceServerConfig>>,
    default_retention_days: i64,
}

#[derive(Clone)]
struct Enrollment {
    request: EnrollmentRequest,
    expires_at: chrono::DateTime<Utc>,
    claimed: bool,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vms_api=info,tower_http=info".into()),
        )
        .init();

    let bind = env::var("API_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let addr: SocketAddr = bind.parse()?;
    let plugin_dir = PathBuf::from(env::var("PLUGIN_DIR").unwrap_or_else(|_| "plugins.d".into()));
    let plugins = PluginRegistry::load_dir(&plugin_dir).await?;
    let state = AppState {
        gateways: Arc::new(RwLock::new(HashMap::new())),
        camera_batches: Arc::new(RwLock::new(HashMap::new())),
        enrollments: Arc::new(RwLock::new(HashMap::new())),
        gateway_tokens: Arc::new(RwLock::new(HashMap::new())),
        gateway_token: Arc::from(
            env::var("GATEWAY_TOKEN").unwrap_or_else(|_| "demo-local-token".into()),
        ),
        stale_camera_seconds: env::var("STALE_CAMERA_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(75),
        entitlements: EntitlementResolver::from_env(),
        plugins,
        plugin_dir: Arc::new(plugin_dir),
        command_queues: Arc::new(RwLock::new(HashMap::new())),
        commands: Arc::new(RwLock::new(HashMap::new())),
        recordings: Arc::new(RwLock::new(HashMap::new())),
        default_storage_plugin: Arc::from(
            env::var("DEFAULT_STORAGE_PLUGIN").unwrap_or_else(|_| "storage-s3".into()),
        ),
        default_ai_plugin: Arc::from(
            env::var("DEFAULT_AI_PLUGIN").unwrap_or_else(|_| "ai-http-adapter".into()),
        ),
        rtc_ice_servers: Arc::new(rtc_ice_servers_from_env()),
        default_retention_days: env::var("DEFAULT_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
    };

    let app = build_router(state.clone());

    tokio::spawn(retention_loop(state.clone()));
    info!(%addr, "API listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Build the HTTP surface. Split out of `main` so tests can drive it with a
/// `AppState` of their own rather than a live socket and the environment.
fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/system/edition", get(system_edition))
        .route("/api/v1/fleet", get(fleet))
        .route("/api/v1/enrollments", post(create_enrollment))
        .route("/api/v1/gateways/enroll", post(gateway_enroll))
        .route("/api/v1/cameras", get(cameras))
        .route("/api/v1/cameras/telemetry", post(camera_telemetry))
        .route("/api/v1/gateways", get(gateways))
        .route("/api/v1/gateways/heartbeat", post(gateway_heartbeat))
        .route(
            "/api/v1/gateways/{gateway_id}/commands/next",
            get(gateway_next_command),
        )
        .route(
            "/api/v1/gateways/{gateway_id}/commands/{command_id}/complete",
            post(gateway_complete_command),
        )
        .route("/api/v1/commands/{command_id}", get(command_view))
        .route("/api/v1/rtc/config", get(rtc_config))
        .route(
            "/api/v1/cameras/{camera_id}/live",
            post(create_live_session),
        )
        .route(
            "/api/v1/cameras/{camera_id}/analyze",
            post(create_camera_analysis),
        )
        .route(
            "/api/v1/cameras/{camera_id}/recordings",
            post(create_recording).get(camera_timeline),
        )
        .route(
            "/api/v1/recordings/{recording_id}/playback",
            get(recording_playback),
        )
        .route("/api/v1/plugins", get(plugins_list))
        .route("/api/v1/plugins/reload", post(plugins_reload))
        .route("/api/v1/plugins/{plugin_id}/health", get(plugin_health))
        .route(
            "/api/v1/plugins/{plugin_id}/ai/analyze",
            post(plugin_ai_analyze),
        )
        .route(
            "/api/v1/plugins/{plugin_id}/storage/uploads",
            post(plugin_storage_upload),
        )
        .route(
            "/api/v1/plugins/{plugin_id}/storage/downloads",
            post(plugin_storage_download),
        )
        .route(
            "/api/v1/plugins/{plugin_id}/storage/delete",
            post(plugin_storage_delete),
        )
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "vms-api",
    })
}

async fn system_edition(
    State(state): State<AppState>,
) -> Result<Json<EditionEntitlement>, StatusCode> {
    state
        .entitlements
        .resolve("public")
        .await
        .map(Json)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

async fn gateway_heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(heartbeat): Json<GatewayHeartbeat>,
) -> StatusCode {
    if !authorized_gateway(&headers, &state, &heartbeat.gateway_id).await {
        return StatusCode::UNAUTHORIZED;
    }
    state
        .gateways
        .write()
        .await
        .insert(heartbeat.gateway_id.clone(), heartbeat);
    StatusCode::NO_CONTENT
}

async fn camera_telemetry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut batch): Json<CameraTelemetryBatch>,
) -> StatusCode {
    if !authorized_gateway(&headers, &state, &batch.gateway_id).await {
        return StatusCode::UNAUTHORIZED;
    }
    let entitlement = match state.entitlements.resolve(&batch.customer_id).await {
        Ok(value) => value,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    if let Some(limit) = entitlement.camera_limit
        && batch.cameras.len() > limit
    {
        batch.cameras.truncate(limit);
    }
    state
        .camera_batches
        .write()
        .await
        .insert(batch.gateway_id.clone(), batch);
    StatusCode::NO_CONTENT
}

async fn authorized_gateway(headers: &HeaderMap, state: &AppState, gateway_id: &str) -> bool {
    let Some(token) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    if token == state.gateway_token.as_ref() {
        return true;
    }
    state
        .gateway_tokens
        .read()
        .await
        .get(gateway_id)
        .map(|saved| saved == token)
        .unwrap_or(false)
}

async fn create_enrollment(
    State(state): State<AppState>,
    Json(request): Json<EnrollmentRequest>,
) -> Json<EnrollmentCreated> {
    let enrollment_token = Uuid::new_v4().simple().to_string().to_uppercase();
    let expires_at = Utc::now() + chrono::Duration::minutes(30);
    state.enrollments.write().await.insert(
        enrollment_token.clone(),
        Enrollment {
            request,
            expires_at,
            claimed: false,
        },
    );
    Json(EnrollmentCreated {
        enrollment_token,
        expires_at,
    })
}

async fn gateway_enroll(
    State(state): State<AppState>,
    Json(request): Json<GatewayEnrollmentRequest>,
) -> Result<Json<GatewayEnrollmentResponse>, StatusCode> {
    let enrollment_request = {
        let enrollments = state.enrollments.read().await;
        let enrollment = enrollments
            .get(&request.enrollment_token)
            .ok_or(StatusCode::NOT_FOUND)?;
        if enrollment.claimed || enrollment.expires_at < Utc::now() {
            return Err(StatusCode::GONE);
        }
        enrollment.request.clone()
    };
    let entitlement = state
        .entitlements
        .resolve(&enrollment_request.customer_id)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    {
        let mut enrollments = state.enrollments.write().await;
        let enrollment = enrollments
            .get_mut(&request.enrollment_token)
            .ok_or(StatusCode::NOT_FOUND)?;
        if enrollment.claimed || enrollment.expires_at < Utc::now() {
            return Err(StatusCode::GONE);
        }
        enrollment.claimed = true;
    }
    let gateway_token = Uuid::new_v4().simple().to_string();
    state
        .gateway_tokens
        .write()
        .await
        .insert(request.gateway_id, gateway_token.clone());
    Ok(Json(GatewayEnrollmentResponse {
        gateway_token,
        entitlement,
        customer_id: enrollment_request.customer_id,
        customer_name: enrollment_request.customer_name,
        site_id: enrollment_request.site_id,
        site_name: enrollment_request.site_name,
        city: enrollment_request.city,
    }))
}

async fn gateways(State(state): State<AppState>) -> Json<Vec<GatewayHeartbeat>> {
    let mut values: Vec<_> = state.gateways.read().await.values().cloned().collect();
    values.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    Json(values)
}

async fn cameras(State(state): State<AppState>) -> Json<Vec<CameraTelemetry>> {
    let batches = state.camera_batches.read().await;
    let now = Utc::now();
    let mut values: Vec<_> = batches
        .values()
        .flat_map(|batch| batch.cameras.clone())
        .collect();
    for camera in &mut values {
        if (now - camera.last_seen).num_seconds() > state.stale_camera_seconds {
            camera.status = HealthStatus::Offline;
            camera.fps = None;
            camera.bitrate_kbps = None;
            camera.last_error = Some("gateway telemetry is stale".into());
        }
    }
    values.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.camera_id.cmp(&b.camera_id))
    });
    Json(values)
}

async fn fleet(State(state): State<AppState>) -> Json<FleetSnapshot> {
    let batches = state.camera_batches.read().await;
    if batches.values().any(|batch| !batch.cameras.is_empty()) {
        return Json(live_fleet(
            batches.values().cloned().collect(),
            state.stale_camera_seconds,
        ));
    }
    Json(demo_fleet())
}

async fn plugins_list(State(state): State<AppState>) -> Json<Vec<RegisteredPlugin>> {
    Json(state.plugins.list().await)
}

async fn plugins_reload(
    State(state): State<AppState>,
) -> Result<Json<Vec<RegisteredPlugin>>, StatusCode> {
    state
        .plugins
        .reload(state.plugin_dir.as_ref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(state.plugins.list().await))
}

async fn plugin_health(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> Result<Json<PluginHealth>, StatusCode> {
    if !state.plugins.is_registered(&plugin_id).await {
        return Err(StatusCode::NOT_FOUND);
    }
    state
        .plugins
        .health(&plugin_id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

async fn plugin_ai_analyze(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    Json(body): Json<AiAnalyzeRequest>,
) -> Result<Json<AiAnalyzeResponse>, StatusCode> {
    if !state.plugins.is_registered(&plugin_id).await {
        return Err(StatusCode::NOT_FOUND);
    }
    state
        .plugins
        .ai_analyze(&plugin_id, &body)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

async fn plugin_storage_upload(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    Json(body): Json<StorageUploadRequest>,
) -> Result<Json<SignedTransfer>, StatusCode> {
    if !state.plugins.is_registered(&plugin_id).await {
        return Err(StatusCode::NOT_FOUND);
    }
    state
        .plugins
        .storage_upload(&plugin_id, &body)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

async fn plugin_storage_download(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    Json(body): Json<StorageDownloadRequest>,
) -> Result<Json<SignedTransfer>, StatusCode> {
    if !state.plugins.is_registered(&plugin_id).await {
        return Err(StatusCode::NOT_FOUND);
    }
    state
        .plugins
        .storage_download(&plugin_id, &body)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

async fn plugin_storage_delete(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    Json(body): Json<StorageDeleteRequest>,
) -> Result<Json<StorageDeleteResponse>, StatusCode> {
    if !state.plugins.is_registered(&plugin_id).await {
        return Err(StatusCode::NOT_FOUND);
    }
    state
        .plugins
        .storage_delete(&plugin_id, &body)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

fn rtc_ice_servers_from_env() -> Vec<RtcIceServerConfig> {
    let raw = env::var("RTC_ICE_SERVERS_JSON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            r#"[{"urls":["stun:stun.l.google.com:19302"],"username":"","credential":""}]"#.into()
        });
    match serde_json::from_str::<Vec<RtcIceServerConfig>>(&raw) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "invalid RTC_ICE_SERVERS_JSON; WebRTC will start without configured ICE servers");
            vec![]
        }
    }
}

async fn rtc_config(State(state): State<AppState>) -> Json<RtcConfigResponse> {
    Json(RtcConfigResponse {
        ice_servers: state.rtc_ice_servers.as_ref().clone(),
    })
}

async fn gateway_for_camera(state: &AppState, camera_id: &str) -> Result<String, StatusCode> {
    let batches = state.camera_batches.read().await;
    batches
        .values()
        .find_map(|batch| {
            batch
                .cameras
                .iter()
                .any(|camera| camera.camera_id == camera_id)
                .then(|| batch.gateway_id.clone())
        })
        .ok_or(StatusCode::NOT_FOUND)
}

async fn enqueue_gateway_command(state: &AppState, command: GatewayCommand) -> CommandAccepted {
    let accepted = CommandAccepted {
        command_id: command.id.clone(),
        status: GatewayCommandStatus::Queued,
    };
    let gateway_id = command.gateway_id.clone();
    state.commands.write().await.insert(
        command.id.clone(),
        GatewayCommandView {
            command: command.clone(),
            status: GatewayCommandStatus::Queued,
            result: None,
        },
    );
    state
        .command_queues
        .write()
        .await
        .entry(gateway_id)
        .or_default()
        .push_back(command.id);
    accepted
}

async fn create_live_session(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    Json(request): Json<LiveSessionRequest>,
) -> Result<(StatusCode, Json<CommandAccepted>), StatusCode> {
    let gateway_id = gateway_for_camera(&state, &camera_id).await?;
    let now = Utc::now();
    let session_seconds = request.session_seconds.clamp(30, 3600);
    let command = GatewayCommand {
        id: Uuid::new_v4().to_string(),
        gateway_id,
        created_at: now,
        expires_at: now + chrono::Duration::minutes(2),
        kind: GatewayCommandKind::Live {
            camera_id,
            offer_sdp: request.offer_sdp,
            offer_type: request.offer_type,
            session_seconds,
            ice_servers: state.rtc_ice_servers.as_ref().clone(),
        },
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(enqueue_gateway_command(&state, command).await),
    ))
}

async fn create_camera_analysis(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    Json(request): Json<CameraAiAnalysisRequest>,
) -> Result<(StatusCode, Json<CommandAccepted>), StatusCode> {
    let gateway_id = gateway_for_camera(&state, &camera_id).await?;
    let now = Utc::now();
    let ai_plugin_id = request
        .ai_plugin_id
        .unwrap_or_else(|| state.default_ai_plugin.to_string());
    let storage_plugin_id = request
        .storage_plugin_id
        .unwrap_or_else(|| state.default_storage_plugin.to_string());
    let tasks = if request.tasks.is_empty() {
        vec!["person".into(), "vehicle".into()]
    } else {
        request.tasks
    };
    let command = GatewayCommand {
        id: Uuid::new_v4().to_string(),
        gateway_id,
        created_at: now,
        expires_at: now + chrono::Duration::minutes(2),
        kind: GatewayCommandKind::Analyze {
            camera_id,
            ai_plugin_id,
            storage_plugin_id,
            tasks,
        },
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(enqueue_gateway_command(&state, command).await),
    ))
}

async fn create_recording(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    Json(request): Json<RecordingRequest>,
) -> Result<(StatusCode, Json<CommandAccepted>), StatusCode> {
    let duration_seconds = request.duration_seconds.clamp(2, 3600);
    let segment_seconds = request.segment_seconds.clamp(1, 30).min(duration_seconds);
    let storage_plugin_id = request
        .storage_plugin_id
        .unwrap_or_else(|| state.default_storage_plugin.to_string());

    let gateway_id = gateway_for_camera(&state, &camera_id).await?;

    let now = Utc::now();
    let command = GatewayCommand {
        id: Uuid::new_v4().to_string(),
        gateway_id: gateway_id.clone(),
        created_at: now,
        expires_at: now + chrono::Duration::minutes(5),
        kind: GatewayCommandKind::Record {
            camera_id,
            duration_seconds,
            segment_seconds,
            storage_plugin_id,
        },
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(enqueue_gateway_command(&state, command).await),
    ))
}

async fn gateway_next_command(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Option<GatewayCommand>>, StatusCode> {
    loop {
        if !authorized_gateway(&headers, &state, &gateway_id).await {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let command_id = state
            .command_queues
            .write()
            .await
            .entry(gateway_id.clone())
            .or_default()
            .pop_front();
        let Some(command_id) = command_id else {
            return Ok(Json(None));
        };
        let mut commands = state.commands.write().await;
        let Some(view) = commands.get_mut(&command_id) else {
            continue;
        };
        if view.command.expires_at < Utc::now() {
            view.status = GatewayCommandStatus::Failed;
            view.result = Some(GatewayCommandResult {
                command_id: view.command.id.clone(),
                gateway_id: gateway_id.clone(),
                status: GatewayCommandStatus::Failed,
                completed_at: Utc::now(),
                error: Some("command expired before gateway picked it up".into()),
                recording: None,
                live: None,
                analysis: None,
            });
            continue;
        }
        view.status = GatewayCommandStatus::Running;
        return Ok(Json(Some(view.command.clone())));
    }
}

async fn gateway_complete_command(
    State(state): State<AppState>,
    Path((gateway_id, command_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(mut result): Json<GatewayCommandResult>,
) -> StatusCode {
    if !authorized_gateway(&headers, &state, &gateway_id).await {
        return StatusCode::UNAUTHORIZED;
    }
    if result.command_id != command_id || result.gateway_id != gateway_id {
        return StatusCode::BAD_REQUEST;
    }
    let mut commands = state.commands.write().await;
    let Some(view) = commands.get_mut(&command_id) else {
        return StatusCode::NOT_FOUND;
    };
    if view.command.gateway_id != gateway_id {
        return StatusCode::FORBIDDEN;
    }

    if result.status == GatewayCommandStatus::Succeeded
        && let Some(recording) = result.recording.as_mut()
    {
        recording.delete_after = (state.default_retention_days > 0)
            .then(|| recording.ended_at + chrono::Duration::days(state.default_retention_days));
        state
            .recordings
            .write()
            .await
            .insert(recording.recording_id.clone(), recording.clone());
    }
    view.status = result.status.clone();
    view.result = Some(result);
    StatusCode::NO_CONTENT
}

async fn command_view(
    State(state): State<AppState>,
    Path(command_id): Path<String>,
) -> Result<Json<GatewayCommandView>, StatusCode> {
    state
        .commands
        .read()
        .await
        .get(&command_id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn camera_timeline(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
) -> Json<RecordingTimeline> {
    let mut recordings: Vec<_> = state
        .recordings
        .read()
        .await
        .values()
        .filter(|recording| recording.camera_id == camera_id)
        .cloned()
        .collect();
    recordings.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    Json(RecordingTimeline {
        camera_id,
        recordings,
    })
}

async fn recording_playback(
    State(state): State<AppState>,
    Path(recording_id): Path<String>,
) -> Result<Json<PlaybackManifest>, StatusCode> {
    let recording = state
        .recordings
        .read()
        .await
        .get(&recording_id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    let context = vms_plugin_sdk::PluginInvocationContext {
        camera_id: Some(recording.camera_id.clone()),
        ..Default::default()
    };
    let init_transfer = state
        .plugins
        .storage_download(
            &recording.init.storage_plugin_id,
            &StorageDownloadRequest {
                context: context.clone(),
                object_ref: recording.init.object_ref.clone(),
                expires_seconds: 900,
                audience: TransferAudience::Browser,
            },
        )
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut segments = Vec::with_capacity(recording.segments.len());
    for segment in &recording.segments {
        let transfer = state
            .plugins
            .storage_download(
                &segment.object.storage_plugin_id,
                &StorageDownloadRequest {
                    context: context.clone(),
                    object_ref: segment.object.object_ref.clone(),
                    expires_seconds: 900,
                    audience: TransferAudience::Browser,
                },
            )
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        segments.push(PlaybackSegment {
            id: segment.id.clone(),
            sequence: segment.sequence,
            started_at: segment.started_at,
            ended_at: segment.ended_at,
            duration_ms: segment.duration_ms,
            url: transfer.url,
            headers: transfer.headers,
        });
    }
    let codec = recording.codec.clone();
    Ok(Json(PlaybackManifest {
        recording_id: recording.recording_id,
        camera_id: recording.camera_id,
        mime_type: format!("video/mp4; codecs=\"{codec}\""),
        codec,
        init_url: init_transfer.url,
        init_headers: init_transfer.headers,
        segments,
    }))
}

async fn retention_loop(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let now = Utc::now();
        let expired: Vec<_> = state
            .recordings
            .read()
            .await
            .values()
            .filter(|recording| recording.delete_after.as_ref().is_some_and(|at| at <= &now))
            .cloned()
            .collect();
        for recording in expired {
            let mut ok = true;
            let context = vms_plugin_sdk::PluginInvocationContext {
                camera_id: Some(recording.camera_id.clone()),
                ..Default::default()
            };
            let mut objects = Vec::with_capacity(recording.segments.len() + 1);
            objects.push(recording.init.clone());
            objects.extend(
                recording
                    .segments
                    .iter()
                    .map(|segment| segment.object.clone()),
            );
            for object in objects {
                let request = StorageDeleteRequest {
                    context: context.clone(),
                    object_ref: object.object_ref,
                };
                match state
                    .plugins
                    .storage_delete(&object.storage_plugin_id, &request)
                    .await
                {
                    Ok(response) if response.deleted => {}
                    _ => ok = false,
                }
            }
            if ok {
                state
                    .recordings
                    .write()
                    .await
                    .remove(&recording.recording_id);
                info!(recording_id = %recording.recording_id, "retention removed recording objects");
            }
        }
    }
}

fn live_fleet(batches: Vec<CameraTelemetryBatch>, stale_camera_seconds: i64) -> FleetSnapshot {
    #[derive(Default)]
    struct SiteBuild {
        name: String,
        city: String,
        cameras: BTreeMap<String, CameraSummary>,
    }
    #[derive(Default)]
    struct CustomerBuild {
        name: String,
        sites: BTreeMap<String, SiteBuild>,
    }
    let now = Utc::now();
    let mut customers: BTreeMap<String, CustomerBuild> = BTreeMap::new();
    for batch in batches {
        let customer = customers.entry(batch.customer_id.clone()).or_default();
        customer.name = batch.customer_name.clone();
        let site = customer.sites.entry(batch.site_id.clone()).or_default();
        site.name = batch.site_name.clone();
        site.city = batch.city.clone();
        for camera in batch.cameras {
            let stale = (now - camera.last_seen).num_seconds() > stale_camera_seconds;
            site.cameras.insert(
                camera.camera_id.clone(),
                CameraSummary {
                    id: camera.camera_id,
                    name: camera.name,
                    site_id: camera.site_id,
                    status: if stale {
                        HealthStatus::Offline
                    } else {
                        camera.status
                    },
                    fps: if stale { None } else { camera.fps },
                    bitrate_kbps: if stale { None } else { camera.bitrate_kbps },
                    last_seen: camera.last_seen,
                },
            );
        }
    }
    FleetSnapshot {
        generated_at: Utc::now(),
        source: FleetSource::Live,
        customers: customers
            .into_iter()
            .map(|(customer_id, customer)| CustomerSummary {
                id: customer_id.clone(),
                name: customer.name,
                sites: customer
                    .sites
                    .into_iter()
                    .map(|(site_id, site)| SiteSummary {
                        id: site_id,
                        customer_id: customer_id.clone(),
                        name: site.name,
                        city: site.city,
                        cameras: site.cameras.into_values().collect(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn demo_fleet() -> FleetSnapshot {
    let now = Utc::now();
    let make_camera =
        |id: &str, name: &str, site_id: &str, status: HealthStatus, fps, bitrate| CameraSummary {
            id: id.to_string(),
            name: name.to_string(),
            site_id: site_id.to_string(),
            status,
            fps,
            bitrate_kbps: bitrate,
            last_seen: now,
        };
    FleetSnapshot {
        generated_at: now,
        source: FleetSource::Demo,
        customers: vec![
            CustomerSummary {
                id: "acme-retail".into(),
                name: "ACME Retail".into(),
                sites: vec![
                    SiteSummary {
                        id: "madrid-centro".into(),
                        customer_id: "acme-retail".into(),
                        name: "Madrid Centro".into(),
                        city: "Madrid".into(),
                        cameras: vec![
                            make_camera(
                                "cam-001",
                                "Entrance",
                                "madrid-centro",
                                HealthStatus::Healthy,
                                Some(25.0),
                                Some(1840),
                            ),
                            make_camera(
                                "cam-002",
                                "Checkout 01",
                                "madrid-centro",
                                HealthStatus::Healthy,
                                Some(25.0),
                                Some(2110),
                            ),
                            make_camera(
                                "cam-003",
                                "Stock room",
                                "madrid-centro",
                                HealthStatus::Warning,
                                Some(12.0),
                                Some(620),
                            ),
                        ],
                    },
                    SiteSummary {
                        id: "valencia-russafa".into(),
                        customer_id: "acme-retail".into(),
                        name: "Valencia Russafa".into(),
                        city: "Valencia".into(),
                        cameras: vec![
                            make_camera(
                                "cam-004",
                                "Entrance",
                                "valencia-russafa",
                                HealthStatus::Healthy,
                                Some(20.0),
                                Some(1520),
                            ),
                            make_camera(
                                "cam-005",
                                "Warehouse",
                                "valencia-russafa",
                                HealthStatus::Offline,
                                None,
                                None,
                            ),
                        ],
                    },
                ],
            },
            CustomerSummary {
                id: "hotel-group".into(),
                name: "Hotel Group".into(),
                sites: vec![SiteSummary {
                    id: "alicante-marina".into(),
                    customer_id: "hotel-group".into(),
                    name: "Alicante Marina".into(),
                    city: "Alicante".into(),
                    cameras: vec![
                        make_camera(
                            "cam-006",
                            "Reception",
                            "alicante-marina",
                            HealthStatus::Healthy,
                            Some(25.0),
                            Some(2040),
                        ),
                        make_camera(
                            "cam-007",
                            "Parking",
                            "alicante-marina",
                            HealthStatus::Healthy,
                            Some(15.0),
                            Some(940),
                        ),
                    ],
                }],
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const SHARED_TOKEN: &str = "shared-bootstrap-token";

    async fn test_state() -> AppState {
        let dir = tempfile::tempdir().expect("temp plugin dir");
        let plugin_dir = dir.keep();
        AppState {
            gateways: Arc::new(RwLock::new(HashMap::new())),
            camera_batches: Arc::new(RwLock::new(HashMap::new())),
            enrollments: Arc::new(RwLock::new(HashMap::new())),
            gateway_tokens: Arc::new(RwLock::new(HashMap::new())),
            gateway_token: Arc::from(SHARED_TOKEN),
            stale_camera_seconds: 75,
            // No ENTITLEMENTS_URL is set under test, so this resolves the
            // community entitlement locally and never reaches the network.
            entitlements: EntitlementResolver::from_env(),
            plugins: PluginRegistry::load_dir(&plugin_dir)
                .await
                .expect("empty plugin dir"),
            plugin_dir: Arc::new(plugin_dir),
            command_queues: Arc::new(RwLock::new(HashMap::new())),
            commands: Arc::new(RwLock::new(HashMap::new())),
            recordings: Arc::new(RwLock::new(HashMap::new())),
            default_storage_plugin: Arc::from("storage-s3"),
            default_ai_plugin: Arc::from("ai-http-adapter"),
            rtc_ice_servers: Arc::new(Vec::new()),
            default_retention_days: 30,
        }
    }

    async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = build_router(state.clone())
            .oneshot(request)
            .await
            .expect("router responds");
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    fn post(uri: &str, token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn telemetry_batch(gateway_id: &str) -> serde_json::Value {
        serde_json::json!({
            "gateway_id": gateway_id,
            "customer_id": "cust-1",
            "customer_name": "Customer",
            "site_id": "site-1",
            "site_name": "Site",
            "city": "Barcelona",
            "sent_at": chrono::Utc::now(),
            "cameras": [],
        })
    }

    #[tokio::test]
    async fn healthz_reports_the_service() {
        let state = test_state().await;
        let (status, body) = send(&state, get("/healthz")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["service"], "vms-api");
    }

    // ---- the authorisation boundary ----

    #[tokio::test]
    async fn telemetry_without_a_token_is_refused() {
        // Unauthenticated telemetry would let anyone inject camera state into
        // another customer's fleet view.
        let state = test_state().await;
        let (status, _) = send(
            &state,
            post("/api/v1/cameras/telemetry", None, telemetry_batch("gw-1")),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(state.camera_batches.read().await.is_empty());
    }

    #[tokio::test]
    async fn telemetry_with_the_wrong_token_is_refused() {
        let state = test_state().await;
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some("not-the-token"),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_malformed_authorization_header_is_refused() {
        // Only the Bearer scheme is accepted; a bare token must not pass.
        let state = test_state().await;
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/cameras/telemetry")
            .header("content-type", "application/json")
            .header("authorization", SHARED_TOKEN)
            .body(Body::from(telemetry_batch("gw-1").to_string()))
            .unwrap();
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_shared_token_admits_any_gateway_id() {
        // Deliberate: this is the bootstrap path before a gateway has enrolled.
        // Pinned because it is a real weakening — anyone holding GATEWAY_TOKEN
        // can post as any gateway, so it must stay a bootstrap secret.
        let state = test_state().await;
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(SHARED_TOKEN),
                telemetry_batch("any-gateway-at-all"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(
            state
                .camera_batches
                .read()
                .await
                .contains_key("any-gateway-at-all")
        );
    }

    #[tokio::test]
    async fn an_enrolled_gateway_token_works_and_does_not_cover_other_gateways() {
        let state = test_state().await;

        let (status, created) = send(
            &state,
            post(
                "/api/v1/enrollments",
                None,
                serde_json::json!({
                    "customer_id": "cust-1",
                    "customer_name": "Customer",
                    "site_id": "site-1",
                    "site_name": "Site",
                    "city": "Barcelona",
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let enrollment_token = created["enrollment_token"].as_str().unwrap().to_owned();

        let (status, enrolled) = send(
            &state,
            post(
                "/api/v1/gateways/enroll",
                None,
                serde_json::json!({
                    "enrollment_token": enrollment_token,
                    "gateway_id": "gw-1",
                    "hostname": "edge-1",
                    "version": "0.1.0",
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let gateway_token = enrolled["gateway_token"].as_str().unwrap().to_owned();
        assert_ne!(
            gateway_token, SHARED_TOKEN,
            "enrolment must mint a new token"
        );

        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(&gateway_token),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // The token is bound to the gateway it was issued for. Without this a
        // stolen token from one site would speak for every other site.
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(&gateway_token),
                telemetry_batch("gw-2-belonging-to-someone-else"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_enrollment_token_cannot_be_used_twice() {
        let state = test_state().await;
        let (_, created) = send(
            &state,
            post(
                "/api/v1/enrollments",
                None,
                serde_json::json!({
                    "customer_id": "cust-1",
                    "customer_name": "Customer",
                    "site_id": "site-1",
                    "site_name": "Site",
                    "city": "Barcelona",
                }),
            ),
        )
        .await;
        let token = created["enrollment_token"].as_str().unwrap().to_owned();
        let enroll = |gateway: &str| {
            post(
                "/api/v1/gateways/enroll",
                None,
                serde_json::json!({
                    "enrollment_token": token,
                    "gateway_id": gateway,
                    "hostname": "edge",
                    "version": "0.1.0",
                }),
            )
        };
        let (first, _) = send(&state, enroll("gw-1")).await;
        assert_eq!(first, StatusCode::OK);
        let (second, _) = send(&state, enroll("gw-2")).await;
        assert_ne!(
            second,
            StatusCode::OK,
            "a replayed enrolment code must not mint a second gateway token"
        );
    }

    #[tokio::test]
    async fn a_heartbeat_from_an_unauthenticated_gateway_is_refused() {
        let state = test_state().await;
        let (status, _) = send(
            &state,
            post(
                "/api/v1/gateways/heartbeat",
                None,
                serde_json::json!({
                    "gateway_id": "gw-1",
                    "site_id": "site-1",
                    "hostname": "edge-1",
                    "version": "0.1.0",
                    "uptime_seconds": 10,
                    "cpu_percent": 1.0,
                    "memory_percent": 2.0,
                    "cameras_seen": 0,
                    "healthy_cameras": 0,
                    "warning_cameras": 0,
                    "offline_cameras": 0,
                    "sent_at": chrono::Utc::now(),
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(state.gateways.read().await.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_plugin_is_a_404_not_a_bad_gateway() {
        // 502 says "the upstream failed", which invites a retry. There is no
        // upstream here — the id is wrong, and no amount of retrying fixes that.
        let state = test_state().await;
        for path in [
            "/api/v1/plugins/nope/health",
            "/api/v1/plugins/nope/ai/analyze",
            "/api/v1/plugins/nope/storage/uploads",
        ] {
            let request = if path.ends_with("health") {
                get(path)
            } else {
                post(path, None, serde_json::json!({}))
            };
            let (status, _) = send(&state, request).await;
            assert_ne!(
                status,
                StatusCode::BAD_GATEWAY,
                "{path} reported an outage for a plugin that does not exist"
            );
        }
        let (status, _) = send(&state, get("/api/v1/plugins/nope/health")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_command_is_a_404_not_a_500() {
        let state = test_state().await;
        let (status, _) = send(&state, get("/api/v1/commands/does-not-exist")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    fn camera(camera_id: &str, gateway_id: &str) -> serde_json::Value {
        serde_json::json!({
            "camera_id": camera_id,
            "gateway_id": gateway_id,
            "site_id": "site-1",
            "name": "Front door",
            "status": "healthy",
            "manufacturer": null, "model": null, "firmware": null,
            "profile_name": null, "codec": "h264",
            "width": 1920, "height": 1080,
            "fps": null, "bitrate_kbps": null,
            "packet_loss": 0, "reconnects": 0,
            "rtsp_endpoint": "rtsp://10.0.0.5/stream",
            "last_seen": chrono::Utc::now(),
            "last_error": null,
        })
    }

    async fn with_camera(state: &AppState, camera_id: &str, gateway_id: &str) {
        let mut batch = telemetry_batch(gateway_id);
        batch["cameras"] = serde_json::json!([camera(camera_id, gateway_id)]);
        let (status, _) = send(
            state,
            post("/api/v1/cameras/telemetry", Some(SHARED_TOKEN), batch),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_live_request_becomes_a_command_the_right_gateway_collects() {
        // The whole point of the outbound design: the cloud never dials the
        // gateway, it parks a command that the gateway picks up on its own poll.
        let state = test_state().await;
        with_camera(&state, "cam-1", "gw-1").await;

        let (status, accepted) = send(
            &state,
            post(
                "/api/v1/cameras/cam-1/live",
                None,
                serde_json::json!({"offer_sdp": "v=0\r\n", "offer_type": "offer"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let command_id = accepted["command_id"].as_str().unwrap().to_owned();

        // Another gateway must not be handed this site's command.
        // An empty queue answers 200 with a null body rather than 204, because
        // the gateway deserialises Option<GatewayCommand> and an empty body has
        // nothing to deserialise. Assert the meaning, not the status.
        let (status, offered) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-2/commands/next")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            offered.is_null(),
            "gw-2 was offered a command addressed to gw-1: {offered}"
        );

        let (status, command) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/commands/next")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(command["id"], command_id.as_str());

        // A command is handed out once. Polling again after collection must not
        // replay it, or a gateway reconnecting would start duplicate sessions.
        let (_, replayed) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/commands/next")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(replayed.is_null(), "the command was handed out twice");

        let (status, _) = send(
            &state,
            post(
                &format!("/api/v1/gateways/gw-1/commands/{command_id}/complete"),
                Some(SHARED_TOKEN),
                serde_json::json!({
                    "command_id": command_id,
                    "gateway_id": "gw-1",
                    "status": "succeeded",
                    "completed_at": chrono::Utc::now(),
                    "error": null,
                    "recording": null,
                    "live": null,
                    "analysis": null,
                }),
            ),
        )
        .await;
        assert!(
            status.is_success() || status == StatusCode::NO_CONTENT,
            "completing a collected command failed with {status}"
        );

        let (status, view) = send(&state, get(&format!("/api/v1/commands/{command_id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["command"]["id"], command_id.as_str());
        assert_eq!(
            view["status"], "succeeded",
            "the completion must be visible to whoever is waiting on the command"
        );
    }

    #[tokio::test]
    async fn a_live_request_for_an_unknown_camera_is_a_404() {
        let state = test_state().await;
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/no-such-camera/live",
                None,
                serde_json::json!({"offer_sdp": "v=0\r\n"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_gateway_cannot_collect_another_gateways_commands() {
        // The path carries the gateway id, so an enrolled token must be checked
        // against it and not merely be present.
        let state = test_state().await;
        state
            .gateway_tokens
            .write()
            .await
            .insert("gw-1".into(), "token-for-gw-1".into());
        let (status, _) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-2/commands/next")
                .header("authorization", "Bearer token-for-gw-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
