//! How much of the real usage a reported cost figure accounts for.
//!
//! This type lives here, below both the engine's cost envelope and the
//! authoritative thread snapshot, so the one definition is the one a stored
//! thread usage carries: a resumed run's earlier `Partial` or `Unavailable`
//! turns reach the next settlement as the same status they were reported with.

use serde::{Deserialize, Serialize};

/// How much of the real usage a reported cost figure accounts for.
///
/// `Complete` is the default and is never serialized, so a complete cost is
/// byte-identical to one written before this field existed. A figure that is
/// `Partial` omits usage some member of it could not report; `Unavailable`
/// carries no real usage at all (its token and spend values are zero
/// placeholders, not measurements).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    #[default]
    Complete,
    Partial,
    Unavailable,
}

/// A usage status spelling that is not `complete`, `partial` or `unavailable`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cost usage_status `{0}` is invalid; expected `complete`, `partial` or `unavailable`")]
pub struct InvalidUsageStatus(pub String);

impl UsageStatus {
    /// Serde `skip_serializing_if` predicate: only non-complete figures name
    /// their status on the wire.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
        }
    }

    /// Parse the snapshot-facet spelling. Absent means `Complete`.
    pub fn from_facet(value: Option<&str>) -> Result<Self, InvalidUsageStatus> {
        match value {
            None | Some("complete") => Ok(Self::Complete),
            Some("partial") => Ok(Self::Partial),
            Some("unavailable") => Ok(Self::Unavailable),
            Some(other) => Err(InvalidUsageStatus(other.to_string())),
        }
    }

    /// Status of an aggregate of two members: complete only when both are,
    /// unavailable only when both are, otherwise partial.
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Complete, Self::Complete) => Self::Complete,
            (Self::Unavailable, Self::Unavailable) => Self::Unavailable,
            _ => Self::Partial,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_facet_spelling_round_trips_and_absent_means_complete() {
        for status in [
            UsageStatus::Complete,
            UsageStatus::Partial,
            UsageStatus::Unavailable,
        ] {
            assert_eq!(
                UsageStatus::from_facet(Some(status.as_str())).unwrap(),
                status
            );
        }
        assert_eq!(
            UsageStatus::from_facet(None).unwrap(),
            UsageStatus::Complete
        );
        assert!(UsageStatus::from_facet(Some("bogus")).is_err());
    }

    #[test]
    fn a_status_never_improves_when_folded_with_another() {
        use UsageStatus::{Complete, Partial, Unavailable};
        assert_eq!(Partial.combine(Complete), Partial);
        assert_eq!(Complete.combine(Unavailable), Partial);
        assert_eq!(Unavailable.combine(Unavailable), Unavailable);
    }
}
