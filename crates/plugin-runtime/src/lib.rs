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
