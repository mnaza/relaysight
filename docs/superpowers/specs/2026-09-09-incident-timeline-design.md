# Camera incident timeline

**Goal:** The dashboard's Incidents nav link is disabled because no events view
exists, yet the 20-hour hardware run logged 32 real outages. Persist camera
disconnect/recovery history in the store and light up an incidents view that
shows it.

**Decisions taken with the user (2026-09-09):**

- An incident is **offline only**: a camera going unreachable until it
  recovers. Warnings stay a live-status concern, not history.
- Detection is a **single periodic reconciler** — one writer, no
  ingest/sweeper races; up to ~60s latency, with backdated start times.
- Closed incidents are pruned after `INCIDENT_RETENTION_DAYS` (default 90,
  `0` = keep forever); open incidents are never pruned.
- Web scope: the incidents view plus rewiring the overview's "Open
  incidents" stat to count actually-open incidents.

## What exists today

- Camera status arrives in telemetry batches (`HealthStatus`
  healthy/warning/offline plus `last_error`); silence past
  `stale_camera_seconds` (default 75) is derived as offline at read time in
  the `/cameras` and `/fleet` handlers. Nothing records transitions.
- A 60-second background loop already runs `retention_pass` (recordings
  sweep, session sweep).
- The web app has a disabled Incidents nav link (`app.html:29`), an
  `app.events` = "Incidents" locale key, and an "Open incidents" overview
  stat currently showing the live warning+offline camera count.

## Detection

`incident_pass(&AppState)`, called from the existing 60s loop next to
`retention_pass`. For every camera in the store roster
(`store.fleet_cameras()`), effective status follows the same rule the
`/cameras` view uses: **offline iff the last telemetry reported offline, or
the camera has been silent past `stale_camera_seconds`** (judged against the
live in-memory batches, falling back to the stored `last_seen`).

Idempotent reconciliation, no read-modify-write:

- Effective offline → `open_incident(camera_id, started_at, detail)` — a
  no-op when one is already open. `started_at` is backdated to
  `last_seen + stale_camera_seconds` when the cause is silence (the moment
  the camera actually went dark), or the sweep time for an explicitly
  reported offline. `detail` carries the camera's `last_error` when present.
- Effective online (healthy or warning) → `close_incident(camera_id, now)` —
  a no-op when none is open.

No extra debounce: the 60s cadence plus the 75s stale window is the damping.
A flap becomes two short incidents with honest durations.

**Startup grace:** the reconciler skips its work until the API has been up
for one `stale_camera_seconds` window. Without this, the first tick (which
fires immediately) would read pre-restart `last_seen` values before gateways
re-report and open false incidents on every deploy.

## Storage

Migration `0003_incidents.sql`:

```sql
CREATE TABLE incidents (
    id         TEXT PRIMARY KEY,   -- uuid
    camera_id  TEXT NOT NULL,
    started_at TEXT NOT NULL,      -- ts() fixed-width RFC 3339, as everywhere
    ended_at   TEXT,               -- NULL = ongoing
    detail     TEXT
);
CREATE UNIQUE INDEX idx_incidents_open ON incidents(camera_id) WHERE ended_at IS NULL;
CREATE INDEX idx_incidents_started ON incidents(started_at);
```

The partial unique index makes "one open incident per camera" a database
invariant; `open_incident` is `INSERT ... ON CONFLICT DO NOTHING`. Camera and
site names are joined from the existing `cameras` and `sites` tables at query
time, not denormalized — a rename shows history under the current name.

New `Store` methods:

- `open_incident(camera_id, started_at, detail)` — idempotent per the index.
- `close_incident(camera_id, ended_at)` — closes the open incident if any.
- `incidents(limit)` — open incidents first, then closed ones newest-first,
  so an open incident can never be paged out; joined camera/site names; a
  camera row that has vanished from the roster still returns with its ids.
- `delete_closed_incidents_before(cutoff)` — called from `retention_pass`,
  driven by `INCIDENT_RETENTION_DAYS` (env, default 90; `0` disables).

## API

`GET /api/v1/incidents` in the **protected** router group, added to the
`PROTECTED_ROUTES` test table. Response: up to 200 of

```json
{ "camera_id": "...", "camera_name": "...", "site_id": "...",
  "site_name": "...", "started_at": "...", "ended_at": null,
  "detail": "gateway telemetry is stale" }
```

`IncidentView` lives in `vms-domain` alongside the other API types.
`AppState` gains `incident_retention_days: i64` and a startup instant for the
grace window.

## Web

- Enable the Incidents nav link (`href="#incidents"`, drop `disabled`).
- A new `#incidents` panel: a table of camera (name + site), started (local
  time), duration (computed from ended−started, or an "ongoing" badge), and
  cause (`detail`); an empty-state line when there are none.
- Incidents are fetched with the dashboard's existing load/refresh cycle
  (`api/v1/incidents`, empty list on failure).
- The overview's "Open incidents" stat becomes the count of incidents with
  no `ended_at`; its sub-line keeps the live warning/offline counts.
- New `app.incidents.*` keys in en, es and ru.
- Demo mode shows the empty state — no invented incidents; every number on
  that screen stays real.

## Testing

TDD as always.

- Store: open is idempotent (second open is a no-op, pinned by a row count),
  close is idempotent, ordering is open-first-then-newest, pruning removes
  only closed incidents older than the cutoff and never open ones.
- Reconciler: a silent camera opens an incident backdated to
  `last_seen + stale_window`; an explicitly-offline camera opens one; a
  recovered camera closes it; flap → two incidents; the startup grace skips
  the first sweep after boot.
- Router: `GET /api/v1/incidents` is in `PROTECTED_ROUTES` (401 without a
  session); with a session it returns the open incident for a silent camera.
- Restart: an open incident survives an API restart (two AppStates over one
  store file) and closes after recovery.
- Web (jsdom): rows render with names and durations, the ongoing badge shows
  for `ended_at: null`, the stat counts open incidents, the nav link is
  enabled, locales carry the new keys (parity suite).

## Out of scope

Gateway-level incident grouping, warning incidents, alerting
(email/webhook is its own backlog item), per-camera timeline inside the
live modal, demo-mode fake incidents, gateway (as opposed to camera)
incidents.
