use serde::Serialize;
use serde_json::Value;

use crate::cache::NodeCache;
use crate::context;
use crate::model::*;
use ryeos_runtime::checkpoint::checkpoint_shape_limits;
use ryeos_runtime::envelope::{RuntimeCost, RuntimeCostError};
use ryeos_runtime::{EvaluationLimits, RuntimeJsonArrayBudget};

pub(super) fn add_runtime_cost(
    total: &mut Option<RuntimeCost>,
    cost: Option<RuntimeCost>,
) -> Result<(), RuntimeCostError> {
    let Some(cost) = cost else {
        return Ok(());
    };
    cost.validate()?;
    if let Some(aggregate) = total.as_mut() {
        aggregate.checked_accumulate(&cost)?;
    } else {
        *total = Some(RuntimeCost {
            input_tokens: cost.input_tokens,
            output_tokens: cost.output_tokens,
            total_usd: cost.total_usd,
            basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
        });
    }
    Ok(())
}

/// Running cost accumulator for a single graph execution.
///
/// The aggregate is a rollup: child cost remains attributable to the child
/// thread and is also included here for parent graph reporting.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct GraphAccounting {
    pub(super) total: Option<RuntimeCost>,
    pub(super) nodes: Vec<NodeCostRecord>,
    pub(super) hooks: Vec<HookCostRecord>,
}

impl GraphAccounting {
    fn record(&mut self, record: NodeCostRecord) -> anyhow::Result<()> {
        let mut total = self.total.clone().unwrap_or_else(|| RuntimeCost {
            input_tokens: 0,
            output_tokens: 0,
            total_usd: 0.0,
            basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
        });
        total
            .checked_accumulate(&record.cost)
            .map_err(anyhow::Error::new)?;
        self.total = Some(total);
        self.nodes.push(record);
        Ok(())
    }

    fn record_hook(&mut self, record: HookCostRecord) -> anyhow::Result<()> {
        let mut total = self.total.clone().unwrap_or_else(|| RuntimeCost {
            input_tokens: 0,
            output_tokens: 0,
            total_usd: 0.0,
            basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
        });
        total
            .checked_accumulate(&record.cost)
            .map_err(anyhow::Error::new)?;
        self.total = Some(total);
        self.hooks.push(record);
        Ok(())
    }
}

/// Incremental rye-expr/1 aggregate budget for one typed run history.
///
/// The history value itself remains strongly typed. Serialization exists only
/// at this accounting boundary so the exact JSON that can reach a checkpoint
/// is measured once, when an entry is accepted, instead of rescanning the
/// entire history after every graph step.
#[derive(Debug, Clone)]
struct RunHistoryBudget {
    field: &'static str,
    max_entries: usize,
    aggregate: RuntimeJsonArrayBudget,
}

impl RunHistoryBudget {
    fn new(field: &'static str) -> Self {
        Self::with_limits(
            field,
            crate::model::MAX_GRAPH_STEPS as usize,
            checkpoint_shape_limits(),
        )
    }

    fn with_limits(
        field: &'static str,
        max_entries: usize,
        limits: EvaluationLimits,
    ) -> Self {
        Self {
            field,
            max_entries,
            aggregate: RuntimeJsonArrayBudget::with_limits(field, limits),
        }
    }

    fn append<T: Serialize>(&mut self, entry: &T) -> anyhow::Result<()> {
        if self.aggregate.elements() >= self.max_entries {
            anyhow::bail!(
                "{} exceeds the graph run history entry limit ({})",
                self.field,
                self.max_entries
            );
        }
        let value = serde_json::to_value(entry)
            .map_err(|error| anyhow::anyhow!("serialize {} entry: {error}", self.field))?;
        self.aggregate
            .append(&value)
            .map_err(|error| anyhow::anyhow!("{} exceeded rye-expr/1 bounds: {error}", self.field))
    }
}

/// Per-run history bounds shared by accounting and suppressed errors, plus the
/// validation boundary for independently persisted node receipts. The first
/// rejection is sticky: later history entries are discarded so a failed run
/// cannot continue growing memory while it approaches its commit boundary.
/// Checkpoint and terminal settlement surface the stored failure.
#[derive(Debug, Clone)]
pub(super) struct RunHistoryBudgets {
    accounting: RunHistoryBudget,
    suppressed_errors: RunHistoryBudget,
    failure: Option<String>,
}

