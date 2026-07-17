use super::*;

impl Walker {
    pub(super) async fn commit_retry_scheduled(
        &self,
        input: CommitStepContext<'_>,
        outcome: RetryScheduledOutcome,
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
        let RetryScheduledOutcome {
            item_id,
            error,
            failed_attempt,
            total_attempts,
            delay_ms,
            elapsed_ms,
            cost,
        } = outcome;
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
        self.emit_tool_call_result(
            graph_run_id,
            step,
            current,
            &item_id,
            GraphToolCallStatus::Error,
        )
        .await;

        // A native child may have spent tokens before failing this
        // attempt — account for it, exactly like a soft error.
        if let Some(c) = &cost {
            self.record_node_cost(current, step, &item_id, c.clone());
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
        self.write_node_receipt_or_warn(graph_run_id, receipt).await;

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
        self.emit_graph_step_completed(
            graph_run_id,
            step,
            current,
            GraphStepStatus::Retry,
            Some(&error),
        )
        .await;
        self.fire_graph_hooks(
            self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
            self.step_hook_context(
                graph_run_id,
                current,
                step,
                GraphStepStatus::Retry,
                Some(&error),
                state,
            ),
        )
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
                inputs,
            )
            .await;
        if let CommitResult::Advance { .. } = &advance {
            if delay_ms > 0 {
                self.sleep_retry_backoff(delay_ms).await;
            }
        }
        advance
    }
}
