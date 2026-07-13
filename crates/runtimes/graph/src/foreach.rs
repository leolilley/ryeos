use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::sync::Semaphore;

use crate::context::ExecutionContext;
use crate::model::{ErrorRecord, GraphNode, RetryConfig, WalkContext};
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::RuntimeCost;
use ryeos_runtime::events::RuntimeEventType;

/// Fold one iteration's reported cost into the foreach node's running
/// aggregate. The first cost-bearing iteration seeds the total so a
/// foreach over pure tools stays `None`.
fn add_cost(acc: &mut Option<RuntimeCost>, cost: Option<RuntimeCost>) {
    let Some(c) = cost else { return };
    let total = acc.get_or_insert(RuntimeCost {
        input_tokens: 0,
        output_tokens: 0,
        total_usd: 0.0,
        basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
    });
    total.input_tokens += c.input_tokens;
    total.output_tokens += c.output_tokens;
    total.total_usd += c.total_usd;
}

/// Stamp the bounded-fanout launch window onto a detach action. A foreach
/// with `detach: true` spawns children whose dispatch returns immediately,
/// so `max_concurrency` bounds LIVE children daemon-side (the launch
/// window), not merely concurrent dispatch tasks. Keyed per fanout
/// (`graph_run_id:node`); the daemon namespaces it under the parent thread.
fn fold_launch_window(
    node: &GraphNode,
    action: &mut Value,
    graph_run_id: &str,
    current_node: &str,
) {
    if !node.detach {
        return;
    }
    let Some(width) = node.max_concurrency else {
        return;
    };
    if let Some(obj) = action.as_object_mut() {
        obj.insert(
            ryeos_runtime::callback::action_keys::LAUNCH_WINDOW.to_string(),
            serde_json::json!({
                "key": format!("{graph_run_id}:{current_node}"),
                "width": width,
            }),
        );
    }
}

/// Outcome of running every iteration of a foreach node.
///
/// The runner does NOT mutate the graph's `suppressed_errors` or `state`
/// directly. It returns per-item `errors` (so the caller can apply the
/// node/graph `on_error` policy) and an accumulated `assign_delta` (so
/// the caller can commit foreach `assign` mutations into real state —
/// the runner only ever sees a clone).
pub struct ForeachRun {
    /// One entry per input item, index-aligned (`Null` placeholder for a
    /// failed/skipped item).
    pub results: Vec<Value>,
    /// Per-item failures (interpolation, dispatch, leaf, assign).
    pub errors: Vec<ErrorRecord>,
    /// Accumulated `assign` mutations to merge into graph state, as a
    /// single object. Empty object when the node has no `assign`.
    pub assign_delta: Value,
    /// Aggregate cost across every iteration's native child, if any.
    pub cost: Option<RuntimeCost>,
}

pub struct ForeachContext<'a> {
    pub items: &'a [Value],
    pub var: &'a str,
    pub node: &'a GraphNode,
    pub thread_id: &'a str,
    pub project_path: &'a str,
    pub client: &'a CallbackClient,
    pub exec_ctx: Option<&'a ExecutionContext>,
    pub step: u32,
    pub current_node: &'a str,
    pub graph_run_id: &'a str,
    pub definition_ref: &'a str,
    pub definition_hash: &'a str,
}

/// Immutable event context for a foreach node's braid-visible per-item retry
/// events. Cloned into each parallel task so a spawned iteration can emit its
/// own retry milestones.
#[derive(Clone)]
struct RetryEventCtx {
    graph_run_id: String,
    definition_ref: String,
    definition_hash: String,
    node: String,
    step: u32,
}

