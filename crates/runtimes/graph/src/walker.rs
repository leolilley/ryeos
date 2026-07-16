use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::cache::NodeCache;
use crate::context;
use crate::edges;
use crate::evaluation::{validate_runtime_value, ExpressionScope};
use crate::foreach;
use crate::model::*;
use crate::validation::analyze_graph;
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::checkpoint::CheckpointWriter;
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::{TerminalCompletion, ThreadTerminalStatus};

#[cfg(test)]
mod checkpoint_authority_tests;
mod checkpointing;
mod commit;
mod events;
mod execution;
mod history;
mod outcome;
mod transitions;

use events::node_ref;

fn continued_terminal_completion(
    result: &GraphResult,
    warnings: Vec<String>,
) -> TerminalCompletion {
    TerminalCompletion {
        status: ThreadTerminalStatus::Continued,
        outcome_code: Some(ThreadTerminalStatus::Continued.as_str().to_string()),
        result: Some(
            serde_json::to_value(result)
                .expect("typed GraphResult must serialize for continuation settlement"),
        ),
        error: None,
        cost: result.cost.as_ref().map(|cost| {
            serde_json::to_value(cost)
                .expect("typed graph cost must serialize for continuation settlement")
        }),
        outputs: Value::Null,
        warnings,
    }
}
pub(crate) use history::validate_checkpoint_snapshots;
use history::WarningBuffer;
#[cfg(test)]
use history::{GRAPH_WARNINGS_TRUNCATED, MAX_GRAPH_WARNING_SCALAR_BYTES};
use outcome::*;
use transitions::resolve_next_on_error;

/// Marker for the one exact current graph checkpoint contract. Reader and
/// writer evolve together under this marker; missing fields, unknown fields,
/// and structural drift are rejected rather than migrated. The contract pins
/// the signed graph definition and expression language, and no alternate or
/// legacy checkpoint versions are accepted.
pub(crate) const GRAPH_CHECKPOINT_SCHEMA_VERSION: u32 = 1;
pub(crate) const EXPRESSION_LANGUAGE: &str = "rye-expr/1";

/// Follow-resume field keys for the checkpoint / resume-state payload. Shared by
/// the write (walker), read (resume), and inject (main) sites so the vocabulary
/// lives in one place instead of scattered string literals.
pub(crate) mod follow_keys {
    /// Marker object recorded at a follow suspend (local facts, no child IDs).
    pub const PENDING_FOLLOW: &str = "pending_follow";
    /// The child's canonical terminal envelope, spliced in for resume. Shared with
    /// the daemon splicer — one definition, in the checkpoint crate both depend on.
    pub const FOLLOW_RESULT: &str = ryeos_runtime::checkpoint::FOLLOW_RESULT_KEY;
    /// Nested inside `PENDING_FOLLOW`: the follow node to resume into.
    pub const FOLLOW_NODE: &str = "follow_node";
}

/// Free-form breadcrumb passed to `request_continuation` when a segment budget
/// is exhausted. For logs only — the substrate keys off the thread lineage, not
/// this string.
const SEGMENT_CONTINUATION_REASON: &str = "graph segment step budget exhausted";

// ── F3 advanced path: StepOutcome + commit_step ─────────────────
//
// D13: `commit_step` is the single mutation point for graph state
// lifecycle. Every walker branch produces exactly one `StepOutcome`
// and hands it to `commit_step`. The walker's main loop never
// appends a transition-commit step event, writes a receipt, or writes a
// checkpoint outside `commit_step`. Execution may publish explicitly live
// progress such as `GraphForeachStarted` before the node settles.

/// A cooperative control action drained from the thread's command queue between
/// nodes. Ordered by severity so `Kill` supersedes `Cancel` when both queue in a
/// single drained batch (`Kill > Cancel`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ControlAction {
    Cancel,
    Kill,
}

impl ControlAction {
    /// The cooperative-termination action for a command's `command_type`, or
    /// `None` for a type the walker does not action between nodes.
    fn from_command_type(command_type: &str) -> Option<Self> {
        match command_type {
            "cancel" => Some(Self::Cancel),
            "kill" => Some(Self::Kill),
            _ => None,
        }
    }

    /// The command_type this action was raised from (for the ack payload).
    fn command_type(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Kill => "kill",
        }
    }

    /// The terminal status the run settles as.
    fn terminal_status(self) -> GraphRunStatus {
        match self {
            Self::Cancel => GraphRunStatus::Cancelled,
            Self::Kill => GraphRunStatus::Killed,
        }
    }
}

/// Closed command-settlement domain used by walker control flow. Conversion to
/// the callback protocol's string spelling happens only at the RPC boundary.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandSettlementStatus {
    Completed,
    Rejected,
}

impl CommandSettlementStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Rejected => "rejected",
        }
    }
}

/// The cooperative control decision for one between-nodes drain: which action
/// won the batch and the reason recorded on the terminal.
struct ControlDirective {
    action: ControlAction,
    reason: Option<String>,
}

pub struct Walker {
    graph: GraphDefinition,
    project_path: String,
    thread_id: String,
    client: CallbackClient,
    checkpoint: Option<CheckpointWriter>,
    /// Deterministic unit-test seam for persistence-authority coverage. `None`
    /// in normal tests; absent from production builds entirely.
    #[cfg(test)]
    checkpoint_writes_before_failure: Mutex<Option<usize>>,
    /// Deterministic unit-test crash seam immediately after an atomic
    /// checkpoint replacement succeeds. This models the process disappearing
    /// before `commit_step` can expose a successor or perform a follow handoff.
    #[cfg(test)]
    checkpoint_writes_before_crash: Mutex<Option<usize>>,
    /// Accumulated non-fatal callback drift surfaced during a single
    /// `execute` run. Every failed callback (event-store rejection,
    /// transient transport failure) is recorded here instead of being
    /// dropped. Drained by `take_warnings()` after `execute` returns so
    /// the daemon-side launcher can attach them to
    /// `RuntimeResult.warnings`.
    ///
    /// `Mutex` interior mutability lets the emitter (`record_callback_warning`)
    /// run with `&self`, which keeps `execute` non-mutable and
    /// avoids fighting the long-lived `&self.graph.config` borrow
    /// taken at the top of the run loop. The lock is held for a
    /// single push and never across an `await`.
    warnings: Mutex<WarningBuffer>,
    /// Per-run token/spend accounting, accumulated as cost-bearing nodes
    /// commit. Interior-mutable for the same reason as `warnings`: it is
    /// updated with `&self` from `commit_step` and read once at terminal
    /// finalization. The lock is held for a single record/read, never
    /// across an `await`.
    accounting: Mutex<GraphAccounting>,
    /// Incremental aggregate limits for every run-owned history. The first
    /// rejection is sticky and is surfaced by the next checkpoint or terminal
    /// settlement; rejected entries are never retained.
    run_history: Mutex<RunHistoryBudgets>,
    /// Armed when resuming INTO a follow node with a spliced child envelope: the
    /// walker consumes it at that node (classifies it like a live dispatch) instead
    /// of re-suspending. Taken once, at the follow node. Interior-mutable for the
    /// same `&self` reason as `accounting`.
    follow_resume: Mutex<Option<FollowResumeState>>,
    /// Signal-driven cooperative cancel. Set by the graph process's `SIGTERM`
    /// handler (mirroring the directive runtime's `cancelled_flag`); the run loop
    /// checks it at each node boundary and finalizes `cancelled` cleanly, so a
    /// daemon graceful-cancel signal stops a graph at a checkpoint boundary
    /// instead of the process dying mid-node. `None` in tests / when unset.
    cancel_flag: Option<Arc<AtomicBool>>,
}

