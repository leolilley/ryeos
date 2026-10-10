//! Durable destination selection, not cold reconstruction or cleanup authority.

use super::inspection_plan::InspectionSourcePlan;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalInspectionMode {
    DryRun,
    Mutating,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum InspectionAccountingRequirement {
    Required,
    Unused,
    Absent,
    Inherited {
        operation: ryeos_state::ThreadHistoryDiscardOperationRef,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalInspectionPlan {
    pub(super) mode: OriginalInspectionMode,
    pub(super) scheduler_selected: bool,
    pub(super) accounting: [InspectionAccountingRequirement; 2],
}
impl OriginalInspectionPlan {
    pub(super) fn original(
        mode: OriginalInspectionMode,
        scheduler_selected: bool,
        prior: Option<&ryeos_state::ThreadHistoryDiscardOperationRef>,
        sources: &[InspectionSourcePlan; 2],
    ) -> Self {
        let runtime = match mode {
            OriginalInspectionMode::DryRun => InspectionAccountingRequirement::Required,
            OriginalInspectionMode::Mutating => InspectionAccountingRequirement::Unused,
        };
        let scheduler = if let Some(operation) = prior {
            InspectionAccountingRequirement::Inherited {
                operation: operation.clone(),
            }
        } else if !scheduler_selected || sources[1].roles[0].is_none() {
            InspectionAccountingRequirement::Absent
        } else {
            InspectionAccountingRequirement::Required
        };
        Self {
            mode,
            scheduler_selected,
            accounting: [runtime, scheduler],
        }
    }
    pub(super) fn validate(
        &self,
        prior: Option<&ryeos_state::ThreadHistoryDiscardOperationRef>,
        sources: &[InspectionSourcePlan; 2],
    ) -> Result<()> {
        ensure!(
            self == &Self::original(self.mode, self.scheduler_selected, prior, sources),
            "inspection accounting plan differs from original mode/selection/ancestor"
        );
        if !self.scheduler_selected
            || matches!(self.accounting[1], InspectionAccountingRequirement::Absent)
        {
            ensure!(
                sources[1].roles.iter().all(Option::is_none),
                "unselected/absent scheduler retains original source roles"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectionDestinationPlan {
    pub(super) purpose: String,
    pub(super) directory_name: String,
}

pub(super) fn destination_plans(operation_id: &str) -> [InspectionDestinationPlan; 2] {
    ["runtime", "scheduler"].map(|purpose| InspectionDestinationPlan {
        purpose: purpose.to_owned(),
        directory_name: format!("inspection.{operation_id}.{purpose}"),
    })
}

pub(super) fn validate_destinations(
    plans: &[InspectionDestinationPlan; 2],
    operation_id: &str,
    sources: &[InspectionSourcePlan; 2],
) -> Result<()> {
    let expected = destination_plans(operation_id);
    for ((plan, expected), source) in plans.iter().zip(&expected).zip(sources) {
        ensure!(
            plan.purpose == expected.purpose
                && plan.purpose == source.purpose
                && plan.directory_name == expected.directory_name,
            "inspection destination differs from original fixed operation selection"
        );
    }
    Ok(())
}

#[cfg(test)]
mod controls {
    use super::*;
    #[test]
    fn fixed_names_cannot_select_another_operation_or_store() {
        let operation = "a".repeat(32);
        let sources = ["runtime", "scheduler"].map(|purpose| InspectionSourcePlan {
            purpose: purpose.into(),
            directory_identity: "wire-only".into(),
            database_name: if purpose == "runtime" {
                "runtime.sqlite3"
            } else {
                "scheduler.sqlite3"
            }
            .into(),
            roles: std::array::from_fn(|_| None),
        });
        let plans = destination_plans(&operation);
        validate_destinations(&plans, &operation, &sources).unwrap();
        assert!(validate_destinations(&plans, &"b".repeat(32), &sources).is_err());
        let mut changed = plans.clone();
        changed.swap(0, 1);
        assert!(validate_destinations(&changed, &operation, &sources).is_err());
        changed = plans;
        changed[0].directory_name = "../foreign".into();
        assert!(validate_destinations(&changed, &operation, &sources).is_err());
    }
}

pub(super) fn disposition_name(operation_id: &str) -> String {
    format!("disposed.{operation_id}")
}
pub(super) fn terminal_name(operation_id: &str) -> String {
    format!("terminal.{operation_id}")
}

pub(super) fn prepared_source_retirement_name(operation_id: &str) -> String {
    format!("prepared.{operation_id}")
}


pub(super) fn account_settlement_intent_name(operation_id: &str) -> String {
    format!("account-intent.{operation_id}")
}

pub(super) fn account_settlement_name(operation_id: &str) -> String {
    format!("account-settled.{operation_id}")
}
