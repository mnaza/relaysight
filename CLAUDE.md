# CLAUDE.md — RelaySight VMS

Cloud video-management system: ONVIF/RTSP cameras behind a customer's firewall reach the
cloud through a Rust edge gateway that dials **outbound only**, so no inbound port is
opened. Live video goes gateway → browser over WebRTC as **passthrough H.264, never
transcoded**; recordings are written at the edge as fragmented MP4 and pushed to object
storage. The cloud is a control plane, not a media pipe. That single decision is what
makes the economics work, so treat it as an invariant, not a preference.

Read `docs/ARCHITECTURE.md` first, then `docs/LIVE.md` and `docs/ARCHIVE.md`.

## Read this before trusting anything in docs/

**Every line of this repository was written by a model that never compiled it.** The
prototype went v1 → v6 inside a ChatGPT sandbox with no Rust toolchain, no Docker and no
camera. `docs/BUILD_STATUS.md` was honest about that, and it was right to be: on the
first real `cargo check` (2026-08-29) the WebRTC live path — the load-bearing part — had
four compile errors.

So: **the prose in `docs/` describes intent, not verified behaviour.** Where a document
claims something works, that claim is unverified unless this file says otherwise. Do not
propagate a claim from `docs/` into a README, a commit message or a customer-facing page
without running it first.

## Verify

The CI gate (`.github/workflows/ci.yml`) is the contract. Run all of it before claiming
anything:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --manifest-path commercial/control-plane/Cargo.toml   # excluded from the workspace
node --check web/theme.js && node --check web/landing.js && node --check web/dashboard.js
```

## State as of 2026-08-29

- **Compiles.** Whole workspace plus `commercial/control-plane`.
- `clippy -- -D warnings` **clean**; `cargo fmt` clean.
- **5 tests, all passing — for ~3,300 lines of Rust.** This is the largest gap in the
  repository. Nothing in the media path, the gateway protocol or the API has a test.
- Never run against a real camera. Never run under Docker Compose.

The three fixes that made it build, all in `edge/gateway/src/live.rs`, are worth knowing
because the same mistake will recur:

1. `MediaStreamTrack::new` returns `Self`; `TrackLocalStaticSample::new` returns
   `Result<Self>`. The generated code had the `?` on the wrong one.
2. **`rtc` and `webrtc` are two different crates and both are dependencies.**
   `MediaStreamTrack` lives in `rtc::media_stream`; the `Track` trait that provides
   `ssrcs()` lives in `webrtc::media_stream`. Importing from the wrong one looks right
   and fails.

## Real blockers, in the order they will bite

1. **NAT traversal is not implemented.** Direct gateway → browser WebRTC works until the
   customer's network is behind symmetric NAT or a strict egress firewall — and premises
   with security cameras are exactly those networks. Without TURN those sessions simply
   fail; with TURN the media relays through your server and the bandwidth cost the
   architecture was designed to avoid comes straight back. **Decide and measure this
   before anything else**, because it changes the hosting model.
2. **Camera interoperability is untested.** The passthrough path depends on H.264
   parameter sets (SPS/PPS) arriving on keyframes. Vendors that send them once at session
   start, or out of band, will break it. Hikvision and Dahua at minimum need real
   hardware testing. This tail is the actual moat in this market and it is ground out one
   vendor at a time.
3. **No tests.** See above.

Fields marked `#[allow(dead_code)]` in `onvif.rs` and `rtsp.rs` are parsed off the wire
and kept deliberately — each comment names the feature that would consume them. They are
a to-do list, not clutter.

## Boundaries

- `~/tmp/relaysight-site/` is an **earlier packaging of the same thing**: docs plus the
  web front-end, byte-identical `web/` directory, no Rust. This repository supersedes it.
  Do not edit both.
- The idea began as "a simpler competitor to 3dEYE", and Andrey interviewed with 3dEYE in
  August 2026. **Nothing non-public learned in that process may be used here.** Public
  product pages and pricing are fine; anything said in an interview is not.
- Unrelated to `~/docs/Andrey_CV` (job search) and `~/docs/signalscreen` (a separate,
  live product). Do not cross-reference them in this repo's history.
