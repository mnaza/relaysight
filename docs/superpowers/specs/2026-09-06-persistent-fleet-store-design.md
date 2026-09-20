# Persistent fleet store — SQLite behind a trait, Postgres-ready

Beads epic: `relaysight-vms-9zl`
Backlog item: "Persistent Postgres model for organizations, sites, gateways and cameras"
Decision: SQLite first, behind a `Store` trait, so Postgres becomes one new
implementation later rather than a rewrite. For Community self-hosted, SQLite
is the better default: no extra container, the database is one file, backup is
a file copy.

## Problem

All API state lives in six `RwLock<HashMap>`s in `services/api/src/main.rs`.
A restart of the API:

- invalidates every enrolled gateway token — every site must re-enroll by hand;
- loses every recording manifest — recorded video in object storage becomes
  unfindable, and the retention worker forgets what to delete, leaking storage;
- empties the fleet — the dashboard falls back to demo data until telemetry
  re-arrives, and cameras that are genuinely offline vanish instead of showing
  as offline.

Fleet identity (organization, site, camera) exists only as strings riding
inside telemetry batches; nothing owns it.

## Scope

**Becomes persistent:** organizations, sites, gateways (including their token
hashes), pending enrollments, cameras, recording manifests.

**Stays in memory, deliberately:** latest gateway heartbeats, latest camera
telemetry batches, command queues and command views. A queued command expires
in 2 minutes and telemetry refreshes every poll; persisting them buys nothing.

**Out of scope:** telemetry history (the 7/30-day health item deserves its own
schema), plugin definitions in the DB (separate backlog item), production auth,
edge-side token storage.

## Architecture

- New module `services/api/src/store/mod.rs`: a single `Store` trait speaking
  domain language, object-safe (`Arc<dyn Store>`), async via `async_trait`.
- One implementation now: `SqliteStore` in `services/api/src/store/sqlite.rs`,
  built on `sqlx` with the SQLite driver, runtime `query_as`/`FromRow` (not the
  compile-time macros, which would pin the crate to one database).
- Migrations in `services/api/migrations/`, applied at startup with
  `sqlx::migrate!`. A future `PostgresStore` brings its own migrations dir.
- `AppState` gains `store: Arc<dyn Store>` and loses `enrollments`,
  `gateway_tokens`, `recordings`.
- Configuration: `DATABASE_URL`, default `sqlite:data/vms.db?mode=rwc`. The
  compose file mounts a volume for `data/`. The API fails to start if the
  database cannot be opened or migrated — a half-up API that silently forgot
  its fleet is worse than a crash loop.

### Trait surface (domain methods, no SQL leaks)

Enrollment: `create_enrollment`, `claim_enrollment` (atomic, see below).
Gateways: `upsert_gateway`, `gateway_token_hash(gateway_id)`.
Fleet identity: `upsert_fleet_identity` (org + site + cameras from one
telemetry batch), `list_cameras`, `fleet_identity` (orgs → sites → cameras).
Recordings: `save_recording`, `recording(id)`, `camera_recordings(camera_id)`,
`expired_recordings(now)`, `delete_recording(id)`.

Exact signatures may shift during TDD; the boundary — handlers never see SQL,
the store never sees HTTP — does not.

## Schema

Five tables. IDs are the existing external string IDs (`TEXT PRIMARY KEY`);
timestamps are RFC 3339 `TEXT` (chrono round-trips them; the Postgres impl may
choose `timestamptz`).

```sql
organizations(id TEXT PK, name TEXT NOT NULL)
sites(id TEXT PK, org_id TEXT NOT NULL REFERENCES organizations(id),
      name TEXT NOT NULL, city TEXT NOT NULL)
gateways(id TEXT PK, site_id TEXT NOT NULL REFERENCES sites(id),
         hostname TEXT, version TEXT, token_hash TEXT NOT NULL,
         enrolled_at TEXT NOT NULL, last_seen TEXT)
enrollments(token_hash TEXT PK, org_id TEXT NOT NULL, org_name TEXT NOT NULL,
            site_id TEXT NOT NULL, site_name TEXT NOT NULL, city TEXT NOT NULL,
            expires_at TEXT NOT NULL, claimed INTEGER NOT NULL DEFAULT 0)
cameras(id TEXT PK, gateway_id TEXT NOT NULL REFERENCES gateways(id),
        site_id TEXT NOT NULL, name TEXT NOT NULL,
        manufacturer TEXT, model TEXT, firmware TEXT, codec TEXT,
        width INTEGER, height INTEGER,
        first_seen TEXT NOT NULL, last_seen TEXT NOT NULL)
recordings(id TEXT PK, camera_id TEXT NOT NULL,
           started_at TEXT NOT NULL, ended_at TEXT NOT NULL,
           delete_after TEXT, codec TEXT NOT NULL,
           manifest TEXT NOT NULL)  -- full RecordingManifest as JSON
```

