# Persistent Fleet Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move fleet identity, gateway tokens, enrollments and recording manifests out of in-memory HashMaps into SQLite behind a `Store` trait, so an API restart no longer breaks enrolled gateways or loses recordings.

**Architecture:** A single object-safe `Store` trait (`services/api/src/store/mod.rs`) speaking domain language, one `SqliteStore` implementation (`store/sqlite.rs`) on sqlx 0.9 with migrations. `AppState` swaps three of its six maps (`enrollments`, `gateway_tokens`, `recordings`) for `Arc<dyn Store>`; heartbeats, telemetry batches and command queues deliberately stay in memory.

**Tech Stack:** Rust (edition 2024, rustc 1.98), axum 0.8, sqlx 0.9 (SQLite driver, runtime queries — NOT the compile-time `query!` macros, which would pin the crate to one database), async-trait, sha2, serde_json.

**Spec:** `docs/superpowers/specs/2026-09-06-persistent-fleet-store-design.md`

## Global Constraints

- No production code without a failing test first (RED → verify → GREEN → verify → commit). Test commands below assume repo root.
- Tokens (gateway + enrollment) are NEVER stored in plaintext — SHA-256 hex only. A test queries the raw table to pin this.
- Handlers never see SQL; the store never sees HTTP. `StoreError::{NotFound, Gone, Internal}` is the whole error vocabulary at the boundary.
- Timestamps are stored as RFC 3339 TEXT via `to_rfc3339_opts(SecondsFormat::Micros, true)` (fixed width + `Z`, so SQL string comparison is chronological). Never `to_rfc3339()` bare.
- sqlx 0.9 facts (verified 2026-09-06): MSRV 1.94 (ours: 1.98, OK); query functions take `&'static str` SQL (ours all are); repo moved to github.com/transact-rs.
- Command flow, live sessions, plugin endpoints, TURN code: untouched.

## File Structure

- Create: `services/api/migrations/0001_fleet.sql` — the five tables + indexes.
- Create: `services/api/src/store/mod.rs` — `Store` trait, `StoreError`, `CameraRecord`/`SiteRecord`/`OrganizationRecord`, `token_hash()`, `ts()`/`parse_ts()` helpers.
- Create: `services/api/src/store/sqlite.rs` — `SqliteStore` + its unit tests.
- Modify: `services/api/Cargo.toml` — add sqlx, async-trait, sha2.
- Modify: `services/api/src/main.rs` — `AppState`, startup, handlers, retention, router tests.
- Modify: `docker-compose.yml` — data volume + `DATABASE_URL` for the api service.
- Modify: `docs/RUNNING-LOCALLY.md`, `docs/BACKLOG.md` — the database file, backup, backlog note.

---

### Task 1: SqliteStore skeleton — deps, migration, connect

**Files:**
- Modify: `services/api/Cargo.toml`
- Create: `services/api/migrations/0001_fleet.sql`
- Create: `services/api/src/store/mod.rs`
- Create: `services/api/src/store/sqlite.rs`
- Modify: `services/api/src/main.rs` (add `mod store;` only)

**Interfaces:**
- Produces: `SqliteStore::connect(url: &str) -> anyhow::Result<SqliteStore>` (applies migrations, creates the file), `SqliteStore::in_memory() -> anyhow::Result<SqliteStore>` (single-connection pool so the in-memory DB survives), `StoreError`, `ts()`, `parse_ts()`.

- [ ] **Step 1: Add dependencies**

In `services/api/Cargo.toml` under `[dependencies]`:

```toml
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "sqlite", "migrate"] }
async-trait = "0.1"
sha2 = "0.10"
```

Run: `cargo check -p vms-api`. If a feature name fails to resolve, consult `cargo add sqlx --dry-run -F runtime-tokio,sqlite,migrate` output and fix the feature list — do not add TLS or other drivers.

- [ ] **Step 2: Write the migration**

`services/api/migrations/0001_fleet.sql`:

```sql
CREATE TABLE organizations (
    id   TEXT PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE sites (
    id     TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES organizations(id),
    name   TEXT NOT NULL,
    city   TEXT NOT NULL
);
CREATE INDEX idx_sites_org ON sites(org_id);

CREATE TABLE gateways (
    id          TEXT PRIMARY KEY,
    site_id     TEXT NOT NULL REFERENCES sites(id),
    hostname    TEXT,
    version     TEXT,
    -- Empty string means "seen in telemetry, never enrolled": such a gateway
    -- authorizes only via the bootstrap GATEWAY_TOKEN. A real hash is 64 hex
    -- chars, so no presented token can ever match ''.
    token_hash  TEXT NOT NULL DEFAULT '',
    enrolled_at TEXT,
    last_seen   TEXT
);

CREATE TABLE enrollments (
    token_hash TEXT PRIMARY KEY,
    org_id     TEXT NOT NULL,
    org_name   TEXT NOT NULL,
    site_id    TEXT NOT NULL,
    site_name  TEXT NOT NULL,
    city       TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    claimed    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE cameras (
    id           TEXT PRIMARY KEY,
    gateway_id   TEXT NOT NULL REFERENCES gateways(id),
    site_id      TEXT NOT NULL,
    name         TEXT NOT NULL,
    manufacturer TEXT,
    model        TEXT,
    firmware     TEXT,
    codec        TEXT,
    width        INTEGER,
    height       INTEGER,
    first_seen   TEXT NOT NULL,
    last_seen    TEXT NOT NULL
);
CREATE INDEX idx_cameras_gateway ON cameras(gateway_id);

CREATE TABLE recordings (
    id           TEXT PRIMARY KEY,
    camera_id    TEXT NOT NULL,
    started_at   TEXT NOT NULL,
    ended_at     TEXT NOT NULL,
    delete_after TEXT,
    codec        TEXT NOT NULL,
    manifest     TEXT NOT NULL
);
CREATE INDEX idx_recordings_camera ON recordings(camera_id, started_at);
CREATE INDEX idx_recordings_expiry ON recordings(delete_after);
```

Note: `gateways.enrolled_at` is nullable here although the spec sketch said NOT NULL — telemetry can legitimately introduce a gateway that never enrolled (bootstrap token path), and NULL says that more honestly than a sentinel string.

- [ ] **Step 3: Write the failing test**

`services/api/src/store/sqlite.rs`:

```rust
//! SQLite implementation of the fleet store.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

pub struct SqliteStore {
    pool: SqlitePool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_applies_migrations() {
        let store = SqliteStore::in_memory().await.expect("in-memory store");
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
        )
        .fetch_all(&store.pool)
        .await
        .expect("list tables");
        for expected in ["organizations", "sites", "gateways", "enrollments", "cameras", "recordings"] {
            assert!(tables.iter().any(|t| t == expected), "missing table {expected}, have {tables:?}");
        }
    }
}
```

