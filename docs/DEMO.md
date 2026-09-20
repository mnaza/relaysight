# Demoing both editions on one machine

The product has a free side and a paid side, and the difference is one
service. Community Core — this repository — runs on its own. A commercial
deployment runs the same core next to an entitlement service that says which
plan a customer is on. This page brings both up locally.

The real control plane is private (`relaysight-platform`). What this repository
carries is a stand-in: `deploy/demo-entitlements/`, an nginx that hands out one
prepared entitlement for every customer id. It is enough to show Community Core
receiving a commercial entitlement and acting on it, and it is not an
implementation of a control plane.

## The free side

```
make demo-community
```

API on 8080, dashboard on 8081 (8090 and 8091 if you have the override file
from `docs/RUNNING-LOCALLY.md`). No entitlement service, so the API answers the
Community entitlement itself and never calls out. Log in with
`demo-admin-password`.

The dashboard says **Community Self-Hosted** with unlimited cameras.

## The paid side

```
make demo-commercial            # Hosted Free: three cameras
make demo-commercial PLAN=pro   # Commercial Pro: unlimited
```

The same core, plus the stand-in on 8088. `ENTITLEMENTS_URL` is what changes:
with it set, every entitlement decision goes to that service.

- **Hosted Free** — the dashboard says Hosted Free and counts cameras against
  the limit of three, and the onboarding copy names it.
- **Commercial Pro** — the dashboard says the commercial plan with unlimited
  cameras.

Curl the stand-in directly to see what the API is being told:

```
curl -s localhost:8088/api/v1/entitlements/pilot-customer
```

## Putting a fleet in front of it

Either stack starts empty. In another terminal:

```
make demo-fleet          # five cameras
CAMERAS=9 make demo-fleet
```

It logs in, creates an enrollment, enrolls a gateway over HTTP and posts one
telemetry batch — no camera and no gateway container — then prints the plan and
how many cameras the API kept.

## What the cap actually does

**It drops cameras; it does not refuse anything.** `camera_telemetry` truncates
the batch to the plan's limit and stores what is left. The gateway is not told,
gets no error, and keeps reporting all of them; the ones past the limit simply
never appear in the fleet. Nothing refuses an enrollment over the limit either.

So on Hosted Free, `CAMERAS=5 make demo-fleet` leaves three cameras on the
dashboard and says so. That is today's behaviour, and the demo shows it rather
than staging a refusal that does not exist.

## Proving it without clicking

```
make check-editions
```

Brings the API up three times — no entitlement service, hosted-free, pro —
seeds five cameras into each, and checks what comes back:

| stack | edition | camera limit | cameras kept |
| --- | --- | --- | --- |
| community | `community` | none | 5 |
| hosted-free | `commercial` | 3 | 3 |
| pro | `commercial` | none | 5 |

It uses ports 18085 and 18089, its own throwaway databases, and removes the
container it starts. Verified on 2026-09-20.

## What this demo does not show

Billing, subscriptions, licence signing, resellers, SSO — everything the real
control plane does beyond answering with a plan. The stand-in answers every
customer with the same plan, so multi-tenant behaviour is out of reach here
too.
