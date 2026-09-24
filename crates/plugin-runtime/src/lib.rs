use std::{
    collections::BTreeMap,
    env,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use reqwest::Client;
use tokio::sync::RwLock;
use tracing::warn;
use vms_plugin_sdk::{
    AiAnalyzeRequest, AiAnalyzeResponse, EventDeliveryRequest, EventDeliveryResponse,
    PLUGIN_PROTOCOL_VERSION, PluginCapability, PluginHealth, PluginManifest, PluginRegistration,
    RegisteredPlugin, SignedTransfer, StorageDeleteRequest, StorageDeleteResponse,
    StorageDownloadRequest, StorageUploadRequest,
};

/// How long each kind of call may take. Inference is not a health check: one
/// number for both means either a model gets cut off or a dead plugin holds a
/// request open for half a minute.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Manifest, health, and anything else that should answer at once.
    pub quick: Duration,
    /// Signing a URL, delivering an event: a round trip to somebody else's
    /// service.
    pub normal: Duration,
    /// Running a model.
    pub inference: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            quick: Duration::from_secs(3),
            normal: Duration::from_secs(8),
            inference: Duration::from_secs(
                env::var("PLUGIN_AI_TIMEOUT_SECONDS")
                    .ok()
                    .and_then(|raw| raw.parse().ok())
                    .unwrap_or(30),
            ),
        }
    }
}

/// How many consecutive failures mean a plugin is down rather than unlucky.
const TRIP_AFTER: u32 = 3;
/// The first cooling period, doubled on each further failure up to the cap.
const FIRST_COOLDOWN: Duration = Duration::from_secs(15);
const MAX_COOLDOWN: Duration = Duration::from_secs(300);

/// What this registry remembers about a plugin that has been failing.
#[derive(Debug, Clone, Default)]
struct Breaker {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    cooldown: Option<Duration>,
}

#[derive(Clone)]
pub struct PluginRegistry {
    client: Client,
    timeouts: Timeouts,
    plugins: Arc<RwLock<BTreeMap<String, PluginEntry>>>,
    /// Keyed by plugin id, and deliberately not inside `PluginEntry`: a
    /// reload replaces registrations, and a plugin that was down a second ago
    /// is still down after somebody edits a manifest.
    breakers: Arc<RwLock<BTreeMap<String, Breaker>>>,
}

#[derive(Clone)]
struct PluginEntry {
    registration: PluginRegistration,
    manifest: PluginManifest,
    reachable: bool,
    last_error: Option<String>,
}

impl PluginRegistry {
    pub async fn load_dir(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        // No client-wide timeout: each call says how long it may take.
        let client = Client::builder().build()?;
        let registry = Self {
            client,
            timeouts: Timeouts::default(),
            plugins: Arc::new(RwLock::new(BTreeMap::new())),
            breakers: Arc::new(RwLock::new(BTreeMap::new())),
        };
        registry.reload(path).await?;
        Ok(registry)
    }

    /// A registry holding exactly these registrations and nothing on disk.
    /// What a conformance check wants: one plugin, asked directly.
    pub async fn from_registrations(
        registrations: Vec<PluginRegistration>,
    ) -> anyhow::Result<Self> {
        let registry = Self {
            client: Client::builder().build()?,
            timeouts: Timeouts::default(),
            plugins: Arc::new(RwLock::new(BTreeMap::new())),
            breakers: Arc::new(RwLock::new(BTreeMap::new())),
        };
        for registration in registrations {
            if let Some((id, entry)) = registry.entry_for(&registration).await {
                registry.plugins.write().await.insert(id, entry);
            }
        }
        Ok(registry)
    }

    /// Reload from disk, then from whatever else the caller keeps
    /// registrations in.
    ///
    /// Files are the bootstrap: a control plane whose database is having a
    /// bad day still comes up with the plugins the box was installed with.
    /// A registration from the caller wins over a file with the same id,
    /// because somebody typed it more recently.
    pub async fn reload_with(
        &self,
        path: impl AsRef<Path>,
        extra: Vec<PluginRegistration>,
    ) -> anyhow::Result<()> {
        self.reload(path).await?;
        for registration in extra {
            match self.entry_for(&registration).await {
                Some((id, entry)) => {
                    self.plugins.write().await.insert(id, entry);
                }
                None => warn!(
                    endpoint = %registration.endpoint,
                    "a stored plugin registration could not be read and was skipped"
                ),
            }
        }
        Ok(())
    }

