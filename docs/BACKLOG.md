# MVP backlog

## Done in this prototype

### Product / editions

- [x] Community Self-Hosted vs Commercial service boundary
- [x] Community mode when no entitlement service is configured
- [x] Unlimited camera entitlement for Community Self-Hosted
- [x] Commercial hosted-free entitlement with configurable free camera count
- [x] Commercial paid entitlement prototype
- [x] Gateway receives entitlement during enrollment
- [x] API enforces the same camera entitlement at telemetry ingress
- [x] Landing page explains Community / Cloud / Enterprise separately
- [x] Dashboard displays Community vs hosted/commercial plan state

### Plugin platform

- [x] Open plugin SDK shared by Community and Commercial
- [x] Versioned Plugin Protocol v1
- [x] Out-of-process HTTP plugin runtime / registry
- [x] Plugin manifest + health endpoints
- [x] AI analysis capability contract
- [x] Storage blob capability contract
- [x] Tenant-aware invocation context / connection ID placeholder
- [x] Presigned upload/download storage design
- [x] Example custom-AI HTTP adapter
- [x] Example S3/MinIO/B2-compatible storage plugin
- [x] Plugin dashboard with capability/status/health checks
- [x] Docker plugin profile with MinIO

### Camera / edge

- [x] Rust Axum control API
- [x] Real enrollment creation UI with 30-minute one-time token
- [x] Gateway one-time enrollment → per-gateway bearer token
- [x] Rust edge heartbeat agent
- [x] ONVIF WS-Discovery
- [x] ONVIF GetDeviceInformation / GetCapabilities / GetProfiles / GetStreamUri
- [x] WS-Security UsernameToken PasswordDigest for ONVIF SOAP
- [x] RTSP DESCRIBE / SETUP / PLAY and real frame sampling via Retina
- [x] Real FPS, bitrate, RTP packet-loss and reconnect telemetry
- [x] Stale gateway/camera telemetry becomes offline
- [x] Direct RTSP URL fallback for cameras with broken/disabled ONVIF
- [x] Outbound gateway media-command polling
- [x] On-demand H.264 recording without transcoding
- [x] fMP4 initialization + keyframe-aligned `.m4s` media segments
- [x] Signed direct upload through storage plugin
- [x] Recording manifest/index + camera timeline API
- [x] Signed playback manifest + browser MediaSource playback
- [x] Configurable prototype retention worker through storage plugin delete

### White-label web

- [x] Runtime-configurable brand name, logo URL, palette, custom CSS and locale list
- [x] Configurable hosted free-camera count and gateway install-command template
- [x] English / Spanish / Russian dictionaries
- [x] Marketing landing page
- [x] Fleet dashboard with API → demo-data fallback
- [x] In-dashboard white-label preview editor
- [x] API smoke-test script

## Next — make the demo sellable on real sites

- [ ] On-demand RTSP → WebRTC live session
- [x] Camera disconnect/recovery incident timeline
- [x] Persistent model for organizations, sites, gateways and cameras — done as SQLite behind a `Store` trait (spec: `docs/superpowers/specs/2026-09-06-persistent-fleet-store-design.md`); Postgres becomes a second `Store` implementation when hosted scale calls for it
- [x] Production auth for installer dashboard
- [x] Encrypted persistent gateway token / identity
- [x] Per-camera credential store encrypted at rest on edge — `vms-gateway credentials`, keyed by address, with `CAMERA_USERNAME`/`CAMERA_PASSWORD` as the fallback (spec: `docs/superpowers/specs/2026-09-21-per-camera-credentials-design.md`)
- [x] Video sources that are not discovered cameras — an address the gateway pulls or a stream pushed to it over RTMP/SRT, added from the dashboard, with no password in the control plane (`docs/VIDEO-SOURCES.md`, spec: `docs/superpowers/specs/2026-09-22-video-sources-design.md`); no encoder has pushed to either listener yet, and SRT is behind `--features srt`
- [ ] Hikvision/Dahua compatibility fixtures and device test matrix
- [x] Gateway installer package / update channel — `install.sh` onto a hardened systemd service, signed releases, daily self-update with rollback, image on ghcr (`docs/INSTALL-GATEWAY.md`, `make check-installer`); the first real release waits on the release key
- [x] Gateway revocation and audit log
- [x] Camera decommission flow — retire a revoked gateway's cameras from the roster, as a `retired_at` tombstone a reporting gateway can undo (spec: `docs/superpowers/specs/2026-09-21-camera-decommission-design.md`)
- [x] Both editions demonstrable on one machine — `make demo-community`, `make demo-commercial [PLAN=pro]`, `make check-editions`, against a stand-in entitlement service in `deploy/demo-entitlements/` (`docs/DEMO.md`)
- [x] Relay serves TURN over TLS on 443 (`make check-relay`)
- [x] Gateway relays over TURN TCP/TLS — through a local bridge in the gateway (`make check-gateway-relay` covers TLS; plain TCP has only met a test server); remove it once webrtc-rs supports `turns:` (webrtc-rs#848)
- [x] Relay healthcheck notices an expired or wrong-name certificate, not only a missing TLS listener (`make check-relay` steps 9 and 10)
- [x] install-cert.sh notices a relay whose TLS listener never came up and says to restart it instead of reporting a reload (step 12)

## Plugin productionization

- [ ] Persist plugin definitions/bindings in Postgres instead of only `plugins.d`
- [ ] Per-organization plugin binding UI
- [ ] Vault-backed plugin connection secrets
- [ ] mTLS/service identity for plugin calls
- [ ] Network policies / resource limits / plugin timeouts and circuit breaker metrics
- [ ] AI snapshot/frame scheduler
- [x] Event-sink capability implementation — contract, runtime call, and a reference webhook sink in `relaysight-plugins`
- [x] Storage lifecycle / archive index integration (prototype/in-memory)
- [ ] Persist archive index and lifecycle policies in Postgres
- [ ] Plugin protocol compatibility tests / SDK examples for Python and Go

## Commercial productionization

- [x] Move `commercial/` into a private repository — done 2026-08-31. The control plane is now `relaysight-platform`, the reference plugins are `relaysight-plugins`, and this repository is Community Core alone.
- [ ] Billing provider integration
- [ ] Signed license / subscription validation
- [ ] Reseller hierarchy and multi-tenancy
- [ ] Advanced white-label domains/apps
- [ ] SSO/OIDC/SAML
- [ ] Audit/compliance controls
- [ ] HA control plane and SLA tooling

## After first installer feedback

- [ ] Technician/team RBAC
- [ ] Customer login
- [x] Email/webhook alerts — the control plane raises fleet events and an `event_sink` plugin delivers them, with an outbox that retries and a panel showing where each one got to (`docs/ALERTS.md`, spec: `docs/superpowers/specs/2026-09-23-alerts-design.md`); no real chat service has received one yet
- [ ] Remote NVR access tunnel
- [x] 7/30-day health history — hourly rollups folded from the telemetry the fleet already sends, with uptime and how much of the window was actually reported (`docs/HEALTH.md`); nothing has run for a month yet
- [x] On-demand cloud recording / archive pipeline through storage plugins
- [x] Continuous/event recording policies and rolling archive — a ring buffer on the gateway inside a byte budget, kept by schedule, by an operator saving the last minutes, by a source going quiet or by an AI plugin (`docs/RECORDING.md`, spec: `docs/superpowers/specs/2026-09-23-recording-policies-design.md`); no site has filled a disk with it yet
- [ ] Mobile/PWA packaging if demanded by pilots
