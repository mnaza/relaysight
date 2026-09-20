# Camera Incident Timeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist camera disconnect/recovery history in the SQLite store and light up the dashboard's disabled Incidents view with real outage data.

**Architecture:** One idempotent reconciler (`incident_pass`) on the existing 60-second background loop opens/closes incident rows using the same effective-status rule as the `/cameras` view; a partial unique index makes "one open incident per camera" a database invariant; `GET /api/v1/incidents` (protected) feeds a new dashboard panel and the rewired "Open incidents" stat.

**Tech Stack:** Rust (axum 0.8, sqlx 0.9 runtime queries, chrono, uuid), vanilla-JS SPA tested with `node --test` + jsdom.

**Spec:** `docs/superpowers/specs/2026-09-09-incident-timeline-design.md`

## Global Constraints

- No production code without a failing test first; run the test and watch it fail before implementing.
- Timestamps in SQLite always via `ts()` / `parse_ts()` from `store/mod.rs`. sqlx runtime queries only; migrations via `sqlx::migrate!("./migrations")` (picks up new files automatically).
- The repo-root `Cargo.toml` carries a local uncommitted `[patch.crates-io]` retina patch. **In any fresh worktree, append it FIRST, before any cargo command** (path `/home/andrey/work/retina-fork`), and never stage `Cargo.toml` or run `git add .` — stage files by explicit path.
- Rust tests: `cargo test -p vms-api` (and `-p vms-domain` compiles via workspace). Web tests: `npm test` in `web/` (run `npm install` once in a fresh worktree).
- New route `GET /api/v1/incidents` goes in the **protected** router group AND the `PROTECTED_ROUTES` test table.
- Incident invariants: at most one open incident per camera (DB-enforced); open incidents are never pruned; `incidents(limit)` returns open first, then closed newest-first; limit for the API is 200.
- Env: `INCIDENT_RETENTION_DAYS` default 90, `0` = keep forever. Startup grace: `incident_pass` is a no-op until the API has been up for `stale_camera_seconds`.
- The silence detail string is exactly `"gateway telemetry is stale"` (same literal the `/cameras` handler uses).

## File Structure

- `services/api/migrations/0003_incidents.sql` — create.
- `crates/domain/src/lib.rs` — modify: add `IncidentView`.
- `services/api/src/store/mod.rs` — modify: 4 new `Store` methods.
- `services/api/src/store/sqlite.rs` — modify: impl + store tests.
- `services/api/src/main.rs` — modify: `AppState` fields, `incident_pass`, loop call, handler, route, retention pruning, tests.
- `web/app.html` — modify: enable nav link, add `#incidents` panel.
- `web/dashboard-app.js` — modify: loader, painter, stat, refresh.
- `web/locales/{en,es,ru}.json` — modify: `app.incidents.*` keys.
- `web/tests/incidents.test.mjs` — create.
- `docs/RUNNING-LOCALLY.md`, `docs/BACKLOG.md` — modify.

---

### Task 1: Incident storage

**Files:**
- Create: `services/api/migrations/0003_incidents.sql`
- Modify: `crates/domain/src/lib.rs` (add `IncidentView` near `RecordingTimeline`, ~line 351)
- Modify: `services/api/src/store/mod.rs` (trait methods; add `IncidentView` to the `vms_domain` use list)
- Modify: `services/api/src/store/sqlite.rs` (impl + tests; add `IncidentView` to its `vms_domain` use list)

**Interfaces:**
- Consumes: existing `ts()`, `parse_ts()`, `StoreError`, `uuid` crate (already a vms-api dep).
- Produces (later tasks call these exact signatures on `dyn Store`):
  - `open_incident(camera_id: &str, started_at: DateTime<Utc>, detail: Option<&str>) -> Result<(), StoreError>`
  - `close_incident(camera_id: &str, ended_at: DateTime<Utc>) -> Result<(), StoreError>`
  - `incidents(limit: i64) -> Result<Vec<IncidentView>, StoreError>`
  - `delete_closed_incidents_before(cutoff: DateTime<Utc>) -> Result<(), StoreError>`
  - `vms_domain::IncidentView { camera_id, camera_name, site_id, site_name, started_at: DateTime<Utc>, ended_at: Option<DateTime<Utc>>, detail: Option<String> }`

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `services/api/src/store/sqlite.rs`:

