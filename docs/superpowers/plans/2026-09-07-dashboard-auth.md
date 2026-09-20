# Dashboard Auth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put a real login — single admin account, argon2id-hashed password, cookie-carried server-side sessions in the SQLite store — in front of every human-facing API route and the dashboard.

**Architecture:** One new module `services/api/src/auth.rs` holds password hashing, session-id minting, the login throttle, credential bootstrap and the auth handlers/middleware. The `Store` trait grows credential and session methods; SQL stays in `store/sqlite.rs`. `build_router` splits into an open group (health, edition, login, session check, gateway machine endpoints) and a protected group wrapped in one `from_fn_with_state` middleware layer. The SPA gets a login overlay, a session gate before `startDashboard`, and an account modal for logout/change-password.

**Tech Stack:** Rust (axum 0.8, sqlx 0.9 runtime queries, async-trait, chrono, uuid), argon2 0.6 (new dep), vanilla-JS SPA tested with `node --test` + jsdom.

**Spec:** `docs/superpowers/specs/2026-09-07-dashboard-auth-design.md`

## Global Constraints

- No production code without a failing test first; run the test and watch it fail before implementing.
- Secrets never at rest in plaintext: session ids stored only as `token_hash()` (SHA-256 hex, 64 chars); the admin password only as an argon2id PHC string.
- Timestamps in SQLite always via `ts()` / `parse_ts()` from `store/mod.rs` (fixed-width RFC 3339, `Z` suffix).
- sqlx **runtime** queries (`sqlx::query(...)`), never compile-time `query!` macros; migrations via `sqlx::migrate!("./migrations")` which picks up new files automatically.
- The workspace `Cargo.toml` carries a local uncommitted `[patch.crates-io]` retina patch. **Never `git add .` and never `git add Cargo.toml` at the repo root.** Stage files by explicit path. `Cargo.lock` may be committed after checking `git diff Cargo.lock | grep -i retina` is empty.
- Rust tests: `cargo test -p vms-api` from the repo root. Web tests: `npm test` in `web/` (runs `node --test "tests/*.test.mjs"`).
- Cookie name is exactly `vms_session`; attributes `HttpOnly; SameSite=Lax; Path=/; Max-Age=604800`, plus `; Secure` only when `AppState.cookie_secure` is true.
- Minimum new-password length is 12 bytes; there are no composition rules.
- **Deviation from the spec, deliberate:** session ids are minted as two concatenated `Uuid::new_v4().simple()` strings (64 hex chars ≈ 244 bits of OS entropy) instead of adding a `rand` dependency. uuid already mints every other token in this codebase and draws from the OS RNG. The spec's "Dependencies" line shrinks to argon2 only.
- **Deviation from the spec, deliberate:** `POST /api/v1/plugins/{id}/ai/analyze` and `POST /api/v1/plugins/{id}/storage/uploads` stay in the **open** group. The edge gateway calls both without any Authorization header (`edge/gateway/src/main.rs:848`, `:934`), so cookie-gating them would break recording and analysis. They are open today, so this is no regression; a beads follow-up task (created in Task 9) covers giving them gateway bearer auth.

## File Structure

- `services/api/migrations/0002_auth.sql` — create: `admin_credential` and `sessions` tables.
- `services/api/src/store/mod.rs` — modify: 7 new `Store` trait methods.
- `services/api/src/store/sqlite.rs` — modify: implement them; store tests.
- `services/api/src/auth.rs` — create: hashing, session ids, throttle, seeding, cookie helpers, middleware, the four auth handlers, unit tests.
- `services/api/src/main.rs` — modify: `mod auth;`, `AppState` fields, router split, bootstrap call, retention hook, router tests.
- `services/api/Cargo.toml` — modify: add argon2.
- `web/auth.js` — create: session gate, login form wiring, 401 trap, logout, change-password.
- `web/app.html` — modify: login overlay, account modal.
- `web/dashboard.js` — modify: gate on session before `startDashboard`.
- `web/dashboard-app.js` — modify: wire the account modal open/close.
- `web/styles.css` — modify: opaque login backdrop.
- `web/locales/{en,es,ru}.json` — modify: `app.auth.*` strings.
- `web/tests/auth.test.mjs` — create: jsdom tests for the login gate and account modal.
- `docker-compose.yml`, `docs/RUNNING-LOCALLY.md` — modify: `ADMIN_PASSWORD` wiring and docs.

---

### Task 1: Auth tables and store methods

**Files:**
- Create: `services/api/migrations/0002_auth.sql`
- Modify: `services/api/src/store/mod.rs` (append trait methods)
- Modify: `services/api/src/store/sqlite.rs` (impl + tests)

**Interfaces:**
- Consumes: existing `token_hash()`, `ts()`, `StoreError`, `SqliteStore::in_memory()/connect()`.
- Produces (later tasks call these exact signatures on `dyn Store`):
  - `admin_password_hash() -> Result<Option<String>, StoreError>`
  - `set_admin_password_hash(phc: &str, now: DateTime<Utc>) -> Result<(), StoreError>`
  - `create_session(session_id: &str, now: DateTime<Utc>, expires_at: DateTime<Utc>) -> Result<(), StoreError>`
  - `session_is_valid(session_id: &str, now: DateTime<Utc>) -> Result<bool, StoreError>`
  - `delete_session(session_id: &str) -> Result<(), StoreError>`
  - `delete_all_sessions() -> Result<(), StoreError>`
  - `delete_expired_sessions(now: DateTime<Utc>) -> Result<(), StoreError>`

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `services/api/src/store/sqlite.rs`:

