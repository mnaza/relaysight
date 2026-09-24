mod auth;
mod health;
mod store;
mod turn;

/// Seconds since the epoch. Used for TURN credential expiry.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
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
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header::AUTHORIZATION},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use tracing::{info, warn};
use uuid::Uuid;
use vms_domain::{
    AiAnalysisRequest as CameraAiAnalysisRequest, AuditView, CameraSummary, CameraTelemetry,
    CameraTelemetryBatch, ClipRequest, CommandAccepted, CreateUserRequest, CustomerSummary,
    EditionEntitlement, EnrollmentCreated, EnrollmentRequest, FleetSnapshot, FleetSource,
    GatewayCommand, GatewayCommandKind, GatewayCommandResult, GatewayCommandStatus,
    GatewayCommandView, GatewayEnrollmentRequest, GatewayEnrollmentResponse, GatewayHeartbeat,
    GatewayView, HealthStatus, IncidentView, KeepRule, LiveSessionRequest, PlaybackManifest,
    PlaybackSegment, RecordingMode, RecordingPolicy, RecordingPolicyRequest, RecordingRequest,
    RecordingTimeline, RtcConfigResponse, SiteSummary, SourceKind, UpdateUserRequest, UserView,
    VideoSource, VideoSourceRequest,
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
    store: Arc<dyn crate::store::Store>,
    gateway_token: Arc<str>,
    stale_camera_seconds: i64,
    entitlements: EntitlementResolver,
    plugins: PluginRegistry,
    plugin_dir: Arc<PathBuf>,
    command_queues: Arc<RwLock<HashMap<String, VecDeque<String>>>>,
    commands: Arc<RwLock<HashMap<String, GatewayCommandView>>>,
    default_storage_plugin: Arc<str>,
    default_ai_plugin: Arc<str>,
    rtc: Arc<crate::turn::RtcConfig>,
    default_retention_days: i64,
    login_throttle: Arc<tokio::sync::Mutex<crate::auth::LoginThrottle>>,
    cookie_secure: bool,
    /// No incident sweeps until the API has been up this long — right after a
    /// restart every camera looks silent until its gateway re-reports.
    incident_grace: Duration,
    incident_retention_days: i64,
    /// Open tunnel sessions, keyed by session id. In memory on purpose: a
    /// restart should close every way into a site network rather than
    /// carrying them across.
    tunnels: Arc<RwLock<HashMap<String, vms_domain::TunnelSession>>>,
    /// Requests waiting for a gateway to perform them, per gateway.
    tunnel_calls: Arc<RwLock<HashMap<String, VecDeque<vms_domain::TunnelCall>>>>,
    /// Answers the gateway has posted back, keyed by request id.
    tunnel_answers: Arc<RwLock<HashMap<String, vms_domain::TunnelAnswer>>>,
    /// How long the hourly rollups are kept. Thirty days is two screens of
    /// history and a few hundred rows per camera.
    health_retention_days: i64,
    audit_retention_days: i64,
    up_since: std::time::Instant,
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

    // Say this at startup. Without a relay, sites behind symmetric NAT or a strict
    // egress firewall never connect, and the symptom is a live session that simply
    // never starts — with nothing in the logs to point at the cause.
    let rtc = crate::turn::RtcConfig::from_env();
    if rtc.turn_enabled() {
        info!(
            relays = rtc.turn_urls.len(),
            ttl_secs = rtc.ttl_secs,
            "TURN configured; credentials are minted per session"
        );
    } else {
        warn!(
            "no TURN relay configured: sites that cannot hold a direct path will fail to connect"
        );
    }
    let plugin_dir = PathBuf::from(env::var("PLUGIN_DIR").unwrap_or_else(|_| "plugins.d".into()));
    let plugins = PluginRegistry::load_dir(&plugin_dir).await?;
    // A failed open or migration refuses to start — a half-up API that forgot
    // its fleet is worse than a crash loop.
    let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:data/vms.db".into());
    if let Some(path) = database_url.strip_prefix("sqlite:")
        && let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let store: Arc<dyn store::Store> = Arc::new(store::SqliteStore::connect(&database_url).await?);
    info!(%database_url, "fleet store open");
    let admin_password = env::var("ADMIN_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty());
    let force_reset = env::var("ADMIN_PASSWORD_RESET").is_ok_and(|value| value == "true");
    auth::seed_credentials(
        store.as_ref(),
        &env::var("ADMIN_EMAIL").unwrap_or_else(|_| "admin@localhost".into()),
        admin_password.as_deref(),
        force_reset,
    )
    .await?;
    let stale_camera_seconds: i64 = env::var("STALE_CAMERA_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(75);

    let state = AppState {
        gateways: Arc::new(RwLock::new(HashMap::new())),
        camera_batches: Arc::new(RwLock::new(HashMap::new())),
        store,
        gateway_token: Arc::from(
            env::var("GATEWAY_TOKEN").unwrap_or_else(|_| "demo-local-token".into()),
        ),
        stale_camera_seconds,
        entitlements: EntitlementResolver::from_env(),
        plugins,
        plugin_dir: Arc::new(plugin_dir),
        command_queues: Arc::new(RwLock::new(HashMap::new())),
        commands: Arc::new(RwLock::new(HashMap::new())),
        default_storage_plugin: Arc::from(
            env::var("DEFAULT_STORAGE_PLUGIN").unwrap_or_else(|_| "storage-s3".into()),
        ),
        default_ai_plugin: Arc::from(
            env::var("DEFAULT_AI_PLUGIN").unwrap_or_else(|_| "ai-http-adapter".into()),
        ),
        rtc: Arc::new(rtc),
        default_retention_days: env::var("DEFAULT_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
        login_throttle: Arc::new(tokio::sync::Mutex::new(auth::LoginThrottle::default())),
        cookie_secure: env::var("AUTH_COOKIE_SECURE").is_ok_and(|value| value == "true"),
        incident_grace: Duration::from_secs(stale_camera_seconds.max(0) as u64),
        tunnels: Arc::new(RwLock::new(HashMap::new())),
        tunnel_calls: Arc::new(RwLock::new(HashMap::new())),
        tunnel_answers: Arc::new(RwLock::new(HashMap::new())),
        health_retention_days: env::var("HEALTH_RETENTION_DAYS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(30),
        incident_retention_days: env::var("INCIDENT_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90),
        audit_retention_days: env::var("AUDIT_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        up_since: std::time::Instant::now(),
    };

    // Plugins somebody connected from the dashboard, beside the ones on disk.
    if reload_plugins(&state).await.is_err() {
        warn!("stored plugin registrations could not be loaded; plugins.d only");
    }
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
///
/// Three groups. `open` is health, the login pair, and the gateway machine
/// endpoints, which carry their own per-gateway bearer checks in the
/// handlers. `machine_plugins` is the two plugin endpoints the edge calls —
/// no gateway id in the path, so one middleware accepts any gateway
/// credential. Everything else is `protected` behind the session middleware —
/// a route added there is covered by construction, and must also be added to
/// PROTECTED_ROUTES in the tests.
fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);
    let open = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/system/edition", get(system_edition))
        .route("/api/v1/auth/login", post(crate::auth::auth_login))
        .route("/api/v1/auth/session", get(crate::auth::auth_session))
        .route("/api/v1/cameras/telemetry", post(camera_telemetry))
        .route("/api/v1/gateways/heartbeat", post(gateway_heartbeat))
        .route("/api/v1/gateways/enroll", post(gateway_enroll))
        .route(
            "/api/v1/gateways/{gateway_id}/commands/next",
            get(gateway_next_command),
        )
        .route(
            "/api/v1/gateways/{gateway_id}/recording-policies",
            get(gateway_recording_policies),
        )
        .route(
            "/api/v1/gateways/{gateway_id}/recordings",
            post(gateway_recording),
        )
        .route(
            "/api/v1/gateways/{gateway_id}/tunnel/next",
            get(next_tunnel_call),
        )
        .route(
            "/api/v1/gateways/{gateway_id}/tunnel/answer",
            post(answer_tunnel_call),
        )
        .route(
            "/api/v1/gateways/{gateway_id}/sources",
            get(gateway_video_sources),
        )
        .route(
            "/api/v1/gateways/{gateway_id}/commands/{command_id}/complete",
            post(gateway_complete_command),
        );
    let machine_plugins = Router::new()
        .route(
            "/api/v1/plugins/{plugin_id}/ai/analyze",
            post(plugin_ai_analyze),
        )
        .route(
            "/api/v1/plugins/{plugin_id}/storage/uploads",
            post(plugin_storage_upload),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_machine_bearer,
        ));
    // Three groups, not a check inside each handler: a route has to be put
    // in one to exist at all, and where it goes is a decision somebody makes
    // rather than one they forget.
    //
    // `readable` is everything a viewer may see. `operable` is running the
    // fleet. `owned` is changing the system itself.
    let readable = Router::new()
        .route("/api/v1/fleet", get(fleet))
        .route("/api/v1/incidents", get(incidents))
        .route("/api/v1/cameras", get(cameras))
        .route("/api/v1/gateways", get(gateways))
        .route("/api/v1/commands/{command_id}", get(command_view))
        .route("/api/v1/rtc/config", get(rtc_config))
        .route(
            "/api/v1/cameras/{camera_id}/recordings",
            get(camera_timeline),
        )
        .route("/api/v1/health", get(fleet_health))
        .route("/api/v1/cameras/{camera_id}/health", get(camera_health))
        .route("/api/v1/events", get(fleet_events))
        .route(
            "/api/v1/cameras/{camera_id}/recording-policy",
            get(camera_recording_policy),
        )
        .route(
            "/api/v1/recordings/{recording_id}/playback",
            get(recording_playback),
        )
        .route("/api/v1/plugins", get(plugins_list))
        .route("/api/v1/plugins/{plugin_id}/health", get(plugin_health))
        .route("/api/v1/auth/logout", post(crate::auth::auth_logout))
        .route(
            "/api/v1/auth/password",
            post(crate::auth::auth_change_password),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_viewer,
        ));
    let operable = Router::new()
        .route("/api/v1/enrollments", post(create_enrollment))
        .route("/api/v1/sources", post(add_video_source).get(video_sources))
        .route(
            "/api/v1/sources/{source_id}/delete",
            post(delete_video_source),
        )
        .route("/api/v1/gateways/{gateway_id}/revoke", post(revoke_gateway))
        .route(
            "/api/v1/gateways/{gateway_id}/cameras/retire",
            post(retire_gateway_cameras),
        )
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
            post(create_recording),
        )
        .route("/api/v1/cameras/{camera_id}/clips", post(create_clip))
        .route(
            "/api/v1/cameras/{camera_id}/recording-policy",
            post(set_camera_recording_policy),
        )
        .route("/api/v1/events/test", post(test_event))
        .route("/api/v1/gateways/{gateway_id}/tunnel", post(open_tunnel))
        .route("/api/v1/tunnels/{session_id}/close", post(close_tunnel))
        .route("/api/v1/tunnels/{session_id}/{*path}", get(through_tunnel))
        .route(
            "/api/v1/plugins/{plugin_id}/storage/downloads",
            post(plugin_storage_download),
        )
        .route(
            "/api/v1/plugins/{plugin_id}/storage/delete",
            post(plugin_storage_delete),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_technician,
        ));
    let owned = Router::new()
        .route("/api/v1/audit", get(audit_entries))
        .route("/api/v1/audit/verify", get(audit_verify))
        .route("/api/v1/audit/export", get(audit_export))
        .route("/api/v1/users", get(list_users).post(create_user))
        .route("/api/v1/users/{user_id}", post(update_user))
        .route("/api/v1/plugins/reload", post(plugins_reload))
        .route(
            "/api/v1/plugins/registrations",
            post(create_plugin_registration),
        )
        .route(
            "/api/v1/plugins/registrations/{plugin_id}/delete",
            post(delete_plugin_registration),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_owner,
        ));
    open.merge(machine_plugins)
        .merge(readable)
        .merge(operable)
        .merge(owned)
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
    if let Err(err) = state.store.upsert_fleet_identity(&batch, Utc::now()).await {
        return store_status(err);
    }
    // History is the time between this report and the last one from the same
    // gateway, so the fold happens before the new batch replaces the old.
    let mut batches = state.camera_batches.write().await;
    let folded = crate::health::samples(batches.get(&batch.gateway_id), &batch);
    batches.insert(batch.gateway_id.clone(), batch);
    drop(batches);
    if let Err(err) = state.store.fold_health(&folded).await {
        // Losing an hour of history is not worth refusing telemetry over: the
        // fleet's current state is the more important half of this request.
        warn!(error = %err, "health history was not folded in");
    }
    StatusCode::NO_CONTENT
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

async fn authorized_gateway(headers: &HeaderMap, state: &AppState, gateway_id: &str) -> bool {
    let Some(token) = bearer_token(headers) else {
        return false;
    };
    // A revoked gateway is refused every credential, the bootstrap secret
    // included — revocation must actually evict. A store failure reads as
    // not-revoked so a database hiccup cannot 401 the whole fleet; the
    // per-token check below still fails closed on its own.
    if state
        .store
        .gateway_revoked(gateway_id)
        .await
        .unwrap_or(false)
    {
        return false;
    }
    if token == state.gateway_token.as_ref() {
        return true;
    }
    state
        .store
        .verify_gateway_token(gateway_id, token)
        .await
        .unwrap_or(false)
}

/// The layer on the plugin endpoints the edge gateway calls. They carry no
/// gateway id in the path, so any credential that identifies a gateway
/// passes: the shared bootstrap token or any enrolled gateway's own token.
/// A dashboard session cookie is deliberately not accepted — these are
/// machine endpoints. A store failure answers 401: fail closed.
async fn require_machine_bearer(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let authorized = match bearer_token(request.headers()) {
        Some(token) => {
            token == state.gateway_token.as_ref()
                || state
                    .store
                    .verify_any_gateway_token(token)
                    .await
                    .unwrap_or(false)
        }
        None => false,
    };
    if authorized {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// The live camera map from the gateway batches, deduped by newest
/// `last_seen`. Batches are keyed by gateway and never pruned, so a camera
/// that moved gateways exists in two batches until a restart — an arbitrary
/// winner here could show a false offline, or persist a false incident.
fn newest_camera_map(
    batches: &HashMap<String, CameraTelemetryBatch>,
) -> HashMap<String, CameraTelemetry> {
    let mut live: HashMap<String, CameraTelemetry> = HashMap::new();
    for camera in batches.values().flat_map(|batch| batch.cameras.iter()) {
        match live.get(&camera.camera_id) {
            Some(existing) if existing.last_seen >= camera.last_seen => {}
            _ => {
                live.insert(camera.camera_id.clone(), camera.clone());
            }
        }
    }
    live
}

fn store_status(err: crate::store::StoreError) -> StatusCode {
    match err {
        crate::store::StoreError::NotFound => StatusCode::NOT_FOUND,
        crate::store::StoreError::Gone => StatusCode::GONE,
        crate::store::StoreError::AlreadyExists => StatusCode::CONFLICT,
        crate::store::StoreError::Internal(err) => {
            warn!(error = %err, "store failure");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Fire-and-forget: the trail matters, but never enough to fail the request
/// it describes. Secrets never reach `detail` — that is the caller's oath.
async fn audit(state: &AppState, actor: &str, action: &str, subject: &str, detail: Option<&str>) {
    if let Err(err) = state
        .store
        .record_audit(Utc::now(), actor, action, subject, detail)
        .await
    {
        warn!(action, error = %err, "audit write failed");
    }
}

async fn create_enrollment(
    State(state): State<AppState>,
    Json(request): Json<EnrollmentRequest>,
) -> Result<Json<EnrollmentCreated>, StatusCode> {
    let enrollment_token = Uuid::new_v4().simple().to_string().to_uppercase();
    let expires_at = Utc::now() + chrono::Duration::minutes(30);
    state
        .store
        .create_enrollment(&enrollment_token, &request, expires_at)
        .await
        .map_err(store_status)?;
    audit(
        &state,
        "admin",
        "enrollment.created",
        &request.site_id,
        Some(&format!(
            "{} / {}",
            request.customer_name, request.site_name
        )),
    )
    .await;
    Ok(Json(EnrollmentCreated {
        enrollment_token,
        expires_at,
    }))
}

async fn gateway_enroll(
    State(state): State<AppState>,
    Json(request): Json<GatewayEnrollmentRequest>,
) -> Result<Json<GatewayEnrollmentResponse>, StatusCode> {
    // Look, resolve, then claim — an entitlement outage must not burn the token.
    let enrollment_request = state
        .store
        .enrollment_request(&request.enrollment_token, Utc::now())
        .await
        .map_err(store_status)?;
    let entitlement = state
        .entitlements
        .resolve(&enrollment_request.customer_id)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let enrollment_request = state
        .store
        .claim_enrollment(&request.enrollment_token, Utc::now())
        .await
        .map_err(store_status)?;
    let gateway_token = Uuid::new_v4().simple().to_string();
    state
        .store
        .enroll_gateway(&enrollment_request, &request, &gateway_token, Utc::now())
        .await
        .map_err(store_status)?;
    audit(
        &state,
        &format!("gateway:{}", request.gateway_id),
        "gateway.enrolled",
        &request.gateway_id,
        None,
    )
    .await;
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

async fn revoke_gateway(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let now = Utc::now();
    state
        .store
        .revoke_gateway(&gateway_id, now)
        .await
        .map_err(store_status)?;
    audit(&state, "admin", "gateway.revoked", &gateway_id, None).await;
    // A revoke is deliberate and audited, not an outage: its cameras leave the
    // incident timeline now. Failure only warns — the incident pass closes
    // whatever this misses.
    match state.store.fleet_cameras().await {
        Ok(cameras) => {
            for camera in cameras
                .iter()
                .filter(|camera| camera.gateway_id == gateway_id)
            {
                if let Err(err) = state.store.close_incident(&camera.id, now).await {
                    warn!(camera_id = %camera.id, error = %err, "revoke could not close an incident");
                }
            }
        }
        Err(err) => warn!(gateway_id = %gateway_id, error = %err, "revoke could not list cameras"),
    }
    // Drop its live presence so the dashboard stops showing a healthy
    // reporter it will never hear from again.
    state.gateways.write().await.remove(&gateway_id);
    state.camera_batches.write().await.remove(&gateway_id);
    Ok(StatusCode::NO_CONTENT)
}

/// What a source's address has to look like, per kind. A URL that carries a
/// credential is refused outright: that is the one place a password could reach
/// the control plane, and it belongs on the gateway instead.
fn validate_source(request: &VideoSourceRequest) -> Result<(), &'static str> {
    if request.name.trim().is_empty() || request.name.chars().count() > 80 {
        return Err("a source needs a name of at most 80 characters");
    }
    let address = request.address.trim();
    match request.kind {
        SourceKind::Rtsp => {
            let rest = address
                .strip_prefix("rtsp://")
                .or_else(|| address.strip_prefix("rtsps://"))
                .ok_or("an RTSP source's address has to start with rtsp:// or rtsps://")?;
            let authority = rest.split(['/', '?']).next().unwrap_or_default();
            if authority.is_empty() {
                return Err("that address names no host");
            }
            if authority.contains('@') {
                return Err(
                    "leave the credentials out of the address: set them on the gateway with                      relaysight-gateway-credentials",
                );
            }
        }
        SourceKind::Rtmp | SourceKind::Srt => {
            if address.is_empty() || address.chars().count() > 64 {
                return Err("a pushed source needs a stream key of at most 64 characters");
            }
            if !address
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err("a stream key can hold letters, digits, dot, dash and underscore");
            }
        }
    }
    Ok(())
}

type ApiError = (StatusCode, String);

async fn add_video_source(
    State(state): State<AppState>,
    Json(request): Json<VideoSourceRequest>,
) -> Result<(StatusCode, Json<VideoSource>), ApiError> {
    let refuse = |status: StatusCode| (status, String::new());
    if let Err(why) = validate_source(&request) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, why.to_owned()));
    }
    if !state
        .store
        .gateway_exists(&request.gateway_id)
        .await
        .map_err(|err| refuse(store_status(err)))?
    {
        return Err(refuse(StatusCode::NOT_FOUND));
    }
    let source = VideoSource {
        id: Uuid::new_v4().simple().to_string(),
        gateway_id: request.gateway_id,
        name: request.name.trim().to_owned(),
        kind: request.kind,
        address: request.address.trim().to_owned(),
        added_at: Utc::now(),
    };
    state
        .store
        .add_video_source(&source)
        .await
        .map_err(|err| refuse(store_status(err)))?;
    audit(
        &state,
        "admin",
        "source.added",
        &source.id,
        Some(&format!("{} {}", source.kind.as_str(), source.address)),
    )
    .await;
    Ok((StatusCode::CREATED, Json(source)))
}

async fn video_sources(
    State(state): State<AppState>,
) -> Result<Json<Vec<VideoSource>>, StatusCode> {
    state
        .store
        .video_sources()
        .await
        .map(Json)
        .map_err(store_status)
}

async fn delete_video_source(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    state
        .store
        .delete_video_source(&source_id)
        .await
        .map_err(store_status)?;
    audit(&state, "admin", "source.removed", &source_id, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// What a gateway polls: its own sources, and nobody else's.
/// What each of this gateway's cameras is recorded like. Polled beside the
/// source list, and a list for the same reason: state survives a restart,
/// and a removal arrives on its own.
async fn gateway_recording_policies(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<RecordingPolicy>>, StatusCode> {
    if !authorized_gateway(&headers, &state, &gateway_id).await {
        return Err(StatusCode::UNAUTHORIZED);
    }
    state
        .store
        .gateway_recording_policies(&gateway_id)
        .await
        .map(Json)
        .map_err(store_status)
}

/// A recording nobody asked for: a gateway keeping a window its policy told
/// it to keep. The media is already in storage; this is the index entry, and
/// without it the clip exists and nobody can find it.
async fn gateway_recording(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
    headers: HeaderMap,
    Json(mut manifest): Json<vms_domain::RecordingManifest>,
) -> StatusCode {
    if !authorized_gateway(&headers, &state, &gateway_id).await {
        return StatusCode::UNAUTHORIZED;
    }
    // A gateway may only file recordings under its own name, whatever the
    // manifest says.
    if manifest.gateway_id != gateway_id {
        return StatusCode::FORBIDDEN;
    }
    let retention = match state.store.recording_policy(&manifest.camera_id).await {
        Ok(Some(policy)) if policy.retention_days > 0 => i64::from(policy.retention_days),
        _ => state.default_retention_days,
    };
    manifest.delete_after =
        (retention > 0).then(|| manifest.ended_at + chrono::Duration::days(retention));
    match state.store.save_recording(&manifest).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(error) => store_status(error),
    }
}

/// A camera with no policy is not recorded, which is what `off` means, so
/// this answers for one that has never been given a policy rather than 404.
async fn camera_recording_policy(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<RecordingPolicy>, StatusCode> {
    refuse_unless_visible(&state, &who, &camera_id).await?;
    let gateway_id = gateway_for_camera(&state, &camera_id).await?;
    let stored = state
        .store
        .recording_policy(&camera_id)
        .await
        .map_err(store_status)?;
    Ok(Json(stored.unwrap_or(RecordingPolicy {
        camera_id,
        gateway_id,
        mode: RecordingMode::Off,
        keep: Vec::new(),
        retention_days: 0,
        storage_plugin_id: state.default_storage_plugin.to_string(),
        updated_at: Utc::now(),
    })))
}

async fn set_camera_recording_policy(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    Json(request): Json<RecordingPolicyRequest>,
) -> Result<Json<RecordingPolicy>, (StatusCode, String)> {
    let gateway_id = gateway_for_camera(&state, &camera_id)
        .await
        .map_err(|status| (status, String::new()))?;
    if let Err(why) = validate_keep_rules(&request.keep) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, why.to_owned()));
    }
    let policy = RecordingPolicy {
        camera_id: camera_id.clone(),
        gateway_id,
        mode: request.mode,
        keep: request.keep,
        // Ten years is not a retention policy, it is a mistake.
        retention_days: request.retention_days.min(3650),
        storage_plugin_id: plugin_for(
            &state,
            &camera_id,
            vms_plugin_sdk::PluginCapability::StorageBlob,
            &state.default_storage_plugin,
        )
        .await,
        updated_at: Utc::now(),
    };
    state
        .store
        .set_recording_policy(&policy)
        .await
        .map_err(|error| (store_status(error), String::new()))?;
    audit(
        &state,
        "admin",
        "recording.policy.set",
        &camera_id,
        Some(&format!(
            "{} with {} keep rule{}",
            policy.mode.as_str(),
            policy.keep.len(),
            if policy.keep.len() == 1 { "" } else { "s" }
        )),
    )
    .await;
    Ok(Json(policy))
}

/// A rule the gateway cannot act on is worse than no rule: it looks set.
fn validate_keep_rules(rules: &[KeepRule]) -> Result<(), &'static str> {
    for rule in rules {
        match rule {
            KeepRule::Schedule {
                from_minute,
                to_minute,
                ..
            } => {
                if *from_minute >= 1440 || *to_minute >= 1440 {
                    return Err("a schedule's times are minutes into the day, 0 to 1439");
                }
                if from_minute == to_minute {
                    return Err("a schedule window of no length keeps nothing");
                }
            }
            KeepRule::OnIncident {
                pre_roll_seconds,
                post_roll_seconds,
            } => {
                if *pre_roll_seconds == 0 && *post_roll_seconds == 0 {
                    return Err("an incident rule with no roll either side keeps nothing");
                }
            }
            KeepRule::OnAnalysis {
                plugin_id,
                every_seconds,
                ..
            } => {
                if plugin_id.trim().is_empty() {
                    return Err("an analysis rule needs the plugin that does the analysing");
                }
                if *every_seconds == 0 {
                    return Err("an analysis rule needs an interval");
                }
            }
        }
    }
    Ok(())
}

async fn gateway_video_sources(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<VideoSource>>, StatusCode> {
    if !authorized_gateway(&headers, &state, &gateway_id).await {
        return Err(StatusCode::UNAUTHORIZED);
    }
    state
        .store
        .gateway_video_sources(&gateway_id)
        .await
        .map(Json)
        .map_err(store_status)
}

/// Take a revoked gateway's cameras out of the roster. They come back on
/// their own if a gateway ever reports them again, which is what makes this
/// safe to offer: it tidies the fleet without deciding anything permanent.
async fn retire_gateway_cameras(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // A mistyped id is not a conflict, it is a miss — the same 404 a revoke
    // gives.
    if !state
        .store
        .gateway_exists(&gateway_id)
        .await
        .map_err(store_status)?
    {
        return Err(StatusCode::NOT_FOUND);
    }
    // Only after a revoke. On a working gateway this would empty a live site's
    // roster until the next telemetry batch refilled it.
    if !state
        .store
        .gateway_revoked(&gateway_id)
        .await
        .map_err(store_status)?
    {
        return Err(StatusCode::CONFLICT);
    }
    let now = Utc::now();
    // Close the incidents first. A retired camera is out of the roster the
    // incident pass walks, so an incident left open here would stay open
    // forever; failing before anything is stamped leaves the camera visible
    // and the retry harmless.
    let cameras = state.store.fleet_cameras().await.map_err(store_status)?;
    for camera in cameras
        .iter()
        .filter(|camera| camera.gateway_id == gateway_id)
    {
        state
            .store
            .close_incident(&camera.id, now)
            .await
            .map_err(store_status)?;
    }
    let retired = state
        .store
        .retire_gateway_cameras(&gateway_id, now)
        .await
        .map_err(store_status)?;
    audit(
        &state,
        "admin",
        "cameras.retired",
        &gateway_id,
        Some(&format!("{} cameras", retired.len())),
    )
    .await;
    Ok(Json(serde_json::json!({ "retired": retired.len() })))
}

async fn gateways(
    State(state): State<AppState>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<Vec<GatewayView>>, StatusCode> {
    let visible = visible_to(&state, &who).await;
    // The store is the roster, memory is the liveness — same split as /cameras.
    let mut views = state.store.gateway_views().await.map_err(store_status)?;
    let now = Utc::now();
    let live = state.gateways.read().await;
    for view in &mut views {
        if let Some(heartbeat) = live.get(&view.gateway_id) {
            view.online = (now - heartbeat.sent_at).num_seconds() <= state.stale_camera_seconds;
            view.last_seen = Some(match view.last_seen {
                Some(seen) => seen.max(heartbeat.sent_at),
                None => heartbeat.sent_at,
            });
            view.heartbeat = Some(heartbeat.clone());
        }
    }
    // A live gateway the store has not caught up with yet still shows.
    for (id, heartbeat) in live.iter() {
        if !views.iter().any(|view| view.gateway_id == *id) {
            views.push(GatewayView {
                gateway_id: id.clone(),
                site_id: heartbeat.site_id.clone(),
                site_name: String::new(),
                customer_name: String::new(),
                hostname: Some(heartbeat.hostname.clone()),
                version: Some(heartbeat.version.clone()),
                enrolled: false,
                revoked_at: None,
                last_seen: Some(heartbeat.sent_at),
                online: (now - heartbeat.sent_at).num_seconds() <= state.stale_camera_seconds,
                heartbeat: Some(heartbeat.clone()),
            });
        }
    }
    drop(live);
    if let Some(visible) = &visible {
        views.retain(|view| visible.sites.contains(&view.site_id));
    }
    views.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    Ok(Json(views))
}

async fn cameras(
    State(state): State<AppState>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<Vec<CameraTelemetry>>, StatusCode> {
    let visible = visible_to(&state, &who).await;
    // The store is the roster, memory is the liveness.
    let records = state.store.fleet_cameras().await.map_err(store_status)?;
    let now = Utc::now();
    let mut live = newest_camera_map(&*state.camera_batches.read().await);
    let mut values: Vec<CameraTelemetry> = records
        .into_iter()
        .map(|record| match live.remove(&record.id) {
            Some(camera) => camera,
            None => offline_camera(record),
        })
        .collect();
    // A live camera the store has not caught up with yet still shows.
    values.extend(live.into_values());
    for camera in &mut values {
        if (now - camera.last_seen).num_seconds() > state.stale_camera_seconds {
            camera.status = HealthStatus::Offline;
            camera.fps = None;
            camera.bitrate_kbps = None;
            camera.last_error = Some("gateway telemetry is stale".into());
        }
    }
    if let Some(visible) = &visible {
        values.retain(|camera| {
            visible.cameras.contains(&camera.camera_id) || visible.sites.contains(&camera.site_id)
        });
    }
    values.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.camera_id.cmp(&b.camera_id))
    });
    Ok(Json(values))
}

fn offline_camera(record: crate::store::CameraRecord) -> CameraTelemetry {
    CameraTelemetry {
        camera_id: record.id,
        gateway_id: record.gateway_id,
        site_id: record.site_id,
        name: record.name,
        status: HealthStatus::Offline,
        manufacturer: record.manufacturer,
        model: record.model,
        firmware: record.firmware,
        profile_name: None,
        codec: record.codec,
        width: record.width,
        height: record.height,
        fps: None,
        bitrate_kbps: None,
        packet_loss: 0,
        reconnects: 0,
        rtsp_endpoint: None,
        last_seen: record.last_seen,
        last_error: Some("no telemetry since the API restarted".into()),
    }
}

async fn fleet(
    State(state): State<AppState>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<FleetSnapshot>, StatusCode> {
    let scoped_to = who.customer_id.clone();
    // Identity from the store, status from memory.
    let orgs = state.store.fleet_identity().await.map_err(store_status)?;
    let batches = state.camera_batches.read().await;
    if orgs.is_empty() {
        // Nothing enrolled yet. A scoped user sees an empty fleet rather than
        // the demo one: their customer has nothing, and a demo would look
        // like somebody else's cameras.
        if scoped_to.is_some() {
            return Ok(Json(FleetSnapshot {
                generated_at: Utc::now(),
                source: FleetSource::Live,
                customers: Vec::new(),
            }));
        }
        if batches.values().any(|batch| !batch.cameras.is_empty()) {
            return Ok(Json(live_fleet(
                batches.values().cloned().collect(),
                state.stale_camera_seconds,
            )));
        }
        return Ok(Json(demo_fleet()));
    }
    let now = Utc::now();
    let live = newest_camera_map(&batches);
    let customers = orgs
        .into_iter()
        // A customer's own login sees their own fleet. Everybody else's is
        // not hidden on the screen: it never leaves here.
        .filter(|org| scoped_to.as_ref().is_none_or(|scope| &org.id == scope))
        .map(|org| CustomerSummary {
            id: org.id.clone(),
            name: org.name,
            sites: org
                .sites
                .into_iter()
                .map(|site| SiteSummary {
                    id: site.id,
                    customer_id: org.id.clone(),
                    name: site.name,
                    city: site.city,
                    cameras: site
                        .cameras
                        .into_iter()
                        .map(|record| {
                            let (status, fps, bitrate, last_seen) = match live.get(&record.id) {
                                Some(camera)
                                    if (now - camera.last_seen).num_seconds()
                                        <= state.stale_camera_seconds =>
                                {
                                    (
                                        camera.status.clone(),
                                        camera.fps,
                                        camera.bitrate_kbps,
                                        camera.last_seen,
                                    )
                                }
                                _ => (HealthStatus::Offline, None, None, record.last_seen),
                            };
                            CameraSummary {
                                id: record.id,
                                name: record.name,
                                site_id: record.site_id,
                                status,
                                fps,
                                bitrate_kbps: bitrate,
                                last_seen,
                            }
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    Ok(Json(FleetSnapshot {
        generated_at: Utc::now(),
        source: FleetSource::Live,
        customers,
    }))
}

async fn plugins_list(State(state): State<AppState>) -> Json<Vec<RegisteredPlugin>> {
    Json(state.plugins.list().await)
}

async fn plugins_reload(
    State(state): State<AppState>,
) -> Result<Json<Vec<RegisteredPlugin>>, StatusCode> {
    reload_plugins(&state).await?;
    Ok(Json(state.plugins.list().await))
}

/// Load the plugins on disk, then the ones somebody connected from the
/// dashboard. A row wins over a file with the same id.
async fn reload_plugins(state: &AppState) -> Result<(), StatusCode> {
    let stored = state
        .store
        .plugin_registrations()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| vms_plugin_sdk::PluginRegistration {
            endpoint: row.endpoint,
            placement: match row.placement.as_str() {
                "edge" => vms_plugin_sdk::PluginPlacement::Edge,
                "either" => vms_plugin_sdk::PluginPlacement::Either,
                _ => vms_plugin_sdk::PluginPlacement::ControlPlane,
            },
            enabled: row.enabled,
            token_env: row.token_env,
            token_file: row.token_file,
            manifest: None,
        })
        .collect();
    state
        .plugins
        .reload_with(state.plugin_dir.as_ref(), stored)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Connect a plugin: the control plane asks it what it is, and keeps the
/// registration if it answers something this build can speak to.
async fn create_plugin_registration(
    State(state): State<AppState>,
    Extension(who): Extension<crate::store::SessionUser>,
    Json(request): Json<crate::store::StoredRegistration>,
) -> Result<Json<Vec<RegisteredPlugin>>, (StatusCode, String)> {
    let endpoint = request.endpoint.trim().to_owned();
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "a plugin endpoint is an http or https URL".into(),
        ));
    }
    // Ask it who it is before writing anything down: a registration for a
    // plugin that never answers is a row nobody can explain later.
    let probe = vms_plugin_sdk::PluginRegistration {
        endpoint: endpoint.clone(),
        placement: vms_plugin_sdk::PluginPlacement::ControlPlane,
        enabled: true,
        token_env: request.token_env.clone(),
        token_file: request.token_file.clone(),
        manifest: None,
    };
    let manifest = state
        .plugins
        .describe(&probe)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;

    let row = crate::store::StoredRegistration {
        plugin_id: manifest.id.clone(),
        endpoint,
        placement: request.placement,
        enabled: true,
        token_env: request.token_env,
        token_file: request.token_file,
        customer_id: request.customer_id,
    };
    state
        .store
        .save_plugin_registration(&row, Utc::now())
        .await
        .map_err(|err| (store_status(err), String::new()))?;
    audit(&state, &who.email, "plugin.connected", &row.plugin_id, None).await;
    reload_plugins(&state)
        .await
        .map_err(|status| (status, String::new()))?;
    Ok(Json(state.plugins.list().await))
}

async fn delete_plugin_registration(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<Vec<RegisteredPlugin>>, StatusCode> {
    state
        .store
        .delete_plugin_registration(&plugin_id)
        .await
        .map_err(store_status)?;
    audit(&state, &who.email, "plugin.disconnected", &plugin_id, None).await;
    reload_plugins(&state).await?;
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

async fn rtc_config(State(state): State<AppState>) -> Json<RtcConfigResponse> {
    // Minted per request, never stored. See turn.rs for why static credentials
    // in the ICE config are the same as publishing them.
    Json(RtcConfigResponse {
        ice_servers: state.rtc.ice_servers(now_unix(), "browser"),
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
    // Credentials are minted for this gateway and this moment, so a relay log can
    // be tied back to a site without a second lookup.
    let ice_servers = state.rtc.ice_servers(now_unix(), &gateway_id);
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
            ice_servers,
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
    let ai_plugin_id = match request.ai_plugin_id {
        Some(chosen) => chosen,
        None => {
            plugin_for(
                &state,
                &camera_id,
                vms_plugin_sdk::PluginCapability::AiAnalyze,
                &state.default_ai_plugin,
            )
            .await
        }
    };
    let storage_plugin_id = match request.storage_plugin_id {
        Some(chosen) => chosen,
        None => {
            plugin_for(
                &state,
                &camera_id,
                vms_plugin_sdk::PluginCapability::StorageBlob,
                &state.default_storage_plugin,
            )
            .await
        }
    };
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
    let storage_plugin_id = match request.storage_plugin_id {
        Some(chosen) => chosen,
        None => {
            plugin_for(
                &state,
                &camera_id,
                vms_plugin_sdk::PluginCapability::StorageBlob,
                &state.default_storage_plugin,
            )
            .await
        }
    };

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

/// How long a window of history to answer with, in days.
#[derive(Debug, Clone, serde::Deserialize)]
struct HealthWindow {
    #[serde(default = "default_health_days")]
    days: i64,
}

fn default_health_days() -> i64 {
    7
}

/// One camera's history, hour by hour, with the summary the dashboard shows.
async fn camera_health(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    Extension(who): Extension<crate::store::SessionUser>,
    Query(window): Query<HealthWindow>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    refuse_unless_visible(&state, &who, &camera_id).await?;
    let days = window.days.clamp(1, 90);
    let since = Utc::now() - chrono::Duration::days(days);
    let hours = state
        .store
        .camera_health(&camera_id, since)
        .await
        .map_err(store_status)?;
    Ok(Json(health_answer(days, hours)))
}

/// The same across every camera, which is what the overview needs.
async fn fleet_health(
    State(state): State<AppState>,
    Query(window): Query<HealthWindow>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let days = window.days.clamp(1, 90);
    let since = Utc::now() - chrono::Duration::days(days);
    let hours = state
        .store
        .fleet_health(since)
        .await
        .map_err(store_status)?;
    Ok(Json(health_answer(days, hours)))
}

/// The summary and the hours behind it.
///
/// `uptime_percent` is over time that was actually reported, and
/// `covered_percent` says how much of the window that was. A camera nobody
/// heard from for six days is not 100% up; it is 100% of a day, and the
/// second number is what stops the first from lying.
fn health_answer(days: i64, hours: Vec<crate::store::HealthHour>) -> serde_json::Value {
    let counted: i64 = hours.iter().map(|hour| hour.counted_seconds()).sum();
    let healthy: i64 = hours.iter().map(|hour| hour.healthy_seconds).sum();
    let offline: i64 = hours.iter().map(|hour| hour.offline_seconds).sum();
    let warning: i64 = hours.iter().map(|hour| hour.warning_seconds).sum();
    let reconnects: i64 = hours.iter().map(|hour| hour.reconnects).sum();
    let window = days * 24 * 3_600;
    serde_json::json!({
        "days": days,
        "healthy_seconds": healthy,
        "warning_seconds": warning,
        "offline_seconds": offline,
        "counted_seconds": counted,
        "reconnects": reconnects,
        "uptime_percent": (counted > 0).then(|| {
            (healthy as f64 * 1_000.0 / counted as f64).round() / 10.0
        }),
        "covered_percent": (window > 0)
            .then(|| (counted as f64 * 1_000.0 / window as f64).round() / 10.0),
        "hours": hours,
    })
}

/// Which plugin should serve this camera.
///
/// A registration can name a customer, and that scope has until now been
/// recorded and shown and nothing else. A camera belonging to that customer
/// uses their plugin — their bucket, their model — and everybody else uses
/// the default.
async fn plugin_for(
    state: &AppState,
    camera_id: &str,
    capability: vms_plugin_sdk::PluginCapability,
    default: &str,
) -> String {
    let Some(customer_id) = customer_of_camera(state, camera_id).await else {
        return default.to_owned();
    };
    let scoped: Vec<String> = state
        .store
        .plugin_registrations()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.enabled && row.customer_id.as_deref() == Some(customer_id.as_str()))
        .map(|row| row.plugin_id)
        .collect();
    if scoped.is_empty() {
        return default.to_owned();
    }
    // Registered for that customer, and actually able to do the thing: a
    // customer's own storage plugin is not their inference plugin.
    for plugin in state.plugins.list().await {
        if scoped.contains(&plugin.manifest.id)
            && plugin.manifest.capabilities.contains(&capability)
        {
            return plugin.manifest.id;
        }
    }
    default.to_owned()
}

/// Which customer a camera belongs to, by way of its site.
async fn customer_of_camera(state: &AppState, camera_id: &str) -> Option<String> {
    let organizations = state.store.fleet_identity().await.ok()?;
    organizations.into_iter().find_map(|organization| {
        organization
            .sites
            .iter()
            .any(|site| site.cameras.iter().any(|camera| camera.id == camera_id))
            .then_some(organization.id)
    })
}

/// What one session may see, or `None` for everything.
///
/// A user scoped to a customer sees that customer's sites and nothing else.
/// Anything outside answers 404 rather than 403: a customer should not learn
/// that another customer exists by being told they may not look.
struct Visible {
    sites: std::collections::HashSet<String>,
    cameras: std::collections::HashSet<String>,
}

async fn visible_to(state: &AppState, who: &crate::store::SessionUser) -> Option<Visible> {
    let customer_id = who.customer_id.as_deref()?;
    let organizations = state.store.fleet_identity().await.unwrap_or_default();
    let mut visible = Visible {
        sites: std::collections::HashSet::new(),
        cameras: std::collections::HashSet::new(),
    };
    for organization in organizations
        .into_iter()
        .filter(|organization| organization.id == customer_id)
    {
        for site in organization.sites {
            visible.sites.insert(site.id.clone());
            visible
                .cameras
                .extend(site.cameras.into_iter().map(|camera| camera.id));
        }
    }
    Some(visible)
}

/// Has anybody edited the audit log?
///
/// The answer is a number and a place, not a yes: rows written before the
/// chain existed cannot be checked, and saying they are sound would be the
/// dishonest reading.
async fn audit_verify(
    State(state): State<AppState>,
) -> Result<Json<crate::store::AuditIntegrity>, StatusCode> {
    let integrity = state.store.verify_audit().await.map_err(store_status)?;
    if let Some(broken) = &integrity.broken_at {
        warn!(row = %broken, "the audit chain is broken; somebody changed the log");
    }
    // The head hash in the journal is what makes a wholesale rewrite of the
    // table detectable: a rewritten chain verifies against itself and not
    // against what was printed yesterday.
    if let Some(head) = &integrity.head {
        info!(head = %head, checked = integrity.checked, "audit chain verified");
    }
    Ok(Json(integrity))
}

/// The audit log as CSV, hashes included, so an auditor can check the chain
/// outside this system rather than taking its word for it.
async fn audit_export(
    State(state): State<AppState>,
) -> Result<axum::response::Response, StatusCode> {
    let entries = state
        .store
        .audit_entries(100_000)
        .await
        .map_err(store_status)?;
    let mut csv = String::from("at,actor,action,subject,detail\n");
    for entry in entries.iter().rev() {
        csv.push_str(&format!(
            "{},{},{},{},{}\n",
            csv_field(&entry.at.to_rfc3339()),
            csv_field(&entry.actor),
            csv_field(&entry.action),
            csv_field(&entry.subject),
            csv_field(entry.detail.as_deref().unwrap_or("")),
        ));
    }
    use axum::response::IntoResponse as _;
    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"audit.csv\"",
            ),
        ],
        csv,
    )
        .into_response())
}

/// A field a spreadsheet will read back as what it was.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// The device addresses this gateway has actually told us about.
///
/// A tunnel reaches those and nothing else. Anything looser is a proxy into
/// somebody's network wearing a camera's name, and the distance between the
/// two is one typo in a host field.
async fn tunnel_hosts(state: &AppState, gateway_id: &str) -> std::collections::HashSet<String> {
    let mut hosts = std::collections::HashSet::new();
    let host_of = |value: &str| -> Option<String> {
        let value = value.trim();
        let after_scheme = value.split("://").nth(1).unwrap_or(value);
        let authority = after_scheme.split(['/', '?']).next()?;
        let authority = authority.rsplit('@').next()?;
        let host = authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
            .trim_matches(['[', ']']);
        (!host.is_empty()).then(|| host.to_owned())
    };
    if let Some(batch) = state.camera_batches.read().await.get(gateway_id) {
        for camera in &batch.cameras {
            if let Some(endpoint) = &camera.rtsp_endpoint
                && let Some(host) = host_of(endpoint)
            {
                hosts.insert(host);
            }
        }
    }
    if let Ok(sources) = state.store.gateway_video_sources(gateway_id).await {
        for source in sources {
            if let Some(host) = host_of(&source.address) {
                hosts.insert(host);
            }
        }
    }
    hosts
}

/// Open a way to one device's own web page, for a few minutes.
async fn open_tunnel(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
    Extension(who): Extension<crate::store::SessionUser>,
    Json(request): Json<vms_domain::TunnelRequest>,
) -> Result<Json<vms_domain::TunnelSession>, (StatusCode, String)> {
    // A scoped login is a viewer and cannot reach this route at all; this is
    // the belt to that braces.
    if who.customer_id.is_some() {
        return Err((
            StatusCode::FORBIDDEN,
            "a scoped login may not open a tunnel".into(),
        ));
    }
    let host = request.host.trim().to_owned();
    if !tunnel_hosts(&state, &gateway_id).await.contains(&host) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "that gateway has never reported a device at that address".into(),
        ));
    }
    if request.port == 0 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "port 0 reaches nothing".into(),
        ));
    }
    let now = Utc::now();
    // An hour is already generous for looking at a device's settings page.
    let minutes = i64::from(request.minutes.clamp(1, 60));
    let session = vms_domain::TunnelSession {
        id: Uuid::new_v4().to_string(),
        gateway_id: gateway_id.clone(),
        host: host.clone(),
        port: request.port,
        opened_by: who.email.clone(),
        opened_at: now,
        expires_at: now + chrono::Duration::minutes(minutes),
        requests: 0,
    };
    state
        .tunnels
        .write()
        .await
        .insert(session.id.clone(), session.clone());
    audit(
        &state,
        &who.email,
        "tunnel.opened",
        &format!("{gateway_id} {host}:{}", request.port),
        Some(&format!("{minutes} minutes")),
    )
    .await;
    Ok(Json(session))
}

