//! The single strict audit event emitted from committed ledger transitions:
//! `provider_attempt_budget_transition_v1`.
//!
//! Emitted only by the daemon through the transactional outbox; forbidden
//! content: credentials, headers, request/response bodies, prompts, tool
//! arguments, and unbounded raw error metadata.

use serde::{Deserialize, Serialize};

use crate::state::{AttemptBudgetState, ChargeBasis, ReconciliationReason};

pub const PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION: u32 = 1;

/// `accounting-transition/<attempt_id>/<transition-sequence>`
pub fn transition_id(attempt_id: &str, transition_sequence: u32) -> String {
    format!("accounting-transition/{attempt_id}/{transition_sequence}")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptBudgetTransitionV1 {
    pub version: u32,
    pub transition_id: String,
    pub transition_sequence: u32,
    pub attempt_id: String,
    pub budget_authority_site_id: String,
    pub ledger_epoch: u64,
    pub execution_budget_id: String,
    /// Top-level execution attribution (immutable admitted root).
    pub root_chain_id: String,
    /// The physical audit chain this transition is published to. Never
    /// conflated with `root_chain_id`.
    pub audit_chain_root_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directive_budget_id: Option<String>,
    pub thread_id: String,
    pub turn: u32,
    pub attempt_number: u32,
    pub transition: AttemptBudgetState,
    /// True for a late-actual observation on `ChargedReservedMaximum`;
    /// the terminal state does not change, budget commitments may.
    #[serde(default)]
    pub observation: bool,
    pub config_hash: String,
    pub provider_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub reserved_usd_nanos: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_charge_usd_nanos: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_actual_usd_nanos: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_usd_nanos: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_basis: Option<ChargeBasis>,
    pub occurred_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ReconciliationReason>,
}

impl ProviderAttemptBudgetTransitionV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION {
            return Err(format!("unsupported transition version {}", self.version));
        }
        if self.transition_id != transition_id(&self.attempt_id, self.transition_sequence) {
            return Err("transition_id does not match attempt/sequence".to_string());
        }
        if self.reserved_usd_nanos < 0
            || self.budget_charge_usd_nanos.is_some_and(|v| v < 0)
            || self.provider_actual_usd_nanos.is_some_and(|v| v < 0)
            || self.released_usd_nanos.is_some_and(|v| v < 0)
        {
            return Err("money fields must be non-negative nanos".to_string());
        }
        if self.observation && self.transition != AttemptBudgetState::ChargedReservedMaximum {
            return Err(
                "late observation transitions exist only on charged_reserved_maximum".to_string(),
            );
        }
        if self.thread_id.is_empty()
            || self.attempt_id.is_empty()
            || self.execution_budget_id.is_empty()
            || self.budget_authority_site_id.is_empty()
            || self.root_chain_id.is_empty()
            || self.audit_chain_root_id.is_empty()
        {
            return Err("identity fields must be non-empty".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> ProviderAttemptBudgetTransitionV1 {
        ProviderAttemptBudgetTransitionV1 {
            version: 1,
            transition_id: transition_id("A-1", 3),
            transition_sequence: 3,
            attempt_id: "A-1".to_string(),
            budget_authority_site_id: "S-1".to_string(),
            ledger_epoch: 1,
            execution_budget_id: "B-1".to_string(),
            root_chain_id: "T-root".to_string(),
            audit_chain_root_id: "T-chain".to_string(),
            directive_budget_id: Some("D-1".to_string()),
            thread_id: "T-1".to_string(),
            turn: 3,
            attempt_number: 2,
            transition: AttemptBudgetState::Reconciled,
            observation: false,
            config_hash: "cfg".to_string(),
            provider_id: "route".to_string(),
            model: "model".to_string(),
            profile: None,
            reserved_usd_nanos: 1_250_000_000,
            budget_charge_usd_nanos: Some(380_000_000),
            provider_actual_usd_nanos: Some(380_000_000),
            released_usd_nanos: Some(870_000_000),
            charge_basis: Some(ChargeBasis::ProviderReported),
            occurred_at_ms: 1_753_305_600_000,
            reason: Some(ReconciliationReason::ProviderReportedFinal),
        }
    }

    #[test]
    fn valid_event_round_trips_strictly() {
        let e = event();
        e.validate().unwrap();
        let mut value = serde_json::to_value(&e).unwrap();
        let back: ProviderAttemptBudgetTransitionV1 =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, e);
        value
            .as_object_mut()
            .unwrap()
            .insert("prompt".to_string(), serde_json::Value::Null);
        assert!(serde_json::from_value::<ProviderAttemptBudgetTransitionV1>(value).is_err());
    }

    #[test]
    fn transition_id_binding_enforced() {
        let mut e = event();
        e.transition_sequence = 4;
        assert!(e.validate().is_err());
    }

    #[test]
    fn negative_money_rejected() {
        let mut e = event();
        e.budget_charge_usd_nanos = Some(-1);
        assert!(e.validate().is_err());
    }

    #[test]
    fn observation_only_on_charged_reserved_maximum() {
        let mut e = event();
        e.observation = true;
        assert!(e.validate().is_err());
        e.transition = AttemptBudgetState::ChargedReservedMaximum;
        e.validate().unwrap();
    }
}
