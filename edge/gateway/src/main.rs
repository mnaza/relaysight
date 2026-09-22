mod archive;
mod backoff;
mod camera_credentials;
#[cfg(test)]
mod fake_browser;
#[cfg(test)]
mod fake_camera;
#[cfg(test)]
mod fake_control_plane;
mod icepath;
mod identity;
mod live;
mod onvif;
mod release;
mod rtsp;
mod snapshot;
mod turn_bridge;
mod update;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::backoff::{Backoff, DEFAULT_CAP};
use vms_domain::{
    AiAnalysisResult, AiBoundingBox, AiDetectionResult, CameraTelemetry, CameraTelemetryBatch,
    GatewayCommand, GatewayCommandKind, GatewayCommandResult, GatewayCommandStatus,
    GatewayEnrollmentRequest, GatewayEnrollmentResponse, GatewayHeartbeat, HealthStatus,
    LiveSessionAnswer, RecordingManifest, RecordingObject, RecordingSegment,
};
use vms_plugin_sdk::{
    AiAnalyzeRequest as PluginAiAnalyzeRequest, AiAnalyzeResponse as PluginAiAnalyzeResponse,
    MediaInput, PluginInvocationContext, SignedTransfer, StorageUploadRequest, TransferAudience,
};

#[derive(Clone)]
struct Config {
    api_url: String,
    gateway_id: String,
    customer_id: String,
    customer_name: String,
    site_id: String,
    site_name: String,
    city: String,
    token: String,
    enrollment_token: Option<String>,
    camera_limit: usize,
    heartbeat_interval: Duration,
    discovery_wait: Duration,
    probe_interval: Duration,
    rtsp_probe_window: Duration,
    command_poll_interval: Duration,
    camera_username: Option<String>,
    camera_password: Option<String>,
    /// Per-camera credentials, keyed by address. The pair above is what a
    /// camera without its own entry still uses.
    camera_credentials: Arc<crate::camera_credentials::CameraCredentials>,
    /// Where the heartbeat marker goes. `None` for a gateway with no state to keep.
    state_dir: Option<std::path::PathBuf>,
    explicit_rtsp_url: Option<String>,
    explicit_camera_name: String,
    /// Addresses to talk ONVIF to directly, skipping multicast discovery.
    onvif_hosts: Vec<String>,
}

impl Config {
    /// What to present to the camera at this address: its own credentials when
    /// it has been given some, otherwise the pair every camera shared before
    /// this existed. Both halves are optional, as before: a camera on an open
    /// network needs neither.
    fn camera_login(&self, address: &str) -> (Option<String>, Option<String>) {
        match self.camera_credentials.get(address) {
            Some(found) => (Some(found.username.clone()), Some(found.password.clone())),
            None => (self.camera_username.clone(), self.camera_password.clone()),
        }
    }

    /// The same, shaped for ONVIF, which wants both or neither.
    fn onvif_login(&self, address: &str) -> Option<onvif::Credentials> {
        match self.camera_login(address) {
            (Some(username), Some(password)) => Some(onvif::Credentials { username, password }),
            _ => None,
        }
    }

