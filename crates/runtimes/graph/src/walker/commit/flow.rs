use super::*;

impl Walker {
    pub(super) async fn commit_terminal_outcome(
        &self,
        input: CommitStepContext<'_>,
        outcome: TerminalOutcome,
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
        let TerminalOutcome {
            status,
            error,
            origin,
            output,
        } = outcome;
        let terminal_steps = match origin {
            TerminalOrigin::Node => {
                let step_status = if status == GraphRunStatus::Completed {
                    GraphStepStatus::Ok
                } else {
                    GraphStepStatus::Error
                };
                self.emit_graph_step_started(graph_run_id, step, current)
                    .await;
                self.emit_graph_step_completed(
                    graph_run_id,
                    step,
                    current,
                    step_status,
                    error.as_deref(),
                )
                .await;
                self.fire_graph_hooks(
                    self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
                    self.step_hook_context(
                        graph_run_id,
                        current,
                        step,
                        step_status,
                        error.as_deref(),
                        state,
                    ),
                )
                .await;
                step + 1
            }
            TerminalOrigin::RunControl => step,
        };
        self.commit_terminal(CommitTerminalInput {
            graph_run_id,
            steps: terminal_steps,
            state,
            suppressed_errors,
            base_status: status,
            error: error.as_deref(),
            output,
            guard,
            current_node_id: current,
            inputs,
            execution,
        })
        .await
    }

    pub(super) async fn commit_gate_taken(
        &self,
        input: CommitStepContext<'_>,
        outcome: GateTakenOutcome,
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
        let GateTakenOutcome { target } = outcome;
        // Gate lifecycle: graph_step_started → graph_branch_taken → graph_step_completed → checkpoint
        self.emit_graph_step_started(graph_run_id, step, current)
            .await;
        self.emit_graph_branch_taken(graph_run_id, step, current, target.as_deref())
            .await;
        self.emit_graph_step_completed(
            graph_run_id,
            step,
            current,
            GraphStepStatus::Ok,
            None,
        )
        .await;
        self.fire_graph_hooks(
            self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
            self.step_hook_context(
                graph_run_id,
                current,
                step,
                GraphStepStatus::Ok,
                None,
                state,
            ),
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
