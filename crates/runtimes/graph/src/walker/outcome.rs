use serde_json::Value;

use crate::cache::NodeCache;
use crate::context;
use crate::model::*;
use ryeos_runtime::envelope::RuntimeCost;

pub(super) fn add_runtime_cost(total: &mut Option<RuntimeCost>, cost: Option<RuntimeCost>) {
    let Some(cost) = cost else { return };
    let acc = total.get_or_insert(RuntimeCost {
        input_tokens: 0,
        output_tokens: 0,
        total_usd: 0.0,
        basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
    });
    acc.input_tokens += cost.input_tokens;
    acc.output_tokens += cost.output_tokens;
    acc.total_usd += cost.total_usd;
}

/// Running cost accumulator for a single graph execution.
///
/// The aggregate is a rollup: child cost remains attributable to the child
/// thread and is also included here for parent graph reporting.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct GraphAccounting {
    pub(super) total: Option<RuntimeCost>,
    pub(super) nodes: Vec<NodeCostRecord>,
}

impl GraphAccounting {
    pub(super) fn record(&mut self, node: &str, step: u32, item_id: &str, cost: RuntimeCost) {
        let total = self.total.get_or_insert(RuntimeCost {
            input_tokens: 0,
            output_tokens: 0,
            total_usd: 0.0,
            basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
        });
        total.input_tokens += cost.input_tokens;
        total.output_tokens += cost.output_tokens;
        total.total_usd += cost.total_usd;
        self.nodes.push(NodeCostRecord {
            node: node.to_string(),
            step,
            item_id: item_id.to_string(),
            cost,
        });
    }
}

pub(super) enum StepOutcome {
    ActionOk {
        item_id: String,
        result: Value,
        assign: Option<Value>,
        next: Option<String>,
        cache_hit: bool,
        elapsed_ms: u64,
        cost: Option<RuntimeCost>,
    },
    LeafSoftError {
        item_id: String,
        error: String,
        next_on_error: NextOnError,
        elapsed_ms: u64,
        cost: Option<RuntimeCost>,
    },
    DispatchHardError {
        item_id: Option<String>,
        error: String,
        next_on_error: NextOnError,
        elapsed_ms: u64,
        cost: Option<RuntimeCost>,
    },
    GateTaken { target: Option<String> },
    ForeachDone {
        results: Vec<Value>,
        collect_key: Option<String>,
        var_name: String,
        assign_delta: Value,
        errors: Vec<ErrorRecord>,
        next: Option<String>,
        item_id: String,
        cost: Option<RuntimeCost>,
    },
    FollowSuspend { item_id: String, params: Value },
    FollowFanoutSuspend {
        children: Vec<ryeos_runtime::callback::FollowChildSpec>,
        width: Option<u32>,
        iteration_snapshot: Vec<Value>,
    },
    FollowFanoutDone {
        results: Vec<Value>,
        statuses: Vec<String>,
        errors: Vec<ErrorRecord>,
        assign_delta: Value,
        collect_key: Option<String>,
        var_name: String,
        item_id: String,
        next: Option<String>,
        next_on_error: NextOnError,
        cost: Option<RuntimeCost>,
        elapsed_ms: u64,
    },
    RetryScheduled {
        item_id: String,
        error: String,
        failed_attempt: u32,
        total_attempts: u32,
        delay_ms: u64,
        elapsed_ms: u64,
        cost: Option<RuntimeCost>,
    },
    Terminal {
        status: &'static str,
        error: Option<String>,
    },
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
    pub(super) receipts: &'a mut Vec<NodeReceipt>,
    pub(super) suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub(super) outcome: StepOutcome,
    pub(super) guard: &'a mut RunGuard,
    pub(super) inputs: &'a Value,
    pub(super) execution: &'a Value,
}

pub(super) struct CommitTerminalInput<'a> {
    pub(super) graph_run_id: &'a str,
    pub(super) steps: u32,
    pub(super) state: &'a mut Value,
    pub(super) suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub(super) base_status: &'a str,
    pub(super) error: Option<&'a str>,
    pub(super) guard: &'a mut RunGuard,
    pub(super) current_node_id: &'a str,
    pub(super) inputs: &'a Value,
    pub(super) execution: &'a Value,
}
