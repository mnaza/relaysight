# Gateway TURN over TLS Bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a camera site blocks UDP to the relay, the gateway carries its TURN traffic to the relay over TLS through a local UDP-to-TLS bridge, so live video still gets a relay.

**Architecture:** A new module `edge/gateway/src/turn_bridge/` plans each live session's ICE servers: it probes the relay's UDP TURN URL once (cached ten minutes) and, if the relay does not answer, replaces each `turns:` / `turn:…?transport=tcp` URL with a `turn:127.0.0.1:<port>` URL served by a bridge. A bridge is a loopback UDP socket whose datagrams are written unchanged to a TLS or TCP connection to the relay, and whose inbound byte stream is split back into STUN and ChannelData datagrams. `live::start_h264` runs the plan, keeps the bridges alive for the session, and logs which transport a relayed session used.

**Tech Stack:** Rust (edition 2024, toolchain 1.98), tokio, webrtc-rs/rtc 0.20.3 (`rtc::stun`, `rtc::ice::url`), rustls 0.23 with `ring`, tokio-rustls 0.26, webpki-roots 1; bash + Docker + `coturn/coturn:4.6` for the end-to-end check.

**Spec:** `docs/superpowers/specs/2026-09-13-gateway-turn-bridge-design.md`

## Global Constraints

- Branch `gateway-turn-bridge`. The repo-root `Cargo.toml` carries a local, uncommitted `[patch.crates-io]` entry that builds need: never stage it, never `git add .` or `git add -A` — stage files by name.
- Commit messages are one plain sentence in the repository's style, with no trailer lines.
- Nothing tracked may match `publish.sh`'s leak check. Before each commit: `bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | grep -vxF "$(sed -n "8s/^# \([^ ]*\).*/\1/p" publish.sh)" | xargs -r grep -lniE "$LEAKS"'` — expected output: nothing. The private notes file that `publish.sh` excludes (its line 8) is the only exemption.
- No change to webrtc-rs, the API, `services/api/src/turn.rs`, the relay config, or the browser.
- No new crates: `grep -c '^\[\[package\]\]' Cargo.lock` must print the same number before and after. `Cargo.lock` may only gain dependency lines under `vms-gateway`.
- New direct dependencies of `edge/gateway/Cargo.toml`, exactly: `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }`, `tokio-rustls = { version = "0.26", default-features = false, features = ["ring", "tls12"] }`, `webpki-roots = "1"`; and `tokio.workspace = true` becomes `tokio = { workspace = true, features = ["io-util"] }`.
- Constants: probe timeout `Duration::from_secs(1)`; probe verdict lifetime `Duration::from_secs(600)`, keyed by `(host, port)`; datagrams queued per source while connecting: `16`; relay connect timeout `Duration::from_secs(5)`; STUN error code for failed requests: `500` (`CODE_SERVER_ERROR`); bridge socket bound to `127.0.0.1:0`; bridged URL format `turn:127.0.0.1:<port>?transport=udp`; CA environment variable `GATEWAY_TURN_CA_FILE`; the probe and bridges are IPv4 only.
- Probe rule, per ICE server entry: only an entry containing a stream URL (`turns:`, or `turn:` with `transport=tcp`) is probed, at its first `turn:` URL with UDP transport. Reachable → the entry's stream URLs are removed. Unreachable, or no UDP URL → its UDP `turn:` URLs are removed and each stream URL is bridged. Other URLs and credentials pass through; an entry left with no URLs is dropped.
- Production behaviour of `live::start_h264` stays as today apart from the plan: no ICE transport policy is set unless a caller passes one.
- Baseline: `cargo test -p vms-gateway` → `83 passed; 0 failed; 1 ignored` on this branch before Task 1.
- Until Task 6 wires the module in, `dead_code` warnings for `turn_bridge` items are expected staging; do not silence them.
- End-to-end check ports: coturn plain `13478`, TLS `15349`, relay range `49300`–`49340`, closed UDP port for the failing probe `13479`.

## File Structure

- `edge/gateway/src/turn_bridge/mod.rs` — create (Task 1), grows in Task 6: module docs, `plan()`, `BridgeGuard`, `RelayTransport`.
- `edge/gateway/src/turn_bridge/framing.rs` — create (Task 1): `StreamSplitter`.
- `edge/gateway/src/turn_bridge/probe.rs` — create (Task 2): `stun_binding_answers`, `ProbeCache`.
- `edge/gateway/src/turn_bridge/plan.rs` — create (Task 3): `probe_target`, `decide`, `UrlPlan`, `EntryPlan`.
- `edge/gateway/src/turn_bridge/bridge.rs` — create (Task 4): `Bridge`, `BridgeStats`, `Connector`, `RelayStream`, `error_response`.
- `edge/gateway/src/turn_bridge/tls.rs` — create (Task 5): `root_store`, `client_config`, `connector`.
- `edge/gateway/Cargo.toml`, `Cargo.lock` — modify (Tasks 4 and 5).
- `edge/gateway/src/main.rs` — modify (Task 1): `mod turn_bridge;`.
- `edge/gateway/src/icepath.rs` — modify (Task 6): `observed_candidate`.
- `edge/gateway/src/live.rs` — modify (Task 6 wiring, Task 7 end-to-end test).
- `scripts/check-gateway-relay.sh`, `Makefile` — create / modify (Task 7).
- `docs/TURN-DEPLOY.md`, `docs/RUNNING-LOCALLY.md`, `docs/BACKLOG.md`, the private notes file — modify (Task 8).

---

### Task 1: Splitting a TURN byte stream into messages

**Files:**
- Create: `edge/gateway/src/turn_bridge/mod.rs`
- Create: `edge/gateway/src/turn_bridge/framing.rs`
- Modify: `edge/gateway/src/main.rs` (module list at the top)

**Interfaces:**
- Produces: `pub(crate) struct StreamSplitter` with `Default`, `fn push(&mut self, bytes: &[u8])`, `fn next_message(&mut self) -> Result<Option<bytes::Bytes>, FramingError>`; `pub(crate) enum FramingError { NotTurn(u8), BadStunLength(usize) }` implementing `Display` and `std::error::Error`.

- [ ] **Step 1: Create the module and write the failing tests.** In `edge/gateway/src/main.rs`, add `mod turn_bridge;` on the line after `mod snapshot;`. Create `edge/gateway/src/turn_bridge/mod.rs`:

```rust
//! Carry the gateway's TURN traffic to the relay over TLS or TCP.
//!
//! webrtc-rs (0.20.x, and master at 0.21.0-rc.2) skips every TURN URL that is
//! not plain UDP, so a camera site whose firewall allows nothing outbound but
//! TLS gets no relay. This module works around it outside the library:
//! webrtc-rs talks plain UDP TURN to a socket on 127.0.0.1, and each message is
//! carried to the relay over TLS (or TCP) and back.
//!
//! Delete this module once a webrtc-rs release supports `turns:` itself
//! (https://github.com/webrtc-rs/webrtc/issues/848).
//! See docs/superpowers/specs/2026-09-13-gateway-turn-bridge-design.md.

mod framing;
```

Create `edge/gateway/src/turn_bridge/framing.rs` containing only the tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A STUN Binding request header followed by `body_len` bytes of body.
    fn stun(body_len: u16) -> Vec<u8> {
        let mut message = vec![0x00, 0x01];
        message.extend_from_slice(&body_len.to_be_bytes());
        message.extend_from_slice(&0x2112_A442u32.to_be_bytes());
        message.extend_from_slice(&[7u8; 12]);
        message.extend(std::iter::repeat_n(0xAB, usize::from(body_len)));
        message
    }

    /// ChannelData on channel 0x4000 carrying `payload`, padded to 4 when `pad`.
    fn channel_data(payload: &[u8], pad: bool) -> Vec<u8> {
        let mut message = vec![0x40, 0x00];
        message.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        message.extend_from_slice(payload);
        while pad && message.len() % 4 != 0 {
            message.push(0);
        }
        message
    }

    #[test]
    fn a_whole_stun_message_comes_out_whole() {
        let message = stun(8);
        let mut splitter = StreamSplitter::default();
        splitter.push(&message);
        assert_eq!(splitter.next_message().unwrap().as_deref(), Some(&message[..]));
        assert_eq!(splitter.next_message().unwrap(), None);
    }

    #[test]
    fn channel_data_comes_out_with_its_padding() {
        let message = channel_data(&[1, 2, 3, 4, 5], true);
        assert_eq!(message.len(), 12, "4 header + 5 payload + 3 padding");
        let mut splitter = StreamSplitter::default();
        splitter.push(&message);
        assert_eq!(splitter.next_message().unwrap().as_deref(), Some(&message[..]));
    }

    #[test]
    fn unpadded_channel_data_waits_for_its_padding() {
        // Over a stream the sender must pad, so the message is incomplete until it does.
        let mut splitter = StreamSplitter::default();
        splitter.push(&channel_data(&[1, 2, 3, 4, 5], false));
        assert_eq!(splitter.next_message().unwrap(), None);
        splitter.push(&[0, 0, 0]);
        assert_eq!(splitter.next_message().unwrap().map(|m| m.len()), Some(12));
    }

    #[test]
    fn two_messages_in_one_read_come_out_as_two() {
        let first = stun(4);
        let second = channel_data(&[9; 6], true);
        let mut splitter = StreamSplitter::default();
        splitter.push(&[first.clone(), second.clone()].concat());
        assert_eq!(splitter.next_message().unwrap().as_deref(), Some(&first[..]));
        assert_eq!(splitter.next_message().unwrap().as_deref(), Some(&second[..]));
        assert_eq!(splitter.next_message().unwrap(), None);
    }

    #[test]
    fn a_message_split_across_reads_comes_out_once_complete() {
        let message = stun(12);
        let mut splitter = StreamSplitter::default();
        splitter.push(&message[..3]);
        assert_eq!(splitter.next_message().unwrap(), None, "not even a header yet");
        splitter.push(&message[3..25]);
        assert_eq!(splitter.next_message().unwrap(), None, "header but not the whole body");
        splitter.push(&message[25..]);
        assert_eq!(splitter.next_message().unwrap().as_deref(), Some(&message[..]));
    }

    #[test]
    fn bytes_that_are_neither_stun_nor_channel_data_are_refused() {
        let mut splitter = StreamSplitter::default();
        splitter.push(&[0x80, 0x00, 0x00, 0x00]);
        assert_eq!(splitter.next_message(), Err(FramingError::NotTurn(0x80)));
    }

    #[test]
    fn a_stun_length_that_is_not_a_multiple_of_four_is_refused() {
        let mut splitter = StreamSplitter::default();
        splitter.push(&stun(5));
        assert_eq!(splitter.next_message(), Err(FramingError::BadStunLength(5)));
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p vms-gateway turn_bridge::framing`
Expected: compile error, ``cannot find type `StreamSplitter` in this scope``.

