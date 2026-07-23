//! Typed reservation state machine shared by the daemon ledger, the
//! directive Runner, and the audit projection.
//!
//! Storage or RPC failures are never modeled as business states: if an
//! acknowledgement is lost, the recorded state remains whatever the daemon
//! committed and the client recovers by exact idempotent retry or read.

use serde::{Deserialize, Serialize};

/// Budget-effect state of one provider attempt reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptBudgetState {
    ReservationDenied,
    Reserved,
    Issued,
    Reconciled,
    ReleasedUnissued,
    ChargedReservedMaximum,
    ReservationBoundViolated,
}

impl AttemptBudgetState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReservationDenied => "reservation_denied",
            Self::Reserved => "reserved",
            Self::Issued => "issued",
            Self::Reconciled => "reconciled",
            Self::ReleasedUnissued => "released_unissued",
            Self::ChargedReservedMaximum => "charged_reserved_maximum",
            Self::ReservationBoundViolated => "reservation_bound_violated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "reservation_denied" => Self::ReservationDenied,
            "reserved" => Self::Reserved,
            "issued" => Self::Issued,
            "reconciled" => Self::Reconciled,
            "released_unissued" => Self::ReleasedUnissued,
            "charged_reserved_maximum" => Self::ChargedReservedMaximum,
            "reservation_bound_violated" => Self::ReservationBoundViolated,
            _ => return None,
        })
    }

    /// Terminal for budget effect. A late authoritative actual attached to
    /// `ChargedReservedMaximum` is a monotonic observation, not a state
    /// transition out of terminality.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Reserved | Self::Issued)
    }

    /// The complete legal transition relation. No transition returns to
    /// `Reserved`; no terminal state reopens.
    pub const fn may_transition_to(self, next: AttemptBudgetState) -> bool {
        matches!(
            (self, next),
            (Self::Reserved, Self::Issued)
                | (Self::Reserved, Self::ReleasedUnissued)
                | (Self::Issued, Self::Reconciled)
                | (Self::Issued, Self::ChargedReservedMaximum)
                | (Self::Issued, Self::ReservationBoundViolated)
        )
    }
}

/// What the committed budget charge is based on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ChargeBasis {
    /// The provider's trustworthy reported final charge.
    ProviderReported,
    /// Deterministic signed-tariff cost derived from authoritative usage.
    DeterministicTariff,
    /// The full reserved maximum, conservatively retained.
    ReservedMaximum,
    /// An explicitly-free route settling exact zero under its contract.
    ExplicitlyFree,
}

impl ChargeBasis {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderReported => "provider_reported",
            Self::DeterministicTariff => "deterministic_tariff",
            Self::ReservedMaximum => "reserved_maximum",
            Self::ExplicitlyFree => "explicitly_free",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "provider_reported" => Self::ProviderReported,
            "deterministic_tariff" => Self::DeterministicTariff,
            "reserved_maximum" => Self::ReservedMaximum,
            "explicitly_free" => Self::ExplicitlyFree,
            _ => return None,
        })
    }
}

/// Closed reasons recorded with a terminal reservation transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReconciliationReason {
    /// Actual charge settled from trustworthy provider accounting.
    ProviderReportedFinal,
    /// Actual charge settled from the deterministic signed tariff.
    DeterministicTariff,
    /// Explicitly-free contract settled exact zero.
    ExplicitlyFreeContract,
    /// Final trustworthy accounting was unavailable or malformed after issue.
    AccountingUnavailable,
    /// The attempt was durably issued but external acceptance is ambiguous
    /// (crash, transport cut, cancellation after the issue boundary).
    AmbiguousIssue,
    /// A time-bounded certificate expired between reserve and issue.
    AuthorityExpiredBeforeIssue,
    /// The frozen credential generation was revoked or unavailable at issue.
    CredentialUnavailableBeforeIssue,
    /// The runner released the reservation before issue (cancel, shutdown).
    ReleasedByRunner,
    /// Supervisor or startup recovery fenced the owning launch generation.
    OwnerGenerationFenced,
    /// Reported actual charge exceeded the proven maximum.
    BoundViolation,
    /// Reservation was denied for insufficient available balance.
    InsufficientBudget,
}