/// Dispatch one foreach item, retrying only a dispatch-level or leaf failure
/// explicitly classified retryable, within the node's attempt budget. Unlike
/// the single-action path, a foreach's per-item retries run inside this one
/// walker step (they do NOT consume walker steps and are not individually
/// checkpointed); each item keeps its own attempt count. Every re-attempt
/// emits a braid-visible `graph_node_retry` event, then sleeps the backoff.
async fn dispatch_item_with_retry(
    client: &CallbackClient,
    action: &Value,
    thread_id: &str,
    project_path: &str,
    exec_ctx: Option<&ExecutionContext>,
    retry: Option<&RetryConfig>,
    ev: &RetryEventCtx,
    item_id: &str,
) -> anyhow::Result<crate::dispatch::ActionOutcome> {
    let total = retry.map(|r| r.attempts).unwrap_or(1);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let outcome =
            crate::dispatch::dispatch_action(client, action, thread_id, project_path, exec_ctx)
                .await;
        let retryable = match &outcome {
            Err(error) => error.retryable,
            Ok(crate::dispatch::ActionOutcome::Failure(failure)) => failure.retryable,
            Ok(crate::dispatch::ActionOutcome::Success(_)) => false,
        };
        if !retryable || attempt >= total {
            return outcome;
        }
        let rc = retry.expect("retry policy present when total attempts > 1");
        let diagnostic = match &outcome {
            Err(e) => format!("{e:#}"),
            Ok(crate::dispatch::ActionOutcome::Failure(f)) => f.diagnostic.clone(),
            _ => String::new(),
        };
        let delay = rc.delay_ms(attempt);
        // Fire-and-forget observability: a failed callback here must not abort
        // the item's own retry loop.
        let _ = client
            .append_runtime_event(
                RuntimeEventType::GraphNodeRetry,
                Value::Object(
                    [
                        (
                            "graph_run_id".to_string(),
                            Value::String(ev.graph_run_id.clone()),
                        ),
                        (
                            "definition_ref".to_string(),
                            Value::String(ev.definition_ref.clone()),
                        ),
                        (
                            "definition_hash".to_string(),
                            Value::String(ev.definition_hash.clone()),
                        ),
                        ("node".to_string(), Value::String(ev.node.clone())),
                        (
                            "node_ref".to_string(),
                            Value::String(format!("{}#node:{}", ev.definition_ref, ev.node)),
                        ),
                        ("step".to_string(), Value::from(ev.step)),
                        ("item_id".to_string(), Value::String(item_id.to_string())),
                        ("attempt".to_string(), Value::from(attempt)),
                        ("attempts".to_string(), Value::from(total)),
                        ("delay_ms".to_string(), Value::from(delay)),
                        ("error".to_string(), Value::String(diagnostic)),
                        ("foreach".to_string(), Value::Bool(true)),
                    ]
                    .into_iter()
                    .collect(),
                ),
            )
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
}

