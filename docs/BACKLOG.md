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
- [ ] Camera disconnect/recovery incident timeline
- [ ] Persistent Postgres model for organizations, sites, gateways and cameras
- [ ] Production auth for installer dashboard
- [ ] Encrypted persistent gateway token / identity
- [ ] Per-camera credential store encrypted at rest on edge
- [ ] Hikvision/Dahua compatibility fixtures and device test matrix
- [ ] Gateway installer package / update channel (systemd + Docker)
- [ ] Gateway revocation and audit log

## Plugin productionization

- [ ] Persist plugin definitions/bindings in Postgres instead of only `plugins.d`
- [ ] Per-organization plugin binding UI
- [ ] Vault-backed plugin connection secrets
- [ ] mTLS/service identity for plugin calls
- [ ] Network policies / resource limits / plugin timeouts and circuit breaker metrics
- [ ] AI snapshot/frame scheduler
- [ ] Event-sink capability implementation
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
- [ ] Email/webhook alerts
- [ ] Remote NVR access tunnel
- [ ] 7/30-day health history
- [x] On-demand cloud recording / archive pipeline through storage plugins
- [ ] Continuous/event recording policies and rolling archive
- [ ] Mobile/PWA packaging if demanded by pilots