- [ ] **Step 3: Implement.** Put this above the tests module in `framing.rs`:

```rust
//! Splitting a TURN byte stream back into messages.
//!
//! Over UDP every datagram is one message. Over TCP or TLS there is no length
//! prefix: a STUN message is its 20-byte header plus the length in that header,
//! and a ChannelData message is its 4-byte header plus its length padded to a
//! multiple of 4 (RFC 8656). The first two bits tell them apart: `00` is STUN,
//! `01` is ChannelData.

use bytes::{Bytes, BytesMut};

const STUN_HEADER_LEN: usize = 20;
const CHANNEL_DATA_HEADER_LEN: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FramingError {
    /// The first two bits are neither STUN (`00`) nor ChannelData (`01`).
    NotTurn(u8),
    /// A STUN length that is not a multiple of 4, which STUN never sends.
    BadStunLength(usize),
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotTurn(first) => write!(f, "byte {first:#04x} starts neither STUN nor ChannelData"),
            Self::BadStunLength(len) => write!(f, "STUN length {len} is not a multiple of 4"),
        }
    }
}

impl std::error::Error for FramingError {}

#[derive(Debug, Default)]
pub(crate) struct StreamSplitter {
    buf: BytesMut,
}

impl StreamSplitter {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole message, or `None` until one has arrived.
    pub(crate) fn next_message(&mut self) -> Result<Option<Bytes>, FramingError> {
        if self.buf.len() < CHANNEL_DATA_HEADER_LEN {
            return Ok(None);
        }
        let first = self.buf[0];
        let declared = usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]));
        let total = match first >> 6 {
            0b00 if declared % 4 != 0 => return Err(FramingError::BadStunLength(declared)),
            0b00 => STUN_HEADER_LEN + declared,
            0b01 => CHANNEL_DATA_HEADER_LEN + declared.next_multiple_of(4),
            _ => return Err(FramingError::NotTurn(first)),
        };
        if self.buf.len() < total {
            return Ok(None);
        }
        Ok(Some(self.buf.split_to(total).freeze()))
    }
}
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p vms-gateway turn_bridge::framing`
Expected: `7 passed`. Then `cargo test -p vms-gateway`: `90 passed; 0 failed; 1 ignored`.

- [ ] **Step 5: Leak check and commit**

```bash
git add edge/gateway/src/main.rs edge/gateway/src/turn_bridge/mod.rs edge/gateway/src/turn_bridge/framing.rs
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | xargs -r grep -lniE "$LEAKS"'
git commit -m "A TURN byte stream splits back into STUN and ChannelData messages"
```

---

### Task 2: Probing the relay over UDP

**Files:**
- Create: `edge/gateway/src/turn_bridge/probe.rs`
- Modify: `edge/gateway/src/turn_bridge/mod.rs` (add `mod probe;`)

**Interfaces:**
- Produces: `pub(crate) const PROBE_TIMEOUT: Duration` (1 s); `pub(crate) const VERDICT_TTL: Duration` (600 s); `pub(crate) async fn stun_binding_answers(server: SocketAddr, timeout: Duration) -> bool`; `pub(crate) struct ProbeCache` with `fn new(ttl: Duration) -> Self` and `async fn reachable(&self, host: &str, port: u16, timeout: Duration) -> bool`.

- [ ] **Step 1: Write the failing tests.** Add `mod probe;` after `mod framing;` in `mod.rs`. Create `probe.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rtc::stun::message::BINDING_SUCCESS;

    /// A loopback UDP socket that answers every Binding request with success.
    async fn stun_responder() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                let mut request = Message::new();
                request.raw = buf[..n].to_vec();
                if request.decode().is_err() {
                    continue;
                }
                let mut response = Message::new();
                let setters: [Box<dyn Setter>; 2] =
                    [Box::new(request.transaction_id), Box::new(BINDING_SUCCESS)];
                response.build(&setters).unwrap();
                let _ = socket.send_to(&response.raw, from).await;
            }
        });
        (addr, task)
    }

    #[tokio::test]
    async fn a_relay_that_answers_is_reachable() {
        let (addr, _task) = stun_responder().await;
        assert!(stun_binding_answers(addr, Duration::from_millis(500)).await);
    }

    #[tokio::test]
    async fn a_silent_relay_is_unreachable_once_the_timeout_passes() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = Instant::now();
        assert!(!stun_binding_answers(silent.local_addr().unwrap(), Duration::from_millis(300)).await);
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "the probe must give up at its timeout"
        );
    }

    #[tokio::test]
    async fn a_verdict_is_reused_while_it_is_fresh() {
        let (addr, task) = stun_responder().await;
        let cache = ProbeCache::new(Duration::from_secs(60));
        assert!(cache.reachable("127.0.0.1", addr.port(), Duration::from_millis(500)).await);
        task.abort();
        let _ = task.await;
        assert!(
            cache.reachable("127.0.0.1", addr.port(), Duration::from_millis(300)).await,
            "a fresh verdict must not probe again"
        );
    }

    #[tokio::test]
    async fn an_expired_verdict_probes_again() {
        let (addr, task) = stun_responder().await;
        let cache = ProbeCache::new(Duration::ZERO);
        assert!(cache.reachable("127.0.0.1", addr.port(), Duration::from_millis(500)).await);
        task.abort();
        let _ = task.await;
        assert!(
            !cache.reachable("127.0.0.1", addr.port(), Duration::from_millis(300)).await,
            "an expired verdict must probe, and the responder is gone"
        );
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p vms-gateway turn_bridge::probe`
Expected: compile error, ``cannot find function `stun_binding_answers` in this scope`` (and unresolved `ProbeCache`).

- [ ] **Step 3: Implement.** Put this above the tests module in `probe.rs`:

```rust
//! Whether the relay answers over UDP from here.
//!
//! One STUN Binding request, one answer or none. Whether UDP gets out is a
//! property of the site's firewall, not of a session, so verdicts are cached.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rtc::stun::message::{BINDING_REQUEST, Message, Setter, TransactionId};
use tokio::net::UdpSocket;

/// How long the relay has to answer a probe.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a verdict stands before the relay is probed again.
pub(crate) const VERDICT_TTL: Duration = Duration::from_secs(600);

/// True when `server` answers one STUN Binding request within `timeout`.
pub(crate) async fn stun_binding_answers(server: SocketAddr, timeout: Duration) -> bool {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        return false;
    };
    let mut request = Message::new();
    let setters: [Box<dyn Setter>; 2] = [Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)];
    if request.build(&setters).is_err() || socket.send_to(&request.raw, server).await.is_err() {
        return false;
    }
    let answered = async {
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                return false;
            };
            if from != server {
                continue;
            }
            let mut response = Message::new();
            response.raw = buf[..n].to_vec();
            if response.decode().is_ok() && response.transaction_id == request.transaction_id {
                return true;
            }
        }
    };
    tokio::time::timeout(timeout, answered).await.unwrap_or(false)
}

/// Probe verdicts per relay `host:port`.
pub(crate) struct ProbeCache {
    ttl: Duration,
    verdicts: Mutex<HashMap<(String, u16), (Instant, bool)>>,
}

impl ProbeCache {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            verdicts: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the relay at `host:port` answers over UDP, probing at most once per lifetime.
    pub(crate) async fn reachable(&self, host: &str, port: u16, timeout: Duration) -> bool {
        let key = (host.to_owned(), port);
        let cached = self.verdicts.lock().expect("probe cache poisoned").get(&key).copied();
        if let Some((at, verdict)) = cached
            && at.elapsed() < self.ttl
        {
            return verdict;
        }
        let verdict = match resolve_ipv4(host, port).await {
            Some(server) => stun_binding_answers(server, timeout).await,
            None => false,
        };
        self.verdicts
            .lock()
            .expect("probe cache poisoned")
            .insert(key, (Instant::now(), verdict));
        verdict
    }
}

async fn resolve_ipv4(host: &str, port: u16) -> Option<SocketAddr> {
    tokio::net::lookup_host((host, port))
        .await
        .ok()?
        .find(SocketAddr::is_ipv4)
}
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p vms-gateway turn_bridge::probe`
Expected: `4 passed`. Then `cargo test -p vms-gateway`: `94 passed; 0 failed; 1 ignored`.

- [ ] **Step 5: Leak check and commit**

```bash
git add edge/gateway/src/turn_bridge/mod.rs edge/gateway/src/turn_bridge/probe.rs
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | xargs -r grep -lniE "$LEAKS"'
git commit -m "The gateway can ask the relay over UDP whether it answers, and remembers the answer"
```

---

### Task 3: Deciding which URLs to bridge

**Files:**
- Create: `edge/gateway/src/turn_bridge/plan.rs`
- Modify: `edge/gateway/src/turn_bridge/mod.rs` (add `mod plan;`)