pub async fn run_foreach_sequential(
    ctx: ForeachContext<'_>,
    state: &Value,
    inputs: &Value,
) -> ForeachRun {
    let ForeachContext {
        items,
        var,
        node,
        thread_id,
        project_path,
        client,
        exec_ctx,
        step,
        current_node,
        graph_run_id,
        definition_ref,
        definition_hash,
    } = ctx;
    let retry_ev = RetryEventCtx {
        graph_run_id: graph_run_id.to_string(),
        definition_ref: definition_ref.to_string(),
        definition_hash: definition_hash.to_string(),
        node: current_node.to_string(),
        step,
    };
    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut total_cost: Option<RuntimeCost> = None;
    // Accumulated assign deltas. Each iteration sees base state + the
    // deltas applied so far, so a later item can read an earlier item's
    // assign — but the mutations land in real state only via the caller.
    let mut delta: Map<String, Value> = Map::new();

    for item in items {
        // Effective state = base state with accumulated deltas applied.
        let mut effective_state = state.clone();
        merge_into(&mut effective_state, &Value::Object(delta.clone()));
        let walk_ctx = WalkContext {
            state: effective_state,
            inputs: inputs.clone(),
            result: None,
            execution: exec_ctx.map(|ctx| ctx.as_context_value()),
            graph_run_id: Some(graph_run_id.to_string()),
        };
        let item_ctx_val = walk_ctx.with_foreach_item(var, item);

        let mut action = match &node.action {
            Some(a) => a.clone(),
            None => continue,
        };
        // Fold BEFORE interpolation so per-item facet templates resolve with
        // the iteration variable in scope.
        node.fold_detach_into_action(&mut action);
        fold_launch_window(node, &mut action, graph_run_id, current_node);

        let interpolated = match ryeos_runtime::interpolate_action(&action, &item_ctx_val) {
            Ok(v) => v,
            Err(e) => {
                // Never dispatch a raw `${...}` template — record the
                // error and skip this item with an aligned placeholder.
                errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!("interpolation error in foreach action: {e:#}"),
                });
                results.push(Value::Null);
                continue;
            }
        };
        let stripped = strip_none_values(&interpolated);
        let item_dispatch_id = stripped
            .get("item_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match dispatch_item_with_retry(
            client,
            &stripped,
            thread_id,
            project_path,
            exec_ctx,
            node.retry.as_ref(),
            &retry_ev,
            &item_dispatch_id,
        )
        .await
        {
            Ok(crate::dispatch::ActionOutcome::Success(success)) => {
                let crate::dispatch::ActionSuccess {
                    result: val,
                    cost,
                    child_thread_id,
                } = success;
                // Same portable dispatch-lineage contract as the plain action
                // path — see the parallel runner for the rationale.
                if let Some(ref child_id) = child_thread_id {
                    let _ = client
                        .append_runtime_event(
                            RuntimeEventType::ChildThreadSpawned,
                            serde_json::json!({
                                "child_thread_id": child_id,
                                "node": retry_ev.node,
                                "step": retry_ev.step,
                                "item_id": item_dispatch_id,
                                "spawn_reason": "dispatch",
                            }),
                        )
                        .await;
                }
                add_cost(&mut total_cost, cost);
                // Interpolate assign BEFORE committing the result, so an
                // assign failure makes this item a Null/error — matching
                // the parallel path's per-item failure semantics.
                if let Some(ref assign) = node.assign {
                    let mut assign_ctx_map = item_ctx_val.as_object().cloned().unwrap_or_default();
                    assign_ctx_map.insert("result".into(), val.clone());
                    let assign_ctx = Value::Object(assign_ctx_map);
                    match ryeos_runtime::interpolate(assign, &assign_ctx) {
                        Ok(interpolated) => {
                            merge_object_into(&mut delta, &interpolated);
                            results.push(val);
                        }
                        Err(e) => {
                            errors.push(ErrorRecord {
                                step,
                                node: current_node.to_string(),
                                error: format!("interpolation error in foreach assign: {e:#}"),
                            });
                            results.push(Value::Null);
                        }
                    }
                } else {
                    results.push(val);
                }
            }
            Ok(crate::dispatch::ActionOutcome::Failure(failure)) => {
                // Leaf ran but failed — a failed native child may still have
                // spent tokens, so fold its cost in before recording the
                // diagnostic and a null placeholder (indices stay aligned).
                add_cost(&mut total_cost, failure.cost);
                errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!(
                        "foreach sequential iteration failed: {}",
                        failure.diagnostic
                    ),
                });
                results.push(Value::Null);
            }
            Err(e) => {
                // Dispatch failed before the leaf returned anything.
                errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!("foreach sequential iteration dispatch failed: {e:#}"),
                });
                results.push(Value::Null);
            }
        }
    }
    ForeachRun {
        results,
        errors,
        assign_delta: Value::Object(delta),
        cost: total_cost,
    }
}

/// Per-item result of a parallel foreach task: the leaf result, the
/// (already interpolated) assign object for that item, and the iteration's
/// reported cost — or a diagnostic.
type ParallelItem =
    Result<(Value, Option<Value>, Option<RuntimeCost>), (String, Option<RuntimeCost>)>;