impl Walker {
    pub fn new(
        graph: GraphDefinition,
        project_path: String,
        thread_id: String,
        client: CallbackClient,
        checkpoint: Option<CheckpointWriter>,
    ) -> Self {
        Self {
            graph,
            project_path,
            thread_id,
            client,
            checkpoint,
            #[cfg(test)]
            checkpoint_writes_before_failure: Mutex::new(None),
            #[cfg(test)]
            checkpoint_writes_before_crash: Mutex::new(None),
            warnings: Mutex::new(WarningBuffer::default()),
            accounting: Mutex::new(GraphAccounting::default()),
            run_history: Mutex::new(RunHistoryBudgets::default()),
            follow_resume: Mutex::new(None),
            cancel_flag: None,
        }
    }

    /// Arm a signal-driven cooperative cancel flag (set by the process `SIGTERM`
    /// handler). When set, the run loop finalizes `cancelled` at the next node
    /// boundary.
    pub fn with_cancel_flag(mut self, cancel_flag: Arc<AtomicBool>) -> Self {
        self.cancel_flag = Some(cancel_flag);
        self
    }

    /// Wait for an authored retry delay, waking promptly when SIGTERM requests
    /// cooperative cancellation. The already-written retry checkpoint remains
    /// authoritative; returning early lets the main loop settle cancellation at
    /// its normal between-node boundary.
    async fn sleep_retry_backoff(&self, delay_ms: u64) {
        let delay = tokio::time::sleep(std::time::Duration::from_millis(delay_ms));
        tokio::pin!(delay);
        let Some(flag) = self.cancel_flag.clone() else {
            delay.await;
            return;
        };
        let cancelled = async move {
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        };
        tokio::pin!(cancelled);
        tokio::select! {
            _ = &mut delay => {}
            _ = &mut cancelled => {}
        }
    }

    /// If a follow result is armed for `node`, take it (consumed once). The
    /// resumed follow node strictly validates the daemon-managed terminal
    /// envelope instead of re-suspending.
    fn take_follow_state(&self, node: &str) -> Option<FollowResumeState> {
        let mut slot = self.follow_resume.lock().unwrap();
        if slot.as_ref().is_some_and(|fr| fr.follow_node == node) {
            return slot.take();
        }
        None
    }

    /// Drain the accumulated callback-drift warnings. Called by the
    /// graph-runtime binary's `main.rs` after `execute` returns so the
    /// drift can be threaded into `RuntimeResult.warnings`.
    pub fn take_warnings(&self) -> Vec<String> {
        self.warnings.lock().unwrap().take()
    }

    fn seed_run_history(&self, suppressed_errors: &[ErrorRecord]) -> anyhow::Result<()> {
        let budgets = {
            let accounting = self.accounting.lock().unwrap();
            RunHistoryBudgets::seed(&accounting, suppressed_errors)?
        };
        *self.run_history.lock().unwrap() = budgets;
        Ok(())
    }

    fn record_node_cost(
        &self,
        node: &str,
        step: u32,
        item_id: &str,
        cost: ryeos_runtime::envelope::RuntimeCost,
    ) {
        let record = NodeCostRecord {
            node: node.to_string(),
            step,
            item_id: item_id.to_string(),
            cost,
        };
        let mut history = self.run_history.lock().unwrap();
        let mut accounting = self.accounting.lock().unwrap();
        history.record_accounting(&mut accounting, record);
    }

    fn record_hook_cost(
        &self,
        event: RuntimeEventType,
        step: Option<u32>,
        cost: ryeos_runtime::envelope::RuntimeCost,
    ) {
        let record = HookCostRecord { event, step, cost };
        let mut history = self.run_history.lock().unwrap();
        let mut accounting = self.accounting.lock().unwrap();
        history.record_hook_accounting(&mut accounting, record);
    }

    fn reject_run_history(&self, error: impl std::fmt::Display) {
        self.run_history.lock().unwrap().reject_external(error);
    }

    fn accept_node_receipt(&self, receipt: &NodeReceipt) -> bool {
        self.run_history.lock().unwrap().accept_receipt(receipt)
    }

    fn push_suppressed_error(&self, history: &mut Vec<ErrorRecord>, error: ErrorRecord) {
        self.run_history
            .lock()
            .unwrap()
            .push_suppressed(history, error);
    }

    fn extend_suppressed_errors(
        &self,
        history: &mut Vec<ErrorRecord>,
        errors: impl IntoIterator<Item = ErrorRecord>,
    ) {
        self.run_history
            .lock()
            .unwrap()
            .extend_suppressed(history, errors);
    }

    fn run_history_failure(&self) -> Option<String> {
        self.run_history
            .lock()
            .unwrap()
            .failure()
            .map(str::to_string)
    }

    fn ensure_run_history_bounded(&self) -> anyhow::Result<()> {
        if let Some(error) = self.run_history_failure() {
            anyhow::bail!(error);
        }
        Ok(())
    }