impl Default for RunHistoryBudgets {
    fn default() -> Self {
        Self {
            accounting: RunHistoryBudget::with_limits(
                "graph accounting history",
                (crate::model::MAX_GRAPH_STEPS as usize)
                    .saturating_mul(2)
                    .saturating_add(2),
                checkpoint_shape_limits(),
            ),
            suppressed_errors: RunHistoryBudget::new("graph suppressed error history"),
            failure: None,
        }
    }
}

impl RunHistoryBudgets {
    pub(super) fn seed(
        accounting: &GraphAccounting,
        suppressed_errors: &[ErrorRecord],
    ) -> anyhow::Result<Self> {
        let mut budgets = Self::default();
        for record in &accounting.nodes {
            budgets.accounting.append(record)?;
        }
        for record in &accounting.hooks {
            budgets.accounting.append(record)?;
        }
        for error in suppressed_errors {
            budgets.suppressed_errors.append(error)?;
        }
        Ok(budgets)
    }

    pub(super) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub(super) fn record_accounting(
        &mut self,
        accounting: &mut GraphAccounting,
        record: NodeCostRecord,
    ) {
        if self.failure.is_some() {
            return;
        }
        let mut next_budget = self.accounting.clone();
        if let Err(error) = next_budget.append(&record) {
            self.reject(error);
            return;
        }
        match accounting.record(record) {
            Ok(()) => self.accounting = next_budget,
            Err(error) => self.reject(error),
        }
    }

    pub(super) fn record_hook_accounting(
        &mut self,
        accounting: &mut GraphAccounting,
        record: HookCostRecord,
    ) {
        if self.failure.is_some() {
            return;
        }
        let mut next_budget = self.accounting.clone();
        if let Err(error) = next_budget.append(&record) {
            self.reject(error);
            return;
        }
        match accounting.record_hook(record) {
            Ok(()) => self.accounting = next_budget,
            Err(error) => self.reject(error),
        }
    }

    pub(super) fn accept_receipt(&mut self, receipt: &NodeReceipt) -> bool {
        if self.failure.is_some() {
            return false;
        }
        let result = receipt
            .cost
            .as_ref()
            .map_or(Ok(()), RuntimeCost::validate)
            .map_err(anyhow::Error::new)
            .and_then(|()| {
                serde_json::to_value(receipt)
                    .map_err(|error| {
                        anyhow::anyhow!("serialize graph node receipt: {error}")
                    })
                    .and_then(|value| {
                        crate::evaluation::validate_runtime_shape(&value, "graph node receipt")
                            .map_err(|error| {
                                anyhow::anyhow!(
                                    "graph node receipt exceeded rye-expr/1 bounds: {error}"
                                )
                            })
                    })
            });
        match result {
            Ok(()) => true,
            Err(error) => {
                self.reject(error);
                false
            }
        }
    }

    pub(super) fn push_suppressed(
        &mut self,
        history: &mut Vec<ErrorRecord>,
        error: ErrorRecord,
    ) {
        self.extend_suppressed(history, std::iter::once(error));
    }

    pub(super) fn extend_suppressed(
        &mut self,
        history: &mut Vec<ErrorRecord>,
        errors: impl IntoIterator<Item = ErrorRecord>,
    ) {
        if self.failure.is_some() {
            return;
        }
        let mut next_budget = self.suppressed_errors.clone();
        let mut accepted = Vec::new();
        for entry in errors {
            if let Err(error) = next_budget.append(&entry) {
                self.reject(error);
                return;
            }
            accepted.push(entry);
        }
        self.suppressed_errors = next_budget;
        history.extend(accepted);
    }

    fn reject(&mut self, error: anyhow::Error) {
        if self.failure.is_none() {
            self.failure = Some(format!("graph run history rejected: {error}"));
        }
    }

    pub(super) fn reject_external(&mut self, error: impl std::fmt::Display) {
        if self.failure.is_none() {
            self.failure = Some(format!("graph run history rejected: {error}"));
        }
    }
}

