# Flapping Camera Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A camera that goes up and down every minute sends one `camera_flapping` message out, then one `camera_settled` when it has held a state for ten minutes; every transition in between is stored and shown but not delivered.

**Architecture:** The incident reconciler (`incident_pass` in `services/api/src/main.rs`) already turns incident transitions into events. A new `camera_flaps` table holds one row per camera that has flapped; a small `flaps` module decides, at each transition, whether to deliver, hold, or start a flap, and whether an open flap has settled. The Rust plugin SDK gains the two kinds plus the unknown-kind fallback the plugin doc already promises; the dashboard learns one new delivery string.

**Tech Stack:** Rust (axum, sqlx/SQLite, chrono, serde), Node test runner + jsdom for `web/tests`.

**Spec:** `docs/superpowers/specs/2026-11-10-flapping-camera-design.md`

## Global Constraints

- Kinds on the wire: `camera_flapping`, `camera_settled`; SDK fallback variant `Other` with `as_str() == "other"`.
- Severities: `camera_flapping` is `warning`; `camera_settled` is `info` when the camera is up, `critical` when it is down.
- Titles, exactly: `{name} is flapping at {site}`, `{name} settled and is answering at {site}`, `{name} settled and is still down at {site}`.
- Details: flapping `"{changes} changes in {window_minutes} minutes; single changes are held until it settles"`; settled up `"{changes} changes over {minutes} minutes"`; settled down the same plus `"; down since {HH:MM} UTC"`.
- Metadata: both carry `gateway_id`, `changes`, `flapping_since` (RFC 3339 of the flap row's `started_at`); settled also `state: "up" | "down"`. A held transition carries `held: "flapping"` beside its existing `gateway_id`.
- Settings, all read from the environment in `main` next to `STALE_CAMERA_SECONDS`: `FLAP_TRANSITIONS` (default 3; `<= 0` disables holding entirely), `FLAP_WINDOW_SECONDS` (600), `FLAP_SETTLE_SECONDS` (600).
- Transitions counted toward a flap: incidents of that camera with `started_at >= since` plus incidents with `ended_at >= since`, where `since = max(now - window, settled_at)` (window edge inclusive, my call).
- Settled when `now - last_transition_at >= settle`. Gateways are out of scope.
- Commit messages carry this repository's session trailer (see the last few `git log` entries). Public material only: no camera address, credentials, NDA'd party, tooling marker or home paths in any tracked file; `publish.sh` syncs this plan too and its guard greps for all of them.
- Tests: `cargo test -p vms-api`, `cargo test -p vms-plugin-sdk`, `cd web && npm test`. Before the final commit: `cargo fmt --all --check`, `cargo clippy -p vms-api -p vms-plugin-sdk --lib --tests -- -D warnings`.

## Review Focus

1. A transition exactly `FLAP_WINDOW_SECONDS` old must still count (inclusive edge): test in Task 2.
2. `FLAP_TRANSITIONS=0` or a negative value must deliver everything and never touch the flap table: test in Task 3.
3. A camera that flaps, settles, and flaps again must get a second `camera_flapping`, with its row rewritten to `settled_at NULL`: test in Task 5.
4. A held transition's metadata must keep `gateway_id` beside `held` (merge, not replace): assertion in Task 4.
5. Reading a stored `camera_flapping` or `camera_settled` back through `recent_events` must not fail the Alerts panel's request: test in Task 2.

---

### Task 1: The SDK knows the two kinds and stops choking on unknown ones

**Files:**
- Modify: `crates/plugin-sdk/src/lib.rs:213-233`
- Modify: `docs/PLUGIN-SDK.md:143-146`

**Interfaces:**
- Produces: `FleetEventKind::CameraFlapping`, `FleetEventKind::CameraSettled`, `FleetEventKind::Other` (`#[serde(other)]`); `as_str()` returns `"camera_flapping"`, `"camera_settled"`, `"other"`.

- [ ] **Step 1: Write the failing tests** at the bottom of `crates/plugin-sdk/src/lib.rs` in a `#[cfg(test)] mod tests`:

```rust
#[test]
fn an_unknown_kind_on_the_wire_parses_as_other() {
    let kind: FleetEventKind = serde_json::from_str("\"camera_on_fire\"").unwrap();
    assert_eq!(kind, FleetEventKind::Other);
    assert_eq!(kind.as_str(), "other");
}

#[test]
fn the_known_kinds_round_trip_unchanged() {
    for kind in [
        FleetEventKind::CameraOffline, FleetEventKind::CameraRecovered,
        FleetEventKind::GatewayOffline, FleetEventKind::GatewayRecovered,
        FleetEventKind::CameraFlapping, FleetEventKind::CameraSettled,
        FleetEventKind::Test,
    ] {
        let wire = serde_json::to_string(&kind).unwrap();
        assert_eq!(wire, format!("\"{}\"", kind.as_str()));
        assert_eq!(serde_json::from_str::<FleetEventKind>(&wire).unwrap(), kind);
    }
}
```

- [ ] **Step 2: Run them**: `cargo test -p vms-plugin-sdk` — expected: compile error, `CameraFlapping` and `Other` do not exist.

- [ ] **Step 3: Add the variants.** `CameraFlapping`, `CameraSettled` after `GatewayRecovered`; `#[serde(other)] Other` last, with a doc comment saying a kind this build does not know arrives as this and a sink should still answer. Extend `as_str`.

- [ ] **Step 4: Run**: `cargo test -p vms-plugin-sdk` — PASS. Then `cargo build --workspace` to find exhaustive matches that broke (expected: none outside the SDK; `event_from_row` in `services/api/src/store/sqlite.rs` matches on strings, not the enum, and is Task 2's).

- [ ] **Step 5: Update `docs/PLUGIN-SDK.md`** lines 143-146: list the two new kinds with one clause each (flapping: held transitions follow; settled: which way it ended), and add that a typed Rust sink sees an unknown kind as `FleetEventKind::Other` and should post the title or answer `delivered: false`.

- [ ] **Step 6: Commit**: `git add crates/plugin-sdk/src/lib.rs docs/PLUGIN-SDK.md` — message: "The plugin SDK keeps the promise the doc made about unknown event kinds" with a body naming the two new kinds.

---

### Task 2: The `camera_flaps` table and the store's seven new calls

**Files:**
- Create: `services/api/migrations/0016_camera_flaps.sql`
- Modify: `services/api/src/store/mod.rs` (struct after `IncidentView`; trait methods after `delete_closed_incidents_before`)
- Modify: `services/api/src/store/sqlite.rs` (impls after `delete_closed_incidents_before`; `event_from_row` at line 1466)
- Test: `services/api/src/store/sqlite.rs` tests module (bottom of file; `SqliteStore::in_memory()` is the fixture)

**Interfaces:**
- Produces, in `store/mod.rs`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CameraFlap {
    pub camera_id: String,
    pub started_at: DateTime<Utc>,
    pub changes: i64,
    pub last_transition_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

// on trait Store:
async fn camera_flap(&self, camera_id: &str) -> Result<Option<CameraFlap>, StoreError>;
async fn open_camera_flaps(&self) -> Result<Vec<CameraFlap>, StoreError>;         // settled_at IS NULL, by camera_id
async fn start_camera_flap(&self, camera_id: &str, now: DateTime<Utc>, changes: i64) -> Result<(), StoreError>; // INSERT OR REPLACE: started_at = last_transition_at = now, settled_at NULL
async fn bump_camera_flap(&self, camera_id: &str, now: DateTime<Utc>) -> Result<(), StoreError>; // changes + 1, last_transition_at = now, only where settled_at IS NULL
async fn settle_camera_flap(&self, camera_id: &str, now: DateTime<Utc>) -> Result<(), StoreError>; // settled_at = now
async fn delete_camera_flap(&self, camera_id: &str) -> Result<(), StoreError>;
async fn transitions_since(&self, camera_id: &str, since: DateTime<Utc>) -> Result<i64, StoreError>; // count(started_at >= since) + count(ended_at IS NOT NULL AND ended_at >= since)
```

- [ ] **Step 1: Write the migration** `0016_camera_flaps.sql` with the table from the spec (`camera_id TEXT PRIMARY KEY, started_at TEXT NOT NULL, changes INTEGER NOT NULL, last_transition_at TEXT NOT NULL, settled_at TEXT`) and a two-line comment pointing at the spec.

- [ ] **Step 2: Write the failing tests** in the sqlite tests module:

```rust
#[tokio::test]
async fn a_camera_flap_starts_bumps_settles_and_is_rewritten_by_the_next_start() {
    let store = SqliteStore::in_memory().await.unwrap();
    let t0 = Utc::now();
    assert_eq!(store.camera_flap("cam-1").await.unwrap(), None);
    store.start_camera_flap("cam-1", t0, 3).await.unwrap();
    store.bump_camera_flap("cam-1", t0 + chrono::Duration::seconds(60)).await.unwrap();
    let flap = store.camera_flap("cam-1").await.unwrap().unwrap();
    assert_eq!((flap.changes, flap.settled_at), (4, None));
    assert_eq!(flap.last_transition_at.timestamp(), t0.timestamp() + 60);
    assert_eq!(store.open_camera_flaps().await.unwrap().len(), 1);
    store.settle_camera_flap("cam-1", t0 + chrono::Duration::seconds(700)).await.unwrap();
    assert!(store.open_camera_flaps().await.unwrap().is_empty());
    assert!(store.camera_flap("cam-1").await.unwrap().unwrap().settled_at.is_some());
    // A settled row ignores bumps: it is history, not an open flap.
    store.bump_camera_flap("cam-1", t0 + chrono::Duration::seconds(800)).await.unwrap();
    assert_eq!(store.camera_flap("cam-1").await.unwrap().unwrap().changes, 4);
    store.start_camera_flap("cam-1", t0 + chrono::Duration::seconds(900), 3).await.unwrap();
    let again = store.camera_flap("cam-1").await.unwrap().unwrap();
    assert_eq!((again.changes, again.settled_at), (3, None));
    store.delete_camera_flap("cam-1").await.unwrap();
    assert_eq!(store.camera_flap("cam-1").await.unwrap(), None);
}

#[tokio::test]
async fn transitions_since_counts_starts_and_ends_inside_the_window_edge_included() {
    let store = SqliteStore::in_memory().await.unwrap();
    let now = Utc::now();
    let m = |minutes: i64| now - chrono::Duration::minutes(minutes);
    // Down at -30, up at -25: both outside a 10-minute window.
    store.open_incident("cam-1", m(30), None).await.unwrap();
    store.close_incident("cam-1", m(25)).await.unwrap();
    // Down exactly at the edge, up at -5, down again at -1 (still open).
    store.open_incident("cam-1", m(10), None).await.unwrap();
    store.close_incident("cam-1", m(5)).await.unwrap();
    store.open_incident("cam-1", m(1), None).await.unwrap();
    // Another camera's transitions do not count.
    store.open_incident("cam-2", m(2), None).await.unwrap();
    assert_eq!(store.transitions_since("cam-1", m(10)).await.unwrap(), 3);
    assert_eq!(store.transitions_since("cam-1", m(3)).await.unwrap(), 1);
    assert_eq!(store.transitions_since("cam-1", m(60)).await.unwrap(), 5);
}

#[tokio::test]
async fn the_new_event_kinds_read_back_through_recent_events() {
    let store = SqliteStore::in_memory().await.unwrap();
    for (id, kind) in [("e-1", FleetEventKind::CameraFlapping), ("e-2", FleetEventKind::CameraSettled)] {
        let mut event = /* the module's existing event fixture, or a literal FleetEvent */;
        event.id = id.into();
        event.kind = kind;
        store.record_event(&event, &[], Utc::now()).await.unwrap();
    }
    let kinds: Vec<_> = store.recent_events(10).await.unwrap().into_iter().map(|v| v.event.kind).collect();
    assert_eq!(kinds.len(), 2);
    assert!(kinds.contains(&FleetEventKind::CameraFlapping));
    assert!(kinds.contains(&FleetEventKind::CameraSettled));
}
```

If the sqlite tests module has no `FleetEvent` fixture, write one literal in the test (the fields are in `crates/plugin-sdk/src/lib.rs:255`).

- [ ] **Step 3: Run**: `cargo test -p vms-api camera_flap` and `cargo test -p vms-api transitions_since` and `cargo test -p vms-api the_new_event_kinds` — expected: compile errors (no such methods / no such variant arm → "is not one this build knows" at runtime for the third once it compiles).

- [ ] **Step 4: Implement**: the struct and trait methods in `mod.rs`; the sqlite impls, each one query, timestamps through the file's `ts`/`parse_ts` helpers, rows mapped by a `flap_from_row` helper beside `event_from_row`; two new arms in `event_from_row` (`"camera_flapping"`, `"camera_settled"`). Keep the unknown-kind error arm: the store never writes `other`.

- [ ] **Step 5: Run the three tests** — PASS. Then `cargo test -p vms-api store::` — all store tests PASS, `connect_applies_migrations` included.

- [ ] **Step 6: Commit**: `git add services/api/migrations/0016_camera_flaps.sql services/api/src/store/` — "A camera's flap is a row the pass can act on every minute".

---

### Task 3: The `flaps` module: settings and the verdict at a transition

**Files:**
- Create: `services/api/src/flaps.rs`
- Modify: `services/api/src/main.rs:1-14` (`mod flaps;`)

**Interfaces:**
- Consumes: Task 2's `Store` methods.
- Produces:

```rust
pub struct FlapSettings { pub transitions: i64, pub window: chrono::Duration, pub settle: chrono::Duration }
impl Default for FlapSettings   // 3, 600s, 600s
impl FlapSettings {
    pub fn from_env() -> Self;   // FLAP_TRANSITIONS, FLAP_WINDOW_SECONDS, FLAP_SETTLE_SECONDS; unparsable → default
    pub fn enabled(&self) -> bool;  // transitions > 0
    pub fn window_minutes(&self) -> i64;
}
#[derive(Debug, PartialEq)]
pub enum Verdict { Deliver, Held, Started { changes: i64 } }
/// Called after the transition's incident has been opened or closed.
pub async fn verdict(store: &dyn Store, settings: &FlapSettings, camera_id: &str, now: DateTime<Utc>) -> Result<Verdict, StoreError>;
pub fn has_settled(flap: &CameraFlap, settings: &FlapSettings, now: DateTime<Utc>) -> bool;
pub fn flapping_detail(settings: &FlapSettings, changes: i64) -> String;
pub fn settled_detail(flap: &CameraFlap, settings: &FlapSettings, down: bool) -> String;
```

`verdict` writes the flap row itself: `Held` after `bump_camera_flap`, `Started` after `start_camera_flap`. `settled_detail`'s minutes are `window_minutes + (last_transition_at - started_at).num_minutes()`: the window the flap was noticed in, plus however long it went on (my call; the spec gives the wording, not the arithmetic). The down variant appends `; down since {last_transition_at:%H:%M} UTC`.

- [ ] **Step 1: Write the failing tests** in `flaps.rs`'s `#[cfg(test)] mod tests`, using `crate::store::SqliteStore::in_memory()` and `open_incident`/`close_incident` to lay down transitions:

```rust
#[tokio::test]
async fn the_transition_that_crosses_the_threshold_starts_a_flap_and_the_next_is_held() {
    // down -9m, up -5m, down now: three in a 10-minute window
    ... assert_eq!(verdict(&store, &FlapSettings::default(), "cam-1", now).await.unwrap(), Verdict::Started { changes: 3 });
    let flap = store.camera_flap("cam-1").await.unwrap().unwrap();
    assert_eq!((flap.changes, flap.settled_at), (3, None));
    store.close_incident("cam-1", now + 1 minute);
    assert_eq!(verdict(..., now + 1 minute).await.unwrap(), Verdict::Held);
    assert_eq!(store.camera_flap("cam-1").await.unwrap().unwrap().changes, 4);
}

#[tokio::test]
async fn two_transitions_in_the_window_deliver_as_themselves() { ... == Verdict::Deliver; camera_flap is None }

#[tokio::test]
async fn transitions_before_the_last_settle_do_not_count() {
    // three transitions, start, settle at now; one more transition at now + 1m with window 10m, settle 5m:
    // Deliver, not Started — the old three were already reported.
}

#[tokio::test]
async fn zero_or_negative_transitions_disables_holding() {
    // five transitions in the window, settings.transitions = 0 then -1: Deliver both times, camera_flap None.
}

#[test]
fn has_settled_is_inclusive_at_the_settle_window() { /* 600s → true, 599s → false, settled_at Some → false */ }

#[test]
fn the_details_read_as_the_spec_wrote_them() {
    assert_eq!(flapping_detail(&FlapSettings::default(), 3), "3 changes in 10 minutes; single changes are held until it settles");
    // flap: started_at t, last_transition_at t + 28m, changes 7
    assert_eq!(settled_detail(&flap, &FlapSettings::default(), false), "7 changes over 38 minutes");
    assert_eq!(settled_detail(&flap, &FlapSettings::default(), true), format!("7 changes over 38 minutes; down since {} UTC", (t + 28m).format("%H:%M")));
}
```

- [ ] **Step 2: Run**: `cargo test -p vms-api flaps::` — expected: compile error, no module.

- [ ] **Step 3: Implement `flaps.rs`** per the interfaces. `verdict`: if `!settings.enabled()` → `Deliver` with no store call; else read `camera_flap`; open row → bump, `Held`; otherwise `since = max(now - window, settled_at)`, `transitions_since`, `>= transitions` → start, `Started { changes }`, else `Deliver`.

- [ ] **Step 4: Run**: `cargo test -p vms-api flaps::` — PASS.

- [ ] **Step 5: Commit**: `git add services/api/src/flaps.rs services/api/src/main.rs` — "One rule for the one case that makes people mute alerts".

---

### Task 4: The pass holds transitions and raises `camera_flapping`

**Files:**
- Modify: `services/api/src/main.rs` — `AppState` (line 61), `main` settings (line 160), `retention_loop` (line 2513), `raise_event` (line 2735), `incident_pass` (line 2839), test helpers `test_state_with` (line 3176), every `incident_pass(&state).await` in tests.

**Interfaces:**
- Consumes: Task 3's `FlapSettings`, `verdict`, `Verdict`, `flapping_detail`.
- Produces:
  - `AppState.flap: crate::flaps::FlapSettings` (from `FlapSettings::from_env()` in `main`, `FlapSettings::default()` in `test_state_with`).
  - `async fn incident_pass(state: &AppState, now: DateTime<Utc>)` — `now` replaces the internal `Utc::now()`; `retention_loop` passes `Utc::now()`.
  - `async fn hold_event(state: &AppState, event: FleetEvent)` — `record_event(&event, &[], Utc::now())`, logs `held an event` at info.
  - `fn flap_event(record: &CameraRecord, place: Option<&Place>, flap: &CameraFlap, settings, now, kind, severity, title_tail, detail, extra_metadata) -> FleetEvent` is the builder's choice of factoring; what matters is that `camera_flapping` and `camera_settled` share one metadata shape: `{"gateway_id", "changes", "flapping_since"}` (+ `"state"` for settled).

- [ ] **Step 1: Mechanically rename the call sites**: `sed -i 's/incident_pass(&state)\.await/incident_pass(\&state, Utc::now()).await/; s/incident_pass(&restarted)\.await/incident_pass(\&restarted, Utc::now()).await/' services/api/src/main.rs`, then change the signature and `retention_loop`, add the `flap` field, and run `cargo test -p vms-api incident` — PASS (behaviour unchanged). Commit: "incident_pass takes the clock, as delivery_pass does".

- [ ] **Step 2: Write the failing tests** beside `a_flap_is_two_incidents` (line 7038), on `state_with_sinks` + `fake_sink` so delivery rows are real; `typed_batch` (line 6826) drives the camera. Assert on `state.store.recent_events(10)` (newest first; each `EventView` has `event` and `deliveries`):

```rust
#[tokio::test]
async fn the_third_transition_in_ten_minutes_is_held_and_one_flapping_event_goes_out() {
    // offline, healthy, offline, one pass each with now stepping by 60s and telemetry last_seen = that now
    // events newest first: [camera_flapping, camera_offline(held), camera_recovered, camera_offline]
    assert_eq!(flapping.event.kind, FleetEventKind::CameraFlapping);
    assert_eq!(flapping.event.severity, EventSeverity::Warning);
    assert_eq!(flapping.event.title, "Yard camera is flapping at Bakery");
    assert_eq!(flapping.event.detail.as_deref(), Some("3 changes in 10 minutes; single changes are held until it settles"));
    assert_eq!(flapping.event.metadata["changes"], 3);
    assert_eq!(flapping.deliveries.len(), 1);
    assert_eq!(held.event.metadata["held"], "flapping");
    assert_eq!(held.event.metadata["gateway_id"], "gw-1");
    assert!(held.deliveries.is_empty());
    assert_eq!(first_two_deliveries, 1 each);
    delivery_pass(&state, Utc::now()).await;
    // the sink saw exactly three: offline, recovered, flapping
}

#[tokio::test]
async fn while_flapping_every_further_transition_is_held_and_counted() {
    // continue from a flap: two more passes alternating; both held, flap.changes == 5, sink count unchanged
}

#[tokio::test]
async fn flap_transitions_zero_holds_nothing() {
    // state.flap.transitions = 0; five transitions; every event has one delivery row; camera_flap None
}
```

Use the camera name `Yard camera` and site name `Bakery` the way `a_camera_going_quiet_raises_one_event_and_coming_back_raises_another` (line 7335) gets them, or whatever `typed_batch` gives (read it; adjust the title literals to the fixture's names rather than the fixture to the plan).

- [ ] **Step 3: Run**: `cargo test -p vms-api is_held` and the other two — FAIL: four events with deliveries, no `camera_flapping`.

- [ ] **Step 4: Implement** in `incident_pass`'s `Ok(true)` arm: build the transition event as today, then `match crate::flaps::verdict(state.store.as_ref(), &state.flap, &record.id, now).await` — `Err` → warn and deliver as before; `Deliver` → `raise_event`; `Held` → set `metadata["held"] = "flapping"`, `hold_event`; `Started { changes }` → the same hold, then `raise_event` with the `camera_flapping` event (title `{name} is flapping at {site}`, `flapping_detail`, metadata from the row `camera_flap` returns).

- [ ] **Step 5: Run**: the three tests PASS; `cargo test -p vms-api` all PASS (the existing flap and event tests stand).

- [ ] **Step 6: Commit**: "A flapping camera sends one message out instead of one a minute".

---

### Task 5: Settling, and dropping a flap whose camera is gone

**Files:**
- Modify: `services/api/src/main.rs` — `incident_pass`, after the camera loop.
- Test: beside Task 4's tests.

**Interfaces:**
- Consumes: Task 2's `open_camera_flaps`, `settle_camera_flap`, `delete_camera_flap`; Task 3's `has_settled`, `settled_detail`.
- Produces: a `camera_settled` event per the Global Constraints. Inside the camera loop, collect `down_now: HashMap<String, bool>` (camera id → `stale || reported_offline`) for every camera the loop reached and did not skip as revoked; after the loop, for each open flap: not in `down_now` → `delete_camera_flap`; `has_settled` → `settle_camera_flap`, `raise_event(camera_settled)`; else nothing.

- [ ] **Step 1: Write the failing tests**:

```rust
#[tokio::test]
async fn a_camera_that_holds_up_for_the_settle_window_is_reported_settled_and_answering() {
    // flap as in Task 4 ending healthy at t; pass at t + 599s with fresh healthy telemetry: no settled event;
    // pass at t + 600s with fresh healthy telemetry:
    assert_eq!(settled.event.kind, FleetEventKind::CameraSettled);
    assert_eq!(settled.event.severity, EventSeverity::Info);
    assert_eq!(settled.event.title, "Yard camera settled and is answering at Bakery");
    assert_eq!(settled.event.metadata["state"], "up");
    assert_eq!(settled.deliveries.len(), 1);
    assert!(store.camera_flap("cam-1").settled_at.is_some());
    // the next transition (offline at t + 660s) goes out as itself: one delivery row, no "held"
}

#[tokio::test]
async fn a_camera_that_stays_down_is_reported_settled_and_still_down_as_critical() {
    // flap ending offline; pass at t + 600s with offline telemetry last_seen = then
    severity Critical, title "Yard camera settled and is still down at Bakery", metadata.state "down",
    detail ends with "; down since {t:%H:%M} UTC"
}

#[tokio::test]
async fn a_flapping_camera_whose_gateway_is_revoked_drops_the_flap_without_a_word() {
    // flap; state.store.revoke_gateway("gw-1", t).await; pass at t + 700s:
    // no camera_settled, camera_flap("cam-1") is None.
    // A removed or retired camera leaves fleet_cameras and takes the same path: not in down_now, row dropped.
}

#[tokio::test]
async fn after_a_settle_shorter_than_the_window_the_next_transition_goes_out_as_itself() {
    // state.flap.settle = 120s, window 600s; flap of 3 at t; settle at t + 120s; transition at t + 180s:
    // delivered, not held, no second camera_flapping
}

#[tokio::test]
async fn a_camera_that_flaps_again_after_settling_gets_a_second_flapping_event() {
    // settle, then three more transitions inside a new window: second CameraFlapping, row settled_at None, changes 3
}
```

Telemetry note for the implementer: with `now` moved forward, telemetry older than `stale_camera_seconds` reads as a stale outage and makes a transition of its own, so every forward pass needs a batch whose `last_seen` is that pass's `now`.

- [ ] **Step 2: Run** them — FAIL: no `camera_settled`.

- [ ] **Step 3: Implement** the sweep described under Interfaces.

- [ ] **Step 4: Run**: `cargo test -p vms-api` — PASS.

- [ ] **Step 5: Commit**: "A settled camera says which way it ended up".

---

### Task 6: The dashboard says a transition was held

**Files:**
- Modify: `web/dashboard-app.js:178-186` (`deliveryText`)
- Modify: `web/locales/en.json`, `web/locales/es.json`, `web/locales/ru.json` (after `app.alerts.nosinks`)
- Test: `web/tests/alerts.test.mjs`

**Interfaces:**
- Consumes: an `EventView` as JSON: `metadata` is a top-level field (`EventView` flattens the event), so a held row is `event.metadata?.held === 'flapping'` with `deliveries: []`.
- Produces: `app.alerts.held` = en `"held: the camera is flapping"`, es `"retenido: la cámara parpadea"`, ru `"удержано: камера моргает"`.

- [ ] **Step 1: Write the failing test** in `alerts.test.mjs`, adding to `events` a third entry (`id: 'evt-3', kind: 'camera_offline', severity: 'critical', title: 'Yard camera stopped answering at Bakery', metadata: { gateway_id: 'gw-1', held: 'flapping' }, deliveries: []`) and a fourth (`kind: 'camera_settled', severity: 'critical', title: 'Yard camera settled and is still down at Bakery', deliveries: [delivered]`):

```js
test('a transition held while the camera flaps says so, not "no sink registered"', async () => {
  await startDashboard({ brand, locale: 'en', dict });
  const rows = [...document.querySelectorAll('#alerts-body tr')];
  const held = rows.find(row => row.textContent.includes('stopped answering') && row.textContent.includes('held'));
  assert.ok(held, 'the held transition is shown');
  assert.match(held.textContent, /held: the camera is flapping/);
  assert.doesNotMatch(held.textContent, /no event_sink plugin registered/);
  const settled = rows.find(row => row.textContent.includes('settled and is still down'));
  assert.ok(settled.querySelector('.alert-critical'), 'a camera that settled down is shown as critical');
});
```

Check that the existing two alerts tests still count rows by content, not by index, before adding the entries.

- [ ] **Step 2: Run**: `cd web && npm test -- tests/alerts.test.mjs` — FAIL on the held text; `tests/i18n.test.mjs` passes until the key is added to only one locale.

- [ ] **Step 3: Implement**: in `deliveryText`, before the `nosinks` return: `if (event.metadata?.held === 'flapping') return t(dict, 'app.alerts.held');`. Add the key to the three locales.

- [ ] **Step 4: Run**: `cd web && npm test` — all PASS, i18n parity included.

- [ ] **Step 5: Commit**: `git add web/dashboard-app.js web/locales/` — "The Alerts panel says why a transition was not sent".

---

### Task 7: Docs, and the whole thing verified

**Files:**
- Modify: `docs/ALERTS.md` (the table at line 23; a new section after "What happens when a sink is down"; the "Nothing deduplicates" bullet at line 125)
- Modify: the private working-notes file at the repository root, the one `publish.sh` excludes (the "Nothing deduplicates a camera that flaps" line in its alerts paragraph)

- [ ] **Step 1: `docs/ALERTS.md`**: two table rows for `camera_flapping` / `camera_settled`; a section "A camera that flaps" in the file's voice: what goes out (one, then one, which way it ended), the three settings with defaults and that `FLAP_TRANSITIONS=0` turns it off, that the dashboard still shows every transition marked as held, and that gateways are not covered. Replace the "Nothing deduplicates" bullet under *What has never run* with one saying what does now and that no site has run it.

- [ ] **Step 2: The working-notes file**: replace "Nothing deduplicates a camera that flaps." with one sentence: a flapping camera is collapsed to one message out and one when it settles since 2026-11-10, tested in-process, never run on a site.

- [ ] **Step 3: Verify, fresh**:

```bash
cargo fmt --all --check
cargo clippy -p vms-api -p vms-plugin-sdk --lib --tests -- -D warnings
cargo test -p vms-api -p vms-plugin-sdk
cd web && npm test
git diff master --stat
git diff master | grep '^+' | grep -inEf <(sed -n "s/^LEAKS=.*'|\{0,1\}\(.*\)'$/\1/p" publish.sh) || echo clean
git diff master | grep '^+' | grep -in '/home/' || echo clean
```

Expected: fmt silent, clippy no warnings, all tests `ok`, both greps print `clean` (the pattern file is the guard's own `LEAKS` lines, one per line). `publish.sh` syncs `docs/superpowers/` too, so the spec and this plan are inside the guard; the private notes file is excluded by name and may say what it likes.

- [ ] **Step 4: Commit**: `git add docs/ALERTS.md` plus the notes file — "docs: a flapping camera is one message out and one when it settles".
