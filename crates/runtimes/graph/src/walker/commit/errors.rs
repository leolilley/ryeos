use super::*;

impl Walker {
    pub(super) async fn commit_expression_failed(
        &self,
        input: CommitStepContext<'_>,
        outcome: ExpressionFailedOutcome,
    ) -> CommitResult {
        self.commit_failed_with_status(input, outcome, GraphToolCallStatus::ExpressionFailed)
            .await
    }

    pub(super) async fn commit_integrity_failed(
        &self,
        input: CommitStepContext<'_>,
        outcome: IntegrityFailedOutcome,
    ) -> CommitResult {
        self.commit_failed_with_status(
            input,
            outcome.into(),
            GraphToolCallStatus::IntegrityFailed,
        )
        .await
    }

    async fn commit_failed_with_status(
        &self,
        input: CommitStepContext<'_>,
        outcome: ExpressionFailedOutcome,
        tool_status: GraphToolCallStatus,
    ) -> CommitResult {
        let CommitStepContext {
            graph_run_id,
            step,
            current,
            state,
            suppressed_errors,
            guard,
            inputs,
            execution,
            cache: _,
        } = input;
        let ExpressionFailedOutcome {
            item_id,
            error,
            next_on_error,
            elapsed_ms,
            cost,
            effects,
        } = outcome;
        let error = &error;
        let next_on_error = &next_on_error;
        let cost = &cost;
        let item_id = item_id.as_deref().unwrap_or("");
        self.emit_graph_step_started(graph_run_id, step, current)
            .await;
        if !item_id.is_empty() {
            self.emit_tool_call_start(graph_run_id, step, current, item_id)
                .await;
            self.emit_tool_call_result(
                graph_run_id,
                step,
                current,
                item_id,
                tool_status,
            )
            .await;
        }
        if let Some(foreach) = &effects.foreach {
            self.emit_foreach_iteration_statuses(
                graph_run_id,
                current,
                step,
                &foreach.statuses,
                foreach.total_items,
            )
            .await;
        }
        for observation in &effects.observations {
            self.emit_dispatch_observation(current, step, observation)
                .await;
        }

        if let Some(cost) = cost {
            self.record_node_cost(current, step, item_id, cost.clone());
        }
        let result_hash = match effects
            .fanout
            .as_ref()
            .map(|fanout| {
                hash_json_value(&json!({
                    "results": &fanout.results,
                    "statuses": &fanout.statuses,
                }))
            })
            .transpose()
        {
            Ok(hash) => hash,
            Err(canonical_error) => {
                let message = format!(
                    "failed to canonicalize failed node result for `{current}`: {canonical_error}"
                );
                return self
                    .commit_terminal(CommitTerminalInput {
                        graph_run_id,
                        steps: step,
                        state,
                        suppressed_errors,
                        base_status: GraphRunStatus::Error,
                        error: Some(&message),
                        output: None,
                        guard,
                        current_node_id: current,
                        inputs,
                        execution,
                    })
                    .await;
            }
        };
        let fanout_receipt = effects.fanout.as_ref().map(|fanout| {
            crate::model::FanoutReceiptSummary {
                statuses: fanout.statuses.clone(),
                failed: fanout
                    .statuses
                    .iter()
                    .filter(|status| **status == FanoutItemStatus::Failed)
                    .count(),
                expected: fanout.statuses.len(),
                results: None,
            }
        });
        let receipt = NodeReceipt {
            node: current.to_string(),
            step,
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            result_hash,
            cache_hit: false,
            elapsed_ms,
            error: Some(error.clone()),
            cost: cost.clone(),
            fanout: fanout_receipt,
        };
        self.write_node_receipt_or_warn(graph_run_id, receipt)
            .await;
        self.emit_graph_step_completed(
            graph_run_id,
            step,
            current,
            GraphStepStatus::Error,
            Some(error),
        )
        .await;
        self.fire_graph_hooks(
            self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
            self.step_hook_context(
                graph_run_id,
                current,
                step,
                GraphStepStatus::Error,
                Some(error),
                state,
            ),
        )
        .await;

        match next_on_error {
            NextOnError::Redirect(target) => {
                self.write_checkpoint_or_error(
                    graph_run_id,
                    target,
                    step + 1,
                    state,
                    suppressed_errors,
                    guard,
                    0,
                    inputs,
                    execution,
                )
                .await
            }
            NextOnError::PolicyContinue => {
                self.extend_suppressed_errors(
                    suppressed_errors,
                    effects.suppressed_errors.into_iter().chain(std::iter::once(
                        ErrorRecord {
                            step,
                            node: current.to_string(),
                            error: error.clone(),
                        },
                    )),
                );
                // Expression `continue` terminates this graph path. It
                // never retries or skips the failed normal edge.
                self.commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step + 1,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Completed,
                    error: None,
                    output: None,
                    guard,
                    current_node_id: current,
                    inputs,
                    execution,
                })
                .await
            }
            NextOnError::PolicyFail => {
                let diagnostic = format!("node '{current}' failed: {error}");
                self.commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Error,
                    error: Some(&diagnostic),
                    output: None,
                    guard,
                    current_node_id: current,
                    inputs,
                    execution,
                })
                .await
            }
        }
    }

    pub(super) async fn commit_leaf_soft_error(
        &self,
        input: CommitStepContext<'_>,
        outcome: LeafSoftErrorOutcome,
    ) -> CommitResult {
        let CommitStepContext {
            graph_run_id,
            step,
            current,
            state,
            suppressed_errors,
            guard,
            inputs,
            execution,
            cache: _,
        } = input;
        let LeafSoftErrorOutcome {
            item_id,
            error,
            next_on_error,
            elapsed_ms,
            cost,
            observation,
        } = outcome;
        let item_id = &item_id;
        let error = &error;
        let next_on_error = &next_on_error;
        let cost = &cost;
        // Soft error: dispatch completed, but the child returned a terminal
        // failure or the stored follow envelope was invalid.
        // graph_step_started → tool_call_start → tool_call_result(error) → graph_step_completed(error) → [redirect/continue/fail]
        self.emit_graph_step_started(graph_run_id, step, current)
            .await;
        self.emit_tool_call_start(graph_run_id, step, current, item_id)
            .await;
        self.emit_tool_call_result(
            graph_run_id,
            step,
            current,
            item_id,
            GraphToolCallStatus::Error,
        )
        .await;

        if let Some(observation) = &observation {
            self.emit_dispatch_observation(current, step, observation)
                .await;
        }

        // A cost-bearing child that then errored (or whose assign
        // failed) still spent tokens — account for it.
        if let Some(c) = cost {
            self.record_node_cost(current, step, item_id, c.clone());
        }

        let receipt = NodeReceipt {
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
        };

        self.write_node_receipt_or_warn(graph_run_id, receipt)
            .await;

        self.emit_graph_step_completed(
            graph_run_id,
            step,
            current,
            GraphStepStatus::Error,
            Some(error),
        )
        .await;
        self.fire_graph_hooks(
            self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
            self.step_hook_context(
                graph_run_id,
                current,
                step,
                GraphStepStatus::Error,
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
                    inputs,
                    execution,
                )
                .await
            }
            NextOnError::PolicyContinue => {
                self.push_suppressed_error(
                    suppressed_errors,
                    ErrorRecord {
                        step,
                        node: current.to_string(),
                        error: error.clone(),
                    },
                );
                match edges::evaluate_next(
                    self.graph.compiled.node(current),
                    state,
                    inputs,
                    Some(execution),
                    Some(graph_run_id),
                ) {
                    Ok(Some(next_node)) => {
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
                            inputs,
                            execution,
                        )
                        .await
                    }
                    Ok(None) => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: GraphRunStatus::Completed,
                            error: None,
                            output: None,
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                    Err(expression_error) => {
                        self.push_suppressed_error(
                            suppressed_errors,
                            ErrorRecord {
                                step,
                                node: current.to_string(),
                                error: format!(
                                    "expression evaluation failed while continuing after node error: {expression_error}"
                                ),
                            },
                        );
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: GraphRunStatus::Completed,
                            error: None,
                            output: None,
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
                    base_status: GraphRunStatus::Error,
                    error: Some(&format!("node '{}' failed: {}", current, error)),
                    output: None,
                    guard,
                    current_node_id: current,
                    inputs,
                    execution,
                })
                .await
            }
        }
    }

    pub(super) async fn commit_dispatch_hard_error(
        &self,
        input: CommitStepContext<'_>,
        outcome: DispatchHardErrorOutcome,
    ) -> CommitResult {
        let CommitStepContext {
            graph_run_id,
            step,
            current,
            state,
            suppressed_errors,
            guard,
            inputs,
            execution,
            cache: _,
        } = input;
        let DispatchHardErrorOutcome {
            item_id,
            error,
            next_on_error,
            elapsed_ms,
            cost,
        } = outcome;
        let error = &error;
        let next_on_error = &next_on_error;
        let cost = &cost;
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
                GraphToolCallStatus::DispatchFailed,
            )
            .await;
        }

        if let Some(c) = cost {
            self.record_node_cost(current, step, item_str, c.clone());
        }

        let receipt = NodeReceipt {
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
        };

        self.write_node_receipt_or_warn(graph_run_id, receipt)
            .await;

        self.emit_graph_step_completed(
            graph_run_id,
            step,
            current,
            GraphStepStatus::Error,
            Some(error),
        )
        .await;
        self.fire_graph_hooks(
            self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
            self.step_hook_context(
                graph_run_id,
                current,
                step,
                GraphStepStatus::Error,
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
                    inputs,
                    execution,
                )
                .await
            }
            NextOnError::PolicyContinue => {
                self.push_suppressed_error(
                    suppressed_errors,
                    ErrorRecord {
                        step,
                        node: current.to_string(),
                        error: error.clone(),
                    },
                );
                match edges::evaluate_next(
                    self.graph.compiled.node(current),
                    state,
                    inputs,
                    Some(execution),
                    Some(graph_run_id),
                ) {
                    Ok(Some(next_node)) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            &next_node,
                            next_step,
                            state,
                            suppressed_errors,
                            guard,
                            0,
                            inputs,
                            execution,
                        )
                        .await
                    }
                    Ok(None) => {
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: GraphRunStatus::Completed,
                            error: None,
                            output: None,
                            guard,
                            current_node_id: current,
                            inputs,
                            execution,
                        })
                        .await
                    }
                    Err(expression_error) => {
                        self.push_suppressed_error(
                            suppressed_errors,
                            ErrorRecord {
                                step,
                                node: current.to_string(),
                                error: format!(
                                    "expression evaluation failed while continuing after node error: {expression_error}"
                                ),
                            },
                        );
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id,
                            steps: step + 1,
                            state,
                            suppressed_errors,
                            base_status: GraphRunStatus::Completed,
                            error: None,
                            output: None,
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
                    base_status: GraphRunStatus::Error,
                    error: Some(&format!("node '{}' failed: {}", current, error)),
                    output: None,
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