pub(super) struct ActionOkOutcome {
    pub(super) item_id: String,
    pub(super) result: Value,
    pub(super) assign: Option<Value>,
    pub(super) next: Option<String>,
    /// Deferred until every assignment and branch expression has
    /// succeeded, then emitted by the commit fence.
    pub(super) child_thread_id: Option<String>,
    pub(super) cache_hit: bool,
    /// Cache key reserved on a miss. The result is persisted only in commit,
    /// after result validation, assignment, and branch selection all succeed.
    pub(super) cache_write_key: Option<String>,
    pub(super) elapsed_ms: u64,
    pub(super) cost: Option<RuntimeCost>,
}

pub(super) struct ForeachDoneOutcome {
    pub(super) results: Vec<Value>,
    pub(super) statuses: Vec<GraphToolCallStatus>,
    pub(super) total_items: usize,
    pub(super) collect_key: Option<String>,
    pub(super) assign_delta: Value,
    pub(super) errors: Vec<ErrorRecord>,
    pub(super) next: Option<String>,
    pub(super) item_id: String,
    pub(super) cost: Option<RuntimeCost>,
    pub(super) observations: Vec<DispatchObservation>,
}

pub(super) struct ForeachFailedOutcome {
    pub(super) statuses: Vec<GraphToolCallStatus>,
    pub(super) total_items: usize,
    pub(super) errors: Vec<ErrorRecord>,
    pub(super) item_id: String,
    pub(super) next_on_error: NextOnError,
    pub(super) elapsed_ms: u64,
    pub(super) cost: Option<RuntimeCost>,
    pub(super) observations: Vec<DispatchObservation>,
}

pub(super) struct FollowFanoutSuspendOutcome {
    pub(super) children: Vec<ryeos_runtime::callback::FollowChildSpec>,
    pub(super) width: Option<u32>,
    pub(super) iteration_snapshot: Vec<Value>,
}

pub(super) struct FollowFanoutDoneOutcome {
    pub(super) results: Vec<Value>,
    pub(super) statuses: Vec<FanoutItemStatus>,
    pub(super) errors: Vec<ErrorRecord>,
    pub(super) collect_key: Option<String>,
    pub(super) item_id: String,
    pub(super) next: Option<String>,
    pub(super) next_on_error: NextOnError,
    pub(super) cost: Option<RuntimeCost>,
    pub(super) elapsed_ms: u64,
}

pub(super) struct LeafSoftErrorOutcome {
    pub(super) item_id: String,
    pub(super) error: String,
    pub(super) next_on_error: NextOnError,
    pub(super) elapsed_ms: u64,
    pub(super) cost: Option<RuntimeCost>,
    /// A child dispatch may return a terminal failure after its thread already
    /// exists. Publish that lineage observation behind the normal commit fence.
    pub(super) observation: Option<DispatchObservation>,
}

pub(super) struct DispatchHardErrorOutcome {
    pub(super) item_id: Option<String>,
    pub(super) error: String,
    pub(super) next_on_error: NextOnError,
    pub(super) elapsed_ms: u64,
    pub(super) cost: Option<RuntimeCost>,
}

pub(super) struct ExpressionFailedOutcome {
    pub(super) item_id: Option<String>,
    pub(super) error: String,
    pub(super) next_on_error: NextOnError,
    pub(super) elapsed_ms: u64,
    pub(super) cost: Option<RuntimeCost>,
    pub(super) effects: ExpressionFailureEffects,
}

/// A runtime integrity or resource-bound failure. Unlike an authored
/// expression failure this outcome has no `on_error` route: once the runtime
/// cannot retain the complete result/state/cost provenance, continuing would
/// make the durable graph history untrustworthy.
pub(super) struct IntegrityFailedOutcome {
    pub(super) item_id: Option<String>,
    pub(super) error: String,
    pub(super) elapsed_ms: u64,
    pub(super) cost: Option<RuntimeCost>,
    pub(super) effects: ExpressionFailureEffects,
}

impl From<IntegrityFailedOutcome> for ExpressionFailedOutcome {
    fn from(outcome: IntegrityFailedOutcome) -> Self {
        Self {
            item_id: outcome.item_id,
            error: outcome.error,
            next_on_error: NextOnError::PolicyFail,
            elapsed_ms: outcome.elapsed_ms,
            cost: outcome.cost,
            effects: outcome.effects,
        }
    }
}

