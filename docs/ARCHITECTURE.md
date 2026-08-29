# Architecture

## Product split

```text
Community Self-Hosted (GPL core)               Commercial / managed overlay

┌─────────────────────────────┐                 ┌───────────────────────────┐
│ Gateway                     │                 │ private control plane     │
│ API                         │◄── entitlement ─│ plans / billing / license │
│ Fleet / health              │                 │ reseller / SSO / SLA     │
│ Plugin SDK + runtime        │                 └───────────────────────────┘
│ Web UI                      │
└─────────────────────────────┘
```

If no commercial entitlement endpoint is configured, core runs in Community mode with no camera cap.

## Current camera vertical slice

```text
Customer LAN

ONVIF cameras
     │
     ├── WS-Discovery multicast
     ▼
Rust gateway
     ├── GetDeviceInformation
     ├── GetCapabilities(Media)
     ├── GetProfiles
     ├── GetStreamUri
     ├── RTSP DESCRIBE / SETUP / PLAY
     ├── frame sampling → FPS / bitrate / RTP loss
     └── outbound HTTPS bearer-auth telemetry
                         │
                         ▼
                    Rust Axum API
                    ├── one-time enrollment
                    ├── per-gateway token
                    ├── entitlement enforcement
                    ├── plugin registry
                    ├── stale/offline detection
                    └── live fleet projection
                         │
                         ▼
                    Web dashboard
```

## Plugin plane

Plugins are intentionally out of process and shared by both editions.

```text
                        ┌─────────────── AI plugin (Python/CUDA/etc.)
                        │                POST /v1/ai/analyze
                        │
Core API ─ Plugin Host ─┼─────────────── Storage plugin
                        │                presigned PUT/GET/delete
                        │
                        └─────────────── future event/auth/driver plugins
```

The plugin host reads `plugins.d/*.json`. The wire contract lives in `crates/plugin-sdk` and currently uses protocol version 1. A plugin can expose multiple capabilities.

### Why storage uses presigned transfers

The storage plugin supplies a short-lived signed upload/download. Large video bytes should travel directly:

```text
gateway ───────────────────────► S3 / MinIO / B2
             signed PUT

browser ◄─────────────────────── storage
             signed GET
```

The core API handles authorization and metadata, not the bulk media path.

### AI input

AI accepts a `MediaInput` reference (short-lived URL or storage object) so the inference runtime does not need to live inside Rust core. A user can place a GPU inference service on-prem, in their cloud, or next to the VMS.

See `docs/PLUGIN-SDK.md`.

## Enrollment / entitlement path

1. Dashboard creates a one-time enrollment token.
2. Gateway exchanges it for a per-gateway token.
3. Core resolves the customer entitlement.
4. Community: unlimited (`camera_limit = null`).
5. Commercial hosted free: default 3-camera limit.
6. Commercial paid: entitlement may return unlimited or a contracted limit.
7. Gateway applies the limit locally; API applies it again at ingress.

## Security boundary

Implemented in the prototype:

- outbound-only gateway API traffic
- one-time enrollment token
- unique per-gateway bearer token
- plugin bearer token can stay server-side in an environment secret
- plugin code is isolated from the core process
- no camera password entered into cloud onboarding form
- RTSP URLs redacted before telemetry is sent
- stale telemetry turns cameras offline

Still required before production:

- TLS-only deployment and certificate policy
- encrypted persistent gateway identity/token
- authenticated web users, organizations and RBAC
- secret vault / encrypted per-camera credentials on edge
- plugin network policy / egress restrictions / resource limits
- enrollment audit trail and revocation
- persistent database

## Next media slice

```text
Browser requests live
      │
      ▼
Control API creates short-lived live session
      │
      ▼
Gateway opens/uses RTSP stream
      │
      ▼
H.264 RTP → WebRTC publisher/SFU → Browser
```

## Current archive slice

```text
Browser -- record command --> Core API
                              |
                              v
                         command queue
                              | outbound poll
                              v
Gateway -- RTSP H.264 --> Retina (MP4 framing) --> fMP4 muxer
                              |
                              | signed PUT
                              v
                     storage plugin --> object storage
                              |
                              +--> init.mp4
                              +--> seg-00000.m4s ...

Browser <-- playback manifest -- Core API
Browser <------ signed GET ------- object storage
```

The core stores only recording metadata/index. Bulk media bypasses it. The current command queue and archive index are in memory; Postgres persistence is the next productionization step. See `docs/ARCHIVE.md`.

The same storage plugin abstraction is intended for archive segments, snapshots and exported clips.