**Interfaces:**
- Produces: `pub(crate) enum UrlPlan { Keep(String), Bridge(String) }`; `pub(crate) struct EntryPlan { pub(crate) username: String, pub(crate) credential: String, pub(crate) urls: Vec<UrlPlan> }` (both `Debug, Clone, PartialEq`); `pub(crate) fn probe_target(entry: &RtcIceServerConfig) -> Option<(String, u16)>`; `pub(crate) fn decide(servers: &[RtcIceServerConfig], udp_reachable: impl Fn(&str, u16) -> bool) -> Vec<EntryPlan>`.

- [ ] **Step 1: Write the failing tests.** Add `mod plan;` after `mod framing;` in `mod.rs`. Create `plan.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const UDP: &str = "turn:relay.test:3478?transport=udp";
    const TCP: &str = "turn:relay.test:3478?transport=tcp";
    const TLS: &str = "turns:relay.test:443?transport=tcp";
    const STUN: &str = "stun:stun.test:3478";

    fn entry(urls: &[&str]) -> RtcIceServerConfig {
        RtcIceServerConfig {
            urls: urls.iter().map(|url| url.to_string()).collect(),
            username: "u".into(),
            credential: "c".into(),
        }
    }

    fn planned(urls: Vec<UrlPlan>) -> EntryPlan {
        EntryPlan {
            username: "u".into(),
            credential: "c".into(),
            urls,
        }
    }

    #[test]
    fn the_first_udp_turn_url_is_probed_only_when_stream_urls_exist() {
        assert_eq!(probe_target(&entry(&[UDP, TCP, TLS])), Some(("relay.test".into(), 3478)));
        assert_eq!(probe_target(&entry(&[UDP])), None, "nothing to decide without a stream URL");
        assert_eq!(probe_target(&entry(&[TLS])), None, "nothing to probe without a UDP URL");
    }

    #[test]
    fn an_unreachable_relay_gets_its_stream_urls_bridged_and_its_udp_url_dropped() {
        let plans = decide(&[entry(&[STUN]), entry(&[UDP, TCP, TLS])], |_, _| false);
        assert_eq!(
            plans,
            vec![
                planned(vec![UrlPlan::Keep(STUN.into())]),
                planned(vec![UrlPlan::Bridge(TCP.into()), UrlPlan::Bridge(TLS.into())]),
            ]
        );
    }

    #[test]
    fn a_reachable_relay_keeps_plain_urls_and_bridges_nothing() {
        let plans = decide(&[entry(&[UDP, TCP, TLS])], |host, port| {
            host == "relay.test" && port == 3478
        });
        assert_eq!(plans, vec![planned(vec![UrlPlan::Keep(UDP.into())])]);
    }

    #[test]
    fn an_entry_with_no_udp_url_is_bridged_without_asking() {
        let asked = Cell::new(false);
        let plans = decide(&[entry(&[TLS])], |_, _| {
            asked.set(true);
            true
        });
        assert_eq!(plans, vec![planned(vec![UrlPlan::Bridge(TLS.into())])]);
        assert!(!asked.get(), "there was no UDP URL to ask about");
    }

    #[test]
    fn entries_without_stream_urls_pass_through_untouched() {
        let plans = decide(&[entry(&[STUN]), entry(&[UDP])], |_, _| {
            panic!("nothing here needs a probe")
        });
        assert_eq!(
            plans,
            vec![planned(vec![UrlPlan::Keep(STUN.into())]), planned(vec![UrlPlan::Keep(UDP.into())])]
        );
    }

    #[test]
    fn an_entry_with_no_urls_is_dropped() {
        assert_eq!(decide(&[entry(&[])], |_, _| false), vec![]);
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p vms-gateway turn_bridge::plan`
Expected: compile error, ``cannot find type `RtcIceServerConfig` in this scope`` and unresolved `probe_target`, `decide`, `UrlPlan`, `EntryPlan`.

- [ ] **Step 3: Implement.** Put this above the tests module in `plan.rs`:

```rust
//! Deciding which TURN URLs webrtc-rs gets as they are, and which go through a bridge.

use rtc::ice::url::{ProtoType, SchemeType, Url};
use vms_domain::RtcIceServerConfig;

/// What happens to one URL of an ICE server entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UrlPlan {
    /// Hand it to webrtc-rs unchanged.
    Keep(String),
    /// Carry it through a bridge; webrtc-rs gets a 127.0.0.1 URL instead.
    Bridge(String),
}

/// One ICE server entry after planning.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EntryPlan {
    pub(crate) username: String,
    pub(crate) credential: String,
    pub(crate) urls: Vec<UrlPlan>,
}

/// A URL webrtc-rs cannot use itself: `turns:`, or `turn:` over TCP.
fn is_stream_url(url: &Url) -> bool {
    url.scheme == SchemeType::Turns || (url.scheme == SchemeType::Turn && url.proto == ProtoType::Tcp)
}

fn is_udp_turn_url(url: &Url) -> bool {
    url.scheme == SchemeType::Turn && url.proto == ProtoType::Udp
}

/// The relay to probe for `entry`: its first `turn:` URL over UDP, and only when
/// the entry also has a stream URL — otherwise there is nothing to decide.
pub(crate) fn probe_target(entry: &RtcIceServerConfig) -> Option<(String, u16)> {
    let parsed: Vec<Url> = entry.urls.iter().filter_map(|raw| Url::parse_url(raw).ok()).collect();
    if !parsed.iter().any(is_stream_url) {
        return None;
    }
    parsed
        .iter()
        .find(|url| is_udp_turn_url(url))
        .map(|url| (url.host.clone(), url.port))
}

/// Plan every entry, given whether a probe target answered over UDP.
pub(crate) fn decide(
    servers: &[RtcIceServerConfig],
    udp_reachable: impl Fn(&str, u16) -> bool,
) -> Vec<EntryPlan> {
    servers
        .iter()
        .map(|entry| {
            let has_stream_url = entry
                .urls
                .iter()
                .any(|raw| Url::parse_url(raw).is_ok_and(|url| is_stream_url(&url)));
            let reachable = probe_target(entry).is_some_and(|(host, port)| udp_reachable(&host, port));
            let urls = entry
                .urls
                .iter()
                .filter_map(|raw| {
                    let Ok(url) = Url::parse_url(raw) else {
                        return Some(UrlPlan::Keep(raw.clone()));
                    };
                    match (has_stream_url, reachable) {
                        (true, true) if is_stream_url(&url) => None,
                        (true, false) if is_stream_url(&url) => Some(UrlPlan::Bridge(raw.clone())),
                        (true, false) if is_udp_turn_url(&url) => None,
                        _ => Some(UrlPlan::Keep(raw.clone())),
                    }
                })
                .collect();
            EntryPlan {
                username: entry.username.clone(),
                credential: entry.credential.clone(),
                urls,
            }
        })
        .filter(|plan| !plan.urls.is_empty())
        .collect()
}
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p vms-gateway turn_bridge::plan`
Expected: `6 passed`. Then `cargo test -p vms-gateway`: `100 passed; 0 failed; 1 ignored`.

- [ ] **Step 5: Leak check and commit**

```bash
git add edge/gateway/src/turn_bridge/mod.rs edge/gateway/src/turn_bridge/plan.rs
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | xargs -r grep -lniE "$LEAKS"'
git commit -m "Plain TURN URLs stay; stream URLs are bridged only when the relay is silent over UDP"
```

---

### Task 4: The bridge

**Files:**
- Create: `edge/gateway/src/turn_bridge/bridge.rs`
- Modify: `edge/gateway/src/turn_bridge/mod.rs` (add `mod bridge;`)
- Modify: `edge/gateway/Cargo.toml` (`tokio` line), `Cargo.lock`

**Interfaces:**
- Consumes: Task 1 `StreamSplitter`.
- Produces: `pub(crate) trait RelayStream: AsyncRead + AsyncWrite + Unpin + Send` (blanket impl); `pub(crate) type Connector = Arc<dyn Fn() -> BoxFuture<'static, io::Result<Box<dyn RelayStream>>> + Send + Sync>`; `pub(crate) struct BridgeStats` with `fn bytes_up(&self) -> u64` and `fn bytes_down(&self) -> u64`; `pub(crate) struct Bridge` with `async fn spawn(connector: Connector) -> io::Result<Bridge>`, `fn local_addr(&self) -> SocketAddr`, `fn stats(&self) -> Arc<BridgeStats>`, stopping on drop; `pub(crate) fn error_response(datagram: &[u8]) -> Option<Vec<u8>>`.