#[derive(Default)]
pub(super) struct ExpressionFailureEffects {
    pub(super) observations: Vec<DispatchObservation>,
    pub(super) foreach: Option<ForeachIterationEffects>,
    pub(super) fanout: Option<FanoutExpressionEffects>,
    pub(super) suppressed_errors: Vec<ErrorRecord>,
}

pub(super) struct ForeachIterationEffects {
    pub(super) statuses: Vec<GraphToolCallStatus>,
    pub(super) total_items: usize,
}

pub(super) struct FanoutExpressionEffects {
    pub(super) results: Vec<Value>,
    pub(super) statuses: Vec<FanoutItemStatus>,
}

impl ExpressionFailureEffects {
    pub(super) fn action(observation: Option<DispatchObservation>) -> Self {
        Self {
            observations: observation.into_iter().collect(),
            ..Self::default()
        }
    }

    pub(super) fn foreach(
        observations: Vec<DispatchObservation>,
        statuses: Vec<GraphToolCallStatus>,
        total_items: usize,
        errors: Vec<ErrorRecord>,
    ) -> Self {
        Self {
            observations,
            foreach: Some(ForeachIterationEffects {
                statuses,
                total_items,
            }),
            suppressed_errors: errors,
            ..Self::default()
        }
    }

    pub(super) fn fanout(
        results: Vec<Value>,
        statuses: Vec<FanoutItemStatus>,
        errors: Vec<ErrorRecord>,
    ) -> Self {
        Self {
            fanout: Some(FanoutExpressionEffects { results, statuses }),
            suppressed_errors: errors,
            ..Self::default()
        }
    }
}

pub(super) struct FollowSuspendOutcome {
    pub(super) item_id: String,
    pub(super) params: Value,
}

pub(super) struct RetryScheduledOutcome {
    pub(super) item_id: String,
    pub(super) error: String,
    pub(super) failed_attempt: u32,
    pub(super) total_attempts: u32,
    pub(super) delay_ms: u64,
    pub(super) elapsed_ms: u64,
    pub(super) cost: Option<RuntimeCost>,
}

pub(super) struct TerminalOutcome {
    pub(super) status: GraphRunStatus,
    pub(super) error: Option<String>,
    pub(super) origin: TerminalOrigin,
    pub(super) output: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalOrigin {
    Node,
    RunControl,
}

pub(super) struct GateTakenOutcome {
    pub(super) target: Option<String>,
}

pub(super) enum StepOutcome {
    ActionOk(Box<ActionOkOutcome>),
    LeafSoftError(LeafSoftErrorOutcome),
    DispatchHardError(DispatchHardErrorOutcome),
    /// A data-dependent rye-expr/1 failure after graph launch. The route is
    /// resolved once at the failure site so `continue` can terminate without
    /// ever re-evaluating the failed normal edge.
    ExpressionFailed(ExpressionFailedOutcome),
    /// Unsteerable integrity/resource failure. Commit preserves all bounded
    /// cost and dispatch provenance accumulated before settling the run failed.
    IntegrityFailed(IntegrityFailedOutcome),
    GateTaken(GateTakenOutcome),
    ForeachDone(Box<ForeachDoneOutcome>),
    ForeachFailed(Box<ForeachFailedOutcome>),
    FollowSuspend(FollowSuspendOutcome),
    FollowFanoutSuspend(Box<FollowFanoutSuspendOutcome>),
    FollowFanoutDone(Box<FollowFanoutDoneOutcome>),
    RetryScheduled(RetryScheduledOutcome),
    Terminal(TerminalOutcome),
}

pub(super) enum NextOnError {
    Redirect(String),
    PolicyContinue,
    PolicyFail,
}

pub(super) enum CommitResult {
    Advance {
        next_node: String,
        next_step: u32,
        next_retry_attempt: u32,
    },
    Terminate(Box<GraphResult>),
}

pub(super) struct FollowResumeState {
    pub(super) follow_node: String,
    pub(super) follow_result: Option<Value>,
    pub(super) iteration_snapshot: Option<Vec<Value>>,
}

pub(super) struct RunGuard {
    pub(super) finalized: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.finalized {
            tracing::warn!("graph RunGuard dropped without finalization");
        }
    }
}

