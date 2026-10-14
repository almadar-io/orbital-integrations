//! Peer matching, token lifetimes and moderation scoring that need no I/O (`socialLogic.ts`).

use sha1::{Digest, Sha1};

use crate::json::number;
use crate::knowledge::store::{MessageModeration, NodeMember};

pub const PEER_WINDOW_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
pub const ACTIVE_NOW_MS: f64 = 5.0 * 60.0 * 1000.0;
pub const PEER_TOKEN_TTL_MS: f64 = 10.0 * 60.0 * 1000.0;
pub const DORMANT_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
pub const FLAG_THRESHOLD: f64 = 0.4;

/// `concept:<id>` or `path:<id>`.
pub fn is_node_key(text: &str) -> bool {
    ["concept:", "path:"].iter().any(|prefix| {
        text.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('\n') && !rest.contains('\r'))
    })
}

/// Deterministic, non-reversible handle for a learner; stable across nodes.
pub fn anonymous_handle_for(uid: &str) -> String {
    let digest = Sha1::digest(uid.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("peer-{}", &hex[..6])
}

pub fn is_active_now(last_seen_on_node: f64, now: f64) -> bool {
    now - last_seen_on_node <= ACTIVE_NOW_MS
}

pub fn is_dormant(last_activity_at: f64, now: f64) -> bool {
    now - last_activity_at >= DORMANT_MS
}

pub fn is_expired(expires_at: f64, now: f64) -> bool {
    expires_at < now
}

pub struct PeerCandidate {
    pub peer_ref: String,
    pub member: NodeMember,
    pub canonical_names: Vec<String>,
    pub concepts_completed: usize,
}

pub struct RankedPeer {
    pub peer_ref: String,
    pub handle: String,
    pub concepts_completed: usize,
    pub convergence_pct: f64,
    pub active_now: bool,
}

/// Peers ranked by convergence with the caller, then active-now.
pub fn rank_peers(
    caller_names: &[String],
    candidates: Vec<PeerCandidate>,
    now: f64,
) -> Vec<RankedPeer> {
    let mut peers: Vec<RankedPeer> = candidates
        .into_iter()
        .map(|c| RankedPeer {
            handle: anonymous_handle_for(&c.member.uid),
            peer_ref: c.peer_ref,
            concepts_completed: c.concepts_completed,
            convergence_pct: (super::queries::convergence_score(caller_names, &c.canonical_names)
                * 100.0)
                .round(),
            active_now: is_active_now(c.member.last_seen_on_node, now),
        })
        .collect();
    peers.sort_by(|a, b| {
        b.convergence_pct
            .partial_cmp(&a.convergence_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| u8::from(b.active_now).cmp(&u8::from(a.active_now)))
    });
    peers
}

pub fn moderation_from_score(score: f64, reason: Option<String>) -> MessageModeration {
    let clamped = score.clamp(0.0, 1.0);
    MessageModeration {
        score: json_number(clamped),
        flagged: clamped < FLAG_THRESHOLD,
        reason,
    }
}

fn json_number(n: f64) -> f64 {
    number(n).as_f64().unwrap_or(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_is_stable_and_hides_the_uid() {
        assert_eq!(anonymous_handle_for("learner-2"), "peer-fff82e");
        assert_eq!(
            anonymous_handle_for("learner-2"),
            anonymous_handle_for("learner-2")
        );
        assert_ne!(
            anonymous_handle_for("learner-3"),
            anonymous_handle_for("learner-2")
        );
    }

    #[test]
    fn node_keys_need_a_known_kind_and_an_id() {
        assert!(is_node_key("concept:light"));
        assert!(is_node_key("path:abc"));
        assert!(!is_node_key("light"));
        assert!(!is_node_key("concept:"));
        assert!(!is_node_key("topic:light"));
    }

    #[test]
    fn a_score_is_clamped_and_flagged_below_the_threshold() {
        assert!(moderation_from_score(0.1, None).flagged);
        assert!(!moderation_from_score(0.4, None).flagged);
        assert_eq!(moderation_from_score(7.0, None).score, 1.0);
        assert_eq!(
            moderation_from_score(-1.0, Some("spam".into()))
                .reason
                .as_deref(),
            Some("spam")
        );
    }

    #[test]
    fn peers_rank_by_convergence_then_activity() {
        let member = |uid: &str, seen: f64| NodeMember {
            uid: uid.into(),
            last_seen_on_node: seen,
        };
        let names = vec!["a".to_string(), "b".to_string()];
        let candidates = vec![
            PeerCandidate {
                peer_ref: "r1".into(),
                member: member("u1", 0.0),
                canonical_names: vec!["z".into()],
                concepts_completed: 1,
            },
            PeerCandidate {
                peer_ref: "r2".into(),
                member: member("u2", 1_000_000.0),
                canonical_names: vec!["a".into(), "b".into()],
                concepts_completed: 2,
            },
            PeerCandidate {
                peer_ref: "r3".into(),
                member: member("u3", 1_000_000.0),
                canonical_names: vec!["q".into()],
                concepts_completed: 1,
            },
        ];
        let ranked = rank_peers(&names, candidates, 1_000_000.0);
        assert_eq!(
            ranked
                .iter()
                .map(|p| p.peer_ref.as_str())
                .collect::<Vec<_>>(),
            ["r2", "r3", "r1"]
        );
        assert_eq!(ranked[0].convergence_pct, 100.0);
    }
}