    pub async fn reload(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        let path = path.as_ref();
        let mut loaded = BTreeMap::new();
        if !path.exists() {
            *self.plugins.write().await = loaded;
            return Ok(());
        }
        for item in std::fs::read_dir(path)
            .with_context(|| format!("read plugin dir {}", path.display()))?
        {
            let item = item?;
            let file = item.path();
            if file.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            // A bad file costs that plugin and no more. Failing the whole reload
            // would turn one editing slip into a fleet-wide outage, and on a
            // running control plane the previous set would silently stay in
            // place while the operator believed the change had applied. The
            // unreachable-plugin and wrong-protocol cases below already skip;
            // this is the same class of fault.
            let raw = match std::fs::read_to_string(&file) {
                Ok(raw) => raw,
                Err(error) => {
                    warn!(file = %file.display(), %error, "cannot read plugin manifest; skipping it");
                    continue;
                }
            };
            let registration: PluginRegistration = match serde_json::from_str(&raw) {
                Ok(registration) => registration,
                Err(error) => {
                    warn!(file = %file.display(), %error, "plugin manifest is not a valid registration; skipping it");
                    continue;
                }
            };
            match self.entry_for(&registration).await {
                Some((id, entry)) => {
                    loaded.insert(id, entry);
                }
                None => {
                    warn!(file = %file.display(), "this plugin could not be registered; skipping it")
                }
            }
        }
        *self.plugins.write().await = loaded;
        Ok(())
    }

    /// Ask a plugin what it is, without registering it. What a control plane
    /// does before writing down a registration somebody typed.
    pub async fn describe(
        &self,
        registration: &PluginRegistration,
    ) -> anyhow::Result<PluginManifest> {
        let manifest = self.fetch_manifest(registration).await?;
        if manifest.protocol_version != PLUGIN_PROTOCOL_VERSION {
            return Err(anyhow!(
                "plugin {} speaks protocol {}, this build speaks {}",
                manifest.id,
                manifest.protocol_version,
                PLUGIN_PROTOCOL_VERSION
            ));
        }
        Ok(manifest)
    }

    /// Turn a registration into a registry entry: ask the plugin what it is,
    /// fall back to the manifest it was registered with, and refuse a
    /// protocol this build does not speak.
    ///
    /// The same path for a file and for a row: one of them being newer is no
    /// reason for it to be loaded differently.
    async fn entry_for(&self, registration: &PluginRegistration) -> Option<(String, PluginEntry)> {
        if !registration.enabled {
            return None;
        }
        let fallback = registration.manifest.clone();
        let (manifest, reachable, last_error) = match self.fetch_manifest(registration).await {
            Ok(manifest) => (manifest, true, None),
            Err(error) => {
                let manifest = fallback?;
                (manifest, false, Some(error.to_string()))
            }
        };
        if manifest.protocol_version != PLUGIN_PROTOCOL_VERSION {
            warn!(
                plugin = %manifest.id,
                protocol = manifest.protocol_version,
                "unsupported plugin protocol"
            );
            return None;
        }
        Some((
            manifest.id.clone(),
            PluginEntry {
                registration: registration.clone(),
                manifest,
                reachable,
                last_error,
            },
        ))
    }

    pub async fn list(&self) -> Vec<RegisteredPlugin> {
        let now = Instant::now();
        let breakers = self.breakers.read().await.clone();
        self.plugins
            .read()
            .await
            .values()
            .map(|entry| RegisteredPlugin {
                endpoint: entry.registration.endpoint.clone(),
                placement: entry.registration.placement.clone(),
                enabled: entry.registration.enabled,
                reachable: entry.reachable,
                cooling_off_seconds: breakers
                    .get(&entry.manifest.id)
                    .and_then(|breaker| breaker.open_until)
                    .and_then(|until| until.checked_duration_since(now))
                    .map(|left| left.as_secs() + 1),
                manifest: entry.manifest.clone(),
                last_error: entry.last_error.clone(),
            })
            .collect()
    }

    pub async fn health(&self, id: &str) -> anyhow::Result<PluginHealth> {
        let entry = self.entry(id).await?;
        self.request(
            &entry,
            reqwest::Method::GET,
            "/v1/plugin/health",
            None::<&()>,
            self.timeouts.quick,
        )
        .await
    }

