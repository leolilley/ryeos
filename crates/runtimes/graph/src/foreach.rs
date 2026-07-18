use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::compiled_graph::CompiledNode;
use crate::context::ExecutionContext;
use crate::evaluation::{validate_runtime_value, ExpressionScope};
use crate::model::{DispatchObservation, ErrorRecord, GraphNode, GraphToolCallStatus, RetryConfig};
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::{RuntimeCost, RuntimeCostError};
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::{ExpressionError, RuntimeJsonArrayBudget, RuntimeJsonObjectBudget};

/// Fold one iteration's reported cost into the foreach node's running
/// aggregate. The first cost-bearing iteration seeds the total so a
/// foreach over pure tools stays `None`.
fn add_cost(
    aggregate: &mut Option<RuntimeCost>,
    cost: Option<RuntimeCost>,
) -> Result<(), RuntimeCostError> {
    let Some(cost) = cost else {
        return Ok(());
    };
    cost.validate()?;
    if let Some(aggregate) = aggregate.as_mut() {
        aggregate.checked_accumulate(&cost)?;
    } else {
        *aggregate = Some(RuntimeCost {
            input_tokens: cost.input_tokens,
            output_tokens: cost.output_tokens,
            total_usd: cost.total_usd,
            basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
        });
    }
    Ok(())
}

/// Keep a completed parallel handle from retaining an unbounded diagnostic
/// while it waits for an earlier input-order handle to be collected.
const PARALLEL_DIAGNOSTIC_LIMIT_FAILURE: &str =
    "parallel foreach failure diagnostic exceeded rye-expr/1 bounds";