And in `services/api/src/main.rs`, after `mod turn;`: add `mod store;`. Create `services/api/src/store/mod.rs`:

```rust
//! The persistence boundary. Handlers speak these methods; SQL lives in the
//! implementations. See docs/superpowers/specs/2026-09-06-persistent-fleet-store-design.md.

mod sqlite;

pub use sqlite::SqliteStore;

use chrono::{DateTime, SecondsFormat, Utc};

#[derive(Debug)]
pub enum StoreError {
    /// The row does not exist.
    NotFound,
    /// The row exists but is no longer usable (claimed or expired enrollment).
    Gone,
    Internal(anyhow::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NotFound => write!(f, "not found"),
            StoreError::Gone => write!(f, "gone"),
            StoreError::Internal(err) => write!(f, "store error: {err}"),
        }
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        StoreError::Internal(err.into())
    }
}

/// Fixed-width RFC 3339 with microseconds and a `Z` suffix, so that SQL string
/// comparison of two stored timestamps is chronological comparison.
pub(crate) fn ts(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub(crate) fn parse_ts(value: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| StoreError::Internal(anyhow::anyhow!("bad stored timestamp {value:?}: {err}")))
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p vms-api store::sqlite::tests::connect_applies_migrations 2>&1 | tail -20`
Expected: compile error — `in_memory` not found (the test names the API before it exists).

- [ ] **Step 5: Implement connect / in_memory**

Add to `sqlite.rs` (above the tests):

```rust
impl SqliteStore {
    /// Open (creating if missing) and migrate. `url` is `sqlite:<path>`.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// One permanent connection: an in-memory SQLite database lives exactly as
    /// long as its connection, so the pool must never open a second one or
    /// close the first.
    pub async fn in_memory() -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .min_connections(1)
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -5`
Expected: `connect_applies_migrations ... ok`. Also run `cargo check -p vms-api` — warnings about unused items are fine at this stage, errors are not.

- [ ] **Step 7: Commit**

```bash
git add services/api
git commit -m "Add the SQLite fleet store skeleton: schema, connect, migrations"
```

---

### Task 2: Enrollment lifecycle in the store

**Files:**
- Modify: `services/api/src/store/mod.rs` (trait begins)
- Modify: `services/api/src/store/sqlite.rs`

**Interfaces:**
- Consumes: Task 1 (`SqliteStore`, `ts`, `parse_ts`, `StoreError`).
- Produces on `trait Store: Send + Sync` (async via `#[async_trait::async_trait]`):
  - `async fn create_enrollment(&self, token: &str, request: &EnrollmentRequest, expires_at: DateTime<Utc>) -> Result<(), StoreError>`
  - `async fn enrollment_request(&self, token: &str, now: DateTime<Utc>) -> Result<EnrollmentRequest, StoreError>` — read-only validity check; `NotFound` = no such token, `Gone` = claimed or expired
  - `async fn claim_enrollment(&self, token: &str, now: DateTime<Utc>) -> Result<EnrollmentRequest, StoreError>` — atomic claim, same error meanings
  - free fn `pub fn token_hash(token: &str) -> String` — SHA-256 hex, 64 chars

- [ ] **Step 1: Write the failing tests**

Append to the tests module in `sqlite.rs`:

```rust
use crate::store::{token_hash, Store, StoreError};
use chrono::{Duration, Utc};
use vms_domain::EnrollmentRequest;

fn request() -> EnrollmentRequest {
    EnrollmentRequest {
        customer_id: "cust-1".into(),
        customer_name: "Customer".into(),
        site_id: "site-1".into(),
        site_name: "Site".into(),
        city: "Barcelona".into(),
    }
}

#[tokio::test]
async fn an_enrollment_claims_exactly_once() {
    let store = SqliteStore::in_memory().await.unwrap();
    let now = Utc::now();
    store.create_enrollment("TOKEN-A", &request(), now + Duration::minutes(30)).await.unwrap();

    let claimed = store.claim_enrollment("TOKEN-A", now).await.expect("first claim");
    assert_eq!(claimed.customer_id, "cust-1");

    match store.claim_enrollment("TOKEN-A", now).await {
        Err(StoreError::Gone) => {}
        other => panic!("second claim must be Gone, got {other:?}"),
    }
}

#[tokio::test]
async fn an_expired_enrollment_is_gone_and_an_unknown_one_is_not_found() {
    let store = SqliteStore::in_memory().await.unwrap();
    let now = Utc::now();
    store.create_enrollment("TOKEN-B", &request(), now - Duration::seconds(1)).await.unwrap();
    assert!(matches!(store.claim_enrollment("TOKEN-B", now).await, Err(StoreError::Gone)));
    assert!(matches!(store.claim_enrollment("NEVER-ISSUED", now).await, Err(StoreError::NotFound)));
    assert!(matches!(store.enrollment_request("NEVER-ISSUED", now).await, Err(StoreError::NotFound)));
}

#[tokio::test]
async fn enrollment_tokens_are_not_stored_in_plaintext() {
    let store = SqliteStore::in_memory().await.unwrap();
    let now = Utc::now();
    store.create_enrollment("SECRET-TOKEN", &request(), now + Duration::minutes(30)).await.unwrap();
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM enrollments")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_ne!(stored, "SECRET-TOKEN");
    assert_eq!(stored, token_hash("SECRET-TOKEN"));
    assert_eq!(stored.len(), 64);
}
```

Note `claim_enrollment` derives `Debug` needs: `EnrollmentRequest` already derives `Debug` in vms-domain; the `other` arm formats `Result<EnrollmentRequest, StoreError>` so both sides need Debug — `StoreError` already derives it.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -20`
Expected: compile errors — no `Store` trait, no `token_hash`, no `create_enrollment`.

- [ ] **Step 3: Implement trait + methods**

In `store/mod.rs` add:

```rust
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use vms_domain::EnrollmentRequest;