```rust
    #[tokio::test]
    async fn at_most_one_open_incident_per_camera_and_a_flap_is_two_rows() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store.open_incident("cam-1", now, Some("gateway telemetry is stale")).await.unwrap();
        store.open_incident("cam-1", now + Duration::minutes(1), None).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM incidents")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(rows, 1, "the second open must be a no-op");

        store.close_incident("cam-1", now + Duration::minutes(5)).await.unwrap();
        // Closing nothing is Ok — the reconciler closes unconditionally.
        store.close_incident("cam-1", now + Duration::minutes(6)).await.unwrap();
        store.open_incident("cam-1", now + Duration::minutes(10), None).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM incidents")
            .fetch_one(&store.pool).await.unwrap();
        assert_eq!(rows, 2, "a flap is two incidents, not one reopened row");
    }

    #[tokio::test]
    async fn incidents_come_open_first_with_joined_names() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.upsert_fleet_identity(&batch_with_camera("gw-1", "cam-1", "Entrance"), Utc::now()).await.unwrap();
        let now = Utc::now();
        store.open_incident("cam-1", now - Duration::hours(2), None).await.unwrap();
        store.close_incident("cam-1", now - Duration::hours(1)).await.unwrap();
        store.open_incident("cam-1", now - Duration::minutes(5), Some("rtsp: connection refused")).await.unwrap();
        store.open_incident("cam-gone", now - Duration::days(3), None).await.unwrap();

        let incidents = store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 3);
        assert!(
            incidents[0].ended_at.is_none() && incidents[1].ended_at.is_none(),
            "open incidents come first: {incidents:?}"
        );
        assert_eq!(incidents[0].camera_name, "Entrance", "names join from the roster");
        assert_eq!(incidents[0].site_name, "Site");
        assert_eq!(incidents[0].detail.as_deref(), Some("rtsp: connection refused"));
        // A camera no longer in the roster still shows, under its id.
        assert_eq!(incidents[1].camera_name, "cam-gone");
        assert!(incidents[2].ended_at.is_some());
    }

    #[tokio::test]
    async fn pruning_removes_only_old_closed_incidents() {
        let store = SqliteStore::in_memory().await.unwrap();
        let now = Utc::now();
        store.open_incident("cam-old", now - Duration::days(120), None).await.unwrap();
        store.close_incident("cam-old", now - Duration::days(119)).await.unwrap();
        store.open_incident("cam-recent", now - Duration::days(2), None).await.unwrap();
        store.close_incident("cam-recent", now - Duration::days(1)).await.unwrap();
        store.open_incident("cam-stuck", now - Duration::days(200), None).await.unwrap();

        store.delete_closed_incidents_before(now - Duration::days(90)).await.unwrap();
        let left = store.incidents(10).await.unwrap();
        assert_eq!(left.len(), 2, "only the old closed incident goes: {left:?}");
        assert!(
            left.iter().any(|i| i.camera_id == "cam-stuck" && i.ended_at.is_none()),
            "an open incident is never pruned, however old"
        );
        assert!(left.iter().any(|i| i.camera_id == "cam-recent"));
    }
```

Also extend `connect_applies_migrations`'s expected-tables list with `"incidents"`.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api -- at_most_one_open_incident` — expected: compile error, `open_incident` not found.

- [ ] **Step 3: Write the migration** — `services/api/migrations/0003_incidents.sql`:

```sql
-- Camera disconnect/recovery history.
-- See docs/superpowers/specs/2026-09-09-incident-timeline-design.md.

CREATE TABLE incidents (
    id         TEXT PRIMARY KEY,   -- uuid
    camera_id  TEXT NOT NULL,
    started_at TEXT NOT NULL,      -- fixed-width RFC 3339, as everywhere
    ended_at   TEXT,               -- NULL = ongoing
    detail     TEXT
);
-- One open incident per camera, as a database invariant.
CREATE UNIQUE INDEX idx_incidents_open ON incidents(camera_id) WHERE ended_at IS NULL;
CREATE INDEX idx_incidents_started ON incidents(started_at);
```

