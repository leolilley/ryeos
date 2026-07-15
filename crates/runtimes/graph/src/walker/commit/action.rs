use super::*;

impl Walker {
    pub(super) async fn commit_action_ok(
        &self,
        input: CommitStepContext<'_>,
        outcome: Box<ActionOkOutcome>,
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
            cache,
        } = input;
        let ActionOkOutcome {
            item_id,
            result,
            assign,
            next,
            child_thread_id,
            cache_hit,
            cache_write_key,
            elapsed_ms,
            cost,
        } = outcome.as_ref();
        let cache_hit = *cache_hit;
        let elapsed_ms = *elapsed_ms;
        // R3 fence order:
        // graph_step_started → tool_call_start → (dispatch in run_node_body) →
        // tool_call_result → state mutation → receipt → graph_step_completed → checkpoint
        self.emit_graph_step_started(graph_run_id, step, current)
            .await;
        self.emit_tool_call_start(graph_run_id, step, current, item_id)
            .await;
        self.emit_tool_call_result(
            graph_run_id,
            step,
            current,
            item_id,
            GraphToolCallStatus::Ok,
        )
        .await;

        // These observations describe a dispatch that already
        // happened, but are deliberately deferred until assignment and
        // branch evaluation have both succeeded. They therefore cannot
        // make an expression-failed transition look committed.
        if let Some(observation) = DispatchObservation::from_success(
            item_id.to_string(),
            child_thread_id.clone(),
            result,
        ) {
            self.emit_dispatch_observation(current, step, &observation)
                .await;
        }

        // State mutation: `assign` was already evaluated in
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
            self.record_node_cost(current, step, item_id, c.clone());
        }

        // Receipt
        let receipt = NodeReceipt {
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
        };
        self.write_node_receipt_or_warn(graph_run_id, receipt)
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

        let committed = match next {
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
        };

        // A cache entry is replay authority, so it may only become visible
        // after the advancing checkpoint has become authoritative. Publishing
        // before this fence lets a crash
        // replay an older checkpoint through a cost-free cache hit.
        // Terminal callback settlement currently reports failures through the
        // warning channel, so a successful-looking GraphResult is not proof of
        // durable authority. Do not publish terminal-node cache entries until
        // that boundary returns an explicit settlement acknowledgement.
        let authoritative = matches!(&committed, CommitResult::Advance { .. });
        if authoritative {
            if let Some(cache_key) = cache_write_key {
                cache.store(cache_key, result);
            }
        }
        committed
    }
}