    async fn fail_runtime_preflight(
        &self,
        graph_run_id: String,
        diagnostic: String,
        guard: &mut RunGuard,
    ) -> GraphResult {
        let result = GraphResult {
            success: false,
            graph_id: self.graph.graph_id.clone(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            graph_run_id,
            status: GraphRunStatus::Error,
            steps: 0,
            state: json!({}),
            result: None,
            errors_suppressed: None,
            errors: None,
            error: Some(diagnostic.clone()),
            cost: None,
            node_costs: Vec::new(),
            hook_costs: Vec::new(),
        };
        let completion = TerminalCompletion {
            status: ThreadTerminalStatus::Failed,
            outcome_code: Some(ThreadTerminalStatus::Failed.as_str().to_string()),
            result: Some(
                serde_json::to_value(&result)
                    .expect("GraphResult is an infallibly serializable runtime DTO"),
            ),
            error: Some(json!(diagnostic)),
            cost: None,
            outputs: Value::Null,
            warnings: self.warnings.lock().unwrap().snapshot(),
        };
        let finalized = self.client.finalize_thread(completion).await;
        match finalized {
            Ok(_) => guard.finalized = true,
            Err(error) => {
                self.record_callback_warning("finalize_thread", Err(anyhow::anyhow!(error)))
            }
        }
        result
    }

    /// Record a non-fatal callback failure as a warning.
    ///
    /// Mirrors `record_callback_warning` in the directive runner.
    /// Use after every event-emission attempt so event-store rejection
    /// (event_type drift, storage_class drift, transient transport
    /// failures) lands in `RuntimeResult.warnings` instead of being
    /// silently discarded.
    ///
    /// Also used for non-event callbacks (finalize_thread,
    /// mark_running, publish_artifact, write_node_receipt,
    /// write_knowledge_transcript) — any callback whose failure
    /// should be surfaced rather than dropped.
    ///
    /// The lock is taken only for the single `push` and is never held
    /// across an `await`.
    fn record_callback_warning(&self, label: &str, result: anyhow::Result<()>) {
        if let Err(e) = result {
            self.record_warning(format!("callback {label} failed: {e}"));
        }
    }

    fn record_warning(&self, warning: String) {
        self.warnings.lock().unwrap().push(warning);
    }

    #[cfg(test)]
    pub fn validate(&self) -> GraphResult {
        let result = analyze_graph(&self.graph);
        GraphResult {
            success: result.errors.is_empty(),
            graph_id: self.graph.graph_id.clone(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            graph_run_id: String::new(),
            status: if result.errors.is_empty() {
                GraphRunStatus::Valid
            } else {
                GraphRunStatus::Invalid
            },
            steps: 0,
            state: json!({}),
            result: Some(json!({
                "errors": result.errors,
                "warnings": result.warnings,
                "node_count": self.graph.config.nodes.len(),
            })),
            errors_suppressed: None,
            errors: None,
            error: None,
            cost: None,
            node_costs: Vec::new(),
            hook_costs: Vec::new(),
        }
    }

    #[tracing::instrument(
        name = "graph:execute",
        skip(self, params),
        fields(
            graph_id = %self.graph.graph_id,
            thread_id = %self.thread_id,
        )
    )]
    pub async fn execute(&self, params: Value, graph_run_id: Option<String>) -> GraphResult {
        tracing::info!(
            graph_id = %self.graph.graph_id,
            version = %self.graph.version,
            file_path = ?self.graph.file_path,
            "graph loaded"
        );

        // Reset per-run accounting so a Walker reused across multiple `execute`
        // calls does not carry stale cost from a prior run. If this is a resumed
        // run, the checkpoint accounting snapshot is restored below from
        // `resume_state`, so pre-checkpoint cost is preserved.
        *self.warnings.lock().unwrap() = WarningBuffer::default();
        *self.accounting.lock().unwrap() = GraphAccounting::default();
        *self.run_history.lock().unwrap() = RunHistoryBudgets::default();
        // Clear any follow-resume armed by a prior run on a reused Walker, so a
        // stale child result can never be consumed by a later execute.
        *self.follow_resume.lock().unwrap() = None;

        let mut guard = RunGuard { finalized: false };

        let mut graph_run_id = graph_run_id.unwrap_or_else(|| {
            format!(
                "gr-{}",
                &lillux::cas::sha256_hex(
                    format!(
                        "{}{}{}",
                        self.graph.graph_id,
                        lillux::time::timestamp_millis(),
                        rand::random::<u32>()
                    )
                    .as_bytes()
                )[..12]
            )
        });

        let validation = analyze_graph(&self.graph);
        if !validation.errors.is_empty() {
            let result = GraphResult {
                success: false,
                graph_id: self.graph.graph_id.clone(),
                definition_ref: self.graph.definition_ref.clone(),
                definition_hash: self.graph.definition_hash.clone(),
                graph_run_id,
                status: GraphRunStatus::Invalid,
                steps: 0,
                state: json!({}),
                result: None,
                errors_suppressed: None,
                errors: None,
                error: Some(validation.errors.join("; ")),
                cost: None,
                node_costs: Vec::new(),
                hook_costs: Vec::new(),
            };
            let completion = TerminalCompletion {
                status: ThreadTerminalStatus::Failed,
                outcome_code: Some(ThreadTerminalStatus::Failed.as_str().to_string()),
                result: Some(
                    serde_json::to_value(&result)
                        .expect("GraphResult is an infallibly serializable runtime DTO"),
                ),
                error: Some(json!(validation.errors.join("; "))),
                cost: None,
                outputs: Value::Null,
                warnings: self.warnings.lock().unwrap().snapshot(),
            };
            let r = self.client.finalize_thread(completion).await;
            match r {
                Ok(_) => guard.finalized = true,
                Err(error) => {
                    self.record_callback_warning("finalize_thread", Err(anyhow::anyhow!(error)))
                }
            }
            return result;
        }

        // D16: the daemon enforces capabilities at the callback boundary.
        // The walker does NOT self-police. The daemon enforces caps at the
        // callback boundary and carries parent budget/depth out-of-band on the
        // callback token. `exec_ctx` remains a local execution descriptor for
        // walker helpers; it is not injected into action params.

        let exec_ctx = context::execution_context_from_envelope(
            params
                .get("parent_thread_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            params.get("depth").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            // Graph-local hard limits. Child budget inheritance no longer flows
            // through action params; the daemon supplies trusted parent context
            // from the callback token when the callback dispatch reaches a
            // managed child launch.
            params
                .get("hard_limits")
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
        let execution_context = exec_ctx.as_context_value();

        // pid/pgid is registered earlier, in `main.rs` right after the callback
        // client is built — BEFORE any durable callback or this `execute()` —
        // so the daemon can always tell a live graph from a crashed one.
        let r = self.client.mark_running().await;
        self.record_callback_warning("mark_running", r.map(|_| ()));

        let cfg = &self.graph.config;
        let inputs = match params.get("inputs") {
            Some(inputs) => {
                if let Err(error) = validate_runtime_value(inputs, "graph inputs") {
                    return self
                        .fail_runtime_preflight(
                            graph_run_id,
                            format!("graph inputs exceeded rye-expr/1 bounds: {error}"),
                            &mut guard,
                        )
                        .await;
                }
                inputs.clone()
            }
            None => json!({}),
        };

        // Initial state precedence (lowest → highest): authored
        // `config.state` defaults, then caller `inject_state`, then
        // `resume_state` (handled below) for a resumed run.
        let mut state = cfg.state.clone().unwrap_or_else(|| json!({}));

        if let Some(defaults) = params.get("inject_state") {
            if !defaults.is_object() {
                return self
                    .fail_runtime_preflight(
                        graph_run_id,
                        "graph inject_state must be a JSON object".to_string(),
                        &mut guard,
                    )
                    .await;
            }
            if let Err(error) = validate_runtime_value(defaults, "graph inject_state") {
                return self
                    .fail_runtime_preflight(
                        graph_run_id,
                        format!("graph inject_state exceeded rye-expr/1 bounds: {error}"),
                        &mut guard,
                    )
                    .await;
            }
            merge_into(&mut state, defaults);
        }
        if let Err(error) = validate_runtime_value(&state, "initial graph state") {
            return self
                .fail_runtime_preflight(
                    graph_run_id,
                    format!("initial graph state exceeded rye-expr/1 bounds: {error}"),
                    &mut guard,
                )
                .await;
        }

        let mut current = cfg.start.clone();
        let mut step: u32 = 0;
        // Retry attempts already spent on `current`. Rides the checkpoint
        // so a segment cut or crash mid-retry resumes with the count instead of
        // restarting attempts per resume. Reset to 0 on every advance to a
        // fresh node.
        let mut retry_attempt: u32 = 0;
        let mut suppressed_errors: Vec<ErrorRecord> = Vec::new();
        let cache = NodeCache::new(&self.graph.graph_id);
        let mut resumed = false;

        // Resume state injected by main.rs from an identity-verified local
        // checkpoint. Presence is authoritative: it must parse as the one full
        // typed DTO and match this exact graph, or the run fails preflight.
        // A malformed resume can never fall through into a cold start.
        if let Some(resume_val) = params.get("resume_state") {
            let resume = match crate::resume::from_injected_value(resume_val, &self.graph) {
                Ok(resume) => resume,
                Err(error) => {
                    return self
                        .fail_runtime_preflight(
                            graph_run_id,
                            format!("invalid graph resume_state: {error}"),
                            &mut guard,
                        )
                        .await;
                }
            };
            let restored_accounting =
                match serde_json::from_value::<GraphAccounting>(resume.accounting.clone()) {
                    Ok(accounting) => accounting,
                    Err(error) => {
                        return self
                            .fail_runtime_preflight(
                                graph_run_id,
                                format!("invalid graph resume_state accounting: {error}"),
                                &mut guard,
                            )
                            .await;
                    }
                };
            let restored_errors = match serde_json::from_value::<Vec<ErrorRecord>>(
                resume.suppressed_errors.clone(),
            ) {
                Ok(errors) => errors,
                Err(error) => {
                    return self
                        .fail_runtime_preflight(
                            graph_run_id,
                            format!("invalid graph resume_state suppressed_errors: {error}"),
                            &mut guard,
                        )
                        .await;
                }
            };

            current = resume.current_node;
            step = resume.step_count;
            state = resume.state;
            resumed = true;
            // Restore the ORIGINAL run id so a follow re-entry re-drives
            // spawn_follow_child with the same graph_run_id -> same follow_key
            // -> idempotent. Done before graph_started and the run loop.
            graph_run_id = resume.graph_run_id;
            retry_attempt = resume.retry_attempt;
            *self.accounting.lock().unwrap() = restored_accounting;
            suppressed_errors = restored_errors;

            // The shared resume validator has already proven the marker points
            // at this cursor and that fanout/single-follow snapshot presence is
            // exact. Arm it once for consumption at the follow node.
            if let Some(pending) = resume.pending_follow {
                *self.follow_resume.lock().unwrap() = Some(FollowResumeState {
                    follow_node: pending.follow_node,
                    follow_result: resume.follow_result,
                    item_refs: pending.item_refs,
                    iteration_snapshot: pending.iteration_snapshot,
                });
            }
            tracing::info!(
                node = %current,
                step,
                "resuming graph from injected state"
            );
        }

        // Initialize incremental history accounting exactly once after the
        // authoritative resume DTO has either restored both histories or left
        // them empty for a fresh run. This avoids rescanning on every step.
        if let Err(error) = self.seed_run_history(&suppressed_errors) {
            return self
                .fail_runtime_preflight(
                    graph_run_id,
                    format!("invalid graph resume history: {error}"),
                    &mut guard,
                )
                .await;
        }

        // GraphStarted belongs to the logical graph run, not to each process
        // segment. A resume restores that run and must not duplicate observer
        // side effects or graph-started cost accounting.
        if !resumed {
            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphStarted,
                    json!({
                        "graph_id": &self.graph.graph_id,
                        "definition_ref": &self.graph.definition_ref,
                        "definition_hash": &self.graph.definition_hash,
                        "graph_run_id": &graph_run_id,
                    }),
                )
                .await;
            self.record_callback_warning(RuntimeEventType::GraphStarted.as_str(), r);
            self.fire_graph_hooks(
                self.graph_started_hook_occurrence(&graph_run_id),
                json!({
                    "event": RuntimeEventType::GraphStarted.as_str(),
                    "graph_id": &self.graph.graph_id,
                    "graph_run_id": &graph_run_id,
                    "state": &state,
                    "inputs": &inputs,
                }),
            )
            .await;
        }

        // A graph-started hook may have dispatched real children before its
        // checked rollup rejected an overflow. Do not execute the first authored
        // node once accounting authority is incomplete.
        if let Err(error) = self.ensure_run_history_bounded() {
            let outcome = StepOutcome::Terminal(TerminalOutcome {
                status: GraphRunStatus::Error,
                error: Some(error.to_string()),
                origin: TerminalOrigin::RunControl,
                output: None,
            });
            return match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                    cache: &cache,
                })
                .await
            {
                CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                CommitResult::Terminate(result) => *result,
            };
        }

