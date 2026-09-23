mod analysis;
mod archive;
mod backoff;
mod camera_credentials;
#[cfg(test)]
mod fake_browser;
#[cfg(test)]
mod fake_camera;
#[cfg(test)]
mod fake_control_plane;
#[cfg(test)]
mod fake_publisher;
mod frames;
// Only the pushed-SRT path needs these, but its tests are worth running in
// every build.
#[cfg_attr(not(feature = "srt"), allow(dead_code))]
mod h264;
mod icepath;
mod identity;
mod incident;
mod ingest;
mod live;
mod onvif;
mod recorder;
mod release;
mod ringbuffer;
mod rtmp;
mod rtsp;
mod schedule;
mod segmenter;
mod snapshot;
#[cfg(feature = "srt")]
mod srt;
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
use chrono::{DateTime, Utc};
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
    /// Streams pushed to this gateway. Empty and idle unless RTMP_LISTEN is set,
    /// and shared here because every loop that needs it already has a Config.
    ingest: crate::ingest::Ingest,
    /// Where the ring buffer lives, when one is running. `None` follows the
    /// state directory.
    recording_dir: Option<std::path::PathBuf>,
    /// How much disk the ring may use, per camera.
    recording_budget_bytes: u64,
    /// How the windows a schedule keeps are cut into clips.
    cutting: crate::schedule::Cutting,
}

impl Config {
    /// What to present to the camera at this address: its own credentials when
    /// it has been given some, otherwise the pair every camera shared before
    /// this existed. Both halves are optional, as before: a camera on an open
    /// network needs neither.
    /// Where this gateway keeps recorded video it has not been asked for yet.
    fn recording_dir(&self) -> std::path::PathBuf {
        self.recording_dir.clone().unwrap_or_else(|| {
            self.state_dir
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("ring")
        })
    }

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
            ingest: crate::ingest::Ingest::new(),
            recording_dir: env::var("RECORDING_DIR").ok().map(std::path::PathBuf::from),
            cutting: crate::schedule::Cutting::default(),
            recording_budget_bytes: env::var("RECORDING_BUDGET_BYTES")
                .ok()
                .and_then(|raw| raw.parse().ok())
                // Two gigabytes is a few hours of one camera at a sensible
                // bitrate, and small enough not to fill a site box by itself.
                .unwrap_or(2 * 1024 * 1024 * 1024),
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
    /// Set when this source is pushed to the gateway: the stream key it
    /// publishes on. Live view and recording then read the ingest instead of
    /// dialling anything.
    push_key: Option<String>,
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

