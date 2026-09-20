# Community vs Commercial editions

The product is deliberately split at a service boundary rather than with `if paid` checks throughout the VMS.

## Community Self-Hosted — open source

Community Core runs without an entitlement service. When `ENTITLEMENTS_URL` is absent, the API returns a Community entitlement automatically:

- self-hosted
- no camera-count cap
- ONVIF discovery and RTSP health
- edge gateway
- plugin SDK/runtime
- custom AI plugins
- custom storage plugins
- local/basic users as the auth layer is implemented

The intended operator owns the infrastructure, upgrades, backups and support burden.

## Commercial — non-free

Commercial deployment starts the same open core plus a private entitlement/control-plane service. Core only knows the versioned entitlement response; it does not contain subscription logic.

Prototype commercial plans:

### Hosted Free

- managed cloud
- first 3 cameras free by default
- same plugin SDK as Community
- upgrade path to paid entitlement

### Commercial Pro / Enterprise

- managed hosting/upgrades
- paid/unlimited camera entitlement
- multi-tenant reseller hierarchy
- advanced white-label
- billing/subscription management
- SSO
- audit/compliance controls
- HA
- SLA / priority support

These capabilities are represented in the commercial entitlement but most are still backlog items.

## Boundary

```text
                         ┌──────────────────────────────┐
                         │      Community Core          │
                         │                              │
Cameras → Gateway ──────►│ API / Fleet / Plugin Host  │────► Web UI
                         │                              │
                         └──────────────┬───────────────┘
                                        │ optional
                                        │ ENTITLEMENTS_URL
                                        ▼
                         ┌──────────────────────────────┐
                         │  Commercial Control Plane   │
                         │ proprietary / private       │
                         │                              │
                         │ plans / billing / licenses  │
                         │ reseller / SSO / SLA        │
                         └──────────────────────────────┘
```

Removing the commercial service must leave a useful self-hosted VMS rather than a crippled trial.

## Entitlement contract

`EditionEntitlement` contains:

- `edition`: `community | commercial`
- `plan`
- `self_hosted`
- `managed`
- `camera_limit`: `null` means unlimited
- `capabilities[]`

The gateway receives this during enrollment and applies the returned camera limit locally. The API enforces the same entitlement on telemetry ingress: a batch over the limit is truncated and the rest stored, so cameras past the cap never reach the fleet and the gateway is told nothing.

## Seeing both sides

`make demo-community` and `make demo-commercial` run the free and the paid side on one machine, against a stand-in for the control plane that lives in this repository. `docs/DEMO.md` has the detail; `make check-editions` proves the three plans answer differently without a browser.
