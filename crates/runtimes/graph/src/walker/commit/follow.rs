use super::*;
use crate::walker::continued_terminal_completion;

impl Walker {
    pub(super) async fn commit_follow_suspend(
        &self,
        input: CommitStepContext<'_>,
        outcome: FollowSuspendOutcome,
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
        let FollowSuspendOutcome {
            item_id,
            ref_bindings,
            params,
        } = outcome;
        let item_id = item_id.as_str();
        let params = &params;
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
            .write_follow_checkpoint(graph_run_id, current, step, state, suppressed_errors, None)
            .await
        {
            let msg = format!("follow checkpoint write failed: {e}");
            return self
                .commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Error,
                    error: Some(&msg),
                    output: None,
                    guard,
                    inputs,
                })
                .await;
        }

        let (agg_cost, node_costs, hook_costs) = {
            let acc = self.accounting.lock().unwrap();
            (acc.total.clone(), acc.nodes.clone(), acc.hooks.clone())
        };
        let continued_result = GraphResult {
            success: false,
            graph_id: self.graph.graph_id.clone(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            graph_run_id: graph_run_id.to_string(),
            status: GraphRunStatus::Continued,
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
            hook_costs,
        };
        let completion = continued_terminal_completion(
            &continued_result,
            self.warnings.lock().unwrap().snapshot(),
        );

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
                ref_bindings,
                params.clone(),
                None,
                completion,
            )
            .await
        {
            Ok(_) => {
                // The daemon settled this thread `continued` inside the
                // handoff (it created the follow-resume successor), so do NOT
                // finalize. The pending-follow checkpoint is the resume point.
                guard.finalized = true;
                CommitResult::Terminate(Box::new(continued_result))
            }
            Err(e) => {
                let msg = format!("follow handoff failed: {e}");
                self.commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Error,
                    error: Some(&msg),
                    output: None,
                    guard,
                    inputs,
                })
                .await
            }
        }
    }

    pub(super) async fn commit_follow_fanout_suspend(
        &self,
        input: CommitStepContext<'_>,
        outcome: Box<FollowFanoutSuspendOutcome>,
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
        let FollowFanoutSuspendOutcome {
            children,
            width,
            iteration_snapshot,
        } = *outcome;
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
                    base_status: GraphRunStatus::Error,
                    error: Some(&msg),
                    output: None,
                    guard,
                    inputs,
                })
                .await;
        }
        let (agg_cost, node_costs, hook_costs) = {
            let acc = self.accounting.lock().unwrap();
            (acc.total.clone(), acc.nodes.clone(), acc.hooks.clone())
        };
        let continued_result = GraphResult {
            success: false,
            graph_id: self.graph.graph_id.clone(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            graph_run_id: graph_run_id.to_string(),
            status: GraphRunStatus::Continued,
            steps: step,
            state: state.clone(),
            result: None,
            errors_suppressed: (!suppressed_errors.is_empty()).then_some(suppressed_errors.len()),
            errors: (!suppressed_errors.is_empty()).then_some(suppressed_errors.clone()),
            error: None,
            cost: agg_cost,
            node_costs,
            hook_costs,
        };
        let completion = continued_terminal_completion(
            &continued_result,
            self.warnings.lock().unwrap().snapshot(),
        );
        match self
            .client
            .spawn_follow_children(
                graph_run_id,
                current,
                step as i64,
                children,
                width,
                None,
                completion,
            )
            .await
        {
            Ok(_) => {
                guard.finalized = true;
                CommitResult::Terminate(Box::new(continued_result))
            }
            Err(e) => {
                let msg = format!("follow cohort handoff failed: {e}");
                self.commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Error,
                    error: Some(&msg),
                    output: None,
                    guard,
                    inputs,
                })
                .await
            }
        }
    }

    pub(super) async fn commit_follow_fanout_done(
        &self,
        input: CommitStepContext<'_>,
        outcome: Box<FollowFanoutDoneOutcome>,
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
        let FollowFanoutDoneOutcome {
            results,
            statuses,
            errors,
            collect_key,
            item_id,
            next,
            next_on_error,
            cost,
            elapsed_ms,
        } = *outcome;
        self.emit_graph_step_started(graph_run_id, step, current)
            .await;
        if let Some(c) = &cost {
            // Match classic follow resume: the detached children's
            // aggregate cost is attributed once to the parent node.
            self.record_node_cost(current, step, &item_id, c.clone());
        }
        let diagnostic = (!errors.is_empty()).then(|| {
            errors
                .iter()
                .map(|e| e.error.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        });

        // Execution already evaluated the normal branch against the collected
        // candidate. A fail/redirect discards it; per-item continue commits the
        // collection and keeps the preselected edge.
        let (target, commit_candidate, fail_graph) = if errors.is_empty() {
            (next, true, false)
        } else {
            match next_on_error {
                NextOnError::Redirect(target) => (Some(target), false, false),
                NextOnError::PolicyContinue => {
                    self.extend_suppressed_errors(suppressed_errors, errors.iter().cloned());
                    (next, true, false)
                }
                NextOnError::PolicyFail => (None, false, true),
            }
        };
        if commit_candidate {
            if let Some(key) = collect_key {
                if !state.is_object() {
                    *state = Value::Object(serde_json::Map::new());
                }
                state
                    .as_object_mut()
                    .unwrap()
                    .insert(key, Value::Array(results.clone()));
            }
            // The fanout variable is lexical, not a temporary state key.
        }
        let result_hash = match hash_json_value(&json!({
            "results": &results,
            "statuses": &statuses,
        })) {
            Ok(hash) => hash,
            Err(error) => {
                let message = format!("failed to canonicalize follow result: {error}");
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
                        inputs,
                    })
                    .await;
            }
        };
        let receipt = NodeReceipt {
            node: current.to_string(),
            step,
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            result_hash: Some(result_hash),
            cache_hit: false,
            elapsed_ms,
            error: diagnostic.clone(),
            cost: cost.clone(),
            fanout: Some(crate::model::FanoutReceiptSummary {
                statuses: statuses.clone(),
                failed: statuses
                    .iter()
                    .filter(|status| **status == FanoutItemStatus::Failed)
                    .count(),
                expected: statuses.len(),
                // Results remain represented by result_hash; receipts do not have
                // an explicit local-content policy permitting raw result persistence.
                results: None,
            }),
        };
        self.write_node_receipt_or_warn(graph_run_id, receipt).await;
        let status = if errors.is_empty() {
            GraphStepStatus::Ok
        } else {
            GraphStepStatus::Error
        };
        self.emit_graph_step_completed(graph_run_id, step, current, status, diagnostic.as_deref())
            .await;
        self.fire_graph_hooks(
            self.graph_step_completed_hook_occurrence(graph_run_id, step, current),
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

        if fail_graph {
            return self
                .commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: step,
                    state,
                    suppressed_errors,
                    base_status: GraphRunStatus::Error,
                    error: diagnostic.as_deref(),
                    output: None,
                    guard,
                    inputs,
                })
                .await;
        }
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
                    inputs,
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
                    inputs,
                })
                .await
            }
        }
    }
}
