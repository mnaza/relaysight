# Gateway Revocation and Audit Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Revoke a gateway from the dashboard — every credential for that id refused, bootstrap secret included, with a fresh admin-issued enrollment as the un-revoke — and keep a durable audit trail of security events at `GET /api/v1/audit`.

**Architecture:** Migration `0004_revocation_audit.sql` adds a `revoked_at` tombstone to `gateways` and an `audit_log` table. `authorized_gateway` checks the tombstone before the shared-token path. `GET /api/v1/gateways` becomes roster-merged (`GatewayView` with an embedded optional live heartbeat), the dashboard grid gains a Revoke button and revoked badge, and handlers write fire-and-forget audit rows.

**Tech Stack:** Rust (axum 0.8, sqlx 0.9 runtime queries), vanilla-JS SPA with `node --test` + jsdom. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-10-revocation-audit-design.md`

## Global Constraints

- No production code without a failing test first; run the test and watch it fail before implementing.
- The repo-root `Cargo.toml` carries a local uncommitted `[patch.crates-io]` retina patch. **In any fresh worktree, append it FIRST, before any cargo command** (path `/home/andrey/work/retina-fork`); never stage `Cargo.toml`, never `git add .`. `Cargo.lock` must not change (no new deps) — if it does, stop and investigate. NOTE: `target/` was recently deleted to free disk, so the first build is cold (~minutes); that is expected.
- Tests: `cargo test -p vms-api` (77 passing at base). Web: `npm test` in `web/` (67 passing; `npm install` once in a fresh worktree).
- Timestamps via `ts()`/`parse_ts()`; runtime sqlx only; migrations via `sqlx::migrate!` (picks up 0004 automatically).
- Audit rows NEVER contain a password, token, or enrollment token. Audit writes are fire-and-forget: a failed insert `warn!`s and never fails the request.
- Action strings are exactly: `login.ok`, `login.failed`, `password.changed`, `password.reset`, `enrollment.created`, `gateway.enrolled`, `gateway.revoked`. Actors: `admin`, `gateway:<id>`, `system`.
- New routes go in the **protected** router group AND the `PROTECTED_ROUTES` test table: `("POST", "/api/v1/gateways/gw-1/revoke")` and `("GET", "/api/v1/audit")`.
- `AUDIT_RETENTION_DAYS` default **0 = keep forever**; pruning only when > 0.
- The pinned test `the_shared_token_admits_any_gateway_id` must stay green — revocation changes behavior only for revoked ids.

## File Structure

- `services/api/migrations/0004_revocation_audit.sql` — create.
- `crates/domain/src/lib.rs` — modify: add `GatewayView`, `AuditView`.
- `services/api/src/store/mod.rs` — modify: 6 new trait methods; `enroll_gateway` doc note.
- `services/api/src/store/sqlite.rs` — modify: impls + `enroll_gateway` un-revoke + store tests.
- `services/api/src/main.rs` — modify: `authorized_gateway`, `audit` helper, revoke + audit handlers, `gateways` handler rewrite, routes, retention, `AppState.audit_retention_days`, tests.
- `services/api/src/auth.rs` — modify: audit hooks in login/change-password/seed.
- `web/dashboard-app.js` — modify: `renderGateways` for `GatewayView` + revoke button.
- `web/locales/{en,es,ru}.json` — modify: 3 new keys.
- `web/tests/gateways.test.mjs` — create.
- `docs/RUNNING-LOCALLY.md`, `docs/BACKLOG.md` — modify (Task 5).

---

### Task 1: Storage — tombstone, audit table, roster view

**Files:**
- Create: `services/api/migrations/0004_revocation_audit.sql`
- Modify: `crates/domain/src/lib.rs` (two types, after `IncidentView`)
- Modify: `services/api/src/store/mod.rs` (trait; add `AuditView, GatewayView, GatewayHeartbeat` to the `vms_domain` use list as needed)
- Modify: `services/api/src/store/sqlite.rs` (impls + tests; extend its `vms_domain` use list too)

**Interfaces:**
- Produces (later tasks call these exact signatures on `dyn Store`):
  - `revoke_gateway(gateway_id: &str, now: DateTime<Utc>) -> Result<(), StoreError>` (NotFound for unknown id)
  - `gateway_revoked(gateway_id: &str) -> Result<bool, StoreError>` (false for unknown)
  - `gateway_views() -> Result<Vec<GatewayView>, StoreError>` (roster; `online: false`, `heartbeat: None` — the handler merges liveness)
  - `record_audit(at: DateTime<Utc>, actor: &str, action: &str, subject: &str, detail: Option<&str>) -> Result<(), StoreError>`
  - `audit_entries(limit: i64) -> Result<Vec<AuditView>, StoreError>` (newest first)
  - `delete_audit_before(cutoff: DateTime<Utc>) -> Result<(), StoreError>`
  - `vms_domain::GatewayView { gateway_id, site_id, site_name, customer_name: String, hostname: Option<String>, version: Option<String>, enrolled: bool, revoked_at: Option<DateTime<Utc>>, last_seen: Option<DateTime<Utc>>, online: bool, heartbeat: Option<GatewayHeartbeat> }`
  - `vms_domain::AuditView { at: DateTime<Utc>, actor: String, action: String, subject: String, detail: Option<String> }`

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `services/api/src/store/sqlite.rs`:

```rust
    #[tokio::test]
    async fn revoking_kills_the_token_and_a_fresh_enrollment_clears_it() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.enroll_gateway(&request(), &enroll_req("gw-1"), "tok-old", Utc::now()).await.unwrap();
        assert!(store.verify_gateway_token("gw-1", "tok-old").await.unwrap());
        assert!(!store.gateway_revoked("gw-1").await.unwrap());

        store.revoke_gateway("gw-1", Utc::now()).await.unwrap();
        assert!(store.gateway_revoked("gw-1").await.unwrap());
        assert!(
            !store.verify_gateway_token("gw-1", "tok-old").await.unwrap(),
            "a revoked gateway's token must stop working"
        );
        assert!(
            !store.verify_any_gateway_token("tok-old").await.unwrap(),
            "the cleared hash must not match the any-gateway check either"
        );

        // A fresh admin-issued enrollment is the un-revoke.
        store.enroll_gateway(&request(), &enroll_req("gw-1"), "tok-new", Utc::now()).await.unwrap();
        assert!(!store.gateway_revoked("gw-1").await.unwrap());
        assert!(store.verify_gateway_token("gw-1", "tok-new").await.unwrap());

        // Unknown ids: revoking is NotFound, the check is a calm false.
        assert!(matches!(store.revoke_gateway("gw-never", Utc::now()).await, Err(StoreError::NotFound)));
        assert!(!store.gateway_revoked("gw-never").await.unwrap());
    }

    #[tokio::test]
    async fn the_gateway_roster_carries_names_flags_and_revocation() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.enroll_gateway(&request(), &enroll_req("gw-1"), "tok-1", Utc::now()).await.unwrap();
        store.upsert_fleet_identity(&batch_with_camera("gw-boot", "cam-1", "Entrance"), Utc::now()).await.unwrap();
        store.revoke_gateway("gw-1", Utc::now()).await.unwrap();

        let views = store.gateway_views().await.unwrap();
        assert_eq!(views.len(), 2);
        let gw1 = views.iter().find(|v| v.gateway_id == "gw-1").unwrap();
        assert_eq!(gw1.site_name, "Site");
        assert_eq!(gw1.customer_name, "Customer");
        assert!(gw1.revoked_at.is_some());
        assert!(!gw1.enrolled, "a revoked gateway has no working token");
        assert!(!gw1.online && gw1.heartbeat.is_none(), "the store never claims liveness");
        let boot = views.iter().find(|v| v.gateway_id == "gw-boot").unwrap();
        assert!(!boot.enrolled && boot.revoked_at.is_none());
    }

    #[tokio::test]
    async fn audit_rows_insert_list_newest_first_and_prune_by_cutoff() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store.record_audit(now - Duration::days(400), "system", "password.reset", "", None).await.unwrap();
        store.record_audit(now - Duration::minutes(5), "admin", "login.failed", "", None).await.unwrap();
        store.record_audit(now, "admin", "gateway.revoked", "gw-1", Some("from the dashboard")).await.unwrap();

        let entries = store.audit_entries(10).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].action, "gateway.revoked");
        assert_eq!(entries[0].subject, "gw-1");
        assert_eq!(entries[0].detail.as_deref(), Some("from the dashboard"));
        assert_eq!(entries[2].action, "password.reset");

        store.delete_audit_before(now - Duration::days(365)).await.unwrap();
        let entries = store.audit_entries(10).await.unwrap();
        assert_eq!(entries.len(), 2, "only the 400-day-old row goes");
        assert_eq!(store.audit_entries(1).await.unwrap().len(), 1, "the limit caps the page");
    }