pub(super) struct RunNodeBodyContext<'a> {
    pub(super) current: &'a str,
    pub(super) node: &'a GraphNode,
    pub(super) cfg: &'a GraphConfig,
    pub(super) step: u32,
    pub(super) state: &'a Value,
    pub(super) inputs: &'a Value,
    pub(super) exec_ctx: &'a context::ExecutionContext,
    pub(super) cache: &'a NodeCache,
    pub(super) graph_run_id: &'a str,
    pub(super) suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub(super) retry_attempt: u32,
}

pub(super) struct CommitStepInput<'a> {
    pub(super) graph_run_id: &'a str,
    pub(super) step: u32,
    pub(super) current: &'a str,
    pub(super) state: &'a mut Value,
    pub(super) suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub(super) outcome: StepOutcome,
    pub(super) guard: &'a mut RunGuard,
    pub(super) inputs: &'a Value,
    pub(super) execution: &'a Value,
}

pub(super) struct CommitStepContext<'a> {
    pub(super) graph_run_id: &'a str,
    pub(super) step: u32,
    pub(super) current: &'a str,
    pub(super) state: &'a mut Value,
    pub(super) suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub(super) guard: &'a mut RunGuard,
    pub(super) inputs: &'a Value,
    pub(super) execution: &'a Value,
}

pub(super) struct CommitTerminalInput<'a> {
    pub(super) graph_run_id: &'a str,
    pub(super) steps: u32,
    pub(super) state: &'a mut Value,
    pub(super) suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub(super) base_status: GraphRunStatus,
    pub(super) error: Option<&'a str>,
    pub(super) output: Option<Value>,
    pub(super) guard: &'a mut RunGuard,
    pub(super) current_node_id: &'a str,
    pub(super) inputs: &'a Value,
    pub(super) execution: &'a Value,
}

#[cfg(test)]
mod history_tests {
    use super::*;

    fn error_record(message: impl Into<String>) -> ErrorRecord {
        ErrorRecord {
            step: 1,
            node: "node".to_string(),
            error: message.into(),
        }
    }

    #[test]
    fn history_budget_enforces_the_graph_step_ceiling() {
        assert_eq!(
            RunHistoryBudget::new("production history").max_entries,
            crate::model::MAX_GRAPH_STEPS as usize
        );
        let mut budget = RunHistoryBudget::with_limits(
            "test history",
            1,
            EvaluationLimits::default(),
        );

        budget.append(&error_record("first")).unwrap();
        let error = budget.append(&error_record("second")).unwrap_err();

        assert!(error.to_string().contains("entry limit (1)"));
        assert_eq!(budget.aggregate.elements(), 1);
    }

    #[test]
    fn production_history_budget_matches_checkpoint_shape_fuel() {
        let record = error_record("x".repeat(250_000));
        let encoded = serde_json::to_value(&record).unwrap();
        assert!(
            crate::evaluation::validate_runtime_shape(&encoded, "test history record").is_ok()
        );

        let mut budget = RunHistoryBudget::new("test history");
        assert!(budget.append(&record).is_ok());
    }

    #[test]
    fn history_budget_rejects_combined_bytes_incrementally() {
        let limits = EvaluationLimits {
            max_result_bytes: 80,
            ..EvaluationLimits::default()
        };
        let mut budget = RunHistoryBudget::with_limits("test history", 10, limits);

        budget.append(&error_record("first")).unwrap();
        let error = budget.append(&error_record("second")).unwrap_err();

        assert!(error.to_string().contains("JSON byte limit"));
        assert_eq!(budget.aggregate.elements(), 1);
    }

    #[test]
    fn history_budget_enforces_aggregate_nodes_and_depth() {
        let node_limits = EvaluationLimits {
            max_result_nodes: 8,
            ..EvaluationLimits::default()
        };
        let mut nodes = RunHistoryBudget::with_limits("node history", 10, node_limits);
        nodes.append(&error_record("first")).unwrap();
        let node_error = nodes.append(&error_record("second")).unwrap_err();
        assert!(node_error.to_string().contains("JSON node limit"));

        let depth_limits = EvaluationLimits {
            max_result_depth: 3,
            ..EvaluationLimits::default()
        };
        let mut depth = RunHistoryBudget::with_limits("depth history", 10, depth_limits);
        let depth_error = depth
            .append(&serde_json::json!({"nested": {"too_deep": true}}))
            .unwrap_err();
        assert!(depth_error.to_string().contains("JSON depth limit"));
    }