pub async fn run_foreach_parallel(
    ctx: ForeachContext<'_>,
    state: &Value,
    inputs: &Value,
    client: CallbackClient,
    exec_ctx: Arc<ExecutionContext>,
) -> ForeachRun {
    let ForeachContext {
        items,
        var,
        node,
        thread_id,
        project_path,
        client: _client_ref,
        exec_ctx: _exec_ctx_ref,
        step,
        current_node,
        graph_run_id,
        definition_ref,
        definition_hash,
    } = ctx;
    let retry_ev = RetryEventCtx {
        graph_run_id: graph_run_id.to_string(),
        definition_ref: definition_ref.to_string(),
        definition_hash: definition_hash.to_string(),
        node: current_node.to_string(),
        step,
    };
    let retry_cfg = node.retry.clone();
    let max_conc = node.max_concurrency.unwrap_or(8);
    let sem = Arc::new(Semaphore::new(max_conc));
    let mut handles = Vec::new();

    for item in items {
        let permit = sem.clone().acquire_owned().await.unwrap();
        // Parallel iterations are independent: each sees base state +
        // inputs + its own item var. No cross-item delta visibility.
        let walk_ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
            execution: Some(exec_ctx.as_context_value()),
            graph_run_id: Some(graph_run_id.to_string()),
        };
        let item_ctx_val = walk_ctx.with_foreach_item(var, item);
        let mut action = match &node.action {
            Some(a) => a.clone(),
            None => {
                drop(permit);
                continue;
            }
        };
        // Fold BEFORE interpolation so per-item facet templates resolve with
        // the iteration variable in scope.
        node.fold_detach_into_action(&mut action);
        fold_launch_window(node, &mut action, graph_run_id, current_node);

        // Interpolate before spawning. On failure, push an immediate-error
        // task (NOT a raw-template dispatch) so handle ordering — and thus
        // result index alignment — is preserved.
        let interpolated = match ryeos_runtime::interpolate_action(&action, &item_ctx_val) {
            Ok(v) => v,
            Err(e) => {
                drop(permit);
                let diagnostic = format!("interpolation error in foreach action: {e:#}");
                // Interpolation failed before dispatch — no cost.
                handles.push(tokio::spawn(async move {
                    ParallelItem::Err((diagnostic, None))
                }));
                continue;
            }
        };

        let client = client.clone();
        let thread_id = thread_id.to_string();
        let project_path = project_path.to_string();
        let exec_ctx = exec_ctx.clone();
        let assign = node.assign.clone();
        let retry_cfg = retry_cfg.clone();
        let retry_ev = retry_ev.clone();
        // Full item context (state + inputs + item var) so assign resolves
        // the same way the sequential path does.
        let assign_ctx_base = item_ctx_val.clone();

        let handle = tokio::spawn(async move {
            let _permit = permit;
            let stripped = strip_none_values(&interpolated);
            let item_dispatch_id = stripped
                .get("item_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match dispatch_item_with_retry(
                &client,
                &stripped,
                &thread_id,
                &project_path,
                Some(&exec_ctx),
                retry_cfg.as_ref(),
                &retry_ev,
                &item_dispatch_id,
            )
            .await
            {
                Ok(crate::dispatch::ActionOutcome::Success(success)) => {
                    let crate::dispatch::ActionSuccess {
                        result: val,
                        cost,
                        child_thread_id,
                    } = success;
                    // Same portable dispatch-lineage contract as the plain
                    // action path: a spawned native child lands in the parent's
                    // braid as `child_thread_spawned`, so the fanout cohort is
                    // drillable from the parent (fire-and-forget append — a
                    // callback failure must not fail the iteration).
                    if let Some(ref child_id) = child_thread_id {
                        let _ = client
                            .append_runtime_event(
                                RuntimeEventType::ChildThreadSpawned,
                                serde_json::json!({
                                    "child_thread_id": child_id,
                                    "node": retry_ev.node,
                                    "step": retry_ev.step,
                                    "item_id": item_dispatch_id,
                                    "spawn_reason": "dispatch",
                                }),
                            )
                            .await;
                    }
                    let assign_val = if let Some(ref assign_expr) = assign {
                        let mut assign_ctx_map =
                            assign_ctx_base.as_object().cloned().unwrap_or_default();
                        assign_ctx_map.insert("result".into(), val.clone());
                        let assign_ctx = Value::Object(assign_ctx_map);
                        match ryeos_runtime::interpolate(assign_expr, &assign_ctx) {
                            Ok(v) => Some(v),
                            Err(e) => {
                                // Child succeeded (with cost); only assign
                                // failed — carry the cost through.
                                return ParallelItem::Err((
                                    format!("interpolation error in foreach assign: {e:#}"),
                                    cost,
                                ));
                            }
                        }
                    } else {
                        None
                    };
                    ParallelItem::Ok((val, assign_val, cost))
                }
                Ok(crate::dispatch::ActionOutcome::Failure(failure)) => ParallelItem::Err((
                    format!("foreach parallel iteration failed: {}", failure.diagnostic),
                    failure.cost,
                )),
                Err(e) => ParallelItem::Err((
                    format!("foreach parallel iteration dispatch failed: {e:#}"),
                    None,
                )),
            }
        });
        handles.push(handle);
    }

    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut total_cost: Option<RuntimeCost> = None;
    // Merge assign deltas in input order for determinism.
    let mut delta: Map<String, Value> = Map::new();
    for handle in handles {
        match handle.await {
            Ok(Ok((val, assign_val, cost))) => {
                add_cost(&mut total_cost, cost);
                results.push(val);
                if let Some(obj) = assign_val {
                    merge_object_into(&mut delta, &obj);
                }
            }
            Ok(Err((diagnostic, cost))) => {
                // Leaf, transport, or interpolation failure — fold any cost
                // the child spent before failing, then record the diagnostic
                // and a null placeholder so indices stay aligned.
                add_cost(&mut total_cost, cost);
                errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: diagnostic,
                });
                results.push(Value::Null);
            }
            Err(join_err) => {
                errors.push(ErrorRecord {
                    step,
                    node: current_node.to_string(),
                    error: format!("foreach parallel iteration task panicked: {join_err}"),
                });
                results.push(Value::Null);
            }
        }
    }
    ForeachRun {
        results,
        errors,
        assign_delta: Value::Object(delta),
        cost: total_cost,
    }
}

