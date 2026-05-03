use serde_json::Value;

use crate::model::{ConditionalEdge, EdgeSpec, GraphNode, WalkContext};

pub fn evaluate_next(node: &GraphNode, state: &Value, inputs: &Value) -> Option<String> {
    match &node.next {
        Some(EdgeSpec::Unconditional { to: target }) => Some(target.clone()),
        Some(EdgeSpec::Conditional { branches: edges }) => {
            let ctx = WalkContext {
                state: state.clone(),
                inputs: inputs.clone(),
                result: None,
            };
            evaluate_conditional_edges(edges, &ctx.as_context())
        }
        None => None,
    }
}

pub fn evaluate_next_with_result(
    node: &GraphNode,
    state: &Value,
    inputs: &Value,
    result: &Value,
) -> Option<String> {
    match &node.next {
        Some(EdgeSpec::Unconditional { to: target }) => Some(target.clone()),
        Some(EdgeSpec::Conditional { branches: edges }) => {
            let ctx = WalkContext {
                state: state.clone(),
                inputs: inputs.clone(),
                result: Some(result.clone()),
            };
            evaluate_conditional_edges(edges, &ctx.as_context())
        }
        None => None,
    }
}

fn evaluate_conditional_edges(edges: &[ConditionalEdge], context: &Value) -> Option<String> {
    let mut default = None;
    for edge in edges {
        match &edge.when {
            Some(condition) => {
                match ryeos_runtime::condition::matches(context, condition) {
                    Ok(true) => return Some(edge.to.clone()),
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(
                            edge_to = %edge.to,
                            "edge condition evaluation failed, skipping branch: {e:#}"
                        );
                    }
                }
            }
            None => {
                default = Some(edge.to.clone());
            }
        }
    }
    default
}