    #[test]
    fn rejected_suppressed_error_batch_is_atomic_and_sticky() {
        let mut budgets = RunHistoryBudgets {
            suppressed_errors: RunHistoryBudget::with_limits(
                "test suppressed errors",
                1,
                EvaluationLimits::default(),
            ),
            ..RunHistoryBudgets::default()
        };
        let mut retained = Vec::new();

        budgets.extend_suppressed(
            &mut retained,
            [error_record("first"), error_record("second")],
        );

        assert!(retained.is_empty());
        assert!(budgets.failure().is_some());
        budgets.push_suppressed(&mut retained, error_record("third"));
        assert!(retained.is_empty());
    }

    #[test]
    fn accounting_out_of_range_rejects_the_record_and_marks_history_failed() {
        let mut accounting = GraphAccounting {
            total: Some(RuntimeCost {
                input_tokens: i64::MAX as u64,
                output_tokens: 0,
                total_usd: 0.0,
                basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
            }),
            nodes: Vec::new(),
            hooks: Vec::new(),
        };
        let mut budgets = RunHistoryBudgets::default();

        budgets.record_accounting(
            &mut accounting,
            NodeCostRecord {
                node: "node".to_string(),
                step: 1,
                item_id: "directive:test/item".to_string(),
                cost: RuntimeCost {
                    input_tokens: 1,
                    output_tokens: 0,
                    total_usd: 0.0,
                    basis: None,
                },
            },
        );

        assert!(accounting.nodes.is_empty());
        assert!(budgets
            .failure()
            .is_some_and(|error| error.contains("settlement storage maximum")));
    }

    #[test]
    fn accounting_rejects_negative_spend_without_admitting_the_record() {
        let mut accounting = GraphAccounting::default();
        let mut budgets = RunHistoryBudgets::default();

        budgets.record_accounting(
            &mut accounting,
            NodeCostRecord {
                node: "node".to_string(),
                step: 1,
                item_id: "directive:test/item".to_string(),
                cost: RuntimeCost {
                    input_tokens: 1,
                    output_tokens: 1,
                    total_usd: -0.01,
                    basis: None,
                },
            },
        );

        assert!(accounting.total.is_none());
        assert!(accounting.nodes.is_empty());
        assert!(budgets
            .failure()
            .is_some_and(|error| error.contains("must be non-negative")));
    }

    #[test]
    fn receipts_are_validated_independently_without_accumulation() {
        let mut budgets = RunHistoryBudgets::default();
        let mut receipt = NodeReceipt {
            node: "node".to_string(),
            step: 1,
            definition_ref: "graph:test/example".to_string(),
            definition_hash: "sha256:definition".to_string(),
            result_hash: None,
            cache_hit: false,
            elapsed_ms: 1,
            error: None,
            cost: None,
            fanout: None,
        };

        assert!(budgets.accept_receipt(&receipt));
        assert!(budgets.accept_receipt(&receipt));
        assert!(budgets.failure().is_none());

        receipt.error = Some("x".repeat(EvaluationLimits::default().max_scalar_bytes + 1));
        assert!(!budgets.accept_receipt(&receipt));
        assert!(budgets.failure().is_some());
    }

    #[test]
    fn receipt_rejects_invalid_cost_before_shape_admission() {
        let mut budgets = RunHistoryBudgets::default();
        let receipt = NodeReceipt {
            node: "node".to_string(),
            step: 1,
            definition_ref: "graph:test/example".to_string(),
            definition_hash: "sha256:definition".to_string(),
            result_hash: None,
            cache_hit: false,
            elapsed_ms: 1,
            error: None,
            cost: Some(RuntimeCost {
                input_tokens: 1,
                output_tokens: 1,
                total_usd: -0.01,
                basis: None,
            }),
            fanout: None,
        };

        assert!(!budgets.accept_receipt(&receipt));
        assert!(budgets
            .failure()
            .is_some_and(|error| error.contains("must be non-negative")));
    }
}
