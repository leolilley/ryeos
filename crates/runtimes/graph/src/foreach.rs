use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Semaphore;

use crate::context::ExecutionContext;
use crate::model::{ErrorRecord, GraphNode, WalkContext};
use ryeos_runtime::callback_client::CallbackClient;

pub struct ForeachContext<'a> {
    pub items: &'a [Value],
    pub var: &'a str,
    pub node: &'a GraphNode,
    pub thread_id: &'a str,
    pub project_path: &'a str,
    pub client: &'a CallbackClient,
    pub exec_ctx: Option<&'a ExecutionContext>,
    pub suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub step: u32,
    pub current_node: &'a str,
}

pub async fn run_foreach_sequential(
    ctx: ForeachContext<'_>,
    state: &mut Value,
    inputs: &Value,
) -> Vec<Value> {
    let ForeachContext {
        items,
        var,
        node,
        thread_id,
        project_path,
        client,
        exec_ctx,
        suppressed_errors,
        step,
        current_node,
    } = ctx;
    let mut results = Vec::new();
    for item in items {
        let walk_ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
        };
        let item_ctx_val = walk_ctx.with_foreach_item(var, item);

        let action = match &node.action {
            Some(a) => a.clone(),
            None => continue,
        };

        let interpolated = match ryeos_runtime::interpolate_action(&action, &item_ctx_val) {
            Ok(v) => v,
            Err(e) => {
                suppressed_errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!("interpolation error in foreach action: {e:#}"),
                });
                action.clone()
            }
        };
        let stripped = strip_none_values(&interpolated);

        if let Ok(val) = crate::dispatch::dispatch_action(client, &stripped, thread_id, project_path, exec_ctx).await {
            // Typed contract: dispatch_action returns the leaf result
            // directly; no `{status, data}` unwrap step.
            results.push(val.clone());

            if let Some(ref assign) = node.assign {
                let mut assign_ctx_map = item_ctx_val.as_object().cloned().unwrap_or_default();
                assign_ctx_map.insert("result".into(), val);
                let assign_ctx = Value::Object(assign_ctx_map);
                match ryeos_runtime::interpolate(assign, &assign_ctx) {
                    Ok(interpolated) => merge_into(state, &interpolated),
                    Err(e) => {
                        suppressed_errors.push(ErrorRecord {
                            step,
                            node: current_node.to_string(),
                            error: format!("interpolation error in foreach assign: {e:#}"),
                        });
                    }
                }
            }
        } else {
            // Dispatch failed — record error and push null placeholder
            // to keep result indices aligned with input items (matching
            // parallel path semantics).
            suppressed_errors.push(ErrorRecord {
                step,
                node: current_node.to_string(),
                error: format!("foreach sequential iteration dispatch failed"),
            });
            results.push(Value::Null);
        }
    }
    results
}

pub async fn run_foreach_parallel(
    ctx: ForeachContext<'_>,
    state: &Value,
    inputs: &Value,
    client: CallbackClient,
    exec_ctx: Arc<ExecutionContext>,
) -> Vec<Value> {
    let ForeachContext {
        items,
        var,
        node,
        thread_id,
        project_path,
        client: _client_ref,
        exec_ctx: _exec_ctx_ref,
        suppressed_errors,
        step,
        current_node,
    } = ctx;
    let max_conc = node.max_concurrency.unwrap_or(8);
    let sem = Arc::new(Semaphore::new(max_conc));
    let mut handles = Vec::new();

    for item in items {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let walk_ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
        };
        let item_ctx_val = walk_ctx.with_foreach_item(var, item);
        let action = match &node.action {
            Some(a) => a.clone(),
            None => {
                drop(permit);
                continue;
            }
        };

        // Interpolate before spawning — errors go to suppressed_errors
        // rather than being silently swallowed inside the spawned task.
        let interpolated = match ryeos_runtime::interpolate_action(&action, &item_ctx_val) {
            Ok(v) => v,
            Err(e) => {
                suppressed_errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!("interpolation error in foreach action: {e:#}"),
                });
                action.clone()
            }
        };

        let client = client.clone();
        let thread_id = thread_id.to_string();
        let project_path = project_path.to_string();
        let exec_ctx = exec_ctx.clone();
        let assign = node.assign.clone();
        let item_owned = item.clone();
        let step_owned = step;
        let current_node_owned = current_node.to_string();

        let handle = tokio::spawn(async move {
            let _permit = permit;
            let stripped = strip_none_values(&interpolated);
            // Typed contract: dispatch_action already returns the leaf
            // result directly; no `{status, data}` unwrap step.
            match crate::dispatch::dispatch_action(&client, &stripped, &thread_id, &project_path, Some(&exec_ctx)).await {
                Ok(val) => {
                    // Perform node.assign (same as sequential path).
                    if let Some(ref assign_expr) = assign {
                        let mut assign_ctx_map = item_owned
                            .as_object()
                            .cloned()
                            .unwrap_or_default();
                        assign_ctx_map.insert("result".into(), val.clone());
                        let assign_ctx = Value::Object(assign_ctx_map);
                        if let Err(e) = ryeos_runtime::interpolate(assign_expr, &assign_ctx) {
                            tracing::warn!(
                                step = step_owned,
                                node = %current_node_owned,
                                "foreach parallel iteration assign interpolation failed: {e:#}"
                            );
                        }
                    }
                    Ok(val)
                }
                Err(e) => Err(e),
            }
        });
        handles.push(handle);
    }

    let mut results = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(Ok(val)) => {
                results.push(val);
            }
            Ok(Err(dispatch_err)) => {
                // Dispatch failed — record in suppressed_errors (same
                // semantics as sequential path) and push a null placeholder
                // so result indices stay aligned with input items.
                suppressed_errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!("foreach parallel iteration dispatch failed: {dispatch_err:#}"),
                });
                results.push(Value::Null);
            }
            Err(join_err) => {
                suppressed_errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!("foreach parallel iteration task panicked: {join_err}"),
                });
                results.push(Value::Null);
            }
        }
    }
    results
}

fn merge_into(target: &mut Value, source: &Value) {
    if let (Value::Object(ref mut t_map), Value::Object(ref s_map)) = (target, source) {
        for (k, v) in s_map {
            t_map.insert(k.clone(), v.clone());
        }
    }
}

fn strip_none_values(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let cleaned: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_none_values(v)))
                .collect();
            Value::Object(cleaned)
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(strip_none_values).collect())
        }
        other => other.clone(),
    }
}