```rust
    #[tokio::test]
    async fn the_admin_credential_is_a_single_replaceable_row() {
        let store = SqliteStore::in_memory().await.unwrap();
        assert_eq!(store.admin_password_hash().await.unwrap(), None);
        store.set_admin_password_hash("$argon2id$fake-one", Utc::now()).await.unwrap();
        store.set_admin_password_hash("$argon2id$fake-two", Utc::now()).await.unwrap();
        assert_eq!(
            store.admin_password_hash().await.unwrap().as_deref(),
            Some("$argon2id$fake-two")
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_credential")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "the second seed must replace, not accumulate");
    }

    #[tokio::test]
    async fn sessions_validate_until_expiry_and_ids_are_not_stored_in_plaintext() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store.create_session("session-secret", now, now + Duration::days(7)).await.unwrap();
        assert!(store.session_is_valid("session-secret", now).await.unwrap());
        assert!(!store.session_is_valid("session-secret", now + Duration::days(8)).await.unwrap());
        assert!(!store.session_is_valid("never-issued", now).await.unwrap());
        let stored: String = sqlx::query_scalar("SELECT id_hash FROM sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(stored, token_hash("session-secret"));
        assert_ne!(stored, "session-secret");
    }

    #[tokio::test]
    async fn deleting_sessions_one_all_and_expired() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store.create_session("s1", now, now + Duration::days(7)).await.unwrap();
        store.create_session("s2", now, now + Duration::days(7)).await.unwrap();
        store.create_session("s3", now, now - Duration::seconds(1)).await.unwrap();

        store.delete_session("s1").await.unwrap();
        assert!(!store.session_is_valid("s1", now).await.unwrap());
        // Logging out twice must not error.
        store.delete_session("s1").await.unwrap();

        store.delete_expired_sessions(now).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(left, 1, "only the live s2 row should remain");

        store.delete_all_sessions().await.unwrap();
        assert!(!store.session_is_valid("s2", now).await.unwrap());
    }
```

Also extend the `connect_applies_migrations` test's expected-tables list with `"admin_credential", "sessions"`.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api admin_credential` — expected: compile error, `admin_password_hash` not found in trait.

- [ ] **Step 3: Write the migration** — `services/api/migrations/0002_auth.sql`:

```sql
-- Auth: the single admin credential and its sessions.
-- See docs/superpowers/specs/2026-09-07-dashboard-auth-design.md.

CREATE TABLE admin_credential (
    id            INTEGER PRIMARY KEY CHECK (id = 1),  -- single row by construction
    password_hash TEXT NOT NULL,                       -- argon2id PHC string
    updated_at    TEXT NOT NULL
);

CREATE TABLE sessions (
    id_hash    TEXT PRIMARY KEY,  -- SHA-256 hex of the session id; the id itself never lands here
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_expiry ON sessions(expires_at);
```

- [ ] **Step 4: Extend the trait** — append to `trait Store` in `services/api/src/store/mod.rs` (before the closing brace):

```rust
    /// The admin credential as an argon2id PHC string, if one has been seeded.
    async fn admin_password_hash(&self) -> Result<Option<String>, StoreError>;

    /// Insert or replace the single credential row.
    async fn set_admin_password_hash(
        &self,
        phc: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn create_session(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// True only for a stored, unexpired session.
    async fn session_is_valid(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError>;

    /// Deleting an absent session is Ok — logging out twice is not an error.
    async fn delete_session(&self, session_id: &str) -> Result<(), StoreError>;

    /// Password change and forced reset revoke everything at once.
    async fn delete_all_sessions(&self) -> Result<(), StoreError>;

    /// Called from the retention loop.
    async fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), StoreError>;
```

- [ ] **Step 5: Implement in `SqliteStore`** — append inside `impl Store for SqliteStore`:

```rust
    async fn admin_password_hash(&self) -> Result<Option<String>, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT password_hash FROM admin_credential WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn set_admin_password_hash(
        &self,
        phc: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO admin_credential (id, password_hash, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
                 password_hash = excluded.password_hash,
                 updated_at = excluded.updated_at",
        )
        .bind(phc)
        .bind(ts(&now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn create_session(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO sessions (id_hash, created_at, expires_at) VALUES (?1, ?2, ?3)")
            .bind(token_hash(session_id))
            .bind(ts(&now))
            .bind(ts(&expires_at))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn session_is_valid(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let row: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM sessions WHERE id_hash = ?1 AND expires_at > ?2",
        )
        .bind(token_hash(session_id))
        .bind(ts(&now))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE id_hash = ?1")
            .bind(token_hash(session_id))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_all_sessions(&self) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions").execute(&self.pool).await?;
        Ok(())
    }

    async fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM sessions WHERE expires_at <= ?1")
            .bind(ts(&now))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

- [ ] **Step 6: Run the tests and make sure they pass**

Run: `cargo test -p vms-api store::` — expected: all store tests pass, including the three new ones.

- [ ] **Step 7: Commit**

```bash
git add services/api/migrations/0002_auth.sql services/api/src/store/mod.rs services/api/src/store/sqlite.rs
git commit -m "Admin credential and sessions live in the store"
```

---

### Task 2: Password hashing, session ids, login throttle

**Files:**
- Modify: `services/api/Cargo.toml` (add argon2)
- Create: `services/api/src/auth.rs`
- Modify: `services/api/src/main.rs:1-2` (add `mod auth;` next to `mod store;`)

**Interfaces:**
- Produces:
  - `auth::hash_password(password: &str) -> anyhow::Result<String>` (argon2id PHC string)
  - `auth::verify_password(password: &str, phc: &str) -> bool`
  - `auth::mint_session_id() -> String` (64 lowercase hex chars)
  - `auth::failure_delay(consecutive_failures: u32) -> std::time::Duration`
  - `auth::LoginThrottle` with `register_failure(&mut self) -> Duration`, `reset(&mut self)`, `consecutive(&self) -> u32`, `Default`

- [ ] **Step 1: Add the dependency** — in `services/api/Cargo.toml` under `[dependencies]` after `sha2 = "0.10"`:

```toml
argon2 = { version = "0.6", features = ["getrandom"] }
```

(`getrandom` lets `hash_password` generate its own salt.) Verify the lockfile stays clean of the retina patch: `cargo check -p vms-api && git diff Cargo.lock | grep -i retina` must print nothing.

- [ ] **Step 2: Write the failing tests** — create `services/api/src/auth.rs` containing only a tests module for now, and add `mod auth;` after `mod store;` in `main.rs`:

```rust
//! The authentication boundary: password hashing, sessions, and the handlers
//! and middleware that speak them. Argon2 and cookie syntax live here and
//! nowhere else. See docs/superpowers/specs/2026-09-07-dashboard-auth-design.md.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let phc = hash_password("correct horse battery staple").unwrap();
        assert!(phc.starts_with("$argon2id$"), "not a PHC argon2id string: {phc}");
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong password entirely", &phc));
        assert!(!verify_password("correct horse battery staple", "not-a-phc-string"));
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
        assert!(second > first, "the delay must grow with consecutive failures");
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
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test -p vms-api auth::` — expected: compile error, `hash_password` not found.

- [ ] **Step 4: Implement** — above the tests module in `auth.rs`:

```rust
use std::time::Duration;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;

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

    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}