    // Push ingest, when a site has an encoder that pushes rather than a camera
    // to pull. Off unless RTMP_LISTEN says where to listen.
    if let Some(address) = rtmp::listen_address() {
        match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => {
                info!(%address, "listening for pushed RTMP streams");
                let ingest = config.ingest.clone();
                tokio::spawn(rtmp::serve(listener, ingest));
            }
            Err(error) => {
                warn!(%address, %error, "could not listen for RTMP; push ingest is off")
            }
        }
    }

    #[cfg(feature = "srt")]
    if let Some(address) = srt::listen_address() {
        let ingest = config.ingest.clone();
        tokio::spawn(async move {
            if let Err(error) = srt::serve(address, ingest).await {
                warn!(%address, %error, "SRT ingest stopped");
            }
        });
        info!(%address, "listening for pushed SRT streams");
    }

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
    // What the dashboard has given this gateway. Kept between passes, so an API
    // that cannot be reached does not take every source off the roster.
    let mut video_sources: Vec<vms_domain::VideoSource> = Vec::new();
    // What each pushed stream had counted at the end of the last pass, so the
    // rates reported are for this interval rather than the whole session.
    let mut pushed_marks: HashMap<String, (u64, u64, Instant)> = HashMap::new();
    // How each camera is to be recorded, and what is recording right now.
    let mut policies: Vec<vms_domain::RecordingPolicy> = Vec::new();
    let mut recorders = recorder::Recorders::new();
    let mut incidents = incident::Incidents::new();
    let mut pacing = analysis::Pacing::new();
    loop {
        let mut telemetry = Vec::new();
        let mut fresh_sources = HashMap::new();
        if let Some(fresh) = fetch_sources(&client, &config).await {
            video_sources = fresh;
        }
        if let Some(fresh) = fetch_recording_policies(&client, &config).await {
            policies = fresh;
        }
        // Only keys the dashboard listed may publish. Doing this every pass is
        // how a removed source stops the next publisher using its key.
        config.ingest.allow(
            video_sources
                .iter()
                .filter(|source| is_pushed(source.kind))
                .map(|source| source.address.clone()),
        );
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
                                    push_key: None,
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
                        push_key: None,
                        // An explicitly configured URL names one stream; there is
                        // no profile list to pick a substream from.
                        live_rtsp_uri: url.clone(),
                        snapshot_uri: None,
                        username: login.0,
                        password: login.1,
                    },
                );
                telemetry.push(
                    probe_explicit(
                        &config,
                        id,
                        config.explicit_camera_name.clone(),
                        url,
                        &reconnects,
                        &backoff,
                    )
                    .await,
                );
            }
        }

        // Sources added from the dashboard, pulled like any other camera. A source
        // at an address discovery already found is not a second camera.
        let discovered: std::collections::HashSet<String> = telemetry
            .iter()
            .map(|camera| camera.camera_id.clone())
            .collect();
        for (source, id) in sources_to_probe(&video_sources, &discovered) {
            let login = config.camera_login(&source.address);
            fresh_sources.insert(
                id.clone(),
                CameraSource {
                    rtsp_uri: source.address.clone(),
                    push_key: None,
                    live_rtsp_uri: source.address.clone(),
                    snapshot_uri: None,
                    username: login.0,
                    password: login.1,
                },
            );
            telemetry.push(
                probe_explicit(
                    &config,
                    id,
                    source.name.clone(),
                    &source.address,
                    &reconnects,
                    &backoff,
                )
                .await,
            );
        }

        // Streams pushed to this gateway. Nothing is dialled: either a publisher
        // is on the key or it is not, and the ingest is the only witness.
        for source in video_sources.iter().filter(|source| is_pushed(source.kind)) {
            let camera_id = source_camera_id(&source.address);
            if discovered.contains(&camera_id) {
                continue;
            }
            let endpoint = match source.kind {
                vms_domain::SourceKind::Srt => {
                    format!("srt://{}?streamid={}", config.gateway_id, source.address)
                }
                _ => format!("rtmp://{}/live/{}", config.gateway_id, source.address),
            };
            fresh_sources.insert(
                camera_id.clone(),
                CameraSource {
                    rtsp_uri: endpoint.clone(),
                    push_key: Some(source.address.clone()),
                    live_rtsp_uri: endpoint.clone(),
                    snapshot_uri: None,
                    username: None,
                    password: None,
                },
            );
            let result = pushed_metrics(
                &config,
                &source.address,
                &mut pushed_marks,
                config.probe_interval,
            );
            let (width, height) = match config
                .ingest
                .stats(&source.address, config.probe_interval * 2)
            {
                Some(stats) => (
                    stats.dimensions.map(|(width, _)| width),
                    stats.dimensions.map(|(_, height)| height),
                ),
                None => (None, None),
            };
            telemetry.push(
                telemetry_from_probe(
                    &config,
                    camera_id,
                    source.name.clone(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    width,
                    height,
                    &endpoint,
                    result,
                    // Nothing was dialled, so a silent key is not a reconnect
                    // and does not deserve a warning every interval.
                    true,
                    &reconnects,
                )
                .await,
            );
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
        // Keep what is being recorded in step with what the policies ask for,
        // before anything is reported: a camera that just came back should be
        // recording again by the time the cloud hears it is healthy.
        recorders.reconcile(&cameras_to_record(&policies, &fresh_sources), &config);
        // A camera going quiet is something this gateway sees before the
        // cloud does, and the video worth having is the video from before it.
        let now = Utc::now();
        for camera in &telemetry {
            if let Some(policy) = policies
                .iter()
                .find(|policy| policy.camera_id == camera.camera_id)
                .filter(|policy| policy.mode == vms_domain::RecordingMode::Continuous)
            {
                incidents.observe(&camera.camera_id, &camera.status, &policy.keep, now);
            }
        }
        watch_with_plugins(
            &config,
            &client,
            &policies,
            &fresh_sources,
            &mut pacing,
            &mut incidents,
            now,
        )
        .await;
        let present: std::collections::HashSet<String> = telemetry
            .iter()
            .map(|camera| camera.camera_id.clone())
            .collect();
        incidents.forget_missing(&present);
        pacing.forget_missing(&present);
        for (camera_id, keep) in incidents.due(now) {
            if let Some(policy) = policies.iter().find(|policy| policy.camera_id == camera_id) {
                keep_window(&config, &client, policy, keep.from, keep.to, "incident").await;
            }
        }
        keep_scheduled(&config, &client, &policies).await;
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

/// The sources this gateway has been given, or `None` when the API could not be
/// reached or answered with something else. The caller keeps what it had: a list
/// is state, and an outage is not an instruction to drop every source.
async fn fetch_sources(
    client: &reqwest::Client,
    config: &Config,
) -> Option<Vec<vms_domain::VideoSource>> {
    let endpoint = format!(
        "{}/api/v1/gateways/{}/sources",
        config.api_url.trim_end_matches('/'),
        config.gateway_id
    );
    match client.get(endpoint).bearer_auth(&config.token).send().await {
        Ok(response) if response.status().is_success() => match response.json().await {
            Ok(sources) => Some(sources),
            Err(error) => {
                warn!(%error, "the source list did not parse");
                None
            }
        },
        Ok(response) => {
            warn!(status = %response.status(), "the source list was refused");
            None
        }
        Err(error) => {
            debug!(%error, "the source list could not be fetched; keeping the last one");
            None
        }
    }
}

/// How each camera is to be recorded. `None` on any failure, so the last list
/// the gateway was given keeps standing: an API that cannot be reached must
/// not quietly stop a site recording.
async fn fetch_recording_policies(
    client: &reqwest::Client,
    config: &Config,
) -> Option<Vec<vms_domain::RecordingPolicy>> {
    let endpoint = format!(
        "{}/api/v1/gateways/{}/recording-policies",
        config.api_url.trim_end_matches('/'),
        config.gateway_id
    );
    match client.get(endpoint).bearer_auth(&config.token).send().await {
        Ok(response) if response.status().is_success() => match response.json().await {
            Ok(policies) => Some(policies),
            Err(error) => {
                warn!(%error, "the recording policies did not parse");
                None
            }
        },
        Ok(response) => {
            warn!(status = %response.status(), "the recording policies were refused");
            None
        }
        Err(error) => {
            debug!(%error, "the recording policies could not be fetched; keeping the last ones");
            None
        }
    }
}

/// File a recording nobody asked for. The media is already in storage; this
/// is what makes it findable.
async fn report_recording(
    config: &Config,
    client: &reqwest::Client,
    manifest: &RecordingManifest,
) -> anyhow::Result<()> {
    let endpoint = format!(
        "{}/api/v1/gateways/{}/recordings",
        config.api_url.trim_end_matches('/'),
        config.gateway_id
    );
    let response = client
        .post(endpoint)
        .bearer_auth(&config.token)
        .json(manifest)
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!(
            "the control plane refused the recording: {}",
            response.status()
        );
    }
    Ok(())
}

/// Upload the stretches of ring that a schedule says to keep.
///
/// Each camera's progress is a watermark beside its ring, so a restart does
/// not upload the same morning twice. A window that cannot be cut — the ring
/// dropped it, or the upload failed — is logged and skipped rather than
/// retried forever: the video is going away either way, and a gateway stuck
/// on last Tuesday keeps nothing at all.
async fn keep_scheduled(
    config: &Config,
    client: &reqwest::Client,
    policies: &[vms_domain::RecordingPolicy],
) {
    for policy in policies {
        if policy.mode != vms_domain::RecordingMode::Continuous || policy.keep.is_empty() {
            continue;
        }
        let dir = config.recording_dir().join(&policy.camera_id);
        let now = Utc::now();
        let watermark = schedule::read_watermark(&dir).unwrap_or(now - schedule::MAX_CATCH_UP);
        let windows = schedule::windows_to_keep(
            &policy.keep,
            watermark,
            now,
            &chrono::Local::now().timezone(),
            config.cutting,
        );

        for (from, to) in windows {
            keep_window(config, client, policy, from, to, "schedule").await;
            // Either way the window is behind us: the ring will drop it, and a
            // gateway retrying it forever keeps nothing new.
            if let Err(error) = schedule::write_watermark(&dir, to) {
                warn!(camera_id = %policy.camera_id, %error, "the schedule watermark did not save");
            }
        }
    }
}

/// Ask a plugin what it sees, without keeping the picture.
///
/// The snapshot is not uploaded: a camera looked at every thirty seconds is
/// two and a half thousand images a day, and the point of looking is the clip,
/// not the picture.
async fn look_at(
    config: &Config,
    client: &reqwest::Client,
    source: &CameraSource,
    camera_id: &str,
    ai_plugin_id: &str,
) -> anyhow::Result<Vec<AiDetectionResult>> {
    let snapshot_uri = source.snapshot_uri.as_deref().ok_or_else(|| {
        anyhow!("camera {camera_id} offers no snapshot, so nothing can look at it")
    })?;
    let snapshot = snapshot::fetch(
        client,
        snapshot_uri,
        source.username.as_deref(),
        source.password.as_deref(),
    )
    .await?;
    let request = PluginAiAnalyzeRequest {
        context: invocation_context(config, camera_id, "recording-policy"),
        camera_id: camera_id.to_owned(),
        captured_at: Utc::now(),
        input: MediaInput::InlineBase64 {
            content_type: snapshot.content_type,
            data_base64: BASE64_STANDARD.encode(&snapshot.bytes),
        },
        tasks: vec!["detect".to_string()],
        parameters: serde_json::json!({}),
    };
    let endpoint = format!(
        "{}/api/v1/plugins/{}/ai/analyze",
        config.api_url.trim_end_matches('/'),
        ai_plugin_id
    );
    let analyzed: PluginAiAnalyzeResponse = client
        .post(endpoint)
        .bearer_auth(&config.token)
        .json(&request)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(analyzed
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
        .collect())
}

/// Look at the cameras whose policy says to, and remember a window to keep
/// wherever a plugin was sure enough about something.
async fn watch_with_plugins(
    config: &Config,
    client: &reqwest::Client,
    policies: &[vms_domain::RecordingPolicy],
    sources: &HashMap<String, CameraSource>,
    pacing: &mut analysis::Pacing,
    incidents: &mut incident::Incidents,
    now: DateTime<Utc>,
) {
    for policy in policies {
        if policy.mode != vms_domain::RecordingMode::Continuous {
            continue;
        }
        let Some(source) = sources.get(&policy.camera_id) else {
            continue;
        };
        for rule in &policy.keep {
            let vms_domain::KeepRule::OnAnalysis {
                plugin_id,
                every_seconds,
                threshold,
                pre_roll_seconds,
                post_roll_seconds,
            } = rule
            else {
                continue;
            };
            if !pacing.due(&policy.camera_id, *every_seconds, now) {
                continue;
            }
            match look_at(config, client, source, &policy.camera_id, plugin_id).await {
                Ok(detections) => {
                    let Some(crossed) = analysis::crossed(&detections, *threshold) else {
                        continue;
                    };
                    info!(
                        camera_id = %policy.camera_id,
                        label = %crossed.label,
                        confidence = crossed.confidence,
                        "a plugin saw something worth keeping"
                    );
                    incidents.remember(
                        &policy.camera_id,
                        incident::Keep {
                            from: now - chrono::Duration::seconds(i64::from(*pre_roll_seconds)),
                            to: now + chrono::Duration::seconds(i64::from(*post_roll_seconds)),
                            ready_at: now
                                + chrono::Duration::seconds(i64::from(*post_roll_seconds)),
                        },
                    );
                }
                Err(error) => warn!(
                    camera_id = %policy.camera_id, %error,
                    "nothing could look at this camera"
                ),
            }
        }
    }
}

/// Cut one stretch out of a camera's ring, upload it and file it. Failure is
/// logged and nothing else: the ring is dropping that video either way.
async fn keep_window(
    config: &Config,
    client: &reqwest::Client,
    policy: &vms_domain::RecordingPolicy,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    reason: &str,
) {
    let ring = match ringbuffer::Ring::open(
        &config.recording_dir(),
        &policy.camera_id,
        config.recording_budget_bytes,
    ) {
        Ok(ring) => ring,
        Err(error) => {
            warn!(camera_id = %policy.camera_id, %error, "no ring to keep from");
            return;
        }
    };
    let clip = match ring.clip(from, to) {
        Ok(clip) => clip,
        Err(error) => {
            warn!(
                camera_id = %policy.camera_id, %error, reason,
                from = %from.to_rfc3339(),
                "the ring could not give up a window"
            );
            return;
        }
    };
    let started_at = clip.start;
    let recorded = Recorded {
        started_at,
        codec: clip.codec,
        width: clip.width,
        height: clip.height,
        init: clip.init,
        segments: clip
            .segments
            .into_iter()
            .enumerate()
            .map(|(sequence, segment)| RecordedSegment {
                sequence: sequence as u32,
                started_at: segment.start,
                duration_ms: segment.duration_ms,
                bytes: segment.bytes,
            })
            .collect(),
    };
    match store_recording(
        config,
        client,
        &policy.storage_plugin_id,
        &policy.camera_id,
        &format!("{reason}-{}", from.timestamp()),
        recorded,
    )
    .await
    {
        Ok(manifest) => {
            info!(
                camera_id = %policy.camera_id, reason,
                from = %from.to_rfc3339(), to = %to.to_rfc3339(),
                recording_id = %manifest.recording_id,
                "kept a window"
            );
            if let Err(error) = report_recording(config, client, &manifest).await {
                warn!(camera_id = %policy.camera_id, %error, "a kept clip was not reported");
            }
        }
        Err(error) => warn!(
            camera_id = %policy.camera_id, %error, reason,
            from = %from.to_rfc3339(),
            "a window was not kept"
        ),
    }
}

/// Which cameras a gateway should be recording continuously right now: the
/// ones a policy says so about, and that it still has an address for.
fn cameras_to_record(
    policies: &[vms_domain::RecordingPolicy],
    sources: &HashMap<String, CameraSource>,
) -> Vec<(String, CameraSource)> {
    policies
        .iter()
        .filter(|policy| policy.mode == vms_domain::RecordingMode::Continuous)
        .filter_map(|policy| {
            sources
                .get(&policy.camera_id)
                .map(|source| (policy.camera_id.clone(), source.clone()))
        })
        .collect()
}

/// A source's camera id. Derived from the address, so the same address keeps its
/// identity — and its recordings and incidents — across restarts and re-adds.
fn source_camera_id(address: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, address.as_bytes()).to_string()
}