fn bounded_parallel_diagnostic(diagnostic: String) -> String {
    let value = Value::String(diagnostic);
    match validate_runtime_value(&value, "parallel foreach failure diagnostic") {
        Ok(()) => match value {
            Value::String(diagnostic) => diagnostic,
            _ => "parallel foreach failure diagnostic had an invalid internal shape".to_string(),
        },
        Err(error) => format!("{PARALLEL_DIAGNOSTIC_LIMIT_FAILURE} and was discarded: {error}"),
    }
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

fn stamp_detached_operation(
    action: &mut Value,
    graph_run_id: &str,
    node: &str,
    step: u32,
    item_index: usize,
) {
    if action.get("thread").and_then(Value::as_str) != Some("detached") {
        return;
    }
    let identity = lillux::canonical_json(&serde_json::json!({
        "graph_run_id": graph_run_id,
        "node": node,
        "step": step,
        "item_index": item_index,
        "kind": "detached_foreach_action"
    }))
    .expect("fixed foreach operation identity is canonical JSON");
    action
        .as_object_mut()
        .expect("rendered foreach action is validated as an object")
        .insert(
            "operation_id".to_string(),
            Value::String(lillux::sha256_hex(identity.as_bytes())),
        );
}

/// Outcome of running every iteration of a foreach node.
///
/// The runner does NOT mutate the graph's `suppressed_errors` or `state`
/// directly. It returns per-item `errors` (so the caller can apply the
/// node/graph `on_error` policy) and an accumulated `assign_delta` (so
/// the caller can commit foreach `assign` mutations into real state —
/// the runner only ever sees a clone).
pub struct ForeachRun {
    /// One entry per attempted item, index-aligned with `statuses`, when no
    /// fatal aggregate/candidate limit is hit. Ordinary failed items retain a
    /// `Null` placeholder. A fatal limit invalidates the entire candidate, so
    /// the partial results vector is diagnostic only and is never committed.
    pub results: Vec<Value>,
    /// Typed outcome for every attempted iteration. This remains complete for
    /// already-dispatched work after a fatal aggregate error, even when the
    /// discarded partial `results` vector can no longer retain placeholders.
    /// String conversion happens only when events are emitted.
    pub statuses: Vec<GraphToolCallStatus>,
    /// Original cardinality, including sequential items not attempted after a
    /// fail/redirect policy stopped the node.
    pub total_items: usize,
    /// Per-item failures (expression, dispatch, leaf, assign).
    pub errors: Vec<ErrorRecord>,
    /// Accumulated `assign` mutations to merge into graph state, as a
    /// single object. Empty object when the node has no `assign`.
    pub assign_delta: Value,
    /// Aggregate cost across every iteration's native child, if any.
    pub cost: Option<RuntimeCost>,
    /// Lineage and milestone observations held until the caller has evaluated
    /// the foreach node's final branch successfully.
    pub observations: Vec<DispatchObservation>,
    /// Aggregate/candidate resource-limit failure. Unlike an ordinary item
    /// failure, this invalidates the whole node-local candidate and must route
    /// through integrity failure even when per-item policy is `continue`.
    pub limit_error: Option<String>,
    /// Non-fatal callback drift from per-item retry milestones. The walker
    /// folds these into the run's normal warning buffer after execution.
    pub callback_warnings: Vec<String>,
}

fn append_result(
    results: &mut Vec<Value>,
    budget: &mut RuntimeJsonArrayBudget,
    value: Value,
) -> Result<(), ExpressionError> {
    budget.append(&value)?;
    results.push(value);
    Ok(())
}

fn retain_error(
    errors: &mut Vec<ErrorRecord>,
    budget: &mut RuntimeJsonArrayBudget,
    error: ErrorRecord,
) -> Result<(), ExpressionError> {
    let encoded = serde_json::to_value(&error)
        .expect("ErrorRecord contains only infallibly serializable JSON fields");
    budget.append(&encoded)?;
    errors.push(error);
    Ok(())
}

fn retain_observation(
    observations: &mut Vec<DispatchObservation>,
    budget: &mut RuntimeJsonArrayBudget,
    observation: Option<DispatchObservation>,
) -> Result<(), ExpressionError> {
    let Some(observation) = observation else {
        return Ok(());
    };
    let encoded = serde_json::to_value(&observation)
        .expect("DispatchObservation contains only infallibly serializable JSON fields");
    budget.append(&encoded)?;
    observations.push(observation);
    Ok(())
}

const RETRY_WARNINGS_TRUNCATED: &str =
    "additional foreach retry callback warnings omitted after rye-expr/1 bounds were reached";

fn retain_retry_warnings(
    warnings: &mut Vec<String>,
    budget: &mut RuntimeJsonArrayBudget,
    incoming: Vec<String>,
) {
    if warnings
        .last()
        .is_some_and(|warning| warning == RETRY_WARNINGS_TRUNCATED)
    {
        return;
    }
    for warning in incoming {
        if budget.append(&Value::String(warning.clone())).is_err() {
            warnings.push(RETRY_WARNINGS_TRUNCATED.to_string());
            return;
        }
        warnings.push(warning);
    }
}

fn bounded_retry_warning(error: impl std::fmt::Display) -> String {
    let warning = format!("graph_node_retry callback failed: {error}");
    if validate_runtime_value(
        &Value::String(warning.clone()),
        "foreach retry callback warning",
    )
    .is_ok()
    {
        warning
    } else {
        "graph_node_retry callback failed with an oversized diagnostic".to_string()
    }
}

pub struct ForeachContext<'a> {
    pub items: &'a [Value],
    pub var: &'a str,
    pub node: &'a GraphNode,
    pub compiled: &'a CompiledNode,
    pub thread_id: &'a str,
    pub project_path: &'a str,
    pub client: &'a CallbackClient,
    pub exec_ctx: Option<&'a ExecutionContext>,
    pub step: u32,
    pub current_node: &'a str,
    pub graph_run_id: &'a str,
    pub definition_ref: &'a str,
    pub definition_hash: &'a str,
    pub continue_on_error: bool,
    pub cancel_flag: Option<Arc<AtomicBool>>,
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
// Retry plumbing: the policy, backoff, and per-item event context for one
// dispatch attempt, threaded verbatim from the foreach loop.
#[allow(clippy::too_many_arguments)]
async fn dispatch_item_with_retry(
    client: &CallbackClient,
    action: &Value,
    thread_id: &str,
    project_path: &str,
    exec_ctx: Option<&ExecutionContext>,
    retry: Option<&RetryConfig>,
    ev: &RetryEventCtx,
    item_id: &str,
    cancel_flag: Option<Arc<AtomicBool>>,
) -> RetriedDispatch {
    let mut callback_warnings = Vec::new();
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
            return RetriedDispatch {
                outcome,
                callback_warnings,
            };
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
        if let Err(error) = client
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
            .await
        {
            callback_warnings.push(bounded_retry_warning(format_args!("{error:#}")));
        }
        sleep_retry_delay(delay, cancel_flag.as_ref()).await;
        if cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            // Return the failed attempt already in hand. The foreach runner
            // records its cost/provenance, then stops launching new items; the
            // main loop observes the same flag at the next commit boundary.
            return RetriedDispatch {
                outcome,
                callback_warnings,
            };
        }
    }
}

struct RetriedDispatch {
    outcome: Result<crate::dispatch::ActionOutcome, crate::dispatch::ActionDispatchError>,
    callback_warnings: Vec<String>,
}

