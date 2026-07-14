use serde_json::{json, Value};

use crate::edges;
use crate::knowledge;
use crate::model::*;
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::TerminalCompletion;

use super::events::node_ref;
use super::outcome::*;
use super::{hash_json_value, merge_into, Walker};

impl Walker {
    /// D13: The ONLY function allowed to:
    ///   - emit step lifecycle events
    ///   - write a node receipt
    ///   - write a checkpoint
    ///   - emit `GraphCompleted` on terminal
    ///   - finalize the thread on terminal
    ///
    /// `commit_step` MUST be called exactly once per loop iteration.
    pub(super) async fn commit_step(&self, input: CommitStepInput<'_>) -> CommitResult {
        let CommitStepInput {
            graph_run_id,
            step,
            current,
            state,
            receipts,
            suppressed_errors,
            outcome,
            guard,
            inputs,
            execution,
        } = input;
        match outcome {
            StepOutcome::FollowSuspend {
                ref item_id,
                ref params,
            } => {
                // Suspend lifecycle: started + a DISTINCT suspended event + the
                // pending-follow checkpoint, THEN the handoff. Deliberately NO node
                // receipt and NO graph_step_completed — the child result does not
                // exist yet; those are emitted on resume.
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                self.emit_graph_follow_suspended(graph_run_id, step, current, item_id, None)
                    .await;

                // Checkpoint at the follow node so a re-entry re-drives the suspend
                // idempotently (by follow_key). A checkpoint failure is a hard error
                // like any other resume-correctness failure.
                if let Err(e) = self
                    .write_follow_checkpoint(
                        graph_run_id,
                        current,
                        step,
                        state,
                        suppressed_errors,
                        None,
                    )
                    .await
                {
                    let msg = format!("follow checkpoint write failed: {e}");
                    return self
                        .commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step,
                            state,
                            suppressed_errors,
                            base_status: "error",
                            error: Some(&msg),
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await;
                }

                // Hand off: launch the detached child and suspend this graph. A
                // failed handoff settles a terminal error, never `continued` with no
                // child behind it.
                match self
                    .client
                    .spawn_follow_child(
                        graph_run_id,
                        current,
                        step as i64,
                        item_id,
                        params.clone(),
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        // The daemon settled this thread `continued` inside the
                        // handoff (it created the follow-resume successor), so do NOT
                        // finalize. The pending-follow checkpoint is the resume point.
                        guard.finalized = true;
                        let (agg_cost, node_costs) = {
                            let acc = self.accounting.lock().unwrap();
                            (acc.total.clone(), acc.nodes.clone())
                        };
                        CommitResult::Terminate(Box::new(GraphResult {
                            success: false,
                            graph_id: self.graph.graph_id.clone(),
                            definition_ref: self.graph.definition_ref.clone(),
                            definition_hash: self.graph.definition_hash.clone(),
                            graph_run_id: graph_run_id.to_string(),
                            status: "continued".into(),
                            steps: step,
                            state: state.clone(),
                            result: None,
                            errors_suppressed: if suppressed_errors.is_empty() {
                                None
                            } else {
                                Some(suppressed_errors.len())
                            },
                            errors: if suppressed_errors.is_empty() {
                                None
                            } else {
                                Some(suppressed_errors.clone())
                            },
                            error: None,
                            cost: agg_cost,
                            node_costs,
                        }))
                    }
                    Err(e) => {
                        let msg = format!("follow handoff failed: {e}");
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step,
                            state,
                            suppressed_errors,
                            base_status: "error",
                            error: Some(&msg),
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }
            StepOutcome::FollowFanoutSuspend {
                children,
                width,
                iteration_snapshot,
            } => {
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                self.emit_graph_follow_suspended(
                    graph_run_id,
                    step,
                    current,
                    "cohort",
                    Some(children.len()),
                )
                .await;
                if let Err(e) = self
                    .write_follow_checkpoint(
                        graph_run_id,
                        current,
                        step,
                        state,
                        suppressed_errors,
                        Some(&iteration_snapshot),
                    )
                    .await
                {
                    let msg = format!("follow checkpoint write failed: {e}");
                    return self
                        .commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step,
                            state,
                            suppressed_errors,
                            base_status: "error",
                            error: Some(&msg),
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await;
                }
                match self
                    .client
                    .spawn_follow_children(
                        graph_run_id,
                        current,
                        step as i64,
                        children,
                        width,
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        guard.finalized = true;
                        let acc = self.accounting.lock().unwrap();
                        CommitResult::Terminate(Box::new(GraphResult {
                            success: false,
                            graph_id: self.graph.graph_id.clone(),
                            definition_ref: self.graph.definition_ref.clone(),
                            definition_hash: self.graph.definition_hash.clone(),
                            graph_run_id: graph_run_id.to_string(),
                            status: "continued".into(),
                            steps: step,
                            state: state.clone(),
                            result: None,
                            errors_suppressed: (!suppressed_errors.is_empty())
                                .then_some(suppressed_errors.len()),
                            errors: (!suppressed_errors.is_empty())
                                .then_some(suppressed_errors.clone()),
                            error: None,
                            cost: acc.total.clone(),
                            node_costs: acc.nodes.clone(),
                        }))
                    }
                    Err(e) => {
                        let msg = format!("follow cohort handoff failed: {e}");
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step,
                            state,
                            suppressed_errors,
                            base_status: "error",
                            error: Some(&msg),
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }
            StepOutcome::RetryScheduled {
                item_id,
                error,
                failed_attempt,
                total_attempts,
                delay_ms,
                elapsed_ms,
                cost,
            } => {
                // A failed attempt that will be retried: the same step lifecycle
                // a soft error emits (so the attempt is visible in the braid) plus
                // a graph_node_retry milestone, then a checkpoint that re-points at
                // THIS node with the incremented attempt count, then the backoff.
                // The error is NOT pushed to suppressed_errors — only the final
                // exhausted outcome routes through on_error/continue.
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                self.emit_tool_call_start(graph_run_id, step, current, &item_id)
                    .await;
                self.emit_tool_call_result(graph_run_id, step, current, &item_id, "error")
                    .await;

                // A native child may have spent tokens before failing this
                // attempt — account for it, exactly like a soft error.
                if let Some(c) = &cost {
                    self.accounting
                        .lock()
                        .unwrap()
                        .record(current, step, &item_id, c.clone());
                }

                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    definition_ref: self.graph.definition_ref.clone(),
                    definition_hash: self.graph.definition_hash.clone(),
                    result_hash: None,
                    cache_hit: false,
                    elapsed_ms,
                    error: Some(error.clone()),
                    cost: cost.clone(),
                    fanout: None,
                });
                self.write_node_receipt_or_warn(graph_run_id, receipts.last().unwrap())
                    .await;

                self.emit_graph_node_retry(
                    graph_run_id,
                    step,
                    current,
                    &item_id,
                    failed_attempt,
                    total_attempts,
                    delay_ms,
                    &error,
                )
                .await;
                self.emit_graph_step_completed(graph_run_id, step, current, "retry", Some(&error))
                    .await;

                // Checkpoint re-points at THIS node (same cursor, incremented
                // attempt) so a segment cut or crash during the backoff resumes
                // with the count instead of restarting the attempts.
                let advance = self
                    .write_checkpoint_or_error(
                        graph_run_id,
                        current,
                        step + 1,
                        state,
                        suppressed_errors,
                        guard,
                        failed_attempt,
                    )
                    .await;
                if let CommitResult::Advance { .. } = &advance {
                    // Plain sleep: a daemon cancel kills the walker's pgid, which
                    // kills this sleeping task — no custom cancellation plumbing.
                    if delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                }
                advance
            }
            StepOutcome::Terminal { status, error } => {
                self.commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step,
                    state,
                    suppressed_errors,
                    base_status: status,
                    error: error.as_deref(),
                    guard,
                    current_node_id: current,
                    inputs,
                    execution,
                })
                .await
            }