    fn from_env() -> Self {
        Self {
            api_url: env::var("API_URL").unwrap_or_else(|_| "http://localhost:8080".into()),
            gateway_id: env::var("GATEWAY_ID").unwrap_or_else(|_| "demo-gateway-01".into()),
            customer_id: env::var("CUSTOMER_ID").unwrap_or_else(|_| "pilot-customer".into()),
            customer_name: env::var("CUSTOMER_NAME").unwrap_or_else(|_| "Pilot customer".into()),
            site_id: env::var("SITE_ID").unwrap_or_else(|_| "demo-site".into()),
            site_name: env::var("SITE_NAME").unwrap_or_else(|_| "Demo site".into()),
            city: env::var("SITE_CITY").unwrap_or_default(),
            token: env::var("GATEWAY_TOKEN").unwrap_or_else(|_| "demo-local-token".into()),
            enrollment_token: env::var("ENROLLMENT_TOKEN").ok().filter(|s| !s.is_empty()),
            camera_limit: env::var("CAMERA_LIMIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            heartbeat_interval: duration_env("HEARTBEAT_INTERVAL_SECONDS", 10),
            discovery_wait: duration_env("ONVIF_DISCOVERY_SECONDS", 3),
            probe_interval: duration_env("CAMERA_PROBE_INTERVAL_SECONDS", 30),
            rtsp_probe_window: duration_env("RTSP_PROBE_SECONDS", 5),
            command_poll_interval: duration_env("COMMAND_POLL_INTERVAL_SECONDS", 1),
            camera_username: env::var("CAMERA_USERNAME").ok().filter(|s| !s.is_empty()),
            camera_password: env::var("CAMERA_PASSWORD").ok().filter(|s| !s.is_empty()),
            camera_credentials: Arc::new(crate::camera_credentials::CameraCredentials::empty()),
            state_dir: None,
            explicit_rtsp_url: env::var("CAMERA_RTSP_URL").ok().filter(|s| !s.is_empty()),
            explicit_camera_name: env::var("CAMERA_NAME")
                .unwrap_or_else(|_| "Manual RTSP camera".into()),
            onvif_hosts: env::var("ONVIF_HOSTS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }
}

/// `vms-gateway update`, run by the update timer as root: take a newer signed release, and
/// give it back if the gateway does not prove itself on it.
async fn run_update() -> anyhow::Result<()> {
    let channel =
        env::var("GATEWAY_UPDATE_URL").unwrap_or_else(|_| update::DEFAULT_CHANNEL.to_owned());
    let state_dir = env::var("GATEWAY_STATE_DIR").unwrap_or_else(|_| "data/gateway".into());
    let unit = env::var("GATEWAY_SERVICE_UNIT").unwrap_or_else(|_| "relaysight-gateway".into());
    let wait = duration_env("GATEWAY_UPDATE_WAIT_SECONDS", 90);
    let current = VERSION;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let fetched = update::fetch(
        &client,
        &channel,
        &update::release_public_key(),
        current,
        std::env::consts::ARCH,
    )
    .await?;
    let Some((version, binary)) = fetched else {
        println!("vms-gateway {current} is up to date");
        return Ok(());
    };

    let path = std::env::current_exe()?;
    let mut service = update::Systemd {
        unit,
        marker: std::path::Path::new(&state_dir).join(update::HEARTBEAT_MARKER),
        wait,
    };
    // The swap waits on the service in a plain loop; keep it off the async workers.
    let outcome = tokio::task::spawn_blocking(move || {
        update::swap_and_confirm(&path, &binary, &mut service, current, &version)
    })
    .await??;
    match outcome {
        update::Outcome::Updated { from, to } => {
            println!("updated vms-gateway {from} -> {to}");
            Ok(())
        }
        update::Outcome::RolledBack { from, to } => {
            anyhow::bail!("vms-gateway {to} did not come up within {wait:?}; rolled back to {from}")
        }
    }
}

/// This build's version: the release tag's, when CI sets `GATEWAY_VERSION` at build time,
/// otherwise the crate's. Heartbeats report it and updates compare against it.
const VERSION: &str = match option_env!("GATEWAY_VERSION") {
    // An empty one is a build that meant to leave it unset.
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

/// One line from stdin, so the password is never an argument in `ps` or a
/// line in a shell history.
fn read_password() -> anyhow::Result<String> {
    let mut password = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut password)?;
    Ok(password)
}

#[derive(Clone, Debug)]
struct CameraSource {
    rtsp_uri: String,
    /// Stream used for live view — the camera's substream where it has one, so a
    /// TURN-relayed session carries a fraction of the bytes. See docs/TURN-COSTS.md.
    live_rtsp_uri: String,
    snapshot_uri: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

fn duration_env(name: &str, default_secs: u64) -> Duration {
    Duration::from_secs(
        env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_secs),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // One subcommand, checked before anything starts: `credentials` is run by
    // hand on the box, not by the service.
    let args: Vec<String> = env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|first| first == "--version" || first == "version")
    {
        println!("vms-gateway {}", VERSION);
        return Ok(());
    }
    if args.first().is_some_and(|first| first == "update") {
        return run_update().await;
    }
    if args.first().is_some_and(|first| first == "credentials") {
        let state_dir = env::var("GATEWAY_STATE_DIR").unwrap_or_else(|_| "data/gateway".into());
        let state_key = env::var("GATEWAY_STATE_KEY").ok().filter(|k| !k.is_empty());
        return camera_credentials::run(
            std::path::Path::new(&state_dir),
            state_key.as_deref(),
            &args[1..],
            read_password,
            &mut std::io::stdout(),
        );
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vms_gateway=info".into()),
        )
        .init();

    let mut config = Config::from_env();
    let client = reqwest::Client::builder().build()?;
    let cameras = Arc::new(RwLock::new(Vec::<CameraTelemetry>::new()));
    let sources = Arc::new(RwLock::new(HashMap::<String, CameraSource>::new()));
    let reconnects = Arc::new(RwLock::new(HashMap::<String, u32>::new()));
    // Doubling from the probe interval. A camera that stops answering is dialled
    // less and less often instead of once every interval forever; see backoff.rs
    // for what that trades away.
    let backoff = Arc::new(RwLock::new(Backoff::new(
        config.probe_interval,
        DEFAULT_CAP,
    )));
    let started = Instant::now();
    let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "edge-node".into());

    let state_dir = env::var("GATEWAY_STATE_DIR").unwrap_or_else(|_| "data/gateway".into());
    let state_key = env::var("GATEWAY_STATE_KEY").ok().filter(|k| !k.is_empty());
    let store =
        identity::IdentityStore::open(std::path::Path::new(&state_dir), state_key.as_deref())?;
    // A credential file that will not open stops the gateway rather than
    // letting every camera quietly fall back to the shared password.
    let credentials = camera_credentials::CameraCredentials::load(
        std::path::Path::new(&state_dir),
        state_key.as_deref(),
    )?;
    if !credentials.hosts().is_empty() {
        info!(
            cameras = credentials.hosts().len(),
            "per-camera credentials loaded"
        );
    }
    config.camera_credentials = Arc::new(credentials);
    config.state_dir = Some(std::path::PathBuf::from(&state_dir));
    let reenroll = env::var("GATEWAY_REENROLL").is_ok_and(|v| v == "true");
    establish_identity(&mut config, &client, &hostname, &store, reenroll).await?;
    info!(gateway_id = %config.gateway_id, site_id = %config.site_id, camera_limit = config.camera_limit, "gateway started");

    let probe_task = tokio::spawn(probe_loop(
        config.clone(),
        client.clone(),
        cameras.clone(),
        reconnects,
        backoff,
        sources.clone(),
    ));
    let command_task = tokio::spawn(command_loop(config.clone(), client.clone(), sources));
    let heartbeat_task = tokio::spawn(heartbeat_loop(config, client, cameras, hostname, started));

    tokio::select! {
        result = probe_task => result??,
        result = command_task => result??,
        result = heartbeat_task => result??,
        _ = tokio::signal::ctrl_c() => info!("shutdown requested"),
    }
    Ok(())
}

/// Boot-time identity: use what enrollment earned last time, or earn it now
/// and write it down. See identity::boot_plan for the decision table.
async fn establish_identity(
    config: &mut Config,
    client: &reqwest::Client,
    hostname: &str,
    store: &identity::IdentityStore,
    reenroll: bool,
) -> anyhow::Result<()> {
    let loaded = if reenroll { Ok(None) } else { store.load() };
    match identity::boot_plan(loaded, config.enrollment_token.is_some(), reenroll)? {
        identity::BootPlan::UsePersisted(id) => {
            info!(gateway_id = %id.gateway_id, site = %id.site_name, "using persisted gateway identity");
            config.token = id.gateway_token;
            config.gateway_id = id.gateway_id;
            config.customer_id = id.customer_id;
            config.customer_name = id.customer_name;
            config.site_id = id.site_id;
            config.site_name = id.site_name;
            config.city = id.city;
            config.camera_limit = id.camera_limit;
            // The env token is one-shot and already burned; never spend it again.
            config.enrollment_token = None;
        }
        identity::BootPlan::Enroll => {
            if reenroll {
                store.wipe()?;
                warn!("GATEWAY_REENROLL: wiped persisted identity, enrolling fresh");
            }
            enroll_if_requested(config, client, hostname).await?;
            store.save(&identity::GatewayIdentity {
                version: identity::CURRENT_VERSION,
                gateway_token: config.token.clone(),
                gateway_id: config.gateway_id.clone(),
                customer_id: config.customer_id.clone(),
                customer_name: config.customer_name.clone(),
                site_id: config.site_id.clone(),
                site_name: config.site_name.clone(),
                city: config.city.clone(),
                camera_limit: config.camera_limit,
            })?;
            info!("gateway identity persisted");
        }
        identity::BootPlan::Bootstrap => {}
    }
    Ok(())
}

async fn enroll_if_requested(
    config: &mut Config,
    client: &reqwest::Client,
    hostname: &str,
) -> anyhow::Result<()> {
    let Some(enrollment_token) = config.enrollment_token.take() else {
        return Ok(());
    };
    let endpoint = format!(
        "{}/api/v1/gateways/enroll",
        config.api_url.trim_end_matches('/')
    );
    let request = GatewayEnrollmentRequest {
        enrollment_token,
        gateway_id: config.gateway_id.clone(),
        hostname: hostname.to_owned(),
        version: VERSION.into(),
    };
    let response = client.post(endpoint).json(&request).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("gateway enrollment failed with HTTP {}", response.status());
    }
    let enrolled: GatewayEnrollmentResponse = response.json().await?;
    config.token = enrolled.gateway_token;
    config.customer_id = enrolled.customer_id;
    config.customer_name = enrolled.customer_name;
    config.site_id = enrolled.site_id;
    config.site_name = enrolled.site_name;
    config.city = enrolled.city;
    config.camera_limit = enrolled.entitlement.camera_limit.unwrap_or(0);
    info!(edition = ?enrolled.entitlement.edition, plan = %enrolled.entitlement.plan, camera_limit = ?enrolled.entitlement.camera_limit, "gateway entitlement applied");
    Ok(())
}

fn health_rank(status: &HealthStatus) -> u8 {
    match status {
        HealthStatus::Healthy => 0,
        HealthStatus::Warning => 1,
        HealthStatus::Offline => 2,
    }
}

async fn probe_loop(
    config: Config,
    client: reqwest::Client,
    shared: Arc<RwLock<Vec<CameraTelemetry>>>,
    reconnects: Arc<RwLock<HashMap<String, u32>>>,
    backoff: Arc<RwLock<Backoff>>,
    sources: Arc<RwLock<HashMap<String, CameraSource>>>,
) -> anyhow::Result<()> {
    loop {
        let mut telemetry = Vec::new();
        let mut fresh_sources = HashMap::new();
        // Discovery is multicast, so it only reaches the local segment. Set
        // ONVIF_DISCOVERY_SECONDS=0 on a routed network to stop paying for a
        // probe that cannot succeed, and name the cameras in ONVIF_HOSTS instead.
        let mut devices = if config.discovery_wait.is_zero() {
            debug!("ONVIF discovery disabled");
            Vec::new()
        } else {
            match onvif::discover(config.discovery_wait).await {
                Ok(found) => {
                    info!(count = found.len(), "ONVIF discovery completed");
                    found
                }
                Err(error) => {
                    warn!(%error, "ONVIF discovery failed");
                    Vec::new()
                }
            }
        };

        // A camera can be both discovered and configured. Discovery wins, because
        // its endpoint reference is a stable identity while an address is only a
        // place — and resolving the same camera twice would give it two ids and
        // list it twice.
        let discovered: HashSet<String> = devices
            .iter()
            .flat_map(|d| d.xaddrs.iter())
            .filter_map(|x| onvif::xaddr_authority(x))
            .collect();

        for address in &config.onvif_hosts {
            match onvif::device_from_address(address) {
                Ok(device) => {
                    let authority = device
                        .xaddrs
                        .first()
                        .and_then(|x| onvif::xaddr_authority(x));
                    if authority.is_some_and(|a| discovered.contains(&a)) {
                        debug!(%address, "already found by discovery");
                        continue;
                    }
                    devices.push(device);
                }
                Err(error) => warn!(%address, %error, "bad ONVIF_HOSTS entry"),
            }
        }

        {
            {
                for device in devices {
                    // Whose camera this is decides which password it gets.
                    let address = device
                        .xaddrs
                        .first()
                        .and_then(|x| onvif::xaddr_authority(x))
                        .unwrap_or_default();
                    let credentials = config.onvif_login(&address);
                    match onvif::resolve_camera(&client, &device, credentials.as_ref()).await {
                        Ok(candidate) => {
                            let (username, password) = config.camera_login(&candidate.rtsp_uri);
                            fresh_sources.insert(
                                candidate.camera_id.clone(),
                                CameraSource {
                                    rtsp_uri: candidate.rtsp_uri.clone(),
                                    live_rtsp_uri: candidate.live_rtsp_uri.clone(),
                                    snapshot_uri: candidate.snapshot_uri.clone(),
                                    username,
                                    password,
                                },
                            );
                            telemetry.push(
                                probe_candidate(&config, &candidate, &reconnects, &backoff).await,
                            );
                        }
                        Err(error) => {
                            let identity = device
                                .endpoint_reference
                                .clone()
                                .or_else(|| device.xaddrs.first().cloned())
                                .unwrap_or_else(|| "unknown-onvif-camera".into());
                            warn!(camera = %identity, %error, "ONVIF camera resolution failed");
                            telemetry.push(CameraTelemetry {
                                camera_id: uuid::Uuid::new_v5(
                                    &uuid::Uuid::NAMESPACE_URL,
                                    identity.as_bytes(),
                                )
                                .to_string(),
                                gateway_id: config.gateway_id.clone(),
                                site_id: config.site_id.clone(),
                                name: identity,
                                status: HealthStatus::Warning,
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
                                last_seen: Utc::now(),
                                last_error: Some(error.to_string()),
                            });
                        }
                    }
                }
            }
        }

        if let Some(url) = &config.explicit_rtsp_url {
            let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, url.as_bytes()).to_string();
            if !telemetry.iter().any(|camera| camera.camera_id == id) {
                let login = config.camera_login(url);
                fresh_sources.insert(
                    id.clone(),
                    CameraSource {
                        rtsp_uri: url.clone(),
                        // An explicitly configured URL names one stream; there is
                        // no profile list to pick a substream from.
                        live_rtsp_uri: url.clone(),
                        snapshot_uri: None,
                        username: login.0,
                        password: login.1,
                    },
                );
                telemetry.push(probe_explicit(&config, id, url, &reconnects, &backoff).await);
            }
        }

        telemetry.sort_by(|a, b| {
            health_rank(&a.status)
                .cmp(&health_rank(&b.status))
                .then_with(|| a.name.cmp(&b.name))
        });
        if config.camera_limit > 0 && telemetry.len() > config.camera_limit {
            telemetry.truncate(config.camera_limit);
        }
        fresh_sources.retain(|camera_id, _| {
            telemetry
                .iter()
                .any(|camera| &camera.camera_id == camera_id)
        });
        *sources.write().await = fresh_sources;

        let batch = CameraTelemetryBatch {
            gateway_id: config.gateway_id.clone(),
            customer_id: config.customer_id.clone(),
            customer_name: config.customer_name.clone(),
            site_id: config.site_id.clone(),
            site_name: config.site_name.clone(),
            city: config.city.clone(),
            sent_at: Utc::now(),
            cameras: telemetry.clone(),
        };
        *shared.write().await = telemetry;
        let _ = post_json(&client, &config, "/api/v1/cameras/telemetry", &batch).await;
        tokio::time::sleep(config.probe_interval).await;
    }
}

async fn probe_candidate(
    config: &Config,
    candidate: &onvif::CameraCandidate,
    reconnects: &Arc<RwLock<HashMap<String, u32>>>,
    backoff: &Arc<RwLock<Backoff>>,
) -> CameraTelemetry {
    let camera_name = candidate
        .profile
        .name
        .clone()
        .or_else(|| candidate.model.clone())
        .unwrap_or_else(|| format!("Camera {}", &candidate.camera_id[..8]));
    let (result, held_off) =
        probe_with_backoff(config, &candidate.camera_id, &candidate.rtsp_uri, backoff).await;
    telemetry_from_probe(
        config,
        candidate.camera_id.clone(),
        camera_name,
        candidate.manufacturer.clone(),
        candidate.model.clone(),
        candidate.firmware.clone(),
        candidate.profile.name.clone(),
        candidate.profile.encoding.clone(),
        candidate.profile.width,
        candidate.profile.height,
        &candidate.rtsp_uri,
        result,
        held_off,
        reconnects,
    )
    .await
}

async fn probe_explicit(
    config: &Config,
    camera_id: String,
    url: &str,
    reconnects: &Arc<RwLock<HashMap<String, u32>>>,
    backoff: &Arc<RwLock<Backoff>>,
) -> CameraTelemetry {
    let (result, held_off) = probe_with_backoff(config, &camera_id, url, backoff).await;
    telemetry_from_probe(
        config,
        camera_id,
        config.explicit_camera_name.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        url,
        result,
        held_off,
        reconnects,
    )
    .await
}

/// Probe, unless this camera failed recently enough that it is still waiting.
///
/// A camera held off still produces telemetry — the error that put it there,
/// repeated — because dropping it from the fleet while it is broken hides
/// exactly the thing an operator is looking for. What is skipped is the dialling
/// and its eight-second timeout, not the reporting.
async fn probe_with_backoff(
    config: &Config,
    camera_id: &str,
    url: &str,
    backoff: &Arc<RwLock<Backoff>>,
) -> (anyhow::Result<rtsp::RtspMetrics>, bool) {
    let now = Instant::now();
    if let Some(reason) = backoff.read().await.skip_reason(camera_id, now) {
        // Held off, not failed. The caller has to be able to tell: a skipped
        // dial is not a reconnect and does not deserve its own warning, and
        // conflating them was the first version of this.
        return (Err(anyhow!(reason)), true);
    }

    let (username, password) = config.camera_login(url);
    let result = rtsp::probe(
        url,
        username.as_deref(),
        password.as_deref(),
        config.rtsp_probe_window,
    )
    .await;

    let mut guard = backoff.write().await;
    match &result {
        Ok(_) => {
            if guard.failures(camera_id) > 0 {
                info!(camera_id, "camera answered again");
            }
            guard.record_success(camera_id);
        }
        Err(error) => {
            guard.record_failure(camera_id, &error.to_string(), now);
            // The count is the useful part. Twenty-five identical warnings say
            // nothing the first one did not; a rising count and a rising wait
            // say the loop knows the camera is gone and has stopped pretending
            // otherwise.
            debug!(
                camera_id,
                consecutive_failures = guard.failures(camera_id),
                "backing off"
            );
        }
    }
    (result, false)
}

#[allow(clippy::too_many_arguments)]
async fn telemetry_from_probe(
    config: &Config,
    camera_id: String,
    name: String,
    manufacturer: Option<String>,
    model: Option<String>,
    firmware: Option<String>,
    profile_name: Option<String>,
    onvif_codec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    rtsp_uri: &str,
    result: anyhow::Result<rtsp::RtspMetrics>,
    // True when no dial happened because the camera is still in backoff.
    held_off: bool,
    reconnects: &Arc<RwLock<HashMap<String, u32>>>,
) -> CameraTelemetry {
    match result {
        Ok(metrics) => {
            let reconnect_count = *reconnects.read().await.get(&camera_id).unwrap_or(&0);
            CameraTelemetry {
                camera_id,
                gateway_id: config.gateway_id.clone(),
                site_id: config.site_id.clone(),
                name,
                status: if metrics.packet_loss > 0 {
                    HealthStatus::Warning
                } else {
                    HealthStatus::Healthy
                },
                manufacturer,
                model,
                firmware,
                profile_name,
                codec: metrics.codec.or(onvif_codec),
                width,
                height,
                fps: metrics.fps,
                bitrate_kbps: metrics.bitrate_kbps,
                packet_loss: metrics.packet_loss,
                reconnects: reconnect_count,
                rtsp_endpoint: rtsp::redacted_endpoint(rtsp_uri),
                last_seen: Utc::now(),
                last_error: None,
            }
        }
        Err(error) => {
            // A camera held off did not fail here — it failed earlier and is
            // waiting. Counting that as a reconnect inflates the number an
            // operator uses to judge a flaky link, and warning about it again
            // every interval is the noise backoff exists to remove.
            let count = if held_off {
                *reconnects.read().await.get(&camera_id).unwrap_or(&0)
            } else {
                let mut map = reconnects.write().await;
                let count = map.entry(camera_id.clone()).or_default();
                *count += 1;
                *count
            };
            if !held_off {
                warn!(camera_id = %camera_id, %error, "RTSP probe failed");
            }
            CameraTelemetry {
                camera_id,
                gateway_id: config.gateway_id.clone(),
                site_id: config.site_id.clone(),
                name,
                status: HealthStatus::Offline,
                manufacturer,
                model,
                firmware,
                profile_name,
                codec: onvif_codec,
                width,
                height,
                fps: None,
                bitrate_kbps: None,
                packet_loss: 0,
                reconnects: count,
                rtsp_endpoint: rtsp::redacted_endpoint(rtsp_uri),
                last_seen: Utc::now(),
                last_error: Some(error.to_string()),
            }
        }
    }
}

async fn heartbeat_loop(
    config: Config,
    client: reqwest::Client,
    shared: Arc<RwLock<Vec<CameraTelemetry>>>,
    hostname: String,
    started: Instant,
) -> anyhow::Result<()> {
    loop {
        let cameras = shared.read().await;
        let heartbeat = GatewayHeartbeat {
            gateway_id: config.gateway_id.clone(),
            site_id: config.site_id.clone(),
            hostname: hostname.clone(),
            version: VERSION.into(),
            uptime_seconds: started.elapsed().as_secs(),
            cpu_percent: 0.0,
            memory_percent: 0.0,
            cameras_seen: cameras.len() as u32,
            healthy_cameras: cameras
                .iter()
                .filter(|c| c.status == HealthStatus::Healthy)
                .count() as u32,
            warning_cameras: cameras
                .iter()
                .filter(|c| c.status == HealthStatus::Warning)
                .count() as u32,
            offline_cameras: cameras
                .iter()
                .filter(|c| c.status == HealthStatus::Offline)
                .count() as u32,
            sent_at: Utc::now(),
        };
        drop(cameras);
        // An accepted heartbeat is what an update waits for before it keeps a new binary.
        if post_json(&client, &config, "/api/v1/gateways/heartbeat", &heartbeat).await
            && let Some(state_dir) = &config.state_dir
        {
            update::record_heartbeat(state_dir);
        }
        tokio::time::sleep(config.heartbeat_interval).await;
    }
}

/// True when the API accepted it.
async fn post_json<T: serde::Serialize>(
    client: &reqwest::Client,
    config: &Config,
    path: &str,
    payload: &T,
) -> bool {
    let endpoint = format!("{}{}", config.api_url.trim_end_matches('/'), path);
    match client
        .post(endpoint)
        .bearer_auth(&config.token)
        .json(payload)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => true,
        Ok(response) => {
            warn!(status = %response.status(), path, "API request rejected");
            false
        }
        Err(error) => {
            warn!(%error, path, "API request failed; will retry");
            false
        }
    }
}