```

API note for the implementer: this targets argon2 0.6, where `hash_password(&[u8])` generates its own salt (needs the `getrandom` feature) and `verify_password(&[u8], &str)` takes the PHC string directly. If the shipped 0.6 signatures differ (e.g. verification wants a parsed `PasswordHash`), adapt this one file — parse with `argon2::password_hash::phc::PasswordHash::new(phc)` — the tests pin behavior, not signatures.

- [ ] **Step 5: Run the tests and make sure they pass**

Run: `cargo test -p vms-api auth::` — expected: 3 passed. Also `cargo test -p vms-api` — nothing else broke (expect `dead_code` warnings for the not-yet-used functions; they disappear by Task 4 — do not silence them).

- [ ] **Step 6: Commit**

```bash
git add services/api/Cargo.toml services/api/src/auth.rs services/api/src/main.rs Cargo.lock
git commit -m "Password hashing, session ids and a login throttle"
```

---

### Task 3: Credential bootstrap and AppState plumbing

**Files:**
- Modify: `services/api/src/auth.rs` (seeding + tests)
- Modify: `services/api/src/main.rs` (`AppState` fields, `main()` wiring, test helpers)

**Interfaces:**
- Consumes: Task 1 store methods, Task 2 `hash_password`.
- Produces:
  - `auth::seed_admin_credential(store: &dyn Store, env_password: Option<&str>, force_reset: bool) -> anyhow::Result<()>`
  - `AppState.login_throttle: Arc<tokio::sync::Mutex<crate::auth::LoginThrottle>>`
  - `AppState.cookie_secure: bool`
  - test helpers: `ADMIN_PASSWORD` const, `seed_admin(&AppState)`

- [ ] **Step 1: Write the failing tests** — in `auth.rs` tests module:

```rust
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
        seed_admin_credential(&store, Some("first boot password"), false).await.unwrap();
        let seeded = store.admin_password_hash().await.unwrap().unwrap();
        assert!(verify_password("first boot password", &seeded));

        // A changed env var without the reset flag must not overwrite.
        seed_admin_credential(&store, Some("attacker sets a new env"), false).await.unwrap();
        let unchanged = store.admin_password_hash().await.unwrap().unwrap();
        assert_eq!(seeded, unchanged);

        // And a boot with no env at all is fine once a hash is stored.
        seed_admin_credential(&store, None, false).await.unwrap();
    }

    #[tokio::test]
    async fn forced_reset_reseeds_and_wipes_sessions() {
        let store = store().await;
        let now = chrono::Utc::now();
        seed_admin_credential(&store, Some("original password!"), false).await.unwrap();
        store.create_session("old-session", now, now + chrono::Duration::days(7)).await.unwrap();

        seed_admin_credential(&store, Some("replacement password"), true).await.unwrap();
        let reseeded = store.admin_password_hash().await.unwrap().unwrap();
        assert!(verify_password("replacement password", &reseeded));
        assert!(
            !store.session_is_valid("old-session", now).await.unwrap(),
            "a forced reset must revoke every session"
        );

        // Reset without a password to reset to is a refusal, not a wipe.
        assert!(seed_admin_credential(&store, None, true).await.is_err());
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api auth::` — expected: compile error, `seed_admin_credential` not found.

- [ ] **Step 3: Implement seeding** — in `auth.rs` (add `use crate::store::{Store, StoreError};` and `use tracing::{info, warn};` to the imports):

```rust
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
            let phc = hash_password(password)?;
            store.set_admin_password_hash(&phc, chrono::Utc::now()).await.map_err(seed_err)?;
            store.delete_all_sessions().await.map_err(seed_err)?;
            warn!("ADMIN_PASSWORD_RESET: admin password re-seeded from env, all sessions revoked");
            Ok(())
        }
        (Some(_), None, true) => anyhow::bail!(
            "ADMIN_PASSWORD_RESET=true but ADMIN_PASSWORD is not set; nothing to reset to"
        ),
        (None, Some(password), _) => {
            let phc = hash_password(password)?;
            store.set_admin_password_hash(&phc, chrono::Utc::now()).await.map_err(seed_err)?;
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
```

- [ ] **Step 4: Run the tests and make sure they pass**

Run: `cargo test -p vms-api auth::` — expected: 6 passed (3 from Task 2 + 3 new).

- [ ] **Step 5: Wire `AppState` and `main()`** — in `main.rs`:

Add to `struct AppState` (after `default_retention_days`):

```rust
    login_throttle: Arc<tokio::sync::Mutex<crate::auth::LoginThrottle>>,
    cookie_secure: bool,
```

In `main()`, right after `info!(%database_url, "fleet store open");`:

```rust
    let admin_password = env::var("ADMIN_PASSWORD").ok().filter(|value| !value.is_empty());
    let force_reset = env::var("ADMIN_PASSWORD_RESET").is_ok_and(|value| value == "true");
    auth::seed_admin_credential(store.as_ref(), admin_password.as_deref(), force_reset).await?;
```

In the `AppState { ... }` literal in `main()`, add:

```rust
        login_throttle: Arc::new(tokio::sync::Mutex::new(auth::LoginThrottle::default())),
        cookie_secure: env::var("AUTH_COOKIE_SECURE").is_ok_and(|value| value == "true"),
```

In `test_state_with` in the tests module, add the same two fields:

```rust
            login_throttle: Arc::new(tokio::sync::Mutex::new(crate::auth::LoginThrottle::default())),
            cookie_secure: false,
```

And add next to `SHARED_TOKEN`:

```rust
    const ADMIN_PASSWORD: &str = "correct horse battery staple";

    /// Seed the admin credential the way main() does at startup.
    async fn seed_admin(state: &AppState) {
        crate::auth::seed_admin_credential(state.store.as_ref(), Some(ADMIN_PASSWORD), false)
            .await
            .expect("seed admin credential");
    }
```

- [ ] **Step 6: Run all tests**

Run: `cargo test -p vms-api` — expected: everything passes (`seed_admin` may warn as unused until Task 4; fine).

- [ ] **Step 7: Commit**

```bash
git add services/api/src/auth.rs services/api/src/main.rs
git commit -m "The API refuses to start without an admin credential"
```

---

### Task 4: Router split, session middleware, login and session-check endpoints

**Files:**
- Modify: `services/api/src/auth.rs` (cookie helpers, middleware, `auth_login`, `auth_session`)
- Modify: `services/api/src/main.rs` (`build_router` split, tests)

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `auth::require_session` — axum middleware fn
  - `auth::auth_login`, `auth::auth_session` — handlers
  - `auth::session_cookie(headers: &HeaderMap) -> Option<String>`
  - `auth::set_cookie_value(session_id: &str, secure: bool) -> String`
  - `auth::clear_cookie_value(secure: bool) -> String`
  - test helper `login_cookie(&AppState) -> String` returning `"vms_session=<id>"`

- [ ] **Step 1: Write the failing tests** — in `main.rs` tests module:

```rust
    /// Every route the browser touches. The router in build_router has exactly
    /// two groups; when you add a protected route there, add it here or the
    /// with-a-session test below cannot vouch for it.
    const PROTECTED_ROUTES: &[(&str, &str)] = &[
        ("GET", "/api/v1/fleet"),
        ("GET", "/api/v1/cameras"),
        ("GET", "/api/v1/gateways"),
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
        assert!(set_cookie.contains("HttpOnly"), "cookie missing HttpOnly: {set_cookie}");
        assert!(set_cookie.contains("SameSite=Lax"), "cookie missing SameSite: {set_cookie}");
        set_cookie.split(';').next().unwrap().to_string()
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
        let cookie = login_cookie(&state).await;
        for (method, uri) in PROTECTED_ROUTES {
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
            post("/api/v1/auth/login", None, serde_json::json!({ "password": "wrong" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(state.login_throttle.lock().await.consecutive(), 1);

        // A later success resets the count.
        let _ = login_cookie(&state).await;
        assert_eq!(state.login_throttle.lock().await.consecutive(), 0);
    }

    #[tokio::test]
    async fn the_session_check_reports_alive_or_not() {
        let state = test_state().await;
        seed_admin(&state).await;
        let (status, _) = send(&state, get("/api/v1/auth/session")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let cookie = login_cookie(&state).await;
        let mut request = get("/api/v1/auth/session");
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
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
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api every_protected_route` — expected: FAIL — the routes answer 200/404, not 401 (no middleware yet); `login` returns 404.

- [ ] **Step 3: Implement cookie helpers, middleware and the two handlers** — in `auth.rs`. Extend imports:

```rust
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header::{COOKIE, SET_COOKIE}},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;

use crate::AppState;
```

Then:

```rust
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
        .find_map(|pair| pair.strip_prefix("vms_session="))
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
        let delay = state.login_throttle.lock().await.register_failure();
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
    (
        StatusCode::NO_CONTENT,
        [(SET_COOKIE, set_cookie_value(&session_id, state.cookie_secure))],
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
```

- [ ] **Step 4: Split the router** — replace the body of `build_router` in `main.rs`:

```rust
/// Build the HTTP surface. Split out of `main` so tests can drive it with a
/// `AppState` of their own rather than a live socket and the environment.
///
/// Two groups. `open` is health, the login pair, and the gateway machine
/// endpoints, which carry their own bearer checks. Everything else is
/// `protected` behind the session middleware — a route added there is covered
/// by construction, and must also be added to PROTECTED_ROUTES in the tests.
/// The two plugin endpoints the edge gateway calls without credentials
/// (ai/analyze, storage/uploads) stay open for now; a beads task tracks
/// giving them gateway bearer auth.
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
        )
        .route(
            "/api/v1/plugins/{plugin_id}/ai/analyze",
            post(plugin_ai_analyze),
        )
        .route(
            "/api/v1/plugins/{plugin_id}/storage/uploads",
            post(plugin_storage_upload),
        );
    let protected = Router::new()
        .route("/api/v1/fleet", get(fleet))
        .route("/api/v1/enrollments", post(create_enrollment))
        .route("/api/v1/cameras", get(cameras))
        .route("/api/v1/gateways", get(gateways))
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
        .route("/api/v1/auth/password", post(crate::auth::auth_change_password))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_session,
        ));
    open.merge(protected)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
```

**For this task only**, stub the two Task 5 handlers in `auth.rs` so the router compiles (Task 5 replaces them):

```rust
pub async fn auth_logout() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

pub async fn auth_change_password() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
```

- [ ] **Step 5: Fix the pre-existing tests that now need a session.** These tests call protected routes and must seed + log in + attach the cookie (use `login_cookie` and add a `cookie: Option<&str>` variant of the request builders, or insert the header on the built request as the new tests do):

- `enrollment_and_gateway_token_live_in_the_store_not_in_memory` — `POST /api/v1/enrollments` needs the cookie (seed + login on the first state).
- `a_known_camera_reports_offline_after_a_restart_instead_of_vanishing` — `GET /api/v1/cameras` and `/api/v1/fleet` on the restarted state: seed once (the store file is shared), log in on the restarted state.
- `a_completed_recording_survives_a_restart_and_appears_on_the_timeline` — `POST /api/v1/cameras/cam-1/recordings` and the timeline GET.
- `an_enrolled_gateway_token_works_and_does_not_cover_other_gateways` — its `POST /api/v1/enrollments` step.
- any other test the run flags with an unexpected 401 (let the failures list them).

Also check `with_camera` (it posts telemetry — open route, no change) and leave gateway-token tests untouched: they exercise the open group.

- [ ] **Step 6: Run all tests**

Run: `cargo test -p vms-api` — expected: all pass, including the five new ones.

- [ ] **Step 7: Commit**

```bash
git add services/api/src/auth.rs services/api/src/main.rs
git commit -m "Every human-facing route now demands a session"
```

---

### Task 5: Logout and change-password

**Files:**
- Modify: `services/api/src/auth.rs` (replace the two stubs)
- Modify: `services/api/src/main.rs` (tests)

**Interfaces:**
- Consumes: Task 4 helpers; store methods.
- Produces: real `auth_logout`, `auth_change_password`. Request body for password change: `{"current": "...", "new": "..."}`. Wrong current → 403; new shorter than 12 → 422; success → 204 + fresh cookie, all other sessions dead.

- [ ] **Step 1: Write the failing tests** — in `main.rs` tests module:

```rust
    #[tokio::test]
    async fn logout_invalidates_the_session_and_clears_the_cookie() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let mut request = post("/api/v1/auth/logout", None, serde_json::json!({}));
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cleared = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("logout must clear the cookie")
            .to_str()
            .unwrap();
        assert!(cleared.contains("Max-Age=0"), "not a clearing cookie: {cleared}");

        let mut request = get("/api/v1/auth/session");
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "the session survived logout");
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
        request.headers_mut().insert("cookie", caller.parse().unwrap());
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
        request.headers_mut().insert("cookie", other.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "the old session outlived the change");
        let mut request = get("/api/v1/auth/session");
        request.headers_mut().insert("cookie", fresh.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // And only the new password logs in now.
        let (status, _) = send(
            &state,
            post("/api/v1/auth/login", None, serde_json::json!({ "password": ADMIN_PASSWORD })),
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
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": ADMIN_PASSWORD, "new": "short" }),
        );
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // Both refusals left the credential untouched.
        let (status, _) = send(
            &state,
            post("/api/v1/auth/login", None, serde_json::json!({ "password": ADMIN_PASSWORD })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api logout_invalidates` — expected: FAIL with 501 from the stubs.

- [ ] **Step 3: Replace the stubs** — in `auth.rs`:

```rust
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
        return StatusCode::FORBIDDEN.into_response();
    }
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
    (
        StatusCode::NO_CONTENT,
        [(SET_COOKIE, set_cookie_value(&session_id, state.cookie_secure))],
    )
        .into_response()
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p vms-api` — expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/auth.rs services/api/src/main.rs
git commit -m "Logout and password change; a change revokes every other session"
```

---

### Task 6: Session expiry in the retention loop, and restart survival

**Files:**
- Modify: `services/api/src/main.rs` (`retention_pass`, tests)

**Interfaces:**
- Consumes: `delete_expired_sessions` (Task 1), test helpers (Tasks 3–4).

- [ ] **Step 1: Write the failing tests** — in `main.rs` tests module:

```rust
    #[tokio::test]
    async fn the_retention_pass_sweeps_expired_sessions() {
        let state = test_state().await;
        let now = Utc::now();
        state.store.create_session("expired", now - chrono::Duration::days(8), now - chrono::Duration::days(1)).await.unwrap();
        state.store.create_session("alive", now, now + chrono::Duration::days(7)).await.unwrap();

        retention_pass(&state).await;

        assert!(!state.store.session_is_valid("expired", now - chrono::Duration::days(2)).await.unwrap(),
            "the expired row should be gone even for a past `now`");
        assert!(state.store.session_is_valid("alive", now).await.unwrap());
    }

    #[tokio::test]
    async fn a_session_survives_an_api_restart() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("store"),
        );
        let state = test_state_with(store).await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let mut request = get("/api/v1/auth/session");
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&restarted, request).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "a login must survive an API restart; that is what storing sessions is for"
        );
    }
```

- [ ] **Step 2: Run and watch the sweep test fail**

Run: `cargo test -p vms-api the_retention_pass_sweeps` — expected: FAIL — the expired session is still valid for a past `now` (nothing sweeps it). The restart test should already pass; keep it anyway as the pinned spec behavior.

- [ ] **Step 3: Implement** — at the top of `retention_pass` in `main.rs`, before the expired-recordings block:

```rust
    if let Err(err) = state.store.delete_expired_sessions(Utc::now()).await {
        warn!(error = %err, "retention could not sweep expired sessions");
    }
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p vms-api` — expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/main.rs
git commit -m "Expired sessions are swept by retention; live ones survive restarts"
```

---

### Task 7: The login view in the SPA

**Files:**
- Create: `web/auth.js`
- Modify: `web/app.html` (login overlay markup, before the closing `</body>` script tag)
- Modify: `web/dashboard.js` (session gate)
- Modify: `web/styles.css` (opaque backdrop)
- Modify: `web/locales/en.json`, `web/locales/es.json`, `web/locales/ru.json`
- Create: `web/tests/auth.test.mjs`

**Interfaces:**
- Consumes: `t(dict, key)` from `web/theme.js`; the API contract from Tasks 4–5.
- Produces: `requireSession(dict)` (resolves once a session exists), `installUnauthorizedTrap()`, `logout()`, `changePassword(current, next)` — all exported from `web/auth.js`.

- [ ] **Step 1: Write the failing tests** — create `web/tests/auth.test.mjs`:

```js
// The login gate. The dashboard must not paint before a session exists, and
// any later 401 must flip back to the login view.
import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');
const dict = JSON.parse(read('locales/en.json'));

let requireSession, installUnauthorizedTrap;
let responses; // path suffix -> status

function stubFetch() {
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    const match = Object.entries(responses).find(([suffix]) => path.includes(suffix));
    const status = match ? match[1] : 404;
    return {
      ok: status >= 200 && status < 300,
      status,
      json: async () => ({}),
      _options: options,
    };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'Event', 'FormData']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key];
  }
  return dom;
}

beforeEach(async () => {
  loadPage();
  responses = {};
  stubFetch();
  ({ requireSession, installUnauthorizedTrap } = await import('../auth.js'));
});

test('an alive session passes straight through without showing the login view', async () => {
  responses['api/v1/auth/session'] = 204;
  await requireSession(dict);
  assert.ok(!document.querySelector('#login-view').classList.contains('open'));
});

test('no session shows the login view and a successful login resolves the gate', async () => {
  responses['api/v1/auth/session'] = 401;
  responses['api/v1/auth/login'] = 204;
  const gate = requireSession(dict);

  // The view is up and translated.
  const view = document.querySelector('#login-view');
  assert.ok(view.classList.contains('open'), 'the login view did not open');
  const heading = view.querySelector('[data-i18n="app.auth.title"]');
  assert.notEqual(heading.textContent, '', 'the login view is untranslated');

  const form = document.querySelector('#login-form');
  form.querySelector('input[name=password]').value = 'a fine password';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await gate;
  assert.ok(!view.classList.contains('open'), 'the login view stayed up after login');
});

test('a wrong password shows the error line and keeps the view open', async () => {
  responses['api/v1/auth/session'] = 401;
  responses['api/v1/auth/login'] = 401;
  const gate = requireSession(dict);
  const form = document.querySelector('#login-form');
  form.querySelector('input[name=password]').value = 'wrong';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(document.querySelector('#login-error').hidden, false);
  assert.ok(document.querySelector('#login-view').classList.contains('open'));
  // The gate must still be pending; a resolved gate would paint the dashboard.
  let settled = false;
  gate.then(() => { settled = true; });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(settled, false, 'the gate resolved on a failed login');
});

test('a 401 from any API call flips back to the login view', async () => {
  responses['api/v1/auth/session'] = 204;
  await requireSession(dict);
  installUnauthorizedTrap();
  responses['api/v1/fleet'] = 401;
  await globalThis.fetch('api/v1/fleet');
  assert.ok(document.querySelector('#login-view').classList.contains('open'),
    'an expired session did not bring the login view back');
});
```

- [ ] **Step 2: Run and watch them fail**

Run: `cd web && npm test` — expected: `auth.test.mjs` fails (no `../auth.js`, no `#login-view`).

- [ ] **Step 3: Add the markup** — in `web/app.html`, insert immediately after `<body class="app-body">`:

```html
  <div id="login-view" class="modal-backdrop login-backdrop">
    <form id="login-form" class="modal login-modal">
      <h2 data-i18n="app.auth.title"></h2>
      <p data-i18n="app.auth.subtitle"></p>
      <label class="field"><span data-i18n="app.auth.password"></span><input name="password" type="password" required autocomplete="current-password" /></label>
      <div id="login-error" class="form-error" hidden data-i18n="app.auth.failed"></div>
      <div class="modal-actions"><button class="button primary" type="submit" data-i18n="app.auth.signIn"></button></div>
    </form>
  </div>
```

And in `web/styles.css`, after the `.modal-backdrop.open` rule:

```css
/* The login gate sits above everything and is opaque: an unauthenticated
   viewer must not see the fleet behind a translucent scrim. */
.login-backdrop { background: #0a1420; backdrop-filter: none; z-index: 60; }
.login-modal { width: min(400px, 100%); }
```

- [ ] **Step 4: Write `web/auth.js`:**

```js
// The session gate. Runs before the dashboard paints, owns the login view,
// and traps any later 401 so an expired session brings the login back
// instead of quietly demoting the page to demo data.
import { t } from './theme.js';

const view = () => document.querySelector('#login-view');

function showLogin(dict) {
  const node = view();
  node.querySelectorAll('[data-i18n]').forEach(el => el.textContent = t(dict, el.dataset.i18n, el.dataset.i18n));
  node.classList.add('open');
}

/** Resolves once a session exists — immediately, or after a successful login. */
export async function requireSession(dict) {
  const alive = await fetch('api/v1/auth/session').then(r => r.status === 204).catch(() => false);
  if (alive) return;
  showLogin(dict);
  await new Promise(resolve => {
    const form = document.querySelector('#login-form');
    form.addEventListener('submit', async event => {
      event.preventDefault();
      const password = new FormData(form).get('password');
      const response = await fetch('api/v1/auth/login', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ password }),
      }).catch(() => ({ status: 0 }));
      if (response.status === 204) {
        view().classList.remove('open');
        document.querySelector('#login-error').hidden = true;
        resolve();
      } else {
        document.querySelector('#login-error').hidden = false;
      }
    });
  });
}

/** After this, any 401 from the API re-opens the login view. */
export function installUnauthorizedTrap() {
  const original = globalThis.fetch;
  globalThis.fetch = async (url, options) => {
    const response = await original(url, options);
    const path = String(url);
    if (response.status === 401 && path.includes('api/') && !path.includes('api/v1/auth/')) {
      view().classList.add('open');
    }
    return response;
  };
}

export async function logout() {
  await fetch('api/v1/auth/logout', { method: 'POST' }).catch(() => {});
  location.reload();
}

/** Returns the response status; 204 means changed. */
export async function changePassword(current, next) {
  const response = await fetch('api/v1/auth/password', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ current, new: next }),
  }).catch(() => ({ status: 0 }));
  return response.status;
}
```

- [ ] **Step 5: Gate the bootstrap** — replace `web/dashboard.js`:

```js
// Bootstrap only. Everything the page does lives in dashboard-app.js, which is
// a function of its context so a test can supply one — see tests/dashboard.test.mjs.
// The session gate runs first: nothing paints until the API vouches for us.
import { loadRuntime } from './theme.js';
import { requireSession, installUnauthorizedTrap } from './auth.js';
import { startDashboard } from './dashboard-app.js';

const { brand, locale, dict } = await loadRuntime();
await requireSession(dict);
installUnauthorizedTrap();
await startDashboard({ brand, locale, dict });
```

- [ ] **Step 6: Add the strings** — to `web/locales/en.json` (and translated equivalents to `es.json`, `ru.json`):

```json
  "app.auth.title": "Sign in",
  "app.auth.subtitle": "This dashboard controls cameras. It needs to know who you are.",
  "app.auth.password": "Password",
  "app.auth.signIn": "Sign in",
  "app.auth.failed": "That password was not accepted."
```

es: `"Iniciar sesión"`, `"Este panel controla cámaras. Necesita saber quién eres."`, `"Contraseña"`, `"Iniciar sesión"`, `"Esa contraseña no fue aceptada."`
ru: `"Вход"`, `"Эта панель управляет камерами. Ей нужно знать, кто вы."`, `"Пароль"`, `"Войти"`, `"Пароль не принят."`

- [ ] **Step 7: Run the web tests**

Run: `cd web && npm test` — expected: all pass, including the existing suites (the i18n test may require every locale to carry the new keys — that is what step 6 did).

- [ ] **Step 8: Commit**

```bash
git add web/auth.js web/app.html web/dashboard.js web/styles.css web/locales/en.json web/locales/es.json web/locales/ru.json web/tests/auth.test.mjs
git commit -m "The dashboard asks who you are before it paints"
```

---

### Task 8: Account modal — logout and change password

**Files:**
- Modify: `web/app.html` (account modal; wire the avatar)
- Modify: `web/dashboard-app.js` (open/close + submit wiring)
- Modify: `web/locales/{en,es,ru}.json`
- Modify: `web/tests/auth.test.mjs`

**Interfaces:**
- Consumes: `logout()`, `changePassword()` from `web/auth.js` (Task 7).

- [ ] **Step 1: Write the failing tests** — append to `web/tests/auth.test.mjs`:

```js
test('the account modal changes the password and reports success', async () => {
  const { wireAccountModal } = await import('../auth.js');
  responses['api/v1/auth/password'] = 204;
  wireAccountModal(dict);

  document.querySelector('.avatar').dispatchEvent(new Event('click', { bubbles: true }));
  assert.ok(document.querySelector('#account-modal').classList.contains('open'));

  const form = document.querySelector('#password-form');
  form.querySelector('input[name=current]').value = 'the old password';
  form.querySelector('input[name=new]').value = 'a long enough new one';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(document.querySelector('#password-done').hidden, false);
});

test('a rejected password change shows the error, not the success line', async () => {
  const { wireAccountModal } = await import('../auth.js');
  responses['api/v1/auth/password'] = 403;
  wireAccountModal(dict);
  const form = document.querySelector('#password-form');
  form.querySelector('input[name=current]').value = 'wrong';
  form.querySelector('input[name=new]').value = 'a long enough new one';
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(document.querySelector('#password-error').hidden, false);
  assert.equal(document.querySelector('#password-done').hidden, true);
});
```

- [ ] **Step 2: Run and watch them fail**

Run: `cd web && npm test` — expected: FAIL, no `wireAccountModal`, no `#account-modal`.

- [ ] **Step 3: Add the modal** — in `web/app.html`, before `<div id="brand-modal" ...>`:

```html
  <div id="account-modal" class="modal-backdrop">
    <form id="password-form" class="modal login-modal">
      <h2 data-i18n="app.auth.accountTitle"></h2>
      <p data-i18n="app.auth.accountSubtitle"></p>
      <label class="field"><span data-i18n="app.auth.currentPassword"></span><input name="current" type="password" required autocomplete="current-password" /></label>
      <label class="field"><span data-i18n="app.auth.newPassword"></span><input name="new" type="password" required minlength="12" autocomplete="new-password" /></label>
      <div id="password-error" class="form-error" hidden data-i18n="app.auth.changeFailed"></div>
      <div id="password-done" class="metric-sub" hidden data-i18n="app.auth.changed"></div>
      <div class="modal-actions">
        <button id="logout-button" type="button" class="button" data-i18n="app.auth.signOut"></button>
        <button type="button" class="button" data-close-account data-i18n="app.close"></button>
        <button type="submit" class="button primary" data-i18n="app.auth.change"></button>
      </div>
    </form>
  </div>
```

- [ ] **Step 4: Implement `wireAccountModal`** — add to `web/auth.js`:

```js
/** Avatar opens the account modal; the form changes the password; the button signs out. */
export function wireAccountModal(dict) {
  const modal = document.querySelector('#account-modal');
  modal.querySelectorAll('[data-i18n]').forEach(el => el.textContent = t(dict, el.dataset.i18n, el.dataset.i18n));
  document.querySelector('.avatar').addEventListener('click', () => modal.classList.add('open'));
  modal.querySelectorAll('[data-close-account]').forEach(node =>
    node.addEventListener('click', () => modal.classList.remove('open')));
  document.querySelector('#logout-button').addEventListener('click', () => logout());
  const form = document.querySelector('#password-form');
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const data = new FormData(form);
    const status = await changePassword(data.get('current'), data.get('new'));
    document.querySelector('#password-done').hidden = status !== 204;
    document.querySelector('#password-error').hidden = status === 204;
    if (status === 204) form.reset();
  });
}
```

And call it from `web/dashboard-app.js` — at the top, extend the import:

```js
import { wireAccountModal } from './auth.js';
```

and near the other wiring (next to the `#add-gateway` listener around line 643):

```js
  wireAccountModal(dict);
```

Check `web/tests/dashboard.test.mjs` still passes: its jsdom page now needs the modal nodes (they are in app.html, so it does) and its fetch stub will 404 the auth calls, which `wireAccountModal` never makes at wire time — only on click/submit.

- [ ] **Step 5: Add the strings** — `en.json`:

```json
  "app.auth.accountTitle": "Account",
  "app.auth.accountSubtitle": "One admin account. Changing the password signs out every other browser.",
  "app.auth.currentPassword": "Current password",
  "app.auth.newPassword": "New password (12 characters or more)",
  "app.auth.change": "Change password",
  "app.auth.changed": "Password changed.",
  "app.auth.changeFailed": "Not changed — check the current password and the new length.",
  "app.auth.signOut": "Sign out"
```

es: `"Cuenta"`, `"Una sola cuenta de administrador. Cambiar la contraseña cierra la sesión en los demás navegadores."`, `"Contraseña actual"`, `"Nueva contraseña (12 caracteres o más)"`, `"Cambiar contraseña"`, `"Contraseña cambiada."`, `"Sin cambios: revisa la contraseña actual y la longitud de la nueva."`, `"Cerrar sesión"`
ru: `"Аккаунт"`, `"Один аккаунт администратора. Смена пароля завершает сеансы в остальных браузерах."`, `"Текущий пароль"`, `"Новый пароль (не менее 12 символов)"`, `"Сменить пароль"`, `"Пароль изменён."`, `"Не изменено — проверьте текущий пароль и длину нового."`, `"Выйти"`

- [ ] **Step 6: Run the web tests**

Run: `cd web && npm test` — expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add web/app.html web/auth.js web/dashboard-app.js web/locales/en.json web/locales/es.json web/locales/ru.json web/tests/auth.test.mjs
git commit -m "Account modal: sign out and change the password from the dashboard"
```

---

### Task 9: Compose wiring, docs, side-quest, smoke test

**Files:**
- Modify: `docker-compose.yml` (api service environment)
- Modify: `docs/RUNNING-LOCALLY.md`
- Modify: `docs/BACKLOG.md` (if it lists dashboard auth, check it off)

- [ ] **Step 1: Compose** — in `docker-compose.yml`, add to the api service `environment:` block:

```yaml
      # No default on purpose: an API that cannot authenticate anyone refuses
      # to start, and compose should refuse with it rather than boot a locked-out
      # stack. Set it in .env or the shell.
      ADMIN_PASSWORD: ${ADMIN_PASSWORD:?set ADMIN_PASSWORD to the dashboard admin password}
```

- [ ] **Step 2: Docs** — in `docs/RUNNING-LOCALLY.md`, add a "Logging in" section: `ADMIN_PASSWORD` seeds the single admin account on first boot and is ignored afterwards; change it from the avatar menu; `ADMIN_PASSWORD_RESET=true` re-seeds from env and signs everything out; `AUTH_COOKIE_SECURE=true` for HTTPS deployments; the API refuses to start with no credential at all.

- [ ] **Step 3: Beads side-quest** — record the discovered hole:

```bash
bd create -t bug --priority=2 \
  --title="Gateway-called plugin endpoints (ai/analyze, storage/uploads) accept unauthenticated callers" \
  --description="Discovered while cookie-gating the dashboard: edge/gateway calls POST /api/v1/plugins/{id}/ai/analyze and /storage/uploads with no Authorization header, so they had to stay in the open router group. Anyone who can reach the API can mint signed upload URLs or spend AI quota. Fix: gateway sends its bearer on these calls; API checks authorized_gateway. Needs a coordinated gateway+API change."
bd dep add <new-id> relaysight-vms-06f --type discovered-from
```

- [ ] **Step 4: Smoke test against a real process** (check the port is free first with `ss -ltn`; use 8123 or another free one):

```bash
cargo build -p vms-api
(DATABASE_URL=sqlite:/tmp/vms-auth-smoke.db API_BIND=127.0.0.1:8123 ADMIN_PASSWORD='smoke-test-password' ./target/debug/vms-api &> /tmp/vms-auth-smoke.log &)
sleep 2
curl -s http://127.0.0.1:8123/healthz                                   # {"status":"ok",...}
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8123/api/v1/fleet   # 401
curl -s -c /tmp/smoke-jar -o /dev/null -w '%{http_code}\n' \
  -H 'content-type: application/json' -d '{"password":"smoke-test-password"}' \
  http://127.0.0.1:8123/api/v1/auth/login                               # 204
curl -s -b /tmp/smoke-jar -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8123/api/v1/fleet  # 200
(pkill -f 'debug/vms-api' || true)
rm -f /tmp/vms-auth-smoke.db /tmp/smoke-jar
```

Also verify the refusal: run once with no `ADMIN_PASSWORD` on a fresh DB path and confirm the process exits nonzero with the "no admin credential" message in the log.

- [ ] **Step 5: Full verification**

Run: `cargo test --workspace` and `cd web && npm test` — everything green.

- [ ] **Step 6: Commit**

```bash
git add docker-compose.yml docs/RUNNING-LOCALLY.md docs/BACKLOG.md
git commit -m "Compose demands ADMIN_PASSWORD; running-locally explains the login"
```
