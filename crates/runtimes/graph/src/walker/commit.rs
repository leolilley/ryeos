use serde_json::{json, Value};

use crate::edges;
use crate::knowledge;
use crate::model::*;
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::{TerminalCompletion, ThreadTerminalStatus};

use super::events::node_ref;
use super::outcome::*;
use super::transitions::foreach_failure_summary;
use super::{hash_json_value, merge_into, Walker};

mod action;
mod errors;
mod flow;
mod follow;
mod foreach_done;
mod foreach_failed;
mod retry;

impl Walker {
    /// D13: The ONLY function allowed to commit a completed step outcome:
    ///   - emit transition-commit lifecycle events
    ///   - write a node receipt
    ///   - write a checkpoint
    ///   - emit `GraphCompleted` on terminal
    ///   - finalize the thread on terminal
    ///
    /// Execution may publish live foreach/retry progress observations, but it
    /// must not perform any of the state, receipt, checkpoint, or transition
    /// commit effects above before it has produced a complete `StepOutcome`.
    ///
    /// `commit_step` MUST be called exactly once per loop iteration.
    pub(super) async fn commit_step(&self, input: CommitStepInput<'_>) -> CommitResult {
        let CommitStepInput {
            graph_run_id,
            step,
            current,
            state,
            suppressed_errors,
            outcome,
            guard,
            inputs,
            execution,
            cache,
        } = input;
        let context = CommitStepContext {
            graph_run_id,
            step,
            current,
            state,
            suppressed_errors,
            guard,
            inputs,
            execution,
            cache,
        };

        match outcome {
            StepOutcome::FollowSuspend(outcome) => {
                self.commit_follow_suspend(context, outcome).await
            }
            StepOutcome::FollowFanoutSuspend(outcome) => {
                self.commit_follow_fanout_suspend(context, outcome).await
            }
            StepOutcome::RetryScheduled(outcome) => {
                self.commit_retry_scheduled(context, outcome).await
            }
            StepOutcome::Terminal(outcome) => {
                self.commit_terminal_outcome(context, outcome).await
            }
            StepOutcome::GateTaken(outcome) => self.commit_gate_taken(context, outcome).await,
            StepOutcome::ForeachDone(outcome) => {
                self.commit_foreach_done(context, outcome).await
            }
            StepOutcome::ForeachFailed(outcome) => {
                self.commit_foreach_failed(context, outcome).await
            }
            StepOutcome::FollowFanoutDone(outcome) => {
                self.commit_follow_fanout_done(context, outcome).await
            }
            StepOutcome::ActionOk(outcome) => self.commit_action_ok(context, outcome).await,
            StepOutcome::ExpressionFailed(outcome) => {
                self.commit_expression_failed(context, outcome).await
            }
            StepOutcome::IntegrityFailed(outcome) => {
                self.commit_integrity_failed(context, outcome).await
            }
            StepOutcome::LeafSoftError(outcome) => {
                self.commit_leaf_soft_error(context, outcome).await
            }
            StepOutcome::DispatchHardError(outcome) => {
                self.commit_dispatch_hard_error(context, outcome).await
            }
        }
    }

    async fn emit_dispatch_observation(
        &self,
        node: &str,
        step: u32,
        observation: &DispatchObservation,
    ) {
        if let Some(child_thread_id) = &observation.child_thread_id {
            let result = self
                .client
                .append_runtime_event(
                    RuntimeEventType::ChildThreadSpawned,
                    json!({
                        "child_thread_id": child_thread_id,
                        "node": node,
                        "step": step,
                        "item_id": &observation.item_id,
                        "spawn_reason": "dispatch",
                    }),
                )
                .await;
            self.record_callback_warning("child_thread_spawned", result);
        }
        for entry in &observation.milestones {
            let Some(kind) = entry.get("kind").and_then(Value::as_str) else {
                continue;
            };
            let result = self
                .client
                .append_runtime_event(
                    RuntimeEventType::Milestone,
                    json!({
                        "kind": kind,
                        "payload": entry.get("payload").cloned().unwrap_or(Value::Null),
                        "node": node,
                        "step": step,
                    }),
                )
                .await;
            self.record_callback_warning("milestone", result);
        }
    }

    async fn emit_foreach_iteration_statuses(
        &self,
        graph_run_id: &str,
        node: &str,
        step: u32,
        statuses: &[GraphToolCallStatus],
        total_items: usize,
    ) {
        for (iteration, status) in statuses.iter().enumerate() {
            let result = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphForeachIteration,
                    json!({
                        "graph_run_id": graph_run_id,
                        "definition_ref": &self.graph.definition_ref,
                        "definition_hash": &self.graph.definition_hash,
                        "node": node,
                        "node_ref": node_ref(&self.graph.definition_ref, node),
                        "step": step,
                        "iteration": iteration,
                        "total": total_items,
                        "status": status.as_str(),
                    }),
                )
                .await;
            self.record_callback_warning("graph_foreach_iteration", result);
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
            output,
            guard,
            current_node_id: _,
            inputs,
            execution: _,
        } = input;
        let resolve_status = |history_failed: bool| {
            let effective_status = if history_failed {
                GraphRunStatus::Error
            } else {
                base_status
            };
            match effective_status {
                GraphRunStatus::Completed => {
                    let status = if suppressed_errors.is_empty() {
                        GraphRunStatus::Completed
                    } else {
                        GraphRunStatus::CompletedWithErrors
                    };
                    (true, status)
                }
                GraphRunStatus::CompletedWithErrors => {
                    (true, GraphRunStatus::CompletedWithErrors)
                }
                GraphRunStatus::MaxStepsExceeded => (false, GraphRunStatus::MaxStepsExceeded),
                GraphRunStatus::Cancelled => (false, GraphRunStatus::Cancelled),
                GraphRunStatus::Killed => (false, GraphRunStatus::Killed),
                GraphRunStatus::Valid
                | GraphRunStatus::Invalid
                | GraphRunStatus::Continued
                | GraphRunStatus::Error => (false, GraphRunStatus::Error),
            }
        };

