//! Signing in with somebody else's identity provider.
//!
//! Every account here has a password this system stores, which is the thing
//! an organisation with an identity provider has spent years getting rid of.
//! A technician who leaves should stop being able to sign in because HR
//! turned their account off, not because somebody remembered to open the
//! fleet dashboard.
//!
//! Authorization code with PKCE. PKCE on a confidential client is belt and
//! braces and costs a hash.
//! See docs/superpowers/specs/2026-09-24-sso-oidc-design.md.

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    extract::{Query, State},
    http::{StatusCode, header::SET_COOKIE},
    response::{IntoResponse, Redirect, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::AppState;

/// How long somebody has to finish signing in before the attempt is
/// forgotten. Long enough to type a password at the provider, short enough
/// that a `state` lying around is not a way in.
const ATTEMPT_TTL: Duration = Duration::from_secs(600);

/// What the operator configured, or nothing at all.
#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    /// `None` means only people who already have an account may sign in.
    pub autocreate_role: Option<vms_domain::Role>,
}

impl OidcConfig {
    /// From the environment, or `None` when no issuer is set.
    pub fn from_env() -> Option<Self> {
        let issuer = std::env::var("OIDC_ISSUER").ok()?;
        let issuer = issuer.trim().trim_end_matches('/').to_owned();
        if issuer.is_empty() {
            return None;
        }
        let config = Self {
            issuer,
            client_id: std::env::var("OIDC_CLIENT_ID").unwrap_or_default(),
            client_secret: std::env::var("OIDC_CLIENT_SECRET").unwrap_or_default(),
            redirect_url: std::env::var("OIDC_REDIRECT_URL").unwrap_or_default(),
            autocreate_role: std::env::var("OIDC_AUTOCREATE_ROLE")
                .ok()
                .and_then(|raw| vms_domain::Role::parse(raw.trim())),
        };
        if config.client_id.is_empty() || config.redirect_url.is_empty() {
            // Half-configured SSO is worse than none: the button appears and
            // whoever presses it gets an error from somebody else's provider.
            warn!("OIDC_ISSUER is set but OIDC_CLIENT_ID or OIDC_REDIRECT_URL is not; SSO is off");
            return None;
        }
        Some(config)
    }
}

/// One sign-in somebody started, remembered until they come back.
#[derive(Clone)]
struct Attempt {
    nonce: String,
    verifier: String,
    started: std::time::Instant,
}

#[derive(Clone, Default)]
pub struct Attempts {
    inner: Arc<RwLock<HashMap<String, Attempt>>>,
}

/// Where the provider's own endpoints are, once asked.
#[derive(Clone, Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    email: Option<String>,
    /// A provider saying it never checked that this address belongs to
    /// whoever just signed in. Absent from plenty of providers, which is not
    /// the same as false.
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    nonce: Option<String>,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Serialize)]
pub struct Methods {
    /// Always true: somebody has to be able to get in when the provider is
    /// down.
    pub password: bool,
    pub sso: bool,
    /// What to put on the button.
    pub sso_label: Option<String>,
}

/// What the login page offers.
pub async fn auth_methods(State(state): State<AppState>) -> axum::Json<Methods> {
    axum::Json(Methods {
        password: true,
        sso: state.oidc.is_some(),
        sso_label: state
            .oidc
            .as_ref()
            .map(|config| host_of(&config.issuer).unwrap_or_else(|| "SSO".into())),
    })
}

/// Send the browser to the provider.
pub async fn oidc_start(State(state): State<AppState>) -> Response {
    let Some(config) = state.oidc.clone() else {
        return (StatusCode::NOT_FOUND, "SSO is not configured").into_response();
    };
    let discovery = match discover(&state, &config).await {
        Ok(discovery) => discovery,
        Err(error) => {
            warn!(%error, "the identity provider could not be reached");
            return (
                StatusCode::BAD_GATEWAY,
                "the identity provider did not answer",
            )
                .into_response();
        }
    };

    let state_token = random_token();
    let nonce = random_token();
    let verifier = random_token();
    let challenge = BASE64URL.encode(Sha256::digest(verifier.as_bytes()));
    state.oidc_attempts.inner.write().await.insert(
        state_token.clone(),
        Attempt {
            nonce: nonce.clone(),
            verifier,
            started: std::time::Instant::now(),
        },
    );

    let url = format!(
        "{}?response_type=code&scope=openid%20email&client_id={}&redirect_uri={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        discovery.authorization_endpoint,
        urlencode(&config.client_id),
        urlencode(&config.redirect_url),
        urlencode(&state_token),
        urlencode(&nonce),
        urlencode(&challenge),
    );
    Redirect::to(&url).into_response()
}

