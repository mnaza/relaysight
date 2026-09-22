mod auth;
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
use tracing::{info, warn};
use uuid::Uuid;
use vms_domain::{
    AiAnalysisRequest as CameraAiAnalysisRequest, AuditView, CameraSummary, CameraTelemetry,
    CameraTelemetryBatch, CommandAccepted, CustomerSummary, EditionEntitlement, EnrollmentCreated,
    EnrollmentRequest, FleetSnapshot, FleetSource, GatewayCommand, GatewayCommandKind,
    GatewayCommandResult, GatewayCommandStatus, GatewayCommandView, GatewayEnrollmentRequest,
    GatewayEnrollmentResponse, GatewayHeartbeat, GatewayView, HealthStatus, IncidentView,
    LiveSessionRequest, PlaybackManifest, PlaybackSegment, RecordingRequest, RecordingTimeline,
    RtcConfigResponse, SiteSummary,
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
    auth::seed_admin_credential(store.as_ref(), admin_password.as_deref(), force_reset).await?;
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
    let protected = Router::new()
        .route("/api/v1/fleet", get(fleet))
        .route("/api/v1/incidents", get(incidents))
        .route("/api/v1/audit", get(audit_entries))
        .route("/api/v1/enrollments", post(create_enrollment))
        .route("/api/v1/cameras", get(cameras))
        .route("/api/v1/gateways", get(gateways))
        .route("/api/v1/gateways/{gateway_id}/revoke", post(revoke_gateway))
        .route(
            "/api/v1/gateways/{gateway_id}/cameras/retire",
            post(retire_gateway_cameras),
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
            "/api/v1/plugins/{plugin_id}/storage/downloads",
            post(plugin_storage_download),
        )
        .route(
            "/api/v1/plugins/{plugin_id}/storage/delete",
            post(plugin_storage_delete),
        )
        .route("/api/v1/auth/logout", post(crate::auth::auth_logout))
        .route(
            "/api/v1/auth/password",
            post(crate::auth::auth_change_password),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_session,
        ));
    open.merge(machine_plugins)
        .merge(protected)
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
    state
        .camera_batches
        .write()
        .await
        .insert(batch.gateway_id.clone(), batch);
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

async fn gateways(State(state): State<AppState>) -> Result<Json<Vec<GatewayView>>, StatusCode> {
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
    views.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    Ok(Json(views))
}

async fn cameras(State(state): State<AppState>) -> Result<Json<Vec<CameraTelemetry>>, StatusCode> {
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

async fn fleet(State(state): State<AppState>) -> Result<Json<FleetSnapshot>, StatusCode> {
    // Identity from the store, status from memory.
    let orgs = state.store.fleet_identity().await.map_err(store_status)?;
    let batches = state.camera_batches.read().await;
    if orgs.is_empty() {
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
) -> Result<Json<RecordingTimeline>, StatusCode> {
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

async fn incidents(State(state): State<AppState>) -> Result<Json<Vec<IncidentView>>, StatusCode> {
    // Open first, newest-closed after; 200 is plenty for a screen.
    state
        .store
        .incidents(200)
        .await
        .map(Json)
        .map_err(store_status)
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
) -> Result<Json<PlaybackManifest>, StatusCode> {
    let recording = state
        .store
        .recording(&recording_id)
        .await
        .map_err(store_status)?;
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
        retention_pass(&state).await;
        incident_pass(&state).await;
    }
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
        let result = if stale {
            let started = last_seen + chrono::Duration::seconds(state.stale_camera_seconds);
            state
                .store
                .open_incident(&record.id, started, Some("gateway telemetry is stale"))
                .await
        } else if reported_offline {
            state
                .store
                .open_incident(&record.id, now, last_error.as_deref())
                .await
        } else {
            state.store.close_incident(&record.id, now).await
        };
        if let Err(err) = result {
            warn!(camera_id = %record.id, error = %err, "incident pass store failure");
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

    /// Seed the admin credential the way main() does at startup.
    async fn seed_admin(state: &AppState) {
        crate::auth::seed_admin_credential(state.store.as_ref(), Some(ADMIN_PASSWORD), false)
            .await
            .expect("seed admin credential");
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
                serde_json::json!({ "password": ADMIN_PASSWORD }),
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
                serde_json::json!({ "password": "wrong" }),
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
                    serde_json::json!({ "password": "wrong" }),
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
                serde_json::json!({ "password": ADMIN_PASSWORD }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            &state,
            post(
                "/api/v1/auth/login",
                None,
                serde_json::json!({ "password": "an entirely new passphrase" }),
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
                serde_json::json!({ "password": ADMIN_PASSWORD }),
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
        let now = Utc::now();
        state
            .store
            .create_session(
                "expired",
                now - chrono::Duration::days(8),
                now - chrono::Duration::days(1),
            )
            .await
            .unwrap();
        state
            .store
            .create_session("alive", now, now + chrono::Duration::days(7))
            .await
            .unwrap();

        retention_pass(&state).await;

        assert!(
            !state
                .store
                .session_is_valid("expired", now - chrono::Duration::days(2))
                .await
                .unwrap(),
            "the expired row should be gone even for a past `now`"
        );
        assert!(state.store.session_is_valid("alive", now).await.unwrap());
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
                serde_json::json!({ "password": "wrong" }),
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
        crate::auth::seed_admin_credential(
            state.store.as_ref(),
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