- [ ] **Step 4: Add the domain type** — in `crates/domain/src/lib.rs`, after `RecordingTimeline`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentView {
    pub camera_id: String,
    pub camera_name: String,
    pub site_id: String,
    pub site_name: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub detail: Option<String>,
}
```

- [ ] **Step 5: Extend the trait** — append to `trait Store` in `services/api/src/store/mod.rs` (and add `IncidentView` to the `use vms_domain::{...}` list):

```rust
    /// Open a disconnect incident. Idempotent: at most one open incident per
    /// camera, enforced by the database.
    async fn open_incident(
        &self,
        camera_id: &str,
        started_at: DateTime<Utc>,
        detail: Option<&str>,
    ) -> Result<(), StoreError>;

    /// Close the camera's open incident, if any. Closing nothing is Ok — the
    /// reconciler closes unconditionally.
    async fn close_incident(
        &self,
        camera_id: &str,
        ended_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Open incidents first (newest-started first), then closed ones
    /// newest-first — an open incident can never be paged out by the limit.
    async fn incidents(&self, limit: i64) -> Result<Vec<IncidentView>, StoreError>;

    /// Prune closed incidents that ended before the cutoff. Open incidents
    /// are never pruned.
    async fn delete_closed_incidents_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<(), StoreError>;
```

- [ ] **Step 6: Implement in `SqliteStore`** — append inside `impl Store for SqliteStore` (add `IncidentView` to sqlite.rs's `use vms_domain::{...}` list):

```rust
    async fn open_incident(
        &self,
        camera_id: &str,
        started_at: DateTime<Utc>,
        detail: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO incidents (id, camera_id, started_at, ended_at, detail)
             VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(camera_id) WHERE ended_at IS NULL DO NOTHING",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(camera_id)
        .bind(ts(&started_at))
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn close_incident(
        &self,
        camera_id: &str,
        ended_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE incidents SET ended_at = ?2 WHERE camera_id = ?1 AND ended_at IS NULL")
            .bind(camera_id)
            .bind(ts(&ended_at))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn incidents(&self, limit: i64) -> Result<Vec<IncidentView>, StoreError> {
        let rows = sqlx::query(
            "SELECT i.camera_id,
                    COALESCE(c.name, i.camera_id) AS camera_name,
                    COALESCE(c.site_id, '') AS site_id,
                    COALESCE(s.name, '') AS site_name,
                    i.started_at, i.ended_at, i.detail
             FROM incidents i
             LEFT JOIN cameras c ON c.id = i.camera_id
             LEFT JOIN sites s ON s.id = c.site_id
             ORDER BY (i.ended_at IS NULL) DESC, i.started_at DESC
             LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(IncidentView {
                    camera_id: row.get("camera_id"),
                    camera_name: row.get("camera_name"),
                    site_id: row.get("site_id"),
                    site_name: row.get("site_name"),
                    started_at: parse_ts(row.get::<String, _>("started_at").as_str())?,
                    ended_at: row
                        .get::<Option<String>, _>("ended_at")
                        .map(|value| parse_ts(&value))
                        .transpose()?,
                    detail: row.get("detail"),
                })
            })
            .collect()
    }

    async fn delete_closed_incidents_before(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM incidents WHERE ended_at IS NOT NULL AND ended_at < ?1")
            .bind(ts(&cutoff))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

- [ ] **Step 7: Run the tests and make sure they pass**

Run: `cargo test -p vms-api store::` — expected: all store tests pass including the three new ones. (`dead_code` warnings on the new trait methods are expected staging until Tasks 2–4 consume them; do not silence.)

- [ ] **Step 8: Commit**

```bash
git add services/api/migrations/0003_incidents.sql crates/domain/src/lib.rs services/api/src/store/mod.rs services/api/src/store/sqlite.rs
git commit -m "Incidents live in the store: one open per camera, by construction"
```

---

### Task 2: The reconciler

**Files:**
- Modify: `services/api/src/main.rs` (`AppState` fields, `main()` wiring, `incident_pass`, loop call, test helpers, tests)

**Interfaces:**
- Consumes: Task 1 store methods; existing `HealthStatus`, `CameraTelemetry`, `CameraTelemetryBatch` (all already imported in main.rs); `state.camera_batches`, `state.stale_camera_seconds`.
- Produces: `async fn incident_pass(state: &AppState)`; `AppState.incident_grace: Duration`, `AppState.up_since: std::time::Instant`; test helper `typed_batch(gateway_id, camera_id, status, last_seen, last_error) -> CameraTelemetryBatch`.

- [ ] **Step 1: Add the AppState fields** (compile scaffolding for the tests). In `struct AppState` after `cookie_secure`:

```rust
    /// No incident sweeps until the API has been up this long — right after a
    /// restart every camera looks silent until its gateway re-reports.
    incident_grace: Duration,
    up_since: std::time::Instant,
```

In `main()`'s `AppState` literal (`stale_camera_seconds` is currently parsed inline in the literal — pull it into a local `let stale_camera_seconds: i64 = env::var("STALE_CAMERA_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(75);` above the literal, then):

```rust
        stale_camera_seconds,
        incident_grace: Duration::from_secs(stale_camera_seconds.max(0) as u64),
        up_since: std::time::Instant::now(),
```

In `test_state_with` in the tests module:

```rust
            incident_grace: Duration::ZERO,
            up_since: std::time::Instant::now(),
```

- [ ] **Step 2: Write the failing tests** — in the main.rs tests module, add the typed-batch helper and four tests:

```rust
    fn typed_batch(
        gateway_id: &str,
        camera_id: &str,
        status: HealthStatus,
        last_seen: chrono::DateTime<chrono::Utc>,
        last_error: Option<&str>,
    ) -> CameraTelemetryBatch {
        CameraTelemetryBatch {
            gateway_id: gateway_id.into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
            sent_at: last_seen,
            cameras: vec![CameraTelemetry {
                camera_id: camera_id.into(),
                gateway_id: gateway_id.into(),
                site_id: "site-1".into(),
                name: "Entrance".into(),
                status,
                manufacturer: None,
                model: None,
                firmware: None,
                profile_name: None,
                codec: None,
                width: None,
                height: None,
                fps: None,
                bitrate_kbps: None,
                packet_loss: 0,
                reconnects: 0,
                rtsp_endpoint: None,
                last_seen,
                last_error: last_error.map(Into::into),
            }],
        }
    }

    #[tokio::test]
    async fn a_silent_camera_opens_a_backdated_incident_and_recovery_closes_it() {
        let state = test_state().await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        // In the roster with an old last_seen and no live telemetry: silence.
        state.store
            .upsert_fleet_identity(&typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None), went_dark)
            .await.unwrap();

        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert!(incidents[0].ended_at.is_none());
        let expected_start = went_dark + chrono::Duration::seconds(state.stale_camera_seconds);
        assert_eq!(
            incidents[0].started_at.timestamp(),
            expected_start.timestamp(),
            "silence must backdate to when the camera actually went dark"
        );
        assert_eq!(incidents[0].detail.as_deref(), Some("gateway telemetry is stale"));

        // A second silent pass must not open another one.
        incident_pass(&state).await;
        assert_eq!(state.store.incidents(10).await.unwrap().len(), 1);

        // Fresh healthy telemetry closes it.
        state.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None),
        );
        incident_pass(&state).await;
        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert!(incidents[0].ended_at.is_some(), "recovery must close the incident");
    }

    #[tokio::test]
    async fn a_camera_reported_offline_opens_an_incident_with_its_error() {
        let state = test_state().await;
        let now = Utc::now();
        state.store
            .upsert_fleet_identity(&typed_batch("gw-1", "cam-1", HealthStatus::Offline, now, Some("rtsp: connection refused")), now)
            .await.unwrap();
        state.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Offline, now, Some("rtsp: connection refused")),
        );

        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert!(incidents[0].ended_at.is_none());
        assert_eq!(incidents[0].detail.as_deref(), Some("rtsp: connection refused"));
    }

    #[tokio::test]
    async fn a_flap_is_two_incidents() {
        let state = test_state().await;
        let now = Utc::now();
        state.store
            .upsert_fleet_identity(&typed_batch("gw-1", "cam-1", HealthStatus::Offline, now, None), now)
            .await.unwrap();
        state.camera_batches.write().await.insert(
            "gw-1".into(), typed_batch("gw-1", "cam-1", HealthStatus::Offline, now, None));
        incident_pass(&state).await;
        state.camera_batches.write().await.insert(
            "gw-1".into(), typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None));
        incident_pass(&state).await;
        state.camera_batches.write().await.insert(
            "gw-1".into(), typed_batch("gw-1", "cam-1", HealthStatus::Offline, Utc::now(), None));
        incident_pass(&state).await;

        let incidents = state.store.incidents(10).await.unwrap();
        assert_eq!(incidents.len(), 2);
        assert_eq!(incidents.iter().filter(|i| i.ended_at.is_none()).count(), 1);
    }

    #[tokio::test]
    async fn the_startup_grace_skips_sweeps_until_gateways_can_report() {
        let mut state = test_state().await;
        state.incident_grace = Duration::from_secs(3600);
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        state.store
            .upsert_fleet_identity(&typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None), went_dark)
            .await.unwrap();

        incident_pass(&state).await;

        assert!(
            state.store.incidents(10).await.unwrap().is_empty(),
            "a sweep inside the grace window must not blame a deploy on the cameras"
        );
    }
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test -p vms-api -- a_silent_camera` — expected: compile error, `incident_pass` not found.

- [ ] **Step 4: Implement `incident_pass`** — in main.rs, next to `retention_pass`:

```rust
/// Record disconnects and recoveries. One writer, on the retention cadence.
/// Effective status follows the same rule as the /cameras view: reported
/// offline, or silent past the stale window. Idempotent against the store's
/// one-open-incident-per-camera invariant, so no read-modify-write.
async fn incident_pass(state: &AppState) {
    if state.up_since.elapsed() < state.incident_grace {
        return;
    }
    let records = match state.store.fleet_cameras().await {
        Ok(records) => records,
        Err(err) => {
            warn!(error = %err, "incident pass could not list cameras");
            return;
        }
    };
    let now = Utc::now();
    let live: HashMap<String, CameraTelemetry> = state
        .camera_batches
        .read()
        .await
        .values()
        .flat_map(|batch| batch.cameras.clone())
        .map(|camera| (camera.camera_id.clone(), camera))
        .collect();
    for record in records {
        let (last_seen, reported_offline, last_error) = match live.get(&record.id) {
            Some(camera) => (
                camera.last_seen,
                camera.status == HealthStatus::Offline,
                camera.last_error.clone(),
            ),
            None => (record.last_seen, false, None),
        };
        let stale = (now - last_seen).num_seconds() > state.stale_camera_seconds;
        let result = if stale {
            let started = last_seen + chrono::Duration::seconds(state.stale_camera_seconds);
            state
                .store
                .open_incident(&record.id, started, Some("gateway telemetry is stale"))
                .await
        } else if reported_offline {
            state
                .store
                .open_incident(&record.id, now, last_error.as_deref())
                .await
        } else {
            state.store.close_incident(&record.id, now).await
        };
        if let Err(err) = result {
            warn!(camera_id = %record.id, error = %err, "incident pass store failure");
        }
    }
}
```

And in `retention_loop`, after `retention_pass(&state).await;` add:

```rust
        incident_pass(&state).await;
```

- [ ] **Step 5: Run the tests and make sure they pass**

Run: `cargo test -p vms-api` — expected: all pass, including the four new ones.

- [ ] **Step 6: Commit**

```bash
git add services/api/src/main.rs
git commit -m "A reconciler records disconnects and recoveries every minute"
```

---

### Task 3: The incidents API

**Files:**
- Modify: `services/api/src/main.rs` (handler, route, `PROTECTED_ROUTES`, imports, tests)

**Interfaces:**
- Consumes: `store.incidents(200)`, `store_status`, `IncidentView` (add to main.rs's `use vms_domain::{...}` list), test helpers `seed_admin`, `login_cookie`, `typed_batch`, `incident_pass`.

- [ ] **Step 1: Write the failing tests** — main.rs tests module:

```rust
    #[tokio::test]
    async fn the_incident_list_needs_a_session_and_shows_open_incidents() {
        let state = test_state().await;
        seed_admin(&state).await;
        let (status, _) = send(&state, get("/api/v1/incidents")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        state.store
            .open_incident("cam-1", Utc::now(), Some("gateway telemetry is stale"))
            .await.unwrap();
        let cookie = login_cookie(&state).await;
        let mut request = get("/api/v1/incidents");
        request.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (status, body) = send(&state, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["camera_id"], "cam-1");
        assert!(body[0]["ended_at"].is_null());
        assert_eq!(body[0]["detail"], "gateway telemetry is stale");
    }

    #[tokio::test]
    async fn an_open_incident_survives_a_restart_and_recovery_closes_it() {
        let file = tempfile::NamedTempFile::new().expect("temp db");
        let url = format!("sqlite:{}", file.path().display());
        let store: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("store"),
        );
        let state = test_state_with(store).await;
        let went_dark = Utc::now() - chrono::Duration::seconds(300);
        state.store
            .upsert_fleet_identity(&typed_batch("gw-1", "cam-1", HealthStatus::Healthy, went_dark, None), went_dark)
            .await.unwrap();
        incident_pass(&state).await;
        assert_eq!(state.store.incidents(10).await.unwrap().len(), 1);

        let store2: Arc<dyn crate::store::Store> = Arc::new(
            crate::store::SqliteStore::connect(&url).await.expect("reopen"),
        );
        let restarted = test_state_with(store2).await;
        let open = restarted.store.incidents(10).await.unwrap();
        assert_eq!(open.len(), 1, "the incident vanished across the restart");
        assert!(open[0].ended_at.is_none());

        restarted.camera_batches.write().await.insert(
            "gw-1".into(),
            typed_batch("gw-1", "cam-1", HealthStatus::Healthy, Utc::now(), None),
        );
        incident_pass(&restarted).await;
        assert!(restarted.store.incidents(10).await.unwrap()[0].ended_at.is_some());
    }
```

Also add `("GET", "/api/v1/incidents"),` to `PROTECTED_ROUTES`.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p vms-api -- the_incident_list_needs` — expected: FAIL — 404 (route absent), and `every_protected_route_is_401_without_a_session` now also fails on the new table entry.

- [ ] **Step 3: Implement** — handler next to `camera_timeline`:

```rust
async fn incidents(State(state): State<AppState>) -> Result<Json<Vec<IncidentView>>, StatusCode> {
    // Open first, newest-closed after; 200 is plenty for a screen.
    state.store.incidents(200).await.map(Json).map_err(store_status)
}
```

Route in the **protected** group in `build_router`, next to `/api/v1/fleet`:

```rust
        .route("/api/v1/incidents", get(incidents))
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p vms-api` — expected: all pass, including both 401-table tests.

- [ ] **Step 5: Commit**

```bash
git add services/api/src/main.rs
git commit -m "GET /api/v1/incidents: the outage history, behind the session wall"
```

---

### Task 4: Incident retention

**Files:**
- Modify: `services/api/src/main.rs` (`AppState.incident_retention_days`, env wiring, `retention_pass` addition, tests)

**Interfaces:**
- Consumes: `delete_closed_incidents_before` (Task 1).
- Produces: `AppState.incident_retention_days: i64` (env `INCIDENT_RETENTION_DAYS`, default 90, `0` disables).

- [ ] **Step 1: Add the field** — `struct AppState`: `incident_retention_days: i64,`; in `main()`'s literal:

```rust
        incident_retention_days: env::var("INCIDENT_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90),
```

and in `test_state_with`: `incident_retention_days: 90,`.

- [ ] **Step 2: Write the failing tests** — main.rs tests module:

```rust
    #[tokio::test]
    async fn retention_prunes_old_closed_incidents_but_never_open_ones() {
        let state = test_state().await;
        let now = Utc::now();
        state.store.open_incident("cam-old", now - chrono::Duration::days(120), None).await.unwrap();
        state.store.close_incident("cam-old", now - chrono::Duration::days(119)).await.unwrap();
        state.store.open_incident("cam-stuck", now - chrono::Duration::days(200), None).await.unwrap();

        retention_pass(&state).await;

        let left = state.store.incidents(10).await.unwrap();
        assert_eq!(left.len(), 1, "the 119-day-old closed incident must be pruned: {left:?}");
        assert_eq!(left[0].camera_id, "cam-stuck");
        assert!(left[0].ended_at.is_none());
    }

    #[tokio::test]
    async fn incident_retention_zero_keeps_everything() {
        let mut state = test_state().await;
        state.incident_retention_days = 0;
        let now = Utc::now();
        state.store.open_incident("cam-old", now - chrono::Duration::days(400), None).await.unwrap();
        state.store.close_incident("cam-old", now - chrono::Duration::days(399)).await.unwrap();

        retention_pass(&state).await;

        assert_eq!(state.store.incidents(10).await.unwrap().len(), 1);
    }
```

- [ ] **Step 3: Run and watch the first fail**

Run: `cargo test -p vms-api -- retention_prunes_old_closed` — expected: FAIL — the old closed incident is still there (nothing prunes yet). The zero test passes trivially; keep it as the pinned knob behavior.

- [ ] **Step 4: Implement** — in `retention_pass`, after the expired-sessions sweep:

```rust
    if state.incident_retention_days > 0
        && let Err(err) = state
            .store
            .delete_closed_incidents_before(
                Utc::now() - chrono::Duration::days(state.incident_retention_days),
            )
            .await
    {
        warn!(error = %err, "retention could not prune closed incidents");
    }
```

- [ ] **Step 5: Run all tests**

Run: `cargo test -p vms-api` — expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add services/api/src/main.rs
git commit -m "Closed incidents age out after INCIDENT_RETENTION_DAYS; open ones never do"
```

---

### Task 5: The incidents view

**Files:**
- Modify: `web/app.html` (nav link, panel)
- Modify: `web/dashboard-app.js` (loader, painter, stat, refresh)
- Modify: `web/locales/en.json`, `web/locales/es.json`, `web/locales/ru.json`
- Create: `web/tests/incidents.test.mjs`

**Interfaces:**
- Consumes: `GET api/v1/incidents` (Task 3's JSON shape); `t(dict, key)` from theme.js (already imported in dashboard-app.js).

- [ ] **Step 1: Write the failing tests** — create `web/tests/incidents.test.mjs`:

```js
// The incidents view: real history rows, an ongoing badge, and a stat that
// counts open incidents instead of guessing from live status.
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

const anHourAgo = new Date(Date.now() - 3600_000).toISOString();
const twoIncidents = [
  { camera_id: 'cam-001', camera_name: 'Entrance', site_id: 'madrid-centro', site_name: 'Madrid Centro',
    started_at: anHourAgo, ended_at: null, detail: 'gateway telemetry is stale' },
  { camera_id: 'cam-002', camera_name: 'Checkout 01', site_id: 'madrid-centro', site_name: 'Madrid Centro',
    started_at: anHourAgo, ended_at: new Date(Date.now() - 3000_000).toISOString(), detail: null },
];

let startDashboard;
let incidentsBody;

function stubFetch() {
  globalThis.fetch = async (url) => {
    const path = String(url);
    const body =
      path.includes('api/v1/incidents') ? incidentsBody
      : path.endsWith('demo-fleet.json') ? demoFleet
      : path.endsWith('demo-plugins.json') ? []
      : undefined;
    if (body === undefined) return { ok: false, status: 404, json: async () => ({}) };
    return { ok: true, status: 200, json: async () => body };
  };
}

function loadPage() {
  const dom = new JSDOM(read('app.html'), { url: 'https://example.test/app.html' });
  for (const key of ['document', 'window', 'location', 'localStorage', 'URL', 'Event', 'FormData']) {
    globalThis[key] = key === 'document' ? dom.window.document
      : key === 'window' ? dom.window
      : dom.window[key];
  }
}

beforeEach(async () => {
  loadPage();
  incidentsBody = twoIncidents;
  stubFetch();
  if (!startDashboard) ({ startDashboard } = await import('../dashboard-app.js'));
});

test('the incidents table renders history with an ongoing badge and a cause', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const rows = document.querySelectorAll('#incidents-body tr');
  assert.equal(rows.length, 2);
  assert.match(rows[0].textContent, /Entrance/);
  assert.match(rows[0].textContent, /Ongoing/);
  assert.match(rows[0].textContent, /gateway telemetry is stale/);
  assert.match(rows[1].textContent, /min/, 'a closed incident shows a duration');
  assert.equal(document.querySelector('#incidents-empty').hidden, true);
});

test('the open-incidents stat counts open incidents, not live camera status', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  assert.equal(document.querySelector('#stat-alerts').textContent, '1');
});

test('no incidents shows the empty state and a zero stat', async () => {
  incidentsBody = [];
  await startDashboard({ brand, locale: 'en', dict });
  assert.equal(document.querySelectorAll('#incidents-body tr').length, 0);
  assert.equal(document.querySelector('#incidents-empty').hidden, false);
  assert.equal(document.querySelector('#stat-alerts').textContent, '0');
});

test('the incidents nav link is enabled and points at the view', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const link = document.querySelector('a[href="#incidents"]');
  assert.ok(link, 'no nav link to #incidents');
  assert.ok(!link.classList.contains('disabled'));
});
```

- [ ] **Step 2: Run and watch them fail**

Run: `cd web && npm test` — expected: incidents.test.mjs fails (no `#incidents-body`, stat shows the old count, no `href="#incidents"`); every other suite stays green.

- [ ] **Step 3: Enable the nav link** — in `web/app.html`, replace the disabled link AND the two-line comment above it:

```html
        <a class="sidebar-link" href="#incidents"><span class="nav-icon">△</span><span data-i18n="app.events"></span></a>
```

- [ ] **Step 4: Add the panel** — in `web/app.html`, between the `#fleet` section and the `#gateways` section:

```html
        <section id="incidents" class="panel">
          <div class="panel-head">
            <div><strong data-i18n="app.incidents.title"></strong><div class="metric-sub" data-i18n="app.incidents.subtitle"></div></div>
          </div>
          <div style="overflow-x:auto">
            <table class="fleet-table">
              <thead><tr><th data-i18n="app.incidents.camera"></th><th data-i18n="app.incidents.started"></th><th data-i18n="app.incidents.duration"></th><th data-i18n="app.incidents.cause"></th></tr></thead>
              <tbody id="incidents-body"></tbody>
            </table>
          </div>
          <div id="incidents-empty" class="metric-sub" hidden data-i18n="app.incidents.empty"></div>
        </section>
```

- [ ] **Step 5: Wire the data** — in `web/dashboard-app.js` (do not touch the `innerHTML` template blocks):

After `loadGateways()`:

```js
  async function loadIncidents() {
    try { return await tryJson('api/v1/incidents'); }
    catch { return []; }
  }
```

Extend the initial load (line ~59):

```js
  let [fleet, telemetry, edition, plugins, gateways, incidents] = await Promise.all([loadFleet(), loadTelemetry(), loadEdition(), loadPlugins(), loadGateways(), loadIncidents()]);
```

In `paintStats()`, replace the `#stat-alerts` line:

```js
    document.querySelector('#stat-alerts').textContent = String(incidents.filter(incident => !incident.ended_at).length);
```

(The `#sub-alerts` line stays — the sub-line keeps the live warning/offline counts.)

After `paintStats()`'s definition, add the painter (DOM-built, not innerHTML — `detail` and names are API data):

```js
  function formatDuration(ms) {
    const minutes = Math.max(1, Math.round(ms / 60000));
    if (minutes < 60) return `${minutes} min`;
    return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
  }

  function paintIncidents() {
    const body = document.querySelector('#incidents-body');
    body.replaceChildren();
    document.querySelector('#incidents-empty').hidden = incidents.length > 0;
    const cell = text => { const td = document.createElement('td'); td.textContent = text; return td; };
    for (const incident of incidents) {
      const row = document.createElement('tr');
      const started = new Date(incident.started_at);
      const duration = incident.ended_at
        ? formatDuration(new Date(incident.ended_at) - started)
        : t(dict, 'app.incidents.ongoing');
      row.append(
        cell(`${incident.camera_name} · ${incident.site_name || incident.site_id}`),
        cell(started.toLocaleString()),
        cell(duration),
        cell(incident.detail || '—'),
      );
      body.appendChild(row);
    }
  }
```

Call `paintIncidents();` immediately after the existing initial `paintStats();` call. In `refresh()` extend the reload:

```js
      next = await Promise.all([loadFleet(), loadTelemetry(), loadGateways(), loadIncidents()]);
```
```js
    [fleet, telemetry, gateways, incidents] = next;
```

and add `paintIncidents();` after the `paintStats();` call inside `refresh()`.

- [ ] **Step 6: Add the strings** — `web/locales/en.json`:

```json
  "app.incidents.title": "Incidents",
  "app.incidents.subtitle": "Camera disconnects and recoveries, as they actually happened.",
  "app.incidents.camera": "Camera",
  "app.incidents.started": "Started",
  "app.incidents.duration": "Duration",
  "app.incidents.cause": "Cause",
  "app.incidents.ongoing": "Ongoing",
  "app.incidents.empty": "No incidents recorded. Disconnects and recoveries will appear here."
```

es: `"Incidencias"`, `"Desconexiones y recuperaciones de cámaras, tal como ocurrieron."`, `"Cámara"`, `"Inicio"`, `"Duración"`, `"Causa"`, `"En curso"`, `"Sin incidencias registradas. Las desconexiones y recuperaciones aparecerán aquí."`
ru: `"Инциденты"`, `"Отключения и восстановления камер — как они происходили на самом деле."`, `"Камера"`, `"Начало"`, `"Длительность"`, `"Причина"`, `"Продолжается"`, `"Инцидентов не зафиксировано. Отключения и восстановления появятся здесь."`

- [ ] **Step 7: Run the web tests**

Run: `cd web && npm test` — expected: all suites pass, including the four new tests and the i18n parity suite.

- [ ] **Step 8: Commit**

```bash
git add web/app.html web/dashboard-app.js web/locales/en.json web/locales/es.json web/locales/ru.json web/tests/incidents.test.mjs
git commit -m "The Incidents view is alive: real outage history, honest durations"
```

---

### Task 6: Docs, backlog, verification, smoke

**Files:**
- Modify: `docs/RUNNING-LOCALLY.md`
- Modify: `docs/BACKLOG.md`

- [ ] **Step 1: Docs** — in `docs/RUNNING-LOCALLY.md`, after the "Where the database lives" section, add an "Incidents" paragraph: the API records a camera's disconnects and recoveries once a minute (offline reported by the gateway, or silence past `STALE_CAMERA_SECONDS`); history is kept `INCIDENT_RETENTION_DAYS` (default 90, `0` = forever); ongoing incidents are never pruned; there are no incidents for the first stale-window after the API starts, by design.

- [ ] **Step 2: Backlog** — in `docs/BACKLOG.md`, check off `- [ ] Camera disconnect/recovery incident timeline`.

- [ ] **Step 3: Full verification**

Run: `cargo test --workspace` and `cd web && npm test` — everything green; report exact counts.

- [ ] **Step 4: Smoke test** (verify the port is free first with `ss -ltn`; use 8123 or another free one):

```bash
cargo build -p vms-api
(DATABASE_URL=sqlite:/tmp/vms-incidents-smoke.db API_BIND=127.0.0.1:8123 ADMIN_PASSWORD='smoke-test-password' ./target/debug/vms-api &> /tmp/vms-incidents-smoke.log &)
sleep 2
ss -ltn | grep 8123
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8123/api/v1/incidents   # 401
curl -s -c /tmp/smoke-jar -o /dev/null -w '%{http_code}\n' \
  -H 'content-type: application/json' -d '{"password":"smoke-test-password"}' \
  http://127.0.0.1:8123/api/v1/auth/login                                          # 204
curl -s -b /tmp/smoke-jar http://127.0.0.1:8123/api/v1/incidents                   # []
(pkill -f 'debug/vms-api' || true)
rm -f /tmp/vms-incidents-smoke.db /tmp/smoke-jar
```

- [ ] **Step 5: Commit**

```bash
git add docs/RUNNING-LOCALLY.md docs/BACKLOG.md
git commit -m "Write down how incidents are recorded and how long they keep"
```