        // ── F3 main loop: run_node_body → commit_step ───────────
        // Every iteration produces exactly one complete StepOutcome and routes
        // it through commit_step. Execution may publish live foreach/retry
        // progress, but state mutation, receipts, checkpoints, and transition-
        // commit events happen only after that outcome exists.
        //
        // `step` is cumulative across the continuation chain (restored on
        // resume); `steps_this_segment` is per-thread and bounds one segment
        // before the walker cuts a machine continuation. A `None` segment budget
        // ⇒ run until a terminal node or `max_steps`.
        let segment_limit = cfg.segment_steps.unwrap_or(u32::MAX);
        let mut steps_this_segment: u32 = 0;
        while step < cfg.max_steps && steps_this_segment < segment_limit {
            // Cooperative control: between every node, drain any operator commands
            // (cancel/kill/…) queued for this thread and settle each, then fall
            // back to the signal-driven cancel flag (daemon graceful cancel via
            // SIGTERM). A cancel or kill routes a terminal outcome through
            // commit_step exactly like any other terminal — full lifecycle,
            // checkpoint semantics, the thread settles cancelled/killed — rather
            // than a hard process signal landing mid-node.
            if let Some(control) = self.pending_control().await {
                let outcome = StepOutcome::Terminal(TerminalOutcome {
                    status: control.action.terminal_status(),
                    error: control.reason,
                    origin: TerminalOrigin::RunControl,
                    output: None,
                });
                return match self
                    .commit_step(CommitStepInput {
                        graph_run_id: &graph_run_id,
                        step,
                        current: &current,
                        state: &mut state,
                        suppressed_errors: &mut suppressed_errors,
                        outcome,
                        guard: &mut guard,
                        inputs: &inputs,
                        execution: &execution_context,
                        cache: &cache,
                    })
                    .await
                {
                    CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                    CommitResult::Terminate(result) => *result,
                };
            }

            let node = match cfg.nodes.get(&current) {
                Some(n) => n,
                None => {
                    // Node not found is a terminal error — route through
                    // commit_step so it gets proper lifecycle.
                    let outcome = StepOutcome::Terminal(TerminalOutcome {
                        status: GraphRunStatus::Error,
                        error: Some(format!("node '{current}' not found")),
                        origin: TerminalOrigin::RunControl,
                        output: None,
                    });
                    match self
                        .commit_step(CommitStepInput {
                            graph_run_id: &graph_run_id,
                            step,
                            current: &current,
                            state: &mut state,
                            suppressed_errors: &mut suppressed_errors,
                            outcome,
                            guard: &mut guard,
                            inputs: &inputs,
                            execution: &execution_context,
                            cache: &cache,
                        })
                        .await
                    {
                        CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                        CommitResult::Terminate(result) => return *result,
                    }
                }
            };

            let outcome = self
                .run_node_body(RunNodeBodyContext {
                    current: &current,
                    node,
                    cfg,
                    step,
                    state: &state,
                    inputs: &inputs,
                    exec_ctx: &exec_ctx,
                    cache: &cache,
                    graph_run_id: &graph_run_id,
                    retry_attempt,
                })
                .await;

            match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                    cache: &cache,
                })
                .await
            {
                CommitResult::Advance {
                    next_node,
                    next_step,
                    next_retry_attempt,
                } => {
                    current = next_node;
                    step = next_step;
                    retry_attempt = next_retry_attempt;
                    steps_this_segment += 1;
                }
                CommitResult::Terminate(result) => return *result,
            }
        }

        // Budget exhausted without reaching a terminal node. The hard ceiling
        // fails; a segment-budget cut (step < max_steps) hands off to a machine
        // continuation successor that resumes from the checkpoint the last
        // commit_step wrote (pointing at `current`).
        if step >= cfg.max_steps {
            let outcome = StepOutcome::Terminal(TerminalOutcome {
                status: GraphRunStatus::MaxStepsExceeded,
                error: Some(format!("exceeded max_steps ({})", cfg.max_steps)),
                origin: TerminalOrigin::RunControl,
                output: None,
            });
            return match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: "",
                    state: &mut state,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                    cache: &cache,
                })
                .await
            {
                CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                CommitResult::Terminate(result) => *result,
            };
        }

        // A cancel/kill that arrived during the final segment step (or a SIGTERM
        // flag set) must not be lost to the continuation cut: the successor would
        // launch fresh, carrying no cancel. Re-check before handing off and
        // finalize cooperatively instead of continuing.
        if let Some(control) = self.pending_control().await {
            let outcome = StepOutcome::Terminal(TerminalOutcome {
                status: control.action.terminal_status(),
                error: control.reason,
                origin: TerminalOrigin::RunControl,
                output: None,
            });
            return match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                    cache: &cache,
                })
                .await
            {
                CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                CommitResult::Terminate(result) => *result,
            };
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
            graph_run_id: graph_run_id.clone(),
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

        // Segment budget exhausted: cut a machine continuation. The exact result
        // returned on stdout travels into the atomic handoff so the signed source
        // snapshot is already authoritative when the daemon settles `continued`.
        let completion = continued_terminal_completion(
            &continued_result,
            self.warnings.lock().unwrap().snapshot(),
        );
        if let Err(e) = self
            .client
            .request_continuation(Some(SEGMENT_CONTINUATION_REASON), completion)
            .await
        {
            let outcome = StepOutcome::Terminal(TerminalOutcome {
                status: GraphRunStatus::Error,
                error: Some(format!("continuation handoff failed: {e}")),
                origin: TerminalOrigin::RunControl,
                output: None,
            });
            return match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                    cache: &cache,
                })
                .await
            {
                CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                CommitResult::Terminate(result) => *result,
            };
        }

        // Handoff accepted: settle `continued` WITHOUT the terminal lifecycle
        // (no GraphCompleted, no finalize-as-completed). The daemon settles the
        // thread to Continued and launches the successor off this status. The
        // checkpoint already written by the last commit_step is the resume point.
        guard.finalized = true;
        continued_result
    }

    /// Drain and settle every operator command queued for this thread between
    /// nodes. Returns `Some` when a `cancel`/`kill` was seen (the walker should
    /// terminate cooperatively); `None` otherwise.
    ///
    /// Claiming a command transitions it to `claimed`, so EVERY drained command
    /// is settled here or it hangs: `cancel`/`kill` are acknowledged `completed`;
    /// any command type the walker does not action between nodes is `rejected` so
    /// state never leaks a stuck `claimed` row. A claim-RPC hiccup is recorded as
    /// callback drift and treated as "nothing pending" — a transient failure must
    /// not fell a healthy run, and the next node re-drains.
    /// The pending cooperative-control decision at a node boundary: a claimed
    /// cancel/kill command (drained and settled here) or, failing that, the
    /// signal-driven cancel flag (a daemon graceful cancel via SIGTERM). Draining
    /// first means a queued command is still settled even when a SIGTERM also
    /// arrived. Evaluated at each loop top AND before a segment-continuation cut,
    /// so a cancel racing the cut is not lost to a fresh successor.
    async fn pending_control(&self) -> Option<ControlDirective> {
        match self.drain_control_commands().await {
            Some(control) => Some(control),
            None if self
                .cancel_flag
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Relaxed)) =>
            {
                Some(ControlDirective {
                    action: ControlAction::Cancel,
                    reason: Some("cooperative cancel by signal".to_string()),
                })
            }
            None => None,
        }
    }

    async fn drain_control_commands(&self) -> Option<ControlDirective> {
        let claimed = match self.client.claim_commands().await {
            Ok(v) => v,
            Err(e) => {
                self.record_callback_warning("claim_commands", Err(e));
                return None;
            }
        };
        let commands = claimed
            .get("commands")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut winner: Option<ControlAction> = None;
        for cmd in commands {
            let Some(command_id) = cmd.get("command_id").and_then(|v| v.as_i64()) else {
                continue;
            };
            let command_type = cmd
                .get("command_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match ControlAction::from_command_type(command_type) {
                Some(action) => {
                    // Keep the highest-severity action when several queue in one
                    // batch (Kill > Cancel).
                    winner = Some(winner.map_or(action, |w| w.max(action)));
                    self.settle_command(
                        command_id,
                        CommandSettlementStatus::Completed,
                        json!({ "acknowledged": action.command_type() }),
                    )
                    .await;
                }
                None => {
                    // Not actioned by the walker between nodes; settle it rejected
                    // so it never hangs in `claimed`.
                    self.settle_command(
                        command_id,
                        CommandSettlementStatus::Rejected,
                        json!({
                            "reason": format!(
                                "graph walker does not action `{command_type}` between nodes"
                            )
                        }),
                    )
                    .await;
                }
            }
        }
        winner.map(|action| ControlDirective {
            action,
            reason: Some(format!(
                "cooperative {} between nodes",
                action.command_type()
            )),
        })
    }

    /// Settle one claimed command, recording a warning (never failing the run)
    /// if the acknowledgement RPC fails — by the time we ack it the command's
    /// effect is already decided.
    async fn settle_command(
        &self,
        command_id: i64,
        status: CommandSettlementStatus,
        result: Value,
    ) {
        let wire_status = status.as_str();
        let r = self
            .client
            .complete_command(command_id, wire_status, result)
            .await;
        self.record_callback_warning(
            &format!("complete_command({command_id},{wire_status})"),
            r.map(|_| ()),
        );
    }

    /// Run one node body and return a complete `StepOutcome` without mutating
    /// graph state or writing receipts/checkpoints. Foreach and retry execution
    /// may emit live progress observations; transition-commit events remain
    /// deferred to `commit_step`.
    async fn run_node_body(&self, ctx: RunNodeBodyContext<'_>) -> StepOutcome {
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
        let start = Instant::now();
        let execution = exec_ctx.as_context_value();
        let compiled = self.graph.compiled.node(current);

        match node.node_type {
            NodeType::Return => {
                let output = match &compiled.output {
                    Some(output) => match ExpressionScope::new(
                        state,
                        inputs,
                        Some(&execution),
                        Some(graph_run_id),
                    )
                    .render_json(output)
                    {
                        Ok(output) => Some(output),
                        Err(error) => {
                            return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                                item_id: None,
                                error: format!(
                                    "expression evaluation failed in `output` for return node `{current}`: {error}"
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
                StepOutcome::Terminal(TerminalOutcome {
                    status: GraphRunStatus::Completed,
                    error: None,
                    origin: TerminalOrigin::Node,
                    output,
                })
            }

            NodeType::Gate => {
                // Gate: evaluate conditions and pick a branch target.
                match edges::evaluate_next(
                    compiled,
                    state,
                    inputs,
                    Some(&execution),
                    Some(graph_run_id),
                ) {
                    Ok(target) => StepOutcome::GateTaken(GateTakenOutcome { target }),
                    Err(error) => StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                        item_id: None,
                        error: format!(
                            "expression evaluation failed selecting `next` for gate `{current}`: {error}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                        effects: ExpressionFailureEffects::default(),
                    }),
                }
            }

            NodeType::Foreach => {
                // Per-node foreach env requirements are rejected at graph load;
                // graph-wide requirements still apply. Check them before even
                // resolving `over`, publishing the live foreach marker, or
                // dispatching the first iteration.
                if let Err(env_error) =
                    crate::env_preflight::check_env_requires(&cfg.env_requires, &[])
                {
                    let item_id = node
                        .action
                        .as_ref()
                        .and_then(|action| action.get("item_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    return StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
                        item_id: Some(item_id),
                        error: format!("env preflight failed: {env_error}"),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                    });
                }

                let over = compiled
                    .over
                    .as_ref()
                    .expect("validated foreach node has compiled over expression");
                let over_val = match ExpressionScope::new(
                    state,
                    inputs,
                    Some(&execution),
                    Some(graph_run_id),
                )
                .render_template(over)
                {
                    Ok(v) => v,
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
                };

                let items = match over_val {
                    Value::Array(arr) => arr,
                    other => {
                        return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                            item_id: None,
                            error: format!(
                                "foreach node `{current}` `over` must evaluate to an array, got {other}"
                            ),
                            next_on_error: resolve_next_on_error(node, cfg),
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            cost: None,
                            effects: ExpressionFailureEffects::default(),
                        });
                    }
                };

                let var = node.foreach_var().to_string();
                let parallel = node.parallel;
                let continue_on_error = matches!(
                    resolve_next_on_error(node, cfg),
                    NextOnError::PolicyContinue
                );

                // The whole iteration set runs inside this ONE walker step, and
                // step lifecycle events are committed only after the body
                // settles — emit a braid-visible start marker first, so an
                // in-flight fanout reads as "foreach running, N items" instead
                // of silence after `graph_started`.
                {
                    let r = self
                        .client
                        .append_runtime_event(
                            RuntimeEventType::GraphForeachStarted,
                            json!({
                                "graph_run_id": graph_run_id,
                                "definition_ref": &self.graph.definition_ref,
                                "definition_hash": &self.graph.definition_hash,
                                "node": current,
                                "node_ref": node_ref(&self.graph.definition_ref, current),
                                "step": step,
                                "total": items.len(),
                                "parallel": parallel,
                                "max_concurrency": node.max_concurrency,
                                "detach": node.detach,
                            }),
                        )
                        .await;
                    self.record_callback_warning("graph_foreach_started", r);
                }

                let foreach_run = if parallel {
                    foreach::run_foreach_parallel(
                        foreach::ForeachContext {
                            items: &items,
                            var: &var,
                            node,
                            compiled,
                            thread_id: &self.thread_id,
                            project_path: &self.project_path,
                            client: &self.client,
                            exec_ctx: Some(exec_ctx),
                            step,
                            current_node: current,
                            graph_run_id,
                            definition_ref: &self.graph.definition_ref,
                            definition_hash: &self.graph.definition_hash,
                            continue_on_error,
                            cancel_flag: self.cancel_flag.clone(),
                        },
                        state,
                        inputs,
                        self.client.clone(),
                        Arc::new(exec_ctx.clone()),
                    )
                    .await
                } else {
                    foreach::run_foreach_sequential(
                        foreach::ForeachContext {
                            items: &items,
                            var: &var,
                            node,
                            compiled,
                            thread_id: &self.thread_id,
                            project_path: &self.project_path,
                            client: &self.client,
                            exec_ctx: Some(exec_ctx),
                            step,
                            current_node: current,
                            graph_run_id,
                            definition_ref: &self.graph.definition_ref,
                            definition_hash: &self.graph.definition_hash,
                            continue_on_error,
                            cancel_flag: self.cancel_flag.clone(),
                        },
                        state,
                        inputs,
                    )
                    .await
                };

                let foreach::ForeachRun {
                    results,
                    statuses,
                    total_items,
                    errors,
                    assign_delta,
                    cost,
                    observations,
                    limit_error,
                    callback_warnings,
                } = foreach_run;
                for warning in callback_warnings {
                    self.record_callback_warning(
                        RuntimeEventType::GraphNodeRetry.as_str(),
                        Err(anyhow::anyhow!(warning)),
                    );
                }
                let foreach_item_id = node
                    .action
                    .as_ref()
                    .and_then(|a| a.get("item_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if let Some(error) = limit_error {
                    return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                        item_id: Some(foreach_item_id),
                        error,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost,
                        effects: ExpressionFailureEffects::foreach(
                            observations,
                            statuses,
                            total_items,
                            errors,
                        ),
                    });
                }

                // Per-item failures obey the node/graph on_error policy,
                // just like a single action node. Only `continue` keeps
                // going (errors become suppressed); fail/redirect abandon
                // the foreach with one combined diagnostic.
                if !errors.is_empty() {
                    match resolve_next_on_error(node, cfg) {
                        NextOnError::PolicyContinue => {}
                        policy => {
                            // Abandoning the foreach under fail/redirect must
                            // still report cost already spent on completed
                            // iterations before the failure.
                            return StepOutcome::ForeachFailed(Box::new(ForeachFailedOutcome {
                                statuses,
                                total_items,
                                errors,
                                item_id: foreach_item_id,
                                next_on_error: policy,
                                elapsed_ms: start.elapsed().as_millis() as u64,
                                cost,
                                observations,
                            }));
                        }
                    }
                }

                let mut candidate_state = state.clone();
                merge_into(&mut candidate_state, &assign_delta);
                if let Some(collect) = &node.collect {
                    if !candidate_state.is_object() {
                        candidate_state = Value::Object(serde_json::Map::new());
                    }
                    candidate_state
                        .as_object_mut()
                        .unwrap()
                        .insert(collect.clone(), Value::Array(results.clone()));
                }
                if let Err(error) =
                    validate_runtime_value(&candidate_state, "foreach candidate state")
                {
                    return StepOutcome::IntegrityFailed(IntegrityFailedOutcome {
                        item_id: Some(foreach_item_id),
                        error: format!(
                            "foreach candidate state exceeded rye-expr/1 bounds: {error}"
                        ),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost,
                        effects: ExpressionFailureEffects::foreach(
                            observations,
                            statuses,
                            total_items,
                            errors,
                        ),
                    });
                }
                let next = match edges::evaluate_next(
                    compiled,
                    &candidate_state,
                    inputs,
                    Some(&execution),
                    Some(graph_run_id),
                ) {
                    Ok(next) => next,
                    Err(error) => {
                        return StepOutcome::ExpressionFailed(ExpressionFailedOutcome {
                            item_id: Some(foreach_item_id),
                            error: format!(
                                "expression evaluation failed selecting `next` for foreach node `{current}`: {error}"
                            ),
                            next_on_error: resolve_next_on_error(node, cfg),
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            cost,
                            effects: ExpressionFailureEffects::foreach(
                                observations,
                                statuses,
                                total_items,
                                errors,
                            ),
                        });
                    }
                };
                StepOutcome::ForeachDone(Box::new(ForeachDoneOutcome {
                    results,
                    statuses,
                    total_items,
                    collect_key: node.collect.clone(),
                    assign_delta,
                    errors,
                    next,
                    item_id: foreach_item_id,
                    cost,
                    observations,
                }))
            }

            NodeType::Action => {
                self.run_action_body(
                    RunNodeBodyContext {
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
                    },
                    start,
                )
                .await
            }
        }
    }

    /// Write a checkpoint pointing at the next node. If the checkpoint
    /// write fails, terminates with an error.
    // Execution plumbing: each argument is a distinct leg of the thread's
    // auth/provenance context, threaded verbatim — a struct would rename,
    // not simplify. Restructure with a compiler in the loop, not here.
    #[allow(clippy::too_many_arguments)]
    async fn write_checkpoint_or_error(
        &self,
        graph_run_id: &str,
        next_node: &str,
        next_step: u32,
        state: &Value,
        suppressed_errors: &[ErrorRecord],
        guard: &mut RunGuard,
        next_retry_attempt: u32,
        inputs: &Value,
    ) -> CommitResult {
        if let Err(e) = self
            .write_checkpoint(
                graph_run_id,
                next_node,
                next_step,
                state,
                suppressed_errors,
                next_retry_attempt,
            )
            .await
        {
            // Checkpoint failure is a hard terminal, but it still uses the one
            // settlement path so suppressed errors, completion hooks,
            // transcript publication, artifact publication, cost, and thread
            // finalization cannot diverge from other graph failures.
            let diagnostic = format!("checkpoint write failed: {e}");
            let mut terminal_state = state.clone();
            let mut terminal_errors = suppressed_errors.to_vec();
            return self
                .commit_terminal(CommitTerminalInput {
                    graph_run_id,
                    steps: next_step,
                    state: &mut terminal_state,
                    suppressed_errors: &mut terminal_errors,
                    base_status: GraphRunStatus::Error,
                    error: Some(&diagnostic),
                    output: None,
                    guard,
                    inputs,
                })
                .await;
        }

        CommitResult::Advance {
            next_node: next_node.to_string(),
            next_step,
            next_retry_attempt,
        }
    }

    // ── Hook firing ────────────────────────────────────────────────

    /// Fire authored observer hooks for `event` against `context`. Hook actions
    /// dispatch through the same callback path node actions use (effective_caps
    /// enforced, cost accrued, braid-visible). Ordinary hook failures are
    /// observer warnings and cannot steer the walk. Accounting or integrity
    /// failures additionally poison run history so terminal settlement fails
    /// closed instead of publishing an under-accounted result.
    async fn fire_graph_hooks(
        &self,
        occurrence: ryeos_runtime::callback::HookDispatchOccurrence,
        context: Value,
    ) {
        let (event, step) = match &occurrence {
            ryeos_runtime::callback::HookDispatchOccurrence::GraphStarted { .. } => {
                (RuntimeEventType::GraphStarted, None)
            }
            ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
                step, ..
            } => (RuntimeEventType::GraphStepCompleted, Some(*step)),
            ryeos_runtime::callback::HookDispatchOccurrence::GraphCompleted { steps, .. } => {
                (RuntimeEventType::GraphCompleted, Some(*steps))
            }
            _ => {
                self.reject_run_history(format!(
                    "non-graph hook occurrence `{}` reached graph runtime",
                    occurrence.event()
                ));
                return;
            }
        };
        match crate::hooks::run_graph_hooks(
            &self.client,
            &self.thread_id,
            &self.project_path,
            self.graph.compiled.hooks(),
            occurrence,
            &context,
        )
        .await
        {
            Ok(Some(cost)) => self.record_hook_cost(event, step, cost),
            Ok(None) => {}
            Err(error) => {
                if let Some(cost) = error.cost.clone() {
                    self.record_hook_cost(event, step, cost);
                }
                if matches!(
                    error.kind,
                    ryeos_runtime::hooks_eval::HookRunErrorKind::Accounting
                        | ryeos_runtime::hooks_eval::HookRunErrorKind::Integrity
                ) {
                    let authority_kind = match error.kind {
                        ryeos_runtime::hooks_eval::HookRunErrorKind::Accounting => "accounting",
                        ryeos_runtime::hooks_eval::HookRunErrorKind::Integrity => "integrity",
                        _ => unreachable!("only authority failures enter this branch"),
                    };
                    self.reject_run_history(format!(
                        "hook `{}` {authority_kind} failure invalidated terminal authority: {error}",
                        event.as_str(),
                    ));
                }
                self.record_warning(format!("graph hook `{}` failed: {error}", event.as_str()));
            }
        }
    }

    fn graph_started_hook_occurrence(
        &self,
        graph_run_id: &str,
    ) -> ryeos_runtime::callback::HookDispatchOccurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::GraphStarted {
            graph_run_id: graph_run_id.to_string(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
        }
    }

    fn graph_step_completed_hook_occurrence(
        &self,
        graph_run_id: &str,
        step: u32,
        node: &str,
    ) -> ryeos_runtime::callback::HookDispatchOccurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::GraphStepCompleted {
            graph_run_id: graph_run_id.to_string(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            step,
            node: node.to_string(),
        }
    }

    fn graph_completed_hook_occurrence(
        &self,
        graph_run_id: &str,
        steps: u32,
    ) -> ryeos_runtime::callback::HookDispatchOccurrence {
        ryeos_runtime::callback::HookDispatchOccurrence::GraphCompleted {
            graph_run_id: graph_run_id.to_string(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            steps,
        }
    }

    /// Build the hook context for a `graph_step_completed` fire point. Carries
    /// the node facts plus `status` (so a hook can observe a failed node before
    /// `on_error` routing) and a clone of the graph state.
    fn step_hook_context(
        &self,
        graph_run_id: &str,
        node: &str,
        step: u32,
        status: GraphStepStatus,
        error: Option<&str>,
        state: &Value,
    ) -> Value {
        let mut ctx = json!({
            "event": RuntimeEventType::GraphStepCompleted.as_str(),
            "graph_id": &self.graph.graph_id,
            "graph_run_id": graph_run_id,
            "node": node,
            "step": step,
            "status": status.as_str(),
            "state": state,
        });
        if let Some(err) = error {
            ctx["error"] = json!(err);
        }
        ctx
    }
}

fn merge_into(target: &mut Value, source: &Value) {
    if let (Value::Object(ref mut t_map), Value::Object(ref s_map)) = (target, source) {
        for (k, v) in s_map {
            t_map.insert(k.clone(), v.clone());
        }
    }
}

fn hash_json_value(value: &Value) -> Result<String, lillux::cas::CanonicalJsonError> {
    let canonical = lillux::cas::canonical_json(value)?;
    Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
}

fn compute_cache_key(
    definition_hash: &str,
    graph_id: &str,
    node_name: &str,
    action: &Value,
) -> Result<String, lillux::cas::CanonicalJsonError> {
    // Length-prefix each identity component so concatenation cannot alias.
    // The definition hash prevents a changed graph from reusing an entry, and
    // canonical JSON gives object-key ordering one deterministic identity.
    let mut hasher = Sha256::new();
    let canonical_action = lillux::cas::canonical_json(action)?;
    for component in [
        definition_hash.as_bytes(),
        graph_id.as_bytes(),
        node_name.as_bytes(),
        canonical_action.as_bytes(),
    ] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    Ok(lillux::cas::sha256_hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests;
