# Users, roles, and a customer who can log in

**Goal:** there is one password. Everybody who touches the dashboard shares
it, every audit row says `admin`, and a customer who wants to watch their own
cameras has to be handed the keys to the whole fleet. That is the state of the
thing an installer would be selling.

This gives the control plane users, three roles, and a login scoped to one
customer.

## What exists today

- `admin_credential`, one row by construction, argon2id. `POST
  /api/v1/auth/login` takes `{password}` and nothing else.
- `sessions`, keyed by the SHA-256 of a session id, with no idea who the
  session belongs to.
- One middleware, `require_session`, in front of every non-gateway route:
  either you are in or you are not.
- Audit rows carry an actor string, always `admin` for anything from the
  dashboard.
- `ADMIN_PASSWORD` seeds the credential on first boot and is ignored after.

## Design

### Users

```sql
CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,   -- lowercased on the way in
    password_hash TEXT NOT NULL,          -- argon2id, as today
    role          TEXT NOT NULL,          -- owner | technician | viewer
    customer_id   TEXT,                   -- set: sees only this customer
    created_at    TEXT NOT NULL,
    disabled_at   TEXT
);
```

Sessions gain the user they belong to, so a request knows who is asking and
the audit row can say so.

**The single admin becomes a user.** On first boot after this, the existing
`admin_credential` hash is carried over into an `owner` user whose email is
`ADMIN_EMAIL` (default `admin@localhost`). Nobody's password changes. Login
now takes an email as well, which is a break for anyone who had bookmarked the
old form, and it is a break worth taking before there are installs to break:
the alternative is a login that means "the one password" forever.

### Roles

| | |
| --- | --- |
| **owner** | everything, including users and plugins |
| **technician** | runs the fleet: enrol gateways, add sources, set recording policies, record, watch, clip, retire cameras |
| **viewer** | reads: the fleet, incidents, alerts, health, recordings and playback |

Three, not thirty. A permission per endpoint is a matrix nobody maintains and
everybody grants everything on. The split that matters is: who can *change the
system* (owner), who can *operate the fleet* (technician), and who can only
look (viewer).

Enforcement is by router group rather than by a check inside each handler, so
a new endpoint has to be put in a group to exist at all, and forgetting is a
compile-time nuisance rather than a silent hole.

### A customer who can log in

A user with `customer_id` set sees that customer and nothing else: the fleet,
its cameras, its incidents, its health, its recordings. Everything else
answers 404 rather than 403 — a customer should not learn that another
customer exists by being told they may not see it.

Scoping is applied where the data is read, not in the UI. A dashboard that
hides a row is not access control.

### Audit

The actor becomes the user's email. Every row that says `admin` today says who
it actually was, which is the point of having users at all.

## Testing

TDD as always.

- Seeding: an install with the old single credential comes up with one owner
  whose password still works; a fresh install seeds from `ADMIN_PASSWORD`;
  neither re-seeds on the next boot.
- Login: by email, case-insensitively; a disabled user cannot; a wrong
  password is throttled exactly as before and audited with the email.
- Roles: a viewer cannot enrol a gateway, add a source, set a policy or manage
  users; a technician can do all of those and cannot manage users or plugins;
  an owner can do everything. Each refusal is a status, not a 500.
- Scoping: a customer user sees only their own sites, cameras, incidents,
  health and recordings, and gets 404 for another customer's camera.
- Sessions: a session survives a restart, still knows its user, and dies with
  the user being disabled.
- Web: the login form takes an email; the user list is owner-only; a viewer
  does not see buttons they cannot use — belt as well as braces.

## Out of scope

SSO and OIDC, which are their own epic and a commercial one. Per-site roles:
the split is per customer, and a technician is trusted with the fleet they are
given. Password reset by email, which needs mail the control plane
deliberately does not have — an owner sets a new password instead. API tokens
for people, as opposed to the gateway tokens that already exist.
