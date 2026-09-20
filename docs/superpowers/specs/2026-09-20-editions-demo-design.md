# A runnable demo of both editions

**Goal:** show the free side and the paid side of the product on one machine,
with the real API answering. Community Core already runs as the free side. The
paid side needs an entitlement service, and the real control plane lives in the
private `relaysight-platform` repository, so this repository gets a stand-in.

Epic `relaysight-vms-04z`. Decided with the user on 2026-09-20: two runnable
stacks rather than a switch or a written walkthrough, and the paid side shows
both Hosted Free (a camera cap) and Commercial Pro (unlimited).

## What exists today

- `services/api/src/entitlements.rs`: `EntitlementResolver::from_env` reads
  `ENTITLEMENTS_URL` and `ENTITLEMENTS_TOKEN`. With no URL it answers a
  Community entitlement locally and never touches the network. With one, it
  GETs `<url>/api/v1/entitlements/<customer_id>` with an optional bearer token
  and deserializes `vms_domain::EditionEntitlement`.
- Three places use the entitlement: `GET /api/v1/system/edition` (resolved for
  the customer id `public`, what the dashboard reads), gateway enrollment (the
  entitlement is handed to the gateway), and camera telemetry.
- **Enforcement is truncation, not refusal.** `camera_telemetry`
  (`services/api/src/main.rs`) truncates the batch to `camera_limit` and stores
  what is left; the gateway is not told. Cameras past the cap simply never
  appear in the fleet. Nothing refuses an enrollment over the cap.
- The dashboard (`web/dashboard-app.js`) already renders three states from that
  one endpoint: Community (unlimited), commercial paid (unlimited), and hosted
  free (used-of-limit, plus onboarding copy that names the limit).
- `scripts/smoke-api.sh` enrolls a gateway and posts one camera over HTTP. No
  hardware, no gateway container.
- `docker-compose.yml` has profiles `plugins` and `edge`; `make community`,
  `make plugins`, `make edge`, `make demo` map onto them. `ADMIN_PASSWORD` has
  no default on purpose: the API refuses to start without a credential.

## Design

### The stand-in entitlement service

`deploy/demo-entitlements/`, served by `nginx:1.27-alpine` (already in the
stack for the web). Files:

- `hosted-free.json` — `edition: commercial`, `plan: hosted-free`,
  `managed: true`, `camera_limit: 3`, the Community capabilities.
- `pro.json` — `edition: commercial`, `plan: commercial-pro`, `managed: true`,
  `camera_limit: null`, plus the capabilities the paid plan claims today
  (`multi_tenant`, `white_label`, `sso`, `priority_support`).
- `entitlements.conf.template` — nginx substitutes `${DEMO_PLAN}` at start-up
  (the image's own `envsubst` templates directory), so any
  `/api/v1/entitlements/<anything>` returns the chosen plan's file as
  `application/json`, and `/healthz` returns 200.

It answers every customer id with the same plan. That is the point: it is a
stand-in for the control plane, not an implementation of one, and it says so in
a comment at the top of its template and in the docs.

### Compose

- `api` gains `ENTITLEMENTS_URL: ${ENTITLEMENTS_URL:-}` and
  `ENTITLEMENTS_TOKEN: ${ENTITLEMENTS_TOKEN:-}`. Empty means Community, which is
  what the resolver already does, so the default stack does not change.
- New service `entitlements`, profile `commercial`, port 8088, `DEMO_PLAN` from
  the environment with `hosted-free` as the default.

### Make targets

```
make demo-community            # no entitlement service: Community Self-Hosted
make demo-commercial           # + stand-in, PLAN=hosted-free (3 cameras)
make demo-commercial PLAN=pro  # + stand-in, Commercial Pro (unlimited)
make demo-fleet                # seed five cameras into whichever stack is up
make check-editions            # headless: prove the three answers differ
```

`ADMIN_PASSWORD` defaults to `demo-admin-password` in the demo targets only —
the plain `make community` path keeps its refusal to boot without one.

### Seeding a fleet

`scripts/demo-fleet.sh`: enroll a gateway over HTTP the way `smoke-api.sh`
does, then post one telemetry batch of five cameras, then read back
`/api/v1/fleet` and print how many the API kept. No hardware and no gateway
container, so the same script works against all three stacks.

### The check

`scripts/check-editions.sh` (`make check-editions`) brings up the API three
times — no entitlement service, hosted-free, pro — seeds five cameras into each
and asserts what `/api/v1/system/edition` and `/api/v1/fleet` say:

| stack | edition | camera_limit | cameras kept |
| --- | --- | --- | --- |
| community | `community` | `null` | 5 |
| hosted-free | `commercial` | 3 | 3 |
| pro | `commercial` | `null` | 5 |

It uses its own ports and its own database volume, refuses to start if the
ports are taken, and always removes what it started — the shape
`scripts/check-relay.sh` already has.

## Docs

- `docs/DEMO.md`: what to run, what to click, and what differs between the
  three, including that the cap is enforced by dropping cameras at ingress
  rather than by refusing anything.
- `docs/EDITIONS.md`: a pointer to `docs/DEMO.md`.
- `README.md`: the demo targets alongside the existing ones.

## Out of scope

Billing, subscriptions, licence signing, or anything else the real control
plane does. Changing how the cap is enforced — the truncation is what ships
today, so the demo shows it as it is. Multi-tenant demos: one customer, one
site.
