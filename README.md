# RelaySight

Self-hosted video management system for camera fleets you
maintain for someone else.

White-label. `RelaySight` is only the default brand. Name,
logo, palette and locales are runtime configuration, not a
build.

## What it does

Keeps the cameras already installed. Runs a small gateway
inside the customer network, outbound only, so no inbound
ports and no VPN.

One dashboard shows which cameras are offline and which
streams are unstable. Live view and archive playback are
there too. AI analysis runs through a plugin you supply.

## Run it

```bash
make community
```

Then open `http://localhost:8081/`.

No entitlement service configured means Community. The API
reports `camera_limit: null`, and enrollment is unlimited by
camera count.

A real gateway on the camera LAN:

```bash
export CAMERA_USERNAME=admin
export CAMERA_PASSWORD='camera-password'
export ENROLLMENT_TOKEN='TOKEN_FROM_DASHBOARD'
make edge
```

Full local demo, API and web and plugins and MinIO and
gateway:

```bash
CAMERA_USERNAME=admin CAMERA_PASSWORD=secret make demo
```

The edge profile uses host networking, so ONVIF multicast
discovery can see the LAN.

## Layout

```text
crates/domain/          shared wire and domain types
crates/plugin-sdk/      versioned plugin protocol
crates/plugin-runtime/  plugin registry and HTTP dispatch
services/api/           control API
edge/gateway/           Rust camera gateway
plugins.d/              plugin registrations
web/                    landing and dashboard
```

## Plugins

Custom AI and custom storage are never paid-only. A
deployment that cannot bring its own model or its own bucket
is not self-hosted.

Reference implementations live in
[relaysight-plugins](https://github.com/mnaza/relaysight-plugins).

```bash
make plugins
```

Your model anywhere:

```bash
UPSTREAM_AI_URL=https://ai.example.com/analyze \
UPSTREAM_AI_TOKEN=secret \
make plugins
```

Or implement Plugin Protocol v1 directly. The core talks
HTTP and knows nothing else, so a plugin can be Python,
Rust, Go, whatever holds a socket.

Storage signs presigned PUT and GET. Video bytes never route
through the API.

## What works

- Rust Axum core API
- one-time gateway enrollment, per-gateway auth
- ONVIF WS-Discovery, media profile and URI resolution
- WS-Security PasswordDigest
- RTSP health sampling with Retina
- FPS, bitrate, packet loss, reconnect telemetry
- stale and offline detection
- plugin SDK and runtime
- WebRTC live view, with ICE path reporting
- zero-transcode H.264 fMP4 recording
- signed upload of `init.mp4` and `.m4s` via the plugin
- recording index, timeline, MediaSource playback
- automatic archive retention
- EN, ES, RU dictionaries
- 95 Rust tests, 55 web tests

## What does not

- continuous recording policies; archive is on-demand
- archive audio muxing
- Postgres for commands and manifests
- production auth and RBAC
- SSO, HA, reseller hierarchy
- AI frame scheduling beyond snapshot-on-demand

Prototype. It runs, it is tested, it has not been through a
season in production.

## Rebrand

Edit `web/brand.json`. Copy stays in `web/locales/*.json`.
Brand name, logo, colors, locales, gateway image and API URL
are runtime settings.

## Docs

`docs/ARCHITECTURE.md`, `docs/PLUGIN-SDK.md`,
`docs/EDITIONS.md`, `docs/ARCHIVE.md`, `docs/LIVE.md`,
`docs/AI.md`.

`docs/TURN-COSTS.md` is the one to read if you are pricing
relay bandwidth. The finding is that a flat-rate box makes
it free and a per-GB cloud makes it ruinous.

## License

GPL-3.0. See `LICENSE` and `NOTICE.md`.

The commercial control plane is a separate private
repository. The boundary is a service boundary, so there is
no directory here you are asked to ignore.
