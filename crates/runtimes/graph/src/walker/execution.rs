use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::evaluation::{
    validate_runtime_array_shape, validate_runtime_shape, validate_runtime_value, ExpressionScope,
};
use crate::model::*;
use crate::{dispatch, edges, env_preflight};
use ryeos_runtime::envelope::RuntimeCost;
use ryeos_runtime::RuntimeJsonArrayBudget;

use super::outcome::{
    add_runtime_cost, ActionOkOutcome, DispatchHardErrorOutcome, ExpressionFailedOutcome,
    ExpressionFailureEffects, FollowFanoutDoneOutcome, FollowFanoutSuspendOutcome,
    FollowSuspendOutcome, IntegrityFailedOutcome, LeafSoftErrorOutcome, RetryScheduledOutcome,
    RunNodeBodyContext, StepOutcome, TerminalOrigin, TerminalOutcome,
};
use super::transitions::{resolve_next_on_error, retry_attempts_remaining};
use super::{compute_cache_key, merge_into, Walker};

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
            retry_attempt,
        } = ctx;
        let execution = exec_ctx.as_context_value();
        let compiled = self.graph.compiled.node(current);

        // Cohort follow is an action-node state machine of its own. Split before
        // generic action rendering: the authored action may reference `as`.
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
        let action = match &compiled.action {
            Some(action) => action,
            None => {
                // Action node with no action — treat as terminal.
                let next = edges::evaluate_next(
                    compiled,
                    state,
                    inputs,
                    Some(&execution),
                    Some(graph_run_id),
                );
                return match next {
                    Ok(Some(n)) => StepOutcome::ActionOk(Box::new(ActionOkOutcome {
                        item_id: String::new(),
                        result: json!({}),
                        assign: None,
                        next: Some(n),
                        child_thread_id: None,
                        cache_hit: false,
                        cache_write_key: None,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                    })),
                    Ok(None) => StepOutcome::Terminal(TerminalOutcome {
                        status: GraphRunStatus::Completed,
                        error: None,
                        origin: TerminalOrigin::Node,
                        output: None,
                    }),
                    Err(error) => StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                        item_id: None,
                        error: format!(
                            "expression evaluation failed selecting `next` for node `{current}`: {error}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                        effects: ExpressionFailureEffects::default(),
                    }),
                };
            }
        };

        let item_id = node
            .action
            .as_ref()
            .and_then(|action| action.get("item_id").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();

        let elapsed = start.elapsed().as_millis() as u64;

        // D16: no walker-side permission check — the daemon enforces
        // caps at the callback boundary (enforce_callback_caps in
        // runtime_dispatch.rs). The walker is the executor only.

        let mut rendered_action =
            match ExpressionScope::new(state, inputs, Some(&execution), Some(graph_run_id))
                .render_action(action)
            {
                Ok(value) => value,
                Err(err) => {
                    return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                        item_id: Some(item_id),
                        error: format!(
                            "expression evaluation failed in action for node `{current}`: {err}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: elapsed,
                        cost: None,
                        effects: ExpressionFailureEffects::default(),
                    });
                }
            };
        if rendered_action.get("thread").and_then(Value::as_str) == Some("detached") {
            let operation = lillux::canonical_json(&json!({
                "graph_run_id": graph_run_id,
                "node": current,
                "step": step,
                "kind": "detached_action"
            }))
            .expect("fixed detached operation identity is canonical JSON");
            rendered_action
                .as_object_mut()
                .expect("rendered action is validated as an object")
                .insert(
                    "operation_id".to_string(),
                    Value::String(lillux::sha256_hex(operation.as_bytes())),
                );
        }

        // Missing paths fail or are handled explicitly by `??`; authored
        // `null` is data and must survive dispatch unchanged.
        // The dispatched item_id is the rendered one (item_id may itself
        // contain `${...}`). Cost records and receipts for everything past
        // this point use it, not the raw template id.
        let dispatched_item_id = rendered_action
            .get("item_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| item_id.clone());

        // Resume INTO a follow node: consume the child's stored terminal envelope
        // instead of re-dispatching or re-suspending. `None` on a first run, or a
        // re-drive with no result yet (which re-suspends idempotently). Taken
        // BEFORE env preflight so a parent-side env gap can't turn an already-
        // completed child's result into a dispatch hard error.
        let resumed_follow_state = if node.follow {
            self.take_follow_state(current)
        } else {
            None
        };
        if let Some(resumed) = &resumed_follow_state {
            let stored_item_ref = resumed
                .item_refs
                .first()
                .expect("validated single-follow resume carries exactly one item ref");
            if stored_item_ref != &dispatched_item_id {
                return StepOutcome::Terminal(TerminalOutcome {
                    status: GraphRunStatus::Error,
                    error: Some(format!(
                        "follow node `{current}` rendered item `{dispatched_item_id}`, but its checkpoint records `{stored_item_ref}`"
                    )),
                    origin: TerminalOrigin::RunControl,
                    output: None,
                });
            }
        }
        let resumed_follow_envelope = resumed_follow_state.and_then(|state| {
            state.follow_result.map(|envelope| {
                let item_ref = state
                    .item_refs
                    .into_iter()
                    .next()
                    .expect("validated single-follow resume carries exactly one item ref");
                (envelope, item_ref)
            })
        });

        // Env preflight — skipped when consuming a stored follow result (the child
        // already ran). Still enforced for first-run follow suspend, bare-marker
        // re-suspend, and normal dispatches.
        if resumed_follow_envelope.is_none() {
            if let Err(env_err) = env_preflight::check_env_requires(
                &self.graph.config.env_requires,
                &node.env_requires,
            ) {
                let err_msg = format!("env preflight failed: {env_err}");
                return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                    item_id: Some(dispatched_item_id),
                    error: err_msg,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    cost: None,
                });
            }
        }

        // A follow node with no stored result does not dispatch inline: hand the
        // action off to a detached child and suspend (handled in commit_step). The
        // result is consumed on resume, so nothing is dispatched or cached here.
        if node.follow && resumed_follow_envelope.is_none() {
            let ref_bindings = match rendered_action.get("ref_bindings") {
                Some(value) => {
                    match serde_json::from_value::<BTreeMap<String, String>>(value.clone()) {
                        Ok(bindings) => bindings,
                        Err(error) => {
                            return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                                item_id: Some(dispatched_item_id),
                                error: format!("follow action has invalid ref_bindings: {error}"),
                                next_on_error: resolve_next_on_error(node, cfg),
                                elapsed_ms: elapsed,
                                cost: None,
                            });
                        }
                    }
                }
                None => {
                    return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                        item_id: Some(dispatched_item_id),
                        error: "follow action is missing required ref_bindings".to_string(),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: elapsed,
                        cost: None,
                    });
                }
            };
            return StepOutcome::FollowSuspend(FollowSuspendOutcome {
                item_id: dispatched_item_id,
                ref_bindings,
                params: rendered_action
                    .get("params")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            });
        }

        // Dispatch. `dispatch_action` classifies the daemon envelope:
        //   Err            → transport/dispatch failure (hard error)
        //   Ok(Failure(d)) → leaf ran but failed (non-zero exit, etc.)
        //   Ok(Success(v)) → bare, envelope-unwrapped leaf result
        // On follow resume, the stored child value must match the exact managed
        // terminal-envelope contract before the resumed node enters the normal
        // success/failure path (receipt, cost, and assign land in commit_step).
        let mut cache_hit = false;
        let mut cache_write_key = None;
        let outcome: Result<dispatch::ActionOutcome, dispatch::ActionDispatchError> = if let Some(
            (envelope, stored_item_ref),
        ) =
            resumed_follow_envelope
        {
            match dispatch::classify_follow_envelope_for_item(envelope, &stored_item_ref) {
                Ok(classified) => Ok(classified.outcome),
                Err(error) => {
                    return StepOutcome::Terminal(TerminalOutcome {
                            status: GraphRunStatus::Error,
                            error: Some(format!(
                                "follow node `{current}` resumed with invalid terminal envelope: {error}"
                            )),
                            origin: TerminalOrigin::RunControl,
                            output: None,
                        });
                }
            }
        } else if node.is_cacheable() {
            let cache_key = match compute_cache_key(
                &self.graph.definition_hash,
                &self.graph.graph_id,
                current,
                &rendered_action,
            ) {
                Ok(cache_key) => cache_key,
                Err(error) => {
                    return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                        item_id: Some(dispatched_item_id),
                        error: format!(
                            "failed to canonicalize cache identity for node `{current}`: {error}"
                        ),
                        elapsed_ms: elapsed,
                        cost: None,
                        effects: ExpressionFailureEffects::default(),
                    });
                }
            };
            if let Some(cached) = cache.lookup(&cache_key) {
                cache_hit = true;
                // A cache hit replays a result retained earlier in this execution
                // and must NOT re-bill cost — `bare` carries no cost. A cached value
                // carrying a top-level continuation_id is still rejected loudly,
                // exactly like a live inline-continuation dispatch (F10 — inline
                // continuation is retired; use a `follow: true` node).
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
                        child_thread_id: None,
                        integrity: true,
                    }))
                } else {
                    Ok(dispatch::ActionOutcome::Success(
                        dispatch::ActionSuccess::bare(cached),
                    ))
                }
            } else {
                match dispatch::dispatch_action(
                    &self.client,
                    &rendered_action,
                    &self.thread_id,
                    &self.project_path,
                    Some(exec_ctx),
                )
                .await
                {
                    Ok(dispatch::ActionOutcome::Success(success)) => {
                        // Reserve the key only. Commit persists the result
                        // after its rye-expr bounds, complete assignment,
                        // and normal-edge selection all succeed.
                        cache_write_key = Some(cache_key);
                        Ok(dispatch::ActionOutcome::Success(success))
                    }
                    Ok(failure) => Ok(failure),
                    Err(e) => Err(e),
                }
            }
        } else {
            dispatch::dispatch_action(
                &self.client,
                &rendered_action,
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
                        return StepOutcome::RetryScheduled(RetryScheduledOutcome {
                            item_id: dispatched_item_id,
                            error: dispatch_error.diagnostic,
                            failed_attempt,
                            total_attempts: rc.attempts,
                            delay_ms: rc.delay_ms(failed_attempt),
                            elapsed_ms: elapsed,
                            // Transport failed before the child returned — no cost.
                            cost: None,
                        });
                    }
                }
                StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                    item_id: Some(dispatched_item_id),
                    error: dispatch_error.diagnostic,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    // Transport/dispatch failed before the child returned — no cost.
                    cost: None,
                })
            }
            Ok(dispatch::ActionOutcome::Failure(failure)) => {
                if failure.integrity {
                    return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                        item_id: Some(dispatched_item_id.clone()),
                        error: failure.diagnostic,
                        elapsed_ms: elapsed,
                        cost: failure.cost,
                        effects: ExpressionFailureEffects::action(DispatchObservation::child_only(
                            dispatched_item_id,
                            failure.child_thread_id,
                        )),
                    });
                }
                // Authored retry is an attempt budget, not blanket permission.
                // Only a failure explicitly classified retryable may consume it.
                if failure.retryable {
                    if let Some(failed_attempt) = retry_attempts_remaining(node, retry_attempt) {
                        let rc = node.retry.as_ref().expect("retry present when scheduling");
                        return StepOutcome::RetryScheduled(RetryScheduledOutcome {
                            item_id: dispatched_item_id,
                            error: failure.diagnostic,
                            failed_attempt,
                            total_attempts: rc.attempts,
                            delay_ms: rc.delay_ms(failed_attempt),
                            elapsed_ms: elapsed,
                            cost: failure.cost,
                        });
                    }
                }
                let observation = DispatchObservation::child_only(
                    dispatched_item_id.clone(),
                    failure.child_thread_id,
                );
                StepOutcome::LeafSoftError(LeafSoftErrorOutcome {
                    item_id: dispatched_item_id,
                    error: failure.diagnostic,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    // A failed native child may have spent tokens — preserve it.
                    cost: failure.cost,
                    observation,
                })
            }
            Ok(dispatch::ActionOutcome::Success(success)) => {
                let dispatch::ActionSuccess {
                    result: val,
                    cost,
                    child_thread_id,
                } = success;
                if let Err(error) = validate_runtime_value(&val, "graph action result") {
                    return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                        item_id: Some(dispatched_item_id.clone()),
                        error: format!(
                            "action result for node `{current}` exceeded rye-expr/1 bounds: {error}"
                        ),
                        elapsed_ms: elapsed,
                        cost,
                        effects: ExpressionFailureEffects::action(DispatchObservation::child_only(
                            dispatched_item_id,
                            child_thread_id,
                        )),
                    });
                }
                let dispatch_observation = DispatchObservation::from_success(
                    dispatched_item_id.clone(),
                    child_thread_id.clone(),
                    &val,
                );
                // Finish every expression before publishing transition effects.
                // Assign values all read the unchanged pre-node state; only after
                // the full object resolves do we build a candidate and select a
                // branch against that candidate.
                let assign = match &compiled.assign {
                    Some(assign) => match ExpressionScope::new(
                        state,
                        inputs,
                        Some(&execution),
                        Some(graph_run_id),
                    )
                    .with_result(&val)
                    .render_json(assign)
                    {
                        Ok(value) => Some(value),
                        Err(error) => {
                            return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                                item_id: Some(dispatched_item_id),
                                error: format!(
                                    "expression evaluation failed in `assign` for node `{current}`: {error}"
                                ),
                                next_on_error: resolve_next_on_error(node, cfg),
                                elapsed_ms: elapsed,
                                cost,
                                effects: ExpressionFailureEffects::action(
                                    dispatch_observation.clone(),
                                ),
                            });
                        }
                    },
                    None => None,
                };
                let mut candidate_state = state.clone();
                if let Some(assign) = assign.as_ref() {
                    merge_into(&mut candidate_state, assign);
                }
                if let Err(error) =
                    validate_runtime_value(&candidate_state, "action candidate state")
                {
                    return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                        item_id: Some(dispatched_item_id),
                        error: format!(
                            "candidate state for node `{current}` exceeded rye-expr/1 bounds: {error}"
                        ),
                        elapsed_ms: elapsed,
                        cost,
                        effects: ExpressionFailureEffects::action(dispatch_observation),
                    });
                }
                let next = match edges::evaluate_next_with_result(
                    compiled,
                    &candidate_state,
                    inputs,
                    &val,
                    Some(&execution),
                    Some(graph_run_id),
                ) {
                    Ok(next) => next,
                    Err(error) => {
                        return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                            item_id: Some(dispatched_item_id),
                            error: format!(
                                "expression evaluation failed selecting `next` for node `{current}`: {error}"
                            ),
                            next_on_error: resolve_next_on_error(node, cfg),
                            elapsed_ms: elapsed,
                            cost,
                            effects: ExpressionFailureEffects::action(dispatch_observation),
                        });
                    }
                };
                StepOutcome::ActionOk(Box::new(ActionOkOutcome {
                    item_id: dispatched_item_id,
                    result: val,
                    assign,
                    next,
                    child_thread_id,
                    cache_hit,
                    cache_write_key,
                    elapsed_ms: elapsed,
                    cost,
                }))
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
        let compiled = self.graph.compiled.node(current);
        // Consume first. An armed successor must never repeat env preflight or
        // render or dispatch the child action.
        let resumed_state = self.take_follow_state(current);
        let (checkpointed_items, checkpointed_item_refs, resumed) = match resumed_state {
            Some(state) => (
                state.iteration_snapshot,
                state.item_refs,
                state.follow_result,
            ),
            None => (None, Vec::new(), None),
        };
        if resumed.is_some() && checkpointed_items.is_none() {
            return StepOutcome::Terminal(TerminalOutcome {
                status: GraphRunStatus::Error,
                error: Some(format!(
                    "follow fanout node `{current}` resumed without iteration snapshot"
                )),
                origin: TerminalOrigin::RunControl,
                output: None,
            });
        }
        // An armed cohort resume is immutable: its iteration values are local
        // checkpoint facts and must not be re-resolved from mutable state.
        let over = if let Some(items) = checkpointed_items {
            items
        } else {
            let over = compiled
                .over
                .as_ref()
                .expect("validated follow fanout has compiled over expression");
            match ExpressionScope::new(state, inputs, Some(execution), Some(graph_run_id))
                .render_template(over)
            {
                Ok(Value::Array(items)) => items,
                Ok(other) => {
                    return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                        item_id: None,
                        error: format!(
                            "follow fanout node `{current}` `over` must evaluate to an array, got {other}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                        effects: ExpressionFailureEffects::default(),
                    });
                }
                Err(error) => {
                    return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                        item_id: None,
                        error: format!(
                            "expression evaluation failed in `over` for node `{current}`: {error}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                        effects: ExpressionFailureEffects::default(),
                    });
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
            if let Err(error) = validate_runtime_shape(&wrapper, "follow fanout resume envelope") {
                return StepOutcome::Terminal(TerminalOutcome {
                    status: GraphRunStatus::Error,
                    error: Some(format!(
                        "follow fanout resume envelope for node `{current}` exceeded rye-expr/1 bounds: {error}"
                    )),
                    origin: TerminalOrigin::RunControl,
                    output: None,
                });
            }
            let wrapper = match dispatch::classify_follow_fanout_envelope(
                wrapper,
                &checkpointed_item_refs,
            ) {
                Ok(wrapper) => wrapper,
                Err(error) => {
                    return StepOutcome::Terminal(TerminalOutcome {
                        status: GraphRunStatus::Error,
                        error: Some(format!(
                            "follow fanout node `{current}` resumed with invalid terminal envelope: {error}"
                        )),
                        origin: TerminalOrigin::RunControl,
                        output: None,
                    });
                }
            };
            let item_count = wrapper.items.len();
            let statuses = wrapper.statuses;
            let mut results = vec![Value::Null; item_count];
            let mut errors = Vec::new();
            let mut total_cost: Option<RuntimeCost> = None;
            for (index, classified) in wrapper.items.into_iter().enumerate() {
                match classified.outcome {
                    dispatch::ActionOutcome::Success(success) => {
                        results[index] = success.result;
                        if let Err(error) = add_runtime_cost(&mut total_cost, success.cost) {
                            return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                                item_id: Some(raw_item_id),
                                error: format!(
                                    "follow fanout node `{current}` item {index} reported invalid cost: {error}"
                                ),
                                elapsed_ms: start.elapsed().as_millis() as u64,
                                cost: total_cost,
                                effects: ExpressionFailureEffects::fanout(
                                    results, statuses, errors,
                                ),
                            });
                        }
                    }
                    dispatch::ActionOutcome::Failure(failure) => {
                        errors.push(ErrorRecord {
                            step,
                            node: current.to_string(),
                            error: format!("follow item {index} failed: {}", failure.diagnostic),
                        });
                        if let Err(error) = add_runtime_cost(&mut total_cost, failure.cost) {
                            return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                                item_id: Some(raw_item_id),
                                error: format!(
                                    "follow fanout node `{current}` item {index} reported invalid cost: {error}"
                                ),
                                elapsed_ms: start.elapsed().as_millis() as u64,
                                cost: total_cost,
                                effects: ExpressionFailureEffects::fanout(
                                    results, statuses, errors,
                                ),
                            });
                        }
                    }
                }
            }
            if let Err(error) =
                validate_runtime_array_shape(&results, "follow fanout collected results")
            {
                return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                    item_id: Some(raw_item_id),
                    error: format!(
                        "follow fanout results for node `{current}` exceeded rye-expr/1 bounds: {error}"
                    ),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: total_cost,
                    effects: ExpressionFailureEffects::fanout(results, statuses, errors),
                });
            }
            let route = resolve_next_on_error(node, cfg);
            let evaluate_normal_branch =
                errors.is_empty() || matches!(&route, super::outcome::NextOnError::PolicyContinue);
            let next = if evaluate_normal_branch {
                match evaluate_fanout_next(
                    compiled,
                    node,
                    state,
                    inputs,
                    execution,
                    graph_run_id,
                    &results,
                ) {
                    Ok(next) => next,
                    Err(FanoutNextError::Expression(error)) => {
                        return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                            item_id: Some(raw_item_id),
                            error: format!(
                                "expression evaluation failed selecting `next` for follow fanout node `{current}`: {error}"
                            ),
                            next_on_error: route,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            cost: total_cost,
                            effects: ExpressionFailureEffects::fanout(
                                results,
                                statuses,
                                errors,
                            ),
                        });
                    }
                    Err(FanoutNextError::Integrity(error)) => {
                        return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                            item_id: Some(raw_item_id),
                            error: format!(
                                "follow fanout candidate for node `{current}` exceeded rye-expr/1 bounds: {error}"
                            ),
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            cost: total_cost,
                            effects: ExpressionFailureEffects::fanout(
                                results,
                                statuses,
                                errors,
                            ),
                        });
                    }
                }
            } else {
                None
            };
            return StepOutcome::FollowFanoutDone(Box::new(FollowFanoutDoneOutcome {
                results,
                statuses,
                errors,
                collect_key: node.collect.clone(),
                item_id: raw_item_id,
                next,
                next_on_error: route,
                cost: total_cost,
                elapsed_ms: start.elapsed().as_millis() as u64,
            }));
        }

        if over.is_empty() {
            let next = match evaluate_fanout_next(
                compiled,
                node,
                state,
                inputs,
                execution,
                graph_run_id,
                &[],
            ) {
                Ok(next) => next,
                Err(FanoutNextError::Expression(error)) => {
                    return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                        item_id: Some(raw_item_id),
                        error: format!(
                            "expression evaluation failed selecting `next` for follow fanout node `{current}`: {error}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                        effects: ExpressionFailureEffects::fanout(
                            Vec::new(),
                            Vec::new(),
                            Vec::new(),
                        ),
                    });
                }
                Err(FanoutNextError::Integrity(error)) => {
                    return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                        item_id: Some(raw_item_id),
                        error: format!(
                            "follow fanout candidate for node `{current}` exceeded rye-expr/1 bounds: {error}"
                        ),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                        effects: ExpressionFailureEffects::fanout(
                            Vec::new(),
                            Vec::new(),
                            Vec::new(),
                        ),
                    });
                }
            };
            return StepOutcome::FollowFanoutDone(Box::new(FollowFanoutDoneOutcome {
                results: vec![],
                statuses: vec![],
                errors: vec![],
                collect_key: node.collect.clone(),
                item_id: raw_item_id,
                next,
                next_on_error: resolve_next_on_error(node, cfg),
                cost: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
            }));
        }
        if let Err(env_err) =
            env_preflight::check_env_requires(&cfg.env_requires, &node.env_requires)
        {
            return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                item_id: Some(raw_item_id),
                error: format!("env preflight failed: {env_err}"),
                next_on_error: resolve_next_on_error(node, cfg),
                elapsed_ms: start.elapsed().as_millis() as u64,
                cost: None,
            });
        }
        // Do not reserve the untrusted `over` cardinality up front. The
        // aggregate budget below will usually admit far fewer child objects
        // than the expression container ceiling when their payloads are large.
        let mut children = Vec::new();
        let mut launch_budget = RuntimeJsonArrayBudget::new("follow fanout launch cohort");
        for (index, item) in over.iter().enumerate() {
            let scope = ExpressionScope::new(state, inputs, Some(execution), Some(graph_run_id))
                .with_foreach(&var, item);
            let mut action = match scope.render_action(
                compiled
                    .action
                    .as_ref()
                    .expect("validated follow fanout has compiled action"),
            ) {
                Ok(v) => v,
                Err(error) => {
                    return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                        item_id: Some(raw_item_id),
                        error: format!(
                            "expression evaluation failed in follow fanout action: {error}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                        effects: ExpressionFailureEffects::default(),
                    });
                }
            };
            let facets = match &compiled.facets {
                Some(value) => match scope.render_json(value) {
                    Ok(v) => Some(v),
                    Err(error) => {
                        return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                            item_id: Some(raw_item_id),
                            error: format!(
                                "expression evaluation failed in follow fanout facets: {error}"
                            ),
                            next_on_error: resolve_next_on_error(node, cfg),
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            cost: None,
                            effects: ExpressionFailureEffects::default(),
                        });
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
                return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                    item_id: None,
                    error: format!(
                        "follow fanout item {index} has missing or empty rendered item_id"
                    ),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                });
            }
            let ref_bindings = match action.get("ref_bindings") {
                Some(value) => {
                    match serde_json::from_value::<BTreeMap<String, String>>(value.clone()) {
                        Ok(bindings) => bindings,
                        Err(error) => {
                            return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                                item_id: Some(item_ref),
                                error: format!(
                                    "follow fanout item {index} has invalid ref_bindings: {error}"
                                ),
                                next_on_error: resolve_next_on_error(node, cfg),
                                elapsed_ms: start.elapsed().as_millis() as u64,
                                cost: None,
                            });
                        }
                    }
                }
                None => {
                    return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                        item_id: Some(item_ref),
                        error: format!(
                            "follow fanout item {index} is missing required ref_bindings"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                    });
                }
            };

            // A bounded expression result per child is not enough: a large
            // cohort of individually valid child specs can still retain an
            // unbounded aggregate before the daemon handoff. Build the exact
            // callback-shaped value once, account it under one cohort budget,
            // then move its fields into FollowChildSpec without deep-cloning
            // parameters or facets.
            let parameters = action
                .as_object_mut()
                .and_then(|action| action.remove("params"))
                .unwrap_or_else(|| json!({}));
            let mut child_fields = Map::new();
            child_fields.insert("item_ref".to_string(), Value::String(item_ref));
            child_fields.insert(
                "ref_bindings".to_string(),
                serde_json::to_value(&ref_bindings)
                    .expect("validated ref bindings must serialize as JSON"),
            );
            child_fields.insert("parameters".to_string(), parameters);
            if let Some(facets) = facets {
                child_fields.insert("facets".to_string(), facets);
            }
            let mut bounded_child = Value::Object(child_fields);
            if let Err(error) = launch_budget.append(&bounded_child) {
                return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                    item_id: Some(raw_item_id),
                    error: format!(
                        "follow fanout launch cohort for node `{current}` exceeded rye-expr/1 bounds at item {index}: {error}"
                    ),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                    effects: ExpressionFailureEffects::default(),
                });
            }

            let child_fields = bounded_child
                .as_object_mut()
                .expect("follow child budget value was constructed as an object");
            let Value::String(item_ref) = child_fields
                .remove("item_ref")
                .expect("bounded follow child carries item_ref")
            else {
                unreachable!("bounded follow child item_ref was constructed as a string")
            };
            let parameters = child_fields
                .remove("parameters")
                .expect("bounded follow child carries parameters");
            let facets = child_fields.remove("facets");
            children.push(ryeos_runtime::callback::FollowChildSpec {
                item_ref,
                ref_bindings,
                parameters,
                facets,
            });
        }
        if !checkpointed_item_refs.is_empty()
            && (children.len() != checkpointed_item_refs.len()
                || children
                    .iter()
                    .zip(checkpointed_item_refs.iter())
                    .any(|(child, checkpointed)| child.item_ref != *checkpointed))
        {
            return StepOutcome::Terminal(TerminalOutcome {
                status: GraphRunStatus::Error,
                error: Some(format!(
                    "follow fanout node `{current}` rendered child refs that differ from its checkpointed ordered item_refs"
                )),
                origin: TerminalOrigin::RunControl,
                output: None,
            });
        }
        let width = match node.max_concurrency.map(u32::try_from).transpose() {
            Ok(width) => width,
            Err(_) => {
                return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                    item_id: None,
                    error: format!(
                        "follow fanout node `{current}` max_concurrency does not fit in u32"
                    ),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                })
            }
        };
        StepOutcome::FollowFanoutSuspend(Box::new(FollowFanoutSuspendOutcome {
            children,
            width,
            iteration_snapshot: over,
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_fanout_next(
    compiled: &crate::compiled_graph::CompiledNode,
    node: &GraphNode,
    state: &Value,
    inputs: &Value,
    execution: &Value,
    graph_run_id: &str,
    results: &[Value],
) -> Result<Option<String>, FanoutNextError> {
    validate_runtime_array_shape(results, "follow fanout branch results")
        .map_err(FanoutNextError::Integrity)?;
    let mut candidate = state.clone();
    if let Some(collect) = &node.collect {
        if !candidate.is_object() {
            candidate = Value::Object(serde_json::Map::new());
        }
        candidate
            .as_object_mut()
            .unwrap()
            .insert(collect.clone(), Value::Array(results.to_vec()));
    }
    validate_runtime_shape(&candidate, "follow fanout candidate state")
        .map_err(FanoutNextError::Integrity)?;
    let result = Value::Array(results.to_vec());
    edges::evaluate_next_with_result(
        compiled,
        &candidate,
        inputs,
        &result,
        Some(execution),
        Some(graph_run_id),
    )
    .map_err(FanoutNextError::Expression)
}

enum FanoutNextError {
    Integrity(ryeos_runtime::ExpressionError),
    Expression(ryeos_runtime::ExpressionError),
}
