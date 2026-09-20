# Gateway: TURN over TLS through a local bridge

**Goal:** A camera site whose firewall allows nothing outbound but TLS on 443
gets no relay today, even though the relay now serves TURN over TLS on 443
(`docs/superpowers/specs/2026-09-11-relay-tls-443-design.md`). The gateway's
WebRTC stack cannot use it. This makes the gateway reach the relay over TLS (or
TCP) when, and only when, it cannot reach it over UDP — without patching or
forking webrtc-rs.

This is part two of epic `relaysight-vms-8p3` (beads `relaysight-vms-shk`). The
proper fix inside webrtc-rs is a separate track (`relaysight-vms-qxr`).

**Decisions taken with the user (2026-09-13):**

- **A local bridge in the gateway, no fork.** Earlier the same day the choice was
  a pinned fork of webrtc-rs; it was reopened once reading the driver showed a
  bridge needs no change to the library at all. The bridge is removed once a
  webrtc-rs release supports `turns:` itself.
- **Upstream is told, and kept separate.** A comment proposing TCP and TLS in the
  async relayer, and a follow-up saying there will be no fork, are on
  https://github.com/webrtc-rs/webrtc/issues/848 (comments 5654322903 and
  5654363172). Implementation there waits for the maintainer's answer.
- **Probe UDP first; bridge only if it fails.** UDP reachability is a property of
  the site, so the verdict is cached per gateway.

## What exists today

- The API hands the gateway ICE servers with every live command
  (`GatewayCommandKind::Live { ice_servers, .. }` in `edge/gateway/src/main.rs`),
  as `vms_domain::RtcIceServerConfig { urls: Vec<String>, username, credential }`.
  `services/api/src/turn.rs` emits one entry for STUN and one for TURN carrying
  every configured TURN URL with a minted credential. `docs/TURN-DEPLOY.md`
  recommends `turn:…:3478?transport=udp`, `turn:…:3478?transport=tcp` and
  `turns:…:443?transport=tcp`.
- `live::start_h264` (`edge/gateway/src/live.rs`) converts them into webrtc-rs
  `RTCIceServer`s, builds the peer with `.with_udp_addrs(vec!["0.0.0.0:0"])`,
  waits **12 s** for ICE gathering (failing the whole session on timeout), then
  spawns a task that forwards RTSP and closes the peer when the session ends or
  fails to connect within 20 s.
- `icepath::observed` (`edge/gateway/src/icepath.rs`) logs the path by the
  selected local candidate's type; relayed sessions feed the cost model.
- Tests: `fake_camera` and `fake_browser` drive a full live session on loopback
  (`live.rs` tests). A test needing outside resources is `#[ignore = "…"]` and
  reads an environment variable (`rtsp.rs`, `RELAYSIGHT_TEST_RTSP_URL`).
- Gateway settings are `GATEWAY_*` environment variables read in `main.rs`.

## What was established by reading webrtc-rs 0.20.3 and rtc 0.20.3

- `webrtc` `src/peer_connection/transports/turn_relayer.rs:255-263` skips every
  `turns:` URL and every `turn:` URL that is not UDP; the driver sends relayer
  traffic only through its UDP sockets. v0.20.5 and `master` (0.21.0-rc.2) still
  do.
- `rtc::shared::tcp_framing` is RFC 4571 framing for ICE-TCP, not TURN framing.
  TURN over TCP/TLS has no length prefix: a STUN message is 20 bytes plus its
  length field; a ChannelData message is 4 bytes plus its length, padded to a
  multiple of 4.
- `rtc-turn` always pads ChannelData to 4 bytes when encoding, and decoding reads
  the declared length and ignores trailing bytes, so a padded message delivered
  as a UDP datagram is accepted.
- The relayer only uses a TURN server whose address family matches a local
  socket; the gateway binds IPv4 `0.0.0.0:0`, so `127.0.0.1` qualifies.
- Relay candidate priority is computed inside webrtc-rs (no override is set), so
  a bridged relay and a UDP relay tie — the gateway cannot prefer one. Candidate
  stats (`RTCIceCandidateStats`) carry `url`, so a bridged relay is identifiable
  by its `turn:127.0.0.1:<port>` URL.
- Gathering completes only when TURN gathering completes. A TURN transaction
  retransmits from 200 ms, doubling to a 1.6 s cap, 7 requests in all: by those
  constants a relay that never answers holds gathering for roughly 8 s (not
  measured).
- The gateway's dependency tree already contains `rustls` 0.23 (feature `ring`),
  `tokio-rustls` 0.26.4 and `webpki-roots` 1.0.9, via `reqwest`.

## Design

### Planning the ICE servers

New module `edge/gateway/src/turn_bridge.rs`. `live::start_h264` passes its ICE
servers through `plan(servers) -> (Vec<RtcIceServerConfig>, BridgeGuard)` before
converting them for webrtc-rs. URLs are parsed with `rtc::ice::url::Url`.

For each ICE server entry that contains at least one `turns:` URL or `turn:` URL
with `transport=tcp` (a *stream URL*):

1. Take the entry's first `turn:` URL with UDP transport. If there is one, probe
   it: one STUN Binding request from a fresh IPv4 UDP socket to that host and
   port. Any STUN response carrying the request's transaction ID within **1 s**
   means reachable. The verdict is cached per `host:port` for **10 minutes**.
2. **Reachable:** remove the entry's stream URLs (webrtc-rs would skip them
   anyway) and bridge nothing.
3. **Not reachable, or no UDP URL in the entry:** remove the entry's UDP `turn:`
   URLs, so gathering does not wait out their retransmissions, and replace each
   stream URL with a bridge (below).

Entries without stream URLs, STUN URLs, and credentials pass through unchanged.
An entry left with no URLs is dropped.