async fn command_loop(
    config: Config,
    client: reqwest::Client,
    sources: Arc<RwLock<HashMap<String, CameraSource>>>,
) -> anyhow::Result<()> {
    loop {
        let endpoint = format!(
            "{}/api/v1/gateways/{}/commands/next",
            config.api_url.trim_end_matches('/'),
            config.gateway_id
        );
        match client.get(endpoint).bearer_auth(&config.token).send().await {
            Ok(response) if response.status().is_success() => {
                let command: Option<GatewayCommand> = response.json().await?;
                if let Some(command) = command {
                    let result = execute_command(&config, &client, &sources, &command).await;
                    complete_command(&config, &client, &command, result).await;
                    continue;
                }
            }
            Ok(response) => warn!(status = %response.status(), "command poll rejected"),
            Err(error) => warn!(%error, "command poll failed"),
        }
        tokio::time::sleep(config.command_poll_interval).await;
    }
}

#[derive(Default)]
struct CommandPayload {
    recording: Option<RecordingManifest>,
    live: Option<LiveSessionAnswer>,
    analysis: Option<AiAnalysisResult>,
}

async fn execute_command(
    config: &Config,
    client: &reqwest::Client,
    sources: &Arc<RwLock<HashMap<String, CameraSource>>>,
    command: &GatewayCommand,
) -> anyhow::Result<CommandPayload> {
    match &command.kind {
        GatewayCommandKind::Record {
            camera_id,
            duration_seconds,
            segment_seconds,
            storage_plugin_id,
        } => {
            let source = camera_source(sources, camera_id).await?;
            let started_at = Utc::now();
            info!(command_id = %command.id, camera_id, duration_seconds, "recording H264 CMAF");
            let cmaf = archive::record_h264_cmaf(
                &source.rtsp_uri,
                source.username.as_deref(),
                source.password.as_deref(),
                Duration::from_secs(u64::from(*duration_seconds)),
                Duration::from_secs(u64::from(*segment_seconds)),
            )
            .await?;
            let recording_id = uuid::Uuid::new_v4().to_string();
            let namespace = format!(
                "recordings/{}/{}/{}",
                config.customer_id, config.site_id, camera_id
            );
            let context = invocation_context(config, camera_id, &command.id);

            let init_key = format!("{recording_id}/init.mp4");
            let init = upload_recording_object(
                config,
                client,
                storage_plugin_id,
                &context,
                &namespace,
                &init_key,
                "video/mp4",
                cmaf.init,
            )
            .await?;

            let mut segments = Vec::with_capacity(cmaf.segments.len());
            for segment in cmaf.segments {
                let key = format!("{recording_id}/seg-{:05}.m4s", segment.sequence);
                let size = segment.bytes.len() as u64;
                let object = upload_recording_object(
                    config,
                    client,
                    storage_plugin_id,
                    &context,
                    &namespace,
                    &key,
                    "video/iso.segment",
                    segment.bytes,
                )
                .await?;
                debug_assert_eq!(object.size_bytes, size);
                let segment_started_at =
                    started_at + chrono::Duration::milliseconds(segment.start_offset_ms as i64);
                let segment_ended_at =
                    segment_started_at + chrono::Duration::milliseconds(segment.duration_ms as i64);
                segments.push(RecordingSegment {
                    id: uuid::Uuid::new_v4().to_string(),
                    sequence: segment.sequence,
                    started_at: segment_started_at,
                    ended_at: segment_ended_at,
                    duration_ms: segment.duration_ms,
                    keyframe: true,
                    object,
                });
            }
            let ended_at = segments
                .last()
                .map(|segment| segment.ended_at)
                .unwrap_or_else(|| started_at);
            Ok(CommandPayload {
                recording: Some(RecordingManifest {
                    recording_id,
                    camera_id: camera_id.clone(),
                    gateway_id: config.gateway_id.clone(),
                    started_at,
                    ended_at,
                    codec: cmaf.codec,
                    width: cmaf.width,
                    height: cmaf.height,
                    init,
                    segments,
                    delete_after: None,
                }),
                ..Default::default()
            })
        }
        GatewayCommandKind::Live {
            camera_id,
            offer_sdp,
            offer_type,
            session_seconds,
            ice_servers,
        } => {
            let source = camera_source(sources, camera_id).await?;
            info!(command_id = %command.id, camera_id, "starting outbound-signaled WebRTC live session");
            let answer = live::start_h264(
                source.live_rtsp_uri,
                source.username,
                source.password,
                offer_sdp.clone(),
                offer_type.clone(),
                ice_servers.clone(),
                *session_seconds,
            )
            .await?;
            Ok(CommandPayload {
                live: Some(answer),
                ..Default::default()
            })
        }
        GatewayCommandKind::Analyze {
            camera_id,
            ai_plugin_id,
            storage_plugin_id,
            tasks,
        } => {
            let source = camera_source(sources, camera_id).await?;
            let snapshot_uri = source.snapshot_uri.as_deref().ok_or_else(|| {
                anyhow::anyhow!("camera {camera_id} did not advertise ONVIF GetSnapshotUri")
            })?;
            info!(command_id = %command.id, camera_id, ai_plugin_id, "capturing snapshot for AI plugin");
            let captured_at = Utc::now();
            let snapshot = snapshot::fetch(
                client,
                snapshot_uri,
                source.username.as_deref(),
                source.password.as_deref(),
            )
            .await?;
            let context = invocation_context(config, camera_id, &command.id);
            let namespace = format!(
                "analysis/{}/{}/{}",
                config.customer_id, config.site_id, camera_id
            );
            let extension = if snapshot.content_type.eq_ignore_ascii_case("image/png") {
                "png"
            } else {
                "jpg"
            };
            let object_key = format!("{}.{}", command.id, extension);
            let snapshot_object = upload_recording_object(
                config,
                client,
                storage_plugin_id,
                &context,
                &namespace,
                &object_key,
                &snapshot.content_type,
                snapshot.bytes.clone(),
            )
            .await?;

            let request = PluginAiAnalyzeRequest {
                context,
                camera_id: camera_id.clone(),
                captured_at,
                input: MediaInput::InlineBase64 {
                    content_type: snapshot.content_type,
                    data_base64: BASE64_STANDARD.encode(&snapshot.bytes),
                },
                tasks: tasks.clone(),
                parameters: serde_json::json!({}),
            };
            let endpoint = format!(
                "{}/api/v1/plugins/{}/ai/analyze",
                config.api_url.trim_end_matches('/'),
                ai_plugin_id
            );
            let response = client
                .post(endpoint)
                .bearer_auth(&config.token)
                .json(&request)
                .send()
                .await?
                .error_for_status()?;
            let analyzed: PluginAiAnalyzeResponse = response.json().await?;
            let detections = analyzed
                .detections
                .into_iter()
                .map(|detection| AiDetectionResult {
                    label: detection.label,
                    confidence: detection.confidence,
                    bbox: detection.bbox.map(|bbox| AiBoundingBox {
                        x: bbox.x,
                        y: bbox.y,
                        width: bbox.width,
                        height: bbox.height,
                    }),
                    attributes: detection.attributes,
                })
                .collect();
            Ok(CommandPayload {
                analysis: Some(AiAnalysisResult {
                    camera_id: camera_id.clone(),
                    captured_at,
                    ai_plugin_id: analyzed.plugin_id,
                    model: analyzed.model,
                    detections,
                    metadata: analyzed.metadata,
                    snapshot: snapshot_object,
                }),
                ..Default::default()
            })
        }
    }
}

