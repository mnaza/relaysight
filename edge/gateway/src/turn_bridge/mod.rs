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

mod bridge;
mod framing;
mod plan;
mod probe;
mod tls;

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
        self.bridges
            .iter()
            .map(|(bridge, _)| bridge.stats())
            .collect()
    }

    /// The transport behind a relay candidate's TURN URL: a bridge's own when the
    /// URL points at one of this session's bridges, UDP for any other `turn:` URL.
    pub(crate) fn transport_for_url(&self, url: &str) -> Option<RelayTransport> {
        let parsed = Url::parse_url(url).ok()?;
        let bridged = self.bridges.iter().find(|(bridge, _)| {
            bridge.local_addr().ip().to_string() == parsed.host
                && bridge.local_addr().port() == parsed.port
        });
        match bridged {
            Some((_, transport)) => Some(*transport),
            None => (parsed.scheme == SchemeType::Turn).then_some(RelayTransport::Udp),
        }
    }
}

/// The ICE servers for one live session, with any bridges they need running.
pub(crate) async fn plan(
    servers: Vec<RtcIceServerConfig>,
) -> (Vec<RtcIceServerConfig>, BridgeGuard) {
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
        verdicts
            .get(&(host.to_owned(), port))
            .copied()
            .unwrap_or(false)
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
                    urls.push(format!(
                        "turn:{}:{}?transport=udp",
                        local.ip(),
                        local.port()
                    ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, UdpSocket};

    #[tokio::test]
    async fn a_relay_silent_over_udp_is_bridged_and_the_bridge_is_recognised() {
        let silent_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_port = silent_udp.local_addr().unwrap().port();
        let tls_port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
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
        assert_eq!(
            planned[1].urls.len(),
            1,
            "UDP dropped, TLS replaced: {:?}",
            planned[1].urls
        );
        let bridged = planned[1].urls[0].clone();
        assert!(
            bridged.starts_with("turn:127.0.0.1:") && bridged.ends_with("?transport=udp"),
            "{bridged}"
        );
        assert_eq!(
            (planned[1].username.as_str(), planned[1].credential.as_str()),
            ("u", "c")
        );
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
        assert_eq!(
            planned[0].urls,
            vec!["turn:127.0.0.1:9?transport=udp".to_string()]
        );
        assert!(guard.stats().is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "nothing to decide must not wait for a probe"
        );
    }
}