/// SHA-256 of a token, lowercase hex. What the database sees instead of tokens.
pub fn token_hash(token: &str) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn create_enrollment(
        &self,
        token: &str,
        request: &EnrollmentRequest,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Validity check without burning the token (entitlement resolution sits
    /// between look and claim, and an entitlement outage must not burn it).
    async fn enrollment_request(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError>;

    /// Atomic: exactly one caller ever gets Ok for a given token.
    async fn claim_enrollment(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError>;
}
```

In `sqlite.rs`:

```rust
use crate::store::{parse_ts, token_hash, ts, Store, StoreError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row;
use vms_domain::EnrollmentRequest;

fn enrollment_from_row(row: &sqlx::sqlite::SqliteRow) -> EnrollmentRequest {
    EnrollmentRequest {
        customer_id: row.get("org_id"),
        customer_name: row.get("org_name"),
        site_id: row.get("site_id"),
        site_name: row.get("site_name"),
        city: row.get("city"),
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn create_enrollment(
        &self,
        token: &str,
        request: &EnrollmentRequest,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO enrollments (token_hash, org_id, org_name, site_id, site_name, city, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(token_hash(token))
        .bind(&request.customer_id)
        .bind(&request.customer_name)
        .bind(&request.site_id)
        .bind(&request.site_name)
        .bind(&request.city)
        .bind(ts(&expires_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn enrollment_request(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError> {
        let row = sqlx::query(
            "SELECT org_id, org_name, site_id, site_name, city, claimed, expires_at
             FROM enrollments WHERE token_hash = ?1",
        )
        .bind(token_hash(token))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        let claimed: i64 = row.get("claimed");
        let expires_at = parse_ts(row.get::<String, _>("expires_at").as_str())?;
        if claimed != 0 || expires_at < now {
            return Err(StoreError::Gone);
        }
        Ok(enrollment_from_row(&row))
    }

    async fn claim_enrollment(
        &self,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<EnrollmentRequest, StoreError> {
        let claimed = sqlx::query(
            "UPDATE enrollments SET claimed = 1
             WHERE token_hash = ?1 AND claimed = 0 AND expires_at > ?2
             RETURNING org_id, org_name, site_id, site_name, city",
        )
        .bind(token_hash(token))
        .bind(ts(&now))
        .fetch_optional(&self.pool)
        .await?;
        match claimed {
            Some(row) => Ok(enrollment_from_row(&row)),
            // Distinguish "never existed" from "existed, but claimed/expired".
            None => match self.enrollment_request(token, now).await {
                Err(StoreError::NotFound) => Err(StoreError::NotFound),
                _ => Err(StoreError::Gone),
            },
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -10`
Expected: all four tests pass.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/store
git commit -m "Enrollments become rows that claim exactly once, hashed at rest"
```

---

### Task 3: Gateway enrollment and token verification

**Files:**
- Modify: `services/api/src/store/mod.rs`
- Modify: `services/api/src/store/sqlite.rs`

**Interfaces:**
- Consumes: Task 2 (trait, `token_hash`).
- Produces on `Store`:
  - `async fn enroll_gateway(&self, request: &EnrollmentRequest, enroll: &GatewayEnrollmentRequest, gateway_token: &str, now: DateTime<Utc>) -> Result<(), StoreError>` — upserts organization + site + gateway in one transaction; re-enrollment rotates the token hash
  - `async fn verify_gateway_token(&self, gateway_id: &str, token: &str) -> Result<bool, StoreError>`

- [ ] **Step 1: Write the failing tests**

Append to tests in `sqlite.rs`:

```rust
use vms_domain::GatewayEnrollmentRequest;

fn enroll_req(gateway_id: &str) -> GatewayEnrollmentRequest {
    GatewayEnrollmentRequest {
        enrollment_token: "unused-here".into(),
        gateway_id: gateway_id.into(),
        hostname: "edge-1".into(),
        version: "0.1.0".into(),
    }
}

#[tokio::test]
async fn an_enrolled_gateway_survives_a_restart() {
    // The reason this store exists: reopen the same file, the token still works.
    let file = tempfile::NamedTempFile::new().unwrap();
    let url = format!("sqlite:{}", file.path().display());
    {
        let store = SqliteStore::connect(&url).await.unwrap();
        store.enroll_gateway(&request(), &enroll_req("gw-1"), "gw-1-token", Utc::now()).await.unwrap();
        assert!(store.verify_gateway_token("gw-1", "gw-1-token").await.unwrap());
    } // store dropped: the "restart"
    let reopened = SqliteStore::connect(&url).await.unwrap();
    assert!(reopened.verify_gateway_token("gw-1", "gw-1-token").await.unwrap());
    assert!(!reopened.verify_gateway_token("gw-1", "wrong-token").await.unwrap());
    assert!(!reopened.verify_gateway_token("gw-2", "gw-1-token").await.unwrap());
}

#[tokio::test]
async fn gateway_tokens_are_not_stored_in_plaintext() {
    let store = SqliteStore::in_memory().await.unwrap();
    store.enroll_gateway(&request(), &enroll_req("gw-1"), "gw-1-token", Utc::now()).await.unwrap();
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM gateways WHERE id = 'gw-1'")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(stored, token_hash("gw-1-token"));
    assert_ne!(stored, "gw-1-token");
}

#[tokio::test]
async fn re_enrollment_rotates_the_token() {
    let store = SqliteStore::in_memory().await.unwrap();
    store.enroll_gateway(&request(), &enroll_req("gw-1"), "old-token", Utc::now()).await.unwrap();
    store.enroll_gateway(&request(), &enroll_req("gw-1"), "new-token", Utc::now()).await.unwrap();
    assert!(!store.verify_gateway_token("gw-1", "old-token").await.unwrap());
    assert!(store.verify_gateway_token("gw-1", "new-token").await.unwrap());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -20`
Expected: compile errors — `enroll_gateway`, `verify_gateway_token` not on the trait.

- [ ] **Step 3: Implement**

Trait additions in `mod.rs` (inside `trait Store`), plus `use vms_domain::GatewayEnrollmentRequest;`:

```rust
    /// Organization + site + gateway in one transaction. Idempotent; a
    /// repeated enrollment for the same gateway rotates its token.
    async fn enroll_gateway(
        &self,
        request: &EnrollmentRequest,
        enroll: &GatewayEnrollmentRequest,
        gateway_token: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    async fn verify_gateway_token(&self, gateway_id: &str, token: &str) -> Result<bool, StoreError>;
```

Implementation in `sqlite.rs` (inside `impl Store for SqliteStore`):

```rust
    async fn enroll_gateway(
        &self,
        request: &EnrollmentRequest,
        enroll: &GatewayEnrollmentRequest,
        gateway_token: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO organizations (id, name) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
        )
        .bind(&request.customer_id)
        .bind(&request.customer_name)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO sites (id, org_id, name, city) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET org_id = excluded.org_id,
                 name = excluded.name, city = excluded.city",
        )
        .bind(&request.site_id)
        .bind(&request.customer_id)
        .bind(&request.site_name)
        .bind(&request.city)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO gateways (id, site_id, hostname, version, token_hash, enrolled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET site_id = excluded.site_id,
                 hostname = excluded.hostname, version = excluded.version,
                 token_hash = excluded.token_hash, enrolled_at = excluded.enrolled_at",
        )
        .bind(&enroll.gateway_id)
        .bind(&request.site_id)
        .bind(&enroll.hostname)
        .bind(&enroll.version)
        .bind(token_hash(gateway_token))
        .bind(ts(&now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn verify_gateway_token(&self, gateway_id: &str, token: &str) -> Result<bool, StoreError> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT token_hash FROM gateways WHERE id = ?1")
                .bind(gateway_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(stored.is_some_and(|hash| !hash.is_empty() && hash == token_hash(token)))
    }
```

Note `tempfile` is already a dev-dependency; if the test file complains, confirm `tempfile = "3"` is under `[dev-dependencies]` in `services/api/Cargo.toml`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -10`
Expected: all tests pass, including the three new ones.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/store
git commit -m "Gateway enrollment writes org, site and hashed token in one transaction"
```

---

### Task 4: Fleet identity from telemetry

**Files:**
- Modify: `services/api/src/store/mod.rs`
- Modify: `services/api/src/store/sqlite.rs`

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces record types in `mod.rs`:

```rust
#[derive(Debug, Clone)]
pub struct CameraRecord {
    pub id: String,
    pub gateway_id: String,
    pub site_id: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SiteRecord {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub city: String,
    pub cameras: Vec<CameraRecord>,
}

#[derive(Debug, Clone)]
pub struct OrganizationRecord {
    pub id: String,
    pub name: String,
    pub sites: Vec<SiteRecord>,
}
```

- Produces on `Store` (add `use vms_domain::CameraTelemetryBatch;`):
  - `async fn upsert_fleet_identity(&self, batch: &CameraTelemetryBatch, now: DateTime<Utc>) -> Result<(), StoreError>` — org + site + gateway placeholder + camera rows; advances `gateways.last_seen` and `cameras.last_seen`, preserves `cameras.first_seen`, never touches `gateways.token_hash`
  - `async fn fleet_cameras(&self) -> Result<Vec<CameraRecord>, StoreError>` — ordered by name then id
  - `async fn fleet_identity(&self) -> Result<Vec<OrganizationRecord>, StoreError>` — orgs → sites → cameras, all sorted by id

- [ ] **Step 1: Write the failing tests**

```rust
use vms_domain::{CameraTelemetry, CameraTelemetryBatch, HealthStatus};

fn batch_with_camera(gateway_id: &str, camera_id: &str, name: &str) -> CameraTelemetryBatch {
    CameraTelemetryBatch {
        gateway_id: gateway_id.into(),
        customer_id: "cust-1".into(),
        customer_name: "Customer".into(),
        site_id: "site-1".into(),
        site_name: "Site".into(),
        city: "Barcelona".into(),
        sent_at: Utc::now(),
        cameras: vec![CameraTelemetry {
            camera_id: camera_id.into(),
            gateway_id: gateway_id.into(),
            site_id: "site-1".into(),
            name: name.into(),
            status: HealthStatus::Healthy,
            manufacturer: Some("Dahua".into()),
            model: None,
            firmware: None,
            profile_name: None,
            codec: Some("h264".into()),
            width: Some(1920),
            height: Some(1080),
            fps: Some(25.0),
            bitrate_kbps: Some(1800),
            packet_loss: 0,
            reconnects: 0,
            rtsp_endpoint: Some("rtsp://192.0.2.1/stream".into()),
            last_seen: Utc::now(),
            last_error: None,
        }],
    }
}

#[tokio::test]
async fn telemetry_creates_identity_and_repeats_preserve_first_seen() {
    let store = SqliteStore::in_memory().await.unwrap();
    let first = Utc::now();
    store.upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "Entrance"), first).await.unwrap();
    let later = first + Duration::minutes(5);
    let mut renamed = batch_with_camera("gw-1", "cam-1", "Front entrance");
    renamed.customer_name = "Customer Renamed".into();
    store.upsert_fleet_identity(&renamed, later).await.unwrap();

    let cameras = store.fleet_cameras().await.unwrap();
    assert_eq!(cameras.len(), 1);
    assert_eq!(cameras[0].name, "Front entrance");
    assert_eq!(cameras[0].first_seen.timestamp_micros(), first.timestamp_micros());
    assert_eq!(cameras[0].last_seen.timestamp_micros(), later.timestamp_micros());

    let orgs = store.fleet_identity().await.unwrap();
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].name, "Customer Renamed");
    assert_eq!(orgs[0].sites.len(), 1);
    assert_eq!(orgs[0].sites[0].cameras.len(), 1);
}

#[tokio::test]
async fn telemetry_does_not_clobber_an_enrolled_gateways_token() {
    let store = SqliteStore::in_memory().await.unwrap();
    store.enroll_gateway(&request(), &enroll_req("gw-1"), "gw-1-token", Utc::now()).await.unwrap();
    store.upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "Entrance"), Utc::now()).await.unwrap();
    assert!(store.verify_gateway_token("gw-1", "gw-1-token").await.unwrap());
}

#[tokio::test]
async fn a_bootstrap_gateway_gets_a_row_but_no_usable_token() {
    // Telemetry via the shared GATEWAY_TOKEN may arrive before any enrollment.
    let store = SqliteStore::in_memory().await.unwrap();
    store.upsert_fleet_identity(&batch_with_camera("gw-boot", "cam-1", "Entrance"), Utc::now()).await.unwrap();
    assert!(!store.verify_gateway_token("gw-boot", "").await.unwrap());
    assert_eq!(store.fleet_cameras().await.unwrap().len(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -20`
Expected: compile errors — missing trait methods and record types.

- [ ] **Step 3: Implement**

`upsert_fleet_identity` in `sqlite.rs` — one transaction: the same org/site upserts as `enroll_gateway`, then:

```rust
        sqlx::query(
            "INSERT INTO gateways (id, site_id, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET site_id = excluded.site_id,
                 last_seen = excluded.last_seen",
        )
        .bind(&batch.gateway_id)
        .bind(&batch.site_id)
        .bind(ts(&now))
        .execute(&mut *tx)
        .await?;
        for camera in &batch.cameras {
            sqlx::query(
                "INSERT INTO cameras (id, gateway_id, site_id, name, manufacturer, model,
                     firmware, codec, width, height, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
                 ON CONFLICT(id) DO UPDATE SET gateway_id = excluded.gateway_id,
                     site_id = excluded.site_id, name = excluded.name,
                     manufacturer = excluded.manufacturer, model = excluded.model,
                     firmware = excluded.firmware, codec = excluded.codec,
                     width = excluded.width, height = excluded.height,
                     last_seen = excluded.last_seen",
            )
            .bind(&camera.camera_id)
            .bind(&batch.gateway_id)
            .bind(&camera.site_id)
            .bind(&camera.name)
            .bind(&camera.manufacturer)
            .bind(&camera.model)
            .bind(&camera.firmware)
            .bind(&camera.codec)
            .bind(camera.width)
            .bind(camera.height)
            .bind(ts(&now))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
```

(`?11` bound once, used for both `first_seen` and `last_seen` on insert; the conflict arm only advances `last_seen` — that is the preserve-first-seen mechanism. `width`/`height` are `Option<u32>`; bind as `camera.width.map(i64::from)` if sqlx refuses `u32` — read back with `row.get::<Option<i64>, _>("width").map(|v| v as u32)`.)

`fleet_cameras`: `SELECT * FROM cameras ORDER BY name, id`, mapped by hand into `CameraRecord` (a small `camera_from_row` helper — also used by `fleet_identity`).

`fleet_identity`: three queries (`organizations ORDER BY id`, `sites ORDER BY id`, `cameras ORDER BY name, id`), then assemble in Rust by grouping sites under org_id and cameras under site_id.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/store
git commit -m "Telemetry batches upsert fleet identity without touching tokens"
```

---

### Task 5: Recording persistence

**Files:**
- Modify: `services/api/src/store/mod.rs`
- Modify: `services/api/src/store/sqlite.rs`

**Interfaces:**
- Consumes: Task 1.
- Produces on `Store` (add `use vms_domain::RecordingManifest;`):
  - `async fn save_recording(&self, manifest: &RecordingManifest) -> Result<(), StoreError>` — upsert by `recording_id`
  - `async fn recording(&self, recording_id: &str) -> Result<RecordingManifest, StoreError>` — `NotFound` when absent
  - `async fn camera_recordings(&self, camera_id: &str) -> Result<Vec<RecordingManifest>, StoreError>` — newest `started_at` first
  - `async fn expired_recordings(&self, now: DateTime<Utc>) -> Result<Vec<RecordingManifest>, StoreError>` — `delete_after` set and `<= now`
  - `async fn delete_recording(&self, recording_id: &str) -> Result<(), StoreError>`

- [ ] **Step 1: Write the failing tests**

```rust
use vms_domain::{RecordingManifest, RecordingObject};

fn manifest(id: &str, camera_id: &str, delete_after: Option<chrono::DateTime<Utc>>) -> RecordingManifest {
    let now = Utc::now();
    RecordingManifest {
        recording_id: id.into(),
        camera_id: camera_id.into(),
        gateway_id: "gw-1".into(),
        started_at: now - Duration::seconds(10),
        ended_at: now,
        codec: "avc1.640028".into(),
        width: 1920,
        height: 1080,
        init: RecordingObject {
            storage_plugin_id: "storage-s3".into(),
            object_ref: format!("{id}/init.mp4"),
            object_key: format!("{id}/init.mp4"),
            content_type: "video/mp4".into(),
            size_bytes: 1024,
        },
        segments: vec![],
        delete_after,
    }
}

#[tokio::test]
async fn a_saved_recording_survives_a_restart() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let url = format!("sqlite:{}", file.path().display());
    {
        let store = SqliteStore::connect(&url).await.unwrap();
        store.save_recording(&manifest("rec-1", "cam-1", None)).await.unwrap();
    }
    let reopened = SqliteStore::connect(&url).await.unwrap();
    let loaded = reopened.recording("rec-1").await.expect("recording survives");
    assert_eq!(loaded.init.object_ref, "rec-1/init.mp4");
    assert!(matches!(reopened.recording("rec-none").await, Err(StoreError::NotFound)));
}

#[tokio::test]
async fn the_timeline_is_per_camera_and_newest_first() {
    let store = SqliteStore::in_memory().await.unwrap();
    let mut older = manifest("rec-old", "cam-1", None);
    older.started_at = Utc::now() - Duration::hours(2);
    store.save_recording(&older).await.unwrap();
    store.save_recording(&manifest("rec-new", "cam-1", None)).await.unwrap();
    store.save_recording(&manifest("rec-other", "cam-2", None)).await.unwrap();
    let timeline = store.camera_recordings("cam-1").await.unwrap();
    let ids: Vec<_> = timeline.iter().map(|r| r.recording_id.as_str()).collect();
    assert_eq!(ids, vec!["rec-new", "rec-old"]);
}

#[tokio::test]
async fn expiry_returns_exactly_the_overdue_manifests() {
    let store = SqliteStore::in_memory().await.unwrap();
    let now = Utc::now();
    store.save_recording(&manifest("rec-overdue", "cam-1", Some(now - Duration::minutes(1)))).await.unwrap();
    store.save_recording(&manifest("rec-later", "cam-1", Some(now + Duration::hours(1)))).await.unwrap();
    store.save_recording(&manifest("rec-keep-forever", "cam-1", None)).await.unwrap();
    let expired = store.expired_recordings(now).await.unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].recording_id, "rec-overdue");
    store.delete_recording("rec-overdue").await.unwrap();
    assert!(store.expired_recordings(now).await.unwrap().is_empty());
    assert!(matches!(store.recording("rec-overdue").await, Err(StoreError::NotFound)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -20`
Expected: compile errors — missing methods.

- [ ] **Step 3: Implement**

Manifest stored whole as JSON, query columns lifted out:

```rust
    async fn save_recording(&self, manifest: &RecordingManifest) -> Result<(), StoreError> {
        let json = serde_json::to_string(manifest)
            .map_err(|err| StoreError::Internal(err.into()))?;
        sqlx::query(
            "INSERT INTO recordings (id, camera_id, started_at, ended_at, delete_after, codec, manifest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET camera_id = excluded.camera_id,
                 started_at = excluded.started_at, ended_at = excluded.ended_at,
                 delete_after = excluded.delete_after, codec = excluded.codec,
                 manifest = excluded.manifest",
        )
        .bind(&manifest.recording_id)
        .bind(&manifest.camera_id)
        .bind(ts(&manifest.started_at))
        .bind(ts(&manifest.ended_at))
        .bind(manifest.delete_after.as_ref().map(ts))
        .bind(&manifest.codec)
        .bind(json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

Reads parse the `manifest` column back with `serde_json::from_str` (a `manifest_from_row` helper returning `Result<RecordingManifest, StoreError>`):
- `recording`: `SELECT manifest FROM recordings WHERE id = ?1`, `fetch_optional`, `NotFound` on none.
- `camera_recordings`: `... WHERE camera_id = ?1 ORDER BY started_at DESC`.
- `expired_recordings`: `... WHERE delete_after IS NOT NULL AND delete_after <= ?1`.
- `delete_recording`: `DELETE FROM recordings WHERE id = ?1` (deleting an absent row is Ok, not NotFound — the retention loop may race itself).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vms-api store::sqlite 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/store
git commit -m "Recording manifests persist as JSON rows with indexed expiry"
```

---

### Task 6: Wire the store into AppState — enrollment and auth handlers

**Files:**
- Modify: `services/api/src/main.rs`

**Interfaces:**
- Consumes: `Arc<dyn Store>`, `SqliteStore::{connect, in_memory}`, `StoreError`, trait methods from Tasks 2–3.
- Produces: `AppState.store: Arc<dyn crate::store::Store>`; fields `enrollments` and `gateway_tokens` REMOVED (struct `Enrollment` deleted); `authorized_gateway` consults the store; a helper `fn store_status(err: StoreError) -> StatusCode` mapping NotFound→404, Gone→410, Internal→500.

- [ ] **Step 1: Write the failing test**

Add to the router tests in `main.rs`:

```rust
    #[tokio::test]
    async fn enrollment_and_gateway_token_live_in_the_store_not_in_memory() {
        // Two AppStates sharing one store simulate an API restart: the second
        // state has empty in-memory maps but the same database.
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("store"),
        );
        let state = test_state_with(store.clone()).await;

        let (_, created) = send(
            &state,
            post(
                "/api/v1/enrollments",
                None,
                serde_json::json!({
                    "customer_id": "cust-1", "customer_name": "Customer",
                    "site_id": "site-1", "site_name": "Site", "city": "Barcelona",
                }),
            ),
        )
        .await;
        let enrollment_token = created["enrollment_token"].as_str().unwrap().to_owned();
        let (status, enrolled) = send(
            &state,
            post(
                "/api/v1/gateways/enroll",
                None,
                serde_json::json!({
                    "enrollment_token": enrollment_token,
                    "gateway_id": "gw-1", "hostname": "edge-1", "version": "0.1.0",
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let gateway_token = enrolled["gateway_token"].as_str().unwrap().to_owned();

        // "Restart": a fresh AppState over a freshly opened store on the same file.
        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let (status, _) = send(
            &restarted,
            post(
                "/api/v1/cameras/telemetry",
                Some(&gateway_token),
                telemetry_batch("gw-1"),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "an enrolled gateway must survive an API restart without re-enrolling"
        );
    }
```

- [ ] **Step 2: Refactor test_state, run to verify failure**

Split `test_state` so the new test compiles and every existing test keeps its one-liner:

```rust
    async fn test_state() -> AppState {
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::in_memory().await.expect("in-memory store"),
        );
        test_state_with(store).await
    }

    async fn test_state_with(store: Arc<dyn crate::store::Store>) -> AppState {
        // as today's test_state, minus the three removed maps, plus `store`
        ...
    }
```

Run: `cargo test -p vms-api 2>&1 | tail -20`
Expected: compile error — `AppState` has no `store` field yet.

- [ ] **Step 3: Implement**

In `AppState`: remove `enrollments`, `gateway_tokens`, `recordings` is Task 8 (leave it this task); add `store: Arc<dyn crate::store::Store>`. Delete `struct Enrollment`. In `main()`:

```rust
    let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:data/vms.db".into());
    if let Some(path) = database_url.strip_prefix("sqlite:")
        && let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let store: Arc<dyn store::Store> = Arc::new(store::SqliteStore::connect(&database_url).await?);
    info!(%database_url, "fleet store open");
```

(`main` already returns `anyhow::Result`; a failed open or migration refuses to start — a half-up API that forgot its fleet is worse than a crash loop.)

Error mapping helper near the handlers:

```rust
fn store_status(err: crate::store::StoreError) -> StatusCode {
    match err {
        crate::store::StoreError::NotFound => StatusCode::NOT_FOUND,
        crate::store::StoreError::Gone => StatusCode::GONE,
        crate::store::StoreError::Internal(err) => {
            warn!(error = %err, "store failure");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
```

Handler ports:
- `create_enrollment` → returns `Result<Json<EnrollmentCreated>, StatusCode>`; after generating the token: `state.store.create_enrollment(&enrollment_token, &request, expires_at).await.map_err(store_status)?;`
- `gateway_enroll` → same three-phase order as today so an entitlement outage does not burn the token:
  1. `state.store.enrollment_request(&request.enrollment_token, Utc::now()).await.map_err(store_status)?`
  2. entitlement resolve (unchanged, 503 on failure)
  3. `state.store.claim_enrollment(...).await.map_err(store_status)?` then mint the token and `state.store.enroll_gateway(&enrollment_request, &request, &gateway_token, Utc::now()).await.map_err(store_status)?`
- `authorized_gateway` → bootstrap check unchanged, then `state.store.verify_gateway_token(gateway_id, token).await.unwrap_or(false)`.

The test at `main.rs:1520` (`a_gateway_cannot_collect_another_gateways_commands`) currently seeds `state.gateway_tokens` directly — reseed it through the store instead:

```rust
        state.store.enroll_gateway(
            &EnrollmentRequest {
                customer_id: "cust-1".into(), customer_name: "Customer".into(),
                site_id: "site-1".into(), site_name: "Site".into(), city: "Barcelona".into(),
            },
            &GatewayEnrollmentRequest {
                enrollment_token: String::new(), gateway_id: "gw-1".into(),
                hostname: "edge".into(), version: "0.1.0".into(),
            },
            "token-for-gw-1",
            Utc::now(),
        ).await.unwrap();
```

- [ ] **Step 4: Run the whole suite**

Run: `cargo test -p vms-api 2>&1 | tail -20`
Expected: everything passes, including all pre-existing enrollment/auth tests unchanged in behavior (`an_enrollment_token_cannot_be_used_twice`, `an_enrolled_gateway_token_works_and_does_not_cover_other_gateways`, ...).

- [ ] **Step 5: Commit**

```bash
git add services/api/src/main.rs
git commit -m "Enrollment and gateway tokens live in the store; restarts keep gateways enrolled"
```

---

### Task 7: Telemetry writes identity; cameras and fleet read through the store

**Files:**
- Modify: `services/api/src/main.rs`

**Interfaces:**
- Consumes: `upsert_fleet_identity`, `fleet_cameras`, `fleet_identity`, `CameraRecord`, `OrganizationRecord` (Task 4).
- Produces: `/api/v1/cameras` and `/api/v1/fleet` merge DB identity with live telemetry; a DB-known camera with no live telemetry reports `offline` instead of vanishing; demo fleet only when DB and memory are both empty.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_known_camera_reports_offline_after_a_restart_instead_of_vanishing() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("store"),
        );
        let state = test_state_with(store).await;
        with_camera(&state, "cam-1", "gw-1").await;

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("reopen"),
        );
        let restarted = test_state_with(store2).await;

        let (status, cameras) = send(&restarted, get("/api/v1/cameras")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cameras.as_array().map(Vec::len), Some(1), "the camera vanished: {cameras}");
        assert_eq!(cameras[0]["camera_id"], "cam-1");
        assert_eq!(cameras[0]["status"], "offline");

        let (_, fleet) = send(&restarted, get("/api/v1/fleet")).await;
        assert_eq!(fleet["source"], "live", "a restart must not demote the dashboard to demo data");
        assert_eq!(fleet["customers"][0]["sites"][0]["cameras"][0]["status"], "offline");
    }
```

(Check the serde rename on `FleetSource`/`HealthStatus` in `crates/domain/src/lib.rs` — if the wire form is not lowercase `"live"`/`"offline"`, use the actual wire form in the assertions.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vms-api a_known_camera_reports_offline 2>&1 | tail -10`
Expected: FAIL — cameras is an empty array (nothing persisted, nothing merged).

- [ ] **Step 3: Implement**

`camera_telemetry` handler, after the entitlement check and before the memory insert:

```rust
    if let Err(err) = state.store.upsert_fleet_identity(&batch, Utc::now()).await {
        return store_status(err);
    }
```

`cameras` handler — merge (DB is the roster, memory is the liveness):

```rust
async fn cameras(State(state): State<AppState>) -> Result<Json<Vec<CameraTelemetry>>, StatusCode> {
    let records = state.store.fleet_cameras().await.map_err(store_status)?;
    let now = Utc::now();
    let mut live: HashMap<String, CameraTelemetry> = state
        .camera_batches
        .read()
        .await
        .values()
        .flat_map(|batch| batch.cameras.clone())
        .map(|camera| (camera.camera_id.clone(), camera))
        .collect();
    let mut values: Vec<CameraTelemetry> = records
        .into_iter()
        .map(|record| match live.remove(&record.id) {
            Some(camera) => camera,
            None => offline_camera(record),
        })
        .collect();
    // A live camera the store has not caught up with yet still shows.
    values.extend(live.into_values());
    for camera in &mut values {
        if (now - camera.last_seen).num_seconds() > state.stale_camera_seconds {
            camera.status = HealthStatus::Offline;
            camera.fps = None;
            camera.bitrate_kbps = None;
            camera.last_error = Some("gateway telemetry is stale".into());
        }
    }
    values.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.camera_id.cmp(&b.camera_id)));
    Ok(Json(values))
}

fn offline_camera(record: crate::store::CameraRecord) -> CameraTelemetry {
    CameraTelemetry {
        camera_id: record.id,
        gateway_id: record.gateway_id,
        site_id: record.site_id,
        name: record.name,
        status: HealthStatus::Offline,
        manufacturer: record.manufacturer,
        model: record.model,
        firmware: record.firmware,
        profile_name: None,
        codec: record.codec,
        width: record.width,
        height: record.height,
        fps: None,
        bitrate_kbps: None,
        packet_loss: 0,
        reconnects: 0,
        rtsp_endpoint: None,
        last_seen: record.last_seen,
        last_error: Some("no telemetry since the API restarted".into()),
    }
}
```

(The stale-check loop now runs for DB-offline cameras too; their `last_seen` is old so they simply stay offline — same outcome, no special case.)

`fleet` handler — identity from the store, status from memory:

```rust
async fn fleet(State(state): State<AppState>) -> Result<Json<FleetSnapshot>, StatusCode> {
    let orgs = state.store.fleet_identity().await.map_err(store_status)?;
    let batches = state.camera_batches.read().await;
    if orgs.is_empty() {
        if batches.values().any(|batch| !batch.cameras.is_empty()) {
            return Ok(Json(live_fleet(
                batches.values().cloned().collect(),
                state.stale_camera_seconds,
            )));
        }
        return Ok(Json(demo_fleet()));
    }
    let now = Utc::now();
    let live: HashMap<String, CameraTelemetry> = batches
        .values()
        .flat_map(|batch| batch.cameras.clone())
        .map(|camera| (camera.camera_id.clone(), camera))
        .collect();
    let customers = orgs
        .into_iter()
        .map(|org| CustomerSummary {
            id: org.id.clone(),
            name: org.name,
            sites: org
                .sites
                .into_iter()
                .map(|site| SiteSummary {
                    id: site.id,
                    customer_id: org.id.clone(),
                    name: site.name,
                    city: site.city,
                    cameras: site
                        .cameras
                        .into_iter()
                        .map(|record| {
                            let (status, fps, bitrate, last_seen) = match live.get(&record.id) {
                                Some(camera)
                                    if (now - camera.last_seen).num_seconds()
                                        <= state.stale_camera_seconds =>
                                {
                                    (camera.status.clone(), camera.fps, camera.bitrate_kbps, camera.last_seen)
                                }
                                _ => (HealthStatus::Offline, None, None, record.last_seen),
                            };
                            CameraSummary {
                                id: record.id,
                                name: record.name,
                                site_id: record.site_id,
                                status,
                                fps,
                                bitrate_kbps: bitrate,
                                last_seen,
                            }
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    Ok(Json(FleetSnapshot {
        generated_at: Utc::now(),
        source: FleetSource::Live,
        customers,
    }))
}
```

(If `HealthStatus` is `Copy`, drop the `.clone()`. The old `live_fleet` stays — it is still the fallback when memory has batches the store missed.)

- [ ] **Step 4: Run the whole suite**

Run: `cargo test -p vms-api 2>&1 | tail -20`
Expected: all pass. Watch `the_shared_token_admits_any_gateway_id` — it posts telemetry, which now also writes the store; it must still pass unmodified.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/main.rs
git commit -m "The fleet roster comes from the store; silent cameras show offline, not absent"
```

---

### Task 8: Recordings and retention through the store

**Files:**
- Modify: `services/api/src/main.rs`

**Interfaces:**
- Consumes: recording methods from Task 5.
- Produces: `AppState.recordings` map REMOVED; `gateway_complete_command` persists manifests; `camera_timeline`/`recording_playback` read the store; `retention_pass(&AppState)` extracted from `retention_loop` (loop calls it every 60s).

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    async fn a_completed_recording_survives_a_restart_and_appears_on_the_timeline() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("store"),
        );
        let state = test_state_with(store).await;
        with_camera(&state, "cam-1", "gw-1").await;

        let (_, accepted) = send(
            &state,
            post("/api/v1/cameras/cam-1/recordings", None,
                 serde_json::json!({"duration_seconds": 10, "segment_seconds": 2})),
        )
        .await;
        let command_id = accepted["command_id"].as_str().unwrap().to_owned();
        // Collect it so completion is legal, then complete with a manifest.
        let (_, _) = send(
            &state,
            Request::builder()
                .uri("/api/v1/gateways/gw-1/commands/next")
                .header("authorization", format!("Bearer {SHARED_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let now = chrono::Utc::now();
        let (status, _) = send(
            &state,
            post(
                &format!("/api/v1/gateways/gw-1/commands/{command_id}/complete"),
                Some(SHARED_TOKEN),
                serde_json::json!({
                    "command_id": command_id, "gateway_id": "gw-1",
                    "status": "succeeded", "completed_at": now, "error": null,
                    "live": null, "analysis": null,
                    "recording": {
                        "recording_id": "rec-1", "camera_id": "cam-1", "gateway_id": "gw-1",
                        "started_at": now, "ended_at": now,
                        "codec": "avc1.640028", "width": 1920, "height": 1080,
                        "init": {"storage_plugin_id": "storage-s3", "object_ref": "rec-1/init.mp4",
                                  "object_key": "rec-1/init.mp4", "content_type": "video/mp4", "size_bytes": 1024},
                        "segments": [], "delete_after": null,
                    },
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let (status, timeline) = send(&restarted, get("/api/v1/cameras/cam-1/recordings")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            timeline["recordings"][0]["recording_id"], "rec-1",
            "a recording made before the restart must still be on the timeline"
        );
    }

    #[tokio::test]
    async fn retention_keeps_the_manifest_when_storage_delete_fails() {
        // The storage plugin registry is empty under test, so every delete
        // fails — after a retention pass the row must still be there, or a
        // transient storage outage would orphan objects forever.
        let state = test_state().await;
        let mut expired = serde_json::from_value::<vms_domain::RecordingManifest>(serde_json::json!({
            "recording_id": "rec-exp", "camera_id": "cam-1", "gateway_id": "gw-1",
            "started_at": chrono::Utc::now(), "ended_at": chrono::Utc::now(),
            "codec": "avc1.640028", "width": 1920, "height": 1080,
            "init": {"storage_plugin_id": "storage-s3", "object_ref": "rec-exp/init.mp4",
                      "object_key": "rec-exp/init.mp4", "content_type": "video/mp4", "size_bytes": 1},
            "segments": [], "delete_after": null,
        })).unwrap();
        expired.delete_after = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
        state.store.save_recording(&expired).await.unwrap();

        retention_pass(&state).await;

        assert!(
            state.store.recording("rec-exp").await.is_ok(),
            "retention deleted the manifest although storage still holds the objects"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vms-api 2>&1 | tail -20`
Expected: compile errors — `retention_pass` does not exist; then (after stubs) the timeline test fails because recordings still live in the map.

- [ ] **Step 3: Implement**

- Remove `recordings` from `AppState` (and both initializers).
- `gateway_complete_command`: replace the map insert with `if state.store.save_recording(recording).await.is_err() { return StatusCode::INTERNAL_SERVER_ERROR; }` (after setting `delete_after` exactly as today).
- `camera_timeline` → `Result<Json<RecordingTimeline>, StatusCode>` using `state.store.camera_recordings(&camera_id).await.map_err(store_status)?` (already newest-first from the store; drop the local sort).
- `recording_playback`: `let recording = state.store.recording(&recording_id).await.map_err(store_status)?;` — rest unchanged.
- Extract the body of `retention_loop`'s tick into `async fn retention_pass(state: &AppState)`: `state.store.expired_recordings(Utc::now())` → try the storage deletes exactly as today → on full success `state.store.delete_recording(&recording.recording_id)`. `retention_loop` becomes the interval loop calling `retention_pass(&state).await`.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test -p vms-api 2>&1 | tail -20`
Expected: all pass. Then the full workspace: `cargo test --workspace 2>&1 | tail -5` and `cargo clippy -p vms-api 2>&1 | tail -10` — no errors, no new warnings.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/main.rs
git commit -m "Recordings and retention read and write the store; restarts lose nothing"
```

---

### Task 9: Compose wiring and docs

**Files:**
- Modify: `docker-compose.yml`
- Modify: `docs/RUNNING-LOCALLY.md`
- Modify: `docs/BACKLOG.md`

**Interfaces:**
- Consumes: `DATABASE_URL` handling from Task 6.
- Produces: a named volume so the compose deployment actually keeps the database.

- [ ] **Step 1: Compose changes**

In `docker-compose.yml`, api service: add to `environment:`

```yaml
      DATABASE_URL: sqlite:/data/vms.db
```

add to its `volumes:`

```yaml
      - api-data:/data
```

and at the bottom, next to `minio-data:`:

```yaml
  api-data:
```

- [ ] **Step 2: Verify**

Run: `docker compose config 2>&1 | grep -A2 -B2 "DATABASE_URL\|api-data"` — the merged config (including the local override file) must show the env var and the volume on the api service. If Docker is unavailable in the execution environment, state that explicitly in the task report instead of claiming verification.

Also verify the binary end-to-end without Docker:

```bash
DATABASE_URL=sqlite:/tmp/vms-smoke.db cargo run -p vms-api &
sleep 2
curl -s localhost:8080/healthz
ls -la /tmp/vms-smoke.db   # the file exists and is non-empty
kill %1; rm -f /tmp/vms-smoke.db
```

(`API_BIND` may need a free port on this machine — 8080 is taken locally; use `API_BIND=0.0.0.0:8099` and curl 8099.)

- [ ] **Step 3: Docs**

- `docs/RUNNING-LOCALLY.md`: a short section — where the database lives (`data/vms.db` bare, `api-data` volume under compose), that `DATABASE_URL` moves it, and that backup is copying one file while the API is stopped.
- `docs/BACKLOG.md`: under "Next — make the demo sellable on real sites", change the line to:

```markdown
- [x] Persistent model for organizations, sites, gateways and cameras — done as SQLite behind a `Store` trait (spec: `docs/superpowers/specs/2026-09-06-persistent-fleet-store-design.md`); Postgres becomes a second `Store` implementation when hosted scale calls for it
```

- [ ] **Step 4: Commit**

```bash
git add docker-compose.yml docs/RUNNING-LOCALLY.md docs/BACKLOG.md
git commit -m "Give the API a persistent volume and write down where the database lives"
```

---

## Self-review notes (already applied)

- Spec coverage: enrollment atomicity (T2), hashed tokens (T2/T3), restart survival (T3/T5/T6/T8), offline-not-vanished (T7), demo only when both empty (T7), retention via store with fail-safe (T8), `gateways.last_seen` writer (T4), startup refusal (T6), compose volume (T9). The spec's "index on `sites(org_id)`/`cameras(gateway_id)`" is in the migration (T1).
- Deviation from spec, justified inline: `gateways.enrolled_at` and `token_hash` handle the never-enrolled bootstrap gateway (NULL / `''`) — the spec schema sketch did not cover that row existing at all, but `upsert_fleet_identity` + the FK on `cameras.gateway_id` require it.
- Type consistency: `Store` method names used in Tasks 6–8 match Tasks 2–5 definitions; `test_state_with` introduced in T6 is what T7/T8 tests use.
