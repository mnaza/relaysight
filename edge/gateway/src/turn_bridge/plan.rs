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
    url.scheme == SchemeType::Turns
        || (url.scheme == SchemeType::Turn && url.proto == ProtoType::Tcp)
}

fn is_udp_turn_url(url: &Url) -> bool {
    url.scheme == SchemeType::Turn && url.proto == ProtoType::Udp
}

/// The relay to probe for `entry`: its first `turn:` URL over UDP, and only when
/// the entry also has a stream URL — otherwise there is nothing to decide.
pub(crate) fn probe_target(entry: &RtcIceServerConfig) -> Option<(String, u16)> {
    let parsed: Vec<Url> = entry
        .urls
        .iter()
        .filter_map(|raw| Url::parse_url(raw).ok())
        .collect();
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
            let reachable =
                probe_target(entry).is_some_and(|(host, port)| udp_reachable(&host, port));
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
        assert_eq!(
            probe_target(&entry(&[UDP, TCP, TLS])),
            Some(("relay.test".into(), 3478))
        );
        assert_eq!(
            probe_target(&entry(&[UDP])),
            None,
            "nothing to decide without a stream URL"
        );
        assert_eq!(
            probe_target(&entry(&[TLS])),
            None,
            "nothing to probe without a UDP URL"
        );
    }

    #[test]
    fn an_unreachable_relay_gets_its_stream_urls_bridged_and_its_udp_url_dropped() {
        let plans = decide(&[entry(&[STUN]), entry(&[UDP, TCP, TLS])], |_, _| false);
        assert_eq!(
            plans,
            vec![
                planned(vec![UrlPlan::Keep(STUN.into())]),
                planned(vec![
                    UrlPlan::Bridge(TCP.into()),
                    UrlPlan::Bridge(TLS.into())
                ]),
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
            vec![
                planned(vec![UrlPlan::Keep(STUN.into())]),
                planned(vec![UrlPlan::Keep(UDP.into())])
            ]
        );
    }

    #[test]
    fn an_entry_with_no_urls_is_dropped() {
        assert_eq!(decide(&[entry(&[])], |_, _| false), vec![]);
    }
}
