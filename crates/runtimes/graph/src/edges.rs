use serde_json::{json, Value};

use ryeos_runtime::{EvaluationContext, EvaluationLimits, EvaluationSession, ExpressionError};

use crate::compiled_graph::{CompiledCondition, CompiledEdgeSpec, CompiledNode};

pub(crate) fn evaluate_next(
    node: &CompiledNode,
    state: &Value,
    inputs: &Value,
    execution: Option<&Value>,
    graph_run_id: Option<&str>,
) -> Result<Option<String>, ExpressionError> {
    evaluate_next_with_optional_result(node, state, inputs, None, execution, graph_run_id)
}
pub(crate) fn evaluate_next_with_result(
    node: &CompiledNode,
    state: &Value,
    inputs: &Value,
    result: &Value,
    execution: Option<&Value>,
    graph_run_id: Option<&str>,
) -> Result<Option<String>, ExpressionError> {
    evaluate_next_with_optional_result(node, state, inputs, Some(result), execution, graph_run_id)
}

fn evaluate_next_with_optional_result(
    node: &CompiledNode,
    state: &Value,
    inputs: &Value,
    result: Option<&Value>,
    execution: Option<&Value>,
    graph_run_id: Option<&str>,
) -> Result<Option<String>, ExpressionError> {
    let Some(edge) = &node.next else {
        return Ok(None);
    };
    match edge {
        CompiledEdgeSpec::Unconditional { to } => Ok(Some(to.clone())),
        CompiledEdgeSpec::Conditional { branches, .. } => {
            let run = graph_run_id.map(|id| json!({"graph_run_id": id}));
            let mut context = EvaluationContext::new()
                .with_root("state", state)
                .with_root("inputs", inputs);
            if let Some(result) = result {
                context.insert("result", result);
            }
            if let Some(execution) = execution {
                context.insert("_execution", execution);
            }
            if let Some(run) = run.as_ref() {
                context.insert("_run", run);
            }
            let limits = EvaluationLimits::default();
            let mut session = EvaluationSession::with_context(&context, &limits);
            let mut default = None;

            for branch in branches {
                let matches = match &branch.condition {
                    CompiledCondition::Default => {
                        default = Some(branch.to.clone());
                        continue;
                    }
                    CompiledCondition::Constant(value) => *value,
                    CompiledCondition::Expression(expression) => {
                        session.evaluate_bool(expression)?
                    }
                };
                if matches {
                    return Ok(Some(branch.to.clone()));
                }
            }
            Ok(default)
        }
    }
}