async fn sleep_retry_delay(delay_ms: u64, cancel_flag: Option<&Arc<AtomicBool>>) {
    let delay = tokio::time::sleep(std::time::Duration::from_millis(delay_ms));
    tokio::pin!(delay);
    let Some(flag) = cancel_flag.cloned() else {
        delay.await;
        return;
    };
    let cancelled = async move {
        while !flag.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    };
    tokio::pin!(cancelled);
    tokio::select! {
        _ = &mut delay => {}
        _ = &mut cancelled => {}
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
        compiled,
        thread_id,
        project_path,
        client,
        exec_ctx,
        step,
        current_node,
        graph_run_id,
        definition_ref,
        definition_hash,
        continue_on_error,
        cancel_flag,
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
    let mut statuses = Vec::new();
    let mut total_cost: Option<RuntimeCost> = None;
    let mut observations = Vec::new();
    let mut limit_error = None;
    let mut callback_warnings = Vec::new();
    let mut callback_warning_budget =
        RuntimeJsonArrayBudget::new(format!("node {current_node}.foreach retry warnings"));
    let mut result_budget =
        RuntimeJsonArrayBudget::new(format!("node {current_node}.foreach results"));
    let mut error_budget =
        RuntimeJsonArrayBudget::new(format!("node {current_node}.foreach errors"));
    let mut observation_budget =
        RuntimeJsonArrayBudget::new(format!("node {current_node}.foreach observations"));
    // Accumulated assign deltas. Each iteration sees base state + the
    // deltas applied so far, so a later item can read an earlier item's
    // assign — but the mutations land in real state only via the caller.
    let mut delta: Map<String, Value> = Map::new();
    if let Err(error) = validate_runtime_value(state, "foreach base state") {
        return ForeachRun {
            results,
            statuses,
            total_items: items.len(),
            errors,
            assign_delta: Value::Object(delta),
            cost: total_cost,
            observations,
            limit_error: Some(format!(
                "foreach node `{current_node}` received state outside rye-expr/1 bounds: {error}"
            )),
            callback_warnings,
        };
    }
    // One node-local candidate is sufficient: successful assignment deltas
    // accumulate into it, while failed iterations never mutate it. Cloning the
    // whole graph state once per item made work proportional to
    // `cardinality * state_size` outside the evaluator budget.
    let Some(base_object) = state.as_object() else {
        return ForeachRun {
            results,
            statuses,
            total_items: items.len(),
            errors,
            assign_delta: Value::Object(delta),
            cost: total_cost,
            observations,
            limit_error: Some(format!(
                "foreach node `{current_node}` received non-object graph state"
            )),
            callback_warnings,
        };
    };
    let mut candidate_budget = match RuntimeJsonObjectBudget::from_object(
        base_object,
        format!("node {current_node}.foreach candidate state"),
    ) {
        Ok(budget) => budget,
        Err(error) => {
            return ForeachRun {
                results,
                statuses,
                total_items: items.len(),
                errors,
                assign_delta: Value::Object(delta),
                cost: total_cost,
                observations,
                limit_error: Some(format!(
                    "foreach node `{current_node}` could not initialize its candidate-state budget: {error}"
                )),
                callback_warnings,
            };
        }
    };
    let mut candidate_state = state.clone();
    let execution = exec_ctx.map(ExecutionContext::as_context_value);

    for (item_index, item) in items.iter().enumerate() {
        if cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            break;
        }
        let scope = ExpressionScope::new(
            &candidate_state,
            inputs,
            execution.as_ref(),
            Some(graph_run_id),
        )
        .with_foreach(var, item);

        let action = match &compiled.action {
            Some(action) => action,
            None => continue,
        };
        let mut rendered = match scope.render_action(action) {
            Ok(v) => v,
            Err(error) => {
                statuses.push(GraphToolCallStatus::ExpressionFailed);
                if let Err(error) = retain_error(
                    &mut errors,
                    &mut error_budget,
                    ErrorRecord {
                        step,
                        node: current_node.to_string(),
                        error: format!("expression evaluation failed in foreach action: {error}"),
                    },
                ) {
                    limit_error = Some(format!(
                        "foreach node `{current_node}` error history exceeded rye-expr/1 aggregate bounds: {error}"
                    ));
                    break;
                }
                if let Err(error) = append_result(&mut results, &mut result_budget, Value::Null) {
                    limit_error = Some(format!(
                        "foreach node `{current_node}` could not retain a null result placeholder: {error}"
                    ));
                    break;
                }
                if !continue_on_error {
                    break;
                }
                continue;
            }
        };
        fold_launch_window(node, &mut rendered, graph_run_id, current_node);
        stamp_detached_operation(&mut rendered, graph_run_id, current_node, step, item_index);
        // Missing paths are handled by rye-expr/1 before this point. Explicit
        // JSON nulls are authored values and remain in the action payload.
        let item_dispatch_id = rendered
            .get("item_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let retried = dispatch_item_with_retry(
            client,
            &rendered,
            thread_id,
            project_path,
            exec_ctx,
            node.retry.as_ref(),
            &retry_ev,
            &item_dispatch_id,
            cancel_flag.clone(),
        )
        .await;
        retain_retry_warnings(
            &mut callback_warnings,
            &mut callback_warning_budget,
            retried.callback_warnings,
        );
        match retried.outcome {
            Ok(crate::dispatch::ActionOutcome::Success(success)) => {
                let crate::dispatch::ActionSuccess {
                    result: val,
                    cost,
                    child_thread_id,
                } = success;
                if let Err(error) = add_cost(&mut total_cost, cost) {
                    statuses.push(GraphToolCallStatus::IntegrityFailed);
                    let observation_error = retain_observation(
                        &mut observations,
                        &mut observation_budget,
                        DispatchObservation::child_only(item_dispatch_id.clone(), child_thread_id),
                    )
                    .err();
                    limit_error = Some(match observation_error {
                        Some(observation_error) => format!(
                            "foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {observation_error}"
                        ),
                        None => format!(
                            "foreach node `{current_node}` iteration reported invalid cost: {error}"
                        ),
                    });
                    break;
                }
                let result_budget_before = result_budget.clone();
                if let Err(error) = result_budget.append(&val) {
                    statuses.push(GraphToolCallStatus::IntegrityFailed);
                    let observation_error = retain_observation(
                        &mut observations,
                        &mut observation_budget,
                        DispatchObservation::child_only(item_dispatch_id.clone(), child_thread_id),
                    )
                    .err();
                    let history_error = retain_error(
                        &mut errors,
                        &mut error_budget,
                        ErrorRecord {
                            step,
                            node: current_node.to_string(),
                            error: format!(
                                "foreach result exceeded rye-expr/1 aggregate bounds: {error}"
                            ),
                        },
                    )
                    .err();
                    let placeholder = append_result(&mut results, &mut result_budget, Value::Null);
                    limit_error = Some(if let Some(observation_error) = observation_error {
                        format!(
                            "foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {observation_error}"
                        )
                    } else if let Some(history_error) = history_error {
                        format!(
                            "foreach node `{current_node}` error history exceeded rye-expr/1 aggregate bounds: {history_error}"
                        )
                    } else {
                        match placeholder {
                            Ok(()) => format!(
                                "foreach node `{current_node}` result aggregate exceeded rye-expr/1 bounds: {error}"
                            ),
                            Err(placeholder_error) => format!(
                                "foreach node `{current_node}` could not retain a null result placeholder after rejecting an oversized child result: {placeholder_error}"
                            ),
                        }
                    });
                    break;
                }
                if let Err(error) = retain_observation(
                    &mut observations,
                    &mut observation_budget,
                    DispatchObservation::from_success(
                        item_dispatch_id.clone(),
                        child_thread_id,
                        &val,
                    ),
                ) {
                    statuses.push(GraphToolCallStatus::IntegrityFailed);
                    result_budget = result_budget_before;
                    let _ = append_result(&mut results, &mut result_budget, Value::Null);
                    limit_error = Some(format!(
                        "foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {error}"
                    ));
                    break;
                }
                // Each assignment object is evaluated against this iteration's
                // pre-assignment candidate; keys within it are simultaneous.
                if let Some(assign) = &compiled.assign {
                    match ExpressionScope::new(
                        &candidate_state,
                        inputs,
                        execution.as_ref(),
                        Some(graph_run_id),
                    )
                    .with_foreach(var, item)
                    .with_result(&val)
                    .render_json(assign)
                    {
                        Ok(value) => {
                            let Value::Object(assign) = &value else {
                                unreachable!("compiled foreach assign templates are objects")
                            };
                            if let Err(error) = candidate_budget.apply(assign) {
                                result_budget = result_budget_before;
                                statuses.push(GraphToolCallStatus::IntegrityFailed);
                                let placeholder =
                                    append_result(&mut results, &mut result_budget, Value::Null);
                                limit_error = Some(match placeholder {
                                    Ok(()) => format!(
                                        "foreach candidate state exceeded rye-expr/1 bounds: {error}"
                                    ),
                                    Err(placeholder_error) => format!(
                                        "foreach candidate exceeded rye-expr/1 bounds ({error}); its null result placeholder also exceeded aggregate bounds: {placeholder_error}"
                                    ),
                                });
                                break;
                            }
                            merge_into(&mut candidate_state, &value);
                            merge_object_into(&mut delta, &value);
                            results.push(val);
                            statuses.push(GraphToolCallStatus::Ok);
                        }
                        Err(error) => {
                            result_budget = result_budget_before;
                            statuses.push(GraphToolCallStatus::ExpressionFailed);
                            if let Err(history_error) = retain_error(
                                &mut errors,
                                &mut error_budget,
                                ErrorRecord {
                                    step,
                                    node: current_node.to_string(),
                                    error: format!(
                                        "expression evaluation failed in foreach assign: {error}"
                                    ),
                                },
                            ) {
                                limit_error = Some(format!(
                                    "foreach node `{current_node}` error history exceeded rye-expr/1 aggregate bounds: {history_error}"
                                ));
                                break;
                            }
                            if let Err(placeholder_error) =
                                append_result(&mut results, &mut result_budget, Value::Null)
                            {
                                limit_error = Some(format!(
                                    "foreach node `{current_node}` could not retain a null result placeholder: {placeholder_error}"
                                ));
                                break;
                            }
                            if !continue_on_error {
                                break;
                            }
                        }
                    }
                } else {
                    results.push(val);
                    statuses.push(GraphToolCallStatus::Ok);
                }
            }
            Ok(crate::dispatch::ActionOutcome::Failure(failure)) => {
                // Leaf ran but failed — a failed native child may still have
                // spent tokens, so fold its cost in before recording the
                // diagnostic and a null placeholder (indices stay aligned).
                let crate::dispatch::ActionFailure {
                    diagnostic,
                    cost,
                    child_thread_id,
                    integrity,
                    ..
                } = failure;
                if let Err(error) = add_cost(&mut total_cost, cost) {
                    statuses.push(GraphToolCallStatus::IntegrityFailed);
                    let observation_error = retain_observation(
                        &mut observations,
                        &mut observation_budget,
                        DispatchObservation::child_only(item_dispatch_id.clone(), child_thread_id),
                    )
                    .err();
                    limit_error = Some(match observation_error {
                        Some(observation_error) => format!(
                            "foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {observation_error}"
                        ),
                        None => format!(
                            "foreach node `{current_node}` iteration reported invalid cost: {error}"
                        ),
                    });
                    break;
                }
                if integrity {
                    statuses.push(GraphToolCallStatus::IntegrityFailed);
                    let observation_error = retain_observation(
                        &mut observations,
                        &mut observation_budget,
                        DispatchObservation::child_only(item_dispatch_id.clone(), child_thread_id),
                    )
                    .err();
                    limit_error = Some(match observation_error {
                        Some(error) => format!(
                            "foreach node `{current_node}` envelope integrity failed ({diagnostic}); observation history also exceeded bounds: {error}"
                        ),
                        None => format!(
                            "foreach node `{current_node}` envelope integrity failed: {diagnostic}"
                        ),
                    });
                    break;
                }
                statuses.push(GraphToolCallStatus::Error);
                if let Err(error) = retain_observation(
                    &mut observations,
                    &mut observation_budget,
                    DispatchObservation::child_only(item_dispatch_id.clone(), child_thread_id),
                ) {
                    limit_error = Some(format!(
                        "foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {error}"
                    ));
                    break;
                }
                if let Err(error) = retain_error(
                    &mut errors,
                    &mut error_budget,
                    ErrorRecord {
                        step,
                        node: current_node.to_string(),
                        error: format!("foreach sequential iteration failed: {}", diagnostic),
                    },
                ) {
                    limit_error = Some(format!(
                        "foreach node `{current_node}` error history exceeded rye-expr/1 aggregate bounds: {error}"
                    ));
                    break;
                }
                if let Err(error) = append_result(&mut results, &mut result_budget, Value::Null) {
                    limit_error = Some(format!(
                        "foreach node `{current_node}` could not retain a null result placeholder: {error}"
                    ));
                    break;
                }
                if !continue_on_error {
                    break;
                }
            }
            Err(e) => {
                // Dispatch failed before the leaf returned anything.
                statuses.push(GraphToolCallStatus::DispatchFailed);
                if let Err(error) = retain_error(
                    &mut errors,
                    &mut error_budget,
                    ErrorRecord {
                        step,
                        node: current_node.to_string(),
                        error: format!("foreach sequential iteration dispatch failed: {e:#}"),
                    },
                ) {
                    limit_error = Some(format!(
                        "foreach node `{current_node}` error history exceeded rye-expr/1 aggregate bounds: {error}"
                    ));
                    break;
                }
                if let Err(error) = append_result(&mut results, &mut result_budget, Value::Null) {
                    limit_error = Some(format!(
                        "foreach node `{current_node}` could not retain a null result placeholder: {error}"
                    ));
                    break;
                }
                if !continue_on_error {
                    break;
                }
            }
        }
    }
    ForeachRun {
        results,
        statuses,
        total_items: items.len(),
        errors,
        assign_delta: Value::Object(delta),
        cost: total_cost,
        observations,
        limit_error,
        callback_warnings,
    }
}