- [ ] **Step 1: Write the failing tests.** In `mod.rs`, add `mod bridge;` on the line above `mod framing;`, so the `mod` lines read `bridge`, `framing`, `plan`, `probe`. In `edge/gateway/Cargo.toml`, change `tokio.workspace = true` to `tokio = { workspace = true, features = ["io-util"] }`. Create `bridge.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rtc::stun::message::{Getter, METHOD_ALLOCATE, TransactionId};
    use tokio::net::{TcpListener, TcpStream};

    fn tcp_connector(addr: SocketAddr) -> Connector {
        Arc::new(move || -> BoxFuture<'static, io::Result<Box<dyn RelayStream>>> {
            Box::pin(async move {
                let stream = TcpStream::connect(addr).await?;
                Ok(Box::new(stream) as Box<dyn RelayStream>)
            })
        })
    }

    fn allocate_request() -> Message {
        let mut request = Message::new();
        let setters: [Box<dyn Setter>; 2] = [
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
        ];
        request.build(&setters).unwrap();
        request
    }

    async fn next_datagram(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(6), socket.recv_from(&mut buf))
            .await
            .expect("a datagram back from the bridge")
            .unwrap();
        buf[..n].to_vec()
    }

    #[tokio::test]
    async fn datagrams_cross_the_bridge_both_ways_whatever_the_read_boundaries() {
        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = Bridge::spawn(tcp_connector(relay.local_addr().unwrap())).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let request = allocate_request().raw;
        let channel_data = vec![0x40, 0x00, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0];
        client.send_to(&request, bridge.local_addr()).await.unwrap();
        client.send_to(&channel_data, bridge.local_addr()).await.unwrap();

        let (mut upstream, _) = relay.accept().await.unwrap();
        let both = [request.clone(), channel_data.clone()].concat();
        let mut arrived = vec![0u8; both.len()];
        upstream.read_exact(&mut arrived).await.unwrap();
        assert_eq!(arrived, both, "datagrams must reach the relay byte for byte, in order");

        // The relay sends both back, cut in the middle of the first message.
        upstream.write_all(&both[..7]).await.unwrap();
        upstream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        upstream.write_all(&both[7..]).await.unwrap();

        assert_eq!(next_datagram(&client).await, request, "the first message must come back whole");
        assert_eq!(next_datagram(&client).await, channel_data, "ChannelData must come back whole, padding included");
        assert_eq!(bridge.stats().bytes_up(), both.len() as u64);
        assert_eq!(bridge.stats().bytes_down(), both.len() as u64);
    }

    #[tokio::test]
    async fn a_relay_that_refuses_the_connection_turns_requests_into_errors_at_once() {
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let bridge = Bridge::spawn(tcp_connector(closed)).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let request = allocate_request();
        client.send_to(&request.raw, bridge.local_addr()).await.unwrap();

        let mut reply = Message::new();
        reply.raw = next_datagram(&client).await;
        reply.decode().unwrap();
        assert_eq!(reply.transaction_id, request.transaction_id);
        assert_eq!(reply.typ, MessageType::new(METHOD_ALLOCATE, CLASS_ERROR_RESPONSE));
        let mut code = ErrorCodeAttribute::default();
        code.get_from(&reply).unwrap();
        assert_eq!(code.code.0, 500, "the bridge answers a failed relay with 500");

        // ChannelData has nothing to answer, so it is dropped rather than echoed.
        client.send_to(&[0x40, 0x00, 0x00, 0x00], bridge.local_addr()).await.unwrap();
        let mut buf = [0u8; 64];
        assert!(
            tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buf)).await.is_err(),
            "no reply to ChannelData"
        );
    }

    #[tokio::test]
    async fn dropping_the_bridge_closes_its_connection_to_the_relay() {
        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = Bridge::spawn(tcp_connector(relay.local_addr().unwrap())).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(&allocate_request().raw, bridge.local_addr()).await.unwrap();
        let (mut upstream, _) = relay.accept().await.unwrap();
        let mut request = [0u8; 20];
        upstream.read_exact(&mut request).await.unwrap();

        drop(bridge);
        let mut rest = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(2), upstream.read(&mut rest))
            .await
            .expect("the connection must close when the bridge is dropped")
            .unwrap();
        assert_eq!(n, 0);
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `grep -c '^\[\[package\]\]' Cargo.lock` (note the number), then `cargo test -p vms-gateway turn_bridge::bridge`
Expected: compile error, ``cannot find type `Bridge` in this scope`` (and unresolved `Connector`, `RelayStream`, `Message`).

- [ ] **Step 3: Implement.** Put this above the tests module in `bridge.rs`:

```rust
//! One bridge: a loopback UDP socket webrtc-rs uses as its TURN server, carried
//! to the real relay over a stream.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use rtc::stun::error_code::{CODE_SERVER_ERROR, ErrorCodeAttribute};
use rtc::stun::message::{
    CLASS_ERROR_RESPONSE, CLASS_REQUEST, Message, MessageType, Setter, is_stun_message,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tracing::warn;

use super::framing::StreamSplitter;

/// Datagrams held for one source while its connection to the relay opens.
const QUEUE_WHILE_CONNECTING: usize = 16;
/// A relay that has not accepted the connection by then counts as unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection to the relay: TLS or plain TCP.
pub(crate) trait RelayStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> RelayStream for T {}

/// Opens a fresh connection to the relay.
pub(crate) type Connector =
    Arc<dyn Fn() -> BoxFuture<'static, io::Result<Box<dyn RelayStream>>> + Send + Sync>;

/// What a bridge has carried.
#[derive(Debug, Default)]
pub(crate) struct BridgeStats {
    up: AtomicU64,
    down: AtomicU64,
}

impl BridgeStats {
    /// Bytes carried from webrtc-rs to the relay.
    pub(crate) fn bytes_up(&self) -> u64 {
        self.up.load(Ordering::Relaxed)
    }

    /// Bytes carried from the relay back to webrtc-rs.
    pub(crate) fn bytes_down(&self) -> u64 {
        self.down.load(Ordering::Relaxed)
    }
}

/// A running bridge. Dropping it stops the bridge and closes its connections.
pub(crate) struct Bridge {
    local_addr: SocketAddr,
    stats: Arc<BridgeStats>,
    task: JoinHandle<()>,
}

impl Bridge {
    pub(crate) async fn spawn(connector: Connector) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let stats = Arc::new(BridgeStats::default());
        let task = tokio::spawn(run(socket, connector, Arc::clone(&stats)));
        Ok(Self {
            local_addr,
            stats,
            task,
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub(crate) fn stats(&self) -> Arc<BridgeStats> {
        Arc::clone(&self.stats)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(socket: Arc<UdpSocket>, connector: Connector, stats: Arc<BridgeStats>) {
    // Owned by this task, so aborting it aborts every session with it.
    let mut sessions = JoinSet::new();
    let mut queues: HashMap<SocketAddr, mpsc::Sender<Bytes>> = HashMap::new();
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, source) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                ) =>
            {
                continue;
            }
            Err(err) => {
                warn!(error = %err, "TURN bridge socket failed");
                return;
            }
        };
        if queues.get(&source).is_none_or(|queue| queue.is_closed()) {
            let (tx, rx) = mpsc::channel(QUEUE_WHILE_CONNECTING);
            sessions.spawn(session(
                Arc::clone(&socket),
                source,
                Arc::clone(&connector),
                rx,
                Arc::clone(&stats),
            ));
            queues.insert(source, tx);
        }
        // A full queue means the connection is still opening; webrtc-rs retransmits,
        // so dropping here is cheaper than buffering without bound.
        let _ = queues[&source].try_send(Bytes::copy_from_slice(&buf[..n]));
    }
}

async fn session(
    socket: Arc<UdpSocket>,
    source: SocketAddr,
    connector: Connector,
    mut queue: mpsc::Receiver<Bytes>,
    stats: Arc<BridgeStats>,
) {
    let failure = match tokio::time::timeout(CONNECT_TIMEOUT, connector()).await {
        Ok(Ok(stream)) => pump(stream, &socket, source, &mut queue, &stats).await,
        Ok(Err(err)) => err.to_string(),
        Err(_) => format!("no connection within {CONNECT_TIMEOUT:?}"),
    };
    warn!(%source, reason = %failure, "TURN bridge cannot reach the relay; failing its requests");
    // Answer every request from now on, so webrtc-rs gives up on this relay at
    // once instead of retransmitting for seconds.
    while let Some(datagram) = queue.recv().await {
        if let Some(reply) = error_response(&datagram) {
            let _ = socket.send_to(&reply, source).await;
        }
    }
}

/// Carry messages both ways until the connection ends; returns why it ended.
async fn pump(
    stream: Box<dyn RelayStream>,
    socket: &UdpSocket,
    source: SocketAddr,
    queue: &mut mpsc::Receiver<Bytes>,
    stats: &BridgeStats,
) -> String {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut splitter = StreamSplitter::default();
    let mut buf = vec![0u8; 65_536];
    loop {
        tokio::select! {
            datagram = queue.recv() => {
                let Some(datagram) = datagram else {
                    return "the bridge stopped".into();
                };
                if let Err(err) = writer.write_all(&datagram).await {
                    return err.to_string();
                }
                stats.up.fetch_add(datagram.len() as u64, Ordering::Relaxed);
            }
            read = reader.read(&mut buf) => {
                let n = match read {
                    Ok(0) => return "the relay closed the connection".into(),
                    Ok(n) => n,
                    Err(err) => return err.to_string(),
                };
                splitter.push(&buf[..n]);
                loop {
                    match splitter.next_message() {
                        Ok(Some(message)) => {
                            stats.down.fetch_add(message.len() as u64, Ordering::Relaxed);
                            let _ = socket.send_to(&message, source).await;
                        }
                        Ok(None) => break,
                        Err(err) => return err.to_string(),
                    }
                }
            }
        }
    }
}

/// A STUN error response (500) to `datagram` when it is a STUN request.
pub(crate) fn error_response(datagram: &[u8]) -> Option<Vec<u8>> {
    if !is_stun_message(datagram) {
        return None;
    }
    let mut request = Message::new();
    request.raw = datagram.to_vec();
    request.decode().ok()?;
    if request.typ.class != CLASS_REQUEST {
        return None;
    }
    let mut response = Message::new();
    let setters: [Box<dyn Setter>; 3] = [
        Box::new(request.transaction_id),
        Box::new(MessageType::new(request.typ.method, CLASS_ERROR_RESPONSE)),
        Box::new(ErrorCodeAttribute {
            code: CODE_SERVER_ERROR,
            reason: b"the gateway cannot reach the relay".to_vec(),
        }),
    ];
    response.build(&setters).ok()?;
    Some(response.raw)
}
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p vms-gateway turn_bridge::bridge`
Expected: `3 passed`. Then `cargo test -p vms-gateway`: `103 passed; 0 failed; 1 ignored`. Then `grep -c '^\[\[package\]\]' Cargo.lock` prints the same number as in Step 2, and `git diff --stat Cargo.lock` shows no change or dependency lines only.

- [ ] **Step 5: Leak check and commit**

```bash
git add edge/gateway/Cargo.toml Cargo.lock edge/gateway/src/turn_bridge/mod.rs edge/gateway/src/turn_bridge/bridge.rs
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | xargs -r grep -lniE "$LEAKS"'
git commit -m "A loopback bridge carries TURN datagrams over a stream and fails fast when the relay is unreachable"
```

(If `git diff --cached --name-only` does not list `Cargo.lock` because it did not change, that is fine.)

---

### Task 5: TLS trust and the connector

**Files:**
- Create: `edge/gateway/src/turn_bridge/tls.rs`
- Modify: `edge/gateway/src/turn_bridge/mod.rs` (add `mod tls;`)
- Modify: `edge/gateway/Cargo.toml` (`[dependencies]`), `Cargo.lock`

**Interfaces:**
- Consumes: Task 4 `Connector`, `RelayStream`.
- Produces: `pub(crate) fn root_store(extra_ca_file: Option<&Path>) -> (RootCertStore, Option<String>)` (the string is a problem to log); `pub(crate) fn client_config(roots: RootCertStore) -> Arc<ClientConfig>`; `pub(crate) fn connector(url: &Url, config: Arc<ClientConfig>) -> Connector` (TLS for `turns:`, plain TCP otherwise).

- [ ] **Step 1: Write the failing tests.** Add `mod tls;` to `mod.rs` (alphabetical, after `mod probe;`). Create `tls.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A throwaway self-signed CA certificate (no key anywhere), valid until 2126.
    const TEST_CA: &str = "-----BEGIN CERTIFICATE-----
MIIBoDCCAUegAwIBAgIUAp0CUGzMvRlLe7SWcI6QKM6jCzowCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwScmVsYXlzaWdodC10ZXN0LWNhMCAXDTI2MDkxNDA1NTg1MloY
DzIxMjYwODIxMDU1ODUyWjAdMRswGQYDVQQDDBJyZWxheXNpZ2h0LXRlc3QtY2Ew
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASxW/d+8eoofuyOxvy7Es6UgM+XLnFW
hPWQxPcty1oO3i9/VNvwQxxHXTZABrxmY/5sU7EypmLJGZNE7rTGbVUpo2MwYTAd
BgNVHQ4EFgQUUko059P0ORUiPvwAWBtuTbxRtOkwHwYDVR0jBBgwFoAUUko059P0
ORUiPvwAWBtuTbxRtOkwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAgQw
CgYIKoZIzj0EAwIDRwAwRAIgE+DTSNoj1ICrf7+2MxddDpE6zZXXiEfEW7Uz48mJ
RSoCIGUqpC9lwzfdqANSbGctm8IfiHvJnTP8MHgwDUVG9WeY
-----END CERTIFICATE-----
";

    fn public_roots() -> usize {
        webpki_roots::TLS_SERVER_ROOTS.len()
    }

    #[test]
    fn without_a_ca_file_only_the_public_roots_are_trusted() {
        let (roots, problem) = root_store(None);
        assert_eq!(roots.len(), public_roots());
        assert_eq!(problem, None);
    }

    #[test]
    fn a_ca_file_adds_its_certificate() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), TEST_CA).unwrap();
        let (roots, problem) = root_store(Some(file.path()));
        assert_eq!(problem, None);
        assert_eq!(roots.len(), public_roots() + 1);
    }

    #[test]
    fn a_missing_ca_file_is_reported_and_the_public_roots_still_apply() {
        let (roots, problem) = root_store(Some(Path::new("/nonexistent/relaysight-turn-ca.pem")));
        assert!(problem.expect("a missing file is a problem").contains("cannot read"));
        assert_eq!(roots.len(), public_roots());
    }

    #[test]
    fn a_ca_file_without_certificates_is_reported() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "not a certificate\n").unwrap();
        let (roots, problem) = root_store(Some(file.path()));
        assert!(problem.expect("an empty file is a problem").contains("holds no certificates"));
        assert_eq!(roots.len(), public_roots());
    }

    #[tokio::test]
    async fn a_tcp_url_connects_without_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = Url::parse_url(&format!("turn:127.0.0.1:{port}?transport=tcp")).unwrap();
        let connect = connector(&url, client_config(root_store(None).0));

        let (accepted, stream) = tokio::join!(listener.accept(), connect());
        let (mut server, _) = accepted.unwrap();
        let mut stream = stream.unwrap();
        stream.write_all(b"turn").await.unwrap();
        let mut arrived = [0u8; 4];
        server.read_exact(&mut arrived).await.unwrap();
        assert_eq!(&arrived, b"turn", "plain TCP must carry bytes unchanged");
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `grep -c '^\[\[package\]\]' Cargo.lock` (note the number), then `cargo test -p vms-gateway turn_bridge::tls`
Expected: compile error, unresolved crate ``webpki_roots`` and ``cannot find function `root_store` in this scope``.