/// The provider sent them back. Everything below is a reason to refuse.
pub async fn oidc_callback(
    State(state): State<AppState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(config) = state.oidc.clone() else {
        return (StatusCode::NOT_FOUND, "SSO is not configured").into_response();
    };
    if let Some(error) = query.error {
        return refuse(
            &state,
            "the provider refused that sign-in",
            Some(&format!("the provider answered {error}")),
        )
        .await;
    }
    let (Some(code), Some(state_token)) = (query.code, query.state) else {
        return refuse(&state, "the provider came back without a code", None).await;
    };

    // A state is good once. Taking it out here is what makes a replay a
    // different request with nothing behind it.
    let attempt = state.oidc_attempts.inner.write().await.remove(&state_token);
    let Some(attempt) = attempt.filter(|attempt| attempt.started.elapsed() < ATTEMPT_TTL) else {
        return refuse(&state, "that sign-in was not one this system started", None).await;
    };

    let discovery = match discover(&state, &config).await {
        Ok(discovery) => discovery,
        Err(error) => {
            return refuse(
                &state,
                "the identity provider did not answer",
                Some(&format!("{error}")),
            )
            .await;
        }
    };
    let claims = match exchange(&state, &config, &discovery, &code, &attempt).await {
        Ok(claims) => claims,
        Err(error) => {
            return refuse(
                &state,
                "the provider would not complete that sign-in",
                Some(&format!("{error}")),
            )
            .await;
        }
    };
    let Some(email) = claims.email.map(|email| email.trim().to_lowercase()) else {
        return refuse(&state, "the provider did not say who that was", None).await;
    };
    if claims.email_verified == Some(false) {
        // The provider is telling us it never checked. Somewhere that lets
        // people register themselves, that is an address somebody typed —
        // and an account here belonging to it is somebody else's.
        return refuse(
            &state,
            "the provider has not verified that address",
            Some(&format!("{email} is unverified at the provider")),
        )
        .await;
    }

    match sign_in(&state, &config, &email).await {
        Ok(cookie) => (
            StatusCode::SEE_OTHER,
            [
                (SET_COOKIE, cookie),
                (axum::http::header::LOCATION, "../../../app.html".to_owned()),
            ],
        )
            .into_response(),
        Err(why) => refuse_text(&why),
    }
}

/// Turn a verified email into a session, if that email may get in at all.
/// The error is what to tell the browser; the reason is already audited here.
async fn sign_in(state: &AppState, config: &OidcConfig, email: &str) -> Result<String, String> {
    let existing = state.store.user_by_email(email).await.ok().flatten();
    let user = match (existing, config.autocreate_role) {
        (Some(user), _) if user.disabled => {
            // The provider's opinion does not outrank this system's.
            crate::audit(state, email, "login.sso.refused", "", Some("disabled")).await;
            return Err("that account is turned off".into());
        }
        (Some(user), _) => user,
        (None, Some(role)) => {
            let id = uuid::Uuid::new_v4().to_string();
            // No password, and none that could ever verify: an SSO account is
            // not a second way in.
            if let Err(error) = state
                .store
                .create_user(&id, email, NO_PASSWORD, role, None, Utc::now())
                .await
            {
                warn!(%error, "could not create an account for an SSO sign-in");
                return Err("that account could not be created".into());
            }
            crate::audit(
                state,
                email,
                "user.created",
                email,
                Some("by single sign-on"),
            )
            .await;
            state
                .store
                .user_by_email(email)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| "that account could not be created".to_owned())?
        }
        (None, None) => {
            // An identity provider answering for anyone@gmail.com is not a
            // reason to create an owner.
            crate::audit(state, email, "login.sso.refused", "", Some("no account")).await;
            return Err("that email has no account here".into());
        }
    };

    let session_id = crate::auth::mint_session_id();
    let now = Utc::now();
    if let Err(error) = state
        .store
        .create_session(
            &session_id,
            &user.id,
            now,
            now + chrono::Duration::days(crate::auth::SESSION_DAYS),
        )
        .await
    {
        warn!(%error, "could not start a session after an SSO sign-in");
        return Err("that session could not be started".into());
    }
    crate::audit(
        state,
        &user.email,
        "login.ok",
        user.role.as_str(),
        Some("sso"),
    )
    .await;
    info!(email = %user.email, "signed in through the identity provider");
    Ok(crate::auth::set_cookie_value(
        &session_id,
        state.cookie_secure,
    ))
}