impl ReconciliationReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderReportedFinal => "provider_reported_final",
            Self::DeterministicTariff => "deterministic_tariff",
            Self::ExplicitlyFreeContract => "explicitly_free_contract",
            Self::AccountingUnavailable => "accounting_unavailable",
            Self::AmbiguousIssue => "ambiguous_issue",
            Self::AuthorityExpiredBeforeIssue => "authority_expired_before_issue",
            Self::CredentialUnavailableBeforeIssue => "credential_unavailable_before_issue",
            Self::ReleasedByRunner => "released_by_runner",
            Self::OwnerGenerationFenced => "owner_generation_fenced",
            Self::BoundViolation => "bound_violation",
            Self::InsufficientBudget => "insufficient_budget",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "provider_reported_final" => Self::ProviderReportedFinal,
            "deterministic_tariff" => Self::DeterministicTariff,
            "explicitly_free_contract" => Self::ExplicitlyFreeContract,
            "accounting_unavailable" => Self::AccountingUnavailable,
            "ambiguous_issue" => Self::AmbiguousIssue,
            "authority_expired_before_issue" => Self::AuthorityExpiredBeforeIssue,
            "credential_unavailable_before_issue" => Self::CredentialUnavailableBeforeIssue,
            "released_by_runner" => Self::ReleasedByRunner,
            "owner_generation_fenced" => Self::OwnerGenerationFenced,
            "bound_violation" => Self::BoundViolation,
            "insufficient_budget" => Self::InsufficientBudget,
            _ => return None,
        })
    }
}

/// Health of one budget account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountHealth {
    Healthy,
    Violated,
}

impl AccountHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Violated => "violated",
        }
    }
}

/// Health of one provider accounting authority digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorityHealth {
    Healthy,
    Quarantined,
    Violated,
}

impl AuthorityHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Quarantined => "quarantined",
            Self::Violated => "violated",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [AttemptBudgetState; 7] = [
        AttemptBudgetState::ReservationDenied,
        AttemptBudgetState::Reserved,
        AttemptBudgetState::Issued,
        AttemptBudgetState::Reconciled,
        AttemptBudgetState::ReleasedUnissued,
        AttemptBudgetState::ChargedReservedMaximum,
        AttemptBudgetState::ReservationBoundViolated,
    ];

    #[test]
    fn every_legal_transition() {
        use AttemptBudgetState::*;
        assert!(Reserved.may_transition_to(Issued));
        assert!(Reserved.may_transition_to(ReleasedUnissued));
        assert!(Issued.may_transition_to(Reconciled));
        assert!(Issued.may_transition_to(ChargedReservedMaximum));
        assert!(Issued.may_transition_to(ReservationBoundViolated));
    }

    #[test]
    fn every_illegal_regression() {
        use AttemptBudgetState::*;
        // Nothing returns to Reserved, no terminal state reopens, and
        // Issued never releases as unissued.
        for from in ALL {
            for to in ALL {
                let legal = matches!(
                    (from, to),
                    (Reserved, Issued)
                        | (Reserved, ReleasedUnissued)
                        | (Issued, Reconciled)
                        | (Issued, ChargedReservedMaximum)
                        | (Issued, ReservationBoundViolated)
                );
                assert_eq!(from.may_transition_to(to), legal, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn terminality() {
        use AttemptBudgetState::*;
        assert!(!Reserved.is_terminal());
        assert!(!Issued.is_terminal());
        for s in [
            ReservationDenied,
            Reconciled,
            ReleasedUnissued,
            ChargedReservedMaximum,
            ReservationBoundViolated,
        ] {
            assert!(s.is_terminal(), "{s:?}");
        }
    }

    #[test]
    fn wire_round_trip() {
        for s in ALL {
            assert_eq!(AttemptBudgetState::parse(s.as_str()), Some(s));
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(serde_json::from_str::<AttemptBudgetState>(&json).unwrap(), s);
        }
        assert_eq!(AttemptBudgetState::parse("unknown"), None);
    }
}