async fn camera_source(
    sources: &Arc<RwLock<HashMap<String, CameraSource>>>,
    camera_id: &str,
) -> anyhow::Result<CameraSource> {
    sources
        .read()
        .await
        .get(camera_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("camera {camera_id} is not currently available on gateway"))
}

fn invocation_context(config: &Config, camera_id: &str, trace_id: &str) -> PluginInvocationContext {
    PluginInvocationContext {
        organization_id: Some(config.customer_id.clone()),
        site_id: Some(config.site_id.clone()),
        camera_id: Some(camera_id.to_owned()),
        trace_id: Some(trace_id.to_owned()),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_recording_object(
    config: &Config,
    client: &reqwest::Client,
    storage_plugin_id: &str,
    context: &PluginInvocationContext,
    namespace: &str,
    object_key: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<RecordingObject> {
    let request = StorageUploadRequest {
        context: context.clone(),
        namespace: namespace.into(),
        object_key: object_key.into(),
        content_type: content_type.into(),
        content_length: Some(bytes.len() as u64),
        expires_seconds: 900,
        audience: TransferAudience::Edge,
        metadata: BTreeMap::from([("source".into(), "edge-gateway".into())]),
    };
    let endpoint = format!(
        "{}/api/v1/plugins/{}/storage/uploads",
        config.api_url.trim_end_matches('/'),
        storage_plugin_id
    );
    let response = client
        .post(endpoint)
        .bearer_auth(&config.token)
        .json(&request)
        .send()
        .await?
        .error_for_status()?;
    let transfer: SignedTransfer = response.json().await?;
    if !transfer.method.eq_ignore_ascii_case("PUT") {
        anyhow::bail!(
            "storage plugin returned unsupported upload method {}",
            transfer.method
        );
    }
    let size_bytes = bytes.len() as u64;
    let mut upload = client.put(&transfer.url).body(bytes);
    for (name, value) in &transfer.headers {
        upload = upload.header(name, value);
    }
    upload.send().await?.error_for_status()?;
    Ok(RecordingObject {
        storage_plugin_id: storage_plugin_id.into(),
        object_ref: transfer.object_ref,
        object_key: object_key.into(),
        content_type: content_type.into(),
        size_bytes,
    })
}

async fn complete_command(
    config: &Config,
    client: &reqwest::Client,
    command: &GatewayCommand,
    execution: anyhow::Result<CommandPayload>,
) {
    let (status, error, payload) = match execution {
        Ok(payload) => (GatewayCommandStatus::Succeeded, None, payload),
        Err(error) => {
            warn!(command_id = %command.id, %error, "gateway command failed");
            (
                GatewayCommandStatus::Failed,
                Some(error.to_string()),
                CommandPayload::default(),
            )
        }
    };
    let result = GatewayCommandResult {
        command_id: command.id.clone(),
        gateway_id: config.gateway_id.clone(),
        status,
        completed_at: Utc::now(),
        error,
        recording: payload.recording,
        live: payload.live,
        analysis: payload.analysis,
    };
    let endpoint = format!(
        "{}/api/v1/gateways/{}/commands/{}/complete",
        config.api_url.trim_end_matches('/'),
        config.gateway_id,
        command.id
    );
    match client
        .post(endpoint)
        .bearer_auth(&config.token)
        .json(&result)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => {
            warn!(status = %response.status(), command_id = %command.id, "command completion rejected")
        }
        Err(error) => warn!(%error, command_id = %command.id, "command completion failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fake_camera::FakeCamera, fake_control_plane::FakeControlPlane};

    const TOKEN: &str = "gateway-token";

    fn sample_identity() -> identity::GatewayIdentity {
        identity::GatewayIdentity {
            version: identity::CURRENT_VERSION,
            gateway_token: "tok-1".into(),
            gateway_id: "gw-1".into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
            camera_limit: 3,
        }
    }

    #[test]
    fn the_boot_decision_covers_every_arm() {
        use identity::{BootPlan, boot_plan};
        // Reenroll without a token refuses rather than wiping.
        assert!(boot_plan(Ok(None), false, true).is_err());
        // Reenroll with a token enrolls, even over state that cannot be read.
        assert!(matches!(
            boot_plan(Err(anyhow::anyhow!("corrupt")), true, true),
            Ok(BootPlan::Enroll)
        ));
        // Unreadable state without the reenroll escape hatch refuses to start,
        // and the message names both fixes.
        let err = boot_plan(Err(anyhow::anyhow!("bad key")), true, false).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("GATEWAY_REENROLL"),
            "no recovery hint in: {text}"
        );
        assert!(text.contains("GATEWAY_STATE_KEY"), "no key hint in: {text}");
        // A persisted identity wins and the burned env token is ignored.
        assert!(matches!(
            boot_plan(Ok(Some(sample_identity())), true, false),
            Ok(BootPlan::UsePersisted(_))
        ));
        // Fresh state: a token enrolls; no token is today's bootstrap.
        assert!(matches!(
            boot_plan(Ok(None), true, false),
            Ok(BootPlan::Enroll)
        ));
        assert!(matches!(
            boot_plan(Ok(None), false, false),
            Ok(BootPlan::Bootstrap)
        ));
    }

    /// A camera held off by backoff is not reconnecting, and the number an
    /// operator uses to judge a flaky link must not count the waiting.
    ///
    /// The first version of backoff got this wrong. It returned the held-off
    /// case as an ordinary Err, which walked straight into the failure branch,
    /// incremented the counter and logged a warning every interval — so the
    /// change meant to quieten the log left it exactly as loud and started
    /// inventing reconnects on top. Unit tests on the Backoff type could not
    /// see it; only running the thing did.
    #[tokio::test]
    async fn a_held_off_probe_is_not_counted_as_a_reconnect() {
        let cfg = config("http://127.0.0.1:9");
        let reconnects = Arc::new(RwLock::new(HashMap::<String, u32>::new()));

        let real_failure = telemetry_from_probe(
            &cfg,
            "cam".into(),
            "Cam".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "rtsp://example.test/s",
            Err(anyhow!("RTSP DESCRIBE timeout")),
            false,
            &reconnects,
        )
        .await;
        assert_eq!(real_failure.reconnects, 1);

        for _ in 0..5 {
            let waiting = telemetry_from_probe(
                &cfg,
                "cam".into(),
                "Cam".into(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                "rtsp://example.test/s",
                Err(anyhow!("RTSP DESCRIBE timeout")),
                true,
                &reconnects,
            )
            .await;
            assert_eq!(
                waiting.reconnects, 1,
                "waiting for the next attempt counted as reconnecting"
            );
            assert_eq!(waiting.status, HealthStatus::Offline);
            assert_eq!(
                waiting.last_error.as_deref(),
                Some("RTSP DESCRIBE timeout"),
                "a camera in backoff has to keep saying why it is offline"
            );
        }
    }

    /// The updater's proof that a new binary works: a heartbeat the API accepted, written
    /// down where the updater can see it. A heartbeat that failed proves nothing.
    #[tokio::test]
    async fn an_accepted_heartbeat_is_recorded_and_a_failed_one_is_not() {
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        let state = tempfile::tempdir().unwrap();
        let mut accepted = config(&plane.url);
        accepted.state_dir = Some(state.path().to_path_buf());
        accepted.heartbeat_interval = Duration::from_millis(50);
        let marker = state.path().join(update::HEARTBEAT_MARKER);
        let task = tokio::spawn(heartbeat_loop(
            accepted,
            reqwest::Client::new(),
            Arc::new(RwLock::new(Vec::new())),
            "test-host".into(),
            Instant::now(),
        ));
        let recorded = tokio::time::timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        task.abort();
        assert!(recorded.is_ok(), "an accepted heartbeat left no marker");

        let silent = tempfile::tempdir().unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut failing = config(&format!("http://{closed}"));
        failing.state_dir = Some(silent.path().to_path_buf());
        failing.heartbeat_interval = Duration::from_millis(50);
        let task = tokio::spawn(heartbeat_loop(
            failing,
            reqwest::Client::new(),
            Arc::new(RwLock::new(Vec::new())),
            "test-host".into(),
            Instant::now(),
        ));
        tokio::time::sleep(Duration::from_millis(400)).await;
        task.abort();
        assert!(
            !silent.path().join(update::HEARTBEAT_MARKER).exists(),
            "a heartbeat nobody accepted must not count as proof"
        );
    }

    #[test]
    fn a_camera_with_its_own_credentials_does_not_get_the_shared_pair() {
        let state = tempfile::tempdir().unwrap();
        let key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let mut store =
            crate::camera_credentials::CameraCredentials::load(state.path(), Some(key)).unwrap();
        store.set("192.168.1.50", "own", "own-secret").unwrap();
        let mut config = config("http://127.0.0.1:1");
        config.camera_username = Some("shared".into());
        config.camera_password = Some("shared-secret".into());
        config.camera_credentials = Arc::new(store);

        assert_eq!(
            config.camera_login("rtsp://192.168.1.50:554/stream"),
            (Some("own".into()), Some("own-secret".into())),
            "the camera's own credentials, found from the stream it serves"
        );
        assert_eq!(
            config.camera_login("http://192.168.1.99/onvif/device_service"),
            (Some("shared".into()), Some("shared-secret".into())),
            "a camera nobody configured still gets the pair from the environment"
        );
        assert!(
            config.onvif_login("rtsp://192.168.1.50/stream").is_some(),
            "ONVIF takes the same credentials"
        );
    }

    fn config(api_url: &str) -> Config {
        Config {
            api_url: api_url.to_owned(),
            gateway_id: "gw-1".into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
            token: TOKEN.into(),
            enrollment_token: None,
            camera_limit: 10,
            heartbeat_interval: Duration::from_secs(30),
            discovery_wait: Duration::from_millis(1),
            probe_interval: Duration::from_secs(30),
            rtsp_probe_window: Duration::from_millis(300),
            command_poll_interval: Duration::from_millis(20),
            camera_username: None,
            camera_password: None,
            camera_credentials: Arc::new(crate::camera_credentials::CameraCredentials::empty()),
            state_dir: None,
            explicit_rtsp_url: None,
            explicit_camera_name: "Camera".into(),
            onvif_hosts: Vec::new(),
        }
    }

    fn record_command(id: &str, camera_id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "gateway_id": "gw-1",
            "created_at": Utc::now(),
            "expires_at": Utc::now() + chrono::Duration::minutes(2),
            "kind": {
                "type": "record",
                "camera_id": camera_id,
                "duration_seconds": 2,
                "segment_seconds": 1,
                "storage_plugin_id": "storage-s3",
            },
        })
    }

    async fn sources_with(
        camera_id: &str,
        url: &str,
    ) -> Arc<RwLock<HashMap<String, CameraSource>>> {
        let mut map = HashMap::new();
        map.insert(
            camera_id.to_owned(),
            CameraSource {
                rtsp_uri: url.to_owned(),
                live_rtsp_uri: url.to_owned(),
                snapshot_uri: None,
                username: None,
                password: None,
            },
        );
        Arc::new(RwLock::new(map))
    }

    #[tokio::test]
    async fn a_command_that_fails_is_still_reported_rather_than_dropped() {
        // The single most important property of the loop. Whoever asked for this
        // is polling the command view; a failure that never completes leaves
        // them waiting forever with no way to tell a slow gateway from a broken
        // one. The command here names a camera the gateway does not have.
        let api = FakeControlPlane::start(vec![record_command("cmd-1", "no-such-camera")], 0).await;
        let sources = Arc::new(RwLock::new(HashMap::new()));
        let client = reqwest::Client::new();
        tokio::spawn(command_loop(config(&api.url), client, sources));

        let completions = api.wait_for_completions(1, Duration::from_secs(10)).await;
        assert_eq!(completions[0]["command_id"], "cmd-1");
        assert_eq!(completions[0]["status"], "failed");
        assert!(
            completions[0]["error"]
                .as_str()
                .is_some_and(|e| !e.is_empty()),
            "a failure must carry a reason, got {:?}",
            completions[0]["error"]
        );
    }

    #[tokio::test]
    async fn a_recording_command_runs_and_reports_its_manifest() {
        let camera = FakeCamera::start(false).await.unwrap();
        let api = FakeControlPlane::start(vec![record_command("cmd-2", "cam-1")], 0).await;
        let sources = sources_with("cam-1", &camera.url).await;
        tokio::spawn(command_loop(
            config(&api.url),
            reqwest::Client::new(),
            sources,
        ));

        let completions = api.wait_for_completions(1, Duration::from_secs(20)).await;
        let result = &completions[0];
        assert_eq!(result["command_id"], "cmd-2");
        // The upload to a storage plugin will fail — none is configured — but
        // the loop must still report a definite outcome either way, and must
        // never leave the command unanswered.
        eprintln!(
            "DEBUG completion: {}",
            serde_json::to_string_pretty(result).unwrap()
        );
    }

    #[tokio::test]
    async fn the_gateway_token_goes_on_both_the_poll_and_the_completion() {
        // Losing it on either one leaves commands stuck: the poll returns 401
        // forever, or the work is done and the answer is refused.
        let api = FakeControlPlane::start(vec![record_command("cmd-3", "no-such-camera")], 0).await;
        tokio::spawn(command_loop(
            config(&api.url),
            reqwest::Client::new(),
            Arc::new(RwLock::new(HashMap::new())),
        ));
        api.wait_for_completions(1, Duration::from_secs(10)).await;

        let seen = api.seen.read().await;
        assert!(
            seen.tokens.len() >= 2,
            "expected a token on poll and completion"
        );
        assert!(
            seen.tokens.iter().all(|t| t == TOKEN),
            "a request went out with the wrong token: {:?}",
            seen.tokens
        );
    }

    #[tokio::test]
    async fn the_gateway_token_goes_on_plugin_upload_requests() {
        // The API's plugin endpoints demand a gateway bearer; an upload that
        // goes out without one is refused and every recording then fails at
        // its first object.
        let camera = FakeCamera::start(false).await.unwrap();
        let api = FakeControlPlane::start(vec![record_command("cmd-5", "cam-1")], 0).await;
        let sources = sources_with("cam-1", &camera.url).await;
        tokio::spawn(command_loop(
            config(&api.url),
            reqwest::Client::new(),
            sources,
        ));

        api.wait_for_completions(1, Duration::from_secs(20)).await;
        let seen = api.seen.read().await;
        assert!(seen.uploads >= 1, "the recording never asked for an upload");
        assert_eq!(
            seen.upload_tokens.len() as u32,
            seen.uploads,
            "an upload request went out without a bearer token"
        );
        assert!(
            seen.upload_tokens.iter().all(|t| t == TOKEN),
            "an upload went out with the wrong token: {:?}",
            seen.upload_tokens
        );
    }

    #[tokio::test]
    async fn a_rejected_poll_does_not_end_the_loop() {
        // A gateway that gives up on one 401 stays dead until someone restarts
        // it, which on customer premises means a site visit.
        let api = FakeControlPlane::start(vec![record_command("cmd-4", "no-such-camera")], 3).await;
        tokio::spawn(command_loop(
            config(&api.url),
            reqwest::Client::new(),
            Arc::new(RwLock::new(HashMap::new())),
        ));

        let completions = api.wait_for_completions(1, Duration::from_secs(10)).await;
        assert_eq!(completions[0]["command_id"], "cmd-4");
        assert!(
            api.seen.read().await.polls > 3,
            "the loop stopped polling after the rejections"
        );
    }

    #[tokio::test]
    async fn an_empty_queue_is_polled_again_rather_than_treated_as_an_error() {
        let api = FakeControlPlane::start(Vec::new(), 0).await;
        tokio::spawn(command_loop(
            config(&api.url),
            reqwest::Client::new(),
            Arc::new(RwLock::new(HashMap::new())),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let seen = api.seen.read().await;
        assert!(
            seen.polls > 2,
            "only {} polls; the loop stalled",
            seen.polls
        );
        assert!(
            seen.completions.is_empty(),
            "nothing was queued to complete"
        );
    }

    #[tokio::test]
    async fn enrollment_is_persisted_and_a_restart_reuses_it_without_the_api() {
        let api = FakeControlPlane::start(Vec::new(), 0).await;
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();

        let store = identity::IdentityStore::open(dir.path(), None).unwrap();
        let mut cfg = config(&api.url);
        cfg.enrollment_token = Some("ENROLL-1".into());
        establish_identity(&mut cfg, &client, "edge-1", &store, false)
            .await
            .unwrap();
        assert_eq!(cfg.token, "enrolled-1");
        assert_eq!(cfg.customer_id, "cust-1");

        // The restart: fresh env-derived config, same state dir, and the same
        // burned token still sitting in the environment.
        let store2 = identity::IdentityStore::open(dir.path(), None).unwrap();
        let mut cfg2 = config(&api.url);
        cfg2.enrollment_token = Some("ENROLL-1".into());
        establish_identity(&mut cfg2, &client, "edge-1", &store2, false)
            .await
            .unwrap();
        assert_eq!(
            cfg2.token, "enrolled-1",
            "the restart must reuse the persisted token"
        );
        assert_eq!(
            cfg2.site_name, "Site",
            "the persisted identity carries the site"
        );
        assert_eq!(
            api.seen.read().await.enrolls,
            1,
            "the burned enrollment token must not be spent again"
        );
    }

    #[tokio::test]
    async fn reenroll_wipes_state_and_spends_a_fresh_token() {
        let api = FakeControlPlane::start(Vec::new(), 0).await;
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();

        let store = identity::IdentityStore::open(dir.path(), None).unwrap();
        let mut cfg = config(&api.url);
        cfg.enrollment_token = Some("ENROLL-1".into());
        establish_identity(&mut cfg, &client, "edge-1", &store, false)
            .await
            .unwrap();

        let mut cfg2 = config(&api.url);
        cfg2.enrollment_token = Some("ENROLL-2".into());
        establish_identity(&mut cfg2, &client, "edge-1", &store, true)
            .await
            .unwrap();
        assert_eq!(cfg2.token, "enrolled-2", "reenroll must earn a fresh token");
        assert_eq!(
            store.load().unwrap().unwrap().gateway_token,
            "enrolled-2",
            "the fresh identity must be the one persisted"
        );

        // And the flag alone, with no token to re-enroll from, refuses.
        let mut cfg3 = config(&api.url);
        assert!(
            establish_identity(&mut cfg3, &client, "edge-1", &store, true)
                .await
                .is_err()
        );
    }
}