/// Per-item result of a parallel foreach task. Parallel assignment is rejected
/// at graph load, so the task can only return its result and reported cost.
enum ParallelItem {
    Success {
        result: Value,
        cost: Option<RuntimeCost>,
        item_id: String,
        child_thread_id: Option<String>,
    },
    Failure {
        diagnostic: String,
        cost: Option<RuntimeCost>,
        status: GraphToolCallStatus,
        item_id: Option<String>,
        child_thread_id: Option<String>,
        integrity: bool,
    },
}

enum ParallelWork {
    Ready(ParallelItem),
    Spawned(tokio::task::JoinHandle<(ParallelItem, Vec<String>)>),
}

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
        compiled,
        thread_id,
        project_path,
        client: _client_ref,
        exec_ctx: _exec_ctx_ref,
        step,
        current_node,
        graph_run_id,
        definition_ref,
        definition_hash,
        continue_on_error,
        cancel_flag,
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
    let execution = exec_ctx.as_context_value();
    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut statuses = Vec::new();
    let mut total_cost: Option<RuntimeCost> = None;
    let mut observations = Vec::new();
    let mut limit_error = None;
    let mut callback_warnings = Vec::new();
    let mut callback_warning_budget = RuntimeJsonArrayBudget::new(format!(
        "node {current_node}.parallel foreach retry warnings"
    ));
    let mut result_budget =
        RuntimeJsonArrayBudget::new(format!("node {current_node}.parallel foreach results"));
    let mut error_budget =
        RuntimeJsonArrayBudget::new(format!("node {current_node}.parallel foreach errors"));
    let mut observation_budget =
        RuntimeJsonArrayBudget::new(format!("node {current_node}.parallel foreach observations"));

    // Process bounded batches in input order. The previous semaphore design
    // spawned every item immediately; completed tasks could then retain every
    // child result behind one slow early handle, defeating the result budget.
    // A batch keeps at most max_concurrency result envelopes resident while
    // preserving deterministic collect ordering.
    'batches: for (batch_index, chunk) in items.chunks(max_conc).enumerate() {
        if cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            break;
        }
        let mut work = Vec::with_capacity(chunk.len());
        let mut launch_budget = RuntimeJsonArrayBudget::new(format!(
            "node {current_node}.parallel foreach launch batch"
        ));
        for (chunk_index, item) in chunk.iter().enumerate() {
            let action = match &compiled.action {
                Some(action) => action,
                None => continue,
            };
            let mut rendered =
                match ExpressionScope::new(state, inputs, Some(&execution), Some(graph_run_id))
                    .with_foreach(var, item)
                    .render_action(action)
                {
                    Ok(value) => value,
                    Err(error) => {
                        work.push(ParallelWork::Ready(ParallelItem::Failure {
                            diagnostic: bounded_parallel_diagnostic(format!(
                                "expression evaluation failed in foreach action: {error}"
                            )),
                            cost: None,
                            status: GraphToolCallStatus::ExpressionFailed,
                            item_id: None,
                            child_thread_id: None,
                            integrity: false,
                        }));
                        if !continue_on_error {
                            break;
                        }
                        continue;
                    }
                };
            fold_launch_window(node, &mut rendered, graph_run_id, current_node);
            stamp_detached_operation(
                &mut rendered,
                graph_run_id,
                current_node,
                step,
                batch_index * max_conc + chunk_index,
            );
            if let Err(error) = launch_budget.append(&rendered) {
                work.push(ParallelWork::Ready(ParallelItem::Failure {
                    diagnostic: bounded_parallel_diagnostic(format!(
                        "parallel foreach launch batch for node `{current_node}` exceeded rye-expr/1 aggregate bounds: {error}"
                    )),
                    cost: None,
                    status: GraphToolCallStatus::IntegrityFailed,
                    item_id: rendered
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    child_thread_id: None,
                    integrity: true,
                }));
                break;
            }

            let client = client.clone();
            let thread_id = thread_id.to_string();
            let project_path = project_path.to_string();
            let exec_ctx = exec_ctx.clone();
            let retry_cfg = retry_cfg.clone();
            let retry_ev = retry_ev.clone();
            let cancel_flag = cancel_flag.clone();
            work.push(ParallelWork::Spawned(tokio::spawn(async move {
                let item_dispatch_id = rendered
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let retried = dispatch_item_with_retry(
                    &client,
                    &rendered,
                    &thread_id,
                    &project_path,
                    Some(&exec_ctx),
                    retry_cfg.as_ref(),
                    &retry_ev,
                    &item_dispatch_id,
                    cancel_flag,
                )
                .await;
                let outcome = match retried.outcome {
                    Ok(crate::dispatch::ActionOutcome::Success(success)) => {
                        let crate::dispatch::ActionSuccess {
                            result,
                            cost,
                            child_thread_id,
                        } = success;
                        if let Err(error) = validate_runtime_value(
                            &result,
                            "parallel foreach item result",
                        ) {
                            ParallelItem::Failure {
                                diagnostic: bounded_parallel_diagnostic(format!(
                                    "parallel foreach item result exceeded rye-expr/1 bounds: {error}"
                                )),
                                cost,
                                status: GraphToolCallStatus::IntegrityFailed,
                                item_id: Some(item_dispatch_id),
                                child_thread_id,
                                integrity: true,
                            }
                        } else {
                            ParallelItem::Success {
                                result,
                                cost,
                                item_id: item_dispatch_id,
                                child_thread_id,
                            }
                        }
                    }
                    Ok(crate::dispatch::ActionOutcome::Failure(failure)) => {
                        let crate::dispatch::ActionFailure {
                            diagnostic,
                            cost,
                            child_thread_id,
                            integrity,
                            ..
                        } = failure;
                        ParallelItem::Failure {
                            diagnostic: bounded_parallel_diagnostic(format!(
                                "foreach parallel iteration failed: {}",
                                diagnostic
                            )),
                            cost,
                            status: GraphToolCallStatus::Error,
                            item_id: Some(item_dispatch_id),
                            child_thread_id,
                            integrity,
                        }
                    }
                    Err(error) => ParallelItem::Failure {
                        diagnostic: bounded_parallel_diagnostic(format!(
                            "foreach parallel iteration dispatch failed: {error:#}"
                        )),
                        cost: None,
                        status: GraphToolCallStatus::DispatchFailed,
                        item_id: None,
                        child_thread_id: None,
                        integrity: false,
                    },
                };
                (outcome, retried.callback_warnings)
            })));
        }

        for item in work {
            let outcome = match item {
                ParallelWork::Ready(outcome) => Ok((outcome, Vec::new())),
                ParallelWork::Spawned(handle) => handle.await,
            };
            let outcome = match outcome {
                Ok((outcome, warnings)) => {
                    retain_retry_warnings(
                        &mut callback_warnings,
                        &mut callback_warning_budget,
                        warnings,
                    );
                    Ok(outcome)
                }
                Err(error) => Err(error),
            };
            match outcome {
                Ok(ParallelItem::Success {
                    result: value,
                    cost,
                    item_id,
                    child_thread_id,
                }) => {
                    if let Err(error) = add_cost(&mut total_cost, cost) {
                        statuses.push(GraphToolCallStatus::IntegrityFailed);
                        let observation_error = retain_observation(
                            &mut observations,
                            &mut observation_budget,
                            DispatchObservation::child_only(item_id, child_thread_id),
                        )
                        .err();
                        if limit_error.is_none() {
                            limit_error = Some(match observation_error {
                                Some(observation_error) => format!(
                                    "parallel foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {observation_error}"
                                ),
                                None => format!(
                                    "parallel foreach node `{current_node}` iteration reported invalid cost: {error}"
                                ),
                            });
                        }
                        continue;
                    }
                    if limit_error.is_some() {
                        let _ = retain_observation(
                            &mut observations,
                            &mut observation_budget,
                            DispatchObservation::child_only(item_id, child_thread_id),
                        );
                        statuses.push(GraphToolCallStatus::Ok);
                        continue;
                    }
                    if let Err(error) = result_budget.append(&value) {
                        statuses.push(GraphToolCallStatus::IntegrityFailed);
                        let _ = retain_observation(
                            &mut observations,
                            &mut observation_budget,
                            DispatchObservation::child_only(item_id, child_thread_id),
                        );
                        let _ = retain_error(
                            &mut errors,
                            &mut error_budget,
                            ErrorRecord {
                                step,
                                node: current_node.to_string(),
                                error: format!(
                                    "parallel foreach result exceeded rye-expr/1 aggregate bounds: {error}"
                                ),
                            },
                        );
                        limit_error = Some(format!(
                            "parallel foreach node `{current_node}` result aggregate exceeded rye-expr/1 bounds: {error}"
                        ));
                        continue;
                    }
                    if let Err(error) = retain_observation(
                        &mut observations,
                        &mut observation_budget,
                        DispatchObservation::from_success(item_id, child_thread_id, &value),
                    ) {
                        statuses.push(GraphToolCallStatus::IntegrityFailed);
                        limit_error = Some(format!(
                            "parallel foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {error}"
                        ));
                        continue;
                    }
                    results.push(value);
                    statuses.push(GraphToolCallStatus::Ok);
                }
                Ok(ParallelItem::Failure {
                    diagnostic,
                    cost,
                    status,
                    item_id,
                    child_thread_id,
                    integrity,
                }) => {
                    let diagnostic_limit_failure =
                        diagnostic.starts_with(PARALLEL_DIAGNOSTIC_LIMIT_FAILURE);
                    if let Err(error) = add_cost(&mut total_cost, cost) {
                        statuses.push(GraphToolCallStatus::IntegrityFailed);
                        let observation_error = retain_observation(
                            &mut observations,
                            &mut observation_budget,
                            item_id.and_then(|item_id| {
                                DispatchObservation::child_only(item_id, child_thread_id)
                            }),
                        )
                        .err();
                        if limit_error.is_none() {
                            limit_error = Some(match observation_error {
                                Some(observation_error) => format!(
                                    "parallel foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {observation_error}"
                                ),
                                None => format!(
                                    "parallel foreach node `{current_node}` iteration reported invalid cost: {error}"
                                ),
                            });
                        }
                        continue;
                    }
                    statuses.push(if integrity || diagnostic_limit_failure {
                        GraphToolCallStatus::IntegrityFailed
                    } else {
                        status
                    });
                    if integrity && limit_error.is_none() {
                        limit_error = Some(format!(
                            "parallel foreach node `{current_node}` envelope integrity failed: {diagnostic}"
                        ));
                    }
                    if diagnostic_limit_failure && limit_error.is_none() {
                        limit_error = Some(diagnostic.clone());
                    }
                    if limit_error.is_some() {
                        let _ = retain_observation(
                            &mut observations,
                            &mut observation_budget,
                            item_id.and_then(|item_id| {
                                DispatchObservation::child_only(item_id, child_thread_id)
                            }),
                        );
                        continue;
                    }
                    if let Err(error) = retain_observation(
                        &mut observations,
                        &mut observation_budget,
                        item_id.and_then(|item_id| {
                            DispatchObservation::child_only(item_id, child_thread_id)
                        }),
                    ) {
                        limit_error = Some(format!(
                            "parallel foreach node `{current_node}` observation history exceeded rye-expr/1 aggregate bounds: {error}"
                        ));
                        continue;
                    }
                    if let Err(error) = retain_error(
                        &mut errors,
                        &mut error_budget,
                        ErrorRecord {
                            step,
                            node: current_node.to_string(),
                            error: diagnostic,
                        },
                    ) {
                        limit_error = Some(format!(
                            "parallel foreach node `{current_node}` error history exceeded rye-expr/1 aggregate bounds: {error}"
                        ));
                        continue;
                    }
                    if let Err(error) = result_budget.append(&Value::Null) {
                        limit_error = Some(format!(
                            "parallel foreach node `{current_node}` could not retain a null result placeholder: {error}"
                        ));
                    } else {
                        results.push(Value::Null);
                    }
                }
                Err(join_error) => {
                    // A task can panic or be cancelled after its dispatch has
                    // crossed the callback boundary. At that point neither
                    // child lineage nor spent cost is recoverable from the
                    // missing outcome, so treating the join failure as an
                    // ordinary dispatch error would permit an under-accounted
                    // `continue`. Fail the whole candidate as integrity drift.
                    statuses.push(GraphToolCallStatus::IntegrityFailed);
                    if limit_error.is_none() {
                        limit_error = Some(bounded_parallel_diagnostic(format!(
                            "parallel foreach node `{current_node}` iteration task terminated without an authoritative outcome; child cost/provenance may be incomplete: {join_error}"
                        )));
                    }
                }
            }
        }
        if limit_error.is_some() || (!continue_on_error && !errors.is_empty()) {
            break 'batches;
        }
    }
    ForeachRun {
        results,
        statuses,
        total_items: items.len(),
        errors,
        assign_delta: Value::Object(Map::new()),
        cost: total_cost,
        observations,
        limit_error,
        callback_warnings,
    }
}