async fn close_tunnel(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<StatusCode, StatusCode> {
    let session = state.tunnels.write().await.remove(&session_id);
    let Some(session) = session else {
        return Err(StatusCode::NOT_FOUND);
    };
    audit(
        &state,
        &who.email,
        "tunnel.closed",
        &format!("{} {}:{}", session.gateway_id, session.host, session.port),
        Some(&format!("{} requests", session.requests)),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// One request through an open tunnel, answered by the gateway.
async fn through_tunnel(
    State(state): State<AppState>,
    Path((session_id, path)): Path<(String, String)>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse as _;
    let session = {
        let mut tunnels = state.tunnels.write().await;
        let Some(session) = tunnels.get_mut(&session_id) else {
            return Err((StatusCode::NOT_FOUND, String::new()));
        };
        if session.expires_at < Utc::now() {
            tunnels.remove(&session_id);
            return Err((StatusCode::GONE, "that tunnel has expired".into()));
        }
        session.requests += 1;
        session.clone()
    };
    if who.customer_id.is_some() {
        return Err((StatusCode::FORBIDDEN, String::new()));
    }

    let call = vms_domain::TunnelCall {
        id: Uuid::new_v4().to_string(),
        session_id: session.id.clone(),
        host: session.host.clone(),
        port: session.port,
        method: "GET".into(),
        path: if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        },
    };
    state
        .tunnel_calls
        .write()
        .await
        .entry(session.gateway_id.clone())
        .or_default()
        .push_back(call.clone());

    // The gateway is holding a long poll; waiting here is waiting for one
    // round trip on a LAN, not for a poll interval.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if let Some(answer) = state.tunnel_answers.write().await.remove(&call.id) {
            if let Some(error) = answer.error {
                return Err((StatusCode::BAD_GATEWAY, error));
            }
            let body = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                answer.body_base64.as_bytes(),
            )
            .map_err(|_| (StatusCode::BAD_GATEWAY, "unreadable answer".to_string()))?;
            let status =
                StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let content_type = answer
                .content_type
                .unwrap_or_else(|| "application/octet-stream".into());
            return Ok((
                status,
                [(axum::http::header::CONTENT_TYPE, content_type)],
                body,
            )
                .into_response());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err((
        StatusCode::GATEWAY_TIMEOUT,
        "the gateway did not answer; it may not have tunnelling enabled".into(),
    ))
}

/// The gateway's side: hold here until there is something to fetch.
async fn next_tunnel_call(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Option<vms_domain::TunnelCall>>, StatusCode> {
    if !authorized_gateway(&headers, &state, &gateway_id).await {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if let Some(call) = state
            .tunnel_calls
            .write()
            .await
            .get_mut(&gateway_id)
            .and_then(|queue| queue.pop_front())
        {
            return Ok(Json(Some(call)));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Nothing to do. The gateway asks again; an idle tunnel costs one held
    // request and no polling.
    Ok(Json(None))
}

async fn answer_tunnel_call(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
    headers: HeaderMap,
    Json(answer): Json<vms_domain::TunnelAnswer>,
) -> StatusCode {
    if !authorized_gateway(&headers, &state, &gateway_id).await {
        return StatusCode::UNAUTHORIZED;
    }
    state
        .tunnel_answers
        .write()
        .await
        .insert(answer.id.clone(), answer);
    StatusCode::NO_CONTENT
}

/// Everybody who can get in, and what they may do.
async fn list_users(State(state): State<AppState>) -> Result<Json<Vec<UserView>>, StatusCode> {
    state.store.users().await.map(Json).map_err(store_status)
}

async fn create_user(
    State(state): State<AppState>,
    Extension(who): Extension<crate::store::SessionUser>,
    Json(request): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserView>), (StatusCode, String)> {
    let email = request.email.trim().to_lowercase();
    if !email.contains('@') || email.len() < 3 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "that is not an email address".into(),
        ));
    }
    if request.password.len() < crate::auth::MIN_PASSWORD_LEN {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "a password needs at least {} characters",
                crate::auth::MIN_PASSWORD_LEN
            ),
        ));
    }
    if request.customer_id.is_some() && request.role != vms_domain::Role::Viewer {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "a login scoped to a customer is a viewer: scoping only covers reading".into(),
        ));
    }
    let hash = crate::auth::hash_password(&request.password)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, String::new()))?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    state
        .store
        .create_user(
            &id,
            &email,
            &hash,
            request.role,
            request.customer_id.as_deref(),
            now,
        )
        .await
        .map_err(|err| (store_status(err), String::new()))?;
    audit(
        &state,
        &who.email,
        "user.created",
        &email,
        Some(request.role.as_str()),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(UserView {
            id,
            email,
            role: request.role,
            customer_id: request.customer_id,
            created_at: now,
            disabled_at: None,
        }),
    ))
}

