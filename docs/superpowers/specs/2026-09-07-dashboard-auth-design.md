# Production auth for the installer dashboard

**Goal:** The dashboard and every human-facing API route currently answer to
anyone who can reach the port — including minting enrollment tokens, starting
live sessions and fetching signed playback URLs. Put a real login in front of
them.

**Decisions taken with the user (2026-09-07):**

- Single admin account. Multi-user and roles are a later epic.
- Cookie + server-side session, not JWT: an opaque session id in an HttpOnly
  cookie, session rows in the existing SQLite store — revocable, and it
  survives an API restart like everything else in the store.
- Credential is seeded from `ADMIN_PASSWORD` on first boot, changeable from
  the dashboard afterwards, force-resettable from env.

## What exists today

- The SPA (`web/`) is served by nginx, which proxies `/api/` to the API —
  same origin, so cookies work with `SameSite=Lax` and no CORS credentials.
- All SPA fetches use relative URLs (`api/v1/...`).
- Machine flows have their own bearer auth and are out of scope: gateway
  telemetry/heartbeat/enroll/commands (`authorized_gateway`), plugin runtime
  tokens.
- The browser calls `plugins/{id}/storage/downloads` and
  `cameras/{id}/analyze` directly — those are human-facing routes, not
  machine ones.

## Architecture

One new module, `services/api/src/auth.rs`, and an extension of the existing
`Store` trait. Handlers never see argon2 or cookies' internals; the store
never sees HTTP.

**Router split** (`build_router`): an open group and a protected group. The
protected group is wrapped in a single `axum::middleware::from_fn_with_state`
layer that validates the session cookie against the store and returns 401
otherwise. One place to enforce; a new route added to the protected group is
covered by construction.

- Open: `/healthz`, `/api/v1/system/edition`, `POST /api/v1/auth/login`,
  `GET /api/v1/auth/session`, and the gateway machine endpoints (telemetry,
  heartbeat, enroll, commands next/complete) whose existing bearer checks are
  untouched.
- Protected: everything else the browser touches — fleet, cameras,
  enrollments creation, live, analyze, recordings, playback, plugins
  list/reload/health, storage downloads, rtc/config, command view,
  `POST /api/v1/auth/logout`, `POST /api/v1/auth/password`.

## Storage

Migration `0002_auth.sql`:

```sql
CREATE TABLE admin_credential (
    id            INTEGER PRIMARY KEY CHECK (id = 1),  -- single row
    password_hash TEXT NOT NULL,                       -- argon2id PHC string
    updated_at    TEXT NOT NULL
);

CREATE TABLE sessions (
    id_hash    TEXT PRIMARY KEY,  -- SHA-256 hex of the session id
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_expiry ON sessions(expires_at);
```

The session id itself never touches the database — `id_hash` reuses the
existing `token_hash()` discipline, so a leaked database does not leak
usable sessions. Timestamps use the existing `ts()` fixed-width RFC 3339
format.

New `Store` methods: `admin_password_hash()`, `set_admin_password_hash()`,
`create_session(id, expires_at)`, `session_is_valid(id, now)`,
`delete_session(id)`, `delete_all_sessions()`, and
`delete_expired_sessions(now)` called from the existing retention loop.

## Auth flows

- **Login** — `POST /api/v1/auth/login` `{password}`. Verify against the
  argon2id hash. On success mint 32 random bytes (hex) as the session id,
  store its hash with a 7-day expiry, set
  `vms_session=<id>; HttpOnly; SameSite=Lax; Path=/` (plus `Secure` when
  `AUTH_COOKIE_SECURE=true`; default off because local runs are http).
  On failure, 401 after a small in-memory delay that grows with consecutive
  failures (argon2 verification is already slow; this adds friction, not a
  full lockout).
- **Session check** — `GET /api/v1/auth/session` → 204 or 401. The SPA uses
  it to decide whether to show the login view.
- **Logout** — `POST /api/v1/auth/logout`: delete the row, clear the cookie.
- **Change password** — `POST /api/v1/auth/password` `{current, new}`:
  verify current, store new hash, delete all sessions except the calling
  one (re-mint for the caller). A minimum length (12) is the only strength
  rule; no composition theatre.

## Bootstrap and reset

At startup, when `admin_credential` is empty the API hashes `ADMIN_PASSWORD`
from env into it and logs that it did. No `ADMIN_PASSWORD` and no stored
credential → refuse to start, same stance as a failed DB open: an API that
cannot authenticate anyone must not serve. `ADMIN_PASSWORD_RESET=true`
re-seeds the hash from env and wipes all sessions, for the forgotten-password
case; it is logged loudly.

## Web changes

- A login view in `app.html`, shown when `GET /api/v1/auth/session` (or any
  fetch) returns 401: password field, error line, submit posts to login.
- A logout control and a change-password form in the existing settings area.
- Fetches stay relative; the only JS plumbing is a shared 401 handler that
  flips to the login view.

## Dependencies

`argon2` (pure-Rust, maintained by RustCrypto) and `rand` for session ids.
Both in `services/api` only.

## Testing

TDD as always. Store tests: credential hash at rest is a PHC string (raw
table query), sessions expire, password change kills sessions, expired
cleanup. Router tests: a table-driven test asserting 401 without a cookie on
every protected route (the list is in one place so a new route fails the
test if unlisted); login → cookie → 200; wrong password is 401 and delayed;
logout invalidates; gateway machine endpoints still work with bearer tokens
and no cookie; sessions survive an API restart (two AppStates over one
store file, the same trick the fleet-store tests use).

## Out of scope

Multi-user, roles, SSO/IdP, TOTP, audit log (backlog has a separate item),
per-IP rate limiting, HTTPS termination (deployment concern, nginx's job).