/// The polled sources worth probing this pass: the pulled ones, minus anything
/// discovery already found at the same address. Pushed streams (RTMP, SRT) are
/// carried by the ingest that receives them, which does not exist yet.
fn sources_to_probe<'a>(
    sources: &'a [vms_domain::VideoSource],
    known: &std::collections::HashSet<String>,
) -> Vec<(&'a vms_domain::VideoSource, String)> {
    sources
        .iter()
        .filter(|source| source.kind == vms_domain::SourceKind::Rtsp)
        .map(|source| (source, source_camera_id(&source.address)))
        .filter(|(_, id)| !known.contains(id))
        .collect()
}

/// Pushed to the gateway rather than pulled from an address.
fn is_pushed(kind: vms_domain::SourceKind) -> bool {
    matches!(
        kind,
        vms_domain::SourceKind::Rtmp | vms_domain::SourceKind::Srt
    )
}

/// What a pushed stream did since the last pass, as a probe would have
/// reported it. An error is what an operator needs to see: nobody is
/// publishing on this key.
fn pushed_metrics(
    config: &Config,
    key: &str,
    marks: &mut HashMap<String, (u64, u64, Instant)>,
    window: Duration,
) -> anyhow::Result<rtsp::RtspMetrics> {
    let Some(stats) = config.ingest.stats(key, window * 2) else {
        marks.remove(key);
        return Err(anyhow!("nothing has ever published to stream key {key}"));
    };
    let now = Instant::now();
    let (frames, bytes, elapsed) =
        match marks.insert(key.to_string(), (stats.frames, stats.bytes, now)) {
            Some((previous_frames, previous_bytes, at)) => (
                stats.frames.saturating_sub(previous_frames),
                stats.bytes.saturating_sub(previous_bytes),
                now.saturating_duration_since(at).as_secs_f64().max(0.001),
            ),
            // First sight of this stream: report the session so far rather than
            // nothing at all.
            None => (stats.frames, stats.bytes, window.as_secs_f64().max(0.001)),
        };
    if !stats.publishing {
        return Err(anyhow!("no publisher on stream key {key}"));
    }
    Ok(rtsp::RtspMetrics {
        codec: stats.codec.map(|_| "H264".to_string()),
        fps: Some((frames as f64 / elapsed) as f32),
        bitrate_kbps: Some(((bytes as f64 * 8.0 / elapsed) / 1000.0).round() as u32),
        packet_loss: 0,
        frames,
        bytes,
    })
}

