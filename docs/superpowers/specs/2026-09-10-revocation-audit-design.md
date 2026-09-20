# Gateway revocation and audit log

**Goal:** Enrolled gateway tokens now persist on the edge indefinitely
(encrypted identity), so evicting a gateway must be possible from the
dashboard — and security-relevant events need a durable trail. Today nothing
can invalidate an enrolled token short of re-enrolling the same id, and the
shared bootstrap `GATEWAY_TOKEN` admits any gateway id unconditionally.

**Decisions taken with the user (2026-09-10):**

- **Revoke blocks the id; a fresh enrollment clears it.** Revocation sets a
  tombstone and clears the token: every bearer credential is refused for
  that gateway id, including the shared bootstrap token. A fresh
  admin-issued enrollment token is the un-revoke — enrolling the same id
  clears the tombstone and mints a new token, so the return path is gated
  on admin action by construction.
- **Audit = security events, API only.** Login success/failure, password
  change/forced reset, enrollment token created, gateway enrolled,
  gateway revoked. Served at `GET /api/v1/audit`; a dashboard viewer is a
  later follow-up.

## What exists today

- `gateways` (0001) has `token_hash` ('' = never enrolled) and no
  revocation concept. `authorized_gateway` (main.rs) accepts the shared
  `GATEWAY_TOKEN` for ANY gateway id (pinned by
  `the_shared_token_admits_any_gateway_id`), else the per-gateway token via
  `verify_gateway_token`. `enroll_gateway` rotates the token on
  re-enrollment.
- `GET /api/v1/gateways` returns only live in-memory heartbeats — after an
  API restart the list is empty until gateways re-report, and there is no
  store-backed roster in the response.
- The dashboard's gateways grid renders those heartbeats
  (`renderGateways()` in web/dashboard-app.js).
- The retention loop prunes recordings, sessions and incidents; the
  incidents work established the pattern for store-backed views.

## Storage

Migration `0004_revocation_audit.sql`:

```sql
ALTER TABLE gateways ADD COLUMN revoked_at TEXT;  -- NULL = not revoked

CREATE TABLE audit_log (
    id     TEXT PRIMARY KEY,   -- uuid
    at     TEXT NOT NULL,      -- ts() fixed-width RFC 3339
    actor  TEXT NOT NULL,      -- 'admin' | 'gateway:<id>' | 'system'
    action TEXT NOT NULL,      -- fixed strings, below
    subject TEXT NOT NULL,     -- what it acted on ('' when none)
    detail TEXT                -- human context; never a password or token
);
CREATE INDEX idx_audit_at ON audit_log(at);
```

Action strings, fixed: `login.ok`, `login.failed`, `password.changed`,
`password.reset`, `enrollment.created`, `gateway.enrolled`,
`gateway.revoked`.

New `Store` methods:

- `revoke_gateway(gateway_id, now)` — sets `revoked_at`, clears
  `token_hash`. NotFound when the id is not in the roster.
- `gateway_revoked(gateway_id) -> bool` — the tombstone check; false for
  unknown ids.
- `enroll_gateway` (existing) additionally clears `revoked_at` — the
  un-revoke.
- `record_audit(at, actor, action, subject, detail)` — insert one row.
- `audit_entries(limit) -> Vec<AuditView>` — newest first.
- `delete_audit_before(cutoff)` — pruning; called from `retention_pass`
  only when `AUDIT_RETENTION_DAYS > 0`. **Default 0 = keep forever** — it
  is an audit log.

## Authorization change

`authorized_gateway` checks the tombstone first: a revoked gateway id is
refused before the shared-token comparison, so the bootstrap secret no
longer covers a revoked box. Non-revoked ids keep today's behavior exactly
(the pinned shared-token test stays green).

Known limit, documented in RUNNING-LOCALLY: the two id-less machine plugin
endpoints (`plugins/{id}/ai/analyze`, `storage/uploads`) cannot check a
tombstone, but a revoked gateway's own token no longer matches
`verify_any_gateway_token` (its hash is cleared), so only the shared
bootstrap secret still reaches them — the same trust level that secret has
today.

## API

- `POST /api/v1/gateways/{gateway_id}/revoke` — protected group +
  `PROTECTED_ROUTES`. 204 on success, 404 for an unknown id. Writes
  `gateway.revoked` to the audit log.
- `GET /api/v1/gateways` — becomes roster-merged like `/cameras`:
  `GatewayView` in vms-domain:

```json
{ "gateway_id": "...", "site_id": "...", "hostname": null, "version": null,
  "enrolled": true, "revoked_at": null,
  "last_seen": "...", "online": true,
  "site_name": "...", "customer_name": "..." }
```

  built from the store roster (all known gateways) joined with sites/orgs
  for names, merged with live heartbeats for `online`/freshest `last_seen`
  (online iff a live heartbeat is within the stale window). The response
  type changes from `Vec<GatewayHeartbeat>` — the dashboard is the only
  consumer and is updated in the same epic.
- `GET /api/v1/audit` — protected group + `PROTECTED_ROUTES`. Newest
  first, capped 500. `AuditView { at, actor, action, subject, detail }` in
  vms-domain.

## Audit writes

Fire-and-forget from the handlers: a failed audit insert logs a `warn!`
and never fails the request. Sources:

- `auth.rs` login handler → `login.ok` / `login.failed` (actor `admin`,
  subject '').
- `auth.rs` change-password → `password.changed`. `seed_admin_credential`
  forced reset → `password.reset` (actor `system`).
- `create_enrollment` → `enrollment.created` (subject: site id; detail:
  customer/site names — never the token).
- `gateway_enroll` → `gateway.enrolled` (actor `gateway:<id>`).
- revoke handler → `gateway.revoked` (actor `admin`, subject gateway id).

## Web

The gateways grid consumes the new `GatewayView` list: every known gateway
renders (enrolled-but-silent ones show offline instead of vanishing after
an API restart), a "revoked" badge replaces the status pill for revoked
ones, and each non-revoked enrolled gateway gets a Revoke button behind a
`confirm()` dialog that POSTs the revoke endpoint and refreshes. New
`app.gateways.*` keys in en/es/ru.

## Testing

TDD as always.

- Store: revoke sets the tombstone and clears the token; revoking an
  unknown id is NotFound; re-enrollment clears the tombstone and mints a
  working token; audit rows insert and list newest-first; pruning respects
  the cutoff and 0-disables.
- Router: both new routes in `PROTECTED_ROUTES` (401 table); a revoked
  gateway's telemetry is 401 with its old token AND with the shared
  bootstrap token; a non-revoked gateway still passes the shared token;
  re-enrollment restores access end to end; each audited event lands a row
  (login ok/failed, password change, enrollment created, enrolled,
  revoked) asserted through `GET /api/v1/audit`.
- Restart: a revocation survives an API restart (two AppStates over one
  store file).
- Web (jsdom): the grid renders roster gateways including
  enrolled-but-offline ones, the revoked badge shows, the revoke button
  POSTs the right endpoint; locale parity.

## Out of scope

Audit dashboard viewer, unrevoke without re-enrollment, per-IP/user-agent
audit fields, gateway-side reaction to revocation beyond 401s, auditing
plugin invocations, rate limiting.