fn merge_into(target: &mut Value, source: &Value) {
    if let (Value::Object(ref mut t_map), Value::Object(ref s_map)) = (target, source) {
        for (k, v) in s_map {
            t_map.insert(k.clone(), v.clone());
        }
    }
}

/// Merge an interpolated `assign` value into the accumulating delta map.
/// Non-object assign results are ignored (assign must yield an object).
fn merge_object_into(delta: &mut Map<String, Value>, source: &Value) {
    if let Value::Object(map) = source {
        for (k, v) in map {
            delta.insert(k.clone(), v.clone());
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
        Value::Array(arr) => Value::Array(arr.iter().map(strip_none_values).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detach_node(max_concurrency: Option<usize>) -> GraphNode {
        let mut node: GraphNode = serde_yaml::from_str(
            "node_type: foreach\nover: \"${state.items}\"\ndetach: true\nparallel: true\naction: {item_id: \"graph:t/leaf\"}\n",
        )
        .unwrap();
        node.max_concurrency = max_concurrency;
        node
    }

    #[test]
    fn launch_window_stamped_for_detach_with_max_concurrency() {
        let node = detach_node(Some(12));
        let mut action = node.action.clone().unwrap();
        node.fold_detach_into_action(&mut action);
        fold_launch_window(&node, &mut action, "gr-1", "fan");
        assert_eq!(
            action["launch_window"],
            json!({ "key": "gr-1:fan", "width": 12 })
        );
        assert_eq!(action["thread"], json!("detached"));
    }

    #[test]
    fn no_window_without_max_concurrency_or_detach() {
        let node = detach_node(None);
        let mut action = node.action.clone().unwrap();
        node.fold_detach_into_action(&mut action);
        fold_launch_window(&node, &mut action, "gr-1", "fan");
        assert!(action.get("launch_window").is_none());

        let mut inline_node = detach_node(Some(12));
        inline_node.detach = false;
        let mut action = inline_node.action.clone().unwrap();
        inline_node.fold_detach_into_action(&mut action);
        fold_launch_window(&inline_node, &mut action, "gr-1", "fan");
        assert!(action.get("launch_window").is_none());
    }
}