fn merge_into(target: &mut Value, source: &Value) {
    if let (Value::Object(ref mut t_map), Value::Object(ref s_map)) = (target, source) {
        for (k, v) in s_map {
            t_map.insert(k.clone(), v.clone());
        }
    }
}

/// Merge an evaluated `assign` value into the accumulating delta map.
/// Non-object assign results are ignored (assign must yield an object).
fn merge_object_into(delta: &mut Map<String, Value>, source: &Value) {
    if let Value::Object(map) = source {
        for (k, v) in map {
            delta.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MAX_RETRY_BACKOFF_MS;
    use serde_json::json;

    fn detach_node(max_concurrency: Option<usize>) -> GraphNode {
        let mut node: GraphNode = serde_yaml::from_str(
            "node_type: foreach\nover: \"${state.items}\"\ndetach: true\nparallel: true\naction: {item_id: \"graph:t/leaf\", ref_bindings: {}}\n",
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

    #[test]
    fn cost_rollup_rejects_invalid_values_without_mutating_the_prefix() {
        let mut total = Some(RuntimeCost {
            input_tokens: i64::MAX as u64,
            output_tokens: 4,
            total_usd: 0.25,
            basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
        });

        let overflow = add_cost(
            &mut total,
            Some(RuntimeCost {
                input_tokens: 1,
                output_tokens: 1,
                total_usd: 0.5,
                basis: None,
            }),
        )
        .unwrap_err();
        assert_eq!(overflow, RuntimeCostError::InputTokensOutOfRange);
        let total = total.expect("valid prefix remains available");
        assert_eq!(total.input_tokens, i64::MAX as u64);
        assert_eq!(total.output_tokens, 4);
        assert_eq!(total.total_usd, 0.25);

        let mut empty = None;
        let negative = add_cost(
            &mut empty,
            Some(RuntimeCost {
                input_tokens: 1,
                output_tokens: 1,
                total_usd: -0.5,
                basis: None,
            }),
        )
        .unwrap_err();
        assert_eq!(negative, RuntimeCostError::NegativeTotalUsd);
        assert!(empty.is_none());
    }

    #[tokio::test]
    async fn retry_delay_wakes_for_cooperative_cancel() {
        let flag = Arc::new(AtomicBool::new(false));
        let setter = flag.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            setter.store(true, Ordering::Relaxed);
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sleep_retry_delay(MAX_RETRY_BACKOFF_MS, Some(&flag)),
        )
        .await
        .expect("SIGTERM flag must wake a foreach retry backoff promptly");
    }
}