            StepOutcome::GateTaken { target } => {
                // Gate lifecycle: graph_step_started → graph_branch_taken → graph_step_completed → checkpoint
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                self.emit_graph_branch_taken(graph_run_id, step, current, target.as_deref())
                    .await;
                self.emit_graph_step_completed(graph_run_id, step, current, "ok", None)
                    .await;
                self.fire_graph_hooks(
                    "graph_step_completed",
                    self.step_hook_context(graph_run_id, current, step, "ok", None, state),
                )
                .await;

                match target {
                    Some(next_node) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            &next_node,
                            next_step,
                            state,
                            suppressed_errors,
                            guard,
                            0,
                        )
                        .await
                    }
                    None => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: "completed",
                            error: None,
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }

            StepOutcome::ForeachDone {
                ref results,
                ref collect_key,
                ref var_name,
                ref assign_delta,
                ref errors,
                ref next,
                ref item_id,
                ref cost,
            } => {
                // Foreach lifecycle: graph_step_started → (per-iteration
                // graph_foreach_iteration events) → graph_step_completed →
                // checkpoint
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;

                // Foreach aggregates all iteration child costs into one
                // record for the node (per-iteration accounting can be
                // added later if needed).
                if let Some(c) = cost {
                    self.accounting
                        .lock()
                        .unwrap()
                        .record(current, step, item_id, c.clone());
                }

                // Emit per-iteration events from the aggregated results.
                // Each result corresponds to one item that was iterated over.
                for (i, _result) in results.iter().enumerate() {
                    let r = self
                        .client
                        .append_runtime_event(
                            RuntimeEventType::GraphForeachIteration,
                            json!({
                                "graph_run_id": graph_run_id,
                                "definition_ref": &self.graph.definition_ref,
                                "definition_hash": &self.graph.definition_hash,
                                "node": current,
                                "node_ref": node_ref(&self.graph.definition_ref, current),
                                "step": step,
                                "iteration": i,
                                "total": results.len(),
                            }),
                        )
                        .await;
                    self.record_callback_warning("graph_foreach_iteration", r);
                }

                // Merge foreach results into state.
                if let Some(ref key) = collect_key {
                    if let Some(obj) = state.as_object_mut() {
                        obj.insert(key.clone(), Value::Array(results.clone()));
                    }
                }
                // Commit accumulated foreach `assign` mutations.
                merge_into(state, assign_delta);
                // Remove the iteration variable from state.
                if let Some(obj) = state.as_object_mut() {
                    obj.remove(var_name);
                }

                // Surface per-item failures (continue policy) as suppressed
                // errors so the run terminates `completed_with_errors`.
                for err in errors {
                    suppressed_errors.push(err.clone());
                }

                self.emit_graph_step_completed(graph_run_id, step, current, "ok", None)
                    .await;
                self.fire_graph_hooks(
                    "graph_step_completed",
                    self.step_hook_context(graph_run_id, current, step, "ok", None, state),
                )
                .await;

                match next {
                    Some(next_node) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            next_node,
                            next_step,
                            state,
                            suppressed_errors,
                            guard,
                            0,
                        )
                        .await
                    }
                    None => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: "completed",
                            error: None,
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }

            StepOutcome::FollowFanoutDone {
                results,
                statuses,
                errors,
                assign_delta,
                collect_key,
                var_name,
                item_id,
                next,
                next_on_error,
                cost,
                elapsed_ms,
            } => {
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                if let Some(key) = collect_key {
                    if let Some(obj) = state.as_object_mut() {
                        obj.insert(key, Value::Array(results.clone()));
                    }
                }
                merge_into(state, &assign_delta);
                if let Some(obj) = state.as_object_mut() {
                    obj.remove(&var_name);
                }
                if let Some(c) = &cost {
                    // Match classic follow resume: the detached children's
                    // aggregate cost is attributed once to the parent node.
                    self.accounting
                        .lock()
                        .unwrap()
                        .record(current, step, &item_id, c.clone());
                }
                let diagnostic = (!errors.is_empty()).then(|| {
                    errors
                        .iter()
                        .map(|e| e.error.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                });
                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    definition_ref: self.graph.definition_ref.clone(),
                    definition_hash: self.graph.definition_hash.clone(),
                    result_hash: Some(hash_json_value(
                        &json!({"results": results, "statuses": statuses}),
                    )),
                    cache_hit: false,
                    elapsed_ms,
                    error: diagnostic.clone(),
                    cost: cost.clone(),
                    fanout: Some(crate::model::FanoutReceiptSummary {
                        statuses: statuses.clone(),
                        failed: statuses
                            .iter()
                            .filter(|status| status.as_str() == "failed")
                            .count(),
                        expected: statuses.len(),
                        // Results remain represented by result_hash; receipts do not have
                        // an explicit local-content policy permitting raw result persistence.
                        results: None,
                    }),
                });
                self.write_node_receipt_or_warn(graph_run_id, receipts.last().unwrap())
                    .await;
                let status = if errors.is_empty() { "ok" } else { "error" };
                self.emit_graph_step_completed(
                    graph_run_id,
                    step,
                    current,
                    status,
                    diagnostic.as_deref(),
                )
                .await;
                self.fire_graph_hooks(
                    "graph_step_completed",
                    self.step_hook_context(
                        graph_run_id,
                        current,
                        step,
                        status,
                        diagnostic.as_deref(),
                        state,
                    ),
                )
                .await;

                let target = if errors.is_empty() {
                    next
                } else {
                    match next_on_error {
                        NextOnError::Redirect(target) => Some(target),
                        NextOnError::PolicyContinue => {
                            suppressed_errors.extend(errors);
                            edges::evaluate_next_with_result(
                                self.graph.config.nodes.get(current).unwrap(),
                                state,
                                inputs,
                                &Value::Array(results.clone()),
                                Some(execution),
                                Some(graph_run_id),
                            )
                        }
                        NextOnError::PolicyFail => {
                            return self
                                .commit_terminal(CommitTerminalInput {
                                    graph_run_id,
                                    steps: step,
                                    state,
                                    suppressed_errors,
                                    base_status: "error",
                                    error: diagnostic.as_deref(),
                                    guard,
                                    current_node_id: current,
                                    inputs,
                                    execution,
                                })
                                .await;
                        }
                    }
                };
                self.emit_graph_branch_taken(graph_run_id, step, current, target.as_deref())
                    .await;
                match target {
                    Some(target) => {
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            &target,
                            step + 1,
                            state,
                            suppressed_errors,
                            guard,
                            0,
                        )
                        .await
                    }
                    None => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: "completed",
                            error: None,
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }

            StepOutcome::ActionOk {
                ref item_id,
                ref result,
                ref assign,
                ref next,
                cache_hit,
                elapsed_ms,
                ref cost,
            } => {
                // R3 fence order:
                // graph_step_started → tool_call_start → (dispatch in run_node_body) →
                // tool_call_result → state mutation → receipt → graph_step_completed → checkpoint
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                self.emit_tool_call_start(graph_run_id, step, current, item_id)
                    .await;
                self.emit_tool_call_result(graph_run_id, step, current, item_id, "ok")
                    .await;

                // State mutation: `assign` was already interpolated in
                // run_action_body (a failure there became a node error
                // routed through on_error), so here we only merge the
                // resolved value — no `${...}` template can reach state.
                if let Some(assign_val) = assign {
                    merge_into(state, assign_val);
                }

                // Record node cost into the run accumulator before the
                // receipt so the receipt carries it too. A cache hit or a
                // subprocess leaf has no cost and contributes nothing.
                if let Some(c) = cost {
                    self.accounting
                        .lock()
                        .unwrap()
                        .record(current, step, item_id, c.clone());
                }

                // Receipt
                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    definition_ref: self.graph.definition_ref.clone(),
                    definition_hash: self.graph.definition_hash.clone(),
                    result_hash: Some(hash_json_value(result)),
                    cache_hit,
                    elapsed_ms,
                    error: None,
                    cost: cost.clone(),
                    fanout: None,
                });
                self.write_node_receipt_or_warn(graph_run_id, receipts.last().unwrap())
                    .await;

                self.emit_graph_step_completed(graph_run_id, step, current, "ok", None)
                    .await;
                self.fire_graph_hooks(
                    "graph_step_completed",
                    self.step_hook_context(graph_run_id, current, step, "ok", None, state),
                )
                .await;

                match next {
                    Some(next_node) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            next_node,
                            next_step,
                            state,
                            suppressed_errors,
                            guard,
                            0,
                        )
                        .await
                    }
                    None => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: "completed",
                            error: None,
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }

            StepOutcome::LeafSoftError {
                ref item_id,
                ref error,
                ref next_on_error,
                elapsed_ms,
                ref cost,
            } => {
                // Soft error: dispatch succeeded but leaf returned error, OR
                // the child succeeded (with cost) and `assign` then failed.
                // graph_step_started → tool_call_start → tool_call_result(error) → graph_step_completed(error) → [redirect/continue/fail]
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                self.emit_tool_call_start(graph_run_id, step, current, item_id)
                    .await;
                self.emit_tool_call_result(graph_run_id, step, current, item_id, "error")
                    .await;

                // A cost-bearing child that then errored (or whose assign
                // failed) still spent tokens — account for it.
                if let Some(c) = cost {
                    self.accounting
                        .lock()
                        .unwrap()
                        .record(current, step, item_id, c.clone());
                }

                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    definition_ref: self.graph.definition_ref.clone(),
                    definition_hash: self.graph.definition_hash.clone(),
                    result_hash: None,
                    cache_hit: false,
                    elapsed_ms,
                    error: Some(error.clone()),
                    cost: cost.clone(),
                    fanout: None,
                });

                self.write_node_receipt_or_warn(graph_run_id, receipts.last().unwrap())
                    .await;

                self.emit_graph_step_completed(graph_run_id, step, current, "error", Some(error))
                    .await;
                self.fire_graph_hooks(
                    "graph_step_completed",
                    self.step_hook_context(
                        graph_run_id,
                        current,
                        step,
                        "error",
                        Some(error),
                        state,
                    ),
                )
                .await;

                match next_on_error {
                    NextOnError::Redirect(target) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            target,
                            next_step,
                            state,
                            suppressed_errors,
                            guard,
                            0,
                        )
                        .await
                    }
                    NextOnError::PolicyContinue => {
                        suppressed_errors.push(ErrorRecord {
                            step,
                            node: current.to_string(),
                            error: error.clone(),
                        });
                        match edges::evaluate_next(
                            self.graph.config.nodes.get(current).unwrap(),
                            state,
                            inputs,
                            Some(execution),
                            Some(graph_run_id),
                        ) {
                            Some(next_node) => {
                                let next_step = step + 1;
                                // Checkpoint on continue-advance too
                                self.write_checkpoint_or_error(
                                    graph_run_id,
                                    &next_node,
                                    next_step,
                                    state,
                                    suppressed_errors,
                                    guard,
                                    0,
                                )
                                .await
                            }
                            None => {
                                self.commit_terminal(CommitTerminalInput {
                                    graph_run_id,
                                    steps: step + 1,
                                    state,
                                    suppressed_errors,
                                    base_status: "completed",
                                    error: None,
                                    guard,
                                    current_node_id: current,
                                    inputs,
                                    execution,
                                })
                                .await
                            }
                        }
                    }
                    NextOnError::PolicyFail => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step,
                            state,
                            suppressed_errors,
                            base_status: "error",
                            error: Some(&format!("node '{}' failed: {}", current, error)),
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }

            StepOutcome::DispatchHardError {
                item_id,
                ref error,
                ref next_on_error,
                elapsed_ms,
                ref cost,
            } => {
                // Hard error: dispatch failed before leaf returned (no cost),
                // OR a foreach abandoned under fail/redirect after spending
                // cost on completed iterations (cost present).
                let item_str = item_id.as_deref().unwrap_or("");
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                if !item_str.is_empty() {
                    self.emit_tool_call_start(graph_run_id, step, current, item_str)
                        .await;
                }
                if !item_str.is_empty() {
                    self.emit_tool_call_result(
                        graph_run_id,
                        step,
                        current,
                        item_str,
                        "dispatch_failed",
                    )
                    .await;
                }

                if let Some(c) = cost {
                    self.accounting
                        .lock()
                        .unwrap()
                        .record(current, step, item_str, c.clone());
                }

                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    definition_ref: self.graph.definition_ref.clone(),
                    definition_hash: self.graph.definition_hash.clone(),
                    result_hash: None,
                    cache_hit: false,
                    elapsed_ms,
                    error: Some(error.clone()),
                    cost: cost.clone(),
                    fanout: None,
                });

                self.write_node_receipt_or_warn(graph_run_id, receipts.last().unwrap())
                    .await;

                self.emit_graph_step_completed(graph_run_id, step, current, "error", Some(error))
                    .await;
                self.fire_graph_hooks(
                    "graph_step_completed",
                    self.step_hook_context(
                        graph_run_id,
                        current,
                        step,
                        "error",
                        Some(error),
                        state,
                    ),
                )
                .await;

                match next_on_error {
                    NextOnError::Redirect(target) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            target,
                            next_step,
                            state,
                            suppressed_errors,
                            guard,
                            0,
                        )
                        .await
                    }
                    NextOnError::PolicyContinue => {
                        suppressed_errors.push(ErrorRecord {
                            step,
                            node: current.to_string(),
                            error: error.clone(),
                        });
                        match edges::evaluate_next(
                            self.graph.config.nodes.get(current).unwrap(),
                            state,
                            inputs,
                            Some(execution),
                            Some(graph_run_id),
                        ) {
                            Some(next_node) => {
                                let next_step = step + 1;
                                self.write_checkpoint_or_error(
                                    graph_run_id,
                                    &next_node,
                                    next_step,
                                    state,
                                    suppressed_errors,
                                    guard,
                                    0,
                                )
                                .await
                            }
                            None => {
                                self.commit_terminal(CommitTerminalInput {
                                    graph_run_id,
                                    steps: step + 1,
                                    state,
                                    suppressed_errors,
                                    base_status: "completed",
                                    error: None,
                                    guard,
                                    current_node_id: current,
                                    inputs,
                                    execution,
                                })
                                .await
                            }
                        }
                    }
                    NextOnError::PolicyFail => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step,
                            state,
                            suppressed_errors,
                            base_status: "error",
                            error: Some(&format!("node '{}' failed: {}", current, error)),
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                }
            }
        }
    }

    /// Commit a terminal outcome: emit GraphCompleted, write
    /// transcript, publish artifact, finalize thread. Called from
    /// `commit_step` for all terminal variants.
    pub(super) async fn commit_terminal(&self, input: CommitTerminalInput<'_>) -> CommitResult {
        let CommitTerminalInput {
            graph_run_id,
            steps,
            state,
            suppressed_errors,
            base_status,
            error,
            guard,
            current_node_id,
            inputs,
            execution,
        } = input;
        let (success, status) = match base_status {
            "completed" => {
                let s = if suppressed_errors.is_empty() {
                    "completed".to_string()
                } else {
                    "completed_with_errors".to_string()
                };
                (true, s)
            }
            "max_steps_exceeded" => (false, "max_steps_exceeded".to_string()),
            "cancelled" => (false, "cancelled".to_string()),
            "killed" => (false, "killed".to_string()),
            _ => (false, "error".to_string()),
        };

        // Output: ONLY populated when a return node declares an explicit
        // `output:` template. Otherwise `result` stays `None` and
        // consumers read from `state` — so `result` never duplicates
        // `state`.
        //
        // Use the current cursor (deterministic) instead of
        // nodes.values().find() which iterates HashMap in random order.
        let mut output_error: Option<String> = None;
        let output: Option<Value> = if success && base_status == "completed" {
            self.graph
                .config
                .nodes
                .get(current_node_id)
                .filter(|n| n.node_type == NodeType::Return)
                .and_then(|n| n.output.as_ref())
                .and_then(|tpl| {
                    let ctx = WalkContext {
                        state: state.clone(),
                        inputs: inputs.clone(),
                        result: None,
                        execution: Some(execution.clone()),
                        graph_run_id: Some(graph_run_id.to_string()),
                    };
                    // `tpl` is a `Value` (scalar, map, or list);
                    // `interpolate` recurses through all of them.
                    match ryeos_runtime::interpolate(tpl, &ctx.as_context()) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            // A return node that can't resolve its declared
                            // `output` did NOT produce a valid result — fail
                            // the run rather than emit a raw `${...}` template
                            // as the graph's result.
                            output_error = Some(format!(
                                "interpolation error in `output` for node `{current_node_id}`: {e:#}"
                            ));
                            None
                        }
                    }
                })
        } else {
            None
        };

        // A failed output interpolation overrides an otherwise-successful
        // terminal into an error terminal carrying the diagnostic.
        let (success, status, output, error_owned) = match output_error {
            Some(oe) => (false, "error".to_string(), None, Some(oe)),
            None => (success, status, output, error.map(String::from)),
        };

        // Snapshot accounting accumulated across the run. Even a failed
        // graph reports the cost spent before it failed — the accumulator
        // holds every cost-bearing node that committed prior to terminal.
        let (agg_cost, node_costs) = {
            let acc = self.accounting.lock().unwrap();
            (acc.total.clone(), acc.nodes.clone())
        };

        let graph_result = GraphResult {
            success,
            graph_id: self.graph.graph_id.clone(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            graph_run_id: graph_run_id.to_string(),
            status: status.clone(),
            steps,
            state: state.clone(),
            result: output,
            errors_suppressed: if suppressed_errors.is_empty() {
                None
            } else {
                Some(suppressed_errors.len())
            },
            errors: if suppressed_errors.is_empty() {
                None
            } else {
                Some(std::mem::take(suppressed_errors))
            },
            error: error_owned,
            cost: agg_cost,
            node_costs,
        };

        // Emit GraphCompleted event.
        {
            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphCompleted,
                    json!({
                        "graph_id": &self.graph.graph_id,
                        "definition_ref": &self.graph.definition_ref,
                        "definition_hash": &self.graph.definition_hash,
                        "graph_run_id": graph_run_id,
                        "status": &status,
                        "steps": steps,
                    }),
                )
                .await;
            self.record_callback_warning("graph_completed", r);
        }

        // Fire graph_completed observer hooks at the terminal.
        self.fire_graph_hooks(
            "graph_completed",
            json!({
                "event": "graph_completed",
                "graph_id": &self.graph.graph_id,
                "graph_run_id": graph_run_id,
                "status": &status,
                "steps": steps,
                "success": success,
                "state": &graph_result.state,
                "inputs": inputs,
            }),
        )
        .await;

        // Write transcript.
        let r = knowledge::write_knowledge_transcript(
            &self.project_path,
            &self.graph.graph_id,
            graph_run_id,
            &serde_json::to_string(&graph_result).unwrap_or_default(),
        );
        self.record_callback_warning("write_knowledge_transcript", r);

        // Publish artifact.
        let r = self
            .client
            .publish_artifact(json!({
                "artifact_type": "graph_transcript",
                "uri": format!("graph://{}/runs/{}", self.graph.graph_id, graph_run_id),
            }))
            .await;
        self.record_callback_warning("publish_artifact", r.map(|_| ()));

        // Finalize thread. A cooperative cancel/kill settles the THREAD as
        // cancelled/killed (a distinct terminal an operator can tell apart from a
        // failure), not the coarse completed/failed split every other terminal
        // collapses to. `TerminalCompletion.cost` is raw JSON on the callback
        // wire — serialize the typed aggregate.
        let thread_status = match status.as_str() {
            "cancelled" => "cancelled",
            "killed" => "killed",
            _ if success => "completed",
            _ => "failed",
        };
        let completion = TerminalCompletion {
            status: thread_status.to_string(),
            outcome_code: Some(
                match thread_status {
                    "completed" => "success",
                    "cancelled" => "cancelled",
                    "killed" => "killed",
                    _ => "failed",
                }
                .to_string(),
            ),
            result: graph_result.result.clone(),
            error: graph_result.error.as_ref().map(|e| json!(e)),
            cost: graph_result
                .cost
                .as_ref()
                .and_then(|c| serde_json::to_value(c).ok()),
            // A graph's return value is its `result`; it has no separate structured
            // outputs. Send a snapshot of accumulated callback-drift warnings so a
            // follow parent (which consumes THIS envelope, not the later stdout
            // RuntimeResult) sees the same warnings a live dispatch would.
            outputs: Value::Null,
            warnings: self.warnings.lock().unwrap().clone(),
        };
        let r = self.client.finalize_thread(completion).await;
        self.record_callback_warning("finalize_thread", r.map(|_| ()));
        guard.finalized = true;

        CommitResult::Terminate(Box::new(graph_result))
    }
}
