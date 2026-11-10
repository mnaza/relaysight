# Signing in with somebody else's identity provider

Every account here has a password this system stores, which is the thing an
organisation with an identity provider has spent years getting rid of. A
technician who leaves should stop being able to sign in because HR turned
their account off, not because somebody remembered to open the fleet
dashboard.

This is OpenID Connect: authorization code with PKCE, against one provider.

## Turning it on

| | |
| --- | --- |
| `OIDC_ISSUER` | the provider. Everything else about it is discovered from here |
| `OIDC_CLIENT_ID` | this application, as registered there |
| `OIDC_CLIENT_SECRET` | its secret |
| `OIDC_REDIRECT_URL` | where the provider sends the browser back: `https://your-host/api/v1/auth/oidc/callback` |
| `OIDC_AUTOCREATE_ROLE` | unset means only people who already have an account |

No issuer means no SSO, and the login form is exactly what it was. An issuer
without a client id or a redirect URL is refused at startup with a line in the
log rather than half-enabled: a button that goes nowhere is worse than no
button.

`GET /api/v1/auth/methods` says what is available, and the login page shows
the button only when something answers for it. The password form stays either
way — somebody has to be able to get in when the provider is down.

## Who gets in

**Only somebody who already has an account**, unless you say otherwise. An
identity provider answering for `anyone@gmail.com` is not a reason to create
an owner, and the first version of every SSO integration that trusts the
provider's user list ends up with one.

`OIDC_AUTOCREATE_ROLE=viewer` opts into the other behaviour: a first sign-in
creates an account with that role. `viewer` is the only sensible value, and
the choice is the operator's.

Roles stay this system's to decide. A provider's groups are not a permission
model, and nothing here maps them to one.

**An address the provider has not verified is refused.** Providers where
somebody can type their own email say so in `email_verified`, and a client
that ignores it lets whoever typed it sign in as whoever owns that address —
as a new account where autocreate is on, and as the existing account where it
is not. A provider that omits the claim entirely says nothing, and is
believed; one that says `false` is refused.

An account that came from the provider has no password: its stored hash is a
marker nothing can verify against, so `POST /api/v1/auth/login` cannot be used
to get in as it by any means rather than by policy. A disabled account is
refused whatever the provider says.

## What the callback checks

Each of these is a refusal, and each has a test that fails without it:

- the `state` is one this system started, and it is good exactly once
- the attempt is younger than ten minutes
- the provider calls itself the issuer that was configured
- the ID token's signature verifies against the provider's JWKS
- the `iss`, the `aud` and the `exp` are what they must be
- the `nonce` matches the one this sign-in asked for
- the provider has not said `email_verified: false`

The browser is told it was refused. *Why* goes to the log and the audit log:
what went wrong at the provider is a matter for whoever runs this, not for
whoever pressed the button.

Every sign-in lands in the audit log as `login.ok` with `sso` against it, and
every refusal as `login.sso.refused`.

## Trying it without an account anywhere

This repository carries a provider that authenticates nobody: discovery,
authorize, token and JWKS, signing with a key generated at startup.

```bash
make check-sso
```

starts it, starts the control plane, and drives a whole round trip with curl
— a successful sign-in that produces a working session, an invented callback
that is refused, and an email with no account that is refused without a
cookie. Three processes and real HTTP between them.

To sign in by hand, run it yourself and point the API at it:

```bash
cargo run -p vms-fake-oidc -- 19555 someone@example.test
```

It prints the issuer, the client id and the secret to set. It refuses to run
without being told a port, because the one thing worse than a provider that
authenticates nobody is one doing it on a port somebody was using for
something real. **Never run it anywhere that matters.**

## Against Keycloak

```bash
make check-sso-keycloak
```

starts Keycloak in a container, gives it a realm, a client and two people —
one whose email it has verified and one whose it has not — and drives its own
login form with curl. The verified one is signed in and autocreated as a
viewer; the unverified one is refused with no account behind it; and an
account turned off here is refused although Keycloak still says yes.

It needs Docker and it takes about a minute, most of it waiting for Keycloak.
It is the only check here that has met a provider this repository did not
write, which is why it exists: the first time it ran, it found that an
unverified email was being let in.

## What this does not do

SAML. Logout at the provider — signing out here ends the session here, which
is what the button says. Group-to-role mapping. Refresh tokens: the session
cookie is this system's own and its lifetime is this system's business. More
than one provider at a time.

## What has been met, and what has not

**Keycloak 26 has signed somebody in** — a realm, a client, its own login
form, its own RS256 keys, `make check-sso-keycloak`. It found something
immediately: an email Keycloak had not verified was being let in and given an
account. That is fixed, and the check now fails if it comes back.

**Entra ID, Okta, Auth0, Google — none of them.** Each has its own idea of
which claims arrive. The one to expect first: Entra does not always send
`email`, preferring `preferred_username` or `upn`, and a sign-in against it
will be refused here with "the provider did not say who that was" until
somebody decides which claim should name a user there. That decision is not
guessed at in advance.

Nothing has run against two providers, or against a provider with rotating
signing keys, or with more than a handful of accounts. The JWKS is fetched per
sign-in rather than cached, so rotation should work; nothing has proven it.