        // Fire terminal observers before the accounting snapshot and durable
        // finalization. Their child cost is part of this run; a cost-history
        // rejection must still be able to fail the terminal cleanly.
        let initial_history_failure = self.run_history_failure();
        let (observed_success, observed_status) =
            resolve_status(initial_history_failure.is_some());
        if initial_history_failure.is_none() {
            self.fire_graph_hooks(
                self.graph_completed_hook_occurrence(graph_run_id, steps),
                json!({
                    "event": RuntimeEventType::GraphCompleted.as_str(),
                    "graph_id": &self.graph.graph_id,
                    "graph_run_id": graph_run_id,
                    "status": observed_status.as_str(),
                    "settled": false,
                    "steps": steps,
                    "success": observed_success,
                    "state": &state,
                    "inputs": inputs,
                }),
            )
            .await;
        }

        // A history rejection means accounting/error/receipt provenance is no
        // longer complete. Even when the authored node would otherwise finish
        // successfully without another checkpoint, settle the run as failed.
        let history_failure = self.run_history_failure();
        let history_failed = history_failure.is_some();
        let (success, status) = resolve_status(history_failed);

        // Return-node output is evaluated before the terminal StepOutcome is
        // committed, so an expression error cannot publish a successful step
        // or graph terminal. Other terminal paths deliberately carry no output.
        let error_owned = history_failure.or_else(|| error.map(String::from));

        // Snapshot accounting accumulated across the run. Even a failed
        // graph reports the cost spent before it failed — the accumulator
        // holds every cost-bearing node that committed prior to terminal.
        let (agg_cost, node_costs, hook_costs) = {
            let acc = self.accounting.lock().unwrap();
            (acc.total.clone(), acc.nodes.clone(), acc.hooks.clone())
        };

        let graph_result = GraphResult {
            success,
            graph_id: self.graph.graph_id.clone(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            graph_run_id: graph_run_id.to_string(),
            status,
            steps,
            state: state.clone(),
            result: if history_failed { None } else { output },
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
            hook_costs,
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
                        "status": status.as_str(),
                        "steps": steps,
                    }),
                )
                .await;
            self.record_callback_warning(RuntimeEventType::GraphCompleted.as_str(), r);
        }

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
        let thread_status = match status {
            GraphRunStatus::Completed | GraphRunStatus::CompletedWithErrors => {
                ThreadTerminalStatus::Completed
            }
            GraphRunStatus::Cancelled => ThreadTerminalStatus::Cancelled,
            GraphRunStatus::Killed => ThreadTerminalStatus::Killed,
            GraphRunStatus::Valid
            | GraphRunStatus::Invalid
            | GraphRunStatus::Continued
            | GraphRunStatus::Error
            | GraphRunStatus::MaxStepsExceeded => ThreadTerminalStatus::Failed,
        };
        let completion = TerminalCompletion {
            status: thread_status,
            outcome_code: Some(
                match thread_status {
                    ThreadTerminalStatus::Completed => "success",
                    other => other.as_str(),
                }
                .to_string(),
            ),
            // Callback settlement and stdout carry the identical typed graph
            // result. The executor can therefore reject any post-finalization
            // payload contradiction without a graph-specific normalizer.
            result: Some(
                serde_json::to_value(&graph_result)
                    .expect("GraphResult is an infallibly serializable runtime DTO"),
            ),
            error: graph_result.error.as_ref().map(|e| json!(e)),
            cost: graph_result.cost.as_ref().map(|cost| {
                serde_json::to_value(cost)
                    .expect("validated graph cost must serialize for terminal settlement")
            }),
            // A graph's return value is its `result`; it has no separate structured
            // outputs. Send a snapshot of accumulated callback-drift warnings so a
            // follow parent (which consumes THIS envelope, not the later stdout
            // RuntimeResult) sees the same warnings a live dispatch would.
            outputs: Value::Null,
            warnings: self.warnings.lock().unwrap().snapshot(),
        };
        let r = self.client.finalize_thread(completion).await;
        match r {
            Ok(_) => guard.finalized = true,
            Err(error) => self.record_callback_warning(
                "finalize_thread",
                Err(anyhow::anyhow!(error)),
            ),
        }

        CommitResult::Terminate(Box::new(graph_result))
    }
}