/// A password hash nothing can match, for an account that has no password.
pub const NO_PASSWORD: &str = "!oidc-only";

async fn discover(state: &AppState, config: &OidcConfig) -> anyhow::Result<Discovery> {
    let url = format!("{}/.well-known/openid-configuration", config.issuer);
    let discovery: Discovery = state
        .http
        .get(&url)
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // A provider that answers for a different issuer than the one configured
    // is either misconfigured or not the provider.
    if discovery.issuer.trim_end_matches('/') != config.issuer {
        anyhow::bail!(
            "that provider calls itself {} rather than {}",
            discovery.issuer,
            config.issuer
        );
    }
    Ok(discovery)
}

/// The code for a token, and the token checked to pieces.
async fn exchange(
    state: &AppState,
    config: &OidcConfig,
    discovery: &Discovery,
    code: &str,
    attempt: &Attempt,
) -> anyhow::Result<IdClaims> {
    let tokens: TokenResponse = state
        .http
        .post(&discovery.token_endpoint)
        .timeout(Duration::from_secs(8))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &config.redirect_url),
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("code_verifier", &attempt.verifier),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let header = jsonwebtoken::decode_header(&tokens.id_token)?;
    let key = jwk_for(state, &discovery.jwks_uri, header.kid.as_deref()).await?;
    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.set_audience(&[config.client_id.as_str()]);
    validation.set_issuer(&[config.issuer.as_str()]);
    // Expiry is checked by default; saying so is cheaper than somebody later
    // wondering whether it was.
    validation.validate_exp = true;
    let token = jsonwebtoken::decode::<IdClaims>(&tokens.id_token, &key, &validation)?;

    if token.claims.nonce.as_deref() != Some(attempt.nonce.as_str()) {
        anyhow::bail!("that token was minted for a different sign-in");
    }
    Ok(token.claims)
}

/// The provider's signing key, by key id.
async fn jwk_for(
    state: &AppState,
    jwks_uri: &str,
    kid: Option<&str>,
) -> anyhow::Result<jsonwebtoken::DecodingKey> {
    let jwks: serde_json::Value = state
        .http
        .get(jwks_uri)
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let keys = jwks["keys"].as_array().cloned().unwrap_or_default();
    let key = keys
        .iter()
        .find(|key| match kid {
            Some(kid) => key["kid"].as_str() == Some(kid),
            // A provider with one key need not name it.
            None => true,
        })
        .ok_or_else(|| anyhow::anyhow!("the provider has no key for that token"))?;
    let (Some(n), Some(e)) = (key["n"].as_str(), key["e"].as_str()) else {
        anyhow::bail!("that signing key is not an RSA key this build can read");
    };
    Ok(jsonwebtoken::DecodingKey::from_rsa_components(n, e)?)
}

/// Refuse, saying as much to the browser as is its business and as much to
/// the log and the audit as an owner needs. What went wrong at the provider
/// is a matter for whoever runs this, not for whoever pressed the button:
/// nothing good comes of answering a stranger's browser with the provider's
/// own address.
async fn refuse(state: &AppState, why: &str, detail: Option<&str>) -> Response {
    let recorded = detail.unwrap_or(why);
    warn!(reason = recorded, "an SSO sign-in was refused");
    crate::audit(state, "sso", "login.sso.refused", "", Some(recorded)).await;
    refuse_text(why)
}

fn refuse_text(why: &str) -> Response {
    (StatusCode::UNAUTHORIZED, format!("Sign-in refused: {why}")).into_response()
}

/// 32 bytes of randomness, as text a URL can carry. The same shape as a
/// session id, and for the same reason: guessing one must be hopeless.
fn random_token() -> String {
    crate::auth::mint_session_id()
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn host_of(issuer: &str) -> Option<String> {
    let after_scheme = issuer.split("://").nth(1).unwrap_or(issuer);
    let host = after_scheme.split('/').next()?;
    (!host.is_empty()).then(|| host.to_owned())
}

/// Forget sign-ins nobody came back from. Called from the retention pass.
pub async fn forget_stale_attempts(attempts: &Attempts) {
    attempts
        .inner
        .write()
        .await
        .retain(|_, attempt| attempt.started.elapsed() < ATTEMPT_TTL);
}

#[cfg(test)]
pub(crate) async fn attempts_open(attempts: &Attempts) -> usize {
    attempts.inner.read().await.len()
}
