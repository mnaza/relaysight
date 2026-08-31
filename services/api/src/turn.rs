//! Ephemeral TURN credentials.
//!
//! A TURN server relays media, which costs bandwidth, so its credentials are worth
//! stealing. Putting a fixed username and password in the ICE configuration hands
//! them to every visitor — the config is sent to the browser by design — and turns
//! the relay into an open proxy billed to us.
//!
//! Instead the credentials are derived: the username is an expiry timestamp and the
//! password is an HMAC of it under a secret only the API and the TURN server know.
//! coturn validates this with `use-auth-secret` and stores nothing. A leaked pair is
//! useless once it expires, and nothing has to be revoked.

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// How long a freshly minted credential stays valid. Long enough to open a session
/// and reconnect once, short enough that a leaked pair is not worth having.
pub const DEFAULT_TTL_SECS: u64 = 600;

/// A username/password pair for coturn in `use-auth-secret` mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCredential {
    pub username: String,
    pub credential: String,
}

/// Mint a credential valid until `now + ttl`.
///
/// `label` identifies who it was issued to. coturn ignores it; it is there so a
/// relay log can be tied back to a gateway without another lookup.
pub fn mint(secret: &str, label: &str, now_unix: u64, ttl_secs: u64) -> TurnCredential {
    let expiry = now_unix.saturating_add(ttl_secs);
    let username = if label.is_empty() {
        expiry.to_string()
    } else {
        format!("{expiry}:{label}")
    };
    let mut mac =
        Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(username.as_bytes());
    let credential = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    TurnCredential {
        username,
        credential,
    }
}

/// Expiry encoded in a username, if it parses. Test-only on purpose: coturn does its
/// own parsing and nothing here ever validates a credential it minted, so shipping
/// this would be a second implementation of a rule only one side owns.
#[cfg(test)]
pub fn expiry_of(username: &str) -> Option<u64> {
    username
        .split(':')
        .next()
        .and_then(|head| head.parse().ok())
}

/// Everything needed to hand a peer an ICE configuration.
///
/// STUN alone is enough for most networks. TURN is the fallback for the ones where
/// a direct path cannot be negotiated at all — symmetric NAT, or an egress firewall
/// that permits nothing but TLS out. Premises that install security cameras are
/// disproportionately of that kind, so leaving TURN unconfigured means those sites
/// do not degrade, they simply never connect.
#[derive(Debug, Clone, Default)]
pub struct RtcConfig {
    pub stun_urls: Vec<String>,
    pub turn_urls: Vec<String>,
    /// Shared with the TURN server and never sent to a peer. Empty disables TURN.
    turn_secret: String,
    pub ttl_secs: u64,
}

