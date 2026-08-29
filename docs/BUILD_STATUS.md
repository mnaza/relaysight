# Build status — prototype v6

> **Superseded in part on 2026-08-29.** The repository was compiled for the first time on
> real hardware that day. The WebRTC live path did not build — four errors in
> `edge/gateway/src/live.rs` around the `rtc`/`webrtc` crate split and `?` placement.
> After those fixes the whole workspace compiles, `clippy -- -D warnings` is clean,
> `cargo fmt` is clean and the 5 existing tests pass. The "not validated" list below
> still stands for Docker and for physical cameras. See `CLAUDE.md`.

Validated in the artifact environment:

- JavaScript syntax: all `web/*.js`.
- Python syntax: example AI/storage plugins, commercial prototype service and helper scripts.
- i18n parity: EN / ES / RU, 243 keys each.
- all literal UI i18n references resolve.
- JSON / TOML / YAML parsing, including Compose and CI files.
- storage request constructors use the v1 `audience` field where the caller needs a specific network view.
- current Retina 0.4.20 documentation confirms `FrameFormat::SIMPLE` is Annex-B with parameter sets on keyframes and `FrameFormat::MP4` is the ISO-BMFF preset.
- current `webrtc-rs` 0.20.3 documentation/examples confirm the `PeerConnectionBuilder`, Tokio runtime, `TrackLocalStaticSample`, negotiated payload type and encoded-sample writer API used by the live path.

Not validated locally in this environment:

- `cargo check`, `cargo test`, `cargo clippy`, or `cargo fmt`, because the runtime has no Rust toolchain and shell networking cannot obtain one.
- Docker Compose execution, because Docker is not installed in this runtime.
- physical-camera interoperability; Hikvision/Dahua/other firmware variants still need hardware test coverage.

The repository CI remains the mandatory Rust compilation/test gate before merge or deployment. A production pilot additionally requires a TURN service and real camera testing.
