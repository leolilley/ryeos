use crate::model::NodeCostRecord;
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
