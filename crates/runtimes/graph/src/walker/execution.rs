use std::time::Instant;

use serde_json::{json, Value};

use crate::model::*;
use crate::{dispatch, edges, env_preflight};
use ryeos_runtime::envelope::RuntimeCost;
use ryeos_runtime::events::RuntimeEventType;

use super::outcome::{add_runtime_cost, RunNodeBodyContext, StepOutcome};
use super::transitions::{resolve_next_on_error, retry_attempts_remaining};
use super::{compute_cache_key, merge_into, strip_none_values, Walker};

impl Walker {
    /// Action node body: permission check → env preflight → dispatch
    /// → classify result. Returns a StepOutcome without emitting any
    /// events or persisting anything.
    pub(super) async fn run_action_body(
        &self,
        ctx: RunNodeBodyContext<'_>,
        start: Instant,
    ) -> StepOutcome {
        let RunNodeBodyContext {
            current,
            node,
            cfg,
            step,
            state,
            inputs,
            exec_ctx,
            cache,
            graph_run_id,
            suppressed_errors: _suppressed_errors,
            retry_attempt,
        } = ctx;
        let execution = exec_ctx.as_context_value();

        // Cohort follow is an action-node state machine of its own. Split before
        // generic action interpolation: the authored action may reference `as`.
        if node.follow && node.over.is_some() {
            return self
                .run_follow_fanout(
                    node,
                    current,
                    cfg,
                    step,
                    state,
                    inputs,
                    &execution,
                    graph_run_id,
                    start,
                )
                .await;
        }
        let mut action = match &node.action {
            Some(a) => a.clone(),
            None => {
                // Action node with no action — treat as terminal.
                let next =
                    edges::evaluate_next(node, state, inputs, Some(&execution), Some(graph_run_id));
                return match next {
                    Some(n) => StepOutcome::ActionOk {
                        item_id: String::new(),
                        result: json!({}),
                        assign: None,
                        next: Some(n),
                        cache_hit: false,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                    },
                    None => StepOutcome::Terminal {
                        status: "completed",
                        error: None,
                    },
                };
            }
        };

        // A `detach: true` node launches a lineage-linked, cohort-tagged child
        // that runs concurrently while this walk continues — the native fanout
        // primitive (`foreach → launch`). The fold routes it to the daemon's
        // `spawn_detached_child` and carries the node's `facets:` for per-child
        // stamping. `detach` and `follow` are mutually exclusive (enforced at
        // validation); a detach node never suspends, so it flows straight to
        // dispatch below.
        node.fold_detach_into_action(&mut action);

        let item_id = action
            .get("item_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let elapsed = start.elapsed().as_millis() as u64;

        // D16: no walker-side permission check — the daemon enforces
        // caps at the callback boundary (enforce_callback_caps in
        // runtime_dispatch.rs). The walker is the executor only.

        let ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
            execution: Some(execution.clone()),
            graph_run_id: Some(graph_run_id.to_string()),
        };

        let interpolated_action =
            match ryeos_runtime::interpolate_action(&action, &ctx.as_context()) {
                Ok(value) => value,
                Err(err) => {
                    // Interpolation failed before any dispatch — no cost, and
                    // the interpolated item_id is unavailable, so report the
                    // raw template item_id.
                    return StepOutcome::DispatchHardError {
                        item_id: Some(item_id),
                        error: format!(
                            "interpolation error in action for node `{current}`: {err:#}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: elapsed,
                        cost: None,
                    };
                }
            };

        let stripped_action = strip_none_values(&interpolated_action);
        // The dispatched item_id is the interpolated one (item_id may itself
        // contain `${...}`). Cost records and receipts for everything past
        // this point use it, not the raw template id.
        let dispatched_item_id = stripped_action
            .get("item_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| item_id.clone());

        // Resume INTO a follow node: consume the child's stored terminal envelope
        // instead of re-dispatching or re-suspending. `None` on a first run, or a
        // re-drive with no result yet (which re-suspends idempotently). Taken
        // BEFORE env preflight so a parent-side env gap can't turn an already-
        // completed child's result into a dispatch hard error.
        let resumed_follow_envelope = if node.follow {
            self.take_follow_result(current)
        } else {
            None
        };

        // Env preflight — skipped when consuming a stored follow result (the child
        // already ran). Still enforced for first-run follow suspend, bare-marker
        // re-suspend, and normal dispatches.
        if resumed_follow_envelope.is_none() {
            if let Err(env_err) = env_preflight::check_env_requires(
                &self.graph.config.env_requires,
                &node.env_requires,
            ) {
                let err_msg = format!("env preflight failed: {env_err}");
                return StepOutcome::DispatchHardError {
                    item_id: Some(dispatched_item_id),
                    error: err_msg,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    cost: None,
                };
            }
        }

        // A follow node with no stored result does not dispatch inline: hand the
        // action off to a detached child and suspend (handled in commit_step). The
        // result is consumed on resume, so nothing is dispatched or cached here.
        if node.follow && resumed_follow_envelope.is_none() {
            return StepOutcome::FollowSuspend {
                item_id: dispatched_item_id,
                params: stripped_action
                    .get("params")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            };
        }

        // Dispatch. `dispatch_action` classifies the daemon envelope:
        //   Err            → transport/dispatch failure (hard error)
        //   Ok(Failure(d)) → leaf ran but failed (non-zero exit, etc.)
        //   Ok(Success(v)) → bare, envelope-unwrapped leaf result
        // On follow resume, the stored child envelope is classified byte-for-byte
        // like a live dispatch, so the resumed node runs the identical success/
        // failure path (receipt, cost, assign land normally in commit_step).
        let mut cache_hit = false;
        let outcome: Result<dispatch::ActionOutcome, dispatch::ActionDispatchError> =
            if let Some(envelope) = resumed_follow_envelope {
                Ok(dispatch::classify_follow_envelope(envelope))
            } else if node.is_cacheable() {
                let cache_key = compute_cache_key(&self.graph.graph_id, current, &stripped_action);
                if let Some(cached) = cache.lookup(&cache_key) {
                    cache_hit = true;
                    // A cache hit replays the stored result and must NOT re-bill cost —
                    // `bare` carries no cost. A stale/tampered entry still carrying a
                    // top-level continuation_id is rejected loudly, exactly like a live
                    // inline-continuation dispatch (F10 — inline continuation is retired;
                    // use a `follow: true` node). New dispatches never cache such a value.
                    if cached
                        .get("continuation_id")
                        .and_then(|v| v.as_str())
                        .is_some()
                    {
                        Ok(dispatch::ActionOutcome::Failure(dispatch::ActionFailure {
                            diagnostic: format!(
                                "cached result for node `{current}` carries a continuation_id; \
                             inline continuation is retired — use a `follow: true` node"
                            ),
                            cost: None,
                            retryable: false,
                        }))
                    } else {
                        Ok(dispatch::ActionOutcome::Success(
                            dispatch::ActionSuccess::bare(cached),
                        ))
                    }
                } else {
                    match dispatch::dispatch_action(
                        &self.client,
                        &stripped_action,
                        &self.thread_id,
                        &self.project_path,
                        Some(exec_ctx),
                    )
                    .await
                    {
                        Ok(dispatch::ActionOutcome::Success(success)) => {
                            // Only successful dispatches are cached — never a
                            // failure, which would otherwise replay a stale
                            // error (or `null`) on the next run. The cache
                            // stores only the result value; cost is per-run.
                            cache.store(&cache_key, &success.result);
                            Ok(dispatch::ActionOutcome::Success(success))
                        }
                        Ok(failure) => Ok(failure),
                        Err(e) => Err(e),
                    }
                }
            } else {
                dispatch::dispatch_action(
                    &self.client,
                    &stripped_action,
                    &self.thread_id,
                    &self.project_path,
                    Some(exec_ctx),
                )
                .await
            };

        let elapsed = start.elapsed().as_millis() as u64;

        match outcome {
            Err(dispatch_error) => {
                // A transport/dispatch failure with retry attempts remaining
                // reschedules a fresh-step re-dispatch; exhausted → on_error.
                if dispatch_error.retryable {
                    if let Some(failed_attempt) = retry_attempts_remaining(node, retry_attempt) {
                        let rc = node.retry.as_ref().expect("retry present when scheduling");
                        return StepOutcome::RetryScheduled {
                            item_id: dispatched_item_id,
                            error: dispatch_error.diagnostic,
                            failed_attempt,
                            total_attempts: rc.attempts,
                            delay_ms: rc.delay_ms(failed_attempt),
                            elapsed_ms: elapsed,
                            // Transport failed before the child returned — no cost.
                            cost: None,
                        };
                    }
                }
                StepOutcome::DispatchHardError {
                    item_id: Some(dispatched_item_id),
                    error: dispatch_error.diagnostic,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    // Transport/dispatch failed before the child returned — no cost.
                    cost: None,
                }
            }
            Ok(dispatch::ActionOutcome::Failure(failure)) => {
                // Authored retry is an attempt budget, not blanket permission.
                // Only a failure explicitly classified retryable may consume it.
                if failure.retryable {
                    if let Some(failed_attempt) = retry_attempts_remaining(node, retry_attempt) {
                        let rc = node.retry.as_ref().expect("retry present when scheduling");
                        return StepOutcome::RetryScheduled {
                            item_id: dispatched_item_id,
                            error: failure.diagnostic,
                            failed_attempt,
                            total_attempts: rc.attempts,
                            delay_ms: rc.delay_ms(failed_attempt),
                            elapsed_ms: elapsed,
                            cost: failure.cost,
                        };
                    }
                }
                StepOutcome::LeafSoftError {
                    item_id: dispatched_item_id,
                    error: failure.diagnostic,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    // A failed native child may have spent tokens — preserve it.
                    cost: failure.cost,
                }
            }
            Ok(dispatch::ActionOutcome::Success(success)) => {
                let dispatch::ActionSuccess {
                    result: val,
                    cost,
                    child_thread_id,
                } = success;
                // Portable dispatch lineage: when this node spawned a native
                // child thread (a directive or sub-graph), emit a
                // `child_thread_spawned` event into THIS (parent) thread's stream
                // so the edge lands in rebuild-safe history — the braid drill
                // target and the derived `threads.children` edge both come from
                // it. The daemon's `thread_child_link` (recorded at launch) is the
                // separate, non-portable cascade copy. Do NOT set the child's
                // `upstream_thread_id`: that is the continuation-predecessor link
                // and stamping it cross-chain corrupts the child's resume.
                if let Some(ref child_id) = child_thread_id {
                    let r = self
                        .client
                        .append_runtime_event(
                            RuntimeEventType::ChildThreadSpawned,
                            json!({
                                "child_thread_id": child_id,
                                "node": current,
                                "step": step,
                                "item_id": dispatched_item_id,
                                "spawn_reason": "dispatch",
                            }),
                        )
                        .await;
                    self.record_callback_warning("child_thread_spawned", r);
                }
                // Domain milestones: a tool/directive result may carry a
                // `milestones` array of `{kind, payload}`; emit one generic
                // `milestone` event per entry into this thread's stream
                // (runtime-on-behalf-of-tool — tools stay pure content, the engine
                // owns only the generic event; a view styles the kinds via
                // `projections.event_kinds`). `node`/`step` locate it in the braid.
                if let Some(milestones) = val.get("milestones").and_then(|v| v.as_array()) {
                    for entry in milestones {
                        let Some(kind) = entry.get("kind").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let r = self
                            .client
                            .append_runtime_event(
                                RuntimeEventType::Milestone,
                                json!({
                                    "kind": kind,
                                    "payload": entry.get("payload").cloned().unwrap_or(Value::Null),
                                    "node": current,
                                    "step": step,
                                }),
                            )
                            .await;
                        self.record_callback_warning("milestone", r);
                    }
                }
                // Interpolate `assign` HERE (not in commit_step) so an
                // interpolation failure becomes a node error that obeys
                // on_error — never a suppressed error that merges the raw
                // `${...}` template into graph state.
                let assign = match &node.assign {
                    Some(assign_tpl) => {
                        let assign_ctx = WalkContext {
                            state: state.clone(),
                            inputs: inputs.clone(),
                            result: Some(val.clone()),
                            execution: Some(execution.clone()),
                            graph_run_id: Some(graph_run_id.to_string()),
                        };
                        match ryeos_runtime::interpolate(assign_tpl, &assign_ctx.as_context()) {
                            Ok(interpolated) => Some(interpolated),
                            Err(e) => {
                                // The child SUCCEEDED (and may have spent
                                // tokens); only graph post-processing failed.
                                // Carry the cost so it is still accounted.
                                return StepOutcome::LeafSoftError {
                                    item_id: dispatched_item_id,
                                    error: format!(
                                        "interpolation error in `assign` for node `{current}`: {e:#}"
                                    ),
                                    next_on_error: resolve_next_on_error(node, cfg),
                                    elapsed_ms: elapsed,
                                    cost,
                                };
                            }
                        }
                    }
                    None => None,
                };
                let next = edges::evaluate_next_with_result(
                    node,
                    state,
                    inputs,
                    &val,
                    Some(&execution),
                    Some(graph_run_id),
                );
                StepOutcome::ActionOk {
                    item_id: dispatched_item_id,
                    result: val,
                    assign,
                    next,
                    cache_hit,
                    elapsed_ms: elapsed,
                    cost,
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_follow_fanout(
        &self,
        node: &GraphNode,
        current: &str,
        cfg: &GraphConfig,
        step: u32,
        state: &Value,
        inputs: &Value,
        execution: &Value,
        graph_run_id: &str,
        start: Instant,
    ) -> StepOutcome {
        // Consume first. An armed successor must never repeat env preflight or
        // interpolate/dispatch the child action.
        let resumed_state = self.take_follow_state(current);
        let checkpointed_items = resumed_state
            .as_ref()
            .and_then(|state| state.iteration_snapshot.clone());
        let resumed = resumed_state.and_then(|state| state.follow_result);
        if resumed.is_some() && checkpointed_items.is_none() {
            return StepOutcome::LeafSoftError {
                item_id: node
                    .action
                    .as_ref()
                    .and_then(|a| a.get("item_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                error: format!("follow fanout node `{current}` resumed without iteration snapshot"),
                next_on_error: resolve_next_on_error(node, cfg),
                elapsed_ms: start.elapsed().as_millis() as u64,
                cost: None,
            };
        }
        let base_ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
            execution: Some(execution.clone()),
            graph_run_id: Some(graph_run_id.to_string()),
        };
        // An armed cohort resume is immutable: its iteration values are local
        // checkpoint facts and must not be re-resolved from mutable state.
        let over = if let Some(items) = checkpointed_items {
            items
        } else {
            match ryeos_runtime::interpolate(
                &Value::String(node.over.as_deref().unwrap_or_default().to_string()),
                &base_ctx.as_context(),
            ) {
                Ok(Value::Array(items)) => items,
                Ok(other) => {
                    return StepOutcome::DispatchHardError {
                        item_id: None,
                        error: format!(
                        "follow fanout node `{current}` over must resolve to array, got {other}"
                    ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                    }
                }
                Err(e) => {
                    return StepOutcome::DispatchHardError {
                        item_id: None,
                        error: format!("interpolation error in `over` for node `{current}`: {e:#}"),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                    }
                }
            }
        };
        let var = node.foreach_var().to_string();
        let raw_item_id = node
            .action
            .as_ref()
            .and_then(|a| a.get("item_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if let Some(wrapper) = resumed {
            if wrapper.get("fanout").and_then(Value::as_bool) != Some(true) {
                return StepOutcome::LeafSoftError {
                    item_id: raw_item_id,
                    error: format!(
                        "follow fanout node `{current}` resumed with malformed fanout wrapper"
                    ),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                };
            }
            let Some(envelopes) = wrapper.get("items").and_then(Value::as_array) else {
                return StepOutcome::LeafSoftError {
                    item_id: raw_item_id,
                    error: format!(
                        "follow fanout node `{current}` resumed without fanout items wrapper"
                    ),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                };
            };
            let expected = wrapper
                .get("expected")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok());
            let wrapper_statuses = wrapper.get("statuses").and_then(Value::as_array);
            if expected != Some(envelopes.len())
                || envelopes.len() != over.len()
                || wrapper_statuses.map(Vec::len) != Some(envelopes.len())
            {
                return StepOutcome::LeafSoftError { item_id: raw_item_id, error: format!("follow fanout node `{current}` resumed with inconsistent expected/items/statuses/snapshot cardinality"), next_on_error: resolve_next_on_error(node, cfg), elapsed_ms: start.elapsed().as_millis() as u64, cost: None };
            }
            let mut results = vec![Value::Null; envelopes.len()];
            let mut statuses = Vec::with_capacity(envelopes.len());
            let mut errors = Vec::new();
            let mut delta = Value::Object(serde_json::Map::new());
            let mut total_cost: Option<RuntimeCost> = None;
            for (index, envelope) in envelopes.iter().cloned().enumerate() {
                match dispatch::classify_follow_envelope(envelope) {
                    dispatch::ActionOutcome::Success(success) => {
                        statuses.push("completed".to_string());
                        results[index] = success.result.clone();
                        add_runtime_cost(&mut total_cost, success.cost);
                        if let (Some(assign), Some(item)) = (&node.assign, over.get(index)) {
                            let assign_ctx = WalkContext {
                                state: state.clone(),
                                inputs: inputs.clone(),
                                result: Some(success.result),
                                execution: Some(execution.clone()),
                                graph_run_id: Some(graph_run_id.to_string()),
                            }
                            .with_foreach_item(&var, item);
                            match ryeos_runtime::interpolate(assign, &assign_ctx) {
                                Ok(value) => merge_into(&mut delta, &value),
                                Err(e) => {
                                    statuses[index] = "failed".to_string();
                                    results[index] = Value::Null;
                                    errors.push(ErrorRecord { step, node: current.to_string(), error: format!("follow item {index} assign interpolation failed: {e:#}") });
                                }
                            }
                        }
                    }
                    dispatch::ActionOutcome::Failure(failure) => {
                        statuses.push("failed".to_string());
                        add_runtime_cost(&mut total_cost, failure.cost);
                        errors.push(ErrorRecord {
                            step,
                            node: current.to_string(),
                            error: format!("follow item {index} failed: {}", failure.diagnostic),
                        });
                    }
                }
            }
            let next = if errors.is_empty() {
                edges::evaluate_next_with_result(
                    node,
                    state,
                    inputs,
                    &Value::Array(results.clone()),
                    Some(execution),
                    Some(graph_run_id),
                )
            } else {
                None
            };
            return StepOutcome::FollowFanoutDone {
                results,
                statuses,
                errors,
                assign_delta: delta,
                collect_key: node.collect.clone(),
                var_name: var,
                item_id: raw_item_id,
                next,
                next_on_error: resolve_next_on_error(node, cfg),
                cost: total_cost,
                elapsed_ms: start.elapsed().as_millis() as u64,
            };
        }

        if over.is_empty() {
            return StepOutcome::FollowFanoutDone {
                results: vec![],
                statuses: vec![],
                errors: vec![],
                assign_delta: Value::Object(serde_json::Map::new()),
                collect_key: node.collect.clone(),
                var_name: var,
                item_id: raw_item_id,
                next: edges::evaluate_next_with_result(
                    node,
                    state,
                    inputs,
                    &Value::Array(vec![]),
                    Some(execution),
                    Some(graph_run_id),
                ),
                next_on_error: resolve_next_on_error(node, cfg),
                cost: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
            };
        }
        if let Err(env_err) =
            env_preflight::check_env_requires(&cfg.env_requires, &node.env_requires)
        {
            return StepOutcome::DispatchHardError {
                item_id: Some(raw_item_id),
                error: format!("env preflight failed: {env_err}"),
                next_on_error: resolve_next_on_error(node, cfg),
                elapsed_ms: start.elapsed().as_millis() as u64,
                cost: None,
            };
        }
        let mut children = Vec::with_capacity(over.len());
        for (index, item) in over.iter().enumerate() {
            let item_ctx = base_ctx.with_foreach_item(&var, item);
            let action =
                match ryeos_runtime::interpolate_action(node.action.as_ref().unwrap(), &item_ctx) {
                    Ok(v) => strip_none_values(&v),
                    Err(e) => {
                        return StepOutcome::DispatchHardError {
                            item_id: Some(raw_item_id),
                            error: format!("follow fanout action interpolation failed: {e:#}"),
                            next_on_error: resolve_next_on_error(node, cfg),
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            cost: None,
                        }
                    }
                };
            let facets = match &node.facets {
                Some(value) => match ryeos_runtime::interpolate(value, &item_ctx) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        return StepOutcome::DispatchHardError {
                            item_id: Some(raw_item_id),
                            error: format!("follow fanout facets interpolation failed: {e:#}"),
                            next_on_error: resolve_next_on_error(node, cfg),
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            cost: None,
                        }
                    }
                },
                None => None,
            };
            let item_ref = action
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if item_ref.is_empty() {
                return StepOutcome::DispatchHardError {
                    item_id: None,
                    error: format!(
                        "follow fanout item {index} has missing or empty interpolated item_id"
                    ),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                };
            }
            children.push(ryeos_runtime::callback::FollowChildSpec {
                item_ref,
                parameters: action.get("params").cloned().unwrap_or_else(|| json!({})),
                facets,
            });
        }
        let width = match node.max_concurrency.map(u32::try_from).transpose() {
            Ok(width) => width,
            Err(_) => {
                return StepOutcome::DispatchHardError {
                    item_id: None,
                    error: format!(
                        "follow fanout node `{current}` max_concurrency does not fit in u32"
                    ),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                }
            }
        };
        StepOutcome::FollowFanoutSuspend {
            children,
            width,
            iteration_snapshot: over,
        }
    }
}
