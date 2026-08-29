# Prototype v6 checkpoint

v6 closes the first complete media/action loop on top of the v5 archive layer.

## Added

- Outbound-signaled WebRTC H.264 live from real RTSP sources.
- `GET /api/v1/rtc/config` and `POST /api/v1/cameras/:id/live`.
- ONVIF snapshot fetch with Basic/Digest HTTP authentication.
- `POST /api/v1/cameras/:id/analyze` -> custom AI plugin -> normalized detections.
- Snapshot persistence through the selected storage plugin.
- AI bbox overlay in the dashboard.
- Unified gateway command payloads for record/live/analyze.
- Audience-aware storage signed transfers: `browser`, `edge`, `service`.
- S3 plugin endpoint selection per audience.
- Explicit simulated AI mode for local demo only.

## Existing from v5

- ONVIF discovery and RTSP health.
- fMP4/CMAF recording (`init.mp4` + `.m4s`).
- Timeline/playback/retention.
- Community Self-Hosted vs Commercial entitlement split.
- Common Plugin Protocol for both editions.

## Build gate

The current execution environment does not include a Rust toolchain and cannot resolve the Rust distribution host from shell networking. JS/Python/config/i18n checks are run locally; `cargo fmt`, `cargo clippy -D warnings` and `cargo test` remain mandatory CI gates.