### The bridge

Per stream URL: a UDP socket bound to `127.0.0.1:0`, and the URL replaced by
`turn:127.0.0.1:<port>?transport=udp` with the entry's username and credential.
A task per bridge:

*(Superseded 2026-09-20 by `relaysight-vms-ocy`: a bridge carries the first
source that speaks and ignores any other, since webrtc-rs gives it one socket.
Everything else below still holds.)*

- **Outbound.** For each datagram from a source address, write it unchanged to
  that source's connection to the relay (`turns:` → TLS, `turn:…?transport=tcp`
  → TCP; default ports 5349 and 3478). STUN and ChannelData from `rtc-turn` are
  already whole, 4-byte-aligned messages. The connection is opened on the first
  datagram from a source; up to **16** datagrams queue while it connects.
- **Inbound.** Split the byte stream into messages by the first two bits of each
  header: `00` → STUN (20 + length), `01` → ChannelData (4 + length, padded to 4).
  Anything else is a protocol error that closes the connection. Each message goes
  back to the source address as one datagram.
- **Failing fast.** If connecting, the TLS handshake, or an established
  connection fails, the bridge logs it once and, for every STUN request it has
  queued or receives afterwards for that source, replies with a STUN error
  response (same method and transaction ID, error code 500) so `rtc-turn` reports
  an allocation error at once instead of retransmitting for about 8 s.
  ChannelData and indications are dropped.

### TLS and trust

`rustls` client configuration with the `ring` provider. Roots: `webpki-roots`,
plus the PEM certificates in `GATEWAY_TURN_CA_FILE` when set (self-hosted relays
with a private certificate authority, and the end-to-end test). If that file
cannot be read or parsed, the error is logged at the start of the session and
only the public roots are used. The server name is the URL's host; certificate
verification cannot be turned off. New direct dependencies of `vms-gateway`:
`rustls`, `tokio-rustls` and `webpki-roots`, at the versions already in
`Cargo.lock` — no new crates.

### Lifetime

`BridgeGuard` owns the bridge tasks and sockets and stops them when dropped.
`start_h264` moves it into the spawned session task, where it is dropped after
`peer.close()`; any early error in `start_h264` drops it immediately.

### Reporting

The guard maps each bridge's local port back to its original URL. The session's
"WebRTC peer connected" log line gains `relay_transport` = `udp`, `tcp` or `tls`
when the path is a relay, read from the selected local candidate's stats `url`.
`PathKind` and the cost-model meaning of `relayed` are unchanged.

## Testing

TDD as always.

Unit tests (`cargo test -p vms-gateway`, loopback only):

- Stream splitting: a STUN message; ChannelData with and without padding; two
  messages in one read; one message across several reads; invalid first bits;
  a length beyond what the header allows.
- Planning: probe unreachable → stream URLs become `127.0.0.1` URLs and the UDP
  URL is removed; probe reachable → stream URLs removed, nothing bridged; STUN
  URLs and credentials untouched; an entry with no UDP URL is bridged without
  probing.
- Probe: a fake STUN responder on loopback → reachable; a silent socket →
  unreachable within about 1 s; a second plan inside the cache window sends no
  second probe.
- Bridge over plain TCP: an in-process fake TURN server exchanges messages both
  ways, including coalesced and split reads; a refused connection makes a queued
  Allocate request come back as a STUN error response; dropping the guard closes
  the connection.

End-to-end (`make check-gateway-relay`): `scripts/check-gateway-relay.sh` creates
a throwaway certificate authority and a certificate for `localhost`, starts
`coturn/coturn:4.6` in Docker on high ports (plain 13478, TLS 15349, relay range
49300–49340) with long-term credentials,
`--allow-loopback-peers` (coturn refuses loopback peers by default, and
`--allowed-peer-ip` does not lift that), and a TLS listener, then runs an
`#[ignore]`d gateway test with `GATEWAY_TURN_CA_FILE` and
`RELAYSIGHT_TEST_TURN_*` variables set. The test runs a fake camera, a fake
browser, and the gateway forced to relay-only through a `pub(crate)` variant of
`start_h264` that takes an optional ICE transport policy (the public function
passes none, so production behaviour is unchanged) and returns the session's
bridge byte counters. The gateway is given `turns:localhost:15349` and a UDP URL
on a closed port, so the probe fails and the bridge carries the session. It
asserts that exactly one bridge was started, that media arrives, and that more
bytes crossed the TLS bridge than the video payload received — with the gateway
limited to relay candidates, that is only possible through the bridge. The
`relay_transport` value is covered by a unit test of the mapping from a
candidate's TURN URL. The script refuses to start if its ports are taken and
always removes the container.

## Docs

- `docs/TURN-DEPLOY.md`: "What this does not fix yet: the gateway" becomes how
  the gateway reaches the relay — probe, bridge, `GATEWAY_TURN_CA_FILE` — with a
  dated verified list (`make check-gateway-relay`) and a not-verified list (a
  real site behind TLS-only egress).
- `docs/RUNNING-LOCALLY.md`: points to `make check-gateway-relay`.
- The private working notes that `publish.sh` never syncs (named on its line 8),
  "Known not to work": updated.
- `docs/BACKLOG.md`: the gateway TURN TCP/TLS line is checked off.
- A comment at the top of `turn_bridge.rs` says to delete the module once a
  webrtc-rs release supports `turns:` natively (tracked by `relaysight-vms-qxr`).

## Out of scope

Changes to webrtc-rs (the separate upstream track). Changes to the API,
`turn.rs`, the relay, or the browser. IPv6 bridges (the gateway binds IPv4 only).
Preferring one relay over another beyond the UDP probe. DTLS to the relay.
