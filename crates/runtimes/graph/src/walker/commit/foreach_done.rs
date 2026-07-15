use super::*;

impl Walker {
    pub(super) async fn commit_foreach_done(
        &self,
        input: CommitStepContext<'_>,
        outcome: Box<ForeachDoneOutcome>,
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
        } = input;
        let ForeachDoneOutcome {
            results,
            statuses,
            total_items,
            collect_key,
            assign_delta,
            errors,
            next,
            item_id,
            cost,
            observations,
        } = outcome.as_ref();
        // Foreach lifecycle: graph_step_started → (per-iteration
        // graph_foreach_iteration events) → graph_step_completed →
        // checkpoint
        self.emit_graph_step_started(graph_run_id, step, current)
            .await;

        // Foreach aggregates all iteration child costs into one
        // record for the node (per-iteration accounting can be
        // added later if needed).
        if let Some(c) = cost {
            self.record_node_cost(current, step, item_id, c.clone());
        }

        self.emit_foreach_iteration_statuses(
            graph_run_id,
            current,
            step,
            statuses,
            *total_items,
        )
        .await;

        for observation in observations {
            self.emit_dispatch_observation(current, step, observation)
                .await;
        }

        // Merge foreach results into state.
        if let Some(ref key) = collect_key {
            if let Some(obj) = state.as_object_mut() {
                obj.insert(key.clone(), Value::Array(results.clone()));
            }
        }
        // Commit accumulated foreach `assign` mutations.
        merge_into(state, assign_delta);
        // `as` is a lexical rye-expr root and was never inserted into graph
        // state. Preserve any unrelated persistent key with the same name.

        // Surface per-item failures (continue policy) as suppressed
        // errors so the run terminates `completed_with_errors`.
        let diagnostic = (!errors.is_empty()).then(|| foreach_failure_summary(current, errors));
        let status = if diagnostic.is_some() {
            GraphStepStatus::Error
        } else {
            GraphStepStatus::Ok
        };
        self.extend_suppressed_errors(suppressed_errors, errors.iter().cloned());

        self.emit_graph_step_completed(
            graph_run_id,
            step,
            current,
            status,
            diagnostic.as_deref(),
        )
        .await;
        self.fire_graph_hooks(
            RuntimeEventType::GraphStepCompleted,
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
                    inputs,
                    execution,
                )
                .await
            }
            None => {
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
}
