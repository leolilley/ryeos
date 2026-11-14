//! Typed refusal of a fire whose retained schedule authority no longer holds.
//!
//! A schedule pins the exact grant generations admitted at registration and
//! every fire proves they still hold. When that proof fails the refusal is a
//! typed value carried through the dispatch error, so the fire records a
//! distinct outcome and the scheduler pauses the schedule, instead of the
//! cause surviving only as a log line.

use serde::{Deserialize, Serialize};

use crate::ScheduleAutoPauseReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledAuthorityRefusalKind {
    /// The principal's authorized-key grant no longer exists, or carries a
    /// revocation tombstone.
    GrantRevoked,
    /// The principal's authorized-key grant has lapsed.
    GrantExpired,
    /// The grant exists but is not the generation pinned at registration.
    GrantGenerationChanged,
    /// The pinned grant's principal class or origin site no longer matches.
    ClassOrOriginChanged,
    /// The pinned grant no longer covers a capability the schedule declares.
    CapabilityNoLongerCovered,
    /// The forwarding node's grant no longer exists.
    ForwardingGrantRevoked,
    /// The forwarding node's grant is not the generation pinned at
    /// registration, or its class or origin changed.
    ForwardingGrantChanged,
    /// The forwarding node's grant no longer covers forwarded-operator
    /// attestation.
    ForwardingAttestationNotCovered,
    /// A node-owned schedule no longer names this node's identity or site.
    NodeAuthorityChanged,
}

impl ScheduledAuthorityRefusalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GrantRevoked => "grant_revoked",
            Self::GrantExpired => "grant_expired",
            Self::GrantGenerationChanged => "grant_generation_changed",
            Self::ClassOrOriginChanged => "class_or_origin_changed",
            Self::CapabilityNoLongerCovered => "capability_no_longer_covered",
            Self::ForwardingGrantRevoked => "forwarding_grant_revoked",
            Self::ForwardingGrantChanged => "forwarding_grant_changed",
            Self::ForwardingAttestationNotCovered => "forwarding_attestation_not_covered",
            Self::NodeAuthorityChanged => "node_authority_changed",
        }
    }

    /// The pause reason, which is also the refused fire's outcome code.
    pub fn pause_reason(self) -> ScheduleAutoPauseReason {
        match self {
            Self::GrantRevoked | Self::ForwardingGrantRevoked => {
                ScheduleAutoPauseReason::AuthorityRevoked
            }
            Self::GrantExpired => ScheduleAutoPauseReason::AuthorityExpired,
            Self::GrantGenerationChanged
            | Self::ClassOrOriginChanged
            | Self::ForwardingGrantChanged
            | Self::NodeAuthorityChanged => ScheduleAutoPauseReason::AuthorityChanged,
            Self::CapabilityNoLongerCovered | Self::ForwardingAttestationNotCovered => {
                ScheduleAutoPauseReason::AuthorityNarrowed
            }
        }
    }

    /// The outcome recorded on the refused fire: `authority_revoked`,
    /// `authority_expired`, `authority_changed` or `authority_narrowed`.
    pub fn fire_outcome(self) -> &'static str {
        self.pause_reason().as_str()
    }
}

impl std::fmt::Display for ScheduledAuthorityRefusalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A fire refused because its retained schedule authority no longer holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledAuthorityRefusal {
    pub kind: ScheduledAuthorityRefusalKind,
    pub detail: String,
}

impl ScheduledAuthorityRefusal {
    pub fn new(kind: ScheduledAuthorityRefusalKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// The typed refusal anywhere in an error's chain, if the error is one.
    pub fn find(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

impl std::fmt::Display for ScheduledAuthorityRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "scheduled authority refused ({}): {}",
            self.kind, self.detail
        )
    }
}

impl std::error::Error for ScheduledAuthorityRefusal {}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn refusal_is_found_through_added_context() {
        let error = Err::<(), _>(anyhow::Error::new(ScheduledAuthorityRefusal::new(
            ScheduledAuthorityRefusalKind::GrantRevoked,
            "grant file is absent",
        )))
        .context("bind scheduled project authority")
        .context("admit scheduler fire")
        .unwrap_err();
        let refusal = ScheduledAuthorityRefusal::find(&error).expect("typed refusal");
        assert_eq!(refusal.kind, ScheduledAuthorityRefusalKind::GrantRevoked);
        assert!(ScheduledAuthorityRefusal::find(&anyhow::anyhow!("io")).is_none());
    }

    #[test]
    fn every_kind_maps_to_a_typed_fire_outcome() {
        use ScheduledAuthorityRefusalKind::*;
        for (kind, outcome) in [
            (GrantRevoked, "authority_revoked"),
            (GrantExpired, "authority_expired"),
            (ForwardingGrantRevoked, "authority_revoked"),
            (GrantGenerationChanged, "authority_changed"),
            (ClassOrOriginChanged, "authority_changed"),
            (ForwardingGrantChanged, "authority_changed"),
            (NodeAuthorityChanged, "authority_changed"),
            (CapabilityNoLongerCovered, "authority_narrowed"),
            (ForwardingAttestationNotCovered, "authority_narrowed"),
        ] {
            assert_eq!(kind.fire_outcome(), outcome, "{kind}");
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.as_str())
            );
        }
    }
}
