use super::*;

impl Walker {
    pub(super) async fn commit_foreach_failed(
        &self,
        input: CommitStepContext<'_>,
        outcome: Box<ForeachFailedOutcome>,
    ) -> CommitResult {
        let CommitStepContext {
            graph_run_id,
            step,
            current,
            state,
            suppressed_errors,
            guard,
            inputs,
            execution: _,
            cache: _,
        } = input;
        let ForeachFailedOutcome {
            statuses,
            total_items,
            errors,
            item_id,
            next_on_error,
            elapsed_ms,
            cost,
            observations,
        } = outcome.as_ref();
        let diagnostic = foreach_failure_summary(current, errors);

        self.emit_graph_step_started(graph_run_id, step, current)
            .await;
        self.emit_foreach_iteration_statuses(graph_run_id, current, step, statuses, *total_items)
            .await;
        for observation in observations {
            self.emit_dispatch_observation(current, step, observation)
                .await;
        }

        if let Some(cost) = cost {
            self.record_node_cost(current, step, item_id, cost.clone());
        }
        let receipt = NodeReceipt {
            node: current.to_string(),
            step,
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            result_hash: None,
            cache_hit: false,
            elapsed_ms: *elapsed_ms,
            error: Some(diagnostic.clone()),
            cost: cost.clone(),
            fanout: None,
        };
        self.write_node_receipt_or_warn(graph_run_id, receipt).await;
        self.emit_graph_step_completed(
            graph_run_id,
            step,
            current,
            GraphStepStatus::Error,
            Some(&diagnostic),
        )
        .await;
        self.fire_graph_hooks(
            self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
            self.step_hook_context(
                graph_run_id,
                current,
                step,
                GraphStepStatus::Error,
                Some(&diagnostic),
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
                )
                .await
            }
            NextOnError::PolicyFail => {
                let terminal_error = format!("node '{current}' failed: {diagnostic}");
                self.commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Error,
                    error: Some(&terminal_error),
                    output: None,
                    guard,
                    inputs,
                })
                .await
            }
            NextOnError::PolicyContinue => {
                // Construction routes continue-policy foreach outcomes through
                // ForeachDone so successful candidate deltas can commit. Keep
                // this arm total and fail-safe if that invariant changes.
                self.extend_suppressed_errors(suppressed_errors, errors.iter().cloned());
                self.commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step + 1,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Completed,
                    error: None,
                    output: None,
                    guard,
                    inputs,
                })
                .await
            }
        }
    }
}
