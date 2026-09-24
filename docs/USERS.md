# People, roles, and a customer who can log in

There used to be one password. Everyone who touched the dashboard shared it,
every audit row said `admin`, and giving a customer a way to watch their own
cameras meant giving them the whole fleet.

## Roles

| | |
| --- | --- |
| **owner** | everything: people, plugins, the audit log |
| **technician** | runs the fleet — enrol gateways, add sources, set recording policies, record, watch, clip, retire cameras |
| **viewer** | reads: the fleet, incidents, alerts, health, recordings and playback |

Three, not thirty. A permission per endpoint is a matrix nobody maintains and
everybody ends up granting in full.

Enforcement is by router group rather than by a check inside each handler, so
a route has to be put in a group to exist at all. Where it belongs is a
decision somebody makes rather than one they forget.

## A customer who can log in

A user can be scoped to a customer. They then see that customer's fleet,
cameras, gateways, incidents, alerts and history, and nothing else. Another
customer's camera answers **404, not 403**: a customer should not be able to
enumerate somebody else's camera ids by being told they may not look at them.

The filtering is where the data is read. A dashboard that hides a row is not
access control.

**A scoped login is a viewer.** A scoped technician is refused at creation:
scoping covers reading, and an account that could act would be able to touch
the very fleet the scope exists to keep it away from.

## Upgrading from the single password

Nothing changes about the password itself. On the first start after this, the
stored credential becomes an owner account whose email is `ADMIN_EMAIL`
(default `admin@localhost`), and the login form gains an email field.

```bash
ADMIN_EMAIL=you@example.com     # the owner's login, first boot only
ADMIN_PASSWORD=…                # as before: seeds a fresh install
ADMIN_PASSWORD_RESET=true       # sets the owner's password from the env, revokes sessions
```

A forced reset writes to the account the login path reads. It used to write to
the single-credential row, which after this change would have looked like it
worked and changed nothing.

## What an owner can do to people

Add somebody with a role and an optional customer scope, turn an account off,
turn it back on, and set a new password. Turning an account off ends its live
sessions immediately, as does changing its password — not whenever the session
happens to expire.

An owner cannot disable or demote themselves. That is a support call nobody
can answer.

## What is still shared

- **The gateway token.** Gateways authenticate with their own per-gateway
  bearer, not with any of this; `GATEWAY_TOKEN` remains the bootstrap secret.
- **Plugin tokens**, which belong to the plugins.
- **Password reset by email.** There is no mail in the control plane on
  purpose, so an owner sets a new password instead.
- **SSO and OIDC**, which are their own epic.
- **Per-site roles.** The split is per customer; a technician is trusted with
  the fleet they are given.

## What has not been proven

Nobody has run this with more than a handful of accounts, and no real customer
has ever logged into a scoped account. The scoping is tested by making two
customers and checking that one cannot see the other — which is the right
test, and is not the same as a year of somebody trying.
