//! Exact per-thread facts consumed by execution comparison.
//!
//! This module owns no analytics or UI shape. It projects only authoritative
//! signed thread state into a small typed cost sample after the caller has
//! separately authorized the exact subject.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::state_store::{AuthoritativeThreadSubject, StateStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSampleStatus {
    Available,
    Pending,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    Direct,
    Rollup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunCostSample {
    pub status: CostSampleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<CostBasis>,
}

impl RunCostSample {
    fn without_values(status: CostSampleStatus) -> Self {
        Self {
            status,
            turns: None,
            input_tokens: None,
            output_tokens: None,
            spend: None,
            basis: None,
        }
    }
}

/// Read one authorized subject's exact terminal cost without joining parent
/// and child chains or trusting UI-shaped facets.
pub fn run_cost_sample(
    state: &StateStore,
    subject: &AuthoritativeThreadSubject,
) -> Result<RunCostSample> {
    if !subject.status.is_terminal() {
        return project_cost_sample(false, None);
    }

    let terminal = state
        .get_thread_terminal_authority(&subject.thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "terminal thread {} has no authoritative terminal state",
                subject.thread_id
            )
        })?;
    if terminal.status != subject.status {
        bail!(
            "thread {} terminal status changed across authoritative reads",
            subject.thread_id
        );
    }
    project_cost_sample(true, terminal.final_cost.as_ref())
}

fn project_cost_sample(
    terminal: bool,
    final_cost: Option<&ryeos_engine::contracts::FinalCost>,
) -> Result<RunCostSample> {
    if !terminal {
        return Ok(RunCostSample::without_values(CostSampleStatus::Pending));
    }
    let Some(cost) = final_cost else {
        return Ok(RunCostSample::without_values(CostSampleStatus::Unavailable));
    };
    crate::state_store::validate_final_cost_for_settlement(&cost)?;
    let basis = match cost.basis.as_deref() {
        None => CostBasis::Direct,
        Some(ryeos_engine::launch_envelope_types::COST_BASIS_ROLLUP) => CostBasis::Rollup,
        Some(_) => unreachable!("validated final cost admits only direct or rollup basis"),
    };
    Ok(RunCostSample {
        status: CostSampleStatus::Available,
        turns: Some(cost.turns),
        input_tokens: Some(cost.input_tokens),
        output_tokens: Some(cost.output_tokens),
        spend: Some(cost.spend.to_canonical_string()),
        basis: Some(basis),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost() -> ryeos_engine::contracts::FinalCost {
        ryeos_engine::contracts::FinalCost {
            turns: 3,
            input_tokens: 11,
            output_tokens: 7,
            spend: ryeos_accounting::UsdNanos::parse_canonical("0.125").unwrap(),
            provider: Some("private-provider-label".to_string()),
            basis: None,
            metadata: Some(serde_json::json!({"private": "metadata"})),
        }
    }

    #[test]
    fn pending_and_terminal_without_cost_are_distinct() {
        let pending = project_cost_sample(false, None).unwrap();
        let unavailable = project_cost_sample(true, None).unwrap();
        assert_eq!(pending.status, CostSampleStatus::Pending);
        assert_eq!(unavailable.status, CostSampleStatus::Unavailable);
        assert_eq!(pending.turns, None);
        assert_eq!(unavailable.spend, None);
    }

    #[test]
    fn direct_rollup_and_zero_are_exact_without_provider_metadata() {
        let direct = project_cost_sample(true, Some(&cost())).unwrap();
        assert_eq!(direct.status, CostSampleStatus::Available);
        assert_eq!(direct.basis, Some(CostBasis::Direct));
        assert_eq!(direct.spend.as_deref(), Some("0.125"));

        let mut rollup = cost();
        rollup.basis = Some(ryeos_engine::launch_envelope_types::COST_BASIS_ROLLUP.to_string());
        assert_eq!(
            project_cost_sample(true, Some(&rollup)).unwrap().basis,
            Some(CostBasis::Rollup)
        );

        let mut zero = cost();
        zero.turns = 0;
        zero.input_tokens = 0;
        zero.output_tokens = 0;
        zero.spend = ryeos_accounting::UsdNanos::ZERO;
        let zero = project_cost_sample(true, Some(&zero)).unwrap();
        assert_eq!(zero.turns, Some(0));
        assert_eq!(zero.spend.as_deref(), Some("0"));

        let encoded = serde_json::to_string(&direct).unwrap();
        assert!(!encoded.contains("private-provider-label"));
        assert!(!encoded.contains("metadata"));
    }

    #[test]
    fn malformed_basis_and_counter_overflow_are_integrity_errors() {
        let mut malformed = cost();
        malformed.basis = Some("other".to_string());
        assert!(project_cost_sample(true, Some(&malformed)).is_err());

        let mut overflow = cost();
        overflow.input_tokens = i64::MAX as u64 + 1;
        assert!(project_cost_sample(true, Some(&overflow)).is_err());
    }
}
