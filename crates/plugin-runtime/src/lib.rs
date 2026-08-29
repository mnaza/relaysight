use std::{collections::BTreeMap, env, path::Path, sync::Arc};

use anyhow::{Context, anyhow};
use reqwest::Client;
use tokio::sync::RwLock;
use tracing::warn;
use vms_plugin_sdk::{
    AiAnalyzeRequest, AiAnalyzeResponse, PLUGIN_PROTOCOL_VERSION, PluginCapability, PluginHealth,
    PluginManifest, PluginRegistration, RegisteredPlugin, SignedTransfer, StorageDeleteRequest,
    StorageDeleteResponse, StorageDownloadRequest, StorageUploadRequest,
};

#[derive(Clone)]
pub struct PluginRegistry {
    client: Client,
    plugins: Arc<RwLock<BTreeMap<String, PluginEntry>>>,
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
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()?;
        let registry = Self {
            client,
            plugins: Arc::new(RwLock::new(BTreeMap::new())),
        };
        registry.reload(path).await?;
        Ok(registry)
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
            let raw = std::fs::read_to_string(&file)?;
            let registration: PluginRegistration =
                serde_json::from_str(&raw).with_context(|| format!("parse {}", file.display()))?;
            if !registration.enabled {
                continue;
            }

            let fallback = registration.manifest.clone();
            let fetched = self.fetch_manifest(&registration).await;
            let (manifest, reachable, last_error) = match fetched {
                Ok(manifest) => (manifest, true, None),
                Err(error) => {
                    let Some(manifest) = fallback else {
                        warn!(file = %file.display(), %error, "plugin unavailable and no embedded manifest");
                        continue;
                    };
                    (manifest, false, Some(error.to_string()))
                }
            };
            if manifest.protocol_version != PLUGIN_PROTOCOL_VERSION {
                warn!(plugin = %manifest.id, protocol = manifest.protocol_version, "unsupported plugin protocol");
                continue;
            }
            loaded.insert(
                manifest.id.clone(),
                PluginEntry {
                    registration,
                    manifest,
                    reachable,
                    last_error,
                },
            );
        }
        *self.plugins.write().await = loaded;
        Ok(())
    }

    pub async fn list(&self) -> Vec<RegisteredPlugin> {
        self.plugins
            .read()
            .await
            .values()
            .map(|entry| RegisteredPlugin {
                endpoint: entry.registration.endpoint.clone(),
                placement: entry.registration.placement.clone(),
                enabled: entry.registration.enabled,
                reachable: entry.reachable,
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
        self.request(&entry, reqwest::Method::POST, "/v1/ai/analyze", Some(body))
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
        self.request(
            &entry,
            reqwest::Method::POST,
            "/v1/storage/uploads",
            Some(body),
        )
        .await
    }

    pub async fn storage_download(
        &self,
        id: &str,
        body: &StorageDownloadRequest,
    ) -> anyhow::Result<SignedTransfer> {
        let entry = self
            .entry_with_capability(id, PluginCapability::StorageBlob)
            .await?;
        self.request(
            &entry,
            reqwest::Method::POST,
            "/v1/storage/downloads",
            Some(body),
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
        self.request(
            &entry,
            reqwest::Method::POST,
            "/v1/storage/delete",
            Some(body),
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
        )
        .await
    }

    async fn request<T: serde::de::DeserializeOwned, B: serde::Serialize + ?Sized>(
        &self,
        entry: &PluginEntry,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> anyhow::Result<T> {
        self.request_registration(&entry.registration, method, path, body)
            .await
    }

    async fn request_registration<T: serde::de::DeserializeOwned, B: serde::Serialize + ?Sized>(
        &self,
        registration: &PluginRegistration,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> anyhow::Result<T> {
        let url = format!("{}{}", registration.endpoint.trim_end_matches('/'), path);
        let mut request = self.client.request(method, url);
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
    async fn one_malformed_manifest_takes_down_the_whole_reload() {
        // Pinning current behaviour, which is worth knowing rather than
        // discovering: a single unparseable file fails the entire load, so a
        // typo in one manifest removes every plugin, not just its own.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "good.json", &offline("ai-1", &["ai_analyze"]));
        std::fs::write(dir.path().join("bad.json"), "{ not json").unwrap();
        assert!(PluginRegistry::load_dir(dir.path()).await.is_err());
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
