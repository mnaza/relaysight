# White-label CCTV VMS — Community + Commercial prototype

`RelaySight` is only the default demo brand. Name, logo, palette, locale set and hosted free-tier size are runtime configuration.

The project now uses an **open-core split**:

- **Community Self-Hosted** — open-source core, self-operated, no camera-count cap.
- **Commercial Cloud / Enterprise** — the same core plus a private entitlement/control-plane service for managed hosting, paid plans and enterprise features.
- **Plugins are common to both** — custom AI and storage are never commercial-only extension points.

## Repository layout

```text
crates/domain/             shared VMS wire/domain types
crates/plugin-sdk/         versioned plugin protocol (open source)
crates/plugin-runtime/     plugin registry + HTTP dispatcher (open source)
services/api/              open-source control API
edge/gateway/              open-source Rust camera gateway
plugins.d/                 plugin registrations
plugins/examples/          AI adapter + S3-compatible storage examples
web/                       language-agnostic white-label landing/dashboard
commercial/control-plane/  proprietary entitlement overlay prototype
```

See `docs/EDITIONS.md`, `docs/PLUGIN-SDK.md` and `docs/ARCHIVE.md`.

## Community Self-Hosted

Run without the commercial overlay:

```bash
make community
```

Open `http://localhost:8081/`.

With no `ENTITLEMENTS_URL`, the API reports `community-self-hosted` and `camera_limit: null`. Gateway enrollment therefore becomes unlimited by camera count.

To run a real edge gateway on the camera LAN:

```bash
export CAMERA_USERNAME=admin
export CAMERA_PASSWORD='camera-password'
export ENROLLMENT_TOKEN='TOKEN_FROM_DASHBOARD'
make edge
```

## Commercial prototype

Run core plus the proprietary entitlement service:

```bash
make commercial
```

Unknown/new customer IDs receive the hosted free entitlement (3 cameras by default). To mark a demo customer as paid:

```bash
PAID_CUSTOMERS=my-customer-id make commercial
```

Production billing/licensing is intentionally outside core. The current commercial service is only the architectural boundary and entitlement prototype.

## Plugin system

Start the included example AI and storage plugins plus MinIO:

```bash
make plugins
```

For the complete local media demo (API + web + plugins + MinIO + edge gateway):

```bash
CAMERA_USERNAME=admin CAMERA_PASSWORD=secret make demo
```

The edge profile uses host networking so ONVIF multicast discovery can see cameras on the LAN.

The dashboard **Plugins** section reads `GET /api/v1/plugins`, displays capabilities and can call each plugin health endpoint.

### Connect your AI

The included `ai-http-adapter` lets your model live anywhere:

```bash
UPSTREAM_AI_URL=https://ai.example.com/analyze \
UPSTREAM_AI_TOKEN=secret \
make plugins
```

Or implement Plugin Protocol v1 directly. AI plugins can be Python/CUDA, Rust, Go, Node, etc.

### Connect storage

`storage-s3` works with S3-compatible systems and returns presigned PUT/GET URLs. Configure AWS S3, MinIO, Backblaze B2 S3 or another compatible target without modifying core.

This design avoids routing continuous video bytes through the VMS API.

## What is real now

- Rust Axum core API
- one-time gateway enrollment + per-gateway auth
- Community vs Commercial entitlement boundary
- Community unlimited camera entitlement
- Commercial hosted-free / paid entitlement prototype
- ONVIF WS-Discovery and media profile/URI resolution
- WS-Security PasswordDigest
- RTSP health sampling with Retina
- FPS / bitrate / packet-loss / reconnect telemetry
- stale/offline detection
- plugin SDK/runtime shared by both editions
- AI HTTP adapter example
- S3-compatible storage plugin example
- plugin UI with health test
- runtime white-label landing/dashboard
- EN / ES / RU dictionaries
- outbound gateway media-command queue
- zero-transcode H.264 fMP4/CMAF-style recording via Retina + `shiguredo_mp4`
- direct signed upload of `init.mp4` / `.m4s` through the storage plugin
- recording index, timeline, signed playback manifest and browser MediaSource playback
- configurable automatic archive retention/deletion

## Still not implemented

- WebRTC live viewing
- continuous recording policies (current archive is on-demand)
- archive audio muxing
- Postgres persistence for commands/recording manifests
- production user auth/RBAC
- billing provider integration
- production commercial license validation
- SSO/HA/reseller hierarchy
- real AI frame scheduling/snapshot pipeline

## Rebrand without rebuilding

Edit `web/brand.json`. UI copy remains in `web/locales/*.json`; brand name, logo, colors, locales, gateway image/API URL and hosted free-tier count are runtime settings.

## Licensing

Community Core currently carries GPL-3.0 as the working open-source license (`LICENSE`). `commercial/` is separated under a proprietary notice. Review the final licensing model before accepting external contributions or public launch.

## v6 media/action checkpoint

The v6 prototype adds the two remaining interactive pilot paths on top of the v5 archive layer:

- **Live:** browser SDP -> Core API -> outbound edge command -> RTSP H.264 -> WebRTC -> browser.
- **AI:** ONVIF snapshot -> edge -> storage plugin + custom AI plugin -> normalized detections -> bbox overlay.
- **Storage audience:** signed transfers distinguish browser/edge/service endpoints, so MinIO/S3/B2 can be reachable correctly from each network context.

See `docs/LIVE.md`, `docs/AI.md`, and `docs/RELEASE-v6.md`.