    pub async fn ai_analyze(
        &self,
        id: &str,
        body: &AiAnalyzeRequest,
    ) -> anyhow::Result<AiAnalyzeResponse> {
        let entry = self
            .entry_with_capability(id, PluginCapability::AiAnalyze)
            .await?;
        self.guarded(
            id,
            self.request(
                &entry,
                reqwest::Method::POST,
                "/v1/ai/analyze",
                Some(body),
                self.timeouts.inference,
            ),
        )
        .await
    }

    pub async fn storage_upload(
        &self,
        id: &str,
        body: &StorageUploadRequest,
    ) -> anyhow::Result<SignedTransfer> {
        let entry = self
            .entry_with_capability(id, PluginCapability::StorageBlob)
            .await?;
        self.guarded(
            id,
            self.request(
                &entry,
                reqwest::Method::POST,
                "/v1/storage/uploads",
                Some(body),
                self.timeouts.normal,
            ),
        )
        .await
    }

    /// Hand one event to one sink. Anything but a 2xx is an error the caller
    /// will retry; a sink that does not want the event answers `delivered:
    /// false` and is not asked again.
    pub async fn deliver_event(
        &self,
        id: &str,
        body: &EventDeliveryRequest,
    ) -> anyhow::Result<EventDeliveryResponse> {
        let entry = self
            .entry_with_capability(id, PluginCapability::EventSink)
            .await?;
        self.guarded(
            id,
            self.request(
                &entry,
                reqwest::Method::POST,
                "/v1/events",
                Some(body),
                self.timeouts.normal,
            ),
        )
        .await
    }

    /// Every plugin that is enabled, reachable and says it takes events.
    pub async fn event_sinks(&self) -> Vec<String> {
        self.plugins
            .read()
            .await
            .values()
            .filter(|entry| {
                entry.registration.enabled
                    && entry
                        .manifest
                        .capabilities
                        .contains(&PluginCapability::EventSink)
            })
            .map(|entry| entry.manifest.id.clone())
            .collect()
    }

    pub async fn storage_download(
        &self,
        id: &str,
        body: &StorageDownloadRequest,
    ) -> anyhow::Result<SignedTransfer> {
        let entry = self
            .entry_with_capability(id, PluginCapability::StorageBlob)
            .await?;
        self.guarded(
            id,
            self.request(
                &entry,
                reqwest::Method::POST,
                "/v1/storage/downloads",
                Some(body),
                self.timeouts.normal,
            ),
        )
        .await
    }

    pub async fn storage_delete(
        &self,
        id: &str,
        body: &StorageDeleteRequest,
    ) -> anyhow::Result<StorageDeleteResponse> {
        let entry = self
            .entry_with_capability(id, PluginCapability::StorageBlob)
            .await?;
        self.guarded(
            id,
            self.request(
                &entry,
                reqwest::Method::POST,
                "/v1/storage/delete",
                Some(body),
                self.timeouts.normal,
            ),
        )
        .await
    }

    /// Whether a plugin with this id is registered at all.
    ///
    /// Handlers need this to answer "no such plugin" with 404 rather than the
    /// 502 that a reachable-but-failing plugin earns. Collapsing the two makes a
    /// typo look like an outage, and clients that retry on 502 retry forever
    /// against something that will never exist.
    pub async fn is_registered(&self, id: &str) -> bool {
        self.plugins.read().await.contains_key(id)
    }

