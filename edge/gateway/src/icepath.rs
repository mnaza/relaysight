//! Which path a live session actually took.
//!
//! The architecture avoids relaying media, so the cloud never pays for it. That
//! holds until a site cannot negotiate a direct path, and then every byte goes
//! through TURN and the cost comes back. Premises that install cameras skew toward
//! strict egress rules, so the relayed share is not a rounding error to be assumed.
//!
//! It is also the number the hosting model depends on and it cannot be guessed, so
//! each session reports the path it settled on. See `docs/TURN-COSTS.md`.

use std::fmt;

/// How the media reached the far end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Same network. Costs nothing.
    Host,
    /// Direct, through NAT, discovered with STUN. Also costs nothing.
    ServerReflexive,
    /// Discovered during connectivity checks. Still direct.
    PeerReflexive,
    /// Through TURN. This is the one that costs bandwidth.
    Relay,
    /// Connected, but the candidate type was not reported.
    Unknown,
}

impl PathKind {
    /// Whether this path sends media through our relay.
    pub fn is_relayed(self) -> bool {
        matches!(self, PathKind::Relay)
    }
}

impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            PathKind::Host => "host",
            PathKind::ServerReflexive => "srflx",
            PathKind::PeerReflexive => "prflx",
            PathKind::Relay => "relay",
            PathKind::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// Pick the pair that actually carried the media.
///
/// ICE leaves several pairs in the report and there is no "selected" flag here, so
/// the one with the most traffic is the one that won. Ties and empties return None
/// rather than guessing, because a wrong path label is worse than a missing one: it
/// would go straight into a cost model.
pub fn busiest_pair<'a>(pairs: impl Iterator<Item = (&'a str, u64)>) -> Option<&'a str> {
    let mut best: Option<(&str, u64)> = None;
    for (id, bytes) in pairs {
        if bytes == 0 {
            continue;
        }
        match best {
            Some((_, b)) if bytes <= b => {}
            _ => best = Some((id, bytes)),
        }
    }
    best.map(|(id, _)| id)
}

/// Whether a report entry describes the candidate a pair refers to.
///
/// A pair carries the bare candidate id (`candidate:IB9F...`) while the entry is
/// keyed with a type prefix (`RTCLocalIceCandidate_candidate:IB9F...`). Comparing
/// them directly never matches, which is how this silently returned Unknown for
/// every session until a loopback test asserted otherwise.
pub fn entry_is_candidate(entry_id: &str, candidate_id: &str) -> bool {
    entry_id == candidate_id
        || entry_id
            .strip_suffix(candidate_id)
            .is_some_and(|prefix| prefix.ends_with('_'))
}

/// Read the path a connected peer settled on.
///
/// Thin glue over `busiest_pair`, which carries the logic and the tests. Anything
/// missing maps to `Unknown` rather than to a guess: this number goes into a cost
/// model, and a wrong label there is worse than an absent one.
pub async fn observed(
    peer: &std::sync::Arc<dyn webrtc::peer_connection::PeerConnection>,
) -> PathKind {
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
        return PathKind::Unknown;
    };

    let found = report.iter().find_map(|e| match e {
        RTCStatsReportEntry::LocalCandidate(c) if entry_is_candidate(&c.stats.id, local_id) => {
            Some(c)
        }
        _ => None,
    });

    match found {
        Some(c) => match c.candidate_type {
            RTCIceCandidateType::Host => PathKind::Host,
            RTCIceCandidateType::Srflx => PathKind::ServerReflexive,
            RTCIceCandidateType::Prflx => PathKind::PeerReflexive,
            RTCIceCandidateType::Relay => PathKind::Relay,
            _ => PathKind::Unknown,
        },
        None => PathKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pair_carrying_the_media_is_the_one_with_the_most_bytes() {
        let pairs = [("a", 10u64), ("b", 900), ("c", 3)];
        assert_eq!(busiest_pair(pairs.into_iter()), Some("b"));
    }

    #[test]
    fn pairs_that_carried_nothing_are_not_candidates() {
        // ICE keeps failed and unused pairs in the report. Counting one of those as
        // the path would put a wrong number into a cost model.
        let pairs = [("a", 0u64), ("b", 0), ("c", 1)];
        assert_eq!(busiest_pair(pairs.into_iter()), Some("c"));
    }

    #[test]
    fn a_report_with_no_traffic_yields_nothing_rather_than_a_guess() {
        let pairs = [("a", 0u64), ("b", 0)];
        assert_eq!(busiest_pair(pairs.into_iter()), None);
        assert_eq!(busiest_pair(std::iter::empty()), None);
    }

    #[test]
    fn only_the_relay_path_is_counted_as_costing_bandwidth() {
        assert!(PathKind::Relay.is_relayed());
        for p in [
            PathKind::Host,
            PathKind::ServerReflexive,
            PathKind::PeerReflexive,
            PathKind::Unknown,
        ] {
            assert!(!p.is_relayed(), "{p} must not be counted as relayed");
        }
    }

    #[test]
    fn unknown_is_not_counted_as_relayed() {
        // Deliberate. An unreported candidate type must not inflate the relay share,
        // because that share decides the hosting model. Under-counting is visible
        // when the bandwidth bill disagrees; over-counting quietly buys capacity
        // nobody needs.
        assert!(!PathKind::Unknown.is_relayed());
        assert_eq!(PathKind::Unknown.to_string(), "unknown");
    }

    #[test]
    fn a_prefixed_entry_id_still_matches_the_bare_candidate_id() {
        // The real shapes, taken from a live report.
        assert!(entry_is_candidate(
            "RTCLocalIceCandidate_candidate:IB9FsQx",
            "candidate:IB9FsQx"
        ));
        assert!(entry_is_candidate("candidate:IB9FsQx", "candidate:IB9FsQx"));
    }

    #[test]
    fn a_different_candidate_does_not_match() {
        assert!(!entry_is_candidate(
            "RTCLocalIceCandidate_candidate:AAAA",
            "candidate:BBBB"
        ));
        // Suffix alone is not enough. Without the separator check, a candidate id
        // that happens to end another one would match and label the wrong path.
        assert!(!entry_is_candidate(
            "RTCLocalIceCandidateXcandidate:AAAA",
            "candidate:AAAA"
        ));
        assert!(!entry_is_candidate("Zcandidate:AAAA", "candidate:AAAA"));
    }
}