- [ ] **Step 3: Implement.** Add to `[dependencies]` in `edge/gateway/Cargo.toml`, keeping the list's order (after `rtc = "0.20.3"`):

```toml
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
tokio-rustls = { version = "0.26", default-features = false, features = ["ring", "tls12"] }
webpki-roots = "1"
```

Put this above the tests module in `tls.rs`:

```rust
//! TLS to the relay: which certificates to trust, and how to connect.

use std::io;
use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;
use rtc::ice::url::{SchemeType, Url};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use super::bridge::{Connector, RelayStream};

/// The public roots, plus the certificates in `extra_ca_file` when one is given.
/// A file that cannot be used comes back as a problem to log, and the public
/// roots are used alone.
pub(crate) fn root_store(extra_ca_file: Option<&Path>) -> (RootCertStore, Option<String>) {
    let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let Some(path) = extra_ca_file else {
        return (roots, None);
    };
    let certificates: Vec<CertificateDer<'static>> = match CertificateDer::pem_file_iter(path) {
        Ok(iter) => match iter.collect::<Result<Vec<_>, _>>() {
            Ok(certificates) => certificates,
            Err(err) => return (roots, Some(format!("cannot parse {}: {err}", path.display()))),
        },
        Err(err) => return (roots, Some(format!("cannot read {}: {err}", path.display()))),
    };
    if certificates.is_empty() {
        return (roots, Some(format!("{} holds no certificates", path.display())));
    }
    let mut extra = RootCertStore::empty();
    for certificate in certificates {
        if let Err(err) = extra.add(certificate) {
            return (
                roots,
                Some(format!("{} holds an unusable certificate: {err}", path.display())),
            );
        }
    }
    roots.extend(extra.roots);
    (roots, None)
}

/// A client configuration on the `ring` provider trusting `roots`.
pub(crate) fn client_config(roots: RootCertStore) -> Arc<ClientConfig> {
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Connections for a stream URL: TLS for `turns:`, plain TCP for `turn:…?transport=tcp`.
/// The TLS server name is the URL's host; there is no way to skip verification.
pub(crate) fn connector(url: &Url, config: Arc<ClientConfig>) -> Connector {
    let host = url.host.clone();
    let port = url.port;
    let tls = (url.scheme == SchemeType::Turns).then(|| TlsConnector::from(config));
    Arc::new(move || -> BoxFuture<'static, io::Result<Box<dyn RelayStream>>> {
        let host = host.clone();
        let tls = tls.clone();
        Box::pin(async move {
            let tcp = TcpStream::connect((host.as_str(), port)).await?;
            let Some(tls) = tls else {
                return Ok(Box::new(tcp) as Box<dyn RelayStream>);
            };
            let name = ServerName::try_from(host)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            let stream = tls.connect(name, tcp).await?;
            Ok(Box::new(stream) as Box<dyn RelayStream>)
        })
    })
}
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p vms-gateway turn_bridge::tls`
Expected: `5 passed`. Then `cargo test -p vms-gateway`: `108 passed; 0 failed; 1 ignored`. Then `grep -c '^\[\[package\]\]' Cargo.lock` prints the same number as in Step 2; `git diff Cargo.lock` only adds `"rustls"`, `"tokio-rustls"` and `"webpki-roots"` to `vms-gateway`'s dependency list.

- [ ] **Step 5: Leak check and commit**

```bash
git add edge/gateway/Cargo.toml Cargo.lock edge/gateway/src/turn_bridge/mod.rs edge/gateway/src/turn_bridge/tls.rs
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | xargs -r grep -lniE "$LEAKS"'
git commit -m "The bridge trusts the public roots plus an optional CA file, and speaks TLS or TCP to the relay"
```

---

### Task 6: Live sessions use the plan and report the relay transport

**Files:**
- Modify: `edge/gateway/src/turn_bridge/mod.rs` (plan, guard, transport, tests)
- Modify: `edge/gateway/src/icepath.rs:81-123` (`observed` → `observed_candidate`)
- Modify: `edge/gateway/src/live.rs` (imports, `start_h264`, the session task)

**Interfaces:**
- Consumes: Task 2 `ProbeCache`, `PROBE_TIMEOUT`, `VERDICT_TTL`; Task 3 `probe_target`, `decide`, `UrlPlan`; Task 4 `Bridge`, `BridgeStats`; Task 5 `root_store`, `client_config`, `connector`.
- Produces: `pub(crate) async fn turn_bridge::plan(servers: Vec<RtcIceServerConfig>) -> (Vec<RtcIceServerConfig>, BridgeGuard)`; `pub(crate) struct BridgeGuard` with `fn stats(&self) -> Vec<Arc<BridgeStats>>` and `fn transport_for_url(&self, url: &str) -> Option<RelayTransport>`; `pub(crate) enum RelayTransport { Udp, Tcp, Tls }` (`Display`: `udp`, `tcp`, `tls`); `pub(crate) use bridge::BridgeStats`; `pub async fn icepath::observed_candidate(peer) -> (PathKind, Option<String>)`; `pub(crate) async fn live::start_h264_with(rtsp_uri: String, username: Option<String>, password: Option<String>, offer_sdp: String, offer_type: String, ice_servers: Vec<RtcIceServerConfig>, session_seconds: u32, ice_transport_policy: Option<RTCIceTransportPolicy>) -> anyhow::Result<(LiveSessionAnswer, Vec<Arc<BridgeStats>>)>`.

