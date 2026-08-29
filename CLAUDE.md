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
- **74 Rust tests, 37 web tests and 11 Python tests, all passing.** Nine speak real
  RTSP, three negotiate a real peer connection, thirteen drive the HTTP surface and
  fourteen cover the plugin runtime.
  `src/fake_camera.rs` is a test-only RTSP server that serves
  `fixtures/camera.h264` — three seconds of genuine H.264 made once with ffmpeg and
  committed, so the build needs no encoder — over RTP interleaved on the TCP control
  connection, with FU-A fragmentation and optional Basic auth. `rtsp::probe` and
  `archive::record_h264_cmaf` are covered end to end against it: session negotiation,
  depacketisation, avcC from in-band parameter sets, fMP4 segmenting, and the
  credential paths including a camera that refuses anonymous access.
  `src/fake_browser.rs` is the other half: a webrtc-rs peer that offers recvonly
  H.264 with ICE already gathered — the gateway does not trickle, so neither does it —
  accepts the answer and counts the RTP that arrives. `live::start_h264` is covered end
  to end through it: camera to RTSP to gateway to peer connection, asserting real
  payload rather than merely that packets appeared.
  The API is covered by driving `build_router` with `tower`'s `oneshot` — no socket, no
  environment. The tests concentrate on the authorisation boundary and the command
  protocol, because those are where a mistake is a security or correctness failure
  rather than a cosmetic one: telemetry and heartbeats refused without a Bearer token,
  an enrolled gateway token bound to the gateway it was issued for, an enrolment code
  that cannot be replayed, and a command queue that hands each command to one gateway
  once.
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
3. **Nothing renders the UI, and no plugin is run for real.** The Python example
   plugins are tested only where they are pure; their HTTP handlers, and the boto3 and
   upstream calls behind them, are not exercised.
4. **`dashboard.js` itself is untestable as written.** It exports nothing and runs
   `await loadRuntime()` at module scope, so importing it from a test fetches
   `brand.json` and fails; and 16 of its 32 functions close over the module-level
   `brand`, `dict` and `locale`. Reaching them means splitting the bootstrap from the
   render functions and threading that state through — a real refactor of 639 lines of
   working UI with no runtime test to catch a regression. **Worth doing, but as its own
   change, not as a side effect of adding tests.** Until then the page tests below cover
   the part that actually breaks in practice.

Three fakes carry the integration tests, all binding loopback on an ephemeral port and
needing no network: `FakeCamera::start(bool)` for the camera end (the flag makes it
demand Basic auth), `FakeBrowser::offer()` for the viewer end, and
`FakeControlPlane::start(commands, reject_first_polls)` for the API the gateway polls.
The control plane also answers the presigned-upload call and the blob PUT that follows,
so a record command runs the whole way through and the test can check the manifest
rather than stopping at the first upload. The suite was
run five times over to confirm the sockets and ICE do not flake.

**Capability enforcement is the plugin system's security boundary** and is checked
before any network call, so a plugin declaring only `ai_analyze` cannot be reached
through the storage calls whatever id the caller supplies — storage handles recorded
video and issues signed URLs. A test asserts the refusal is fast, because a refusal that
waited on a round trip would mean the body had already been sent.

**A bad manifest costs that plugin and no more.** An unreadable or unparseable file is
warned about by name and skipped, matching how the unreachable-plugin and
wrong-protocol cases already behave. An unreadable plugin *directory* is still an error,
because that is a configuration fault rather than a plugin fault.

`object_key` in the S3 example is the only place a client-supplied string becomes a path
inside the bucket, so its traversal guard is tested — with `boto3` stubbed into
`sys.modules` rather than installed, since the module builds clients at import time and
requiring the SDK in CI to test twenty lines of string handling is a poor trade.

Web tests run with `npm test --prefix web`. **jsdom is the repository's only JavaScript
dependency and it is test-only** — the shipped page loads no bundler and no framework,
and that is worth keeping.

`tests/page.test.mjs` checks the contract between the scripts and the markup: every one
of the 38 selectors `dashboard.js` hands to `querySelector` must match something in
`app.html`. The script checks none of them, so a renamed id makes the first
`appendChild` throw during module evaluation, which stops the whole file — the page
loads and stays blank, with one line in a console nobody has open. Nothing else in this
repository can see that, and it is one rename away.
The i18n tests replaced `scripts/check-i18n.py`, which scanned only the HTML and therefore left all
79 `t()` calls in `dashboard.js` unchecked. **A missing translation key never throws** —
`t()` falls back to the key itself, so the user is shown `app.live.connecting` where a
sentence should be and nothing reports a fault. Those tests are the only thing that
notices, which is why they also check interpolation parity: if English says `{count}
cameras` and another locale drops the placeholder, the number silently vanishes for
those users.

**A command that fails must still be completed.** Whoever asked for it is polling the
command view, so a failure that never reports leaves them unable to tell a slow gateway
from a broken one. Same for a rejected poll: a loop that gives up on one 401 stays dead
until someone restarts it, and on customer premises that is a site visit. Both are
pinned by tests.

⚠️ **`GATEWAY_TOKEN` authorises any `gateway_id`.** That is the bootstrap path before a
gateway enrols, and a test pins it so nobody assumes otherwise — but it means anyone
holding that value can post telemetry as any gateway on the deployment. Treat it as a
provisioning secret with a short life, not a service credential.

**Every path that touches a camera URL goes through `rtsp::strip_userinfo`.** Live,
archive and the HTTP snapshot each used to carry their own copy, and `redacted_endpoint`
had a fourth that discarded the errors from `set_username`/`set_password` — which meant a
URL like `admin:pw@junk`, which parses but cannot hold a host, was published to telemetry
with the password intact. Keep it one implementation.

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