/// Change somebody: their role, their password, whether they are turned off,
/// which customer they are scoped to.
async fn update_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Extension(who): Extension<crate::store::SessionUser>,
    Json(request): Json<UpdateUserRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    // An owner locking themselves out of their own control plane is a support
    // call nobody can answer.
    if who.id == user_id && (request.disabled == Some(true) || request.role.is_some()) {
        return Err((
            StatusCode::CONFLICT,
            "change somebody else's role, or ask another owner to change yours".into(),
        ));
    }
    let hash = match &request.password {
        Some(password) if password.len() < crate::auth::MIN_PASSWORD_LEN => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "a password needs at least {} characters",
                    crate::auth::MIN_PASSWORD_LEN
                ),
            ));
        }
        Some(password) => Some(
            crate::auth::hash_password(password)
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, String::new()))?,
        ),
        None => None,
    };
    state
        .store
        .update_user(
            &user_id,
            request.role,
            hash.as_deref(),
            request.disabled,
            request.customer_id.as_ref().map(|id| id.as_deref()),
            Utc::now(),
        )
        .await
        .map_err(|err| (store_status(err), String::new()))?;
    // A new password or a disabled account has to take effect now, not
    // whenever the session happens to expire.
    if hash.is_some() || request.disabled == Some(true) {
        let _ = state.store.delete_sessions_of(&user_id).await;
    }
    audit(&state, &who.email, "user.updated", &user_id, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// A camera that is not this user's is not found. 404 rather than 403: a
/// customer should not learn another customer's camera ids by being told they
/// may not look at them.
async fn refuse_unless_visible(
    state: &AppState,
    who: &crate::store::SessionUser,
    camera_id: &str,
) -> Result<(), StatusCode> {
    match visible_to(state, who).await {
        Some(visible) if !visible.cameras.contains(camera_id) => Err(StatusCode::NOT_FOUND),
        _ => Ok(()),
    }
}

/// What has been raised lately and whether each sink took it. An alert
/// nobody delivered is exactly what an operator needs to see, so the failure
/// travels with the event rather than staying in a log.
async fn fleet_events(
    State(state): State<AppState>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<Vec<crate::store::EventView>>, StatusCode> {
    let visible = visible_to(&state, &who).await;
    let mut events = state.store.recent_events(100).await.map_err(store_status)?;
    if let Some(visible) = &visible {
        events.retain(|entry| visible.sites.contains(&entry.event.site_id));
    }
    Ok(Json(events))
}

/// Send a test event, because the first question anyone asks about alerting
/// is whether it is wired up at all.
async fn test_event(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let sinks = state.plugins.event_sinks().await;
    let event = vms_plugin_sdk::FleetEvent {
        id: Uuid::new_v4().to_string(),
        kind: vms_plugin_sdk::FleetEventKind::Test,
        severity: vms_plugin_sdk::EventSeverity::Info,
        occurred_at: Utc::now(),
        customer_id: String::new(),
        site_id: String::new(),
        site_name: String::new(),
        gateway_id: None,
        camera_id: None,
        title: "Test alert from the dashboard".into(),
        detail: Some("Nothing is wrong. Somebody pressed the button.".into()),
        metadata: serde_json::json!({}),
    };
    let event_id = event.id.clone();
    raise_event(&state, event).await;
    audit(&state, "admin", "alert.test", &event_id, None).await;
    // Sent now rather than on the next minute's pass: whoever pressed the
    // button is watching for it.
    delivery_pass(&state, Utc::now()).await;
    Ok(Json(
        serde_json::json!({"event_id": event_id, "sinks": sinks.len()}),
    ))
}

/// Keep what already happened. The gateway answers out of its ring buffer,
/// or says how far back the ring goes; nothing is dialled either way.
async fn create_clip(
    State(state): State<AppState>,
    Path(camera_id): Path<String>,
    Json(request): Json<ClipRequest>,
) -> Result<(StatusCode, Json<CommandAccepted>), StatusCode> {
    let seconds = request.seconds.clamp(5, 3600);
    let storage_plugin_id = match request.storage_plugin_id {
        Some(chosen) => chosen,
        None => {
            plugin_for(
                &state,
                &camera_id,
                vms_plugin_sdk::PluginCapability::StorageBlob,
                &state.default_storage_plugin,
            )
            .await
        }
    };
    let gateway_id = gateway_for_camera(&state, &camera_id).await?;

    let now = Utc::now();
    let command = GatewayCommand {
        id: Uuid::new_v4().to_string(),
        gateway_id,
        created_at: now,
        expires_at: now + chrono::Duration::minutes(5),
        kind: GatewayCommandKind::SaveClip {
            camera_id,
            seconds,
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
        if state.store.save_recording(recording).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
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
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<RecordingTimeline>, StatusCode> {
    refuse_unless_visible(&state, &who, &camera_id).await?;
    // Already newest-first from the store.
    let recordings = state
        .store
        .camera_recordings(&camera_id)
        .await
        .map_err(store_status)?;
    Ok(Json(RecordingTimeline {
        camera_id,
        recordings,
    }))
}

async fn incidents(
    State(state): State<AppState>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<Vec<IncidentView>>, StatusCode> {
    let visible = visible_to(&state, &who).await;
    // Open first, newest-closed after; 200 is plenty for a screen.
    let mut incidents = state.store.incidents(200).await.map_err(store_status)?;
    if let Some(visible) = &visible {
        incidents.retain(|incident| visible.sites.contains(&incident.site_id));
    }
    Ok(Json(incidents))
}

async fn audit_entries(State(state): State<AppState>) -> Result<Json<Vec<AuditView>>, StatusCode> {
    state
        .store
        .audit_entries(500)
        .await
        .map(Json)
        .map_err(store_status)
}

async fn recording_playback(
    State(state): State<AppState>,
    Path(recording_id): Path<String>,
    Extension(who): Extension<crate::store::SessionUser>,
) -> Result<Json<PlaybackManifest>, StatusCode> {
    let recording = state
        .store
        .recording(&recording_id)
        .await
        .map_err(store_status)?;
    refuse_unless_visible(&state, &who, &recording.camera_id).await?;
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
    // Whether each gateway was reporting last time round. In memory, because
    // a restart should not announce an outage it never saw start.
    let mut reporting: HashMap<String, bool> = HashMap::new();
    loop {
        interval.tick().await;
        retention_pass(&state).await;
        incident_pass(&state).await;
        gateway_pass(&state, &mut reporting).await;
        delivery_pass(&state, Utc::now()).await;
    }
}

/// Notice a gateway going quiet, which nothing did before: the gateways
/// screen works out `online` when it is read, so a gateway could stop
/// reporting at midnight and the first anyone knew was the next time someone
/// opened the page.
///
/// A gateway going quiet is worse news than a camera going quiet — it takes
/// every camera behind it with it — and it is the one outage the site cannot
/// report itself.
async fn gateway_pass(state: &AppState, reporting: &mut HashMap<String, bool>) {
    if state.up_since.elapsed() < state.incident_grace {
        return;
    }
    let views = match state.store.gateway_views().await {
        Ok(views) => views,
        Err(err) => {
            warn!(error = %err, "gateway pass could not list gateways");
            return;
        }
    };
    let now = Utc::now();
    let live = state.gateways.read().await.clone();
    let mut present = std::collections::HashSet::new();

    for view in views {
        // A revoked gateway is not an outage, it is a decision.
        if view.revoked_at.is_some() {
            reporting.remove(&view.gateway_id);
            continue;
        }
        present.insert(view.gateway_id.clone());
        let last_seen = live
            .get(&view.gateway_id)
            .map(|heartbeat| heartbeat.sent_at)
            .max(view.last_seen);
        let online =
            last_seen.is_some_and(|seen| (now - seen).num_seconds() <= state.stale_camera_seconds);
        let previous = reporting.insert(view.gateway_id.clone(), online);
        // First sight of a gateway says nothing: this process has no idea
        // whether that state is new.
        let Some(previous) = previous else { continue };
        if previous == online {
            continue;
        }
        // Name the box the way the screen does: the heartbeat knows the
        // hostname even when the roster has not caught up.
        let name = live
            .get(&view.gateway_id)
            .map(|heartbeat| heartbeat.hostname.clone())
            .or_else(|| view.hostname.clone())
            .unwrap_or_else(|| view.gateway_id.clone());
        let site = if view.site_name.is_empty() {
            view.site_id.clone()
        } else {
            view.site_name.clone()
        };
        raise_event(
            state,
            vms_plugin_sdk::FleetEvent {
                id: Uuid::new_v4().to_string(),
                kind: if online {
                    vms_plugin_sdk::FleetEventKind::GatewayRecovered
                } else {
                    vms_plugin_sdk::FleetEventKind::GatewayOffline
                },
                severity: if online {
                    vms_plugin_sdk::EventSeverity::Info
                } else {
                    vms_plugin_sdk::EventSeverity::Critical
                },
                occurred_at: now,
                customer_id: String::new(),
                site_id: view.site_id.clone(),
                site_name: site.clone(),
                gateway_id: Some(view.gateway_id.clone()),
                camera_id: None,
                title: format!(
                    "Gateway {name} {} at {site}",
                    if online {
                        "is reporting again"
                    } else {
                        "stopped reporting"
                    }
                ),
                detail: last_seen.map(|seen| format!("last heard from at {}", seen.to_rfc3339())),
                metadata: serde_json::json!({"cameras": view.heartbeat.as_ref().map(|h| h.cameras_seen)}),
            },
        )
        .await;
    }
    // A gateway nobody lists any more cannot come back as a surprise.
    reporting.retain(|gateway_id, _| present.contains(gateway_id));
}

/// How long to wait before trying a sink again, by how many times it has
/// already refused. After the last one the event is given up on and the
/// reason stays on the row.
pub(crate) fn retry_after(attempts: i64) -> Option<Duration> {
    match attempts {
        0 => Some(Duration::from_secs(10)),
        1 => Some(Duration::from_secs(60)),
        2 => Some(Duration::from_secs(300)),
        3 => Some(Duration::from_secs(1800)),
        // Four refusals over half an hour is a sink that is not coming back
        // in time to matter. An alert nobody can deliver is not worth
        // retrying until the end of days.
        _ => None,
    }
}

/// Hand due events to their sinks.
async fn delivery_pass(state: &AppState, now: DateTime<Utc>) {
    let due = match state.store.due_deliveries(now, 50).await {
        Ok(due) => due,
        Err(err) => {
            warn!(error = %err, "could not read the alert outbox");
            return;
        }
    };
    for delivery in due {
        let request = vms_plugin_sdk::EventDeliveryRequest {
            context: vms_plugin_sdk::PluginInvocationContext {
                site_id: Some(delivery.event.site_id.clone()),
                camera_id: delivery.event.camera_id.clone(),
                trace_id: Some(delivery.event.id.clone()),
                ..Default::default()
            },
            event: delivery.event.clone(),
        };
        let outcome = state
            .plugins
            .deliver_event(&delivery.plugin_id, &request)
            .await;
        let result = match outcome {
            Ok(answer) => {
                if answer.delivered {
                    info!(
                        event_id = %delivery.event.id, plugin_id = %delivery.plugin_id,
                        "an alert was delivered"
                    );
                } else {
                    info!(
                        event_id = %delivery.event.id, plugin_id = %delivery.plugin_id,
                        detail = ?answer.detail, "a sink declined an alert"
                    );
                }
                state
                    .store
                    .delivery_succeeded(
                        &delivery.event.id,
                        &delivery.plugin_id,
                        !answer.delivered,
                        now,
                        answer.detail.as_deref(),
                    )
                    .await
            }
            Err(error) => {
                let next = retry_after(delivery.attempts).map(|wait| {
                    now + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::minutes(1))
                });
                if next.is_none() {
                    warn!(
                        event_id = %delivery.event.id, plugin_id = %delivery.plugin_id, %error,
                        "an alert was given up on"
                    );
                }
                state
                    .store
                    .delivery_failed(
                        &delivery.event.id,
                        &delivery.plugin_id,
                        &error.to_string(),
                        next,
                    )
                    .await
            }
        };
        if let Err(err) = result {
            warn!(error = %err, "the alert outbox could not be updated");
        }
    }
}

/// Where a site is, by site id: enough to write a sentence a human can read.
struct Place {
    customer_id: String,
    site_name: String,
}

async fn places_by_site(state: &AppState) -> HashMap<String, Place> {
    let mut places = HashMap::new();
    let Ok(organizations) = state.store.fleet_identity().await else {
        // Without names an event still goes out, naming ids. Silence would be
        // the worse answer.
        return places;
    };
    for organization in organizations {
        for site in organization.sites {
            places.insert(
                site.id.clone(),
                Place {
                    customer_id: organization.id.clone(),
                    site_name: site.name.clone(),
                },
            );
        }
    }
    places
}

/// Write an event down and queue it for every sink registered right now.
///
/// A sink registered later gets nothing: alerts are about now. Failure here
/// is logged and never fatal — an alert must not be able to break the thing
/// it is reporting on.
async fn raise_event(state: &AppState, event: vms_plugin_sdk::FleetEvent) {
    let sinks = state.plugins.event_sinks().await;
    if let Err(err) = state.store.record_event(&event, &sinks, Utc::now()).await {
        warn!(error = %err, kind = event.kind.as_str(), "an event was not written down");
        return;
    }
    info!(
        event_id = %event.id, kind = event.kind.as_str(), sinks = sinks.len(),
        title = %event.title, "raised an event"
    );
}

async fn retention_pass(state: &AppState) {
    if let Err(err) = state.store.delete_expired_sessions(Utc::now()).await {
        warn!(error = %err, "retention could not sweep expired sessions");
    }
    let expired = match state.store.expired_recordings(Utc::now()).await {
        Ok(expired) => expired,
        Err(err) => {
            warn!(error = %err, "retention could not list expired recordings");
            return;
        }
    };
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
            if let Err(err) = state.store.delete_recording(&recording.recording_id).await {
                warn!(recording_id = %recording.recording_id, error = %err,
                    "retention deleted objects but could not drop the manifest");
                continue;
            }
            info!(recording_id = %recording.recording_id, "retention removed recording objects");
        }
    }
    if state.incident_retention_days > 0
        && let Err(err) = state
            .store
            .delete_closed_incidents_before(
                Utc::now() - chrono::Duration::days(state.incident_retention_days),
            )
            .await
    {
        warn!(error = %err, "retention could not prune closed incidents");
    }
    if state.health_retention_days > 0
        && let Err(err) = state
            .store
            .delete_health_before(Utc::now() - chrono::Duration::days(state.health_retention_days))
            .await
    {
        warn!(error = %err, "retention could not prune health history");
    }
    // Events age out with incidents: they are the same outages, said out
    // loud, and keeping the shouting longer than the record would be odd.
    if state.incident_retention_days > 0
        && let Err(err) = state
            .store
            .delete_events_before(
                Utc::now() - chrono::Duration::days(state.incident_retention_days),
            )
            .await
    {
        warn!(error = %err, "retention could not prune old events");
    }
    if state.audit_retention_days > 0
        && let Err(err) = state
            .store
            .delete_audit_before(Utc::now() - chrono::Duration::days(state.audit_retention_days))
            .await
    {
        warn!(error = %err, "retention could not prune the audit log");
    }
}

/// Record disconnects and recoveries. One writer, on the retention cadence.
/// Effective status follows the same rule as the /cameras view: reported
/// offline, or silent past the stale window. Idempotent against the store's
/// one-open-incident-per-camera invariant, so no read-modify-write.
async fn incident_pass(state: &AppState) {
    if state.up_since.elapsed() < state.incident_grace {
        return;
    }
    let records = match state.store.fleet_cameras().await {
        Ok(records) => records,
        Err(err) => {
            warn!(error = %err, "incident pass could not list cameras");
            return;
        }
    };
    let now = Utc::now();
    let live = newest_camera_map(&*state.camera_batches.read().await);
    // Where each camera is, so an event can say "Yard camera at Bakery"
    // rather than a pair of uuids.
    let places = places_by_site(state).await;
    let mut revoked_gateways: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    for record in records {
        let revoked = match revoked_gateways.get(&record.gateway_id) {
            Some(&revoked) => revoked,
            None => {
                // A store failure reads as not revoked, as in authorized_gateway,
                // so a hiccup leaves the pass behaving as it always did.
                let revoked = state
                    .store
                    .gateway_revoked(&record.gateway_id)
                    .await
                    .unwrap_or(false);
                revoked_gateways.insert(record.gateway_id.clone(), revoked);
                revoked
            }
        };
        if revoked {
            // Not an outage. Closing every pass also heals an incident the
            // revoke itself failed to close, or one opened before this rule.
            if let Err(err) = state.store.close_incident(&record.id, now).await {
                warn!(camera_id = %record.id, error = %err, "incident pass store failure");
            }
            continue;
        }
        let (last_seen, reported_offline, last_error) = match live.get(&record.id) {
            Some(camera) => (
                camera.last_seen,
                camera.status == HealthStatus::Offline,
                camera.last_error.clone(),
            ),
            None => (record.last_seen, false, None),
        };
        let stale = (now - last_seen).num_seconds() > state.stale_camera_seconds;
        let (detail, result) = if stale {
            let started = last_seen + chrono::Duration::seconds(state.stale_camera_seconds);
            let detail = "gateway telemetry is stale";
            (
                Some(detail.to_owned()),
                state
                    .store
                    .open_incident(&record.id, started, Some(detail))
                    .await,
            )
        } else if reported_offline {
            (
                last_error.clone(),
                state
                    .store
                    .open_incident(&record.id, now, last_error.as_deref())
                    .await,
            )
        } else {
            (None, state.store.close_incident(&record.id, now).await)
        };
        match result {
            // Only a transition is news. The reconciler opens and closes every
            // pass; without this an outage would be reported every minute for
            // as long as it lasted.
            Ok(true) => {
                let place = places.get(&record.site_id);
                let down = stale || reported_offline;
                raise_event(
                    state,
                    vms_plugin_sdk::FleetEvent {
                        id: Uuid::new_v4().to_string(),
                        kind: if down {
                            vms_plugin_sdk::FleetEventKind::CameraOffline
                        } else {
                            vms_plugin_sdk::FleetEventKind::CameraRecovered
                        },
                        severity: if down {
                            vms_plugin_sdk::EventSeverity::Critical
                        } else {
                            vms_plugin_sdk::EventSeverity::Info
                        },
                        occurred_at: now,
                        customer_id: place
                            .map(|place| place.customer_id.clone())
                            .unwrap_or_default(),
                        site_id: record.site_id.clone(),
                        site_name: place
                            .map(|place| place.site_name.clone())
                            .unwrap_or_else(|| record.site_id.clone()),
                        gateway_id: Some(record.gateway_id.clone()),
                        camera_id: Some(record.id.clone()),
                        title: format!(
                            "{} {} at {}",
                            record.name,
                            if down {
                                "stopped answering"
                            } else {
                                "is answering again"
                            },
                            place
                                .map(|place| place.site_name.clone())
                                .unwrap_or_else(|| record.site_id.clone()),
                        ),
                        detail,
                        metadata: serde_json::json!({"gateway_id": record.gateway_id}),
                    },
                )
                .await;
            }
            Ok(false) => {}
            Err(err) => {
                warn!(camera_id = %record.id, error = %err, "incident pass store failure")
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
    const ADMIN_PASSWORD: &str = "correct horse battery staple";

    const ADMIN_EMAIL: &str = "admin@localhost";

    /// Seed the way main() does at startup: the credential, then the owner it
    /// belongs to.
    async fn seed_admin(state: &AppState) {
        crate::auth::seed_credentials(
            state.store.as_ref(),
            ADMIN_EMAIL,
            Some(ADMIN_PASSWORD),
            false,
        )
        .await
        .expect("seed the owner");
    }

    async fn test_state() -> AppState {
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::in_memory()
                .await
                .expect("in-memory store"),
        );
        test_state_with(store).await
    }

    async fn test_state_with(store: Arc<dyn crate::store::Store>) -> AppState {
        let dir = tempfile::tempdir().expect("temp plugin dir");
        let plugin_dir = dir.keep();
        AppState {
            gateways: Arc::new(RwLock::new(HashMap::new())),
            camera_batches: Arc::new(RwLock::new(HashMap::new())),
            store,
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
            default_storage_plugin: Arc::from("storage-s3"),
            default_ai_plugin: Arc::from("ai-http-adapter"),
            rtc: Arc::new(crate::turn::RtcConfig::default()),
            default_retention_days: 30,
            login_throttle: Arc::new(tokio::sync::Mutex::new(
                crate::auth::LoginThrottle::default(),
            )),
            cookie_secure: false,
            incident_grace: Duration::ZERO,
            tunnels: Arc::new(RwLock::new(HashMap::new())),
            tunnel_calls: Arc::new(RwLock::new(HashMap::new())),
            tunnel_answers: Arc::new(RwLock::new(HashMap::new())),
            health_retention_days: 30,
            incident_retention_days: 90,
            audit_retention_days: 0,
            up_since: std::time::Instant::now(),
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

    /// Attach a session cookie (from `login_cookie`) to an already-built request.
    fn with_cookie(mut request: Request<Body>, cookie: &str) -> Request<Body> {
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        request
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
    async fn enrollment_and_gateway_token_live_in_the_store_not_in_memory() {
        // Two AppStates sharing one store simulate an API restart: the second
        // state has empty in-memory maps but the same database.
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("store"),
        );
        let state = test_state_with(store.clone()).await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let (_, created) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/enrollments",
                    None,
                    serde_json::json!({
                        "customer_id": "cust-1", "customer_name": "Customer",
                        "site_id": "site-1", "site_name": "Site", "city": "Barcelona",
                    }),
                ),
                &cookie,
            ),
        )
        .await;
        let enrollment_token = created["enrollment_token"].as_str().unwrap().to_owned();
        let (status, enrolled) = send(
            &state,
            post(
                "/api/v1/gateways/enroll",
                None,
                serde_json::json!({
                    "enrollment_token": enrollment_token,
                    "gateway_id": "gw-1", "hostname": "edge-1", "version": "0.1.0",
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let gateway_token = enrolled["gateway_token"].as_str().unwrap().to_owned();

        // "Restart": a fresh AppState over a freshly opened store on the same file.
        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let (status, _) = send(
            &restarted,
            post(
                "/api/v1/cameras/telemetry",
                Some(&gateway_token),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "an enrolled gateway must survive an API restart without re-enrolling"
        );
    }

    #[tokio::test]
    async fn a_known_camera_reports_offline_after_a_restart_instead_of_vanishing() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("store"),
        );
        let state = test_state_with(store).await;
        seed_admin(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let cookie = login_cookie(&restarted).await;

        let (status, cameras) =
            send(&restarted, with_cookie(get("/api/v1/cameras"), &cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            cameras.as_array().map(Vec::len),
            Some(1),
            "the camera vanished: {cameras}"
        );
        assert_eq!(cameras[0]["camera_id"], "cam-1");
        assert_eq!(cameras[0]["status"], "offline");

        let (_, fleet) = send(&restarted, with_cookie(get("/api/v1/fleet"), &cookie)).await;
        assert_eq!(
            fleet["source"], "live",
            "a restart must not demote the dashboard to demo data"
        );
        assert_eq!(
            fleet["customers"][0]["sites"][0]["cameras"][0]["status"],
            "offline"
        );
    }

    /// A sink that either takes events or refuses them, and remembers what it
    /// was given.
    async fn fake_sink(
        id: &'static str,
        accept: bool,
    ) -> (String, Arc<RwLock<Vec<serde_json::Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<RwLock<Vec<serde_json::Value>>> = Arc::new(RwLock::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = vec![0_u8; 16384];
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let response = if request.contains("/v1/plugin/manifest") {
                        Some(
                            serde_json::json!({
                                "id": id, "name": id, "version": "0.1.0",
                                "protocol_version": 1, "vendor": "test",
                                "description": null, "capabilities": ["event_sink"],
                            })
                            .to_string(),
                        )
                    } else if request.contains("/v1/events") {
                        if let Some(body) = request.split("\r\n\r\n").nth(1)
                            && let Ok(parsed) =
                                serde_json::from_str::<serde_json::Value>(body.trim())
                        {
                            recorder.write().await.push(parsed);
                        }
                        accept.then(|| serde_json::json!({"delivered": true}).to_string())
                    } else {
                        Some(serde_json::json!({"healthy": true}).to_string())
                    };
                    let wire = match response {
                        Some(body) => format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        None => "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
                    };
                    let _ = socket.write_all(wire.as_bytes()).await;
                });
            }
        });
        (endpoint, seen)
    }

    /// An AppState whose plugin registry holds these sinks.
    async fn state_with_sinks(sinks: &[(&str, &str)]) -> AppState {
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::in_memory()
                .await
                .expect("in-memory store"),
        );
        let dir = tempfile::tempdir().expect("temp plugin dir");
        for (name, endpoint) in sinks {
            std::fs::write(
                dir.path().join(format!("{name}.json")),
                serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null})
                    .to_string(),
            )
            .unwrap();
        }
        let plugin_dir = dir.keep();
        let mut state = test_state_with(store).await;
        state.plugins = PluginRegistry::load_dir(&plugin_dir)
            .await
            .expect("a plugin dir with sinks");
        state.plugin_dir = Arc::new(plugin_dir);
        state
    }

    fn test_event(id: &str) -> vms_plugin_sdk::FleetEvent {
        vms_plugin_sdk::FleetEvent {
            id: id.into(),
            kind: vms_plugin_sdk::FleetEventKind::CameraOffline,
            severity: vms_plugin_sdk::EventSeverity::Critical,
            occurred_at: Utc::now(),
            customer_id: "cust-1".into(),
            site_id: "site-1".into(),
            site_name: "Bakery".into(),
            gateway_id: Some("gw-1".into()),
            camera_id: Some("cam-1".into()),
            title: "Yard camera stopped answering at Bakery".into(),
            detail: None,
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn an_event_is_written_down_before_anyone_tries_to_deliver_it() {
        // The whole point of an outbox: a sink that is restarting cannot lose
        // an alert, because the alert exists before the sink is called.
        let (endpoint, seen) = fake_sink("sink-1", true).await;
        let state = state_with_sinks(&[("sink", &endpoint)]).await;

        raise_event(&state, test_event("evt-1")).await;
        let stored = state.store.recent_events(10).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].deliveries.len(), 1, "queued for the one sink");
        assert!(stored[0].deliveries[0].delivered_at.is_none());
        assert!(seen.read().await.is_empty(), "nothing has been sent yet");

        delivery_pass(&state, Utc::now()).await;
        assert_eq!(seen.read().await.len(), 1, "and now it has");
        let stored = state.store.recent_events(10).await.unwrap();
        assert!(stored[0].deliveries[0].delivered_at.is_some());
        assert!(!stored[0].deliveries[0].declined);
    }

    #[tokio::test]
    async fn a_sink_that_is_down_is_retried_later_and_not_sooner() {
        let (endpoint, _) = fake_sink("sink-down", false).await;
        let state = state_with_sinks(&[("sink", &endpoint)]).await;
        raise_event(&state, test_event("evt-2")).await;
        // A freshly raised event is due at once; everything after it waits.
        let now = Utc::now();

        delivery_pass(&state, now).await;
        let stored = state.store.recent_events(10).await.unwrap();
        let delivery = &stored[0].deliveries[0];
        assert_eq!(delivery.attempts, 1);
        assert!(delivery.delivered_at.is_none());
        assert!(
            delivery
                .last_error
                .as_deref()
                .is_some_and(|e| !e.is_empty()),
            "the reason stays on the row"
        );
        let due_at = delivery.next_attempt_at.expect("it will be tried again");
        assert!(due_at > now, "and not immediately");

        // Before it is due, nothing happens at all.
        delivery_pass(&state, due_at - chrono::Duration::seconds(1)).await;
        let stored = state.store.recent_events(10).await.unwrap();
        assert_eq!(stored[0].deliveries[0].attempts, 1, "not tried early");

        delivery_pass(&state, due_at).await;
        let stored = state.store.recent_events(10).await.unwrap();
        assert_eq!(stored[0].deliveries[0].attempts, 2);
        assert!(
            stored[0].deliveries[0].next_attempt_at.unwrap() > due_at,
            "each wait is longer than the last"
        );
    }

    #[tokio::test]
    async fn a_sink_that_never_answers_is_given_up_on_with_the_reason_kept() {
        let (endpoint, _) = fake_sink("sink-gone", false).await;
        let state = state_with_sinks(&[("sink", &endpoint)]).await;
        raise_event(&state, test_event("evt-3")).await;

        let mut now = Utc::now();
        for _ in 0..6 {
            delivery_pass(&state, now).await;
            now += chrono::Duration::hours(1);
        }
        let stored = state.store.recent_events(10).await.unwrap();
        let delivery = &stored[0].deliveries[0];
        assert!(
            delivery.next_attempt_at.is_none() && delivery.delivered_at.is_none(),
            "an alert nobody can deliver is not retried until the end of days"
        );
        assert!(
            delivery.last_error.is_some(),
            "and the reason is still there"
        );
    }

    #[tokio::test]
    async fn a_sink_that_declines_is_done_rather_than_retried() {
        let (endpoint, _) = fake_sink("sink-1", true).await;
        let state = state_with_sinks(&[("sink", &endpoint)]).await;
        raise_event(&state, test_event("evt-4")).await;
        delivery_pass(&state, Utc::now()).await;
        let stored = state.store.recent_events(10).await.unwrap();
        assert!(stored[0].deliveries[0].next_attempt_at.is_none());
    }

    #[tokio::test]
    async fn a_sink_registered_after_the_event_gets_no_history() {
        // Alerts are about now. A sink plugged in this afternoon should not
        // start by replaying this morning.
        let state = state_with_sinks(&[]).await;
        raise_event(&state, test_event("evt-5")).await;
        let stored = state.store.recent_events(10).await.unwrap();
        assert_eq!(stored.len(), 1, "the event is still recorded");
        assert!(stored[0].deliveries.is_empty(), "with nowhere to go");

        let (endpoint, seen) = fake_sink("sink-late", true).await;
        std::fs::write(
            state.plugin_dir.join("late.json"),
            serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null})
                .to_string(),
        )
        .unwrap();
        state.plugins.reload(&*state.plugin_dir).await.unwrap();
        delivery_pass(&state, Utc::now()).await;
        assert!(seen.read().await.is_empty());
    }

    #[test]
    fn each_retry_waits_longer_than_the_last_and_then_stops() {
        let waits: Vec<_> = (0..4).map(retry_after).collect();
        assert!(waits.iter().all(Option::is_some));
        for pair in waits.windows(2) {
            assert!(pair[1].unwrap() > pair[0].unwrap(), "{waits:?}");
        }
        assert_eq!(retry_after(4), None, "four refusals is enough");
    }

    /// Two reports twenty seconds apart are twenty seconds of history, and
    /// the numbers on the way back out are computed from them rather than
    /// guessed.
    #[tokio::test]
    async fn telemetry_becomes_a_history_somebody_can_read() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let first = Utc::now() - chrono::Duration::seconds(40);
        let second = first + chrono::Duration::seconds(20);
        let third = second + chrono::Duration::seconds(20);
        for (at, status) in [
            (first, HealthStatus::Healthy),
            (second, HealthStatus::Healthy),
            (third, HealthStatus::Offline),
        ] {
            let (code, _) = send(
                &state,
                post(
                    "/api/v1/cameras/telemetry",
                    Some(SHARED_TOKEN),
                    serde_json::to_value(typed_batch("gw-1", "cam-1", status, at, None)).unwrap(),
                ),
            )
            .await;
            assert_eq!(code, StatusCode::NO_CONTENT);
        }

        let (code, health) = send(
            &state,
            with_cookie(get("/api/v1/cameras/cam-1/health?days=7"), &cookie),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            health["healthy_seconds"], 20,
            "the first report has nothing behind it: {health}"
        );
        assert_eq!(health["offline_seconds"], 20);
        assert_eq!(health["counted_seconds"], 40);
        assert_eq!(health["uptime_percent"], 50.0);
        assert!(
            health["covered_percent"].as_f64().is_some_and(|c| c < 1.0),
            "forty seconds is not a week: {health}"
        );
        assert!(!health["hours"].as_array().unwrap().is_empty());

        // The fleet answer is the same history, summed.
        let (code, fleet) = send(&state, with_cookie(get("/api/v1/health?days=7"), &cookie)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(fleet["counted_seconds"], 40);
        assert_eq!(fleet["uptime_percent"], 50.0);
    }

    #[tokio::test]
    async fn a_camera_nobody_has_heard_from_has_no_history_rather_than_perfect_uptime() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let (code, health) = send(
            &state,
            with_cookie(get("/api/v1/cameras/ghost/health"), &cookie),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(health["counted_seconds"], 0);
        assert!(
            health["uptime_percent"].is_null(),
            "nothing is known, and saying 100% would be a lie: {health}"
        );
    }

    #[tokio::test]
    async fn a_window_longer_than_the_store_keeps_is_clamped() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let (_, health) = send(
            &state,
            with_cookie(get("/api/v1/cameras/cam-1/health?days=9000"), &cookie),
        )
        .await;
        assert_eq!(health["days"], 90);
    }

    /// The password people already knew keeps working; what changes is that
    /// the system now knows who used it.
    /// Log in as somebody the owner just made.
    async fn cookie_for(state: &AppState, email: &str, password: &str) -> String {
        let response = build_router(state.clone())
            .oneshot(post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": email, "password": password }),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "login refused");
        response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("a cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    /// Make a user of this role and come back with their cookie.
    async fn user_cookie(state: &AppState, role: &str) -> String {
        let owner = login_cookie(state).await;
        let email = format!("{role}@example.test");
        let password = "a perfectly good passphrase";
        let (status, body) = send(
            state,
            with_cookie(
                post(
                    "/api/v1/users",
                    None,
                    serde_json::json!({ "email": email, "password": password, "role": role }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        cookie_for(state, &email, password).await
    }

    /// A customer's own login sees their own fleet. Everybody else's does
    /// not get hidden on the screen — it never leaves the control plane.
    #[tokio::test]
    async fn a_customer_login_sees_one_customer() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;

        // Two customers, one camera each.
        for (customer, gateway, camera, site) in [
            ("cust-1", "gw-1", "cam-1", "site-1"),
            ("cust-2", "gw-2", "cam-2", "site-2"),
        ] {
            let mut batch = typed_batch(gateway, camera, HealthStatus::Healthy, Utc::now(), None);
            batch.customer_id = customer.into();
            batch.customer_name = customer.into();
            batch.site_id = site.into();
            batch.site_name = site.into();
            for camera in &mut batch.cameras {
                camera.site_id = site.into();
            }
            state
                .store
                .upsert_fleet_identity(&batch, Utc::now())
                .await
                .unwrap();
            state
                .camera_batches
                .write()
                .await
                .insert(gateway.to_string(), batch);
        }

        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/users",
                    None,
                    serde_json::json!({
                        "email": "customer@example.test",
                        "password": "a perfectly good passphrase",
                        "role": "viewer",
                        "customer_id": "cust-1",
                    }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let customer = cookie_for(
            &state,
            "customer@example.test",
            "a perfectly good passphrase",
        )
        .await;

        let (_, fleet) = send(&state, with_cookie(get("/api/v1/fleet"), &customer)).await;
        let customers = fleet["customers"].as_array().unwrap();
        assert_eq!(customers.len(), 1, "{fleet}");
        assert_eq!(customers[0]["id"], "cust-1");

        let (_, cameras) = send(&state, with_cookie(get("/api/v1/cameras"), &customer)).await;
        let listed = cameras.as_array().unwrap();
        assert_eq!(listed.len(), 1, "{cameras}");
        assert_eq!(listed[0]["camera_id"], "cam-1");

        let (_, gateways) = send(&state, with_cookie(get("/api/v1/gateways"), &customer)).await;
        assert!(
            gateways
                .as_array()
                .unwrap()
                .iter()
                .all(|view| view["gateway_id"] == "gw-1"),
            "{gateways}"
        );

        // And the owner still sees both.
        let (_, all) = send(&state, with_cookie(get("/api/v1/cameras"), &owner)).await;
        assert_eq!(all.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn another_customers_camera_is_not_found_rather_than_forbidden() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        let mut batch = typed_batch("gw-2", "cam-2", HealthStatus::Healthy, Utc::now(), None);
        batch.customer_id = "cust-2".into();
        state
            .store
            .upsert_fleet_identity(&batch, Utc::now())
            .await
            .unwrap();
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/users",
                    None,
                    serde_json::json!({
                        "email": "elsewhere@example.test",
                        "password": "a perfectly good passphrase",
                        "role": "viewer",
                        "customer_id": "cust-1",
                    }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let customer = cookie_for(
            &state,
            "elsewhere@example.test",
            "a perfectly good passphrase",
        )
        .await;

        for uri in [
            "/api/v1/cameras/cam-2/health",
            "/api/v1/cameras/cam-2/recordings",
            "/api/v1/cameras/cam-2/recording-policy",
        ] {
            let (status, _) = send(&state, with_cookie(get(uri), &customer)).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{uri} told them the camera exists"
            );
        }
    }

    #[tokio::test]
    async fn a_scoped_login_that_could_change_things_is_refused() {
        // Scoping covers reading. A scoped technician would be able to act on
        // a fleet the scope was supposed to keep them out of.
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/users",
                    None,
                    serde_json::json!({
                        "email": "scoped@example.test",
                        "password": "a perfectly good passphrase",
                        "role": "technician",
                        "customer_id": "cust-1",
                    }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Connecting a plugin from the dashboard, which until now meant a shell
    /// on the box and a file in plugins.d.
    /// A registration scoped to a customer is now more than a label: their
    /// cameras use their plugin, and everybody else keeps the default.
    #[tokio::test]
    async fn a_customers_camera_uses_that_customers_plugin() {
        let (endpoint, _) = fake_sink("sink-1", true).await;
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;

        let mut batch = typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None);
        batch.customer_id = "cust-1".into();
        state
            .store
            .upsert_fleet_identity(&batch, Utc::now())
            .await
            .unwrap();

        // The fake sink declares event_sink, so it stands in for "a plugin
        // this customer has" without pretending to sign anything.
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/plugins/registrations",
                    None,
                    serde_json::json!({
                        "plugin_id": "", "endpoint": endpoint, "placement": "control_plane",
                        "enabled": true, "token_env": null, "customer_id": "cust-1",
                    }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Their camera gets their plugin for the capability it provides.
        assert_eq!(
            plugin_for(
                &state,
                "cam-1",
                vms_plugin_sdk::PluginCapability::EventSink,
                "the-default",
            )
            .await,
            "sink-1"
        );
        // A capability that plugin does not provide falls back rather than
        // sending storage work to an event sink.
        assert_eq!(
            plugin_for(
                &state,
                "cam-1",
                vms_plugin_sdk::PluginCapability::StorageBlob,
                "the-default",
            )
            .await,
            "the-default"
        );
        // And a camera nobody has scoped keeps the default.
        assert_eq!(
            plugin_for(
                &state,
                "cam-unknown",
                vms_plugin_sdk::PluginCapability::EventSink,
                "the-default",
            )
            .await,
            "the-default"
        );
    }

    /// The audit log was a table, and a table is something anybody with
    /// database access can edit without leaving a mark.
    #[tokio::test]
    async fn editing_the_audit_log_breaks_the_chain_and_says_where() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        for subject in ["one", "two", "three"] {
            audit(&state, ADMIN_EMAIL, "test.event", subject, None).await;
        }

        let (status, integrity) =
            send(&state, with_cookie(get("/api/v1/audit/verify"), &owner)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(integrity["checked"].as_i64().unwrap() >= 3, "{integrity}");
        assert!(integrity["broken_at"].is_null(), "{integrity}");
        let head = integrity["head"].as_str().expect("a head hash").to_owned();

        // Somebody edits a row in place, the way somebody with the database
        // would.
        let entries = state.store.audit_entries(10).await.unwrap();
        let tampered = entries
            .iter()
            .find(|entry| entry.subject == "two")
            .expect("the row is there");
        state
            .store
            .tamper_with_audit_for_test(&tampered.subject, "something else")
            .await;

        let (_, integrity) = send(&state, with_cookie(get("/api/v1/audit/verify"), &owner)).await;
        assert!(
            integrity["broken_at"].as_str().is_some(),
            "an edited log must not verify: {integrity}"
        );
        assert_ne!(
            integrity["head"].as_str().unwrap_or_default(),
            head,
            "and the head must not still be what it was"
        );
    }

    #[tokio::test]
    async fn rows_written_before_the_chain_are_not_claimed_to_be_sound() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        state.store.unchained_audit_row_for_test().await;
        audit(&state, ADMIN_EMAIL, "test.event", "after", None).await;

        let (_, integrity) = send(&state, with_cookie(get("/api/v1/audit/verify"), &owner)).await;
        assert_eq!(integrity["unchained"], 1, "{integrity}");
        assert!(integrity["broken_at"].is_null(), "old rows are not a break");
    }

    #[tokio::test]
    async fn the_audit_export_is_csv_an_auditor_can_open() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        audit(
            &state,
            ADMIN_EMAIL,
            "test.event",
            "a subject, with a comma",
            Some("a \"quoted\" detail"),
        )
        .await;

        let response = build_router(state.clone())
            .oneshot(with_cookie(get("/api/v1/audit/export"), &owner))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/csv; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let csv = String::from_utf8(body.to_vec()).unwrap();
        assert!(csv.starts_with("at,actor,action,subject,detail"), "{csv}");
        assert!(csv.contains("\"a subject, with a comma\""), "{csv}");
        assert!(csv.contains("\"a \"\"quoted\"\" detail\""), "{csv}");
    }

    #[tokio::test]
    async fn the_audit_log_is_an_owners_to_read() {
        let state = test_state().await;
        seed_admin(&state).await;
        let technician = user_cookie(&state, "technician").await;
        for uri in [
            "/api/v1/audit",
            "/api/v1/audit/verify",
            "/api/v1/audit/export",
        ] {
            let (status, _) = send(&state, with_cookie(get(uri), &technician)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
    }

    /// A tunnel reaches devices this gateway has reported and nothing else.
    /// Anything looser is a proxy into somebody's network wearing a camera's
    /// name.
    #[tokio::test]
    async fn a_tunnel_only_opens_onto_a_device_the_gateway_reported() {
        let state = test_state().await;
        seed_admin(&state).await;
        let technician = user_cookie(&state, "technician").await;

        let mut batch = typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None);
        batch.cameras[0].rtsp_endpoint = Some("rtsp://10.0.0.7:554/stream".into());
        state
            .camera_batches
            .write()
            .await
            .insert("gw-1".into(), batch);

        let (status, session) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/gateways/gw-1/tunnel",
                    None,
                    serde_json::json!({ "host": "10.0.0.7", "port": 80, "minutes": 5 }),
                ),
                &technician,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{session}");
        assert_eq!(session["host"], "10.0.0.7");
        assert_eq!(session["opened_by"], "technician@example.test");

        // Any other address on that network is not this system's to reach.
        for host in ["10.0.0.8", "127.0.0.1", "169.254.169.254"] {
            let (status, _) = send(
                &state,
                with_cookie(
                    post(
                        "/api/v1/gateways/gw-1/tunnel",
                        None,
                        serde_json::json!({ "host": host, "port": 80 }),
                    ),
                    &technician,
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "a tunnel opened onto {host}"
            );
        }
    }

    #[tokio::test]
    async fn a_request_goes_to_the_gateway_and_the_answer_comes_back() {
        let state = test_state().await;
        seed_admin(&state).await;
        let technician = user_cookie(&state, "technician").await;
        let mut batch = typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None);
        batch.cameras[0].rtsp_endpoint = Some("rtsp://10.0.0.7:554/stream".into());
        state
            .camera_batches
            .write()
            .await
            .insert("gw-1".into(), batch);
        let (_, session) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/gateways/gw-1/tunnel",
                    None,
                    serde_json::json!({ "host": "10.0.0.7", "port": 80 }),
                ),
                &technician,
            ),
        )
        .await;
        let session_id = session["id"].as_str().unwrap().to_owned();

        // A gateway holding its long poll, answering whatever arrives.
        let gateway = {
            let state = state.clone();
            tokio::spawn(async move {
                let (_, call) = send(
                    &state,
                    Request::builder()
                        .uri("/api/v1/gateways/gw-1/tunnel/next")
                        .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
                let id = call["id"].as_str().expect("a call").to_owned();
                assert_eq!(call["host"], "10.0.0.7");
                assert_eq!(call["path"], "/doc/index.html");
                let (status, _) = send(
                    &state,
                    post(
                        "/api/v1/gateways/gw-1/tunnel/answer",
                        Some(SHARED_TOKEN),
                        serde_json::json!({
                            "id": id, "status": 200,
                            "content_type": "text/html",
                            "body_base64": "PGgxPk5WUjwvaDE+",
                            "error": null,
                        }),
                    ),
                )
                .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
            })
        };

        let response = build_router(state.clone())
            .oneshot(with_cookie(
                get(&format!("/api/v1/tunnels/{session_id}/doc/index.html")),
                &technician,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/html"
        );
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "<h1>NVR</h1>");
        gateway.await.unwrap();

        // Closing says what it was used for, not only that it existed.
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    &format!("/api/v1/tunnels/{session_id}/close"),
                    None,
                    serde_json::json!({}),
                ),
                &technician,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let entries = state.store.audit_entries(20).await.unwrap();
        assert!(
            entries.iter().any(|entry| entry.action == "tunnel.closed"
                && entry.detail.as_deref() == Some("1 requests")),
            "{entries:?}"
        );
    }

    #[tokio::test]
    async fn an_expired_tunnel_is_gone_rather_than_slow() {
        let state = test_state().await;
        seed_admin(&state).await;
        let technician = user_cookie(&state, "technician").await;
        let expired = vms_domain::TunnelSession {
            id: "stale".into(),
            gateway_id: "gw-1".into(),
            host: "10.0.0.7".into(),
            port: 80,
            opened_by: "technician@example.test".into(),
            opened_at: Utc::now() - chrono::Duration::hours(2),
            expires_at: Utc::now() - chrono::Duration::hours(1),
            requests: 0,
        };
        state.tunnels.write().await.insert("stale".into(), expired);

        let (status, _) = send(
            &state,
            with_cookie(get("/api/v1/tunnels/stale/index.html"), &technician),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
        assert!(
            state.tunnels.read().await.is_empty(),
            "an expired tunnel is forgotten, not kept around"
        );
    }

    #[tokio::test]
    async fn a_viewer_may_not_reach_into_a_site() {
        let state = test_state().await;
        seed_admin(&state).await;
        let viewer = user_cookie(&state, "viewer").await;
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/gateways/gw-1/tunnel",
                    None,
                    serde_json::json!({ "host": "10.0.0.7", "port": 80 }),
                ),
                &viewer,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_owner_can_connect_a_plugin_and_disconnect_it() {
        let (endpoint, _) = fake_sink("sink-1", true).await;
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;

        let (status, listed) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/plugins/registrations",
                    None,
                    serde_json::json!({
                        "plugin_id": "", "endpoint": endpoint,
                        "placement": "control_plane", "enabled": true,
                        "token_env": null, "customer_id": null,
                    }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .any(|plugin| plugin["manifest"]["id"] == "sink-1"),
            "the plugin should be live immediately: {listed}"
        );
        // The id came from the plugin's own manifest, not from the form.
        let stored = state.store.plugin_registrations().await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].plugin_id, "sink-1");

        let (status, listed) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/plugins/registrations/sink-1/delete",
                    None,
                    serde_json::json!({}),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(listed.as_array().unwrap().is_empty(), "{listed}");
        assert!(state.store.plugin_registrations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_plugin_that_does_not_answer_is_not_written_down() {
        // A registration for something that never answered is a row nobody
        // can explain a month later.
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/plugins/registrations",
                    None,
                    serde_json::json!({
                        "plugin_id": "", "endpoint": "http://127.0.0.1:1",
                        "placement": "control_plane", "enabled": true,
                        "token_env": null, "customer_id": null,
                    }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(state.store.plugin_registrations().await.unwrap().is_empty());

        // And something that is not a URL is refused before anything is
        // dialled at all.
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/plugins/registrations",
                    None,
                    serde_json::json!({
                        "plugin_id": "", "endpoint": "sink:9003",
                        "placement": "control_plane", "enabled": true,
                        "token_env": null, "customer_id": null,
                    }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn connecting_a_plugin_is_an_owners_job() {
        let state = test_state().await;
        seed_admin(&state).await;
        let technician = user_cookie(&state, "technician").await;
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/plugins/registrations",
                    None,
                    serde_json::json!({
                        "plugin_id": "", "endpoint": "http://127.0.0.1:1",
                        "placement": "control_plane", "enabled": true,
                        "token_env": null, "customer_id": null,
                    }),
                ),
                &technician,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_viewer_may_look_and_may_not_touch() {
        let state = test_state().await;
        seed_admin(&state).await;
        let viewer = user_cookie(&state, "viewer").await;

        let (status, _) = send(&state, with_cookie(get("/api/v1/fleet"), &viewer)).await;
        assert_eq!(status, StatusCode::OK, "a viewer reads");

        for (method, uri) in [
            ("POST", "/api/v1/enrollments"),
            ("POST", "/api/v1/sources"),
            ("POST", "/api/v1/cameras/cam-1/recording-policy"),
            ("POST", "/api/v1/cameras/cam-1/clips"),
            ("GET", "/api/v1/users"),
            ("POST", "/api/v1/plugins/reload"),
            ("POST", "/api/v1/plugins/registrations"),
            ("POST", "/api/v1/plugins/registrations/p-1/delete"),
            ("GET", "/api/v1/audit"),
            ("GET", "/api/v1/audit/verify"),
            ("GET", "/api/v1/audit/export"),
        ] {
            let (status, _) =
                send(&state, with_cookie(protected_request(method, uri), &viewer)).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "a viewer got through to {method} {uri}"
            );
        }
    }

    #[tokio::test]
    async fn a_technician_runs_the_fleet_and_does_not_run_the_system() {
        let state = test_state().await;
        seed_admin(&state).await;
        let technician = user_cookie(&state, "technician").await;

        // Operating is allowed: whatever the handler answers, it is not a
        // refusal by role.
        for (method, uri) in [
            ("POST", "/api/v1/enrollments"),
            ("POST", "/api/v1/sources"),
            ("POST", "/api/v1/cameras/cam-1/clips"),
        ] {
            let (status, _) = send(
                &state,
                with_cookie(protected_request(method, uri), &technician),
            )
            .await;
            assert_ne!(
                status,
                StatusCode::FORBIDDEN,
                "a technician was refused {method} {uri}"
            );
        }

        for (method, uri) in [
            ("GET", "/api/v1/users"),
            ("POST", "/api/v1/users"),
            ("POST", "/api/v1/plugins/reload"),
            ("GET", "/api/v1/audit"),
        ] {
            let (status, _) = send(
                &state,
                with_cookie(protected_request(method, uri), &technician),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "a technician got through to {method} {uri}"
            );
        }
    }

    #[tokio::test]
    async fn an_owner_makes_people_and_can_turn_them_off() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        let technician = user_cookie(&state, "technician").await;

        let (status, users) = send(&state, with_cookie(get("/api/v1/users"), &owner)).await;
        assert_eq!(status, StatusCode::OK);
        let listed = users.as_array().unwrap();
        assert_eq!(listed.len(), 2, "{users}");
        let made = listed
            .iter()
            .find(|user| user["email"] == "technician@example.test")
            .expect("the new user is listed");
        assert_eq!(made["role"], "technician");

        // Turning somebody off ends their session there and then.
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    &format!("/api/v1/users/{}", made["id"].as_str().unwrap()),
                    None,
                    serde_json::json!({ "disabled": true }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = send(&state, with_cookie(get("/api/v1/fleet"), &technician)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_owner_cannot_lock_themselves_out() {
        // Nobody can answer that support call.
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;
        let me = state.store.users().await.unwrap().remove(0);

        for change in [
            serde_json::json!({ "disabled": true }),
            serde_json::json!({ "role": "viewer" }),
        ] {
            let (status, _) = send(
                &state,
                with_cookie(
                    post(&format!("/api/v1/users/{}", me.id), None, change.clone()),
                    &owner,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "allowed {change}");
        }
    }

    #[tokio::test]
    async fn a_weak_password_or_a_taken_email_is_refused_rather_than_stored() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = login_cookie(&state).await;

        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/users",
                    None,
                    serde_json::json!({ "email": "weak@example.test", "password": "short",
                                        "role": "viewer" }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/users",
                    None,
                    serde_json::json!({ "email": ADMIN_EMAIL, "password": "a fine long password",
                                        "role": "owner" }),
                ),
                &owner,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "that email is taken");
    }

    #[tokio::test]
    async fn the_old_single_password_becomes_an_owner_without_changing() {
        let state = test_state().await;
        // An install from before users: only the single credential exists.
        crate::auth::seed_admin_credential_for_test(state.store.as_ref(), ADMIN_PASSWORD).await;
        assert!(state.store.users().await.unwrap().is_empty());

        crate::auth::seed_credentials(state.store.as_ref(), ADMIN_EMAIL, None, false)
            .await
            .expect("carrying the credential across needs no env password");

        let users = state.store.users().await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].email, ADMIN_EMAIL);
        assert_eq!(users[0].role, vms_domain::Role::Owner);

        let (status, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "the same password still works"
        );
    }

    #[tokio::test]
    async fn an_email_is_matched_however_it_was_typed() {
        let state = test_state().await;
        seed_admin(&state).await;
        for typed in ["ADMIN@LOCALHOST", " Admin@Localhost "] {
            let (status, _) = send(
                &state,
                post(
                    "/api/v1/auth/login",
                    None,
                    serde_json::json!({ "email": typed, "password": ADMIN_PASSWORD }),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "refused {typed:?}");
        }
    }

    #[tokio::test]
    async fn a_disabled_account_cannot_log_in_and_its_sessions_stop_working() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let owner = state.store.users().await.unwrap().remove(0);

        // The session works right up until the account is turned off.
        let (status, _) = send(&state, with_cookie(get("/api/v1/fleet"), &cookie)).await;
        assert_eq!(status, StatusCode::OK);
        state
            .store
            .update_user(&owner.id, None, None, Some(true), None, Utc::now())
            .await
            .unwrap();

        let (status, _) = send(&state, with_cookie(get("/api/v1/fleet"), &cookie)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a disabled account keeps no way in"
        );
        let (status, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_audit_log_says_who_rather_than_admin() {
        let state = test_state().await;
        seed_admin(&state).await;
        let _ = login_cookie(&state).await;
        let entries = state.store.audit_entries(10).await.unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.action == "login.ok" && entry.actor == ADMIN_EMAIL),
            "every row used to say admin: {entries:?}"
        );
    }

    #[tokio::test]
    async fn a_camera_nobody_has_configured_is_not_recorded() {
        // No policy is a policy: off. Answering 404 would make the dashboard
        // invent a default of its own, and two defaults disagree eventually.
        let state = test_state().await;
        seed_admin(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;
        let cookie = login_cookie(&state).await;

        let (status, policy) = send(
            &state,
            with_cookie(get("/api/v1/cameras/cam-1/recording-policy"), &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(policy["mode"], "off");
        assert_eq!(policy["gateway_id"], "gw-1");
        assert_eq!(policy["keep"].as_array().map(|k| k.len()), Some(0));
    }

    #[tokio::test]
    async fn a_policy_reaches_the_gateway_that_carries_the_camera() {
        let state = test_state().await;
        seed_admin(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;
        with_camera(&state, "cam-2", "gw-2").await;
        let cookie = login_cookie(&state).await;

        let (status, saved) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/cameras/cam-1/recording-policy",
                    None,
                    serde_json::json!({
                        "mode": "continuous",
                        "retention_days": 30,
                        "keep": [{"type": "schedule", "days": 0, "from_minute": 480, "to_minute": 1080}],
                    }),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["mode"], "continuous");

        let (status, mine) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/recording-policies")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let mine = mine.as_array().expect("a list");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["camera_id"], "cam-1");
        assert_eq!(mine[0]["keep"][0]["from_minute"], 480);

        // Another gateway sees nothing of it.
        let (_, theirs) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-2/recording-policies")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(theirs.as_array().map(|list| list.len()), Some(0));
    }

    #[tokio::test]
    async fn the_policy_poll_is_behind_the_gateway_token() {
        let state = test_state().await;
        let (status, _) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/recording-policies")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_rule_the_gateway_could_not_act_on_is_refused() {
        // A rule that looks set but does nothing is worse than no rule.
        let state = test_state().await;
        seed_admin(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;
        let cookie = login_cookie(&state).await;

        for bad in [
            serde_json::json!({"type": "schedule", "days": 0, "from_minute": 600, "to_minute": 600}),
            serde_json::json!({"type": "schedule", "days": 0, "from_minute": 2000, "to_minute": 10}),
            serde_json::json!({"type": "on_incident", "pre_roll_seconds": 0, "post_roll_seconds": 0}),
            serde_json::json!({"type": "on_analysis", "plugin_id": "", "every_seconds": 10,
                               "threshold": 0.5, "pre_roll_seconds": 5, "post_roll_seconds": 5}),
        ] {
            let (status, _) = send(
                &state,
                with_cookie(
                    post(
                        "/api/v1/cameras/cam-1/recording-policy",
                        None,
                        serde_json::json!({"mode": "continuous", "keep": [bad.clone()]}),
                    ),
                    &cookie,
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "should have refused {bad}"
            );
        }
    }

    #[tokio::test]
    async fn asking_for_a_clip_queues_one_at_the_camera_s_own_gateway() {
        // A clip is answered out of the gateway's ring buffer, so the request
        // has to reach the gateway that carries the camera and nobody else.
        let state = test_state().await;
        seed_admin(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;
        let cookie = login_cookie(&state).await;

        let (status, accepted) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/cameras/cam-1/clips",
                    None,
                    serde_json::json!({"seconds": 120}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(accepted["command_id"].as_str().is_some());

        let (_, next) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/commands/next")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(next["kind"]["type"], "save_clip");
        assert_eq!(next["kind"]["camera_id"], "cam-1");
        assert_eq!(next["kind"]["seconds"], 120);
        assert!(
            next["kind"]["storage_plugin_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "a clip has to know where to put itself"
        );
    }

    #[tokio::test]
    async fn a_clip_of_an_unreasonable_length_is_clamped_rather_than_refused() {
        let state = test_state().await;
        seed_admin(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;
        let cookie = login_cookie(&state).await;

        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/cameras/cam-1/clips",
                    None,
                    serde_json::json!({"seconds": 999_999}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (_, next) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/commands/next")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            next["kind"]["seconds"], 3600,
            "an hour is the most it asks for"
        );
    }

    #[tokio::test]
    async fn a_completed_recording_survives_a_restart_and_appears_on_the_timeline() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("store"),
        );
        let state = test_state_with(store).await;
        seed_admin(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;
        let cookie = login_cookie(&state).await;

        let (_, accepted) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/cameras/cam-1/recordings",
                    None,
                    serde_json::json!({"duration_seconds": 10, "segment_seconds": 2}),
                ),
                &cookie,
            ),
        )
        .await;
        let command_id = accepted["command_id"].as_str().unwrap().to_owned();
        // Collect it so completion is legal, then complete with a manifest.
        let (_, _) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/commands/next")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let now = chrono::Utc::now();
        let (status, _) = send(
            &state,
            post(
                &format!("/api/v1/gateways/gw-1/commands/{command_id}/complete"),
                Some(SHARED_TOKEN),
                serde_json::json!({
                    "command_id": command_id, "gateway_id": "gw-1",
                    "status": "succeeded", "completed_at": now, "error": null,
                    "live": null, "analysis": null,
                    "recording": {
                        "recording_id": "rec-1", "camera_id": "cam-1", "gateway_id": "gw-1",
                        "started_at": now, "ended_at": now,
                        "codec": "avc1.640028", "width": 1920, "height": 1080,
                        "init": {"storage_plugin_id": "storage-s3", "object_ref": "rec-1/init.mp4",
                                  "object_key": "rec-1/init.mp4", "content_type": "video/mp4", "size_bytes": 1024},
                        "segments": [], "delete_after": null,
                    },
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let restarted_cookie = login_cookie(&restarted).await;
        let (status, timeline) = send(
            &restarted,
            with_cookie(get("/api/v1/cameras/cam-1/recordings"), &restarted_cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            timeline["recordings"][0]["recording_id"], "rec-1",
            "a recording made before the restart must still be on the timeline"
        );
    }

    #[tokio::test]
    async fn retention_keeps_the_manifest_when_storage_delete_fails() {
        // The storage plugin registry is empty under test, so every delete
        // fails — after a retention pass the row must still be there, or a
        // transient storage outage would orphan objects forever.
        let state = test_state().await;
        let mut expired = serde_json::from_value::<vms_domain::RecordingManifest>(serde_json::json!({
            "recording_id": "rec-exp", "camera_id": "cam-1", "gateway_id": "gw-1",
            "started_at": chrono::Utc::now(), "ended_at": chrono::Utc::now(),
            "codec": "avc1.640028", "width": 1920, "height": 1080,
            "init": {"storage_plugin_id": "storage-s3", "object_ref": "rec-exp/init.mp4",
                      "object_key": "rec-exp/init.mp4", "content_type": "video/mp4", "size_bytes": 1},
            "segments": [], "delete_after": null,
        })).unwrap();
        expired.delete_after = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
        state.store.save_recording(&expired).await.unwrap();

        retention_pass(&state).await;

        assert!(
            state.store.recording("rec-exp").await.is_ok(),
            "retention deleted the manifest although storage still holds the objects"
        );
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
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let (status, created) = send(
            &state,
            with_cookie(
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
                &cookie,
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
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let (_, created) = send(
            &state,
            with_cookie(
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
                &cookie,
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
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        for path in [
            "/api/v1/plugins/nope/health",
            "/api/v1/plugins/nope/ai/analyze",
            "/api/v1/plugins/nope/storage/uploads",
        ] {
            let request = if path.ends_with("health") {
                with_cookie(get(path), &cookie)
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
        let (status, _) = send(
            &state,
            with_cookie(get("/api/v1/plugins/nope/health"), &cookie),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_command_is_a_404_not_a_500() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let (status, _) = send(
            &state,
            with_cookie(get("/api/v1/commands/does-not-exist"), &cookie),
        )
        .await;
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
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        with_camera(&state, "cam-1", "gw-1").await;

        let (status, accepted) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/cameras/cam-1/live",
                    None,
                    serde_json::json!({"offer_sdp": "v=0\r\n", "offer_type": "offer"}),
                ),
                &cookie,
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

        let (status, view) = send(
            &state,
            with_cookie(get(&format!("/api/v1/commands/{command_id}")), &cookie),
        )
        .await;
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
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/cameras/no-such-camera/live",
                    None,
                    serde_json::json!({"offer_sdp": "v=0\r\n"}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Enroll gw-1 end to end and return its per-gateway token.
    async fn enrolled_gateway_token(state: &AppState, cookie: &str) -> String {
        let mut request = post(
            "/api/v1/enrollments",
            None,
            serde_json::json!({
                "customer_id": "cust-1", "customer_name": "Customer",
                "site_id": "site-1", "site_name": "Site", "city": "Barcelona",
            }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (_, created) = send(state, request).await;
        let enrollment_token = created["enrollment_token"].as_str().unwrap().to_owned();
        let (_, enrolled) = send(
            state,
            post(
                "/api/v1/gateways/enroll",
                None,
                serde_json::json!({
                    "enrollment_token": enrollment_token,
                    "gateway_id": "gw-1", "hostname": "edge-1", "version": "0.1.0",
                }),
            ),
        )
        .await;
        enrolled["gateway_token"].as_str().unwrap().to_owned()
    }

    /// Enrol gw-1 and give it two cameras.
    async fn gateway_with_two_cameras(state: &AppState, cookie: &str) -> String {
        let token = enrolled_gateway_token(state, cookie).await;
        let mut batch = telemetry_batch("gw-1");
        batch["cameras"] = serde_json::json!([camera("cam-1", "gw-1"), camera("cam-2", "gw-1")]);
        let (status, _) = send(
            state,
            post("/api/v1/cameras/telemetry", Some(&token), batch),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        token
    }

    async fn roster_size(state: &AppState, cookie: &str) -> usize {
        let (_, body) = send(state, with_cookie(get("/api/v1/cameras"), cookie)).await;
        body.as_array().expect("a list").len()
    }

    async fn add_source(
        state: &AppState,
        cookie: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        send(
            state,
            with_cookie(post("/api/v1/sources", None, body), cookie),
        )
        .await
    }

    fn rtsp_source(gateway_id: &str, address: &str) -> serde_json::Value {
        serde_json::json!({
            "gateway_id": gateway_id, "name": "Yard NVR",
            "kind": "rtsp", "address": address,
        })
    }

    /// The whole life of a source: added from the dashboard, listed there, handed to
    /// its own gateway and to no other, removed, and audited at both ends.
    #[tokio::test]
    async fn a_source_is_added_listed_polled_and_removed() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let token = gateway_with_two_cameras(&state, &cookie).await;

        let (status, created) = add_source(
            &state,
            &cookie,
            rtsp_source("gw-1", "rtsp://10.0.0.7/stream1"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created:?}");
        let id = created["id"].as_str().expect("an id").to_owned();

        let (_, listed) = send(&state, with_cookie(get("/api/v1/sources"), &cookie)).await;
        assert_eq!(listed.as_array().expect("a list").len(), 1);
        assert_eq!(listed[0]["address"], "rtsp://10.0.0.7/stream1");

        // The gateway's own poll, behind its bearer.
        let (status, mine) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/sources")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mine.as_array().expect("a list").len(), 1);
        assert_eq!(mine[0]["id"], id);

        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    &format!("/api/v1/sources/{id}/delete"),
                    None,
                    serde_json::json!({}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, listed) = send(&state, with_cookie(get("/api/v1/sources"), &cookie)).await;
        assert!(listed.as_array().expect("a list").is_empty());

        let actions = audit_actions(&state, &cookie).await;
        for expected in ["source.added", "source.removed"] {
            assert!(
                actions.iter().any(|a| a == expected),
                "missing {expected} in {actions:?}"
            );
        }
    }

    /// A password in the URL would be a password in the control plane. It is refused,
    /// and the gateway's own credential store is where it belongs.
    #[tokio::test]
    async fn an_address_carrying_a_password_is_refused() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        gateway_with_two_cameras(&state, &cookie).await;

        for address in [
            "rtsp://admin:hunter2@10.0.0.7/stream1",
            "rtsp://admin@10.0.0.7/stream1",
        ] {
            let (status, _) = add_source(&state, &cookie, rtsp_source("gw-1", address)).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{address} was accepted"
            );
        }
        let (_, listed) = send(&state, with_cookie(get("/api/v1/sources"), &cookie)).await;
        assert!(
            listed.as_array().expect("a list").is_empty(),
            "nothing was stored"
        );
    }

    #[tokio::test]
    async fn a_source_needs_a_gateway_that_exists_and_an_address_that_parses() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        gateway_with_two_cameras(&state, &cookie).await;

        let (status, _) =
            add_source(&state, &cookie, rtsp_source("gw-nope", "rtsp://10.0.0.7/s")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no such gateway");

        for bad in ["http://10.0.0.7/stream", "rtsp://", "not a url", ""] {
            let (status, _) = add_source(&state, &cookie, rtsp_source("gw-1", bad)).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad:?} was accepted"
            );
        }
        // A pushed stream is named by a key, not a URL, and the key is checked too.
        let mut pushed = rtsp_source("gw-1", "yard entrance");
        pushed["kind"] = serde_json::json!("rtmp");
        let (status, _) = add_source(&state, &cookie, pushed.clone()).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a key with a space"
        );
        pushed["address"] = serde_json::json!("yard-entrance");
        let (status, _) = add_source(&state, &cookie, pushed).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn a_gateway_cannot_poll_another_gateways_sources() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let token = gateway_with_two_cameras(&state, &cookie).await;
        add_source(
            &state,
            &cookie,
            rtsp_source("gw-1", "rtsp://10.0.0.7/stream1"),
        )
        .await;

        let (status, _) = send(&state, get("/api/v1/gateways/gw-1/sources")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "no bearer at all");

        let (status, _) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-other/sources")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "another gateway's list");
    }

    #[tokio::test]
    async fn a_working_gateway_does_not_lose_its_cameras() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        gateway_with_two_cameras(&state, &cookie).await;

        // Retiring is for a gateway that has been revoked. On a live one it is
        // a mistake, and a mistake that would empty a working site's roster.
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/gateways/gw-1/cameras/retire",
                    None,
                    serde_json::json!({}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(roster_size(&state, &cookie).await, 2, "nothing was retired");
    }

    #[tokio::test]
    async fn retiring_the_cameras_of_a_gateway_that_does_not_exist_is_a_404() {
        // A typo is a miss, not a conflict — the same answer a revoke gives.
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let (status, _) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/gateways/gw-typo/cameras/retire",
                    None,
                    serde_json::json!({}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn retiring_a_revoked_gateways_cameras_empties_the_roster() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        gateway_with_two_cameras(&state, &cookie).await;
        let (status, _) = send(
            &state,
            with_cookie(
                post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({})),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, body) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/gateways/gw-1/cameras/retire",
                    None,
                    serde_json::json!({}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["retired"], 2);
        assert_eq!(roster_size(&state, &cookie).await, 0);

        let (_, fleet) = send(&state, with_cookie(get("/api/v1/fleet"), &cookie)).await;
        let in_fleet: usize = fleet["customers"]
            .as_array()
            .expect("customers")
            .iter()
            .flat_map(|customer| customer["sites"].as_array().expect("sites"))
            .map(|site| site["cameras"].as_array().expect("cameras").len())
            .sum();
        assert_eq!(in_fleet, 0, "the dashboard stops showing them too");

        assert!(
            audit_actions(&state, &cookie)
                .await
                .iter()
                .any(|a| a == "cameras.retired"),
            "the retire leaves a row"
        );

        // Nothing left to retire the second time.
        let (status, body) = send(
            &state,
            with_cookie(
                post(
                    "/api/v1/gateways/gw-1/cameras/retire",
                    None,
                    serde_json::json!({}),
                ),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["retired"], 0);
    }

    #[tokio::test]
    async fn a_revoked_gateway_is_refused_every_credential_until_it_reenrolls() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let gateway_token = enrolled_gateway_token(&state, &cookie).await;

        // Sanity: both credentials work before the revoke.
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
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(SHARED_TOKEN),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(&gateway_token),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the revoked token still worked"
        );
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(SHARED_TOKEN),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "revocation must beat the bootstrap secret"
        );

        // A fresh admin-issued enrollment is the un-revoke.
        let new_token = enrolled_gateway_token(&state, &cookie).await;
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(&new_token),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "re-enrollment must restore access"
        );

        // Revoking an unknown gateway is a 404, not a silent success.
        let mut request = post(
            "/api/v1/gateways/gw-nope/revoke",
            None,
            serde_json::json!({}),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_revocation_survives_a_restart() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("store"),
        );
        let state = test_state_with(store).await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let gateway_token = enrolled_gateway_token(&state, &cookie).await;
        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        send(&state, request).await;

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let (status, _) = send(
            &restarted,
            post(
                "/api/v1/cameras/telemetry",
                Some(&gateway_token),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a restart must not resurrect a revoked gateway"
        );
        let (status, _) = send(
            &restarted,
            post(
                "/api/v1/cameras/telemetry",
                Some(SHARED_TOKEN),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_gateways_screen_shows_the_roster_not_just_the_loud() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let _ = enrolled_gateway_token(&state, &cookie).await; // gw-1, enrolled, silent
        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        send(&state, request).await;

        // gw-live heartbeats but is not in the store roster.
        let (status, _) = send(
            &state,
            post(
                "/api/v1/gateways/heartbeat",
                Some(SHARED_TOKEN),
                serde_json::json!({
                    "gateway_id": "gw-live", "site_id": "site-9", "hostname": "edge-9",
                    "version": "0.1.0", "uptime_seconds": 60, "cpu_percent": 1.0,
                    "memory_percent": 1.0, "cameras_seen": 0, "healthy_cameras": 0,
                    "warning_cameras": 0, "offline_cameras": 0, "sent_at": chrono::Utc::now(),
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let mut request = get("/api/v1/gateways");
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::OK);
        let list = body.as_array().expect("a list");
        assert_eq!(list.len(), 2, "roster + live must both show: {body}");
        let gw1 = list.iter().find(|v| v["gateway_id"] == "gw-1").unwrap();
        assert!(
            !gw1["revoked_at"].is_null(),
            "the revoked flag must reach the screen"
        );
        assert_eq!(gw1["online"], false);
        let live = list.iter().find(|v| v["gateway_id"] == "gw-live").unwrap();
        assert_eq!(live["online"], true);
        assert!(
            !live["heartbeat"].is_null(),
            "a live gateway carries its report"
        );
    }

    /// Every route the browser touches. The router in build_router has exactly
    /// two groups; when you add a protected route there, add it here or the
    /// with-a-session test below cannot vouch for it.
    const PROTECTED_ROUTES: &[(&str, &str)] = &[
        ("GET", "/api/v1/fleet"),
        ("GET", "/api/v1/incidents"),
        ("GET", "/api/v1/audit"),
        ("GET", "/api/v1/cameras"),
        ("GET", "/api/v1/gateways"),
        ("POST", "/api/v1/gateways/gw-1/revoke"),
        ("POST", "/api/v1/gateways/gw-1/cameras/retire"),
        ("POST", "/api/v1/enrollments"),
        ("POST", "/api/v1/sources"),
        ("GET", "/api/v1/sources"),
        ("POST", "/api/v1/sources/src-1/delete"),
        ("GET", "/api/v1/commands/cmd-1"),
        ("GET", "/api/v1/rtc/config"),
        ("POST", "/api/v1/cameras/cam-1/live"),
        ("POST", "/api/v1/cameras/cam-1/analyze"),
        ("POST", "/api/v1/cameras/cam-1/recordings"),
        ("GET", "/api/v1/cameras/cam-1/recordings"),
        ("GET", "/api/v1/recordings/rec-1/playback"),
        ("GET", "/api/v1/plugins"),
        ("POST", "/api/v1/plugins/reload"),
        ("GET", "/api/v1/plugins/p-1/health"),
        ("POST", "/api/v1/plugins/p-1/storage/downloads"),
        ("POST", "/api/v1/plugins/p-1/storage/delete"),
        ("POST", "/api/v1/auth/logout"),
        ("POST", "/api/v1/auth/password"),
        ("GET", "/api/v1/users"),
        ("POST", "/api/v1/users"),
        ("POST", "/api/v1/users/u-1"),
        ("GET", "/api/v1/health"),
        ("GET", "/api/v1/cameras/cam-1/health"),
        ("GET", "/api/v1/events"),
        ("POST", "/api/v1/events/test"),
        ("POST", "/api/v1/gateways/gw-1/tunnel"),
        ("POST", "/api/v1/tunnels/s-1/close"),
        ("GET", "/api/v1/tunnels/s-1/index.html"),
        ("POST", "/api/v1/cameras/cam-1/clips"),
        ("GET", "/api/v1/cameras/cam-1/recording-policy"),
        ("POST", "/api/v1/cameras/cam-1/recording-policy"),
    ];

    fn protected_request(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap()
    }

    async fn login_cookie(state: &AppState) -> String {
        let response = build_router(state.clone())
            .oneshot(post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            ))
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "login refused");
        let set_cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("login must set a cookie")
            .to_str()
            .unwrap();
        assert!(
            set_cookie.contains("HttpOnly"),
            "cookie missing HttpOnly: {set_cookie}"
        );
        assert!(
            set_cookie.contains("SameSite=Lax"),
            "cookie missing SameSite: {set_cookie}"
        );
        set_cookie.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn the_incident_list_needs_a_session_and_shows_open_incidents() {
        let state = test_state().await;
        seed_admin(&state).await;
        let (status, _) = send(&state, get("/api/v1/incidents")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        state
            .store
            .open_incident("cam-1", Utc::now(), Some("gateway telemetry is stale"))
            .await
            .unwrap();
        let cookie = login_cookie(&state).await;
        let mut request = get("/api/v1/incidents");
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["camera_id"], "cam-1");
        assert!(body[0]["ended_at"].is_null());
        assert_eq!(body[0]["detail"], "gateway telemetry is stale");
    }

    #[tokio::test]
    async fn an_open_incident_survives_a_restart_and_recovery_closes_it() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("store"),
        );
        let state = test_state_with(store).await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None),
                went_dark,
            )
            .await
            .unwrap();
        incident_pass(&state).await;
        assert_eq!(state.store.incidents(10).await.unwrap().len(), 1);

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let open = restarted.store.incidents(10).await.unwrap();
        assert_eq!(open.len(), 1, "the incident vanished across the restart");
        assert!(open[0].ended_at.is_none());

        restarted.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None),
        );
        incident_pass(&restarted).await;
        assert!(
            restarted.store.incidents(10).await.unwrap()[0]
                .ended_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn retention_prunes_old_closed_incidents_but_never_open_ones() {
        let state = test_state().await;
        let now = Utc::now();
        state
            .store
            .open_incident("cam-old", now - chrono::Duration::days(120), None)
            .await
            .unwrap();
        state
            .store
            .close_incident("cam-old", now - chrono::Duration::days(119))
            .await
            .unwrap();
        state
            .store
            .open_incident("cam-stuck", now - chrono::Duration::days(200), None)
            .await
            .unwrap();

        retention_pass(&state).await;

        let left = state.store.incidents(10).await.unwrap();
        assert_eq!(
            left.len(),
            1,
            "the 119-day-old closed incident must be pruned: {left:?}"
        );
        assert_eq!(left[0].camera_id, "cam-stuck");
        assert!(left[0].ended_at.is_none());
    }

    #[tokio::test]
    async fn incident_retention_zero_keeps_everything() {
        let mut state = test_state().await;
        state.incident_retention_days = 0;
        let now = Utc::now();
        state
            .store
            .open_incident("cam-old", now - chrono::Duration::days(400), None)
            .await
            .unwrap();
        state
            .store
            .close_incident("cam-old", now - chrono::Duration::days(399))
            .await
            .unwrap();

        retention_pass(&state).await;

        assert_eq!(state.store.incidents(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn every_protected_route_is_401_without_a_session() {
        let state = test_state().await;
        seed_admin(&state).await;
        for (method, uri) in PROTECTED_ROUTES {
            let (status, _) = send(&state, protected_request(method, uri)).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} answered without a session"
            );
        }
    }

    #[tokio::test]
    async fn with_a_session_no_protected_route_says_unauthorized() {
        let state = test_state().await;
        seed_admin(&state).await;
        // A fresh session per route: the table contains logout, which kills the session it is called with.
        for (method, uri) in PROTECTED_ROUTES {
            let cookie = login_cookie(&state).await;
            let mut request = protected_request(method, uri);
            request
                .headers_mut()
                .insert("cookie", cookie.parse().unwrap());
            let (status, _) = send(&state, request).await;
            assert_ne!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} rejected a valid session"
            );
        }
    }

    #[tokio::test]
    async fn a_wrong_password_is_401_and_counts_against_the_throttle() {
        let state = test_state().await;
        seed_admin(&state).await;
        let (status, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": "wrong" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(state.login_throttle.lock().await.consecutive(), 1);

        // A later success resets the count.
        let _ = login_cookie(&state).await;
        assert_eq!(state.login_throttle.lock().await.consecutive(), 0);
    }

    #[tokio::test]
    async fn a_failed_login_is_audited_with_how_many_in_a_row() {
        let state = test_state().await;
        seed_admin(&state).await;
        for _ in 0..2 {
            let (status, _) = send(
                &state,
                post(
                    "/api/v1/auth/login",
                    None,
                    serde_json::json!({ "email": ADMIN_EMAIL, "password": "wrong" }),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let details: Vec<_> = state
            .store
            .audit_entries(10)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "login.failed")
            .map(|e| e.detail)
            .collect();
        // An operator reading the log sees a burst, not two identical rows.
        assert!(
            details.contains(&Some("1 wrong password in a row".to_string()))
                && details.contains(&Some("2 wrong passwords in a row".to_string())),
            "the failed-login rows must count the run: {details:?}"
        );
    }

    #[tokio::test]
    async fn a_wrong_current_password_leaves_an_audit_row() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": "not the password", "new": "an entirely new passphrase" }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Every credential failure leaves a row, or the count a failed login
        // reports covers attempts with nothing behind them.
        let rows: Vec<_> = state
            .store
            .audit_entries(10)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "password.change.failed")
            .collect();
        assert_eq!(rows.len(), 1, "one failed change, one row");
        assert_eq!(
            rows[0].detail,
            Some("1 wrong password in a row".to_string())
        );
    }

    #[tokio::test]
    async fn the_session_check_reports_alive_or_not() {
        let state = test_state().await;
        seed_admin(&state).await;
        let (status, _) = send(&state, get("/api/v1/auth/session")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let cookie = login_cookie(&state).await;
        let mut request = get("/api/v1/auth/session");
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // A forged cookie is not a session.
        let mut request = get("/api/v1/auth/session");
        request
            .headers_mut()
            .insert("cookie", "vms_session=forged".parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_invalidates_the_session_and_clears_the_cookie() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let mut request = post("/api/v1/auth/logout", None, serde_json::json!({}));
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cleared = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("logout must clear the cookie")
            .to_str()
            .unwrap();
        assert!(
            cleared.contains("Max-Age=0"),
            "not a clearing cookie: {cleared}"
        );

        let mut request = get("/api/v1/auth/session");
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the session survived logout"
        );
    }

    #[tokio::test]
    async fn changing_the_password_kills_other_sessions_and_keeps_the_caller() {
        let state = test_state().await;
        seed_admin(&state).await;
        let other = login_cookie(&state).await;
        let caller = login_cookie(&state).await;

        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": ADMIN_PASSWORD, "new": "an entirely new passphrase" }),
        );
        request
            .headers_mut()
            .insert("cookie", caller.parse().unwrap());
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let fresh = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("password change must re-mint the caller's session")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        // The other session is dead, the fresh one lives.
        let mut request = get("/api/v1/auth/session");
        request
            .headers_mut()
            .insert("cookie", other.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the old session outlived the change"
        );
        let mut request = get("/api/v1/auth/session");
        request
            .headers_mut()
            .insert("cookie", fresh.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // And only the new password logs in now.
        let (status, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": "an entirely new passphrase" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_password_change_needs_the_current_password_and_a_long_enough_new_one() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": "not the password", "new": "long enough replacement" }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": ADMIN_PASSWORD, "new": "short" }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // Both refusals left the credential untouched.
        let (status, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_wrong_current_password_on_change_counts_against_the_throttle() {
        // A stolen session must not be an unthrottled oracle for guessing the
        // real password; wrong `current` costs the same growing delay a failed
        // login does.
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": "not the password", "new": "long enough replacement" }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            state.login_throttle.lock().await.consecutive(),
            1,
            "a wrong current password must register a throttle failure"
        );

        // Proving knowledge of the password resets the counter, same as login.
        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": ADMIN_PASSWORD, "new": "an entirely fresh passphrase" }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(state.login_throttle.lock().await.consecutive(), 0);
    }

    #[tokio::test]
    async fn the_machine_plugin_endpoints_demand_a_gateway_bearer() {
        // These two are what the edge gateway calls to mint signed upload
        // URLs and spend AI quota; leaving them open let anyone do both.
        let state = test_state().await;
        seed_admin(&state).await;
        for uri in [
            "/api/v1/plugins/p-1/ai/analyze",
            "/api/v1/plugins/p-1/storage/uploads",
        ] {
            let (status, _) = send(&state, post(uri, None, serde_json::json!({}))).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{uri} answered without a bearer"
            );

            let (status, _) = send(
                &state,
                post(uri, Some("not-a-token"), serde_json::json!({})),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{uri} accepted a wrong bearer"
            );

            // A dashboard session cookie is not a machine credential.
            let cookie = login_cookie(&state).await;
            let mut request = post(uri, None, serde_json::json!({}));
            request
                .headers_mut()
                .insert("cookie", cookie.parse().unwrap());
            let (status, _) = send(&state, request).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{uri} accepted a session cookie"
            );

            // The shared bootstrap token passes; 404 (no such plugin) is fine.
            let (status, _) =
                send(&state, post(uri, Some(SHARED_TOKEN), serde_json::json!({}))).await;
            assert_ne!(
                status,
                StatusCode::UNAUTHORIZED,
                "{uri} refused the shared token"
            );
        }
    }

    #[tokio::test]
    async fn an_enrolled_gateways_token_opens_the_machine_plugin_endpoints() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let mut request = post(
            "/api/v1/enrollments",
            None,
            serde_json::json!({
                "customer_id": "cust-1", "customer_name": "Customer",
                "site_id": "site-1", "site_name": "Site", "city": "Barcelona",
            }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (_, created) = send(&state, request).await;
        let enrollment_token = created["enrollment_token"].as_str().unwrap().to_owned();
        let (_, enrolled) = send(
            &state,
            post(
                "/api/v1/gateways/enroll",
                None,
                serde_json::json!({
                    "enrollment_token": enrollment_token,
                    "gateway_id": "gw-1", "hostname": "edge-1", "version": "0.1.0",
                }),
            ),
        )
        .await;
        let gateway_token = enrolled["gateway_token"].as_str().unwrap().to_owned();

        let (status, _) = send(
            &state,
            post(
                "/api/v1/plugins/p-1/storage/uploads",
                Some(&gateway_token),
                serde_json::json!({}),
            ),
        )
        .await;
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "an enrolled gateway's own token must open the machine plugin endpoints"
        );
    }

    #[tokio::test]
    async fn gateway_machine_endpoints_take_bearer_tokens_not_cookies() {
        // The middleware must not swallow the machine surface.
        let state = test_state().await;
        seed_admin(&state).await;
        let (status, _) = send(
            &state,
            post(
                "/api/v1/cameras/telemetry",
                Some(SHARED_TOKEN),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_gateway_cannot_collect_another_gateways_commands() {
        // The path carries the gateway id, so an enrolled token must be checked
        // against it and not merely be present.
        let state = test_state().await;
        state
            .store
            .enroll_gateway(
                &EnrollmentRequest {
                    customer_id: "cust-1".into(),
                    customer_name: "Customer".into(),
                    site_id: "site-1".into(),
                    site_name: "Site".into(),
                    city: "Barcelona".into(),
                },
                &GatewayEnrollmentRequest {
                    enrollment_token: String::new(),
                    gateway_id: "gw-1".into(),
                    hostname: "edge".into(),
                    version: "0.1.0".into(),
                },
                "token-for-gw-1",
                Utc::now(),
            )
            .await
            .unwrap();
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

    #[tokio::test]
    async fn the_retention_pass_sweeps_expired_sessions() {
        let state = test_state().await;
        seed_admin(&state).await;
        let owner = state.store.users().await.unwrap().remove(0).id;
        let now = Utc::now();
        state
            .store
            .create_session(
                "expired",
                &owner,
                now - chrono::Duration::days(8),
                now - chrono::Duration::days(1),
            )
            .await
            .unwrap();
        state
            .store
            .create_session("alive", &owner, now, now + chrono::Duration::days(7))
            .await
            .unwrap();

        retention_pass(&state).await;

        assert!(
            state
                .store
                .session_user("expired", now - chrono::Duration::days(2))
                .await
                .unwrap()
                .is_none(),
            "the expired row should be gone even for a past `now`"
        );
        assert!(
            state
                .store
                .session_user("alive", now)
                .await
                .unwrap()
                .is_some()
        );
    }

    fn typed_batch(
        gateway_id: &str,
        camera_id: &str,
        status: HealthStatus,
        last_seen: chrono::DateTime<chrono::Utc>,
        last_error: Option<&str>,
    ) -> CameraTelemetryBatch {
        CameraTelemetryBatch {
            gateway_id: gateway_id.into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
            sent_at: last_seen,
            cameras: vec![CameraTelemetry {
                camera_id: camera_id.into(),
                gateway_id: gateway_id.into(),
                site_id: "site-1".into(),
                name: "Entrance".into(),
                status,
                manufacturer: None,
                model: None,
                firmware: None,
                profile_name: None,
                codec: None,
                width: None,
                height: None,
                fps: None,
                bitrate_kbps: None,
                packet_loss: 0,
                reconnects: 0,
                rtsp_endpoint: None,
                last_seen,
                last_error: last_error.map(Into::into),
            }],
        }
    }

    #[tokio::test]
    async fn a_silent_camera_opens_a_backdated_incident_and_recovery_closes_it() {
        let state = test_state().await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        // In the roster with an old last_seen and no live telemetry: silence.
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None),
                went_dark,
            )
            .await
            .unwrap();

        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert!(incidents[0].ended_at.is_none());
        let expected_start = went_dark + chrono::Duration::seconds(state.stale_camera_seconds);
        assert_eq!(
            incidents[0].started_at.timestamp(),
            expected_start.timestamp(),
            "silence must backdate to when the camera actually went dark"
        );
        assert_eq!(
            incidents[0].detail.as_deref(),
            Some("gateway telemetry is stale")
        );

        // A second silent pass must not open another one.
        incident_pass(&state).await;
        assert_eq!(state.store.incidents(10).await.unwrap().len(), 1);

        // Fresh healthy telemetry closes it.
        state.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None),
        );
        incident_pass(&state).await;
        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert!(
            incidents[0].ended_at.is_some(),
            "recovery must close the incident"
        );
    }

    #[tokio::test]
    async fn a_camera_reported_offline_opens_an_incident_with_its_error() {
        let state = test_state().await;
        let now = Utc::now();
        state
            .store
            .upsert_fleet_identity(
                &typed_batch(
                    "gw-1",
                    "cam-1",
                    HealthStatus::Offline,
                    now,
                    Some("rtsp: connection refused"),
                ),
                now,
            )
            .await
            .unwrap();
        state.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch(
                "gw-1",
                "cam-1",
                HealthStatus::Offline,
                now,
                Some("rtsp: connection refused"),
            ),
        );

        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert!(incidents[0].ended_at.is_none());
        assert_eq!(
            incidents[0].detail.as_deref(),
            Some("rtsp: connection refused")
        );
    }

    #[test]
    fn the_newest_report_wins_when_a_camera_appears_in_two_batches() {
        // Batches are keyed by gateway and never pruned, so a camera that
        // moved gateways exists in two of them until a restart. The winner
        // must be the freshest report, not whichever the map iterated last.
        let old = Utc::now() - chrono::Duration::seconds(300);
        let fresh = Utc::now();
        let mut batches = HashMap::new();
        batches.insert(
            "gw-old".to_string(),
            typed_batch(
                "gw-old",
                "cam-1",
                HealthStatus::Offline,
                old,
                Some("stale copy"),
            ),
        );
        batches.insert(
            "gw-new".to_string(),
            typed_batch("gw-new", "cam-1", HealthStatus::Healthy, fresh, None),
        );

        let live = newest_camera_map(&batches);

        assert_eq!(live.len(), 1);
        assert_eq!(live["cam-1"].gateway_id, "gw-new");
        assert_eq!(live["cam-1"].status, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn a_camera_that_moved_gateways_is_not_blamed_for_its_old_gateways_ghost() {
        // The false-incident scenario from the final review: the dead
        // gateway's last batch lingers with a stale offline copy. Eight
        // ghosts make an arbitrary-winner implementation fail reliably.
        let state = test_state().await;
        let now = Utc::now();
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-new", "cam-1", HealthStatus::Healthy, now, None),
                now,
            )
            .await
            .unwrap();
        let stale = now - chrono::Duration::seconds(300);
        for n in 0..8 {
            state.camera_batches.write().await.insert(
                format!("gw-ghost-{n}"),
                typed_batch(
                    &format!("gw-ghost-{n}"),
                    "cam-1",
                    HealthStatus::Offline,
                    stale,
                    Some("stale copy"),
                ),
            );
        }
        state.camera_batches.write().await.insert(
            "gw-new".into(),
            typed_batch("gw-new", "cam-1", HealthStatus::Healthy, now, None),
        );

        incident_pass(&state).await;
        assert!(
            state.store.incidents(10).await.unwrap().is_empty(),
            "a moved camera must not get an incident from its old gateway's ghost"
        );

        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let mut request = get("/api/v1/cameras");
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, cameras) = send(&state, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cameras[0]["camera_id"], "cam-1");
        assert_eq!(
            cameras[0]["status"], "healthy",
            "the live view must agree with the incident history: {cameras}"
        );
    }

    #[tokio::test]
    async fn a_flap_is_two_incidents() {
        let state = test_state().await;
        let now = Utc::now();
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Offline, now, None),
                now,
            )
            .await
            .unwrap();
        state.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Offline, now, None),
        );
        incident_pass(&state).await;
        state.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None),
        );
        incident_pass(&state).await;
        state.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Offline, Utc::now(), None),
        );
        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 2);
        assert_eq!(incidents.iter().filter(|i| i.ended_at.is_none()).count(), 1);
    }

    #[tokio::test]
    async fn the_startup_grace_skips_sweeps_until_gateways_can_report() {
        let mut state = test_state().await;
        state.incident_grace = Duration::from_secs(3600);
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None),
                went_dark,
            )
            .await
            .unwrap();

        incident_pass(&state).await;

        assert!(
            state.store.incidents(10).await.unwrap().is_empty(),
            "a sweep inside the grace window must not blame a deploy on the cameras"
        );
    }

    #[tokio::test]
    async fn a_session_survives_an_api_restart() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("store"),
        );
        let state = test_state_with(store).await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url)
                .await
                .expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let mut request = get("/api/v1/auth/session");
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&restarted, request).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "a login must survive an API restart; that is what storing sessions is for"
        );
    }

    async fn audit_actions(state: &AppState, cookie: &str) -> Vec<String> {
        let mut request = get("/api/v1/audit");
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, body) = send(state, request).await;
        assert_eq!(status, StatusCode::OK);
        body.as_array()
            .expect("a list")
            .iter()
            .map(|entry| entry["action"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn security_events_land_in_the_audit_log() {
        let state = test_state().await;
        seed_admin(&state).await;

        // A failed login, then a good one.
        let (_, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "email": ADMIN_EMAIL, "password": "wrong" }),
            ),
        )
        .await;
        let cookie = login_cookie(&state).await;

        // Enrollment token minted, gateway enrolled, then revoked.
        let _ = enrolled_gateway_token(&state, &cookie).await;
        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        send(&state, request).await;

        // Password changed (which re-mints the caller's session).
        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": ADMIN_PASSWORD, "new": "an entirely new passphrase" }),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let fresh = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let actions = audit_actions(&state, &fresh).await;
        for expected in [
            "login.failed",
            "login.ok",
            "enrollment.created",
            "gateway.enrolled",
            "gateway.revoked",
            "password.changed",
        ] {
            assert!(
                actions.iter().any(|a| a == expected),
                "missing {expected} in {actions:?}"
            );
        }
        // Nothing secret in any row.
        let mut request = get("/api/v1/audit");
        request
            .headers_mut()
            .insert("cookie", fresh.parse().unwrap());
        let (_, body) = send(&state, request).await;
        let dump = body.to_string();
        assert!(
            !dump.contains(ADMIN_PASSWORD),
            "a password reached the audit log"
        );
    }

    #[tokio::test]
    async fn a_forced_password_reset_is_audited() {
        let state = test_state().await;
        seed_admin(&state).await;
        crate::auth::seed_credentials(
            state.store.as_ref(),
            ADMIN_EMAIL,
            Some("a replacement passphrase"),
            true,
        )
        .await
        .unwrap();
        let entries = state.store.audit_entries(10).await.unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.action == "password.reset" && e.actor == "system"),
            "the loud reset must leave a row: {entries:?}"
        );
    }

    #[tokio::test]
    async fn audit_retention_prunes_only_when_told_to() {
        let state = test_state().await;
        let now = Utc::now();
        state
            .store
            .record_audit(
                now - chrono::Duration::days(400),
                "admin",
                "login.ok",
                "",
                None,
            )
            .await
            .unwrap();

        // Default 0: keep forever.
        retention_pass(&state).await;
        assert_eq!(state.store.audit_entries(10).await.unwrap().len(), 1);

        let mut state = state;
        state.audit_retention_days = 365;
        retention_pass(&state).await;
        assert!(state.store.audit_entries(10).await.unwrap().is_empty());
    }

    fn incident_for<'a>(incidents: &'a [IncidentView], camera_id: &str) -> Vec<&'a IncidentView> {
        incidents
            .iter()
            .filter(|incident| incident.camera_id == camera_id)
            .collect()
    }

    async fn revoke_through_the_dashboard(state: &AppState, cookie: &str, gateway_id: &str) {
        let mut request = post(
            &format!("/api/v1/gateways/{gateway_id}/revoke"),
            None,
            serde_json::json!({}),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(state, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn revoking_a_gateway_closes_its_camera_incidents_and_the_pass_leaves_them_closed() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        for (gateway, camera) in [("gw-1", "cam-1"), ("gw-2", "cam-2")] {
            state
                .store
                .upsert_fleet_identity(
                    &typed_batch(gateway, camera, HealthStatus::Healthy, went_dark, None),
                    went_dark,
                )
                .await
                .unwrap();
        }
        incident_pass(&state).await;
        assert_eq!(
            state
                .store
                .incidents(10)
                .await
                .unwrap()
                .iter()
                .filter(|i| i.ended_at.is_none())
                .count(),
            2
        );

        revoke_through_the_dashboard(&state, &cookie, "gw-1").await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert!(
            incident_for(&incidents, "cam-1")[0].ended_at.is_some(),
            "the revoke itself must close the revoked gateway's incident"
        );
        assert!(
            incident_for(&incidents, "cam-2")[0].ended_at.is_none(),
            "another gateway's outage is still an outage"
        );

        incident_pass(&state).await;
        let incidents = state.store.incidents(10).await.unwrap();
        let cam1 = incident_for(&incidents, "cam-1");
        assert_eq!(
            cam1.len(),
            1,
            "the pass reopened a revoked gateway's camera: {cam1:?}"
        );
        assert!(cam1[0].ended_at.is_some());
        assert!(incident_for(&incidents, "cam-2")[0].ended_at.is_none());
    }

    /// The whole point of alerts: the camera goes quiet, and something
    /// outside this building hears about it without anyone looking.
    #[tokio::test]
    async fn a_camera_going_quiet_raises_one_event_and_coming_back_raises_another() {
        let (endpoint, seen) = fake_sink("sink-1", true).await;
        let state = state_with_sinks(&[("sink", &endpoint)]).await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None),
                went_dark,
            )
            .await
            .unwrap();

        incident_pass(&state).await;
        delivery_pass(&state, Utc::now()).await;
        let sent = seen.read().await.clone();
        assert_eq!(sent.len(), 1, "one outage, one alert: {sent:?}");
        assert_eq!(sent[0]["event"]["kind"], "camera_offline");
        assert_eq!(sent[0]["event"]["severity"], "critical");
        assert!(
            sent[0]["event"]["title"]
                .as_str()
                .is_some_and(|title| title.contains("stopped answering")),
            "the title is written for a human: {:?}",
            sent[0]["event"]["title"]
        );

        // Still down on the next pass: the same outage, not a new one.
        incident_pass(&state).await;
        delivery_pass(&state, Utc::now()).await;
        assert_eq!(
            seen.read().await.len(),
            1,
            "an outage that lasts an hour is not sixty alerts"
        );

        // And back.
        let now = Utc::now();
        let batch = typed_batch("gw-1", "cam-1", HealthStatus::Healthy, now, None);
        state
            .store
            .upsert_fleet_identity(&batch, now)
            .await
            .unwrap();
        state
            .camera_batches
            .write()
            .await
            .insert("gw-1".into(), batch);
        incident_pass(&state).await;
        delivery_pass(&state, Utc::now()).await;
        let sent = seen.read().await.clone();
        assert_eq!(sent.len(), 2, "coming back is news too: {sent:?}");
        assert_eq!(sent[1]["event"]["kind"], "camera_recovered");
        assert_eq!(sent[1]["event"]["severity"], "info");
    }

    /// A gateway going quiet takes every camera behind it with it, and it is
    /// the one outage a site cannot report itself.
    #[tokio::test]
    async fn a_gateway_that_stops_reporting_is_announced_once() {
        let (endpoint, seen) = fake_sink("sink-1", true).await;
        let mut state = state_with_sinks(&[("sink", &endpoint)]).await;
        state.incident_grace = Duration::from_secs(0);

        let fresh = Utc::now();
        let batch = typed_batch("gw-1", "cam-1", HealthStatus::Healthy, fresh, None);
        state
            .store
            .upsert_fleet_identity(&batch, fresh)
            .await
            .unwrap();
        state.gateways.write().await.insert(
            "gw-1".into(),
            GatewayHeartbeat {
                gateway_id: "gw-1".into(),
                site_id: "site-1".into(),
                hostname: "edge-1".into(),
                version: "0.1.0".into(),
                uptime_seconds: 10,
                cpu_percent: 1.0,
                memory_percent: 1.0,
                cameras_seen: 1,
                healthy_cameras: 1,
                warning_cameras: 0,
                offline_cameras: 0,
                sent_at: fresh,
            },
        );

        let mut reporting = HashMap::new();
        gateway_pass(&state, &mut reporting).await;
        delivery_pass(&state, Utc::now()).await;
        assert!(
            seen.read().await.is_empty(),
            "the first sight of a gateway is not news"
        );

        // It stops reporting: the heartbeat ages out.
        let stale = Utc::now() - chrono::Duration::seconds(3_600);
        state
            .gateways
            .write()
            .await
            .get_mut("gw-1")
            .unwrap()
            .sent_at = stale;
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, stale, None),
                stale,
            )
            .await
            .unwrap();

        gateway_pass(&state, &mut reporting).await;
        delivery_pass(&state, Utc::now()).await;
        let sent = seen.read().await.clone();
        assert_eq!(sent.len(), 1, "one silence, one alert: {sent:?}");
        assert_eq!(sent[0]["event"]["kind"], "gateway_offline");
        assert_eq!(sent[0]["event"]["severity"], "critical");
        assert!(
            sent[0]["event"]["title"]
                .as_str()
                .is_some_and(|title| title.contains("edge-1")),
            "the alert names the box: {:?}",
            sent[0]["event"]["title"]
        );

        gateway_pass(&state, &mut reporting).await;
        delivery_pass(&state, Utc::now()).await;
        assert_eq!(
            seen.read().await.len(),
            1,
            "a gateway that is still gone is still the same outage"
        );

        // And it comes back.
        let now = Utc::now();
        state
            .gateways
            .write()
            .await
            .get_mut("gw-1")
            .unwrap()
            .sent_at = now;
        gateway_pass(&state, &mut reporting).await;
        delivery_pass(&state, Utc::now()).await;
        let sent = seen.read().await.clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["event"]["kind"], "gateway_recovered");
    }

    #[tokio::test]
    async fn a_revoked_gateway_going_quiet_is_a_decision_not_an_outage() {
        let (endpoint, seen) = fake_sink("sink-1", true).await;
        let mut state = state_with_sinks(&[("sink", &endpoint)]).await;
        state.incident_grace = Duration::from_secs(0);
        let fresh = Utc::now();
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, fresh, None),
                fresh,
            )
            .await
            .unwrap();
        let mut reporting = HashMap::new();
        gateway_pass(&state, &mut reporting).await;

        state
            .store
            .revoke_gateway("gw-1", Utc::now())
            .await
            .unwrap();
        gateway_pass(&state, &mut reporting).await;
        delivery_pass(&state, Utc::now()).await;
        assert!(seen.read().await.is_empty(), "nobody needs telling");
    }

    #[tokio::test]
    async fn the_pass_closes_an_incident_left_open_on_a_revoked_gateway() {
        // A gateway revoked before this policy existed, or a revoke whose own
        // close failed: the store says revoked, the incident is still open.
        let state = test_state().await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None),
                went_dark,
            )
            .await
            .unwrap();
        incident_pass(&state).await;
        state
            .store
            .revoke_gateway("gw-1", Utc::now())
            .await
            .unwrap();
        assert!(
            state.store.incidents(10).await.unwrap()[0]
                .ended_at
                .is_none()
        );

        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert!(
            incidents[0].ended_at.is_some(),
            "the pass must heal a stranded open incident"
        );
    }

    #[tokio::test]
    async fn reenrolling_a_revoked_gateway_resumes_incident_tracking() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let _ = enrolled_gateway_token(&state, &cookie).await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        state
            .store
            .upsert_fleet_identity(
                &typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None),
                went_dark,
            )
            .await
            .unwrap();
        revoke_through_the_dashboard(&state, &cookie, "gw-1").await;
        incident_pass(&state).await;
        assert!(
            state.store.incidents(10).await.unwrap().is_empty(),
            "a revoked gateway's camera opened an incident"
        );

        let _ = enrolled_gateway_token(&state, &cookie).await;
        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(
            incidents.len(),
            1,
            "re-enrollment must bring the camera back under watch"
        );
        assert!(incidents[0].ended_at.is_none());
    }
}
