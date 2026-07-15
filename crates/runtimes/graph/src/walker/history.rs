use serde_json::Value;

use crate::evaluation::validate_runtime_shape;
use crate::model::{ErrorRecord, GraphDefinition, MAX_GRAPH_STEPS};
use ryeos_runtime::envelope::{RuntimeCost, COST_BASIS_ROLLUP};
use ryeos_runtime::{EvaluationLimits, RuntimeJsonArrayBudget};

use super::outcome::GraphAccounting;

/// Validate checkpoint-owned snapshots before a resume value reaches the
/// walker. The closed accounting/error types remain private to graph execution;
/// this boundary rejects corrupt history instead of silently under-reporting a
/// resumed run.
pub(crate) fn validate_checkpoint_snapshots(
    accounting: &Value,
    suppressed_errors: &Value,
    step_count: u32,
    definition: &GraphDefinition,
) -> anyhow::Result<()> {
    validate_runtime_shape(accounting, "checkpoint accounting")
        .map_err(|error| anyhow::anyhow!("invalid checkpoint accounting: {error}"))?;
    validate_runtime_shape(suppressed_errors, "checkpoint suppressed_errors")
        .map_err(|error| anyhow::anyhow!("invalid checkpoint suppressed_errors: {error}"))?;
    let accounting_object = accounting
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid checkpoint accounting: expected object"))?;
    for key in accounting_object.keys() {
        if key != "total" && key != "nodes" && key != "hooks" {
            anyhow::bail!("invalid checkpoint accounting: unknown field `{key}`");
        }
    }
    for required in ["total", "nodes", "hooks"] {
        if !accounting_object.contains_key(required) {
            anyhow::bail!(
                "invalid checkpoint accounting: missing required field `{required}`"
            );
        }
    }

    let accounting = serde_json::from_value::<GraphAccounting>(accounting.clone())
        .map_err(|error| anyhow::anyhow!("invalid checkpoint accounting: {error}"))?;
    let suppressed_errors = serde_json::from_value::<Vec<ErrorRecord>>(suppressed_errors.clone())
        .map_err(|error| anyhow::anyhow!("invalid checkpoint suppressed_errors: {error}"))?;
    let limit = MAX_GRAPH_STEPS as usize;
    if accounting.nodes.len() > limit {
        anyhow::bail!(
            "invalid checkpoint accounting: {} entries exceeds graph run history limit ({limit})",
            accounting.nodes.len()
        );
    }
    if accounting.hooks.len() > limit.saturating_add(2) {
        anyhow::bail!(
            "invalid checkpoint accounting: {} hook entries exceeds graph run history limit ({})",
            accounting.hooks.len(),
            limit.saturating_add(2)
        );
    }
    if suppressed_errors.len() > limit {
        anyhow::bail!(
            "invalid checkpoint suppressed_errors: {} entries exceeds graph run history limit ({limit})",
            suppressed_errors.len()
        );
    }

    let mut expected_total = RuntimeCost {
        input_tokens: 0,
        output_tokens: 0,
        total_usd: 0.0,
        basis: Some(COST_BASIS_ROLLUP.to_string()),
    };
    let mut previous_accounting_step = None;
    for (index, record) in accounting.nodes.iter().enumerate() {
        if record.step >= step_count {
            anyhow::bail!(
                "invalid checkpoint accounting: node record {index} step {} must precede checkpoint step_count {step_count}",
                record.step
            );
        }
        if previous_accounting_step.is_some_and(|previous| record.step <= previous) {
            anyhow::bail!(
                "invalid checkpoint accounting: node record {index} step {} must be strictly greater than the previous step",
                record.step
            );
        }
        if !definition.config.nodes.contains_key(&record.node) {
            anyhow::bail!(
                "invalid checkpoint accounting: node record {index} references unknown node `{}`",
                record.node
            );
        }
        record.cost.validate().map_err(|error| {
            anyhow::anyhow!("invalid checkpoint accounting node record {index}: {error}")
        })?;
        expected_total
            .checked_accumulate(&record.cost)
            .map_err(|error| {
                anyhow::anyhow!("invalid checkpoint accounting rollup: {error}")
            })?;
        previous_accounting_step = Some(record.step);
    }

    let mut saw_graph_started = false;
    let mut previous_hook_step = None;
    for (index, record) in accounting.hooks.iter().enumerate() {
        match record.event {
            ryeos_runtime::RuntimeEventType::GraphStarted => {
                if record.step.is_some() || saw_graph_started || index != 0 {
                    anyhow::bail!(
                        "invalid checkpoint accounting: graph_started hook cost must be the unique first hook record with no step"
                    );
                }
                saw_graph_started = true;
            }
            ryeos_runtime::RuntimeEventType::GraphStepCompleted => {
                let step = record.step.ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid checkpoint accounting: graph_step_completed hook cost is missing step"
                    )
                })?;
                if step >= step_count {
                    anyhow::bail!(
                        "invalid checkpoint accounting: hook record {index} step {step} must precede checkpoint step_count {step_count}"
                    );
                }
                if previous_hook_step.is_some_and(|previous| step <= previous) {
                    anyhow::bail!(
                        "invalid checkpoint accounting: hook record {index} step {step} must be strictly greater than the previous hook step"
                    );
                }
                previous_hook_step = Some(step);
            }
            ryeos_runtime::RuntimeEventType::GraphCompleted => {
                anyhow::bail!(
                    "invalid checkpoint accounting: graph_completed hook cost cannot appear in a resumable checkpoint"
                );
            }
            other => {
                anyhow::bail!(
                    "invalid checkpoint accounting: unsupported hook cost event `{}`",
                    other.as_str()
                );
            }
        }
        record.cost.validate().map_err(|error| {
            anyhow::anyhow!("invalid checkpoint accounting hook record {index}: {error}")
        })?;
        expected_total
            .checked_accumulate(&record.cost)
            .map_err(|error| anyhow::anyhow!("invalid checkpoint accounting rollup: {error}"))?;
    }

    let accounting_is_empty = accounting.nodes.is_empty() && accounting.hooks.is_empty();
    match (&accounting.total, accounting_is_empty) {
        (None, true) => {}
        (None, false) => {
            anyhow::bail!(
                "invalid checkpoint accounting: total is missing for non-empty node history"
            );
        }
        (Some(_), true) => {
            anyhow::bail!(
                "invalid checkpoint accounting: total must be absent for empty node history"
            );
        }
        (Some(total), false) => {
            total
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid checkpoint accounting total: {error}"))?;
            if total.basis.as_deref() != Some(COST_BASIS_ROLLUP) {
                anyhow::bail!(
                    "invalid checkpoint accounting: total basis must be `{COST_BASIS_ROLLUP}`"
                );
            }
            if total.input_tokens != expected_total.input_tokens
                || total.output_tokens != expected_total.output_tokens
                || total.total_usd != expected_total.total_usd
            {
                anyhow::bail!(
                    "invalid checkpoint accounting: total does not match checked node/hook rollup"
                );
            }
        }
    }

    let mut previous_error_step = None;
    for (index, error) in suppressed_errors.iter().enumerate() {
        if error.step >= step_count {
            anyhow::bail!(
                "invalid checkpoint suppressed_errors: record {index} step {} must precede checkpoint step_count {step_count}",
                error.step
            );
        }
        if previous_error_step.is_some_and(|previous| error.step < previous) {
            anyhow::bail!(
                "invalid checkpoint suppressed_errors: record {index} step {} precedes the previous record",
                error.step
            );
        }
        if !definition.config.nodes.contains_key(&error.node) {
            anyhow::bail!(
                "invalid checkpoint suppressed_errors: record {index} references unknown node `{}`",
                error.node
            );
        }
        previous_error_step = Some(error.step);
    }
    Ok(())
}