async fn probe_explicit(
    config: &Config,
    camera_id: String,
    name: String,
    url: &str,
    reconnects: &Arc<RwLock<HashMap<String, u32>>>,
    backoff: &Arc<RwLock<Backoff>>,
) -> CameraTelemetry {
    let (result, held_off) = probe_with_backoff(config, &camera_id, url, backoff).await;
    telemetry_from_probe(
        config, camera_id, name, None, None, None, None, None, None, None, url, result, held_off,
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
            let total = Duration::from_secs(u64::from(*duration_seconds));
            let segment = Duration::from_secs(u64::from(*segment_seconds));
            let cmaf = match &source.push_key {
                // A pushed stream is already arriving; there is nothing to dial.
                Some(key) => {
                    let mut pushed = config
                        .ingest
                        .subscribe(key)
                        .ok_or_else(|| anyhow!("nothing is publishing to stream key {key}"))?;
                    archive::record_from(&mut pushed, total, segment).await?
                }
                None => {
                    archive::record_h264_cmaf(
                        &source.rtsp_uri,
                        source.username.as_deref(),
                        source.password.as_deref(),
                        total,
                        segment,
                    )
                    .await?
                }
            };
            let manifest = store_recording(
                config,
                client,
                storage_plugin_id,
                camera_id,
                &command.id,
                Recorded {
                    started_at,
                    codec: cmaf.codec,
                    width: cmaf.width,
                    height: cmaf.height,
                    init: cmaf.init,
                    segments: cmaf
                        .segments
                        .into_iter()
                        .map(|segment| RecordedSegment {
                            sequence: segment.sequence,
                            started_at: started_at
                                + chrono::Duration::milliseconds(segment.start_offset_ms as i64),
                            duration_ms: segment.duration_ms,
                            bytes: segment.bytes,
                        })
                        .collect(),
                },
            )
            .await?;
            Ok(CommandPayload {
                recording: Some(manifest),
                ..Default::default()
            })
        }
        GatewayCommandKind::SaveClip {
            camera_id,
            seconds,
            storage_plugin_id,
        } => {
            // Nothing is dialled: either the ring still holds those seconds or
            // they are gone, and the error says which.
            let ring = ringbuffer::Ring::open(
                &config.recording_dir(),
                camera_id,
                config.recording_budget_bytes,
            )?;
            let to = Utc::now();
            let from = to - chrono::Duration::seconds(i64::from(*seconds));
            let clip = ring.clip(from, to)?;
            info!(
                command_id = %command.id, camera_id, seconds,
                segments = clip.segments.len(),
                from = %clip.start.to_rfc3339(), to = %clip.end.to_rfc3339(),
                "keeping a clip out of the ring"
            );
            let started_at = clip.start;
            let manifest = store_recording(
                config,
                client,
                storage_plugin_id,
                camera_id,
                &command.id,
                Recorded {
                    started_at,
                    codec: clip.codec,
                    width: clip.width,
                    height: clip.height,
                    init: clip.init,
                    segments: clip
                        .segments
                        .into_iter()
                        .enumerate()
                        .map(|(sequence, segment)| RecordedSegment {
                            sequence: sequence as u32,
                            started_at: segment.start,
                            duration_ms: segment.duration_ms,
                            bytes: segment.bytes,
                        })
                        .collect(),
                },
            )
            .await?;
            Ok(CommandPayload {
                recording: Some(manifest),
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
            let media = match &source.push_key {
                Some(key) => live::LiveMedia::Pushed(Box::new(
                    config
                        .ingest
                        .subscribe(key)
                        .ok_or_else(|| anyhow!("nothing is publishing to stream key {key}"))?,
                )),
                None => live::LiveMedia::Rtsp {
                    url: source.live_rtsp_uri,
                    username: source.username,
                    password: source.password,
                },
            };
            let answer = live::start_h264(
                media,
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

/// A recording that exists as bytes and is not yet anywhere durable.
struct Recorded {
    started_at: DateTime<Utc>,
    codec: String,
    width: u32,
    height: u32,
    init: Vec<u8>,
    segments: Vec<RecordedSegment>,
}

struct RecordedSegment {
    sequence: u32,
    started_at: DateTime<Utc>,
    duration_ms: u64,
    bytes: Vec<u8>,
}

/// Upload a recording and describe it, whether it was just captured or came
/// out of the ring. The media goes straight to the storage plugin; the control
/// plane only ever sees this manifest.
async fn store_recording(
    config: &Config,
    client: &reqwest::Client,
    storage_plugin_id: &str,
    camera_id: &str,
    command_id: &str,
    recorded: Recorded,
) -> anyhow::Result<RecordingManifest> {
    let recording_id = uuid::Uuid::new_v4().to_string();
    let namespace = format!(
        "recordings/{}/{}/{}",
        config.customer_id, config.site_id, camera_id
    );
    let context = invocation_context(config, camera_id, command_id);

    let init = upload_recording_object(
        config,
        client,
        storage_plugin_id,
        &context,
        &namespace,
        &format!("{recording_id}/init.mp4"),
        "video/mp4",
        recorded.init,
    )
    .await?;

    let mut segments = Vec::with_capacity(recorded.segments.len());
    for segment in recorded.segments {
        let key = format!("{recording_id}/seg-{:05}.m4s", segment.sequence);
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
        segments.push(RecordingSegment {
            id: uuid::Uuid::new_v4().to_string(),
            sequence: segment.sequence,
            started_at: segment.started_at,
            ended_at: segment.started_at
                + chrono::Duration::milliseconds(segment.duration_ms as i64),
            duration_ms: segment.duration_ms,
            keyframe: true,
            object,
        });
    }
    let ended_at = segments
        .last()
        .map(|segment| segment.ended_at)
        .unwrap_or(recorded.started_at);
    Ok(RecordingManifest {
        recording_id,
        camera_id: camera_id.to_owned(),
        gateway_id: config.gateway_id.clone(),
        started_at: recorded.started_at,
        ended_at,
        codec: recorded.codec,
        width: recorded.width,
        height: recorded.height,
        init,
        segments,
        delete_after: None,
    })
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

    fn video_source(kind: vms_domain::SourceKind, address: &str) -> vms_domain::VideoSource {
        vms_domain::VideoSource {
            id: format!("src-{address}"),
            gateway_id: "gw-1".into(),
            name: format!("Source at {address}"),
            kind,
            address: address.into(),
            added_at: Utc::now(),
        }
    }

    /// The whole path: the dashboard's source is polled, probed and reported like any
    /// camera, and the command loop can find it to serve live video from.
    #[tokio::test]
    async fn a_source_the_dashboard_added_becomes_a_camera() {
        let camera = fake_camera::FakeCamera::start(false).await.unwrap();
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        *plane.sources.write().await = vec![serde_json::json!({
            "id": "src-1", "gateway_id": "gw-1", "name": "Yard NVR",
            "kind": "rtsp", "address": camera.url, "added_at": Utc::now(),
        })];

        let mut config = config(&plane.url);
        config.discovery_wait = Duration::ZERO; // no multicast in a test
        config.probe_interval = Duration::from_millis(100);
        let telemetry = Arc::new(RwLock::new(Vec::new()));
        let camera_sources = Arc::new(RwLock::new(HashMap::new()));
        let task = tokio::spawn(probe_loop(
            config,
            reqwest::Client::new(),
            Arc::clone(&telemetry),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(Backoff::new(
                Duration::from_millis(50),
                Duration::from_secs(1),
            ))),
            Arc::clone(&camera_sources),
        ));

        let reported = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(camera) = telemetry.read().await.first() {
                    return camera.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        task.abort();
        let reported = reported.expect("the source never reached telemetry");

        assert_eq!(reported.camera_id, source_camera_id(&camera.url));
        assert_eq!(
            reported.name, "Yard NVR",
            "the dashboard's name is what is shown"
        );
        assert_eq!(
            reported.status,
            HealthStatus::Healthy,
            "the fake camera answered: {:?}",
            reported.last_error
        );
        let dialled = camera_sources.read().await;
        assert_eq!(
            dialled.get(&reported.camera_id).map(|s| s.rtsp_uri.clone()),
            Some(camera.url.clone()),
            "live and recording dial the address the source gave"
        );
    }

    /// A pushed stream is a camera to everything downstream: it appears in
    /// telemetry with what it is sending, and nothing was dialled to find out.
    #[tokio::test]
    async fn a_pushed_source_reports_what_is_arriving() {
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        *plane.sources.write().await = vec![serde_json::json!({
            "id": "src-push", "gateway_id": "gw-1", "name": "Loading bay encoder",
            "kind": "rtmp", "address": "loading-bay", "added_at": Utc::now(),
        })];

        let mut config = config(&plane.url);
        config.discovery_wait = Duration::ZERO;
        config.probe_interval = Duration::from_millis(100);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(rtmp::serve(listener, config.ingest.clone()));

        let telemetry = Arc::new(RwLock::new(Vec::new()));
        let camera_sources = Arc::new(RwLock::new(HashMap::new()));
        let task = tokio::spawn(probe_loop(
            config,
            reqwest::Client::new(),
            Arc::clone(&telemetry),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(Backoff::new(
                Duration::from_millis(50),
                Duration::from_secs(1),
            ))),
            Arc::clone(&camera_sources),
        ));

        // Until something publishes, the key is a source that is offline, and
        // the reason says so.
        let silent = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(camera) = telemetry.read().await.first() {
                    return camera.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the pushed source never reached telemetry");
        assert_eq!(silent.status, HealthStatus::Offline);
        assert!(
            silent
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("loading-bay")),
            "{:?}",
            silent.last_error
        );

        // The gateway had to have been given the key: a publisher on it is
        // accepted, and then the same camera is healthy.
        let mut publisher = fake_publisher::FakePublisher::publish_to(address, "loading-bay")
            .await
            .expect("the polled source is what allows this publisher");
        publisher.send_metadata(1920, 1080, 25.0).await.unwrap();
        publisher
            .send_video(
                bytes::Bytes::from_static(&[
                    0x17, 0x00, 0, 0, 0, 1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xe0,
                    0x1e, 1, 0, 4, 0x68, 0xee, 0x3c, 0x80,
                ]),
                0,
            )
            .await
            .unwrap();
        let publishing = tokio::spawn(async move {
            for index in 0..60_u32 {
                let mut tag = vec![if index % 25 == 0 { 0x17 } else { 0x27 }, 0x01, 0, 0, 0];
                tag.extend_from_slice(&[0, 0, 0, 2, 0x65, index as u8]);
                if publisher
                    .send_video(bytes::Bytes::from(tag), index * 40)
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });

        let live = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(camera) = telemetry
                    .read()
                    .await
                    .first()
                    .filter(|camera| camera.status == HealthStatus::Healthy)
                {
                    return camera.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a publisher on the key never made the source healthy");
        task.abort();
        publishing.abort();

        assert_eq!(live.name, "Loading bay encoder");
        assert_eq!(live.codec.as_deref(), Some("H264"));
        assert_eq!(live.width, Some(1920));
        assert!(live.fps.is_some_and(|fps| fps > 0.0), "{:?}", live.fps);
        let carried = camera_sources.read().await;
        assert_eq!(
            carried
                .get(&live.camera_id)
                .and_then(|source| source.push_key.clone()),
            Some("loading-bay".to_string()),
            "live and recording read the ingest rather than dialling"
        );
    }

    /// A camera set to record continuously is recorded without anyone asking
    /// again: the policy is polled, the ring fills, and a clip can be cut out
    /// of it afterwards.
    #[tokio::test]
    async fn a_continuous_policy_starts_recording_on_its_own() {
        let camera = fake_camera::FakeCamera::start(false).await.unwrap();
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        *plane.sources.write().await = vec![serde_json::json!({
            "id": "src-1", "gateway_id": "gw-1", "name": "Yard", "kind": "rtsp",
            "address": camera.url, "added_at": Utc::now(),
        })];
        let camera_id = source_camera_id(&camera.url);
        *plane.policies.write().await = vec![serde_json::json!({
            "camera_id": camera_id, "gateway_id": "gw-1", "mode": "continuous",
            "keep": [], "retention_days": 7, "updated_at": Utc::now(),
        })];

        let ring_dir = tempfile::tempdir().unwrap();
        let mut config = config(&plane.url);
        config.discovery_wait = Duration::ZERO;
        config.probe_interval = Duration::from_millis(100);
        config.recording_dir = Some(ring_dir.path().to_path_buf());
        let task = tokio::spawn(probe_loop(
            config,
            reqwest::Client::new(),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(Backoff::new(
                Duration::from_millis(50),
                Duration::from_secs(1),
            ))),
            Arc::new(RwLock::new(HashMap::new())),
        ));

        // Video on disk is the only proof that matters here.
        let ring = ringbuffer::Ring::open(ring_dir.path(), &camera_id, 10 * 1024 * 1024).unwrap();
        let recorded = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(Some(span)) = ring.span() {
                    return span;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        task.abort();
        let (from, to) = recorded.expect("nothing was recorded without being asked twice");
        assert!(to > from, "the ring holds a stretch of video");
        assert!(
            !ring.segments().unwrap().is_empty(),
            "and the segments are on disk"
        );
    }

    /// A schedule keeps video without anyone asking twice: the window passes,
    /// the gateway cuts it out of its own ring and files it.
    #[tokio::test]
    async fn a_schedule_keeps_a_window_that_has_passed() {
        let camera = fake_camera::FakeCamera::start(false).await.unwrap();
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        *plane.sources.write().await = vec![serde_json::json!({
            "id": "src-1", "gateway_id": "gw-1", "name": "Yard", "kind": "rtsp",
            "address": camera.url, "added_at": Utc::now(),
        })];
        let camera_id = source_camera_id(&camera.url);
        // A window that covers the whole day, so whatever the clock says when
        // this runs, the last few minutes are inside it.
        *plane.policies.write().await = vec![serde_json::json!({
            "camera_id": camera_id, "gateway_id": "gw-1", "mode": "continuous",
            "retention_days": 7, "storage_plugin_id": "storage-s3",
            "updated_at": Utc::now(),
            "keep": [{"type": "schedule", "days": 0, "from_minute": 0, "to_minute": 1439}],
        })];

        let ring_dir = tempfile::tempdir().unwrap();
        let mut config = config(&plane.url);
        config.discovery_wait = Duration::ZERO;
        config.probe_interval = Duration::from_millis(100);
        config.recording_dir = Some(ring_dir.path().to_path_buf());
        // Clips of a few seconds, so this test is about the keeping rather
        // than about waiting ten minutes for a window to fill.
        config.cutting = schedule::Cutting {
            max_clip: chrono::Duration::seconds(4),
            lag: chrono::Duration::seconds(1),
        };

        let camera_ring = ring_dir.path().join(&camera_id);
        std::fs::create_dir_all(&camera_ring).unwrap();
        schedule::write_watermark(&camera_ring, Utc::now()).unwrap();

        let task = tokio::spawn(probe_loop(
            config,
            reqwest::Client::new(),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(Backoff::new(
                Duration::from_millis(50),
                Duration::from_secs(1),
            ))),
            Arc::new(RwLock::new(HashMap::new())),
        ));

        let filed = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let seen = plane.seen.read().await;
                if let Some(first) = seen.filed_recordings.first() {
                    return first.clone();
                }
                drop(seen);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        task.abort();

        let filed = filed.expect("the schedule kept nothing");
        assert_eq!(filed["camera_id"], camera_id);
        assert_eq!(filed["gateway_id"], "gw-1");
        assert!(
            filed["segments"].as_array().is_some_and(|s| !s.is_empty()),
            "a kept window carries its segments: {filed}"
        );
        assert!(
            plane.seen.read().await.blobs > 0,
            "and the media went to storage, not through the control plane"
        );
        // The watermark moved, so the same window is not kept twice.
        let watermark = schedule::read_watermark(&camera_ring).expect("a watermark");
        assert!(
            watermark > Utc::now() - chrono::Duration::minutes(1),
            "the watermark did not move: {watermark}"
        );
    }

    /// The minutes before a camera went dark are the ones an investigation
    /// wants, and they only exist because the gateway was already recording.
    #[tokio::test]
    async fn a_camera_going_quiet_keeps_what_led_up_to_it() {
        let camera = fake_camera::FakeCamera::start(false).await.unwrap();
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        *plane.sources.write().await = vec![serde_json::json!({
            "id": "src-1", "gateway_id": "gw-1", "name": "Yard", "kind": "rtsp",
            "address": camera.url, "added_at": Utc::now(),
        })];
        let camera_id = source_camera_id(&camera.url);
        *plane.policies.write().await = vec![serde_json::json!({
            "camera_id": camera_id, "gateway_id": "gw-1", "mode": "continuous",
            "retention_days": 7, "storage_plugin_id": "storage-s3",
            "updated_at": Utc::now(),
            "keep": [{"type": "on_incident", "pre_roll_seconds": 120, "post_roll_seconds": 0}],
        })];

        let ring_dir = tempfile::tempdir().unwrap();
        let mut config = config(&plane.url);
        config.discovery_wait = Duration::ZERO;
        config.probe_interval = Duration::from_millis(100);
        config.recording_dir = Some(ring_dir.path().to_path_buf());
        let task = tokio::spawn(probe_loop(
            config,
            reqwest::Client::new(),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(Backoff::new(
                Duration::from_millis(50),
                Duration::from_secs(1),
            ))),
            Arc::new(RwLock::new(HashMap::new())),
        ));

        // Let it record a few seconds, so there is something to keep.
        let ring = ringbuffer::Ring::open(ring_dir.path(), &camera_id, 10 * 1024 * 1024).unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if ring.segments().is_ok_and(|segments| !segments.is_empty()) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("nothing was being recorded to keep");

        // Now the camera loses power.
        camera.unplug();

        let filed = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let seen = plane.seen.read().await;
                if let Some(first) = seen.filed_recordings.first() {
                    return first.clone();
                }
                drop(seen);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        task.abort();

        let filed = filed.expect("the camera went dark and nothing was kept");
        assert_eq!(filed["camera_id"], camera_id);
        assert!(
            filed["segments"].as_array().is_some_and(|s| !s.is_empty()),
            "the clip carries the video from before the silence: {filed}"
        );
    }

    /// A camera that offers a snapshot, for a plugin to look at.
    async fn snapshot_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/snapshot.jpg", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0_u8; 1024];
                let _ = socket.read(&mut buffer).await;
                let body = [0xff_u8, 0xd8, 0xff, 0xe0, 0, 0, 0, 0];
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: image/jpeg\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        url
    }

    /// A plugin sure enough about what it saw means the video around that
    /// moment is worth keeping — and the snapshot itself is not worth
    /// uploading, which is the difference between a clip and a bill.
    #[tokio::test]
    async fn a_plugin_that_sees_something_leaves_a_window_to_keep() {
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        let snapshot = snapshot_server().await;
        let config = config(&plane.url);

        let mut sources = HashMap::new();
        sources.insert(
            "cam-1".to_string(),
            CameraSource {
                rtsp_uri: "rtsp://10.0.0.1/stream".into(),
                push_key: None,
                live_rtsp_uri: "rtsp://10.0.0.1/stream".into(),
                snapshot_uri: Some(snapshot),
                username: None,
                password: None,
            },
        );
        let policies = vec![vms_domain::RecordingPolicy {
            camera_id: "cam-1".into(),
            gateway_id: "gw-1".into(),
            mode: vms_domain::RecordingMode::Continuous,
            keep: vec![vms_domain::KeepRule::OnAnalysis {
                plugin_id: "ai-demo".into(),
                every_seconds: 30,
                threshold: 0.75,
                pre_roll_seconds: 20,
                post_roll_seconds: 0,
            }],
            retention_days: 7,
            storage_plugin_id: "storage-s3".into(),
            updated_at: Utc::now(),
        }];

        let mut pacing = analysis::Pacing::new();
        let mut incidents = incident::Incidents::new();
        let now = Utc::now();
        watch_with_plugins(
            &config,
            &reqwest::Client::new(),
            &policies,
            &sources,
            &mut pacing,
            &mut incidents,
            now,
        )
        .await;

        let due = incidents.due(now);
        assert_eq!(due.len(), 1, "the plugin was sure and nothing was kept");
        assert_eq!(due[0].0, "cam-1");
        assert_eq!(due[0].1.from, now - chrono::Duration::seconds(20));
        assert_eq!(plane.seen.read().await.analyses, 1);
        assert_eq!(
            plane.seen.read().await.uploads,
            0,
            "looking at a camera does not store the picture"
        );

        // Asked again straight away, it does not call the plugin again: the
        // policy said every thirty seconds, and a plugin costs money.
        watch_with_plugins(
            &config,
            &reqwest::Client::new(),
            &policies,
            &sources,
            &mut pacing,
            &mut incidents,
            now + chrono::Duration::seconds(5),
        )
        .await;
        assert_eq!(plane.seen.read().await.analyses, 1);
    }

    #[tokio::test]
    async fn a_camera_with_no_snapshot_is_reported_rather_than_watched() {
        // Only cameras that advertise a snapshot URI over ONVIF can be looked
        // at; a pushed stream has no such thing, and pretending otherwise
        // would make a policy look set and do nothing.
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        let config = config(&plane.url);
        let mut sources = HashMap::new();
        sources.insert(
            "cam-1".to_string(),
            CameraSource {
                rtsp_uri: "rtmp://gw/live/key".into(),
                push_key: Some("key".into()),
                live_rtsp_uri: "rtmp://gw/live/key".into(),
                snapshot_uri: None,
                username: None,
                password: None,
            },
        );
        let policies = vec![vms_domain::RecordingPolicy {
            camera_id: "cam-1".into(),
            gateway_id: "gw-1".into(),
            mode: vms_domain::RecordingMode::Continuous,
            keep: vec![vms_domain::KeepRule::OnAnalysis {
                plugin_id: "ai-demo".into(),
                every_seconds: 30,
                threshold: 0.5,
                pre_roll_seconds: 20,
                post_roll_seconds: 0,
            }],
            retention_days: 7,
            storage_plugin_id: "storage-s3".into(),
            updated_at: Utc::now(),
        }];

        let mut pacing = analysis::Pacing::new();
        let mut incidents = incident::Incidents::new();
        let now = Utc::now();
        watch_with_plugins(
            &config,
            &reqwest::Client::new(),
            &policies,
            &sources,
            &mut pacing,
            &mut incidents,
            now,
        )
        .await;
        assert!(incidents.due(now).is_empty());
        assert_eq!(plane.seen.read().await.analyses, 0);
    }

    /// The list is state, not an event: a gateway that cannot reach the API keeps
    /// carrying what it was last told, rather than dropping every source.
    #[tokio::test]
    async fn the_source_list_survives_an_api_that_is_not_answering() {
        let plane = fake_control_plane::FakeControlPlane::start(vec![], 0).await;
        *plane.sources.write().await = vec![serde_json::json!({
            "id": "src-1", "gateway_id": "gw-1", "name": "Yard NVR",
            "kind": "rtsp", "address": "rtsp://10.0.0.7/stream1",
            "added_at": Utc::now(),
        })];
        let client = reqwest::Client::new();

        let fetched = fetch_sources(&client, &config(&plane.url))
            .await
            .expect("the list the API served");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].address, "rtsp://10.0.0.7/stream1");

        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(
            fetch_sources(&client, &config(&format!("http://{closed}")))
                .await
                .is_none(),
            "an API that cannot be reached says nothing, and nothing is what the caller keeps"
        );
    }

    /// A source keeps its identity across restarts and re-adds, because the id comes
    /// from the address — the same rule the explicit URL has always used. Its
    /// recordings and incidents hang off that id.
    #[test]
    fn a_source_is_identified_by_its_address() {
        let one = source_camera_id("rtsp://10.0.0.7/stream1");
        assert_eq!(one, source_camera_id("rtsp://10.0.0.7/stream1"));
        assert_ne!(one, source_camera_id("rtsp://10.0.0.7/stream2"));
    }

    #[test]
    fn only_pulled_sources_are_probed_and_never_one_discovery_already_found() {
        let sources = vec![
            video_source(vms_domain::SourceKind::Rtsp, "rtsp://10.0.0.7/stream1"),
            video_source(vms_domain::SourceKind::Rtsp, "rtsp://10.0.0.8/stream1"),
            // Nothing pushes to this gateway yet; RTMP and SRT ingest come later.
            video_source(vms_domain::SourceKind::Rtmp, "yard-entrance"),
            video_source(vms_domain::SourceKind::Srt, "gate"),
        ];
        // ONVIF already found the first one, at the same address.
        let known: std::collections::HashSet<String> =
            [source_camera_id("rtsp://10.0.0.7/stream1")]
                .into_iter()
                .collect();

        let probing = sources_to_probe(&sources, &known);
        let addresses: Vec<&str> = probing
            .iter()
            .map(|(source, _)| source.address.as_str())
            .collect();
        assert_eq!(
            addresses,
            vec!["rtsp://10.0.0.8/stream1"],
            "a camera discovery found is not a second camera, and nothing pushed is probed"
        );
        assert_eq!(probing[0].1, source_camera_id("rtsp://10.0.0.8/stream1"));
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

    pub(crate) fn config(api_url: &str) -> Config {
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
            ingest: crate::ingest::Ingest::new(),
            recording_dir: None,
            recording_budget_bytes: 2 * 1024 * 1024 * 1024,
            cutting: crate::schedule::Cutting::default(),
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
                push_key: None,
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
        assert!(
            result["status"] == "succeeded" || result["status"] == "failed",
            "a command must always come back with an outcome: {result}"
        );
    }

    fn clip_command(id: &str, camera_id: &str, seconds: u32) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "gateway_id": "gw-1",
            "created_at": Utc::now(),
            "expires_at": Utc::now() + chrono::Duration::minutes(2),
            "kind": {
                "type": "save_clip",
                "camera_id": camera_id,
                "seconds": seconds,
                "storage_plugin_id": "storage-s3",
            },
        })
    }

    /// Fill a camera's ring with a few seconds of video, without a camera.
    async fn ring_with_video(dir: &std::path::Path, camera_id: &str) {
        let ring = ringbuffer::Ring::open(dir, camera_id, 10 * 1024 * 1024).unwrap();
        let mut source = crate::frames::testing::ScriptedSource {
            frames: (0..500)
                .map(|index| crate::frames::Frame {
                    data: bytes::Bytes::from(vec![0_u8; 512]),
                    timestamp: index * 3_600,
                    clock_rate: 90_000,
                    keyframe: index % 25 == 0,
                    new_parameters: index == 0,
                })
                .collect(),
            parameters: Some(crate::frames::VideoParameters {
                rfc6381_codec: "avc1.42e01e".into(),
                pixel_dimensions: (640, 480),
                extra_data: vec![
                    1, 0x42, 0xe0, 0x1e, 0xff, 0xe1, 0, 4, 0x67, 0x42, 0xe0, 0x1e, 1, 0, 4, 0x68,
                    0xee, 0x3c, 0x80,
                ],
                frame_rate: Some((1, 25)),
            }),
        };
        ring.record(&mut source, &tokio::sync::Notify::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_clip_command_answers_out_of_the_ring_without_dialling_anything() {
        // No camera, no source entry, no network: a clip is video that has
        // already been recorded, and the only question is whether it is still
        // on disk.
        let dir = tempfile::tempdir().unwrap();
        ring_with_video(dir.path(), "cam-1").await;
        let api = FakeControlPlane::start(vec![clip_command("cmd-8", "cam-1", 3600)], 0).await;
        let mut config = config(&api.url);
        config.recording_dir = Some(dir.path().to_path_buf());
        tokio::spawn(command_loop(
            config,
            reqwest::Client::new(),
            Arc::new(RwLock::new(HashMap::new())),
        ));

        let completions = api.wait_for_completions(1, Duration::from_secs(20)).await;
        let result = &completions[0];
        assert_eq!(result["command_id"], "cmd-8");
        // The upload has no storage plugin behind it here, so this fails at
        // the upload — which is proof the clip itself was assembled.
        let error = result["error"].as_str().unwrap_or_default();
        assert!(
            !error.contains("nothing recorded") && !error.contains("only goes back"),
            "the ring should have answered: {error}"
        );
        let seen = api.seen.read().await;
        assert!(seen.uploads > 0, "a clip is uploaded like any recording");
    }

    #[tokio::test]
    async fn a_clip_nobody_recorded_says_so_rather_than_failing_obscurely() {
        let dir = tempfile::tempdir().unwrap();
        let api = FakeControlPlane::start(vec![clip_command("cmd-9", "cam-2", 60)], 0).await;
        let mut config = config(&api.url);
        config.recording_dir = Some(dir.path().to_path_buf());
        tokio::spawn(command_loop(
            config,
            reqwest::Client::new(),
            Arc::new(RwLock::new(HashMap::new())),
        ));

        let completions = api.wait_for_completions(1, Duration::from_secs(10)).await;
        assert_eq!(completions[0]["status"], "failed");
        let error = completions[0]["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("nothing recorded"),
            "an operator has to learn the ring is empty, not guess: {error}"
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
