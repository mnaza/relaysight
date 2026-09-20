//! Whether the relay answers over UDP from here.
//!
//! A STUN Binding request, resent a couple of times, and an answer or none.
//! Whether UDP gets out is a property of the site's firewall, not of a session,
//! so verdicts are cached.

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
/// Gaps between the request and each resend, all inside `PROBE_TIMEOUT`. One
/// lost datagram would otherwise hold a site on the bridge for `VERDICT_TTL`.
const RETRANSMIT_GAPS: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(250)];

/// True when `server` answers a STUN Binding request within `timeout`. The
/// request is resent at `RETRANSMIT_GAPS`; every send carries the same
/// transaction id, as RFC 5389 requires, so an answer to any of them counts.
pub(crate) async fn stun_binding_answers(server: SocketAddr, timeout: Duration) -> bool {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        return false;
    };
    let mut request = Message::new();
    let built = {
        let setters: [Box<dyn Setter>; 2] =
            [Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)];
        request.build(&setters)
    };
    if built.is_err() || socket.send_to(&request.raw, server).await.is_err() {
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
    // Resends run alongside the wait and never finish on their own; the answer
    // or the timeout ends the probe.
    let resend = async {
        for gap in RETRANSMIT_GAPS {
            tokio::time::sleep(gap).await;
            if socket.send_to(&request.raw, server).await.is_err() {
                break;
            }
        }
        std::future::pending::<bool>().await
    };
    let probe = async {
        tokio::select! {
            answered = answered => answered,
            never = resend => never,
        }
    };
    tokio::time::timeout(timeout, probe).await.unwrap_or(false)
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
        let cached = self
            .verdicts
            .lock()
            .expect("probe cache poisoned")
            .get(&key)
            .copied();
        if let Some((at, verdict)) = cached
            && at.elapsed() < self.ttl
        {
            return verdict;
        }
        let verdict = match resolve_ipv4(host, port, timeout).await {
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

/// A resolver that never answers must not hold a session either: the lookup
/// gets the same budget the probe does, and a name that does not resolve in
/// that time counts as unreachable.
async fn resolve_ipv4(host: &str, port: u16, timeout: Duration) -> Option<SocketAddr> {
    let lookup = async {
        tokio::net::lookup_host((host, port))
            .await
            .ok()?
            .find(SocketAddr::is_ipv4)
    };
    tokio::time::timeout(timeout, lookup).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::stun::message::BINDING_SUCCESS;

    /// A loopback UDP socket that answers every Binding request with success.
    async fn stun_responder() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        stun_responder_dropping(0).await
    }

    /// The same, after throwing away the first `dropped` requests — a relay
    /// whose answer, or whose request, the network lost.
    async fn stun_responder_dropping(dropped: usize) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            let mut seen = 0usize;
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                seen += 1;
                if seen <= dropped {
                    continue;
                }
                let mut request = Message::new();
                request.raw = buf[..n].to_vec();
                if request.decode().is_err() {
                    continue;
                }
                let mut response = Message::new();
                {
                    let setters: [Box<dyn Setter>; 2] =
                        [Box::new(request.transaction_id), Box::new(BINDING_SUCCESS)];
                    response.build(&setters).unwrap();
                }
                let _ = socket.send_to(&response.raw, from).await;
            }
        });
        (addr, task)
    }

    #[test]
    fn the_probe_futures_can_run_in_spawned_tasks() {
        // live sessions reach the probe through tokio::spawn, which needs Send futures.
        fn assert_send<T: Send>(_: &T) {}
        let probe = stun_binding_answers("127.0.0.1:9".parse().unwrap(), Duration::ZERO);
        assert_send(&probe);
        let cache = ProbeCache::new(Duration::ZERO);
        let cached = cache.reachable("127.0.0.1", 9, Duration::ZERO);
        assert_send(&cached);
    }

    #[tokio::test]
    async fn a_relay_that_answers_is_reachable() {
        let (addr, _task) = stun_responder().await;
        assert!(stun_binding_answers(addr, Duration::from_millis(500)).await);
    }

    #[tokio::test]
    async fn a_lost_request_is_retransmitted_inside_the_same_budget() {
        // One lost datagram must not cost the site ten minutes on the bridge.
        let (addr, _task) = stun_responder_dropping(2).await;
        let started = Instant::now();
        assert!(
            stun_binding_answers(addr, PROBE_TIMEOUT).await,
            "the probe must resend rather than believe the first silence"
        );
        let waited = started.elapsed();
        assert!(
            waited >= RETRANSMIT_GAPS[0],
            "an answer this early cannot have come from a retransmission: {waited:?}"
        );
        assert!(
            waited < PROBE_TIMEOUT,
            "the resends must fit the budget: {waited:?}"
        );
    }

    #[tokio::test]
    async fn a_silent_relay_is_unreachable_once_the_timeout_passes() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = Instant::now();
        assert!(
            !stun_binding_answers(silent.local_addr().unwrap(), Duration::from_millis(300)).await
        );
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "the probe must give up at its timeout"
        );
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_unreachable() {
        // .invalid never resolves (RFC 2606); a resolver that hijacks it still
        // answers no STUN, so the verdict is the same either way.
        let cache = ProbeCache::new(VERDICT_TTL);
        let started = Instant::now();
        assert!(
            !cache
                .reachable("relay.invalid", 3478, Duration::from_millis(300))
                .await
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a name that does not resolve must not hold the session"
        );
    }

    #[tokio::test]
    async fn a_verdict_is_reused_while_it_is_fresh() {
        let (addr, task) = stun_responder().await;
        let cache = ProbeCache::new(Duration::from_secs(60));
        assert!(
            cache
                .reachable("127.0.0.1", addr.port(), Duration::from_millis(500))
                .await
        );
        task.abort();
        let _ = task.await;
        assert!(
            cache
                .reachable("127.0.0.1", addr.port(), Duration::from_millis(300))
                .await,
            "a fresh verdict must not probe again"
        );
    }

    #[tokio::test]
    async fn an_expired_verdict_probes_again() {
        let (addr, task) = stun_responder().await;
        let cache = ProbeCache::new(Duration::ZERO);
        assert!(
            cache
                .reachable("127.0.0.1", addr.port(), Duration::from_millis(500))
                .await
        );
        task.abort();
        let _ = task.await;
        assert!(
            !cache
                .reachable("127.0.0.1", addr.port(), Duration::from_millis(300))
                .await,
            "an expired verdict must probe, and the responder is gone"
        );
    }
}