/// Callback and hook drift is diagnostic output, but it is still runtime-owned
/// JSON and must not grow without limit. One slot is reserved for a stable
/// truncation marker so bounded loss remains visible to callers.
const MAX_GRAPH_WARNINGS: usize = MAX_GRAPH_STEPS as usize;
const MAX_GRAPH_WARNING_BYTES: usize = 1024 * 1024;
pub(super) const MAX_GRAPH_WARNING_SCALAR_BYTES: usize = 16 * 1024;
pub(super) const GRAPH_WARNINGS_TRUNCATED: &str =
    "additional graph warnings omitted after warning bounds were reached";

pub(super) struct WarningBuffer {
    entries: Vec<String>,
    aggregate: RuntimeJsonArrayBudget,
    truncated: bool,
}

impl Default for WarningBuffer {
    fn default() -> Self {
        let limits = EvaluationLimits {
            max_container_elements: MAX_GRAPH_WARNINGS,
            max_result_nodes: MAX_GRAPH_WARNINGS.saturating_add(1),
            max_result_bytes: MAX_GRAPH_WARNING_BYTES,
            max_scalar_bytes: MAX_GRAPH_WARNING_SCALAR_BYTES,
            ..EvaluationLimits::default()
        };
        let mut aggregate = RuntimeJsonArrayBudget::with_limits("graph warnings", limits);
        aggregate
            .append(&Value::String(GRAPH_WARNINGS_TRUNCATED.to_string()))
            .expect("fixed graph warning truncation marker fits warning bounds");
        Self {
            entries: Vec::new(),
            aggregate,
            truncated: false,
        }
    }
}

impl WarningBuffer {
    pub(super) fn push(&mut self, warning: String) {
        if self.truncated {
            return;
        }
        if self.aggregate.append(&Value::String(warning.clone())).is_err() {
            self.truncated = true;
            return;
        }
        self.entries.push(warning);
    }

    pub(super) fn snapshot(&self) -> Vec<String> {
        let mut warnings = self.entries.clone();
        if self.truncated {
            warnings.push(GRAPH_WARNINGS_TRUNCATED.to_string());
        }
        warnings
    }

    pub(super) fn take(&mut self) -> Vec<String> {
        let current = std::mem::take(self);
        let mut warnings = current.entries;
        if current.truncated {
            warnings.push(GRAPH_WARNINGS_TRUNCATED.to_string());
        }
        warnings
    }
}