- [ ] **Step 1: Write the failing tests.** Append to `edge/gateway/src/turn_bridge/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, UdpSocket};

    #[tokio::test]
    async fn a_relay_silent_over_udp_is_bridged_and_the_bridge_is_recognised() {
        let silent_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_port = silent_udp.local_addr().unwrap().port();
        let tls_port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let servers = vec![
            RtcIceServerConfig {
                urls: vec!["stun:stun.test:3478".into()],
                username: String::new(),
                credential: String::new(),
            },
            RtcIceServerConfig {
                urls: vec![
                    format!("turn:127.0.0.1:{udp_port}?transport=udp"),
                    format!("turns:127.0.0.1:{tls_port}?transport=tcp"),
                ],
                username: "u".into(),
                credential: "c".into(),
            },
        ];

        let (planned, guard) = plan(servers).await;

        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].urls, vec!["stun:stun.test:3478".to_string()]);
        assert_eq!(planned[1].urls.len(), 1, "UDP dropped, TLS replaced: {:?}", planned[1].urls);
        let bridged = planned[1].urls[0].clone();
        assert!(
            bridged.starts_with("turn:127.0.0.1:") && bridged.ends_with("?transport=udp"),
            "{bridged}"
        );
        assert_eq!((planned[1].username.as_str(), planned[1].credential.as_str()), ("u", "c"));
        assert_eq!(guard.stats().len(), 1);
        assert_eq!(guard.transport_for_url(&bridged), Some(RelayTransport::Tls));
        assert_eq!(
            guard.transport_for_url("turn:relay.test:3478?transport=udp"),
            Some(RelayTransport::Udp)
        );
        assert_eq!(guard.transport_for_url("not a url"), None);
        assert_eq!(RelayTransport::Tls.to_string(), "tls");
    }

    #[tokio::test]
    async fn servers_without_stream_urls_are_neither_probed_nor_bridged() {
        let servers = vec![RtcIceServerConfig {
            urls: vec!["turn:127.0.0.1:9?transport=udp".into()],
            username: "u".into(),
            credential: "c".into(),
        }];
        let started = std::time::Instant::now();
        let (planned, guard) = plan(servers).await;
        assert_eq!(planned[0].urls, vec!["turn:127.0.0.1:9?transport=udp".to_string()]);
        assert!(guard.stats().is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "nothing to decide must not wait for a probe"
        );
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p vms-gateway turn_bridge::tests`
Expected: compile error, ``cannot find function `plan` in this scope`` and unresolved `RtcIceServerConfig`, `RelayTransport`.

- [ ] **Step 3: Implement the plan, the guard and the transport.** In `mod.rs`, below the `mod` lines and above the tests, add:

```rust
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use rtc::ice::url::{SchemeType, Url};
use tracing::{info, warn};
use vms_domain::RtcIceServerConfig;

use bridge::Bridge;
pub(crate) use bridge::BridgeStats;
use plan::UrlPlan;
use probe::ProbeCache;

/// Extra certificate authority for relays the public roots do not cover.
const CA_FILE_ENV: &str = "GATEWAY_TURN_CA_FILE";

static PROBES: LazyLock<ProbeCache> = LazyLock::new(|| ProbeCache::new(probe::VERDICT_TTL));

/// How a relayed session reaches the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayTransport {
    Udp,
    Tcp,
    Tls,
}

impl std::fmt::Display for RelayTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Tls => "tls",
        })
    }
}

/// The bridges one live session uses. Dropping it stops them.
#[derive(Default)]
pub(crate) struct BridgeGuard {
    bridges: Vec<(Bridge, RelayTransport)>,
}

impl BridgeGuard {
    /// Counters for each bridge, in the order they were started.
    pub(crate) fn stats(&self) -> Vec<Arc<BridgeStats>> {
        self.bridges.iter().map(|(bridge, _)| bridge.stats()).collect()
    }

    /// The transport behind a relay candidate's TURN URL: a bridge's own when the
    /// URL points at one of this session's bridges, UDP for any other `turn:` URL.
    pub(crate) fn transport_for_url(&self, url: &str) -> Option<RelayTransport> {
        let parsed = Url::parse_url(url).ok()?;
        let bridged = self.bridges.iter().find(|(bridge, _)| {
            bridge.local_addr().ip().to_string() == parsed.host && bridge.local_addr().port() == parsed.port
        });
        match bridged {
            Some((_, transport)) => Some(*transport),
            None => (parsed.scheme == SchemeType::Turn).then_some(RelayTransport::Udp),
        }
    }
}

/// The ICE servers for one live session, with any bridges they need running.
pub(crate) async fn plan(servers: Vec<RtcIceServerConfig>) -> (Vec<RtcIceServerConfig>, BridgeGuard) {
    let mut verdicts: HashMap<(String, u16), bool> = HashMap::new();
    for (host, port) in servers.iter().filter_map(plan::probe_target) {
        if verdicts.contains_key(&(host.clone(), port)) {
            continue;
        }
        let reachable = PROBES.reachable(&host, port, probe::PROBE_TIMEOUT).await;
        info!(relay = %format!("{host}:{port}"), reachable, "relay probed over UDP");
        verdicts.insert((host, port), reachable);
    }
    let decided = plan::decide(&servers, |host, port| {
        verdicts.get(&(host.to_owned(), port)).copied().unwrap_or(false)
    });

    let mut guard = BridgeGuard::default();
    let mut tls_config = None;
    let mut planned = Vec::new();
    for entry in decided {
        let mut urls = Vec::new();
        for url_plan in entry.urls {
            let raw = match url_plan {
                UrlPlan::Keep(raw) => {
                    urls.push(raw);
                    continue;
                }
                UrlPlan::Bridge(raw) => raw,
            };
            let Ok(url) = Url::parse_url(&raw) else {
                continue;
            };
            let config = tls_config.get_or_insert_with(|| {
                let ca_file = std::env::var_os(CA_FILE_ENV).map(std::path::PathBuf::from);
                let (roots, problem) = tls::root_store(ca_file.as_deref());
                if let Some(problem) = problem {
                    warn!(%problem, "{CA_FILE_ENV} ignored; trusting the public roots only");
                }
                tls::client_config(roots)
            });
            let transport = if url.scheme == SchemeType::Turns {
                RelayTransport::Tls
            } else {
                RelayTransport::Tcp
            };
            match Bridge::spawn(tls::connector(&url, Arc::clone(config))).await {
                Ok(bridge) => {
                    let local = bridge.local_addr();
                    info!(relay = %raw, bridge = %local, %transport, "bridging TURN to the relay");
                    urls.push(format!("turn:{}:{}?transport=udp", local.ip(), local.port()));
                    guard.bridges.push((bridge, transport));
                }
                Err(err) => warn!(relay = %raw, error = %err, "could not start a TURN bridge"),
            }
        }
        if !urls.is_empty() {
            planned.push(RtcIceServerConfig {
                urls,
                username: entry.username,
                credential: entry.credential,
            });
        }
    }
    (planned, guard)
}
```

- [ ] **Step 4: Run the module tests and watch them pass**

Run: `cargo test -p vms-gateway turn_bridge`
Expected: all `turn_bridge` tests pass (`27 passed`).

- [ ] **Step 5: Report the selected candidate's TURN URL.** In `edge/gateway/src/icepath.rs`, replace the whole `observed` function (its doc comment through its closing brace) with:

```rust
/// Read the path a connected peer settled on.
///
/// Thin glue over `busiest_pair`, which carries the logic and the tests. Anything
/// missing maps to `Unknown` rather than to a guess: this number goes into a cost
/// model, and a wrong label there is worse than an absent one.
pub async fn observed(
    peer: &std::sync::Arc<dyn webrtc::peer_connection::PeerConnection>,
) -> PathKind {
    observed_candidate(peer).await.0
}

/// The path, and the selected local candidate's TURN URL when it has one.
pub async fn observed_candidate(
    peer: &std::sync::Arc<dyn webrtc::peer_connection::PeerConnection>,
) -> (PathKind, Option<String>) {
    use rtc::peer_connection::transport::RTCIceCandidateType;
    use rtc::statistics::StatsSelector;
    use rtc::statistics::report::RTCStatsReportEntry;

    let report = peer
        .get_stats(std::time::Instant::now(), StatsSelector::None)
        .await;

    let Some(local_id) = busiest_pair(report.candidate_pairs().map(|p| {
        (
            p.local_candidate_id.as_str(),
            p.bytes_sent + p.bytes_received,
        )
    })) else {
        return (PathKind::Unknown, None);
    };

    let found = report.iter().find_map(|e| match e {
        RTCStatsReportEntry::LocalCandidate(c) if entry_is_candidate(&c.stats.id, local_id) => {
            Some(c)
        }
        _ => None,
    });

    match found {
        Some(c) => {
            let kind = match c.candidate_type {
                RTCIceCandidateType::Host => PathKind::Host,
                RTCIceCandidateType::Srflx => PathKind::ServerReflexive,
                RTCIceCandidateType::Prflx => PathKind::PeerReflexive,
                RTCIceCandidateType::Relay => PathKind::Relay,
                _ => PathKind::Unknown,
            };
            (kind, (!c.url.is_empty()).then(|| c.url.clone()))
        }
        None => (PathKind::Unknown, None),
    }
}
```

- [ ] **Step 6: Wire the plan into live sessions.** In `edge/gateway/src/live.rs`:

