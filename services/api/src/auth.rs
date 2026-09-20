//! The authentication boundary: password hashing, sessions, and the handlers
//! and middleware that speak them. Argon2 and cookie syntax live here and
//! nowhere else. See docs/superpowers/specs/2026-09-07-dashboard-auth-design.md.

use std::time::Duration;

use crate::AppState;
use crate::store::{Store, StoreError};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::{
    Json,
    extract::{Request, State},
    http::{
        HeaderMap, StatusCode,
        header::{COOKIE, SET_COOKIE},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use tracing::{info, warn};

/// Argon2id with the crate defaults, as a PHC string with a fresh random salt.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|err| anyhow::anyhow!("argon2: {err}"))?
        .to_string())
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), phc)
        .is_ok()
}

/// 64 hex chars of OS entropy. Two UUIDv4s back to back — the same source
/// that mints gateway and enrollment tokens, without a new RNG dependency.
pub fn mint_session_id() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 250ms after the first failure, doubling to an 8s cap. Friction for online
/// guessing, not a lockout — argon2 already makes each attempt expensive.
pub fn failure_delay(consecutive_failures: u32) -> Duration {
    Duration::from_millis(250u64 << consecutive_failures.min(5))
}

#[derive(Default)]
pub struct LoginThrottle {
    consecutive: u32,
}

impl LoginThrottle {
    pub fn register_failure(&mut self) -> Duration {
        let delay = failure_delay(self.consecutive);
        self.consecutive = self.consecutive.saturating_add(1);
        delay
    }

    pub fn reset(&mut self) {
        self.consecutive = 0;
    }

    /// How many failures the current run holds. The failed-login audit row
    /// carries it, so a burst is visible in the log and not only in the timing.
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

/// Startup credential bootstrap. An API that cannot authenticate anyone must
/// not serve, so the no-credential case is an error, same as a failed DB open.
pub async fn seed_admin_credential(
    store: &dyn Store,
    env_password: Option<&str>,
    force_reset: bool,
) -> anyhow::Result<()> {
    let stored = store
        .admin_password_hash()
        .await
        .map_err(|err| anyhow::anyhow!("reading admin credential: {err}"))?;
    match (stored, env_password, force_reset) {
        (Some(_), _, false) => Ok(()),
        (Some(_), Some(password), true) => {
            ensure_seedable(password)?;
            let phc = hash_password(password)?;
            store
                .set_admin_password_hash(&phc, chrono::Utc::now())
                .await
                .map_err(seed_err)?;
            store.delete_all_sessions().await.map_err(seed_err)?;
            if let Err(err) = store
                .record_audit(chrono::Utc::now(), "system", "password.reset", "", None)
                .await
            {
                warn!(error = %err, "audit write failed for password.reset");
            }
            warn!("ADMIN_PASSWORD_RESET: admin password re-seeded from env, all sessions revoked");
            Ok(())
        }
        (Some(_), None, true) => anyhow::bail!(
            "ADMIN_PASSWORD_RESET=true but ADMIN_PASSWORD is not set; nothing to reset to"
        ),
        (None, Some(password), _) => {
            ensure_seedable(password)?;
            let phc = hash_password(password)?;
            store
                .set_admin_password_hash(&phc, chrono::Utc::now())
                .await
                .map_err(seed_err)?;
            info!("admin credential seeded from ADMIN_PASSWORD");
            Ok(())
        }
        (None, None, _) => anyhow::bail!(
            "no admin credential in the store and no ADMIN_PASSWORD in the environment; \
             refusing to serve an unauthenticatable API"
        ),
    }
}

fn seed_err(err: StoreError) -> anyhow::Error {
    anyhow::anyhow!("seeding admin credential: {err}")
}

/// The change endpoint enforces the minimum; the env seed is not a back door
/// around it.
fn ensure_seedable(password: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        password.len() >= MIN_PASSWORD_LEN,
        "ADMIN_PASSWORD is shorter than {MIN_PASSWORD_LEN} characters; \
         refusing to seed a weak admin credential"
    );
    Ok(())
}

pub const SESSION_COOKIE: &str = "vms_session";
const SESSION_DAYS: i64 = 7;

/// The session id out of the Cookie header, if any.
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get(COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|pair| pair.strip_prefix(SESSION_COOKIE)?.strip_prefix('='))
        .map(str::to_owned)
}

pub fn set_cookie_value(session_id: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={session_id}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{secure}",
        SESSION_DAYS * 24 * 60 * 60
    )
}