    async fn entry(&self, id: &str) -> anyhow::Result<PluginEntry> {
        self.plugins
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("plugin {id} is not registered"))
    }

    async fn entry_with_capability(
        &self,
        id: &str,
        capability: PluginCapability,
    ) -> anyhow::Result<PluginEntry> {
        let entry = self.entry(id).await?;
        if !entry.manifest.capabilities.contains(&capability) {
            return Err(anyhow!("plugin {id} does not provide {capability:?}"));
        }
        Ok(entry)
    }

    async fn fetch_manifest(
        &self,
        registration: &PluginRegistration,
    ) -> anyhow::Result<PluginManifest> {
        self.request_registration(
            registration,
            reqwest::Method::GET,
            "/v1/plugin/manifest",
            None::<&()>,
            self.timeouts.quick,
        )
        .await
    }

    async fn request<T: serde::de::DeserializeOwned, B: serde::Serialize + ?Sized>(
        &self,
        entry: &PluginEntry,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
        timeout: Duration,
    ) -> anyhow::Result<T> {
        self.request_registration(&entry.registration, method, path, body, timeout)
            .await
    }

    /// Run a call unless this plugin is known to be down, and remember how it
    /// went.
    ///
    /// A plugin that is not answering costs every caller the full timeout,
    /// one after another, for as long as it stays down. After a few failures
    /// in a row this stops trying until a cooling period has passed, so the
    /// cost of a dead plugin is one slow call rather than all of them.
    async fn guarded<T>(
        &self,
        id: &str,
        call: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        if let Some(open_until) = self
            .breakers
            .read()
            .await
            .get(id)
            .and_then(|b| b.open_until)
            && open_until > Instant::now()
        {
            return Err(anyhow!(
                "plugin {id} is not answering; not tried again for another {} seconds",
                (open_until - Instant::now()).as_secs() + 1
            ));
        }
        let outcome = call.await;
        let mut breakers = self.breakers.write().await;
        let breaker = breakers.entry(id.to_owned()).or_default();
        match &outcome {
            Ok(_) => *breaker = Breaker::default(),
            Err(_) => {
                breaker.consecutive_failures += 1;
                if breaker.consecutive_failures >= TRIP_AFTER {
                    // Each further failure waits longer, up to the cap: a
                    // plugin that is gone for the afternoon should not be
                    // dialled every fifteen seconds all afternoon.
                    let cooldown = breaker
                        .cooldown
                        .map(|last| (last * 2).min(MAX_COOLDOWN))
                        .unwrap_or(FIRST_COOLDOWN);
                    breaker.cooldown = Some(cooldown);
                    breaker.open_until = Some(Instant::now() + cooldown);
                    warn!(
                        plugin_id = id,
                        seconds = cooldown.as_secs(),
                        "a plugin stopped answering; leaving it alone for a while"
                    );
                }
            }
        }
        outcome
    }

    /// Whether this plugin is currently being left alone, and for how long.
    pub async fn cooling_off(&self, id: &str) -> Option<Duration> {
        let open_until = self.breakers.read().await.get(id)?.open_until?;
        open_until.checked_duration_since(Instant::now())
    }

    async fn request_registration<T: serde::de::DeserializeOwned, B: serde::Serialize + ?Sized>(
        &self,
        registration: &PluginRegistration,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
        timeout: Duration,
    ) -> anyhow::Result<T> {
        let url = format!("{}{}", registration.endpoint.trim_end_matches('/'), path);
        let mut request = self.client.request(method, url).timeout(timeout);
        if let Some(token_env) = &registration.token_env
            && let Ok(token) = env::var(token_env)
        {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A plugin endpoint that is guaranteed not to answer. Loading must fall back
    /// to the embedded manifest rather than hang or drop the plugin.
    const DEAD_ENDPOINT: &str = "http://127.0.0.1:1";

    fn manifest(id: &str, capabilities: &[&str], protocol: u32) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": id,
            "version": "0.1.0",
            "protocol_version": protocol,
            "vendor": "test",
            "description": null,
            "capabilities": capabilities,
        })
    }

    fn write(dir: &std::path::Path, name: &str, value: &serde_json::Value) {
        let mut file = std::fs::File::create(dir.join(name)).unwrap();
        file.write_all(value.to_string().as_bytes()).unwrap();
    }

    /// Registration for a plugin that is offline but carries its own manifest.
    fn offline(id: &str, capabilities: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "endpoint": DEAD_ENDPOINT,
            "enabled": true,
            "token_env": null,
            "manifest": manifest(id, capabilities, PLUGIN_PROTOCOL_VERSION),
            "placement": "either",
        })
    }

    fn download_request() -> vms_plugin_sdk::StorageDownloadRequest {
        vms_plugin_sdk::StorageDownloadRequest {
            context: Default::default(),
            object_ref: "obj-1".into(),
            expires_seconds: 60,
            audience: Default::default(),
        }
    }

    fn delete_request() -> vms_plugin_sdk::StorageDeleteRequest {
        vms_plugin_sdk::StorageDeleteRequest {
            context: Default::default(),
            object_ref: "obj-1".into(),
        }
    }

    fn upload_request() -> vms_plugin_sdk::StorageUploadRequest {
        vms_plugin_sdk::StorageUploadRequest {
            context: Default::default(),
            namespace: "recordings".into(),
            object_key: "obj-1".into(),
            content_type: "video/mp4".into(),
            content_length: Some(1),
            expires_seconds: 60,
            audience: Default::default(),
            metadata: Default::default(),
        }
    }

    fn analyze_request() -> AiAnalyzeRequest {
        AiAnalyzeRequest {
            context: Default::default(),
            camera_id: "cam-1".into(),
            captured_at: chrono::Utc::now(),
            input: vms_plugin_sdk::MediaInput::InlineBase64 {
                content_type: "image/jpeg".into(),
                data_base64: String::new(),
            },
            tasks: Vec::new(),
            parameters: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn an_offline_plugin_still_registers_from_its_embedded_manifest() {
        // The UI has to be able to show a plugin that is merely down, and say so,
        // rather than pretend it was never configured.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ai.json", &offline("ai-1", &["ai_analyze"]));
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();

        let listed = registry.list().await;
        assert_eq!(listed.len(), 1);
        assert!(registry.is_registered("ai-1").await);
        assert!(!listed[0].reachable, "an unreachable plugin must say so");
        assert!(
            listed[0].last_error.is_some(),
            "and must carry the reason, or the operator has nothing to act on"
        );
    }

    #[tokio::test]
    async fn an_offline_plugin_with_no_embedded_manifest_is_dropped() {
        // Nothing is known about it, so there is nothing to show or to call.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ai.json",
            &serde_json::json!({"endpoint": DEAD_ENDPOINT, "enabled": true, "manifest": null}),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn a_disabled_plugin_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = offline("ai-1", &["ai_analyze"]);
        reg["enabled"] = serde_json::json!(false);
        write(dir.path(), "ai.json", &reg);
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(!registry.is_registered("ai-1").await);
    }

    #[tokio::test]
    async fn a_plugin_speaking_another_protocol_version_is_refused() {
        // Loading it would mean calling it with a contract it does not implement.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ai.json",
            &serde_json::json!({
                "endpoint": DEAD_ENDPOINT,
                "enabled": true,
                "manifest": manifest("ai-future", &["ai_analyze"], PLUGIN_PROTOCOL_VERSION + 1),
            }),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(!registry.is_registered("ai-future").await);
    }

    #[tokio::test]
    async fn files_that_are_not_manifests_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ai.json", &offline("ai-1", &["ai_analyze"]));
        std::fs::write(dir.path().join("README.md"), "not a manifest").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "{ nonsense").unwrap();
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert_eq!(registry.list().await.len(), 1);
    }

    #[tokio::test]
    async fn a_missing_plugin_directory_is_empty_rather_than_fatal() {
        // A deployment with no plugins configured must still start.
        let registry = PluginRegistry::load_dir("/nonexistent/plugins.d")
            .await
            .unwrap();
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn one_malformed_manifest_does_not_take_down_the_others() {
        // A typo in one file must cost that plugin and no more. Failing the
        // whole reload would turn an editing slip into a fleet-wide outage,
        // and on a running control plane the previous set would silently stay
        // in place while the operator believed the change had applied.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "good.json", &offline("ai-1", &["ai_analyze"]));
        write(
            dir.path(),
            "also-good.json",
            &offline("store-1", &["storage_blob"]),
        );
        std::fs::write(dir.path().join("bad.json"), "{ not json").unwrap();

        let registry = PluginRegistry::load_dir(dir.path())
            .await
            .expect("a bad file must not fail the load");
        assert!(registry.is_registered("ai-1").await);
        assert!(registry.is_registered("store-1").await);
        assert_eq!(registry.list().await.len(), 2);
    }

    #[tokio::test]
    async fn a_manifest_that_is_valid_json_but_not_a_registration_is_skipped_too() {
        // Same failure, one layer down: the file parses, the shape is wrong.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "good.json", &offline("ai-1", &["ai_analyze"]));
        std::fs::write(
            dir.path().join("wrong-shape.json"),
            serde_json::json!({"hello": "world"}).to_string(),
        )
        .unwrap();
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert_eq!(registry.list().await.len(), 1);
    }

    #[tokio::test]
    async fn a_directory_that_cannot_be_read_is_still_an_error() {
        // Skipping individual bad files must not turn an unreadable directory
        // into a silent empty registry — that is a configuration fault, not a
        // plugin fault, and it should be loud.
        let dir = tempfile::tempdir().unwrap();
        let unreadable = dir.path().join("locked");
        std::fs::create_dir(&unreadable).unwrap();
        let mut perms = std::fs::metadata(&unreadable).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&unreadable, perms.clone()).unwrap();

        let result = PluginRegistry::load_dir(&unreadable).await;

        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&unreadable, perms).unwrap();
        assert!(
            result.is_err(),
            "an unreadable plugin directory must be reported"
        );
    }

    #[tokio::test]
    async fn reload_replaces_the_set_rather_than_merging_into_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ai.json", &offline("ai-1", &["ai_analyze"]));
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(registry.is_registered("ai-1").await);

        std::fs::remove_file(dir.path().join("ai.json")).unwrap();
        write(
            dir.path(),
            "store.json",
            &offline("store-1", &["storage_blob"]),
        );
        registry.reload(dir.path()).await.unwrap();

        assert!(
            !registry.is_registered("ai-1").await,
            "a removed manifest must stop being served"
        );
        assert!(registry.is_registered("store-1").await);
    }

    // ---- capability enforcement ----

    #[tokio::test]
    async fn a_plugin_cannot_be_used_for_a_capability_it_does_not_declare() {
        // The security boundary of the plugin system. Storage handles recorded
        // video and hands out signed URLs; an AI plugin declaring only
        // ai_analyze must never be reachable through the storage calls, whatever
        // id the caller supplies.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ai.json", &offline("ai-only", &["ai_analyze"]));
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();

        let error = registry
            .storage_download("ai-only", &download_request())
            .await
            .expect_err("an ai plugin must not serve storage");
        assert!(
            error.to_string().contains("does not provide"),
            "refusal must name the missing capability, got: {error}"
        );

        assert!(
            registry
                .storage_delete("ai-only", &delete_request())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_storage_plugin_cannot_be_used_for_inference() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "s.json",
            &offline("store-only", &["storage_blob"]),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(
            registry
                .ai_analyze("store-only", &analyze_request())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_capability_check_happens_before_the_network_call() {
        // Otherwise a wrongly-declared plugin would still receive the payload —
        // for storage that means the recording itself — before being refused.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ai.json", &offline("ai-only", &["ai_analyze"]));
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        let started = std::time::Instant::now();
        let _ = registry.storage_upload("ai-only", &upload_request()).await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "the refusal waited on a network round trip, so the body was already sent"
        );
    }

    // ---- the HTTP path, against a plugin that actually answers ----

    /// Minimal plugin server: answers the manifest, echoes back whether it saw a
    /// bearer token, and can be told to fail.
    async fn fake_plugin(id: &'static str, fail: bool) -> (String, Arc<RwLock<Option<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen_token: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let recorder = Arc::clone(&seen_token);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    if let Some(token) = request.lines().find_map(|line| {
                        line.strip_prefix("authorization: Bearer ")
                            .or_else(|| line.strip_prefix("Authorization: Bearer "))
                    }) {
                        *recorder.write().await = Some(token.trim().to_owned());
                    }
                    let body = if fail {
                        None
                    } else if request.contains("/v1/plugin/manifest") {
                        Some(manifest(id, &["ai_analyze"], PLUGIN_PROTOCOL_VERSION).to_string())
                    } else if request.contains("/v1/plugin/health") {
                        Some(serde_json::json!({"healthy": true, "detail": null}).to_string())
                    } else {
                        Some(serde_json::json!({"detections": []}).to_string())
                    };
                    let response = match body {
                        Some(body) => format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        None => "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (endpoint, seen_token)
    }

    /// A plugin that takes events, and remembers what it was given.
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
                    let mut buf = vec![0u8; 16384];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    let body = if request.contains("/v1/plugin/manifest") {
                        manifest(id, &["event_sink"], PLUGIN_PROTOCOL_VERSION).to_string()
                    } else if request.contains("/v1/plugin/health") {
                        serde_json::json!({"healthy": true, "detail": null}).to_string()
                    } else {
                        if let Some(payload) = request.split("\r\n\r\n").nth(1)
                            && let Ok(parsed) =
                                serde_json::from_str::<serde_json::Value>(payload.trim())
                        {
                            recorder.write().await.push(parsed);
                        }
                        serde_json::json!({
                            "delivered": accept,
                            "detail": if accept { None } else { Some("not for me") },
                        })
                        .to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (endpoint, seen)
    }

    fn event() -> vms_plugin_sdk::FleetEvent {
        vms_plugin_sdk::FleetEvent {
            id: "evt-1".into(),
            kind: vms_plugin_sdk::FleetEventKind::CameraOffline,
            severity: vms_plugin_sdk::EventSeverity::Critical,
            occurred_at: chrono::Utc::now(),
            customer_id: "cust-1".into(),
            site_id: "site-1".into(),
            site_name: "Bakery".into(),
            gateway_id: Some("gw-1".into()),
            camera_id: Some("cam-1".into()),
            title: "Yard camera stopped answering at Bakery".into(),
            detail: Some("RTSP probe failed".into()),
            metadata: serde_json::json!({"reconnects": 3}),
        }
    }

    /// A sink that counts how many times it was actually dialled, so a test
    /// can tell "refused" from "never asked".
    async fn counting_sink(id: &'static str) -> (String, Arc<RwLock<u32>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls: Arc<RwLock<u32>> = Arc::new(RwLock::new(0));
        let counter = Arc::clone(&calls);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    let body = if request.contains("/v1/plugin/manifest") {
                        Some(manifest(id, &["event_sink"], PLUGIN_PROTOCOL_VERSION).to_string())
                    } else {
                        *counter.write().await += 1;
                        None
                    };
                    let response = match body {
                        Some(body) => format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        None => "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (endpoint, calls)
    }

    #[tokio::test]
    async fn a_stored_registration_wins_over_a_file_with_the_same_id() {
        // Files are the bootstrap; a row is what somebody typed more
        // recently, and the two disagreeing is normal during a migration.
        let (endpoint, _) = fake_sink("sink-1", true).await;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "sink.json", &offline("sink-1", &["event_sink"]));
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(
            !registry.list().await[0].reachable,
            "the file's dead endpoint"
        );

        registry
            .reload_with(
                dir.path(),
                vec![
                    serde_json::from_value(serde_json::json!({
                        "endpoint": endpoint, "enabled": true, "manifest": null,
                    }))
                    .unwrap(),
                ],
            )
            .await
            .unwrap();
        let listed = registry.list().await;
        assert_eq!(listed.len(), 1, "one plugin, not two: {listed:?}");
        assert!(listed[0].reachable, "the stored endpoint is the live one");
    }

    #[tokio::test]
    async fn a_stored_registration_that_answers_nothing_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ai.json", &offline("ai-1", &["ai_analyze"]));
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        registry
            .reload_with(
                dir.path(),
                vec![
                    serde_json::from_value(serde_json::json!({
                        "endpoint": DEAD_ENDPOINT, "enabled": true, "manifest": null,
                    }))
                    .unwrap(),
                ],
            )
            .await
            .expect("one bad row must not take the rest down");
        let listed = registry.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].manifest.id, "ai-1");
    }

    #[tokio::test]
    async fn a_plugin_that_keeps_failing_is_left_alone_for_a_while() {
        // A plugin that is down costs every caller the full timeout, one
        // after another, for as long as it stays down. After a few failures
        // the cost should be one slow call rather than all of them.
        let (endpoint, calls) = counting_sink("sink-flaky").await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sink.json",
            &serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null}),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        let request = EventDeliveryRequest {
            context: Default::default(),
            event: event(),
        };

        for attempt in 1..=TRIP_AFTER {
            assert!(
                registry
                    .deliver_event("sink-flaky", &request)
                    .await
                    .is_err(),
                "attempt {attempt} should have failed"
            );
        }
        assert_eq!(*calls.read().await, TRIP_AFTER, "each attempt was tried");
        assert!(
            registry.cooling_off("sink-flaky").await.is_some(),
            "the breaker should be open"
        );

        let error = registry
            .deliver_event("sink-flaky", &request)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not answering"), "{error}");
        assert!(error.contains("seconds"), "it says how long: {error}");
        assert_eq!(
            *calls.read().await,
            TRIP_AFTER,
            "a call while the breaker is open must not reach the plugin"
        );
    }

    #[tokio::test]
    async fn a_plugin_that_answers_again_is_forgiven() {
        let (endpoint, _) = fake_sink("sink-ok", true).await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sink.json",
            &serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null}),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        let request = EventDeliveryRequest {
            context: Default::default(),
            event: event(),
        };
        assert!(registry.deliver_event("sink-ok", &request).await.is_ok());
        assert!(
            registry.cooling_off("sink-ok").await.is_none(),
            "nothing to forgive, nothing held against it"
        );
    }

    #[tokio::test]
    async fn a_reload_does_not_forget_that_a_plugin_is_down() {
        // Editing a manifest is not evidence that the plugin came back.
        let (endpoint, calls) = counting_sink("sink-edited").await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sink.json",
            &serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null}),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        let request = EventDeliveryRequest {
            context: Default::default(),
            event: event(),
        };
        for _ in 0..TRIP_AFTER {
            let _ = registry.deliver_event("sink-edited", &request).await;
        }
        let before = *calls.read().await;

        registry.reload(dir.path()).await.unwrap();
        assert!(
            registry
                .deliver_event("sink-edited", &request)
                .await
                .is_err()
        );
        assert_eq!(*calls.read().await, before, "the reload re-opened the tap");
    }

    #[tokio::test]
    async fn an_event_reaches_a_sink_whole() {
        let (endpoint, seen) = fake_sink("sink-1", true).await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sink.json",
            &serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null}),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert_eq!(registry.event_sinks().await, vec!["sink-1".to_string()]);

        let answer = registry
            .deliver_event(
                "sink-1",
                &EventDeliveryRequest {
                    context: Default::default(),
                    event: event(),
                },
            )
            .await
            .expect("the sink answered");
        assert!(answer.delivered);

        let seen = seen.read().await;
        assert_eq!(seen.len(), 1, "one call, one event");
        // The sink has to be able to write a message from this and nothing
        // else, so the whole event goes over, not an id to look up.
        assert_eq!(seen[0]["event"]["id"], "evt-1");
        assert_eq!(seen[0]["event"]["kind"], "camera_offline");
        assert_eq!(seen[0]["event"]["severity"], "critical");
        assert_eq!(
            seen[0]["event"]["title"],
            "Yard camera stopped answering at Bakery"
        );
        assert_eq!(seen[0]["event"]["site_name"], "Bakery");
        assert_eq!(seen[0]["event"]["metadata"]["reconnects"], 3);
    }

    #[tokio::test]
    async fn a_sink_that_does_not_want_an_event_says_so_rather_than_failing() {
        let (endpoint, _) = fake_sink("sink-quiet", false).await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sink.json",
            &serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null}),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        let answer = registry
            .deliver_event(
                "sink-quiet",
                &EventDeliveryRequest {
                    context: Default::default(),
                    event: event(),
                },
            )
            .await
            .expect("declining is an answer, not an error");
        assert!(!answer.delivered);
        assert_eq!(answer.detail.as_deref(), Some("not for me"));
    }

    #[tokio::test]
    async fn a_plugin_that_does_not_take_events_is_not_asked_to() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ai.json", &offline("ai-only", &["ai_analyze"]));
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(registry.event_sinks().await.is_empty());

        let error = registry
            .deliver_event(
                "ai-only",
                &EventDeliveryRequest {
                    context: Default::default(),
                    event: event(),
                },
            )
            .await
            .expect_err("it never said it takes events");
        assert!(error.to_string().contains("EventSink"), "{error}");
    }

    #[tokio::test]
    async fn a_disabled_sink_is_not_in_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut registration = offline("sink-off", &["event_sink"]);
        registration["enabled"] = serde_json::json!(false);
        write(dir.path(), "sink.json", &registration);
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(registry.event_sinks().await.is_empty());
    }

    #[tokio::test]
    async fn a_reachable_plugin_is_loaded_from_its_own_manifest_and_marked_reachable() {
        let (endpoint, _) = fake_plugin("live-ai", false).await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ai.json",
            &serde_json::json!({"endpoint": endpoint, "enabled": true, "manifest": null}),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        let listed = registry.list().await;
        assert_eq!(listed.len(), 1, "the manifest was served, so it must load");
        assert!(listed[0].reachable);
        assert!(listed[0].last_error.is_none());
    }

    #[tokio::test]
    async fn the_configured_token_is_sent_to_the_plugin() {
        // A plugin that requires auth is unreachable if the header is dropped,
        // and nothing else would show which side lost it.
        let (endpoint, seen) = fake_plugin("tok-ai", false).await;
        unsafe { env::set_var("TEST_PLUGIN_TOKEN_A", "s3cr3t") };
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ai.json",
            &serde_json::json!({
                "endpoint": endpoint,
                "enabled": true,
                "token_env": "TEST_PLUGIN_TOKEN_A",
                "manifest": null,
            }),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(registry.is_registered("tok-ai").await);
        assert_eq!(seen.read().await.as_deref(), Some("s3cr3t"));
    }

    #[tokio::test]
    async fn a_plugin_returning_an_error_status_surfaces_as_an_error() {
        // Not as an empty result that a caller would store as a success.
        let (endpoint, _) = fake_plugin("bad-ai", true).await;
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ai.json",
            &serde_json::json!({
                "endpoint": endpoint,
                "enabled": true,
                "manifest": manifest("bad-ai", &["ai_analyze"], PLUGIN_PROTOCOL_VERSION),
            }),
        );
        let registry = PluginRegistry::load_dir(dir.path()).await.unwrap();
        assert!(registry.health("bad-ai").await.is_err());
    }
}
