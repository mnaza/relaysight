// Commercial/proprietary prototype. See commercial/licenses/COMMERCIAL-LICENSE-NOTICE.md.
use std::{collections::HashSet, env, net::SocketAddr, sync::Arc};

use axum::{
    extract::{Path, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use tower_http::trace::TraceLayer;
use vms_domain::{EditionEntitlement, EditionKind};

#[derive(Clone)]
struct AppState {
    service_token: Arc<str>,
    free_camera_limit: usize,
    paid_customers: Arc<HashSet<String>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let bind: SocketAddr = env::var("CONTROL_PLANE_BIND").unwrap_or_else(|_| "0.0.0.0:8090".into()).parse()?;
    let paid_customers = env::var("PAID_CUSTOMERS").unwrap_or_default().split(',')
        .map(str::trim).filter(|v| !v.is_empty()).map(ToOwned::to_owned).collect();
    let state = AppState {
        service_token: Arc::from(env::var("ENTITLEMENTS_TOKEN").unwrap_or_else(|_| "commercial-demo-token".into())),
        free_camera_limit: env::var("COMMERCIAL_FREE_CAMERA_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(3),
        paid_customers: Arc::new(paid_customers),
    };
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/v1/entitlements/{customer_id}", get(entitlement))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await?;
    Ok(())
}

async fn entitlement(
    State(state): State<AppState>, headers: HeaderMap, Path(customer_id): Path<String>,
) -> Result<Json<EditionEntitlement>, StatusCode> {
    let token = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    if token != Some(state.service_token.as_ref()) { return Err(StatusCode::UNAUTHORIZED); }
    let paid = state.paid_customers.contains(&customer_id);
    Ok(Json(EditionEntitlement {
        edition: EditionKind::Commercial,
        plan: if paid { "commercial-pro".into() } else { "commercial-free".into() },
        self_hosted: false,
        managed: true,
        camera_limit: if paid { None } else { Some(state.free_camera_limit) },
        capabilities: if paid {
            vec!["onvif", "rtsp_health", "plugins", "ai_plugins", "storage_plugins", "managed_cloud", "multi_tenant", "reseller", "advanced_white_label", "billing", "sso", "audit", "ha", "priority_support"]
        } else {
            vec!["onvif", "rtsp_health", "plugins", "ai_plugins", "storage_plugins", "managed_cloud"]
        }.into_iter().map(str::to_string).collect(),
    }))
}