(a) In the `use rtc::{ … peer_connection::{ configuration::{ … } } }` import, change

```rust
        configuration::{
            RTCConfigurationBuilder,
```

to

```rust
        configuration::{
            RTCConfigurationBuilder, RTCIceTransportPolicy,
```

(b) Make three replacements in `start_h264`. First, replace its signature:

```rust
pub async fn start_h264(
    rtsp_uri: String,
    username: Option<String>,
    password: Option<String>,
    offer_sdp: String,
    offer_type: String,
    ice_servers: Vec<RtcIceServerConfig>,
    session_seconds: u32,
) -> anyhow::Result<LiveSessionAnswer> {
```

with:

```rust
pub async fn start_h264(
    rtsp_uri: String,
    username: Option<String>,
    password: Option<String>,
    offer_sdp: String,
    offer_type: String,
    ice_servers: Vec<RtcIceServerConfig>,
    session_seconds: u32,
) -> anyhow::Result<LiveSessionAnswer> {
    start_h264_with(
        rtsp_uri,
        username,
        password,
        offer_sdp,
        offer_type,
        ice_servers,
        session_seconds,
        None,
    )
    .await
    .map(|(answer, _)| answer)
}

/// `start_h264`, optionally forcing an ICE transport policy, also returning the
/// counters of any TURN bridges the session uses. Production passes `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_h264_with(
    rtsp_uri: String,
    username: Option<String>,
    password: Option<String>,
    offer_sdp: String,
    offer_type: String,
    ice_servers: Vec<RtcIceServerConfig>,
    session_seconds: u32,
    ice_transport_policy: Option<RTCIceTransportPolicy>,
) -> anyhow::Result<(LiveSessionAnswer, Vec<Arc<crate::turn_bridge::BridgeStats>>)> {
```

and replace:

```rust
    let rtc_servers = ice_servers
        .into_iter()
```

with:

```rust
    // Relays this site cannot reach over UDP go through local TLS/TCP bridges;
    // the guard keeps them running for the life of the session.
    let (ice_servers, bridges) = crate::turn_bridge::plan(ice_servers).await;
    let bridge_stats = bridges.stats();
    let rtc_servers = ice_servers
        .into_iter()
```

and replace:

```rust
    let rtc_config = RTCConfigurationBuilder::new()
        .with_ice_servers(rtc_servers)
        .build();
```

with:

```rust
    let mut rtc_config = RTCConfigurationBuilder::new().with_ice_servers(rtc_servers);
    if let Some(policy) = ice_transport_policy {
        rtc_config = rtc_config.with_ice_transport_policy(policy);
    }
    let rtc_config = rtc_config.build();
```

(c) In the spawned session task, replace:

```rust
            let path = crate::icepath::observed(&peer_stats).await;
            info!(
                session_id = %task_session_id,
                path = %path,
                relayed = path.is_relayed(),
                "WebRTC peer connected; starting RTSP forwarding"
            );
```

with:

```rust
            let (path, candidate_url) = crate::icepath::observed_candidate(&peer_stats).await;
            // Which way a relayed session reaches the relay: plain UDP, or a TLS/TCP bridge.
            let relay_transport = candidate_url
                .filter(|_| path.is_relayed())
                .and_then(|url| bridges.transport_for_url(&url))
                .map_or_else(|| "-".to_owned(), |transport| transport.to_string());
            info!(
                session_id = %task_session_id,
                path = %path,
                relayed = path.is_relayed(),
                relay_transport = %relay_transport,
                "WebRTC peer connected; starting RTSP forwarding"
            );
```

and replace:

```rust
        let _ = peer_task.close().await;
    });
```

with:

```rust
        let _ = peer_task.close().await;
        // The bridges outlive the peer connection by exactly this long.
        drop(bridges);
    });
```

(d) Replace the function's final expression:

```rust
    Ok(LiveSessionAnswer {
        session_id,
        sdp,
        sdp_type,
        codec: "H264".into(),
        expires_at,
    })
```

with:

```rust
    Ok((
        LiveSessionAnswer {
            session_id,
            sdp,
            sdp_type,
            codec: "H264".into(),
            expires_at,
        },
        bridge_stats,
    ))
```

- [ ] **Step 7: Run the whole gateway suite**

Run: `cargo test -p vms-gateway`
Expected: `110 passed; 0 failed; 1 ignored`, with no `dead_code` warnings left for `turn_bridge`. The existing live tests (`h264_reaches_a_browser_peer_from_a_real_camera` and the others in `live.rs`) pass unchanged: they pass no ICE servers, so the plan probes nothing and starts no bridge.

- [ ] **Step 8: Leak check and commit**

```bash
git add edge/gateway/src/turn_bridge/mod.rs edge/gateway/src/icepath.rs edge/gateway/src/live.rs
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | xargs -r grep -lniE "$LEAKS"'
git commit -m "Live sessions bridge TURN to the relay when UDP cannot reach it, and log which way they relayed"
```

---

### Task 7: The end-to-end check

**Files:**
- Create: `scripts/check-gateway-relay.sh`
- Modify: `Makefile` (`.PHONY` line and a new target)
- Modify: `edge/gateway/src/live.rs` (tests module: one ignored test)

**Interfaces:**
- Consumes: Task 6 `live::start_h264_with`, `BridgeStats::bytes_up`; `FakeCamera::start(false) -> FakeCamera { url, .. }`; `FakeBrowser::offer()`, `offer_sdp()`, `accept_answer(&str)`, `wait_for_media(Duration) -> Received { packets, payload_bytes }`.
- Produces: `make check-gateway-relay`; test `live::tests::a_session_relays_over_tls_when_udp_to_the_relay_is_blocked`, reading `RELAYSIGHT_TEST_TURNS_URL`, `RELAYSIGHT_TEST_TURN_UDP_URL`, `RELAYSIGHT_TEST_TURN_USER`, `RELAYSIGHT_TEST_TURN_PASS` and (through the bridge) `GATEWAY_TURN_CA_FILE`.

- [ ] **Step 1: Write the check.** Create `scripts/check-gateway-relay.sh`:

```bash
#!/usr/bin/env bash
# The gateway's side of TURN over TLS, end to end: a local coturn with a TLS
# listener and a throwaway certificate authority, the gateway's UDP TURN URL
# pointing at a closed port so its probe fails, and a live session limited to
# relay candidates that therefore has to cross the TLS bridge.
#
#   make check-gateway-relay
#
# Needs Docker with coturn/coturn:4.6, openssl, ss, and ports 13478, 13479 and
# 15349 free. See docs/TURN-DEPLOY.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE=coturn/coturn:4.6
NAME=relaysight-gateway-relay-check
PLAIN_PORT=13478
CLOSED_UDP_PORT=13479
TLS_PORT=15349

fail() { echo "FAIL: $*" >&2; exit 1; }

printf '1/4 ports are free... '
if ss -ltnu | grep -qE ":(${PLAIN_PORT}|${CLOSED_UDP_PORT}|${TLS_PORT})\b"; then
  fail "something already listens on ${PLAIN_PORT}, ${CLOSED_UDP_PORT} or ${TLS_PORT}"
fi
echo ok

WORK="$(mktemp -d)"
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

printf '2/4 a throwaway certificate authority and a certificate for localhost... '
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$WORK/ca.key" -out "$WORK/ca.pem" -days 1 -subj /CN=relaysight-check-ca \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$WORK/server.key" -out "$WORK/server.csr" -subj /CN=localhost 2>/dev/null
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' >"$WORK/server.ext"
openssl x509 -req -in "$WORK/server.csr" -CA "$WORK/ca.pem" -CAkey "$WORK/ca.key" \
  -CAcreateserial -out "$WORK/server.pem" -days 1 -extfile "$WORK/server.ext" 2>/dev/null
# coturn runs as nobody inside the container and reads these through the mount.
chmod 0755 "$WORK"
chmod 0644 "$WORK/server.pem" "$WORK/server.key"
echo ok

printf '3/4 coturn listens for TLS on %s... ' "$TLS_PORT"
docker run -d --name "$NAME" --network host -v "$WORK:/certs:ro" "$IMAGE" \
  --listening-ip=127.0.0.1 --listening-port="$PLAIN_PORT" --tls-listening-port="$TLS_PORT" \
  --min-port=49300 --max-port=49340 --external-ip=127.0.0.1 --realm=relay.test \
  --lt-cred-mech --user=check:checkpass --allow-loopback-peers \
  --cert=/certs/server.pem --pkey=/certs/server.key \
  --fingerprint --no-cli --log-file=stdout >/dev/null
for _ in $(seq 1 60); do
  ss -ltn | grep -qE ":${TLS_PORT}\b" && break
  sleep 0.5
done
ss -ltn | grep -qE ":${TLS_PORT}\b" || fail "coturn never listened on ${TLS_PORT}: $(docker logs "$NAME" 2>&1 | tail -5)"
echo ok

echo '4/4 a relay-only live session crosses the TLS bridge:'
export GATEWAY_TURN_CA_FILE="$WORK/ca.pem"
export RELAYSIGHT_TEST_TURNS_URL="turns:localhost:${TLS_PORT}?transport=tcp"
export RELAYSIGHT_TEST_TURN_UDP_URL="turn:127.0.0.1:${CLOSED_UDP_PORT}?transport=udp"
export RELAYSIGHT_TEST_TURN_USER=check
export RELAYSIGHT_TEST_TURN_PASS=checkpass
cd "$ROOT"
cargo test -p vms-gateway -- --ignored --exact \
  live::tests::a_session_relays_over_tls_when_udp_to_the_relay_is_blocked 2>&1 | tee "$WORK/test.log"
grep -q "test result: ok. 1 passed" "$WORK/test.log" \
  || fail "the relayed-session test did not run and pass"