Indexes: `recordings(camera_id, started_at)`, `recordings(delete_after)`,
`cameras(gateway_id)`, `sites(org_id)`.

The recording manifest is stored whole as JSON with the query fields lifted
into columns. Nothing queries individual segments; relational treatment of
segments would be schema for its own sake.

### Tokens are stored hashed

Gateway tokens and enrollment tokens are stored as SHA-256 hashes, never
plaintext. Authorization hashes the presented token and looks the hash up. A
leaked database file leaks no credentials. The bootstrap `GATEWAY_TOKEN`
environment path is unchanged.

## Behavior changes

- **Enrollment** (`POST /api/v1/enrollments`): inserts a row; the plaintext
  token is returned once and never stored.
- **Claim** (`POST /api/v1/gateways/enroll`): atomic in SQL —
  `UPDATE enrollments SET claimed = 1 WHERE token_hash = ? AND claimed = 0
  AND expires_at > ? RETURNING …` — replacing the current read-then-write lock
  dance. On success: upsert organization, site, gateway (with new token hash).
  Distinguish 404 (no such token) from 410 (claimed or expired), as today.
- **Gateway auth** (`authorized_gateway`): bootstrap token check unchanged;
  otherwise compare SHA-256 of the presented token against
  `gateways.token_hash`. One indexed point lookup per request; no cache until
  measurement says otherwise.
- **Telemetry** (`POST /api/v1/cameras/telemetry`): still updates the in-memory
  batch map; additionally upserts org/site names and camera rows
  (`INSERT … ON CONFLICT DO UPDATE`, `first_seen` kept, `last_seen` advanced).
  The same upsert advances `gateways.last_seen` — that column has no other
  writer. A handful of rows per batch is well inside SQLite comfort.
- **`GET /api/v1/cameras` and `GET /api/v1/fleet`**: merge DB identity with
  live telemetry. A camera present in the DB but silent in memory (or stale)
  reports as offline instead of vanishing. Demo fleet only when the DB is also
  empty — first-run experience unchanged.
- **Recording completion**: `gateway_complete_command` writes the manifest row
  (with `delete_after` computed as today) instead of inserting into a map.
- **Timeline / playback**: read from the store.
- **Retention loop**: `expired_recordings(now)` from the store; after the
  storage plugin confirms deletion of every object, `delete_recording(id)`.
  Partial deletion keeps the row and retries next tick, exactly as today.
- Command flow, live sessions, plugin endpoints, TURN: untouched.

## Error handling

The trait returns a `StoreError` (thin: `NotFound` where a handler must
distinguish it, otherwise an opaque internal error carrying context). Handlers
map store failures to 500; nothing panics on a database error. Startup is the
one place that refuses to run without the store.

## Testing

TDD throughout. `SqliteStore` is exercised directly on `sqlite::memory:` with
migrations applied — the tested SQL is the shipping SQL. Router tests keep
their current shape: `test_state()` constructs an in-memory `SqliteStore`.
New tests pin the behaviors this work exists for:

- an enrolled gateway still authorizes against a store reopened from the same
  file (restart survival);
- a recording saved before "restart" is listed and playable after;
- an enrollment token claims exactly once under concurrent claims;
- tokens do not appear in the database in plaintext;
- a DB-known camera absent from telemetry reports offline, not absent;
- the retention query returns exactly the expired manifests.

## Not doing (YAGNI)

Connection pooling knobs, read replicas, a caching layer over token lookups,
per-entity repository traits, sqlx `Any`-driver portability tricks, telemetry
history, and any schema for data nothing reads.