pub fn clear_cookie_value(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{secure}")
}

async fn session_alive(state: &AppState, headers: &HeaderMap) -> bool {
    match session_cookie(headers) {
        // A store failure answers 401, not 500: fail closed on the auth boundary.
        Some(id) => state
            .store
            .session_is_valid(&id, Utc::now())
            .await
            .unwrap_or(false),
        None => false,
    }
}

/// The one layer in front of every human-facing route.
pub async fn require_session(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if session_alive(&state, request.headers()).await {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Count one wrong password and say how long the run is, for the audit row.
/// Both credential checks share the throttle, so the wording is about
/// passwords rather than about logins.
async fn count_wrong_password(state: &AppState) -> (Duration, String) {
    let mut throttle = state.login_throttle.lock().await;
    let delay = throttle.register_failure();
    let count = throttle.consecutive();
    (
        delay,
        format!(
            "{count} wrong password{} in a row",
            if count == 1 { "" } else { "s" }
        ),
    )
}

#[derive(serde::Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

pub async fn auth_login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Response {
    let stored = match state.store.admin_password_hash().await {
        Ok(Some(phc)) => phc,
        Ok(None) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Err(err) => return crate::store_status(err).into_response(),
    };
    if !verify_password(&request.password, &stored) {
        let (delay, detail) = count_wrong_password(&state).await;
        crate::audit(&state, "admin", "login.failed", "", Some(&detail)).await;
        tokio::time::sleep(delay).await;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.login_throttle.lock().await.reset();
    let session_id = mint_session_id();
    let now = Utc::now();
    if let Err(err) = state
        .store
        .create_session(&session_id, now, now + chrono::Duration::days(SESSION_DAYS))
        .await
    {
        return crate::store_status(err).into_response();
    }
    crate::audit(&state, "admin", "login.ok", "", None).await;
    (
        StatusCode::NO_CONTENT,
        [(
            SET_COOKIE,
            set_cookie_value(&session_id, state.cookie_secure),
        )],
    )
        .into_response()
}

/// 204 or 401. The SPA asks this before painting anything.
pub async fn auth_session(State(state): State<AppState>, headers: HeaderMap) -> StatusCode {
    if session_alive(&state, &headers).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::UNAUTHORIZED
    }
}

pub async fn auth_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(session_id) = session_cookie(&headers) {
        // Best effort: the cookie is cleared either way.
        let _ = state.store.delete_session(&session_id).await;
    }
    (
        StatusCode::NO_CONTENT,
        [(SET_COOKIE, clear_cookie_value(state.cookie_secure))],
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub struct ChangePasswordRequest {
    pub current: String,
    #[serde(rename = "new")]
    pub new_password: String,
}

const MIN_PASSWORD_LEN: usize = 12;

pub async fn auth_change_password(
    State(state): State<AppState>,
    Json(request): Json<ChangePasswordRequest>,
) -> Response {
    let stored = match state.store.admin_password_hash().await {
        Ok(Some(phc)) => phc,
        Ok(None) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Err(err) => return crate::store_status(err).into_response(),
    };
    if !verify_password(&request.current, &stored) {
        // A stolen session must not be an unthrottled oracle for the real
        // password: wrong `current` costs the same growing delay a failed
        // login does, and leaves a row of its own so the count a failed login
        // reports is not covering attempts nothing recorded.
        let (delay, detail) = count_wrong_password(&state).await;
        crate::audit(&state, "admin", "password.change.failed", "", Some(&detail)).await;
        tokio::time::sleep(delay).await;
        return StatusCode::FORBIDDEN.into_response();
    }
    state.login_throttle.lock().await.reset();
    if request.new_password.len() < MIN_PASSWORD_LEN {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let phc = match hash_password(&request.new_password) {
        Ok(phc) => phc,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let now = Utc::now();
    // Store the new hash, revoke everything, then re-mint for the caller so
    // the dashboard they are sitting in does not log itself out.
    if let Err(err) = state.store.set_admin_password_hash(&phc, now).await {
        return crate::store_status(err).into_response();
    }
    if let Err(err) = state.store.delete_all_sessions().await {
        return crate::store_status(err).into_response();
    }
    let session_id = mint_session_id();
    if let Err(err) = state
        .store
        .create_session(&session_id, now, now + chrono::Duration::days(SESSION_DAYS))
        .await
    {
        return crate::store_status(err).into_response();
    }
    crate::audit(&state, "admin", "password.changed", "", None).await;
    (
        StatusCode::NO_CONTENT,
        [(
            SET_COOKIE,
            set_cookie_value(&session_id, state.cookie_secure),
        )],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    async fn store() -> crate::store::SqliteStore {
        crate::store::SqliteStore::in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn seeding_refuses_to_run_without_any_credential() {
        let store = store().await;
        assert!(
            seed_admin_credential(&store, None, false).await.is_err(),
            "no stored hash and no ADMIN_PASSWORD must refuse to start"
        );
    }

    #[tokio::test]
    async fn first_boot_seeds_from_env_and_later_boots_ignore_env() {
        let store = store().await;
        seed_admin_credential(&store, Some("first boot password"), false)
            .await
            .unwrap();
        let seeded = store.admin_password_hash().await.unwrap().unwrap();
        assert!(verify_password("first boot password", &seeded));

        // A changed env var without the reset flag must not overwrite.
        seed_admin_credential(&store, Some("attacker sets a new env"), false)
            .await
            .unwrap();
        let unchanged = store.admin_password_hash().await.unwrap().unwrap();
        assert_eq!(seeded, unchanged);

        // And a boot with no env at all is fine once a hash is stored.
        seed_admin_credential(&store, None, false).await.unwrap();
    }

    #[tokio::test]
    async fn forced_reset_reseeds_and_wipes_sessions() {
        let store = store().await;
        let now = chrono::Utc::now();
        seed_admin_credential(&store, Some("original password!"), false)
            .await
            .unwrap();
        store
            .create_session("old-session", now, now + chrono::Duration::days(7))
            .await
            .unwrap();

        seed_admin_credential(&store, Some("replacement password"), true)
            .await
            .unwrap();
        let reseeded = store.admin_password_hash().await.unwrap().unwrap();
        assert!(verify_password("replacement password", &reseeded));
        assert!(
            !store.session_is_valid("old-session", now).await.unwrap(),
            "a forced reset must revoke every session"
        );

        // Reset without a password to reset to is a refusal, not a wipe.
        assert!(seed_admin_credential(&store, None, true).await.is_err());
    }

    #[tokio::test]
    async fn a_short_admin_password_refuses_to_seed_or_reset() {
        // The change endpoint enforces a 12-character minimum; the env seed
        // must not be a back door around it.
        let store = store().await;
        assert!(
            seed_admin_credential(&store, Some("short"), false)
                .await
                .is_err(),
            "a first boot must not seed a weak credential"
        );

        seed_admin_credential(&store, Some("a long enough password"), false)
            .await
            .unwrap();
        let before = store.admin_password_hash().await.unwrap().unwrap();
        let now = chrono::Utc::now();
        store
            .create_session("live", now, now + chrono::Duration::days(7))
            .await
            .unwrap();

        assert!(
            seed_admin_credential(&store, Some("short"), true)
                .await
                .is_err(),
            "a forced reset must not accept a weak credential either"
        );
        assert_eq!(
            store.admin_password_hash().await.unwrap().unwrap(),
            before,
            "a refused reset must leave the credential untouched"
        );
        assert!(
            store.session_is_valid("live", now).await.unwrap(),
            "a refused reset must not wipe sessions"
        );

        // A short env value on a normal boot with a stored credential is
        // ignored, not fatal — env is not consulted at all on that path.
        seed_admin_credential(&store, Some("short"), false)
            .await
            .unwrap();
    }

    #[test]
    fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let phc = hash_password("correct horse battery staple").unwrap();
        assert!(
            phc.starts_with("$argon2id$"),
            "not a PHC argon2id string: {phc}"
        );
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong password entirely", &phc));
        assert!(!verify_password(
            "correct horse battery staple",
            "not-a-phc-string"
        ));
    }

    #[test]
    fn session_ids_are_long_hex_and_unique() {
        let a = mint_session_id();
        let b = mint_session_id();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn the_login_delay_grows_and_caps() {
        let mut throttle = LoginThrottle::default();
        let first = throttle.register_failure();
        let second = throttle.register_failure();
        assert!(
            second > first,
            "the delay must grow with consecutive failures"
        );
        for _ in 0..20 {
            throttle.register_failure();
        }
        assert_eq!(
            throttle.register_failure(),
            failure_delay(5),
            "the delay must cap instead of growing forever"
        );
        throttle.reset();
        assert_eq!(throttle.consecutive(), 0);
        assert_eq!(throttle.register_failure(), failure_delay(0));
    }
}
