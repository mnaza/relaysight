# Signing in with somebody else's identity provider

**Goal:** every account here has a password this system stores, which is the
thing an organisation with an identity provider has spent years getting rid
of. A technician who leaves should stop being able to sign in because HR
turned off their account, not because somebody remembered to open the fleet
dashboard and turn theirs off too.

**Decided with the user (2026-09-24):** OIDC, with a test provider running
locally so the whole path can be exercised without an account anywhere.

## What exists today

- `users`, with an argon2id hash each, three roles, and an optional customer
  scope (`docs/USERS.md`).
- Sessions are a cookie over a hashed session id, and know which user they
  belong to.
- `POST /api/v1/auth/login` takes an email and a password, throttles failures,
  and audits both outcomes.

## Design

### The flow

Authorization Code with PKCE, against a provider named by its issuer:

1. `GET /api/v1/auth/oidc/start` builds the authorize URL — `state`, `nonce`,
   `code_challenge` — remembers them for ten minutes, and redirects.
2. The provider authenticates whoever it is and redirects back to
   `GET /api/v1/auth/oidc/callback?code&state`.
3. The callback exchanges the code at the token endpoint, verifies the ID
   token's signature against the provider's JWKS, and checks `iss`, `aud`,
   `exp` and the `nonce` it remembered.
4. The `email` claim names the user. A session cookie is set, and the browser
   is sent to the dashboard.

PKCE on a confidential client is belt and braces, and costs a hash.

### Who gets in

**Only somebody who already has an account**, by default. An identity provider
answering for `anyone@gmail.com` is not a reason to create an owner — and the
first version of every SSO integration that trusts the provider's user list
ends up with one.

`OIDC_AUTOCREATE_ROLE` opts into the other behaviour: a first sign-in creates
a user with that role. `viewer` is the only sensible value and the
configuration is the operator's to make.

An SSO user has no password. Their stored hash is a marker no password can
verify against, so `POST /api/v1/auth/login` cannot be used to get in as them
by any means, rather than by policy.

### Configuration

| | |
| --- | --- |
| `OIDC_ISSUER` | the provider; everything else is discovered from it |
| `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET` | this application, as registered there |
| `OIDC_REDIRECT_URL` | where the provider sends the browser back |
| `OIDC_AUTOCREATE_ROLE` | unset means "only people who already have an account" |

Unset issuer means no SSO, and the login form is exactly what it is now.
`GET /api/v1/auth/methods` says which are available, so the page offers a
button only when there is something behind it.

### A provider to test against

A real provider needs an account somewhere and a browser with a human in it.
So this repository carries a small one: discovery, authorize (which
authenticates nobody and redirects straight back), token, and JWKS, signing
with a key generated at startup. It runs in the API's own tests, and
`make check-sso` runs it as a process for a round trip that is not mocked
anywhere.

It authenticates nobody. That is the point of it, and the reason it refuses to
run unless it is told the port to listen on.

## Testing

TDD as always.

- Discovery is fetched once and the endpoints come from it.
- The callback refuses: an unknown `state`, a replayed `state`, a `nonce` that
  does not match, an expired token, a token signed by another key, a token for
  another audience, a token from another issuer.
- An email with no account is refused when autocreate is off, and creates a
  user with the configured role when it is on.
- A user who signed in through the provider cannot sign in with a password.
- A disabled user is refused whatever the provider says.
- The whole round trip against the local provider, in process and again as
  processes under `make check-sso`.

## Out of scope

SAML. Logout at the provider (RP-initiated logout) — signing out here ends the
session here, which is what the button says. Group-to-role mapping: the role
is this system's to decide, and a provider's groups are not a permission
model. Refresh tokens: the session cookie is this system's own, and its
lifetime is this system's business.
