use std::{env, sync::Arc};

use vms_domain::{EditionEntitlement, EditionKind};

#[derive(Clone)]
pub struct EntitlementResolver {
    client: reqwest::Client,
    endpoint: Option<Arc<str>>,
    service_token: Option<Arc<str>>,
}

impl EntitlementResolver {
    pub fn from_env() -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: env::var("ENTITLEMENTS_URL")
                .ok()
                .filter(|v| !v.is_empty())
                .map(Arc::from),
            service_token: env::var("ENTITLEMENTS_TOKEN")
                .ok()
                .filter(|v| !v.is_empty())
                .map(Arc::from),
        }
    }

    pub async fn resolve(&self, customer_id: &str) -> anyhow::Result<EditionEntitlement> {
        let Some(endpoint) = &self.endpoint else {
            return Ok(community_entitlement());
        };
        let url = format!(
            "{}/api/v1/entitlements/{}",
            endpoint.trim_end_matches('/'),
            customer_id
        );
        let mut request = self.client.get(url);
        if let Some(token) = &self.service_token {
            request = request.bearer_auth(token.as_ref());
        }
        let response = request.send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}

fn community_entitlement() -> EditionEntitlement {
    EditionEntitlement {
        edition: EditionKind::Community,
        plan: "community-self-hosted".into(),
        self_hosted: true,
        managed: false,
        camera_limit: None,
        capabilities: vec![
            "onvif".into(),
            "rtsp_health".into(),
            "plugins".into(),
            "ai_plugins".into(),
            "storage_plugins".into(),
            "local_users".into(),
            "self_hosted".into(),
        ],
    }
}
