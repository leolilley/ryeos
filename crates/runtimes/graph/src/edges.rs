use serde_json::Value;

use ryeos_runtime::{EvaluationContext, EvaluationLimits, EvaluationSession, ExpressionError};

use ryeos_graph_definition::{CompiledCondition, CompiledEdgeSpec, CompiledNode};

use crate::evaluation::GraphRunExpressionContext;

pub(crate) fn evaluate_next(
    node: &CompiledNode,
    state: &Value,
    inputs: &Value,
    execution: Option<&Value>,
    run: GraphRunExpressionContext<'_>,
) -> Result<Option<String>, ExpressionError> {
    evaluate_next_with_optional_result(node, state, inputs, None, execution, run, None)
}
pub(crate) fn evaluate_next_with_result(
    node: &CompiledNode,
    state: &Value,
    inputs: &Value,
    result: &Value,
    execution: Option<&Value>,
    run: GraphRunExpressionContext<'_>,
    dispatch: Option<&ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
) -> Result<Option<String>, ExpressionError> {
    evaluate_next_with_optional_result(node, state, inputs, Some(result), execution, run, dispatch)
}

fn evaluate_next_with_optional_result(
    node: &CompiledNode,
    state: &Value,
    inputs: &Value,
    result: Option<&Value>,
    execution: Option<&Value>,
    run: GraphRunExpressionContext<'_>,
    dispatch: Option<&ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
) -> Result<Option<String>, ExpressionError> {
    let Some(edge) = &node.next else {
        return Ok(None);
    };
    match edge {
        CompiledEdgeSpec::Unconditional { to } => Ok(Some(to.clone())),
        CompiledEdgeSpec::Conditional { branches, .. } => {
            let run = run.to_value();
            let mut context = EvaluationContext::new()
                .with_root("state", state)
                .with_root("inputs", inputs);
            if let Some(result) = result {
                context.insert("result", result);
            }
            if let Some(execution) = execution {
                context.insert("execution", execution);
            }
            context.insert("run", &run);
            let dispatch_value;
            if let Some(dispatch) = dispatch {
                dispatch_value = serde_json::to_value(dispatch)
                    .expect("typed dispatch evidence is infallibly serializable");
                context.insert("dispatch", &dispatch_value);
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