impl RtcConfig {
    pub fn from_env() -> Self {
        let list = |name: &str, fallback: &str| -> Vec<String> {
            std::env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| fallback.to_string())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        Self {
            stun_urls: list("RTC_STUN_URLS", "stun:stun.l.google.com:19302"),
            turn_urls: list("RTC_TURN_URLS", ""),
            turn_secret: std::env::var("RTC_TURN_SECRET").unwrap_or_default(),
            ttl_secs: std::env::var("RTC_TURN_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_TTL_SECS),
        }
    }

    /// True when a relay is actually available. Worth logging at startup: without
    /// it, sites behind symmetric NAT fail with no obvious cause.
    pub fn turn_enabled(&self) -> bool {
        !self.turn_urls.is_empty() && !self.turn_secret.is_empty()
    }

    /// ICE servers for one peer, with fresh TURN credentials if TURN is configured.
    pub fn ice_servers(&self, now_unix: u64, label: &str) -> Vec<vms_domain::RtcIceServerConfig> {
        let mut out = Vec::new();
        if !self.stun_urls.is_empty() {
            out.push(vms_domain::RtcIceServerConfig {
                urls: self.stun_urls.clone(),
                username: String::new(),
                credential: String::new(),
            });
        }
        if self.turn_enabled() {
            let c = mint(&self.turn_secret, label, now_unix, self.ttl_secs);
            out.push(vms_domain::RtcIceServerConfig {
                urls: self.turn_urls.clone(),
                username: c.username,
                credential: c.credential,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_username_carries_the_expiry_and_the_label() {
        let c = mint("s3cret", "gw-1", 1_000, 600);
        assert_eq!(c.username, "1600:gw-1");
        assert_eq!(expiry_of(&c.username), Some(1_600));
    }

    #[test]
    fn a_missing_label_still_produces_a_valid_username() {
        let c = mint("s3cret", "", 1_000, 600);
        assert_eq!(c.username, "1600");
        assert_eq!(expiry_of(&c.username), Some(1_600));
    }

    #[test]
    fn the_credential_is_the_hmac_coturn_expects() {
        // Pinned against a value computed independently, because getting this
        // wrong produces credentials that look fine and are rejected at 3am.
        // python: base64.b64encode(hmac.new(b"s3cret", b"1600:gw-1", hashlib.sha1).digest())
        let c = mint("s3cret", "gw-1", 1_000, 600);
        assert_eq!(c.credential, "+qiUt29/7hA+7wq2UP6St7/rOl0=");
    }

    #[test]
    fn the_secret_changes_the_credential() {
        let a = mint("one", "gw-1", 1_000, 600);
        let b = mint("two", "gw-1", 1_000, 600);
        assert_eq!(a.username, b.username);
        assert_ne!(a.credential, b.credential, "the secret is not being used");
    }

    #[test]
    fn each_second_produces_a_different_credential() {
        // Otherwise a captured pair stays usable for the whole TTL window of every
        // later session, not just its own.
        let a = mint("s3cret", "gw-1", 1_000, 600);
        let b = mint("s3cret", "gw-1", 1_001, 600);
        assert_ne!(a.username, b.username);
        assert_ne!(a.credential, b.credential);
    }

    fn cfg(turn: &[&str], secret: &str) -> RtcConfig {
        RtcConfig {
            stun_urls: vec!["stun:example.test:3478".into()],
            turn_urls: turn.iter().map(|s| s.to_string()).collect(),
            turn_secret: secret.into(),
            ttl_secs: 600,
        }
    }

    #[test]
    fn without_turn_configured_only_stun_is_offered() {
        // This is the state the project shipped in, and the failure it produces is
        // silent: sites behind symmetric NAT never connect and nothing says why.
        let c = cfg(&[], "");
        assert!(!c.turn_enabled());
        let servers = c.ice_servers(1_000, "gw-1");
        assert_eq!(servers.len(), 1);
        assert!(servers[0].username.is_empty());
    }

    #[test]
    fn turn_urls_without_a_secret_are_not_offered() {
        // Half-configured is worse than absent: the browser would try a relay it
        // cannot authenticate to and spend the ICE timeout doing it.
        let c = cfg(&["turn:relay.test:3478"], "");
        assert!(!c.turn_enabled());
        assert_eq!(c.ice_servers(1_000, "gw-1").len(), 1);
    }

    #[test]
    fn a_configured_relay_gets_fresh_credentials() {
        let c = cfg(&["turn:relay.test:3478", "turns:relay.test:5349"], "s3cret");
        assert!(c.turn_enabled());
        let servers = c.ice_servers(1_000, "gw-1");
        assert_eq!(servers.len(), 2);
        let relay = &servers[1];
        assert_eq!(relay.urls.len(), 2);
        assert_eq!(relay.username, "1600:gw-1");
        assert!(!relay.credential.is_empty());
    }

    #[test]
    fn the_secret_is_never_handed_to_a_peer() {
        // The whole point. Everything in this struct is serialised to the browser
        // except the secret, and a refactor that adds it to the response would be
        // invisible in review.
        let c = cfg(&["turn:relay.test:3478"], "s3cret");
        let servers = c.ice_servers(1_000, "gw-1");
        let json = serde_json::to_string(&servers).unwrap();
        assert!(
            !json.contains("s3cret"),
            "the shared secret reached the peer: {json}"
        );
    }

    #[test]
    fn two_peers_asking_at_the_same_second_still_get_their_own_label() {
        let c = cfg(&["turn:relay.test:3478"], "s3cret");
        let a = c.ice_servers(1_000, "gw-1");
        let b = c.ice_servers(1_000, "gw-2");
        assert_ne!(a[1].username, b[1].username);
        assert_ne!(a[1].credential, b[1].credential);
    }
}
