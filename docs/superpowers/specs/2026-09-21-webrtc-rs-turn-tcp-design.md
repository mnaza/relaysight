# TURN over TCP in webrtc-rs (upstream)

**Goal:** webrtc-rs's async TURN relayer skips every `turn:` URL that is not
UDP and every `turns:` URL, so a peer behind a firewall that allows TCP but not
UDP gets no relay. This adds TURN over TCP — the connection to the TURN server
is TCP, the relayed allocation stays UDP — upstream, where the maintainer asked
for it. TLS follows in a separate PR. Our gateway's bridge
(`edge/gateway/src/turn_bridge/`) goes away once both land in a release.

Beads `relaysight-vms-qxr`. Upstream issue webrtc-rs/webrtc#848.

**The maintainer's answer (2026-09-13):** TCP first, TLS as a follow-up; the
REQUESTED-TRANSPORT split, framing and write routing are the whole of it;
through `RTCTcpTransport`; for TLS later, the rustls `CryptoProvider` comes
through the existing `crypto-ring` / `crypto-aws-lc-rs` features and the
relayer's `crypto_provider`, and `webpki-roots` is not wanted; master only.
Labelled `post-1.0`, unassigned.

**Decided with the user (2026-09-21):** build both halves and prove them
together, open the `rtc` PR first and the `webrtc` PR as a draft that points at
it; include retransmit suppression over TCP as its own commit; no public reply
on #848 until there is a PR to point at.

## Two repositories

`rtc` is its own repository (webrtc-rs/rtc), a submodule of webrtc-rs/webrtc.
The client-side half goes there; the I/O half goes in webrtc with a submodule
bump. Base: rtc `784e464` (= rtc master), webrtc `573cfbe` (master).

## PR 1 — webrtc-rs/rtc

### The REQUESTED-TRANSPORT split

`rtc-turn`'s `allocate()` and its authenticated retry both compute
REQUESTED-TRANSPORT from `transport_protocol`, the transport to the server. A
client talking TCP therefore asks for an RFC 6062 TCP relay, which a server
refuses (442) or grants in a shape the relayer cannot use.

`ClientConfig` gains `requested_transport: TransportProtocol`, defaulting to
UDP, used at both sites. Existing callers set nothing and behave as before.

### A stream splitter

TURN over a stream carries no length prefix (RFC 8656 §12.5): a STUN message
is 20 bytes plus its length field, ChannelData is 4 bytes plus its length
rounded up to a multiple of 4. `rtc-shared`'s `tcp_framing` is RFC 4571, which
is ICE-TCP's framing, not TURN's.

A sans-IO `TurnStreamDecoder` beside `tcp_framing`: push bytes, pop whole
messages. First two bits `00` → STUN, `01` → ChannelData, anything else is an
error that ends the stream. The same shape as `TcpFrameDecoder`, so the host
code can hold either.

### No retransmits over TCP (its own commit)

`Transaction::handle_timeout` retransmits on every transport: 200 ms doubling
to 1.6 s, seven sends. RFC 8489 §6.2.2 says a reliable transport sends once and
waits Ti = 39.5 s. Over TCP the client sends once and times out at 39.5 s.

## PR 2 — webrtc-rs/webrtc

### Framing per stream in `RTCTcpTransport`

Streams gain a framing — `Rfc4571` for ICE-TCP, `TurnStream` for relay
connections — chosen at registration. Reads decode with the matching decoder;
writes prepend a length only for `Rfc4571`. TURN streams are found by exact
four-tuple only: the peer-address fallback that `find_stream` does for ICE-TCP
could otherwise send TURN bytes down an ICE-TCP stream or the reverse.

A stream that ends or fails is reported, not only dropped, so the relayer can
let go of its client. A write to a missing stream is an error, not `Ok(0)`, so
the relayer's existing `SocketWriteFailure` path runs.

### The relayer

- Accept `turn:…?transport=tcp`; keep skipping `turns:` (the TLS PR).
- One TCP connection per server and address family, not one per UDP local
  address: the connection's source address is the kernel's choice, unrelated
  to the UDP sockets.
- Connect without blocking the driver: a spawned `runtime.connect_tcp` bounded
  by a timeout, reported back as a driver event. On success the stream is
  registered with `TurnStream` framing and the client is created with
  `local_addr` = the stream's local address, `transport_protocol` TCP and
  `requested_transport` UDP; on failure or timeout the server is dropped.
- Gathering does not complete while a connect is pending.
- A connect that finishes after an ICE restart or a configuration change is
  discarded, by a generation counter.
- Client keys and address matching take the transport protocol into account,
  so a TCP source port that equals a UDP socket's port cannot confuse them.

No change to routing: relayer output is already tagged with the client's
transport, and `handle_write` sends TCP-tagged messages to `RTCTcpTransport`
before anything else; inbound TCP messages already go through
`is_turn_message` first.

## Testing

Upstream conventions: unit tests beside the code, integration tests in
`tests/` on the tokio runtime (the mock runtime has no TCP).

- rtc: the split — over TCP the Allocate carries REQUESTED-TRANSPORT UDP, and
  the default is unchanged; the splitter — one message, two in one push, one
  across many pushes, ChannelData with and without padding, invalid first bits;
  retransmits — none over TCP, the 39.5 s timeout, UDP unchanged.
- webrtc: a TCP twin of `run_mock_turn_server` (tests/ice_test.rs) and a
  gathering test with `turn:{addr}?transport=tcp` producing a relay candidate;
  a refused connection and a connect timeout completing gathering without one;
  a TURN stream never written to by ICE-TCP traffic and vice versa.
- Both halves together against coturn in Docker, the way our
  `make check-gateway-relay` already runs it — outside the PRs, as evidence.

## Out of scope

TLS (`turns:`) — the next PR, which needs a rustls `CryptoProvider` that
`RTCCryptoProvider` does not expose today. RFC 6062 TCP relays. Choosing the
source interface of the TCP connection (`connect_tcp` takes no local address,
and changing it is public API). The smol runtime's blocking connect. ICE
local-preference for TCP relays, and `relay_protocol` in candidate stats.