echo "Gateway relay check passed."
```

Then `chmod +x scripts/check-gateway-relay.sh`. In `Makefile`, change line 1 to:

```make
.PHONY: web community plugins edge demo gateway-image check-web check-relay check-gateway-relay
```

and append:

```make

check-gateway-relay:
	./scripts/check-gateway-relay.sh
```

(The recipe line starts with a tab.)

- [ ] **Step 2: Run it and watch it fail**

Run: `make check-gateway-relay`
Expected: steps 1–3 print `ok`; step 4's cargo output ends with `0 passed` (the test does not exist yet), then `FAIL: the relayed-session test did not run and pass`, and `make` exits non-zero. Afterwards `docker ps -a --filter name=relaysight-gateway-relay-check -q` prints nothing.

- [ ] **Step 3: Write the test.** In `edge/gateway/src/live.rs`, inside `mod tests`, add after the last test:

```rust
    #[tokio::test]
    #[ignore = "needs a local TLS relay; run make check-gateway-relay"]
    async fn a_session_relays_over_tls_when_udp_to_the_relay_is_blocked() {
        let var = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| panic!("set {name}; run make check-gateway-relay"))
        };
        let camera = FakeCamera::start(false).await.unwrap();
        let browser = FakeBrowser::offer().await.unwrap();
        let ice_servers = vec![vms_domain::RtcIceServerConfig {
            urls: vec![var("RELAYSIGHT_TEST_TURN_UDP_URL"), var("RELAYSIGHT_TEST_TURNS_URL")],
            username: var("RELAYSIGHT_TEST_TURN_USER"),
            credential: var("RELAYSIGHT_TEST_TURN_PASS"),
        }];

        let (answer, bridges) = super::start_h264_with(
            camera.url.clone(),
            None,
            None,
            browser.offer_sdp().to_owned(),
            "offer".into(),
            ice_servers,
            20,
            Some(rtc::peer_connection::configuration::RTCIceTransportPolicy::Relay),
        )
        .await
        .expect("the gateway answers with a relayed candidate");
        assert_eq!(
            bridges.len(),
            1,
            "UDP to the relay is blocked, so exactly one TLS bridge must carry the session"
        );

        browser.accept_answer(&answer.sdp).await.unwrap();
        let received = browser
            .wait_for_media(Duration::from_secs(20))
            .await
            .expect("media must arrive through the relay");
        assert!(
            received.payload_bytes > 1000,
            "only {} payload bytes arrived",
            received.payload_bytes
        );
        // Limited to relay candidates, the only way out is the bridge, so the
        // video has to show up in its counter.
        assert!(
            bridges[0].bytes_up() > received.payload_bytes,
            "{} bytes crossed the TLS bridge for {} payload bytes received",
            bridges[0].bytes_up(),
            received.payload_bytes
        );
    }
```

- [ ] **Step 4: Run the check and watch it pass**

Run: `make check-gateway-relay`
Expected: steps 1–3 `ok`; step 4 shows `test live::tests::a_session_relays_over_tls_when_udp_to_the_relay_is_blocked ... ok` and `test result: ok. 1 passed`; then `Gateway relay check passed.`, exit 0. Then `cargo test -p vms-gateway` still reports `110 passed; 0 failed; 2 ignored`.

If step 4 fails in a way not described here (for example coturn refuses the loopback peer, or no relay candidate is gathered), stop and report it with the test output and `docker logs relaysight-gateway-relay-check` (run the script with `trap - EXIT` temporarily removed to keep the container) — do not change constants in the spec to make it pass.

- [ ] **Step 5: Leak check and commit**

```bash
git add scripts/check-gateway-relay.sh Makefile edge/gateway/src/live.rs
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | xargs -r grep -lniE "$LEAKS"'
git commit -m "make check-gateway-relay: a relay-only live session has to cross the TLS bridge to a local coturn"
```

---

### Task 8: Docs, notes and backlog, and the final run

**Files:**
- Modify: `docs/TURN-DEPLOY.md` (the gateway section)
- Modify: `docs/RUNNING-LOCALLY.md` ("Adding the relay")
- Modify: `docs/BACKLOG.md` (the gateway TURN line)
- Modify: the private notes file at the repository root that `publish.sh` excludes (its line 8): the "Known not to work" paragraph

**Interfaces:**
- Consumes: `make check-gateway-relay` (Task 7), `GATEWAY_TURN_CA_FILE` and `relay_transport` (Task 6).

- [ ] **Step 1: Replace the gateway section of the deploy guide.** In `docs/TURN-DEPLOY.md`, replace everything from the line `## What this does not fix yet: the gateway` up to, but not including, the line `## Capacity` with the text below, then replace `YYYY-MM-DD` with the date of your passing `make check-gateway-relay` run (`date +%F`):

````markdown
## How the gateway reaches the relay

webrtc-rs, which the gateway uses for live video, only speaks TURN over UDP: as
of 0.20.5 it skips every `turns:` URL and every `turn:` URL over TCP
(https://github.com/webrtc-rs/webrtc/issues/848). The gateway works around that
outside the library, in `edge/gateway/src/turn_bridge/`:

1. Before a live session it sends one STUN Binding request to the relay's UDP
   TURN URL. If the relay answers within a second, the session uses plain UDP
   TURN and nothing else changes. The answer is remembered for ten minutes,
   because whether UDP gets out is a property of the site.
2. If the relay does not answer, the gateway drops the UDP URL and bridges each
   `turns:` (and `turn:…?transport=tcp`) URL: webrtc-rs talks UDP TURN to a
   socket on 127.0.0.1, and the gateway carries every message to the relay over
   TLS and back. If the relay cannot be reached that way either, the bridge
   answers webrtc-rs's requests with an error at once, so the session starts
   without a relay instead of spending seconds on retries.

The "WebRTC peer connected" log line says which way a relayed session went:
`relay_transport=udp`, `tcp` or `tls`.

The bridge verifies the relay's certificate against the public roots, with the
URL's host as the server name. A relay whose certificate comes from a private
authority needs `GATEWAY_TURN_CA_FILE` on the gateway, pointing at that
authority's PEM certificate. A file that cannot be read is logged and ignored.

Verified on YYYY-MM-DD by `make check-gateway-relay`: with the gateway's UDP TURN
URL pointing at a closed port and the gateway limited to relay candidates, a live
session from a fake camera to a fake browser carried its video through the TLS
bridge to a local coturn and back.

Not verified: a real site behind TLS-only egress, and a relay on the public
internet reached this way.

````

- [ ] **Step 2: Point the local guide at the check.** In `docs/RUNNING-LOCALLY.md`, find the paragraph that begins ``` `make check-relay` runs the relay's own compose file ```, and after that paragraph insert a blank line and:

```markdown
`make check-gateway-relay` checks the gateway's side: it starts its own coturn
in Docker on high ports with a throwaway certificate authority, points the
gateway's UDP TURN URL at a closed port, and runs a live session that can only
reach the browser through the TLS bridge. It needs Docker, openssl, `ss`, and
ports 13478, 13479 and 15349 free.
```

- [ ] **Step 3: Backlog.** In `docs/BACKLOG.md`, replace the line

```markdown
- [ ] Gateway relays over TURN TCP/TLS — the webrtc-rs async relayer skips non-UDP TURN URLs (upstream webrtc-rs#848 covers TCP only)
```

with

```markdown
- [x] Gateway relays over TURN TCP/TLS — through a local bridge in the gateway (`make check-gateway-relay`); remove it once webrtc-rs supports `turns:` (webrtc-rs#848)
```

- [ ] **Step 4: Correct the private notes.** In the private notes file at the repository root (the one `publish.sh` excludes), replace the paragraph that begins `**Known not to work:** a camera site behind TLS-only egress still gets no relay.` and ends `real relay host with a real certificate yet.` with:

```markdown
**Known not to work, or not yet shown to:** nothing has run against a real relay
host with a real certificate, or at a real site behind TLS-only egress. Both
halves exist and are checked locally: coturn serves TURN over TLS on 443
(`make check-relay`), and the gateway bridges its TURN traffic to the relay over
TLS when UDP to it is blocked (`make check-gateway-relay`), because webrtc-rs
itself only speaks UDP TURN. The bridge in `edge/gateway/src/turn_bridge/` goes
away once webrtc-rs supports `turns:` (webrtc-rs#848; beads
relaysight-vms-qxr).
```

- [ ] **Step 5: Final run and diff review**

Run: `cargo test -p vms-gateway` → `110 passed; 0 failed; 2 ignored`.
Run: `make check-gateway-relay` → `Gateway relay check passed.`
Run: `git diff --stat master...HEAD` and `git status --short`.
Expected: the branch diff covers `Cargo.lock`, `Makefile`, `edge/gateway/Cargo.toml`, `edge/gateway/src/icepath.rs`, `edge/gateway/src/live.rs`, `edge/gateway/src/main.rs`, the six files under `edge/gateway/src/turn_bridge/`, `scripts/check-gateway-relay.sh`, and the spec and plan under `docs/superpowers/`; `git status --short` shows `Cargo.toml` plus `docs/TURN-DEPLOY.md`, `docs/RUNNING-LOCALLY.md`, `docs/BACKLOG.md` and the private notes file.

- [ ] **Step 6: Leak check and commit**

```bash
git add docs/TURN-DEPLOY.md docs/RUNNING-LOCALLY.md docs/BACKLOG.md
git add "$(sed -n '8s/^# \([^ ]*\).*/\1/p' publish.sh)"
bash -c 'source <(sed -n "28,30p" publish.sh); git diff --cached --name-only | grep -vxF "$(sed -n "8s/^# \([^ ]*\).*/\1/p" publish.sh)" | xargs -r grep -lniE "$LEAKS"'
git commit -m "Write down how the gateway reaches the relay over TLS, and what is verified"
```

The second `git add` stages the private notes file by the name on `publish.sh` line 8. The leak check excludes that file and must print nothing.
