//! Why the scheduler paused a schedule by itself.
//!
//! An automatic pause goes through the same signed pause write an operator's
//! `scheduler.pause` uses. The reason is recorded in the signed schedule
//! source beside `enabled: false`, so `scheduler.explain` reports why the
//! schedule stopped instead of leaving it to be inferred.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAutoPauseReason {
    /// The schedule reached its signed `max_fires` bound.
    MaxFiresReached,
    /// The schedule reached its signed `expires_at` bound.
    Expired,
    /// A fire was refused because the pinned grant was revoked.
    AuthorityRevoked,
    /// A fire was refused because the pinned grant has lapsed.
    AuthorityExpired,
    /// A fire was refused because the pinned grant generation, class or
    /// origin changed.
    AuthorityChanged,
    /// A fire was refused because the pinned grant no longer covers the
    /// schedule's capabilities.
    AuthorityNarrowed,
}

impl ScheduleAutoPauseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxFiresReached => "max_fires_reached",
            Self::Expired => "expired",
            Self::AuthorityRevoked => "authority_revoked",
            Self::AuthorityExpired => "authority_expired",
            Self::AuthorityChanged => "authority_changed",
            Self::AuthorityNarrowed => "authority_narrowed",
        }
    }
}

impl std::fmt::Display for ScheduleAutoPauseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_match_display() {
        for reason in [
            ScheduleAutoPauseReason::MaxFiresReached,
            ScheduleAutoPauseReason::Expired,
            ScheduleAutoPauseReason::AuthorityRevoked,
            ScheduleAutoPauseReason::AuthorityExpired,
            ScheduleAutoPauseReason::AuthorityChanged,
            ScheduleAutoPauseReason::AuthorityNarrowed,
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::json!(reason.as_str())
            );
        }
    }
}