```

Also extend `connect_applies_migrations`'s expected-tables list with `"audit_log"`.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api -- revoking_kills` — expected: compile error, `revoke_gateway` not found. (Cold build after the `target/` purge — the first compile takes minutes.)

- [ ] **Step 3: Write the migration** — `services/api/migrations/0004_revocation_audit.sql`:

```sql
-- Gateway revocation and the audit trail.
-- See docs/superpowers/specs/2026-09-10-revocation-audit-design.md.

ALTER TABLE gateways ADD COLUMN revoked_at TEXT;  -- NULL = not revoked

CREATE TABLE audit_log (
    id      TEXT PRIMARY KEY,   -- uuid
    at      TEXT NOT NULL,      -- fixed-width RFC 3339, as everywhere
    actor   TEXT NOT NULL,      -- 'admin' | 'gateway:<id>' | 'system'
    action  TEXT NOT NULL,      -- fixed strings; see the spec
    subject TEXT NOT NULL,      -- what it acted on ('' when none)
    detail  TEXT                -- human context; never a password or token
);
CREATE INDEX idx_audit_at ON audit_log(at);
```

- [ ] **Step 4: Add the domain types** — in `crates/domain/src/lib.rs`, after `IncidentView`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayView {
    pub gateway_id: String,
    pub site_id: String,
    pub site_name: String,
    pub customer_name: String,
    pub hostname: Option<String>,
    pub version: Option<String>,
    /// Holds a working token right now. Revoked gateways are not enrolled.
    pub enrolled: bool,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub online: bool,
    /// The live report, when the gateway has heartbeated this process.
    pub heartbeat: Option<GatewayHeartbeat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditView {
    pub at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub subject: String,
    pub detail: Option<String>,
}
```

- [ ] **Step 5: Extend the trait** — append to `trait Store` in `store/mod.rs`:

```rust
    /// Set the tombstone and clear the token: every bearer credential for
    /// this id is refused from now on, the bootstrap secret included.
    /// NotFound when the id has never been in the roster.
    async fn revoke_gateway(&self, gateway_id: &str, now: DateTime<Utc>) -> Result<(), StoreError>;

    /// False for unknown ids.
    async fn gateway_revoked(&self, gateway_id: &str) -> Result<bool, StoreError>;

    /// The store's half of the gateways screen: every known gateway with
    /// joined names and flags. `online` and `heartbeat` are the handler's to
    /// fill — the store never claims liveness.
    async fn gateway_views(&self) -> Result<Vec<GatewayView>, StoreError>;

    /// One audit row. Callers treat failure as loggable, never fatal.
    async fn record_audit(
        &self,
        at: DateTime<Utc>,
        actor: &str,
        action: &str,
        subject: &str,
        detail: Option<&str>,
    ) -> Result<(), StoreError>;

    /// Newest first.
    async fn audit_entries(&self, limit: i64) -> Result<Vec<AuditView>, StoreError>;

    /// Pruning, only ever called with AUDIT_RETENTION_DAYS > 0.
    async fn delete_audit_before(&self, cutoff: DateTime<Utc>) -> Result<(), StoreError>;
```

And on `enroll_gateway`'s doc comment add the line: `A fresh enrollment also clears a revocation — the admin-issued token is the un-revoke.`

- [ ] **Step 6: Implement in `SqliteStore`.** In `enroll_gateway`, extend the gateways upsert (both arms) to clear the tombstone — the INSERT column list gains `revoked_at` with an explicit `NULL`, and the conflict arm gains `revoked_at = NULL`:

```rust
        sqlx::query(
            "INSERT INTO gateways (id, site_id, hostname, version, token_hash, enrolled_at, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)
             ON CONFLICT(id) DO UPDATE SET site_id = excluded.site_id,
                 hostname = excluded.hostname, version = excluded.version,
                 token_hash = excluded.token_hash, enrolled_at = excluded.enrolled_at,
                 revoked_at = NULL",
        )
```

Then append the new methods inside `impl Store for SqliteStore`:

```rust
    async fn revoke_gateway(&self, gateway_id: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        let result = sqlx::query("UPDATE gateways SET revoked_at = ?2, token_hash = '' WHERE id = ?1")
            .bind(gateway_id)
            .bind(ts(&now))
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn gateway_revoked(&self, gateway_id: &str) -> Result<bool, StoreError> {
        let revoked: Option<Option<String>> =
            sqlx::query_scalar("SELECT revoked_at FROM gateways WHERE id = ?1")
                .bind(gateway_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(revoked.flatten().is_some())
    }

    async fn gateway_views(&self) -> Result<Vec<GatewayView>, StoreError> {
        let rows = sqlx::query(
            "SELECT g.id, g.site_id,
                    COALESCE(s.name, '') AS site_name,
                    COALESCE(o.name, '') AS customer_name,
                    g.hostname, g.version,
                    (g.token_hash != '') AS enrolled,
                    g.revoked_at, g.last_seen
             FROM gateways g
             LEFT JOIN sites s ON s.id = g.site_id
             LEFT JOIN organizations o ON o.id = s.org_id
             ORDER BY g.id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(GatewayView {
                    gateway_id: row.get("id"),
                    site_id: row.get("site_id"),
                    site_name: row.get("site_name"),
                    customer_name: row.get("customer_name"),
                    hostname: row.get("hostname"),
                    version: row.get("version"),
                    enrolled: row.get::<i64, _>("enrolled") != 0,
                    revoked_at: row
                        .get::<Option<String>, _>("revoked_at")
                        .map(|v| parse_ts(&v))
                        .transpose()?,
                    last_seen: row
                        .get::<Option<String>, _>("last_seen")
                        .map(|v| parse_ts(&v))
                        .transpose()?,
                    online: false,
                    heartbeat: None,
                })
            })
            .collect()
    }

    async fn record_audit(
        &self,
        at: DateTime<Utc>,
        actor: &str,
        action: &str,
        subject: &str,
        detail: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO audit_log (id, at, actor, action, subject, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(ts(&at))
        .bind(actor)
        .bind(action)
        .bind(subject)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn audit_entries(&self, limit: i64) -> Result<Vec<AuditView>, StoreError> {
        let rows = sqlx::query(
            "SELECT at, actor, action, subject, detail FROM audit_log
             ORDER BY at DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(AuditView {
                    at: parse_ts(row.get::<String, _>("at").as_str())?,
                    actor: row.get("actor"),
                    action: row.get("action"),
                    subject: row.get("subject"),
                    detail: row.get("detail"),
                })
            })
            .collect()
    }

    async fn delete_audit_before(&self, cutoff: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM audit_log WHERE at < ?1")
            .bind(ts(&cutoff))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

- [ ] **Step 7: Run the tests and make sure they pass**

Run: `cargo test -p vms-api store::` — all store tests pass, three new included. (`dead_code` warnings on the unused-until-later methods are staging; do not silence.)

- [ ] **Step 8: Commit**

```bash
git add services/api/migrations/0004_revocation_audit.sql crates/domain/src/lib.rs services/api/src/store/mod.rs services/api/src/store/sqlite.rs
git commit -m "Revocation tombstone, audit table, and a store-backed gateway roster"
```

---

### Task 2: Revocation enforced, roster served

**Files:**
- Modify: `services/api/src/main.rs` (`authorized_gateway`, revoke handler, `gateways` handler rewrite, routes, `PROTECTED_ROUTES`, imports `GatewayView` from vms_domain, tests)

**Interfaces:**
- Consumes: Task 1 store methods; existing `bearer_token`, `store_status`, test helpers (`seed_admin`, `login_cookie`, `send`, `post`, `get`, `telemetry_batch`, `SHARED_TOKEN`).
- Produces: `POST /api/v1/gateways/{gateway_id}/revoke` (204/404); `GET /api/v1/gateways` returning `Vec<GatewayView>`.

- [ ] **Step 1: Write the failing tests** — main.rs tests module:

```rust
    /// Enroll gw-1 end to end and return its per-gateway token.
    async fn enrolled_gateway_token(state: &AppState, cookie: &str) -> String {
        let mut request = post(
            "/api/v1/enrollments",
            None,
            serde_json::json!({
                "customer_id": "cust-1", "customer_name": "Customer",
                "site_id": "site-1", "site_name": "Site", "city": "Barcelona",
            }),
        );
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (_, created) = send(state, request).await;
        let enrollment_token = created["enrollment_token"].as_str().unwrap().to_owned();
        let (_, enrolled) = send(
            state,
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
        enrolled["gateway_token"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn a_revoked_gateway_is_refused_every_credential_until_it_reenrolls() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let gateway_token = enrolled_gateway_token(&state, &cookie).await;

        // Sanity: both credentials work before the revoke.
        let (status, _) = send(&state, post("/api/v1/cameras/telemetry", Some(&gateway_token), telemetry_batch("gw-1"))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = send(&state, post("/api/v1/cameras/telemetry", Some(SHARED_TOKEN), telemetry_batch("gw-1"))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _) = send(&state, post("/api/v1/cameras/telemetry", Some(&gateway_token), telemetry_batch("gw-1"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "the revoked token still worked");
        let (status, _) = send(&state, post("/api/v1/cameras/telemetry", Some(SHARED_TOKEN), telemetry_batch("gw-1"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "revocation must beat the bootstrap secret");

        // A fresh admin-issued enrollment is the un-revoke.
        let new_token = enrolled_gateway_token(&state, &cookie).await;
        let (status, _) = send(&state, post("/api/v1/cameras/telemetry", Some(&new_token), telemetry_batch("gw-1"))).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "re-enrollment must restore access");

        // Revoking an unknown gateway is a 404, not a silent success.
        let mut request = post("/api/v1/gateways/gw-nope/revoke", None, serde_json::json!({}));
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, _) = send(&state, request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_revocation_survives_a_restart() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("store"),
        );
        let state = test_state_with(store).await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let gateway_token = enrolled_gateway_token(&state, &cookie).await;
        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        send(&state, request).await;

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let (status, _) = send(&restarted, post("/api/v1/cameras/telemetry", Some(&gateway_token), telemetry_batch("gw-1"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a restart must not resurrect a revoked gateway");
        let (status, _) = send(&restarted, post("/api/v1/cameras/telemetry", Some(SHARED_TOKEN), telemetry_batch("gw-1"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_gateways_screen_shows_the_roster_not_just_the_loud() {
        let state = test_state().await;
        seed_admin(&state).await;
        let cookie = login_cookie(&state).await;
        let _ = enrolled_gateway_token(&state, &cookie).await;  // gw-1, enrolled, silent
        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        send(&state, request).await;

        // gw-live heartbeats but is not in the store roster.
        let (status, _) = send(
            &state,
            post(
                "/api/v1/gateways/heartbeat",
                Some(SHARED_TOKEN),
                serde_json::json!({
                    "gateway_id": "gw-live", "site_id": "site-9", "hostname": "edge-9",
                    "version": "0.1.0", "uptime_seconds": 60, "cpu_percent": 1.0,
                    "memory_percent": 1.0, "cameras_seen": 0, "healthy_cameras": 0,
                    "warning_cameras": 0, "offline_cameras": 0, "sent_at": chrono::Utc::now(),
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let mut request = get("/api/v1/gateways");
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::OK);
        let list = body.as_array().expect("a list");
        assert_eq!(list.len(), 2, "roster + live must both show: {body}");
        let gw1 = list.iter().find(|v| v["gateway_id"] == "gw-1").unwrap();
        assert!(!gw1["revoked_at"].is_null(), "the revoked flag must reach the screen");
        assert_eq!(gw1["online"], false);
        let live = list.iter().find(|v| v["gateway_id"] == "gw-live").unwrap();
        assert_eq!(live["online"], true);
        assert!(!live["heartbeat"].is_null(), "a live gateway carries its report");
    }
```

Also add to `PROTECTED_ROUTES`: `("POST", "/api/v1/gateways/gw-1/revoke"),` — and note the existing `GET /api/v1/gateways` entry stays.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api -- a_revoked_gateway_is_refused` — expected: FAIL — the revoke route 404s (assert on the first revoke gets 404 ≠ 204).

- [ ] **Step 3: Implement.** In `authorized_gateway`, after the `bearer_token` extraction and before the shared-token comparison:

```rust
    // A revoked gateway is refused every credential, the bootstrap secret
    // included — revocation must actually evict. A store failure reads as
    // not-revoked so a database hiccup cannot 401 the whole fleet; the
    // per-token check below still fails closed on its own.
    if state.store.gateway_revoked(gateway_id).await.unwrap_or(false) {
        return false;
    }
```

Add the revoke handler near `gateways`:

```rust
async fn revoke_gateway(
    State(state): State<AppState>,
    Path(gateway_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    state
        .store
        .revoke_gateway(&gateway_id, Utc::now())
        .await
        .map_err(store_status)?;
    // Drop its live presence so the dashboard stops showing a healthy
    // reporter it will never hear from again.
    state.gateways.write().await.remove(&gateway_id);
    state.camera_batches.write().await.remove(&gateway_id);
    Ok(StatusCode::NO_CONTENT)
}
```

(The audit write lands in Task 3 — this task keeps the handler minimal.)

Rewrite the `gateways` handler:

```rust
async fn gateways(State(state): State<AppState>) -> Result<Json<Vec<GatewayView>>, StatusCode> {
    // The store is the roster, memory is the liveness — same split as /cameras.
    let mut views = state.store.gateway_views().await.map_err(store_status)?;
    let now = Utc::now();
    let live = state.gateways.read().await;
    for view in &mut views {
        if let Some(heartbeat) = live.get(&view.gateway_id) {
            view.online = (now - heartbeat.sent_at).num_seconds() <= state.stale_camera_seconds;
            view.last_seen = Some(match view.last_seen {
                Some(seen) => seen.max(heartbeat.sent_at),
                None => heartbeat.sent_at,
            });
            view.heartbeat = Some(heartbeat.clone());
        }
    }
    // A live gateway the store has not caught up with yet still shows.
    for (id, heartbeat) in live.iter() {
        if !views.iter().any(|view| view.gateway_id == *id) {
            views.push(GatewayView {
                gateway_id: id.clone(),
                site_id: heartbeat.site_id.clone(),
                site_name: String::new(),
                customer_name: String::new(),
                hostname: Some(heartbeat.hostname.clone()),
                version: Some(heartbeat.version.clone()),
                enrolled: false,
                revoked_at: None,
                last_seen: Some(heartbeat.sent_at),
                online: (now - heartbeat.sent_at).num_seconds() <= state.stale_camera_seconds,
                heartbeat: Some(heartbeat.clone()),
            });
        }
    }
    drop(live);
    views.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    Ok(Json(views))
}
```

Add the route to the **protected** group next to the gateways route:

```rust
        .route("/api/v1/gateways/{gateway_id}/revoke", post(revoke_gateway))
```

Add `GatewayView` to main.rs's `use vms_domain::{...}` list.

- [ ] **Step 4: Run all tests**

Run: `cargo test -p vms-api` — all pass, including `the_shared_token_admits_any_gateway_id` (unchanged behavior for non-revoked ids) and both 401-table tests with the new entry.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/main.rs
git commit -m "Revocation evicts a gateway from every credential; the roster reaches the screen"
```

---

### Task 3: The audit trail

**Files:**
- Modify: `services/api/src/main.rs` (`audit` helper, hooks, `GET /api/v1/audit`, `PROTECTED_ROUTES`, `AppState.audit_retention_days`, retention, imports `AuditView`, tests)
- Modify: `services/api/src/auth.rs` (hooks in login/change-password/seed)

**Interfaces:**
- Consumes: Task 1 `record_audit`/`audit_entries`/`delete_audit_before`; Task 2 revoke handler.
- Produces: `async fn audit(state: &AppState, actor: &str, action: &str, subject: &str, detail: Option<&str>)` in main.rs (crate-visible; auth.rs calls `crate::audit`); `GET /api/v1/audit` → `Vec<AuditView>` (cap 500); `AppState.audit_retention_days: i64` (env `AUDIT_RETENTION_DAYS`, default 0 = forever).

- [ ] **Step 1: Write the failing tests** — main.rs tests module:

```rust
    async fn audit_actions(state: &AppState, cookie: &str) -> Vec<String> {
        let mut request = get("/api/v1/audit");
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, body) = send(state, request).await;
        assert_eq!(status, StatusCode::OK);
        body.as_array()
            .expect("a list")
            .iter()
            .map(|entry| entry["action"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn security_events_land_in_the_audit_log() {
        let state = test_state().await;
        seed_admin(&state).await;

        // A failed login, then a good one.
        let (_, _) = send(&state, post("/api/v1/auth/login", None, serde_json::json!({ "password": "wrong" }))).await;
        let cookie = login_cookie(&state).await;

        // Enrollment token minted, gateway enrolled, then revoked.
        let _ = enrolled_gateway_token(&state, &cookie).await;
        let mut request = post("/api/v1/gateways/gw-1/revoke", None, serde_json::json!({}));
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        send(&state, request).await;

        // Password changed (which re-mints the caller's session).
        let mut request = post(
            "/api/v1/auth/password",
            None,
            serde_json::json!({ "current": ADMIN_PASSWORD, "new": "an entirely new passphrase" }),
        );
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let response = build_router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let fresh = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let actions = audit_actions(&state, &fresh).await;
        for expected in [
            "login.failed",
            "login.ok",
            "enrollment.created",
            "gateway.enrolled",
            "gateway.revoked",
            "password.changed",
        ] {
            assert!(actions.iter().any(|a| a == expected), "missing {expected} in {actions:?}");
        }
        // Nothing secret in any row.
        let mut request = get("/api/v1/audit");
        request.headers_mut().insert("cookie", fresh.parse().unwrap());
        let (_, body) = send(&state, request).await;
        let dump = body.to_string();
        assert!(!dump.contains(ADMIN_PASSWORD), "a password reached the audit log");
    }

    #[tokio::test]
    async fn a_forced_password_reset_is_audited() {
        let state = test_state().await;
        seed_admin(&state).await;
        crate::auth::seed_admin_credential(state.store.as_ref(), Some("a replacement passphrase"), true)
            .await
            .unwrap();
        let entries = state.store.audit_entries(10).await.unwrap();
        assert!(
            entries.iter().any(|e| e.action == "password.reset" && e.actor == "system"),
            "the loud reset must leave a row: {entries:?}"
        );
    }

    #[tokio::test]
    async fn audit_retention_prunes_only_when_told_to() {
        let state = test_state().await;
        let now = Utc::now();
        state.store.record_audit(now - chrono::Duration::days(400), "admin", "login.ok", "", None).await.unwrap();

        // Default 0: keep forever.
        retention_pass(&state).await;
        assert_eq!(state.store.audit_entries(10).await.unwrap().len(), 1);

        let mut state = state;
        state.audit_retention_days = 365;
        retention_pass(&state).await;
        assert!(state.store.audit_entries(10).await.unwrap().is_empty());
    }
```

Also add `("GET", "/api/v1/audit"),` to `PROTECTED_ROUTES`.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api -- security_events_land` — expected: FAIL — `/api/v1/audit` 404s.

- [ ] **Step 3: Implement.**

`AppState`: add `audit_retention_days: i64`; in `main()`'s literal:

```rust
        audit_retention_days: env::var("AUDIT_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
```

and `audit_retention_days: 0,` in `test_state_with`.

The helper, near `store_status`:

```rust
/// Fire-and-forget: the trail matters, but never enough to fail the request
/// it describes. Secrets never reach `detail` — that is the caller's oath.
async fn audit(state: &AppState, actor: &str, action: &str, subject: &str, detail: Option<&str>) {
    if let Err(err) = state.store.record_audit(Utc::now(), actor, action, subject, detail).await {
        warn!(action, error = %err, "audit write failed");
    }
}
```

The endpoint, in the protected group + handler:

```rust
        .route("/api/v1/audit", get(audit_entries))
```

```rust
async fn audit_entries(State(state): State<AppState>) -> Result<Json<Vec<AuditView>>, StatusCode> {
    state.store.audit_entries(500).await.map(Json).map_err(store_status)
}
```

Hooks:
- `create_enrollment` (after the store write succeeds): `audit(&state, "admin", "enrollment.created", &request.site_id, Some(&format!("{} / {}", request.customer_name, request.site_name))).await;`
- `gateway_enroll` (after `enroll_gateway` succeeds): `audit(&state, &format!("gateway:{}", request.gateway_id), "gateway.enrolled", &request.gateway_id, None).await;`
- `revoke_gateway` (after the store call, before returning): `audit(&state, "admin", "gateway.revoked", &gateway_id, None).await;`
- `auth.rs` `auth_login`: on the failure branch, before the sleep: `crate::audit(&state, "admin", "login.failed", "", None).await;` — on success, after the session is created: `crate::audit(&state, "admin", "login.ok", "", None).await;`
- `auth.rs` `auth_change_password`: after the new session is created: `crate::audit(&state, "admin", "password.changed", "", None).await;`
- `auth.rs` `seed_admin_credential` (forced-reset arm, after the wipe): it has only `&dyn Store`, so call the store directly and warn on failure:

```rust
            if let Err(err) = store
                .record_audit(chrono::Utc::now(), "system", "password.reset", "", None)
                .await
            {
                warn!(error = %err, "audit write failed for password.reset");
            }
```

Retention, in `retention_pass` after the incident pruning:

```rust
    if state.audit_retention_days > 0
        && let Err(err) = state
            .store
            .delete_audit_before(Utc::now() - chrono::Duration::days(state.audit_retention_days))
            .await
    {
        warn!(error = %err, "retention could not prune the audit log");
    }
```

Add `AuditView` to main.rs's `use vms_domain::{...}` list.

- [ ] **Step 4: Run all tests**

Run: `cargo test -p vms-api` — all pass.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/main.rs services/api/src/auth.rs
git commit -m "Security events leave a trail: the audit log and its endpoint"
```

---

### Task 4: The gateways grid — roster, badge, revoke button

**Files:**
- Modify: `web/dashboard-app.js` (`renderGateways` for `GatewayView`)
- Modify: `web/locales/en.json`, `es.json`, `ru.json` (3 keys)
- Create: `web/tests/gateways.test.mjs`

**Interfaces:**
- Consumes: Task 2's `GET /api/v1/gateways` shape (`GatewayView` with optional embedded `heartbeat`); `refresh()` already reloads `gateways`.

- [ ] **Step 1: Write the failing tests** — create `web/tests/gateways.test.mjs` (harness copied from the incidents suite pattern):

```js
// The gateways grid: the whole roster, a revoked badge, and a revoke button
// that actually calls the API.
import test, { beforeEach } from 'node:test';
import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { web } from './sources.mjs';

const read = name => readFileSync(join(web, name), 'utf8');
const brand = JSON.parse(read('brand.json'));
const dict = JSON.parse(read('locales/en.json'));
const demoFleet = JSON.parse(read('demo-fleet.json'));

const heartbeat = {
  gateway_id: 'gw-live', site_id: 'site-1', hostname: 'edge-live', version: '0.1.0',
  uptime_seconds: 3600, cpu_percent: 5, memory_percent: 10, cameras_seen: 2,
  healthy_cameras: 2, warning_cameras: 0, offline_cameras: 0,
  sent_at: new Date().toISOString(),
};
const roster = [
  { gateway_id: 'gw-live', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-live', version: '0.1.0', enrolled: true, revoked_at: null,
    last_seen: heartbeat.sent_at, online: true, heartbeat },
  { gateway_id: 'gw-quiet', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-quiet', version: '0.1.0', enrolled: true, revoked_at: null,
    last_seen: new Date(Date.now() - 86400_000).toISOString(), online: false, heartbeat: null },
  { gateway_id: 'gw-dead', site_id: 'site-1', site_name: 'Site', customer_name: 'Customer',
    hostname: 'edge-dead', version: '0.1.0', enrolled: false,
    revoked_at: new Date().toISOString(), last_seen: null, online: false, heartbeat: null },
];

let startDashboard;
let posted;

function stubFetch() {
  posted = [];
  globalThis.fetch = async (url, options = {}) => {
    const path = String(url);
    if ((options.method || 'GET') === 'POST') {
      posted.push(path);
      return { ok: true, status: 204, json: async () => ({}) };
    }
    const body =
      path.includes('api/v1/gateways') ? roster
      : path.includes('api/v1/incidents') ? []
      : path.endsWith('demo-fleet.json') ? demoFleet
      : path.endsWith('demo-plugins.json') ? []
      : undefined;
    if (body === undefined) return { ok: false, status: 404, json: async () => ({}) };
    return { ok: true, status: 200, json: async () => body };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'Event', 'FormData']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key];
  }
  dom.window.confirm = () => true;
  globalThis.confirm = dom.window.confirm;
}

beforeEach(async () => {
  loadPage();
  stubFetch();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('every roster gateway renders, not just the loud ones', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = document.querySelectorAll('#gateways-grid article');
  assert.equal(cards.length, 3);
  const text = document.querySelector('#gateways-grid').textContent;
  assert.match(text, /edge-quiet/, 'the enrolled-but-silent gateway vanished');
});

test('a revoked gateway shows the badge and no revoke button', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const dead = cards.find(card => card.textContent.includes('edge-dead'));
  assert.match(dead.textContent, /Revoked/);
  assert.equal(dead.querySelector('.gw-revoke'), null, 'a revoked gateway cannot be revoked again');
});

test('the revoke button confirms and posts to the revoke endpoint', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const cards = [...document.querySelectorAll('#gateways-grid article')];
  const quiet = cards.find(card => card.textContent.includes('edge-quiet'));
  quiet.querySelector('.gw-revoke').dispatchEvent(new Event('click', { bubbles: true }));
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(posted, ['api/v1/gateways/gw-quiet/revoke']);
});
```

- [ ] **Step 2: Run and watch them fail**

Run: `cd web && npm test` — the new suite fails (cards render from heartbeat fields, no badge, no button). If any EXISTING gateway-rendering test pinned the old heartbeat-array shape, fix it minimally to the new shape and record it in the report.

- [ ] **Step 3: Implement** — rewrite `renderGateways()` in `web/dashboard-app.js` (keep `formatUptime`):

```js
  function renderGateways() {
    const grid = document.querySelector('#gateways-grid');
    grid.replaceChildren();
    if (!Array.isArray(gateways) || gateways.length === 0) {
      const empty = document.createElement('p');
      empty.className = 'metric-sub';
      empty.textContent = t(dict, 'app.gateways.none');
      grid.appendChild(empty);
      return;
    }
    for (const gateway of gateways) {
      const hb = gateway.heartbeat;
      const healthy = Number(hb?.healthy_cameras || 0);
      const warn = Number(hb?.warning_cameras || 0);
      const off = Number(hb?.offline_cameras || 0);
      const revoked = Boolean(gateway.revoked_at);
      // Revoked outranks everything; a silent gateway is offline; a loud one
      // is only as healthy as what it reports about.
      const status = revoked ? 'offline'
        : !gateway.online ? 'offline'
        : off > 0 ? 'offline' : warn > 0 ? 'warning' : 'healthy';
      const card = document.createElement('article');
      card.className = 'plugin-card';
      card.innerHTML = `
        <div class="panel-head">
          <div><strong class="gw-name"></strong><div class="metric-sub gw-site"></div></div>
          <span class="health-pill ${status}"></span>
        </div>
        <div class="telemetry-grid">
          <div class="telemetry-metric"><span class="gw-l-uptime"></span><strong class="gw-uptime"></strong></div>
          <div class="telemetry-metric"><span class="gw-l-cameras"></span><strong class="gw-cameras"></strong></div>
          <div class="telemetry-metric"><span class="gw-l-version"></span><strong class="gw-version"></strong></div>
          <div class="telemetry-metric"><span class="gw-l-seen"></span><strong class="gw-seen"></strong></div>
        </div>`;
      const set = (sel, value) => { card.querySelector(sel).textContent = value; };
      set('.gw-name', gateway.hostname || gateway.gateway_id);
      set('.gw-site', gateway.site_name || gateway.site_id || '');
      set('.health-pill', revoked ? t(dict, 'app.gateways.revoked') : t(dict, `app.${status}`));
      set('.gw-l-uptime', t(dict, 'app.gateways.uptime'));
      set('.gw-l-cameras', t(dict, 'app.gateways.cameras'));
      set('.gw-l-version', t(dict, 'app.gateways.version'));
      set('.gw-l-seen', t(dict, 'app.gateways.lastSeen'));
      set('.gw-uptime', hb ? formatUptime(hb.uptime_seconds) : '—');
      set('.gw-cameras', hb ? `${healthy} / ${healthy + warn + off}` : '—');
      set('.gw-version', gateway.version || '—');
      set('.gw-seen', gateway.last_seen ? new Date(gateway.last_seen).toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit' }) : '—');
      if (gateway.enrolled && !revoked) {
        const revoke = document.createElement('button');
        revoke.className = 'button small gw-revoke';
        revoke.textContent = t(dict, 'app.gateways.revoke');
        revoke.addEventListener('click', async () => {
          if (!confirm(t(dict, 'app.gateways.revokeConfirm'))) return;
          await fetch(`api/v1/gateways/${encodeURIComponent(gateway.gateway_id)}/revoke`, { method: 'POST' }).catch(() => {});
          refresh();
        });
        card.appendChild(revoke);
      }
      grid.appendChild(card);
    }
  }
```

- [ ] **Step 4: Add the strings** — `en.json` (after `app.gateways.none`):

```json
  "app.gateways.revoke": "Revoke",
  "app.gateways.revokeConfirm": "Revoke this gateway? Its token stops working immediately; bringing it back needs a fresh enrollment token.",
  "app.gateways.revoked": "Revoked"
```

es: `"Revocar"`, `"¿Revocar esta pasarela? Su token deja de funcionar de inmediato; para recuperarla hará falta un nuevo token de alta."`, `"Revocada"`
ru: `"Отозвать"`, `"Отозвать этот шлюз? Его токен перестанет работать немедленно; для возврата понадобится новый токен регистрации."`, `"Отозван"`

- [ ] **Step 5: Run the web tests**

Run: `cd web && npm test` — all suites green, including i18n parity.

- [ ] **Step 6: Commit**

```bash
git add web/dashboard-app.js web/locales/en.json web/locales/es.json web/locales/ru.json web/tests/gateways.test.mjs
git commit -m "The gateways grid shows the whole roster and can revoke one"
```

---

### Task 5: Docs, backlog, verification, smoke

**Files:**
- Modify: `docs/RUNNING-LOCALLY.md`
- Modify: `docs/BACKLOG.md`

- [ ] **Step 1: Docs** — in `docs/RUNNING-LOCALLY.md`, after the "Gateway identity" section, add a "Revocation and the audit log" section: revoke from the gateways grid (or `POST /api/v1/gateways/<id>/revoke`); a revoked id is refused every credential including the shared bootstrap token; a fresh enrollment token is the un-revoke; the known limit (the two id-less plugin endpoints answer to the bootstrap secret regardless — same trust that secret already carries); `GET /api/v1/audit` lists security events (logins, password changes, enrollments, revocations), kept forever by default, `AUDIT_RETENTION_DAYS` to prune.

- [ ] **Step 2: Backlog** — check off `- [ ] Gateway revocation and audit log`.

- [ ] **Step 3: Full verification**

Run: `cargo test --workspace` and `cd web && npm test` — everything green, exact counts in the report. `git status --short` shows only expected files; `Cargo.lock` untouched.

- [ ] **Step 4: Smoke** (port free per `ss -ltn`; 8123 unless taken):

```bash
cargo build -p vms-api
(DATABASE_URL=sqlite:/tmp/vms-revoke-smoke.db API_BIND=127.0.0.1:8123 ADMIN_PASSWORD='smoke-test-password' ./target/debug/vms-api &> /tmp/vms-revoke-smoke.log &)
sleep 2
ss -ltn | grep 8123
curl -s -c /tmp/smoke-jar -o /dev/null -w '%{http_code}\n' \
  -H 'content-type: application/json' -d '{"password":"smoke-test-password"}' \
  http://127.0.0.1:8123/api/v1/auth/login                                      # 204
curl -s -b /tmp/smoke-jar http://127.0.0.1:8123/api/v1/audit                   # contains "login.ok"
curl -s -b /tmp/smoke-jar -o /dev/null -w '%{http_code}\n' -X POST \
  http://127.0.0.1:8123/api/v1/gateways/gw-nope/revoke                         # 404
(pkill -f 'debug/vms-api' || true)
rm -f /tmp/vms-revoke-smoke.db /tmp/smoke-jar
```

- [ ] **Step 5: Commit**

```bash
git add docs/RUNNING-LOCALLY.md docs/BACKLOG.md
git commit -m "Write down how revocation and the audit trail work"
```
