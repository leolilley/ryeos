#[cfg(test)]
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::cache::NodeCache;
use crate::context;
use crate::dispatch;
use crate::edges;
use crate::env_preflight;
use crate::foreach;
use crate::knowledge;
use crate::model::*;
use crate::persistence;
use crate::validation::analyze_graph;
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::checkpoint::CheckpointWriter;
use ryeos_runtime::envelope::RuntimeCost;
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::TerminalCompletion;

fn add_runtime_cost(total: &mut Option<RuntimeCost>, cost: Option<RuntimeCost>) {
    let Some(cost) = cost else { return };
    let acc = total.get_or_insert(RuntimeCost {
        input_tokens: 0,
        output_tokens: 0,
        total_usd: 0.0,
        basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
    });
    acc.input_tokens += cost.input_tokens;
    acc.output_tokens += cost.output_tokens;
    acc.total_usd += cost.total_usd;
}

/// Schema version of the graph checkpoint payload. Bump on any incompatible
/// change to the written fields; the resume parser rejects an unknown version.
///
/// v2 adds `retry_attempt` (the per-step retry counter) so a segment cut or a
/// crash mid-retry resumes with the count instead of restarting it per resume.
pub(crate) const GRAPH_CHECKPOINT_SCHEMA_VERSION: u32 = 2;

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

/// Running cost accumulator for a single graph execution. Owned by the
/// walker behind a `Mutex` (like `warnings`) so cost can be recorded with
/// `&self` from `commit_step` — the single state-mutation point.
///
/// SEMANTICS: the aggregate is a **rollup** — a node that dispatches a
/// cost-bearing directive/sub-graph child includes that child's cost here,
/// and the child thread is ALSO finalized with its own cost. Downstream
/// billing/reporting must therefore NOT sum `final_cost` across a thread
/// tree, or nested executions are double-counted. Parent graph cost is a
/// rollup/display figure.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct GraphAccounting {
    /// Aggregate across every cost-bearing node. `None` until the first
    /// node reports cost, so a pure-tool graph finalizes `cost: None`
    /// rather than a misleading all-zeros record.
    total: Option<RuntimeCost>,
    /// One record per cost-bearing node, in execution order.
    nodes: Vec<NodeCostRecord>,
}

impl GraphAccounting {
    fn record(&mut self, node: &str, step: u32, item_id: &str, cost: RuntimeCost) {
        let total = self.total.get_or_insert(RuntimeCost {
            input_tokens: 0,
            output_tokens: 0,
            total_usd: 0.0,
            // The aggregate is marked as a rollup on the wire so downstream
            // consumers can render it as derived, never as own-spend.
            basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
        });
        total.input_tokens += cost.input_tokens;
        total.output_tokens += cost.output_tokens;
        total.total_usd += cost.total_usd;
        self.nodes.push(NodeCostRecord {
            node: node.to_string(),
            step,
            item_id: item_id.to_string(),
            cost,
        });
    }
}

// ── F3 advanced path: StepOutcome + commit_step ─────────────────
//
// D13: `commit_step` is the single mutation point for graph state
// lifecycle. Every walker branch produces exactly one `StepOutcome`
// and hands it to `commit_step`. The walker's main loop never
// appends a step event, writes a receipt, or writes a checkpoint
// outside `commit_step`.

/// What happened during a single step's body, before lifecycle
/// persistence. Every walker branch produces exactly one of these.
enum StepOutcome {
    /// Action node succeeded; leaf returned a result.
    ActionOk {
        item_id: String,
        result: Value,
        /// `assign` template already interpolated against the leaf
        /// result in `run_action_body`. `commit_step` merges it into
        /// state verbatim — interpolation does not run here, so a raw
        /// `${...}` template can never reach state.
        assign: Option<Value>,
        next: Option<String>,
        cache_hit: bool,
        elapsed_ms: u64,
        /// Cost reported by the leaf's native child (directive/sub-graph),
        /// if any. `None` for subprocess leaves and cache hits.
        cost: Option<RuntimeCost>,
    },
    /// Action node ran but the leaf reported `status == "error"`, OR the
    /// leaf succeeded (possibly with cost) and graph post-processing
    /// (`assign` interpolation) then failed. Carries any cost the child
    /// spent before the error so accounting is not lost.
    LeafSoftError {
        item_id: String,
        error: String,
        next_on_error: NextOnError,
        elapsed_ms: u64,
        cost: Option<RuntimeCost>,
    },
    /// Dispatch failed before the leaf returned anything (transport,
    /// permission, env preflight) — so normally no cost. The foreach
    /// fail/redirect path reuses this variant and DOES carry the aggregate
    /// cost already spent across completed iterations.
    DispatchHardError {
        item_id: Option<String>,
        error: String,
        next_on_error: NextOnError,
        elapsed_ms: u64,
        cost: Option<RuntimeCost>,
    },
    /// Gate node: condition evaluation picked `target`.
    GateTaken { target: Option<String> },
    /// Foreach node completed all iterations.
    ForeachDone {
        results: Vec<Value>,
        collect_key: Option<String>,
        var_name: String,
        /// Accumulated foreach `assign` mutations to merge into state.
        assign_delta: Value,
        /// Per-item failures, surfaced as suppressed errors (only present
        /// under the `continue` policy — fail/redirect never reach here).
        errors: Vec<ErrorRecord>,
        next: Option<String>,
        /// `item_id` of the foreach node's action, for the aggregated
        /// cost record.
        item_id: String,
        /// Aggregate cost across all iterations' native children, if any.
        cost: Option<RuntimeCost>,
    },
    /// Follow node: suspend the graph and hand the action off to a detached child
    /// via `spawn_follow_child`. No result exists yet — it is consumed on resume.
    /// Carries only the child item + params; the daemon derives the rest.
    FollowSuspend { item_id: String, params: Value },
    FollowFanoutSuspend {
        children: Vec<ryeos_runtime::callback::FollowChildSpec>,
        width: Option<u32>,
    },
    FollowFanoutDone {
        results: Vec<Value>,
        statuses: Vec<String>,
        errors: Vec<ErrorRecord>,
        assign_delta: Value,
        collect_key: Option<String>,
        var_name: String,
        item_id: String,
        next: Option<String>,
        next_on_error: NextOnError,
        cost: Option<RuntimeCost>,
        elapsed_ms: u64,
    },
    /// Action dispatch failed and the node's `retry` policy has attempts left.
    /// The walker checkpoints the incremented attempt count on the SAME node,
    /// backs off, and re-dispatches on the next step (so max_steps/segment_steps
    /// bound total retry work). Routing to `on_error` happens only once attempts
    /// are exhausted.
    RetryScheduled {
        item_id: String,
        error: String,
        /// 1-based number of the attempt that just failed.
        failed_attempt: u32,
        /// Total attempts configured (`retry.attempts`).
        total_attempts: u32,
        /// Backoff before the re-dispatch, in milliseconds.
        delay_ms: u64,
        elapsed_ms: u64,
        /// Cost a native child spent before failing this attempt, if any.
        cost: Option<RuntimeCost>,
    },
    /// Terminal step — return node, max-steps exhausted, or fatal fail.
    Terminal {
        status: &'static str,
        error: Option<String>,
    },
}

/// How to handle an error when the per-node `on_error` policy is set
/// vs the graph-level `on_error` mode.
enum NextOnError {
    /// Node declares `on_error: <target>` — redirect to that node.
    Redirect(String),
    /// Graph-level `on_error: continue` — suppress and advance.
    PolicyContinue,
    /// Graph-level `on_error: fail` (default) — hard terminate.
    PolicyFail,
}

/// Return from `commit_step`: either advance to the next node or
/// terminate the graph run.
enum CommitResult {
    Advance {
        next_node: String,
        next_step: u32,
        /// Retry attempts already spent on `next_node`. Non-zero only when the
        /// walker is re-entering the same node under a `retry` backoff; every
        /// advance to a fresh node resets it to 0.
        next_retry_attempt: u32,
    },
    Terminate(GraphResult),
}

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
    fn terminal_status(self) -> &'static str {
        match self {
            Self::Cancel => "cancelled",
            Self::Kill => "killed",
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
    warnings: Mutex<Vec<String>>,
    /// Per-run token/spend accounting, accumulated as cost-bearing nodes
    /// commit. Interior-mutable for the same reason as `warnings`: it is
    /// updated with `&self` from `commit_step` and read once at terminal
    /// finalization. The lock is held for a single record/read, never
    /// across an `await`.
    accounting: Mutex<GraphAccounting>,
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

/// A pending follow result armed at resume, consumed at its follow node.
struct FollowResumeState {
    follow_node: String,
    follow_result: Value,
}

struct RunGuard {
    finalized: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.finalized {
            tracing::warn!("graph RunGuard dropped without finalization");
        }
    }
}

struct RunNodeBodyContext<'a> {
    pub current: &'a str,
    pub node: &'a GraphNode,
    pub cfg: &'a GraphConfig,
    pub step: u32,
    pub state: &'a Value,
    pub inputs: &'a Value,
    pub exec_ctx: &'a context::ExecutionContext,
    pub cache: &'a NodeCache,
    pub graph_run_id: &'a str,
    pub suppressed_errors: &'a mut Vec<ErrorRecord>,
    /// Retry attempts already spent on this node (0 on a fresh entry). The
    /// action body uses it to decide whether a further dispatch failure has
    /// attempts remaining under the node's `retry` policy.
    pub retry_attempt: u32,
}

struct CommitStepInput<'a> {
    pub graph_run_id: &'a str,
    pub step: u32,
    pub current: &'a str,
    pub state: &'a mut Value,
    pub receipts: &'a mut Vec<NodeReceipt>,
    pub suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub outcome: StepOutcome,
    pub guard: &'a mut RunGuard,
    pub inputs: &'a Value,
    pub execution: &'a Value,
}

struct CommitTerminalInput<'a> {
    pub graph_run_id: &'a str,
    pub steps: u32,
    pub state: &'a mut Value,
    pub suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub base_status: &'a str,
    pub error: Option<&'a str>,
    pub guard: &'a mut RunGuard,
    pub current_node_id: &'a str,
    /// Graph inputs, threaded so a return node's `output` template can
    /// resolve `${inputs.*}` (not just `${state.*}`).
    pub inputs: &'a Value,
    pub execution: &'a Value,
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
            warnings: Mutex::new(Vec::new()),
            accounting: Mutex::new(GraphAccounting::default()),
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

    /// If a follow result is armed for `node`, take it (consumed once). The
    /// resumed follow node classifies this envelope like a live dispatch instead
    /// of re-suspending.
    fn take_follow_result(&self, node: &str) -> Option<Value> {
        let mut slot = self.follow_resume.lock().unwrap();
        if slot.as_ref().is_some_and(|fr| fr.follow_node == node) {
            return slot.take().map(|fr| fr.follow_result);
        }
        None
    }

    /// Drain the accumulated callback-drift warnings. Called by the
    /// graph-runtime binary's `main.rs` after `execute` returns so the
    /// drift can be threaded into `RuntimeResult.warnings`.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap())
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
            self.warnings
                .lock()
                .unwrap()
                .push(format!("callback {label} failed: {e}"));
        }
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
                "valid".into()
            } else {
                "invalid".into()
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
        *self.accounting.lock().unwrap() = GraphAccounting::default();
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
                status: "invalid".into(),
                steps: 0,
                state: json!({}),
                result: None,
                errors_suppressed: None,
                errors: None,
                error: Some(validation.errors.join("; ")),
                cost: None,
                node_costs: Vec::new(),
            };
            let completion = TerminalCompletion {
                status: "failed".to_string(),
                outcome_code: Some("failed".to_string()),
                result: None,
                error: Some(json!(validation.errors.join("; "))),
                cost: None,
                outputs: Value::Null,
                warnings: Vec::new(),
            };
            let r = self.client.finalize_thread(completion).await;
            self.record_callback_warning("finalize_thread", r.map(|_| ()));
            guard.finalized = true;
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
        let inputs = params.get("inputs").cloned().unwrap_or(json!({}));

        // Initial state precedence (lowest → highest): authored
        // `config.state` defaults, then caller `inject_state`, then
        // `resume_state` (handled below) for a resumed run.
        let mut state = cfg.state.clone().unwrap_or_else(|| json!({}));

        if let Some(defaults) = params.get("inject_state") {
            merge_into(&mut state, defaults);
        }

        let mut current = cfg.start.clone();
        let mut step: u32 = 0;
        // Retry attempts already spent on `current`. Rides the checkpoint (v2)
        // so a segment cut or crash mid-retry resumes with the count instead of
        // restarting attempts per resume. Reset to 0 on every advance to a
        // fresh node.
        let mut retry_attempt: u32 = 0;
        let mut suppressed_errors: Vec<ErrorRecord> = Vec::new();
        let mut receipts: Vec<NodeReceipt> = Vec::new();
        let cache = NodeCache::new(&self.graph.graph_id);

        // Resume state injected by main.rs (from the checkpoint or event
        // replay). main.rs owns the cold-start decision when RYEOS_RESUME=1.
        if let Some(resume_val) = params.get("resume_state") {
            if let Some(node) = resume_val.get("current_node").and_then(|v| v.as_str()) {
                current = node.to_string();
                step = resume_val
                    .get("step_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                state = resume_val.get("state").cloned().unwrap_or(json!({}));
                // Restore the ORIGINAL run id so a follow re-entry re-drives
                // spawn_follow_child with the same graph_run_id → same follow_key
                // → idempotent. Done before graph_started and the run loop.
                if let Some(rid) = resume_val.get("graph_run_id").and_then(|v| v.as_str()) {
                    graph_run_id = rid.to_string();
                }
                // Restore the per-step retry counter so a mid-retry segment cut
                // or crash resumes with the attempts already spent. Schema-mismatched
                // checkpoints are rejected before this point; absence here means a
                // fresh-node resume or non-retry checkpoint, so default to 0.
                retry_attempt = resume_val
                    .get("retry_attempt")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                // Arm follow resume: if the checkpoint marks a pending follow AND a
                // child envelope was spliced in, consume it at the follow node
                // instead of re-suspending. Only when BOTH are present — a bare
                // pending_follow with no result re-drives the suspend (idempotent).
                if let (Some(pf), Some(fr)) = (
                    resume_val
                        .get(follow_keys::PENDING_FOLLOW)
                        .filter(|v| !v.is_null()),
                    resume_val
                        .get(follow_keys::FOLLOW_RESULT)
                        .filter(|v| !v.is_null()),
                ) {
                    if let Some(fnode) = pf.get(follow_keys::FOLLOW_NODE).and_then(|v| v.as_str()) {
                        *self.follow_resume.lock().unwrap() = Some(FollowResumeState {
                            follow_node: fnode.to_string(),
                            follow_result: fr.clone(),
                        });
                    }
                }
                // Restore accumulated cost so post-resume cost adds to the
                // pre-checkpoint total instead of restarting at zero. A corrupt
                // snapshot degrades to fresh accounting (under-bills) rather than
                // failing an otherwise-correct resume.
                if let Some(acc_val) = resume_val.get("accounting").filter(|v| !v.is_null()) {
                    match serde_json::from_value::<GraphAccounting>(acc_val.clone()) {
                        Ok(acc) => *self.accounting.lock().unwrap() = acc,
                        Err(e) => tracing::warn!(
                            error = %e,
                            "failed to restore accounting from checkpoint; starting fresh"
                        ),
                    }
                }
                // Restore suppressed errors accumulated before the checkpoint so
                // the resumed run's final error count/list is complete. A corrupt
                // snapshot degrades to empty rather than failing the resume.
                if let Some(se_val) = resume_val.get("suppressed_errors").filter(|v| !v.is_null()) {
                    match serde_json::from_value::<Vec<ErrorRecord>>(se_val.clone()) {
                        Ok(se) => suppressed_errors = se,
                        Err(e) => tracing::warn!(
                            error = %e,
                            "failed to restore suppressed_errors from checkpoint; starting empty"
                        ),
                    }
                }
                tracing::info!(
                    node = %current,
                    step,
                    "resuming graph from injected state"
                );
            }
        }

        // Emit graph_started runtime event (before the loop — not per-step).
        {
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
            self.record_callback_warning("graph_started", r);
        }

        // Fire graph_started observer hooks before the walk begins.
        self.fire_graph_hooks(
            "graph_started",
            json!({
                "event": "graph_started",
                "graph_id": &self.graph.graph_id,
                "graph_run_id": &graph_run_id,
                "state": &state,
                "inputs": &inputs,
            }),
        )
        .await;

        // ── F3 main loop: run_node_body → commit_step ───────────
        // Every iteration produces exactly one StepOutcome and routes
        // through commit_step. ALL persistence happens there.
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
                let outcome = StepOutcome::Terminal {
                    status: control.action.terminal_status(),
                    error: control.reason,
                };
                return match self
                    .commit_step(CommitStepInput {
                        graph_run_id: &graph_run_id,
                        step,
                        current: &current,
                        state: &mut state,
                        receipts: &mut receipts,
                        suppressed_errors: &mut suppressed_errors,
                        outcome,
                        guard: &mut guard,
                        inputs: &inputs,
                        execution: &execution_context,
                    })
                    .await
                {
                    CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                    CommitResult::Terminate(result) => result,
                };
            }

            let node = match cfg.nodes.get(&current) {
                Some(n) => n,
                None => {
                    // Node not found is a terminal error — route through
                    // commit_step so it gets proper lifecycle.
                    let outcome = StepOutcome::Terminal {
                        status: "error",
                        error: Some(format!("node '{current}' not found")),
                    };
                    match self
                        .commit_step(CommitStepInput {
                            graph_run_id: &graph_run_id,
                            step,
                            current: &current,
                            state: &mut state,
                            receipts: &mut receipts,
                            suppressed_errors: &mut suppressed_errors,
                            outcome,
                            guard: &mut guard,
                            inputs: &inputs,
                            execution: &execution_context,
                        })
                        .await
                    {
                        CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                        CommitResult::Terminate(result) => return result,
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
                    suppressed_errors: &mut suppressed_errors,
                    retry_attempt,
                })
                .await;

            match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    receipts: &mut receipts,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
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
                CommitResult::Terminate(result) => return result,
            }
        }

        // Budget exhausted without reaching a terminal node. The hard ceiling
        // fails; a segment-budget cut (step < max_steps) hands off to a machine
        // continuation successor that resumes from the checkpoint the last
        // commit_step wrote (pointing at `current`).
        if step >= cfg.max_steps {
            let outcome = StepOutcome::Terminal {
                status: "max_steps_exceeded",
                error: Some(format!("exceeded max_steps ({})", cfg.max_steps)),
            };
            return match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: "",
                    state: &mut state,
                    receipts: &mut receipts,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                })
                .await
            {
                CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                CommitResult::Terminate(result) => result,
            };
        }

        // A cancel/kill that arrived during the final segment step (or a SIGTERM
        // flag set) must not be lost to the continuation cut: the successor would
        // launch fresh, carrying no cancel. Re-check before handing off and
        // finalize cooperatively instead of continuing.
        if let Some(control) = self.pending_control().await {
            let outcome = StepOutcome::Terminal {
                status: control.action.terminal_status(),
                error: control.reason,
            };
            return match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    receipts: &mut receipts,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                })
                .await
            {
                CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                CommitResult::Terminate(result) => result,
            };
        }

        // Segment budget exhausted: cut a machine continuation. A failed handoff
        // settles the thread as a terminal error rather than leaving it
        // `continued` with no successor.
        if let Err(e) = self
            .client
            .request_continuation(Some(SEGMENT_CONTINUATION_REASON))
            .await
        {
            let outcome = StepOutcome::Terminal {
                status: "error",
                error: Some(format!("continuation handoff failed: {e}")),
            };
            return match self
                .commit_step(CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    receipts: &mut receipts,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    inputs: &inputs,
                    execution: &execution_context,
                })
                .await
            {
                CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                CommitResult::Terminate(result) => result,
            };
        }

        // Handoff accepted: settle `continued` WITHOUT the terminal lifecycle
        // (no GraphCompleted, no finalize-as-completed). The daemon settles the
        // thread to Continued and launches the successor off this status. The
        // checkpoint already written by the last commit_step is the resume point.
        guard.finalized = true;
        let (agg_cost, node_costs) = {
            let acc = self.accounting.lock().unwrap();
            (acc.total.clone(), acc.nodes.clone())
        };
        GraphResult {
            success: false,
            graph_id: self.graph.graph_id.clone(),
            definition_ref: self.graph.definition_ref.clone(),
            definition_hash: self.graph.definition_hash.clone(),
            graph_run_id: graph_run_id.clone(),
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
        }
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
                        "completed",
                        json!({ "acknowledged": action.command_type() }),
                    )
                    .await;
                }
                None => {
                    // Not actioned by the walker between nodes; settle it rejected
                    // so it never hangs in `claimed`.
                    self.settle_command(
                        command_id,
                        "rejected",
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
    async fn settle_command(&self, command_id: i64, status: &str, result: Value) {
        let r = self
            .client
            .complete_command(command_id, status, result)
            .await;
        self.record_callback_warning(
            &format!("complete_command({command_id},{status})"),
            r.map(|_| ()),
        );
    }

    /// Run a single node's body, producing a `StepOutcome` without
    /// emitting any events, writing any receipts, or writing any
    /// checkpoints. ALL persistence is deferred to `commit_step`.
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
            suppressed_errors,
            retry_attempt,
        } = ctx;
        let start = Instant::now();
        let execution = exec_ctx.as_context_value();

        match node.node_type {
            NodeType::Return => StepOutcome::Terminal {
                status: "completed",
                error: None,
            },

            NodeType::Gate => {
                // Gate: evaluate conditions and pick a branch target.
                let target = edges::evaluate_next(node, state, inputs, Some(&execution));
                StepOutcome::GateTaken { target }
            }

            NodeType::Foreach => {
                let over_expr = node.over.as_deref().unwrap_or("${state.items}");
                let ctx = WalkContext {
                    state: state.clone(),
                    inputs: inputs.clone(),
                    result: None,
                    execution: Some(execution.clone()),
                    graph_run_id: Some(graph_run_id.to_string()),
                };
                let over_val = match ryeos_runtime::interpolate(
                    &Value::String(over_expr.to_string()),
                    &ctx.as_context(),
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        // A foreach that can't resolve its iteration set
                        // is a node error — route it through on_error
                        // rather than silently iterating an empty list.
                        return StepOutcome::DispatchHardError {
                            item_id: None,
                            error: format!(
                                "interpolation error in `over` for node `{current}`: {e:#}"
                            ),
                            next_on_error: resolve_next_on_error(node, cfg),
                            elapsed_ms: start.elapsed().as_millis() as u64,
                            // `over` failed before any iteration ran — no cost.
                            cost: None,
                        };
                    }
                };

                let items = match over_val {
                    Value::Array(arr) => arr,
                    Value::String(s) => {
                        if s.contains(',') {
                            s.split(',')
                                .map(|x| Value::String(x.trim().to_string()))
                                .collect()
                        } else {
                            vec![Value::String(s)]
                        }
                    }
                    other => vec![other],
                };

                let var = node.foreach_var().to_string();
                let parallel = node.parallel;

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
                            thread_id: &self.thread_id,
                            project_path: &self.project_path,
                            client: &self.client,
                            exec_ctx: Some(exec_ctx),
                            step,
                            current_node: &current,
                            graph_run_id,
                            definition_ref: &self.graph.definition_ref,
                            definition_hash: &self.graph.definition_hash,
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
                            thread_id: &self.thread_id,
                            project_path: &self.project_path,
                            client: &self.client,
                            exec_ctx: Some(exec_ctx),
                            step,
                            current_node: &current,
                            graph_run_id,
                            definition_ref: &self.graph.definition_ref,
                            definition_hash: &self.graph.definition_hash,
                        },
                        state,
                        inputs,
                    )
                    .await
                };

                let foreach::ForeachRun {
                    results,
                    errors,
                    assign_delta,
                    cost,
                } = foreach_run;
                let foreach_item_id = node
                    .action
                    .as_ref()
                    .and_then(|a| a.get("item_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

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
                            return StepOutcome::DispatchHardError {
                                item_id: Some(foreach_item_id),
                                error: foreach_failure_summary(&current, &errors),
                                next_on_error: policy,
                                elapsed_ms: start.elapsed().as_millis() as u64,
                                cost: cost.clone(),
                            };
                        }
                    }
                }

                let next = edges::evaluate_next(node, state, inputs, Some(&execution));
                StepOutcome::ForeachDone {
                    results,
                    collect_key: node.collect.clone(),
                    var_name: var,
                    assign_delta,
                    errors,
                    next,
                    item_id: foreach_item_id,
                    cost,
                }
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
                        suppressed_errors,
                        retry_attempt,
                    },
                    start,
                )
                .await
            }
        }
    }

    /// Action node body: permission check → env preflight → dispatch
    /// → classify result. Returns a StepOutcome without emitting any
    /// events or persisting anything.
    async fn run_action_body(&self, ctx: RunNodeBodyContext<'_>, start: Instant) -> StepOutcome {
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
            suppressed_errors: _suppressed_errors,
            retry_attempt,
        } = ctx;
        let execution = exec_ctx.as_context_value();

        // Cohort follow is an action-node state machine of its own. Split before
        // generic action interpolation: the authored action may reference `as`.
        if node.follow && node.over.is_some() {
            return self
                .run_follow_fanout(node, current, cfg, step, state, inputs, &execution, graph_run_id, start)
                .await;
        }
        let mut action = match &node.action {
            Some(a) => a.clone(),
            None => {
                // Action node with no action — treat as terminal.
                let next = edges::evaluate_next(node, state, inputs, Some(&execution));
                return match next {
                    Some(n) => StepOutcome::ActionOk {
                        item_id: String::new(),
                        result: json!({}),
                        assign: None,
                        next: Some(n),
                        cache_hit: false,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        cost: None,
                    },
                    None => StepOutcome::Terminal {
                        status: "completed",
                        error: None,
                    },
                };
            }
        };

        // A `detach: true` node launches a lineage-linked, cohort-tagged child
        // that runs concurrently while this walk continues — the native fanout
        // primitive (`foreach → launch`). The fold routes it to the daemon's
        // `spawn_detached_child` and carries the node's `facets:` for per-child
        // stamping. `detach` and `follow` are mutually exclusive (enforced at
        // validation); a detach node never suspends, so it flows straight to
        // dispatch below.
        node.fold_detach_into_action(&mut action);

        let item_id = action
            .get("item_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let elapsed = start.elapsed().as_millis() as u64;

        // D16: no walker-side permission check — the daemon enforces
        // caps at the callback boundary (enforce_callback_caps in
        // runtime_dispatch.rs). The walker is the executor only.

        let ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
            execution: Some(execution.clone()),
            graph_run_id: Some(graph_run_id.to_string()),
        };

        let interpolated_action =
            match ryeos_runtime::interpolate_action(&action, &ctx.as_context()) {
                Ok(value) => value,
                Err(err) => {
                    // Interpolation failed before any dispatch — no cost, and
                    // the interpolated item_id is unavailable, so report the
                    // raw template item_id.
                    return StepOutcome::DispatchHardError {
                        item_id: Some(item_id),
                        error: format!(
                            "interpolation error in action for node `{current}`: {err:#}"
                        ),
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: elapsed,
                        cost: None,
                    };
                }
            };

        let stripped_action = strip_none_values(&interpolated_action);
        // The dispatched item_id is the interpolated one (item_id may itself
        // contain `${...}`). Cost records and receipts for everything past
        // this point use it, not the raw template id.
        let dispatched_item_id = stripped_action
            .get("item_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| item_id.clone());

        // Resume INTO a follow node: consume the child's stored terminal envelope
        // instead of re-dispatching or re-suspending. `None` on a first run, or a
        // re-drive with no result yet (which re-suspends idempotently). Taken
        // BEFORE env preflight so a parent-side env gap can't turn an already-
        // completed child's result into a dispatch hard error.
        let resumed_follow_envelope = if node.follow {
            self.take_follow_result(current)
        } else {
            None
        };

        // Env preflight — skipped when consuming a stored follow result (the child
        // already ran). Still enforced for first-run follow suspend, bare-marker
        // re-suspend, and normal dispatches.
        if resumed_follow_envelope.is_none() {
            if let Err(env_err) = env_preflight::check_env_requires(
                &self.graph.config.env_requires,
                &node.env_requires,
            ) {
                let err_msg = format!("env preflight failed: {env_err}");
                return StepOutcome::DispatchHardError {
                    item_id: Some(dispatched_item_id),
                    error: err_msg,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    cost: None,
                };
            }
        }

        // A follow node with no stored result does not dispatch inline: hand the
        // action off to a detached child and suspend (handled in commit_step). The
        // result is consumed on resume, so nothing is dispatched or cached here.
        if node.follow && resumed_follow_envelope.is_none() {
            return StepOutcome::FollowSuspend {
                item_id: dispatched_item_id,
                params: stripped_action
                    .get("params")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            };
        }

        // Dispatch. `dispatch_action` classifies the daemon envelope:
        //   Err            → transport/dispatch failure (hard error)
        //   Ok(Failure(d)) → leaf ran but failed (non-zero exit, etc.)
        //   Ok(Success(v)) → bare, envelope-unwrapped leaf result
        // On follow resume, the stored child envelope is classified byte-for-byte
        // like a live dispatch, so the resumed node runs the identical success/
        // failure path (receipt, cost, assign land normally in commit_step).
        let mut cache_hit = false;
        let outcome: Result<dispatch::ActionOutcome, String> =
            if let Some(envelope) = resumed_follow_envelope {
                Ok(dispatch::classify_envelope(envelope))
            } else if node.is_cacheable() {
                let cache_key = compute_cache_key(&self.graph.graph_id, current, &stripped_action);
                if let Some(cached) = cache.lookup(&cache_key) {
                    cache_hit = true;
                    // A cache hit replays the stored result and must NOT re-bill cost —
                    // `bare` carries no cost. A stale/tampered entry still carrying a
                    // top-level continuation_id is rejected loudly, exactly like a live
                    // inline-continuation dispatch (F10 — inline continuation is retired;
                    // use a `follow: true` node). New dispatches never cache such a value.
                    if cached
                        .get("continuation_id")
                        .and_then(|v| v.as_str())
                        .is_some()
                    {
                        Ok(dispatch::ActionOutcome::Failure(dispatch::ActionFailure {
                            diagnostic: format!(
                                "cached result for node `{current}` carries a continuation_id; \
                             inline continuation is retired — use a `follow: true` node"
                            ),
                            cost: None,
                        }))
                    } else {
                        Ok(dispatch::ActionOutcome::Success(
                            dispatch::ActionSuccess::bare(cached),
                        ))
                    }
                } else {
                    match dispatch::dispatch_action(
                        &self.client,
                        &stripped_action,
                        &self.thread_id,
                        &self.project_path,
                        Some(exec_ctx),
                    )
                    .await
                    {
                        Ok(dispatch::ActionOutcome::Success(success)) => {
                            // Only successful dispatches are cached — never a
                            // failure, which would otherwise replay a stale
                            // error (or `null`) on the next run. The cache
                            // stores only the result value; cost is per-run.
                            cache.store(&cache_key, &success.result);
                            Ok(dispatch::ActionOutcome::Success(success))
                        }
                        Ok(failure) => Ok(failure),
                        Err(e) => Err(format!("{e:#}")),
                    }
                }
            } else {
                match dispatch::dispatch_action(
                    &self.client,
                    &stripped_action,
                    &self.thread_id,
                    &self.project_path,
                    Some(exec_ctx),
                )
                .await
                {
                    Ok(o) => Ok(o),
                    Err(e) => Err(format!("{e:#}")),
                }
            };

        let elapsed = start.elapsed().as_millis() as u64;

        match outcome {
            Err(err_detail) => {
                // A transport/dispatch failure with retry attempts remaining
                // reschedules a fresh-step re-dispatch; exhausted → on_error.
                if let Some(failed_attempt) = retry_attempts_remaining(node, retry_attempt) {
                    let rc = node.retry.as_ref().expect("retry present when scheduling");
                    return StepOutcome::RetryScheduled {
                        item_id: dispatched_item_id,
                        error: err_detail,
                        failed_attempt,
                        total_attempts: rc.attempts,
                        delay_ms: rc.delay_ms(failed_attempt),
                        elapsed_ms: elapsed,
                        // Transport failed before the child returned — no cost.
                        cost: None,
                    };
                }
                StepOutcome::DispatchHardError {
                    item_id: Some(dispatched_item_id),
                    error: err_detail,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    // Transport/dispatch failed before the child returned — no cost.
                    cost: None,
                }
            }
            Ok(dispatch::ActionOutcome::Failure(failure)) => {
                // A leaf that ran but failed retries the same way; a failed
                // native child may have spent tokens, carried on every path.
                if let Some(failed_attempt) = retry_attempts_remaining(node, retry_attempt) {
                    let rc = node.retry.as_ref().expect("retry present when scheduling");
                    return StepOutcome::RetryScheduled {
                        item_id: dispatched_item_id,
                        error: failure.diagnostic,
                        failed_attempt,
                        total_attempts: rc.attempts,
                        delay_ms: rc.delay_ms(failed_attempt),
                        elapsed_ms: elapsed,
                        cost: failure.cost,
                    };
                }
                StepOutcome::LeafSoftError {
                    item_id: dispatched_item_id,
                    error: failure.diagnostic,
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                    // A failed native child may have spent tokens — preserve it.
                    cost: failure.cost,
                }
            }
            Ok(dispatch::ActionOutcome::Success(success)) => {
                let dispatch::ActionSuccess {
                    result: val,
                    cost,
                    child_thread_id,
                } = success;
                // Portable dispatch lineage: when this node spawned a native
                // child thread (a directive or sub-graph), emit a
                // `child_thread_spawned` event into THIS (parent) thread's stream
                // so the edge lands in rebuild-safe history — the braid drill
                // target and the derived `threads.children` edge both come from
                // it. The daemon's `thread_child_link` (recorded at launch) is the
                // separate, non-portable cascade copy. Do NOT set the child's
                // `upstream_thread_id`: that is the continuation-predecessor link
                // and stamping it cross-chain corrupts the child's resume.
                if let Some(ref child_id) = child_thread_id {
                    let r = self
                        .client
                        .append_runtime_event(
                            RuntimeEventType::ChildThreadSpawned,
                            json!({
                                "child_thread_id": child_id,
                                "node": current,
                                "step": step,
                                "item_id": dispatched_item_id,
                                "spawn_reason": "dispatch",
                            }),
                        )
                        .await;
                    self.record_callback_warning("child_thread_spawned", r);
                }
                // Domain milestones: a tool/directive result may carry a
                // `milestones` array of `{kind, payload}`; emit one generic
                // `milestone` event per entry into this thread's stream
                // (runtime-on-behalf-of-tool — tools stay pure content, the engine
                // owns only the generic event; a view styles the kinds via
                // `projections.event_kinds`). `node`/`step` locate it in the braid.
                if let Some(milestones) = val.get("milestones").and_then(|v| v.as_array()) {
                    for entry in milestones {
                        let Some(kind) = entry.get("kind").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let r = self
                            .client
                            .append_runtime_event(
                                RuntimeEventType::Milestone,
                                json!({
                                    "kind": kind,
                                    "payload": entry.get("payload").cloned().unwrap_or(Value::Null),
                                    "node": current,
                                    "step": step,
                                }),
                            )
                            .await;
                        self.record_callback_warning("milestone", r);
                    }
                }
                // Interpolate `assign` HERE (not in commit_step) so an
                // interpolation failure becomes a node error that obeys
                // on_error — never a suppressed error that merges the raw
                // `${...}` template into graph state.
                let assign = match &node.assign {
                    Some(assign_tpl) => {
                        let assign_ctx = WalkContext {
                            state: state.clone(),
                            inputs: inputs.clone(),
                            result: Some(val.clone()),
                            execution: Some(execution.clone()),
                            graph_run_id: Some(graph_run_id.to_string()),
                        };
                        match ryeos_runtime::interpolate(assign_tpl, &assign_ctx.as_context()) {
                            Ok(interpolated) => Some(interpolated),
                            Err(e) => {
                                // The child SUCCEEDED (and may have spent
                                // tokens); only graph post-processing failed.
                                // Carry the cost so it is still accounted.
                                return StepOutcome::LeafSoftError {
                                    item_id: dispatched_item_id,
                                    error: format!(
                                        "interpolation error in `assign` for node `{current}`: {e:#}"
                                    ),
                                    next_on_error: resolve_next_on_error(node, cfg),
                                    elapsed_ms: elapsed,
                                    cost,
                                };
                            }
                        }
                    }
                    None => None,
                };
                let next =
                    edges::evaluate_next_with_result(node, state, inputs, &val, Some(&execution));
                StepOutcome::ActionOk {
                    item_id: dispatched_item_id,
                    result: val,
                    assign,
                    next,
                    cache_hit,
                    elapsed_ms: elapsed,
                    cost,
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_follow_fanout(
        &self,
        node: &GraphNode,
        current: &str,
        cfg: &GraphConfig,
        step: u32,
        state: &Value,
        inputs: &Value,
        execution: &Value,
        graph_run_id: &str,
        start: Instant,
    ) -> StepOutcome {
        // Consume first. An armed successor must never repeat env preflight or
        // interpolate/dispatch the child action.
        let resumed = self.take_follow_result(current);
        let base_ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
            execution: Some(execution.clone()),
            graph_run_id: Some(graph_run_id.to_string()),
        };
        let over = match ryeos_runtime::interpolate(
            &Value::String(node.over.as_deref().unwrap_or_default().to_string()),
            &base_ctx.as_context(),
        ) {
            Ok(Value::Array(items)) => items,
            Ok(other) => {
                return StepOutcome::DispatchHardError {
                    item_id: None,
                    error: format!("follow fanout node `{current}` over must resolve to array, got {other}"),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                }
            }
            Err(e) => {
                return StepOutcome::DispatchHardError {
                    item_id: None,
                    error: format!("interpolation error in `over` for node `{current}`: {e:#}"),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                }
            }
        };
        let var = node.foreach_var().to_string();
        let raw_item_id = node.action.as_ref().and_then(|a| a.get("item_id"))
            .and_then(Value::as_str).unwrap_or("").to_string();

        if let Some(wrapper) = resumed {
            let Some(envelopes) = wrapper.get("items").and_then(Value::as_array) else {
                return StepOutcome::LeafSoftError {
                    item_id: raw_item_id,
                    error: format!("follow fanout node `{current}` resumed without fanout items wrapper"),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    cost: None,
                };
            };
            let mut results = vec![Value::Null; envelopes.len()];
            let mut statuses = Vec::with_capacity(envelopes.len());
            let mut errors = Vec::new();
            let mut delta = Value::Object(serde_json::Map::new());
            let mut total_cost: Option<RuntimeCost> = None;
            for (index, envelope) in envelopes.iter().cloned().enumerate() {
                match dispatch::classify_envelope(envelope) {
                    dispatch::ActionOutcome::Success(success) => {
                        statuses.push("completed".to_string());
                        results[index] = success.result.clone();
                        add_runtime_cost(&mut total_cost, success.cost);
                        if let (Some(assign), Some(item)) = (&node.assign, over.get(index)) {
                            let assign_ctx = WalkContext {
                                state: state.clone(), inputs: inputs.clone(),
                                result: Some(success.result), execution: Some(execution.clone()),
                                graph_run_id: Some(graph_run_id.to_string()),
                            }.with_foreach_item(&var, item);
                            match ryeos_runtime::interpolate(assign, &assign_ctx) {
                                Ok(value) => merge_into(&mut delta, &value),
                                Err(e) => errors.push(ErrorRecord { step, node: current.to_string(), error: format!("follow item {index} assign interpolation failed: {e:#}") }),
                            }
                        }
                    }
                    dispatch::ActionOutcome::Failure(failure) => {
                        statuses.push("failed".to_string());
                        add_runtime_cost(&mut total_cost, failure.cost);
                        errors.push(ErrorRecord { step, node: current.to_string(), error: format!("follow item {index} failed: {}", failure.diagnostic) });
                    }
                }
            }
            let next = if errors.is_empty() {
                edges::evaluate_next_with_result(node, state, inputs, &Value::Array(results.clone()), Some(execution))
            } else { None };
            return StepOutcome::FollowFanoutDone {
                results, statuses, errors, assign_delta: delta,
                collect_key: node.collect.clone(), var_name: var, item_id: raw_item_id,
                next, next_on_error: resolve_next_on_error(node, cfg), cost: total_cost,
                elapsed_ms: start.elapsed().as_millis() as u64,
            };
        }

        if let Err(env_err) = env_preflight::check_env_requires(&cfg.env_requires, &node.env_requires) {
            return StepOutcome::DispatchHardError {
                item_id: Some(raw_item_id), error: format!("env preflight failed: {env_err}"),
                next_on_error: resolve_next_on_error(node, cfg),
                elapsed_ms: start.elapsed().as_millis() as u64, cost: None,
            };
        }
        if over.is_empty() {
            return StepOutcome::FollowFanoutDone {
                results: vec![], statuses: vec![], errors: vec![],
                assign_delta: Value::Object(serde_json::Map::new()), collect_key: node.collect.clone(),
                var_name: var, item_id: raw_item_id,
                next: edges::evaluate_next_with_result(node, state, inputs, &Value::Array(vec![]), Some(execution)),
                next_on_error: resolve_next_on_error(node, cfg), cost: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
            };
        }
        let mut children = Vec::with_capacity(over.len());
        for item in &over {
            let item_ctx = base_ctx.with_foreach_item(&var, item);
            let action = match ryeos_runtime::interpolate_action(node.action.as_ref().unwrap(), &item_ctx) {
                Ok(v) => strip_none_values(&v),
                Err(e) => return StepOutcome::DispatchHardError {
                    item_id: Some(raw_item_id), error: format!("follow fanout action interpolation failed: {e:#}"),
                    next_on_error: resolve_next_on_error(node, cfg), elapsed_ms: start.elapsed().as_millis() as u64, cost: None,
                },
            };
            let facets = match &node.facets {
                Some(value) => match ryeos_runtime::interpolate(value, &item_ctx) {
                    Ok(v) => Some(v),
                    Err(e) => return StepOutcome::DispatchHardError {
                        item_id: Some(raw_item_id), error: format!("follow fanout facets interpolation failed: {e:#}"),
                        next_on_error: resolve_next_on_error(node, cfg), elapsed_ms: start.elapsed().as_millis() as u64, cost: None,
                    },
                },
                None => None,
            };
            children.push(ryeos_runtime::callback::FollowChildSpec {
                item_ref: action.get("item_id").and_then(Value::as_str).unwrap_or("").to_string(),
                parameters: action.get("params").cloned().unwrap_or_else(|| json!({})),
                facets,
            });
        }
        StepOutcome::FollowFanoutSuspend { children, width: node.max_concurrency.map(|v| v as u32) }
    }

    /// D13: The ONLY function in walker.rs allowed to:
    ///   - emit step lifecycle events
    ///   - write a node receipt
    ///   - write a checkpoint
    ///   - emit `GraphCompleted` on terminal
    ///   - finalize the thread on terminal
    ///
    /// `commit_step` MUST be called exactly once per loop iteration.
    async fn commit_step(&self, input: CommitStepInput<'_>) -> CommitResult {
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
                    .write_follow_checkpoint(graph_run_id, current, step, state, suppressed_errors)
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
                        CommitResult::Terminate(GraphResult {
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
                        })
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
            StepOutcome::FollowFanoutSuspend { children, width } => {
                self.emit_graph_step_started(graph_run_id, step, current).await;
                self.emit_graph_follow_suspended(
                    graph_run_id, step, current, "cohort", Some(children.len()),
                ).await;
                if let Err(e) = self.write_follow_checkpoint(
                    graph_run_id, current, step, state, suppressed_errors,
                ).await {
                    let msg = format!("follow checkpoint write failed: {e}");
                    return self.commit_terminal(CommitTerminalInput {
                        graph_run_id, steps: step, state, suppressed_errors,
                        base_status: "error", error: Some(&msg), guard,
                        current_node_id: current, inputs, execution,
                    }).await;
                }
                match self.client.spawn_follow_children(
                    graph_run_id, current, step as i64, children, width, None,
                ).await {
                    Ok(_) => {
                        guard.finalized = true;
                        let acc = self.accounting.lock().unwrap();
                        CommitResult::Terminate(GraphResult {
                            success: false, graph_id: self.graph.graph_id.clone(),
                            definition_ref: self.graph.definition_ref.clone(),
                            definition_hash: self.graph.definition_hash.clone(),
                            graph_run_id: graph_run_id.to_string(), status: "continued".into(),
                            steps: step, state: state.clone(), result: None,
                            errors_suppressed: (!suppressed_errors.is_empty()).then_some(suppressed_errors.len()),
                            errors: (!suppressed_errors.is_empty()).then_some(suppressed_errors.clone()),
                            error: None, cost: acc.total.clone(), node_costs: acc.nodes.clone(),
                        })
                    }
                    Err(e) => {
                        let msg = format!("follow cohort handoff failed: {e}");
                        self.commit_terminal(CommitTerminalInput {
                            graph_run_id, steps: step, state, suppressed_errors,
                            base_status: "error", error: Some(&msg), guard,
                            current_node_id: current, inputs, execution,
                        }).await
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
                results, statuses, errors, assign_delta, collect_key, var_name,
                item_id, next, next_on_error, cost, elapsed_ms,
            } => {
                self.emit_graph_step_started(graph_run_id, step, current).await;
                if let Some(key) = collect_key {
                    if let Some(obj) = state.as_object_mut() {
                        obj.insert(key, Value::Array(results.clone()));
                    }
                }
                merge_into(state, &assign_delta);
                if let Some(obj) = state.as_object_mut() { obj.remove(&var_name); }
                if let Some(c) = &cost {
                    self.accounting.lock().unwrap().record(current, step, &item_id, c.clone());
                }
                let diagnostic = (!errors.is_empty()).then(|| errors.iter()
                    .map(|e| e.error.as_str()).collect::<Vec<_>>().join("; "));
                receipts.push(NodeReceipt {
                    node: current.to_string(), step,
                    definition_ref: self.graph.definition_ref.clone(),
                    definition_hash: self.graph.definition_hash.clone(),
                    result_hash: Some(hash_json_value(&json!({"results": results, "statuses": statuses}))),
                    cache_hit: false, elapsed_ms, error: diagnostic.clone(), cost: cost.clone(),
                });
                self.write_node_receipt_or_warn(graph_run_id, receipts.last().unwrap()).await;
                let status = if errors.is_empty() { "ok" } else { "error" };
                self.emit_graph_step_completed(graph_run_id, step, current, status, diagnostic.as_deref()).await;
                self.fire_graph_hooks("graph_step_completed",
                    self.step_hook_context(graph_run_id, current, step, status, diagnostic.as_deref(), state)).await;

                let target = if errors.is_empty() { next } else {
                    match next_on_error {
                        NextOnError::Redirect(target) => Some(target),
                        NextOnError::PolicyContinue => {
                            suppressed_errors.extend(errors);
                            edges::evaluate_next(self.graph.config.nodes.get(current).unwrap(), state, inputs, Some(execution))
                        }
                        NextOnError::PolicyFail => {
                            return self.commit_terminal(CommitTerminalInput {
                                graph_run_id, steps: step, state, suppressed_errors,
                                base_status: "error", error: diagnostic.as_deref(), guard,
                                current_node_id: current, inputs, execution,
                            }).await;
                        }
                    }
                };
                match target {
                    Some(target) => self.write_checkpoint_or_error(
                        graph_run_id, &target, step + 1, state, suppressed_errors, guard, 0,
                    ).await,
                    None => self.commit_terminal(CommitTerminalInput {
                        graph_run_id, steps: step + 1, state, suppressed_errors,
                        base_status: "completed", error: None, guard,
                        current_node_id: current, inputs, execution,
                    }).await,
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
    async fn commit_terminal(&self, input: CommitTerminalInput<'_>) -> CommitResult {
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

        CommitResult::Terminate(graph_result)
    }

    /// Write a checkpoint pointing at the next node. If the checkpoint
    /// write fails, terminates with an error.
    async fn write_checkpoint_or_error(
        &self,
        graph_run_id: &str,
        next_node: &str,
        next_step: u32,
        state: &Value,
        suppressed_errors: &[ErrorRecord],
        guard: &mut RunGuard,
        next_retry_attempt: u32,
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
            // Checkpoint failure is a hard error — resume correctness
            // is contractual (R4). Still report cost spent before the
            // failure so a partial run is accounted for.
            let (agg_cost, node_costs) = {
                let acc = self.accounting.lock().unwrap();
                (acc.total.clone(), acc.nodes.clone())
            };
            let graph_result = GraphResult {
                success: false,
                graph_id: self.graph.graph_id.clone(),
                definition_ref: self.graph.definition_ref.clone(),
                definition_hash: self.graph.definition_hash.clone(),
                graph_run_id: graph_run_id.to_string(),
                status: "error".into(),
                steps: next_step,
                state: state.clone(),
                result: None,
                errors_suppressed: None,
                errors: None,
                error: Some(format!("checkpoint write failed: {e}")),
                cost: agg_cost,
                node_costs,
            };

            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphCompleted,
                    json!({
                        "graph_id": &self.graph.graph_id,
                        "definition_ref": &self.graph.definition_ref,
                        "definition_hash": &self.graph.definition_hash,
                        "graph_run_id": graph_run_id,
                        "status": "error",
                        "steps": next_step,
                    }),
                )
                .await;
            self.record_callback_warning("graph_completed", r);

            let completion = TerminalCompletion {
                status: "failed".to_string(),
                outcome_code: Some("failed".to_string()),
                result: None,
                error: graph_result.error.as_ref().map(|e| json!(e)),
                cost: graph_result
                    .cost
                    .as_ref()
                    .and_then(|c| serde_json::to_value(c).ok()),
                outputs: Value::Null,
                warnings: self.warnings.lock().unwrap().clone(),
            };
            let r = self.client.finalize_thread(completion).await;
            self.record_callback_warning("finalize_thread", r.map(|_| ()));
            guard.finalized = true;

            return CommitResult::Terminate(graph_result);
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
    /// enforced, cost accrued, braid-visible). A failing hook is recorded as a
    /// warning, never a graph failure — graph hooks are fire-and-forget
    /// observers; they cannot steer the walk.
    async fn fire_graph_hooks(&self, event: &str, context: Value) {
        if let Err(e) = crate::hooks::run_graph_hooks(
            &self.client,
            &self.thread_id,
            &self.project_path,
            &self.graph.config.hooks,
            event,
            &context,
        )
        .await
        {
            self.warnings
                .lock()
                .unwrap()
                .push(format!("graph hook `{event}` failed: {e}"));
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
        status: &str,
        error: Option<&str>,
        state: &Value,
    ) -> Value {
        let mut ctx = json!({
            "event": "graph_step_completed",
            "graph_id": &self.graph.graph_id,
            "graph_run_id": graph_run_id,
            "node": node,
            "step": step,
            "status": status,
            "state": state,
        });
        if let Some(err) = error {
            ctx["error"] = json!(err);
        }
        ctx
    }

    // ── Event/receipt emission helpers (all route through record_callback_warning) ──

    async fn write_node_receipt_or_warn(&self, graph_run_id: &str, receipt: &NodeReceipt) {
        let r = persistence::write_node_receipt(&self.client, graph_run_id, receipt).await;
        self.record_callback_warning("write_node_receipt", r.map(|_| ()))
    }

    async fn emit_graph_step_started(&self, graph_run_id: &str, step: u32, current: &str) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::GraphStepStarted,
                json!({
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                }),
            )
            .await;
        self.record_callback_warning("graph_step_started", r);
    }

    async fn emit_graph_follow_suspended(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
        expected: Option<usize>,
    ) {
        let mut payload = json!({
            "graph_run_id": graph_run_id,
            "definition_ref": &self.graph.definition_ref,
            "definition_hash": &self.graph.definition_hash,
            "node": current,
            "node_ref": node_ref(&self.graph.definition_ref, current),
            "step": step,
            "item_id": item_id,
        });
        if let Some(expected) = expected { payload["expected"] = json!(expected); }
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::GraphFollowSuspended,
                payload,
            )
            .await;
        self.record_callback_warning("graph_follow_suspended", r);
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_graph_node_retry(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
        failed_attempt: u32,
        total_attempts: u32,
        delay_ms: u64,
        error: &str,
    ) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::GraphNodeRetry,
                json!({
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                    "item_id": item_id,
                    "attempt": failed_attempt,
                    "attempts": total_attempts,
                    "delay_ms": delay_ms,
                    "error": error,
                }),
            )
            .await;
        self.record_callback_warning("graph_node_retry", r);
    }

    async fn emit_tool_call_start(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
    ) {
        // `tool` + `call_id` are the shared tool-event contract every
        // producer satisfies (the timeline projection pairs on `call_id`);
        // the graph coordinates stay as additive context. The call id is
        // deterministic from the walk coordinates — one dispatch per
        // (run, step, node).
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::ToolCallStart,
                json!({
                    "tool": item_id,
                    "call_id": graph_call_id(graph_run_id, step, current),
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                    "item_id": item_id,
                }),
            )
            .await;
        self.record_callback_warning("tool_call_start", r);
    }

    async fn emit_tool_call_result(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
        status: &str,
    ) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::ToolCallResult,
                json!({
                    "tool": item_id,
                    "call_id": graph_call_id(graph_run_id, step, current),
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                    "item_id": item_id,
                    "status": status,
                }),
            )
            .await;
        self.record_callback_warning("tool_call_result", r);
    }

    async fn emit_graph_step_completed(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        status: &str,
        error: Option<&str>,
    ) {
        let mut payload = json!({
            "graph_run_id": graph_run_id,
            "definition_ref": &self.graph.definition_ref,
            "definition_hash": &self.graph.definition_hash,
            "node": current,
            "node_ref": node_ref(&self.graph.definition_ref, current),
            "step": step,
            "status": status,
        });
        if let Some(err) = error {
            payload["error"] = json!(err);
        }
        let r = self
            .client
            .append_runtime_event(RuntimeEventType::GraphStepCompleted, payload)
            .await;
        self.record_callback_warning("graph_step_completed", r);
    }

    async fn emit_graph_branch_taken(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        target: Option<&str>,
    ) {
        if let Some(t) = target {
            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphBranchTaken,
                    json!({
                        "graph_run_id": graph_run_id,
                        "definition_ref": &self.graph.definition_ref,
                        "definition_hash": &self.graph.definition_hash,
                        "node": current,
                        "node_ref": node_ref(&self.graph.definition_ref, current),
                        "step": step,
                        "target": t,
                        "target_node_ref": node_ref(&self.graph.definition_ref, t),
                    }),
                )
                .await;
            self.record_callback_warning("graph_branch_taken", r);
        }
    }

    /// Write a local checkpoint using the daemon-provided CheckpointWriter.
    ///
    /// Persists the versioned payload: cursor/step/state plus snapshots of the
    /// `GraphAccounting` aggregate and `suppressed_errors`, so a resumed run
    /// reconstructs cost and non-fatal error history from before the checkpoint
    /// (both restored in `execute` from `resume_state`).
    /// Write a checkpoint marking a follow suspend: `current_node` is the follow
    /// node ITSELF (so re-entry re-drives the suspend idempotently by follow_key),
    /// and a `pending_follow` marker carries LOCAL facts only (no child IDs) so the
    /// resume path consumes the stored child result instead of re-dispatching.
    async fn write_follow_checkpoint(
        &self,
        graph_run_id: &str,
        follow_node: &str,
        step: u32,
        state: &Value,
        suppressed_errors: &[ErrorRecord],
    ) -> anyhow::Result<()> {
        let Some(writer) = &self.checkpoint else {
            return Ok(());
        };
        let accounting = {
            let acc = self.accounting.lock().unwrap();
            serde_json::to_value(&*acc).unwrap_or(Value::Null)
        };
        let mut pending = serde_json::Map::new();
        pending.insert(follow_keys::FOLLOW_NODE.to_string(), json!(follow_node));
        pending.insert("step_count".to_string(), json!(step));
        pending.insert("graph_run_id".to_string(), json!(graph_run_id));
        let mut payload = json!({
            "schema_version": GRAPH_CHECKPOINT_SCHEMA_VERSION,
            "graph_run_id": graph_run_id,
            "current_node": follow_node,
            "step_count": step,
            "state": state,
            "accounting": accounting,
            "suppressed_errors": suppressed_errors,
            // A follow node never carries retry (validation excludes the pair),
            // so a follow suspend always checkpoints a zero attempt count.
            "retry_attempt": 0,
            "written_at": lillux::time::iso8601_now(),
        });
        payload[follow_keys::PENDING_FOLLOW] = Value::Object(pending);
        writer.write(&payload)?;
        Ok(())
    }

    async fn write_checkpoint(
        &self,
        graph_run_id: &str,
        next_node: &str,
        next_step: u32,
        state: &Value,
        suppressed_errors: &[ErrorRecord],
        retry_attempt: u32,
    ) -> anyhow::Result<()> {
        let Some(writer) = &self.checkpoint else {
            return Ok(());
        };

        // Accounting is interior-mutable on the walker; snapshot it under the
        // lock (no await) so resume restores accumulated cost instead of
        // restarting it at zero and under-billing the pre-checkpoint work.
        let accounting = {
            let acc = self.accounting.lock().unwrap();
            serde_json::to_value(&*acc).unwrap_or(Value::Null)
        };
        writer.write(&json!({
            "schema_version": GRAPH_CHECKPOINT_SCHEMA_VERSION,
            "graph_run_id": graph_run_id,
            "current_node": next_node,
            "step_count": next_step,
            "state": state,
            "accounting": accounting,
            "suppressed_errors": suppressed_errors,
            // Per-step retry counter for `next_node`: non-zero only when the
            // walker is re-entering the SAME node under a `retry` backoff, so a
            // segment cut or crash mid-retry resumes with the count intact.
            "retry_attempt": retry_attempt,
            "written_at": lillux::time::iso8601_now(),
        }))?;

        // Test-only crash-injection hook (prod-inert: fires ONLY when
        // `RYEOS_GRAPH_TEST_BLOCK_AFTER_CHECKPOINT` is set, which only the
        // graph crash-recovery e2e sets — and that name only reaches this
        // process because the daemon env allowlist lets it through). Once the
        // checkpoint for `next_node` is durably written, park forever so a
        // harness can SIGKILL the daemon with this thread's row still
        // `running`, kill this orphaned process group, and then exercise the
        // daemon's startup-reconcile native-resume path. The resumed launch
        // injects `RYEOS_RESUME=1` (`is_resume()`), so this hook never fires on
        // the resume pass: the walker proceeds from the checkpoint cursor to
        // completion. Gated on `next_node` (the resume cursor), so the test
        // names the node the graph should resume *into*.
        if !CheckpointWriter::is_resume()
            && std::env::var("RYEOS_GRAPH_TEST_BLOCK_AFTER_CHECKPOINT")
                .ok()
                .as_deref()
                == Some(next_node)
        {
            // Park forever without depending on the tokio `time` feature:
            // a never-resolving future suspends this task until the process
            // is killed by the harness.
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

/// Whether a node whose current dispatch just failed has retry attempts left.
///
/// `retry_attempt` is the number of attempts already spent BEFORE this one, so
/// the attempt that just failed is `retry_attempt + 1`. Returns that 1-based
/// failed-attempt number when a further attempt is allowed under the node's
/// `retry.attempts` (the total, incl. the first), and `None` when the policy is
/// absent or exhausted (route through `on_error`).
fn retry_attempts_remaining(node: &GraphNode, retry_attempt: u32) -> Option<u32> {
    let rc = node.retry.as_ref()?;
    let failed_attempt = retry_attempt + 1;
    (failed_attempt < rc.attempts).then_some(failed_attempt)
}

/// Resolve what to do on error based on node-level `on_error` and
/// graph-level `on_error` mode.
fn resolve_next_on_error(node: &GraphNode, cfg: &GraphConfig) -> NextOnError {
    if let Some(ref target) = node.on_error {
        NextOnError::Redirect(target.clone())
    } else {
        match cfg.on_error {
            ErrorMode::Continue => NextOnError::PolicyContinue,
            ErrorMode::Fail => NextOnError::PolicyFail,
        }
    }
}

/// Build one combined diagnostic for a foreach node whose per-item
/// failures trip a fail/redirect policy. Leads with the count and the
/// first item's error (which carries the leaf stderr excerpt).
fn foreach_failure_summary(node: &str, errors: &[ErrorRecord]) -> String {
    let first = errors
        .first()
        .map(|e| e.error.as_str())
        .unwrap_or("unknown error");
    format!(
        "foreach node `{node}` failed: {} of its iterations errored; first: {first}",
        errors.len()
    )
}

fn merge_into(target: &mut Value, source: &Value) {
    if let (Value::Object(ref mut t_map), Value::Object(ref s_map)) = (target, source) {
        for (k, v) in s_map {
            t_map.insert(k.clone(), v.clone());
        }
    }
}

fn strip_none_values(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let cleaned: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_none_values(v)))
                .collect();
            Value::Object(cleaned)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(strip_none_values).collect()),
        other => other.clone(),
    }
}

fn node_ref(definition_ref: &str, node: &str) -> String {
    format!("{definition_ref}#node:{node}")
}

/// Deterministic call id for a node's action dispatch, satisfying the shared
/// tool-event contract (`tool` + `call_id`) so start/result pair without
/// producer-specific knowledge. One dispatch per (run, step, node) makes the
/// coordinates a natural identity; a retried node re-dispatches under a new
/// step, so attempts pair independently.
fn graph_call_id(graph_run_id: &str, step: u32, node: &str) -> String {
    format!("{graph_run_id}:{step}:{node}")
}

fn hash_json_value(value: &Value) -> String {
    let canonical = lillux::cas::canonical_json(value);
    lillux::cas::sha256_hex(canonical.as_bytes())
}

fn compute_cache_key(graph_id: &str, node_name: &str, action: &Value) -> String {
    // Intentionally preserve existing cache-key serialization behavior.
    // `node_result_hash` is portable consequence identity; this cache key is
    // private runtime cache identity and should not change in this PR.
    let mut hasher = Sha256::new();
    hasher.update(graph_id.as_bytes());
    hasher.update(node_name.as_bytes());
    hasher.update(serde_json::to_string(action).unwrap_or_default().as_bytes());
    lillux::cas::sha256_hex(&hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest};
    use std::sync::{Arc, Mutex};

    struct MockClient {
        results: Mutex<Vec<Value>>,
        /// Commands handed back on the FIRST `claim_commands`, then drained empty.
        pending_commands: Mutex<Vec<Value>>,
        /// Recorded `(command_id, status)` for every `complete_command`.
        completed: Mutex<Vec<(i64, String)>>,
        /// Status carried by the terminal `finalize_thread`, if any.
        finalized_status: Mutex<Option<String>>,
    }

    impl MockClient {
        fn new(results: Vec<Value>) -> Self {
            Self {
                results: Mutex::new(results),
                pending_commands: Mutex::new(Vec::new()),
                completed: Mutex::new(Vec::new()),
                finalized_status: Mutex::new(None),
            }
        }

        fn with_pending_commands(results: Vec<Value>, commands: Vec<Value>) -> Self {
            let mock = Self::new(results);
            *mock.pending_commands.lock().unwrap() = commands;
            mock
        }
    }

    #[async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for MockClient {
        async fn dispatch_action(
            &self,
            _request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            let mut results = self.results.lock().unwrap();
            // Strict typed contract: CallbackClient::dispatch_action
            // requires `{thread, result}` shape; preserve any caller-
            // supplied leaf by wrapping it under `result`.
            if results.is_empty() {
                Ok(json!({"thread": {}, "result": {}}))
            } else {
                Ok(json!({"thread": {}, "result": results.remove(0)}))
            }
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            completion: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            *self.finalized_status.lock().unwrap() = Some(completion.status.clone());
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn request_continuation(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            let commands = std::mem::take(&mut *self.pending_commands.lock().unwrap());
            Ok(json!({ "commands": commands }))
        }
        async fn complete_command(
            &self,
            _: &str,
            command_id: i64,
            status: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            self.completed
                .lock()
                .unwrap()
                .push((command_id, status.to_string()));
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn spawn_follow_child(
            &self,
            _request: ryeos_runtime::callback::SpawnFollowChildRequest,
        ) -> Result<Value, CallbackError> {
            // Simulate the daemon accepting the follow handoff (it would settle
            // this thread `continued` server-side).
            Ok(json!({ "phase": "waiting" }))
        }
    }

    fn make_callback(results: Vec<Value>) -> CallbackClient {
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            Arc::new(MockClient::new(results));
        CallbackClient::from_inner(inner, "thread-test", "/tmp/test-project", "tat-test")
    }

    fn make_graph(yaml: &str) -> GraphDefinition {
        GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap()
    }

    fn make_walker(graph: GraphDefinition, results: Vec<Value>) -> Walker {
        Walker::new(
            graph,
            "/tmp/test-project".to_string(),
            "thread-test".to_string(),
            make_callback(results),
            None,
        )
    }

    fn make_test_node() -> GraphNode {
        GraphNode {
            node_type: NodeType::Action,
            action: None,
            assign: None,
            next: None,
            on_error: None,
            cache_result: false,
            cache: false,
            follow: false,
            detach: false,
            facets: None,
            over: None,
            r#as: None,
            collect: None,
            parallel: false,
            max_concurrency: None,
            output: None,
            env_requires: Vec::new(),
            retry: None,
        }
    }

    fn make_test_graph_config() -> GraphConfig {
        GraphConfig {
            start: "x".to_string(),
            max_steps: 100,
            on_error: ErrorMode::Fail,
            nodes: HashMap::new(),
            hooks: Vec::new(),
            config_schema: None,
            env_requires: Vec::new(),
            state: None,
            max_concurrency: None,
            segment_steps: None,
        }
    }

    #[tokio::test]
    async fn simple_action_to_return() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      assign: {echo_result: "${result}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![json!({"msg": "hello"})]);
        let result = w.execute(json!({}), None).await;
        assert!(result.success);
        assert_eq!(result.status, "completed");
        assert_eq!(result.steps, 1);
    }

    /// A cancel queued before the first node runs is drained between nodes: the
    /// walker acks it `completed`, settles the run/thread `cancelled`, and never
    /// executes the node.
    #[tokio::test]
    async fn cooperative_cancel_settles_cancelled_and_acks_command() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let mock = Arc::new(MockClient::with_pending_commands(
            vec![json!({"msg": "hello"})],
            vec![json!({"command_id": 7, "command_type": "cancel"})],
        ));
        let client = CallbackClient::from_inner(
            mock.clone(),
            "thread-test",
            "/tmp/test-project",
            "tat-test",
        );
        let w = Walker::new(
            graph,
            "/tmp/test-project".to_string(),
            "thread-test".to_string(),
            client,
            None,
        );
        let result = w.execute(json!({}), None).await;

        assert!(!result.success);
        assert_eq!(result.status, "cancelled");
        // Terminated before running step1.
        assert_eq!(result.steps, 0);
        // The cancel was acknowledged completed…
        assert_eq!(
            *mock.completed.lock().unwrap(),
            vec![(7, "completed".to_string())]
        );
        // …and the thread finalized cancelled, not failed.
        assert_eq!(
            mock.finalized_status.lock().unwrap().as_deref(),
            Some("cancelled")
        );
    }

    /// When cancel and kill queue in the same drained batch, kill (the harder
    /// stop) wins the terminal status, and BOTH commands are still acked so
    /// neither hangs in `claimed`.
    #[tokio::test]
    async fn cooperative_kill_outranks_cancel_in_one_batch() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let mock = Arc::new(MockClient::with_pending_commands(
            vec![json!({"msg": "hello"})],
            vec![
                json!({"command_id": 1, "command_type": "cancel"}),
                json!({"command_id": 2, "command_type": "kill"}),
            ],
        ));
        let client = CallbackClient::from_inner(
            mock.clone(),
            "thread-test",
            "/tmp/test-project",
            "tat-test",
        );
        let w = Walker::new(
            graph,
            "/tmp/test-project".to_string(),
            "thread-test".to_string(),
            client,
            None,
        );
        let result = w.execute(json!({}), None).await;

        assert_eq!(result.status, "killed");
        assert_eq!(
            mock.finalized_status.lock().unwrap().as_deref(),
            Some("killed")
        );
        // Both commands acked completed, regardless of which won the terminal.
        let completed = mock.completed.lock().unwrap().clone();
        assert!(completed.contains(&(1, "completed".to_string())));
        assert!(completed.contains(&(2, "completed".to_string())));
    }

    /// A signal-driven cancel flag (SIGTERM) already set finalizes the run
    /// cancelled at the first node boundary, without executing a node — the same
    /// cooperative terminal a claimed cancel command produces, but with no
    /// command to settle.
    #[tokio::test]
    async fn signal_cancel_flag_settles_cancelled_between_nodes() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let mock = Arc::new(MockClient::new(vec![json!({"msg": "hello"})]));
        let client = CallbackClient::from_inner(
            mock.clone(),
            "thread-test",
            "/tmp/test-project",
            "tat-test",
        );
        // Flag pre-set, as if SIGTERM already arrived before the first node.
        let flag = Arc::new(AtomicBool::new(true));
        let w = Walker::new(
            graph,
            "/tmp/test-project".to_string(),
            "thread-test".to_string(),
            client,
            None,
        )
        .with_cancel_flag(flag);
        let result = w.execute(json!({}), None).await;

        assert_eq!(result.status, "cancelled");
        assert!(!result.success);
        assert_eq!(result.steps, 0);
        assert_eq!(
            mock.finalized_status.lock().unwrap().as_deref(),
            Some("cancelled")
        );
    }

    #[tokio::test]
    async fn follow_node_suspends_graph_as_continued() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w.execute(json!({}), None).await;
        // A follow node hands off to a detached child and suspends: the daemon
        // settled this thread `continued`, so the walker reports continued (not
        // completed), suspended at the follow node (step 0) with no result yet.
        assert_eq!(result.status, "continued");
        assert!(!result.success);
        assert_eq!(result.steps, 0);
        assert!(result.result.is_none());
    }

    /// Helper: assert no graph-state value contains an unresolved
    /// `${...}` template (the P0 state-corruption symptom).
    fn assert_no_raw_template(state: &Value) {
        let s = serde_json::to_string(state).unwrap();
        assert!(
            !s.contains("${"),
            "graph state must not carry unresolved templates, got: {s}"
        );
    }

    // ── Acceptance: a failing tool inside a graph produces ONE actionable
    //    error (node + exit + stderr) and never poisons state. ──────────

    #[tokio::test]
    async fn failing_tool_on_error_fail_surfaces_diagnostic() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      assign: {captured: "${result.value}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        // Failed subprocess envelope: result null, error carries stderr.
        let w = make_walker(
            graph,
            vec![json!({
                "outcome_code": "exit:1",
                "result": null,
                "error": {"exit_code": 1, "stderr": "Traceback: boom"}
            })],
        );
        let result = w.execute(json!({}), None).await;

        assert!(!result.success, "failed tool must fail the graph");
        assert_eq!(result.status, "error");
        let err = result.error.unwrap_or_default();
        assert!(err.contains("step1"), "error should name the node: {err}");
        assert!(err.contains("boom"), "error should carry stderr: {err}");
        // The poisoned-state symptom must be absent: no `${result...}`.
        assert_no_raw_template(&result.state);
    }

    #[tokio::test]
    async fn failing_tool_on_error_continue_records_structured_error() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: continue
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      assign: {captured: "${result.value}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(
            graph,
            vec![json!({
                "outcome_code": "exit:1",
                "result": null,
                "error": {"exit_code": 1, "stderr": "boom"}
            })],
        );
        let result = w.execute(json!({}), None).await;

        assert!(result.success);
        assert_eq!(result.status, "completed_with_errors");
        assert_eq!(result.errors_suppressed, Some(1));
        let errors = result.errors.unwrap();
        assert_eq!(errors[0].node, "step1");
        assert!(errors[0].error.contains("boom"), "got: {}", errors[0].error);
        // Assignment never ran against a `null`, so no raw template leaked.
        assert_no_raw_template(&result.state);
    }

    #[tokio::test]
    async fn bare_user_status_error_is_not_a_graph_failure() {
        // A tool that legitimately returns domain data shaped like
        // `{status: "error", message: ...}` with a CLEAN process exit is
        // NOT a graph failure — only the execution envelope decides.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/lookup"}
      assign: {outcome: "${result.status}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(
            graph,
            vec![json!({"status": "error", "message": "not found"})],
        );
        let result = w.execute(json!({}), None).await;

        assert!(result.success, "bare domain data must not fail the graph");
        assert_eq!(result.status, "completed");
        assert_eq!(
            result.state.get("outcome").and_then(|v| v.as_str()),
            Some("error")
        );
    }

    #[tokio::test]
    async fn assign_interpolation_failure_obeys_on_error() {
        // Tool succeeds, but `assign` references a missing field — the
        // interpolation failure is a node error (obeys on_error: fail),
        // NOT a suppressed error that merges the raw `${...}` into state.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
      assign: {captured: "${result.missing.deep}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![json!({"present": 1})]);
        let result = w.execute(json!({}), None).await;

        assert!(!result.success);
        assert_eq!(result.status, "error");
        assert!(result.error.unwrap_or_default().contains("assign"));
        assert_no_raw_template(&result.state);
    }

    #[tokio::test]
    async fn return_output_interpolation_failure_fails_run() {
        // A return node whose `output` template can't resolve must FAIL
        // the run rather than emit a raw `${...}` template as the result.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output: "${state.never_set}"
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w.execute(json!({}), None).await;

        assert!(!result.success);
        assert_eq!(result.status, "error");
        assert!(result.error.unwrap_or_default().contains("output"));
        assert!(result.result.is_none(), "no raw template as result");
    }

    #[tokio::test]
    async fn return_output_resolves_inputs() {
        // `${inputs.*}` must resolve in a return node's output (inputs are
        // threaded into the terminal interpolation context).
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output: "${inputs.game_id}"
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w
            .execute(json!({"inputs": {"game_id": "g-42"}}), None)
            .await;

        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(
            result.result.and_then(|v| v.as_str().map(String::from)),
            Some("g-42".to_string())
        );
    }

    #[tokio::test]
    async fn return_output_accepts_map_template() {
        // A map `output:` interpolates each leaf and yields a structured
        // result (not just a string).
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output:
        game_id: "${inputs.game_id}"
        nested:
          level: "${state.level}"
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w
            .execute(
                json!({"inputs": {"game_id": "g-7"}, "inject_state": {"level": "hard"}}),
                None,
            )
            .await;

        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(
            result.result,
            Some(json!({"game_id": "g-7", "nested": {"level": "hard"}}))
        );
    }

    #[tokio::test]
    async fn return_output_accepts_list_template() {
        // A list `output:` interpolates each element.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output:
        - "${inputs.a}"
        - "${state.b}"
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w
            .execute(
                json!({"inputs": {"a": "first"}, "inject_state": {"b": "second"}}),
                None,
            )
            .await;

        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(result.result, Some(json!(["first", "second"])));
    }

    #[tokio::test]
    async fn graph_exposes_directive_outputs_and_cost() {
        // P0/Phase A+C end-to-end: a directive node's declared `outputs`
        // reach graph state via `${result.outputs.X}`, and the directive's
        // reported cost lands in the aggregate + per-node accounting.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action:
        item_id: "directive:test/reason"
      assign:
        recommendations: "${result.outputs.recommendations}"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
      output: "${state.recommendations}"
"#;
        let graph = make_graph(yaml);
        // Native directive envelope: payload in `outputs`, cost reported.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": "directive_return",
            "outputs": {"recommendations": ["a", "b"]},
            "cost": {"input_tokens": 100, "output_tokens": 20, "total_usd": 0.001},
            "warnings": []
        });
        let w = make_walker(graph, vec![envelope]);
        let result = w.execute(json!({}), None).await;

        assert!(result.success, "got: {:?}", result.error);
        // A1: structured outputs flowed through assign into the result.
        assert_eq!(result.result, Some(json!(["a", "b"])));
        // C: aggregate + per-node cost recorded.
        let cost = result.cost.expect("graph cost should be populated");
        assert_eq!(cost.input_tokens, 100);
        assert_eq!(cost.output_tokens, 20);
        assert_eq!(result.node_costs.len(), 1);
        assert_eq!(result.node_costs[0].node, "reason");
        assert_eq!(result.node_costs[0].item_id, "directive:test/reason");
        assert_eq!(result.node_costs[0].cost.output_tokens, 20);
    }

    #[tokio::test]
    async fn graph_aggregates_cost_across_two_directive_nodes() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: first
  nodes:
    first:
      node_type: action
      action:
        item_id: "directive:test/a"
      next:
        type: unconditional
        to: second
    second:
      node_type: action
      action:
        item_id: "directive:test/b"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let env = |i: u64, o: u64, usd: f64| {
            json!({
                "success": true,
                "status": "completed",
                "result": "directive_return",
                "outputs": {"ok": true},
                "cost": {"input_tokens": i, "output_tokens": o, "total_usd": usd},
                "warnings": []
            })
        };
        let w = make_walker(graph, vec![env(10, 5, 0.001), env(30, 7, 0.002)]);
        let result = w.execute(json!({}), None).await;

        assert!(result.success, "got: {:?}", result.error);
        let cost = result.cost.expect("aggregate cost");
        assert_eq!(cost.input_tokens, 40);
        assert_eq!(cost.output_tokens, 12);
        assert!((cost.total_usd - 0.003).abs() < 1e-9);
        assert_eq!(result.node_costs.len(), 2);
    }

    #[tokio::test]
    async fn graph_without_child_cost_reports_no_cost() {
        // A subprocess tool leaf carries no envelope `cost` — the graph
        // must finalize `cost: None` rather than an all-zeros record.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: act
  nodes:
    act:
      node_type: action
      action:
        item_id: "tool:test/echo"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        // Subprocess envelope: clean exit, no cost field.
        let envelope = json!({
            "outcome_code": "exit:0",
            "result": {"ok": true},
            "error": null,
            "artifacts": []
        });
        let w = make_walker(graph, vec![envelope]);
        let result = w.execute(json!({}), None).await;

        assert!(result.success, "got: {:?}", result.error);
        assert!(result.cost.is_none(), "no child cost → no graph cost");
        assert!(result.node_costs.is_empty());
    }

    // ── Phase C: failure-path / foreach / reset / cache cost accounting ──

    fn native_envelope(success: bool, outputs: Value, cost: Option<(u64, u64, f64)>) -> Value {
        let mut env = json!({
            "success": success,
            "status": if success { "completed" } else { "error" },
            "result": if success { json!("directive_return") } else { json!({"error": "boom"}) },
            "outputs": outputs,
            "warnings": []
        });
        if let Some((i, o, usd)) = cost {
            env["cost"] = json!({"input_tokens": i, "output_tokens": o, "total_usd": usd});
        }
        env
    }

    #[tokio::test]
    async fn graph_failed_directive_child_reports_partial_cost() {
        // A directive that burns tokens then fails (success:false + cost)
        // must still surface its cost in the graph result and node_costs.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let env = native_envelope(false, Value::Null, Some((80, 0, 0.0008)));
        let w = make_walker(make_graph(yaml), vec![env]);
        let result = w.execute(json!({}), None).await;

        assert!(!result.success);
        let cost = result.cost.expect("failed child cost should be reported");
        assert_eq!(cost.input_tokens, 80);
        assert_eq!(result.node_costs.len(), 1);
        assert_eq!(result.node_costs[0].node, "reason");
    }

    #[tokio::test]
    async fn graph_cost_recorded_when_assign_fails_after_success() {
        // Child succeeds (with cost), but `assign` interpolation fails →
        // cost must still be accounted, not lost to the error path.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      assign: {x: "${result.outputs.missing}"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let env = native_envelope(
            true,
            json!({"recommendations": ["a"]}),
            Some((50, 10, 0.0005)),
        );
        let w = make_walker(make_graph(yaml), vec![env]);
        let result = w.execute(json!({}), None).await;

        assert!(!result.success, "assign failure should fail the run");
        let cost = result
            .cost
            .expect("cost from successful child must survive assign failure");
        assert_eq!(cost.input_tokens, 50);
    }

    #[tokio::test]
    async fn graph_on_error_continue_records_cost_of_failed_child() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  on_error: continue
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let env = native_envelope(false, Value::Null, Some((30, 0, 0.0003)));
        let w = make_walker(make_graph(yaml), vec![env]);
        let result = w.execute(json!({}), None).await;

        assert!(result.success, "continue policy keeps the run successful");
        assert_eq!(result.status, "completed_with_errors");
        assert_eq!(
            result
                .cost
                .expect("cost recorded under continue")
                .input_tokens,
            30
        );
    }

    #[tokio::test]
    async fn terminal_completion_and_runtime_carry_cost() {
        // The cost aggregate must reach TerminalCompletion.cost (the
        // callback wire), not just the in-process GraphResult.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let env = native_envelope(true, json!({"ok": true}), Some((100, 20, 0.001)));
        let (w, recorder) = make_recording_walker(make_graph(yaml), vec![env], None);
        let result = w.execute(json!({}), None).await;

        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(result.cost.as_ref().unwrap().input_tokens, 100);
        let costs = recorder.finalize_costs.lock().unwrap();
        let last = costs.last().expect("a finalize_thread call").clone();
        let cost = last.expect("TerminalCompletion.cost should be populated");
        assert_eq!(cost["input_tokens"], 100);
    }

    #[tokio::test]
    async fn foreach_aggregates_cost_including_failed_iteration_under_continue() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      action: {item_id: "directive:test/step"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        // iter 1 succeeds (cost 10), iter 2 fails (cost 5) → aggregate 15.
        let results = vec![
            native_envelope(true, json!({"ok": true}), Some((10, 0, 0.001))),
            native_envelope(false, Value::Null, Some((5, 0, 0.0005))),
        ];
        let w = make_walker(make_graph(yaml), results);
        let result = w
            .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
            .await;

        assert!(result.success);
        let cost = result.cost.expect("foreach aggregate cost");
        assert_eq!(cost.input_tokens, 15);
        assert_eq!(
            result.node_costs.len(),
            1,
            "foreach aggregates to one record"
        );
        assert_eq!(result.node_costs[0].item_id, "directive:test/step");
    }

    #[tokio::test]
    async fn parallel_foreach_aggregates_cost_across_iterations() {
        // Parallel path: cost aggregation must not depend on iteration
        // ordering. Sum is 15 regardless of which task drew which envelope.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      parallel: true
      action: {item_id: "directive:test/step"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let results = vec![
            native_envelope(true, json!({"ok": true}), Some((10, 0, 0.001))),
            native_envelope(false, Value::Null, Some((5, 0, 0.0005))),
        ];
        let w = make_walker(make_graph(yaml), results);
        let result = w
            .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
            .await;

        assert!(result.success);
        assert_eq!(result.cost.expect("parallel aggregate").input_tokens, 15);
    }

    #[tokio::test]
    async fn foreach_reports_already_spent_cost_under_fail_policy() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      action: {item_id: "directive:test/step"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let results = vec![
            native_envelope(true, json!({"ok": true}), Some((10, 0, 0.001))),
            native_envelope(false, Value::Null, Some((5, 0, 0.0005))),
        ];
        let w = make_walker(make_graph(yaml), results);
        let result = w
            .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
            .await;

        assert!(!result.success, "default fail policy aborts the foreach");
        let cost = result.cost.expect("already-spent foreach cost on failure");
        assert_eq!(cost.input_tokens, 15);
    }

    #[tokio::test]
    async fn cost_accounting_resets_between_executes() {
        // A Walker reused across execute() calls must not accumulate stale
        // cost — each run reports only its own.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let results = vec![
            native_envelope(true, json!({"ok": true}), Some((100, 20, 0.001))),
            native_envelope(true, json!({"ok": true}), Some((100, 20, 0.001))),
        ];
        let w = make_walker(make_graph(yaml), results);
        let r1 = w.execute(json!({}), None).await;
        let r2 = w.execute(json!({}), None).await;

        assert_eq!(r1.cost.unwrap().input_tokens, 100);
        assert_eq!(
            r2.cost.unwrap().input_tokens,
            100,
            "second run must not include first run's cost"
        );
    }

    #[tokio::test]
    async fn cost_bearing_cache_hit_does_not_rebill() {
        // First run dispatches and bills cost; a second run hits the
        // (disk-backed) cache, replays the stored result, and bills nothing.
        let cache_dir = std::env::temp_dir()
            .join("ryeos-graph-cache")
            .join("cache_rebill/test");
        let _ = std::fs::remove_dir_all(&cache_dir);

        let yaml = r#"
version: "1.0.0"
category: cache_rebill
config:
  start: reason
  nodes:
    reason:
      node_type: action
      cache: true
      action: {item_id: "directive:test/reason"}
      assign: {got: "${result.outputs.recommendations}"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
      output: "${state.got}"
"#;
        let env = native_envelope(
            true,
            json!({"recommendations": ["a", "b"]}),
            Some((100, 20, 0.001)),
        );
        let w1 = make_walker(make_graph(yaml), vec![env]);
        let r1 = w1.execute(json!({}), None).await;
        assert!(r1.success, "got: {:?}", r1.error);
        assert_eq!(r1.cost.expect("first run bills").input_tokens, 100);

        // Second run: NO dispatch result supplied — success proves the
        // cached result was replayed, and `cost: None` proves no re-bill.
        let w2 = make_walker(make_graph(yaml), vec![]);
        let r2 = w2.execute(json!({}), None).await;
        assert!(r2.success, "cache hit should replay; got: {:?}", r2.error);
        assert_eq!(r2.result, Some(json!(["a", "b"])), "cached result replayed");
        assert!(r2.cost.is_none(), "cache hit must not re-bill cost");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn config_state_seeds_initial_state() {
        // Authored `config.state` seeds graph state, so a foreach can run
        // off it with no caller `inject_state`.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  state:
    items: ["a", "b"]
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![json!({"v": "a"}), json!({"v": "b"})]);
        let result = w.execute(json!({}), None).await;

        assert!(result.success, "got: {:?}", result.error);
        let collected = result
            .state
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(collected.len(), 2);
    }

    #[tokio::test]
    async fn inject_state_overrides_config_state() {
        // Caller `inject_state` takes precedence over authored defaults.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  state:
    mode: "default"
  nodes:
    done:
      node_type: return
      output: "${state.mode}"
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w
            .execute(json!({"inject_state": {"mode": "override"}}), None)
            .await;

        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(result.result, Some(json!("override")));
    }

    #[tokio::test]
    async fn gate_node_conditional_routing() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      assign: {mode: fast}
      next:
        type: conditional
        branches:
          - when: {path: state.mode, op: eq, value: fast}
            to: fast_path
          - to: slow_path
    fast_path:
      node_type: return
    slow_path:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w.execute(json!({}), None).await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn max_steps_exceeded() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: loop
  max_steps: 3
  nodes:
    loop:
      action: {item_id: "tool:test/noop"}
      next:
        type: unconditional
        to: loop
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![json!({}), json!({}), json!({})]);
        let result = w.execute(json!({}), None).await;
        assert!(!result.success);
        assert_eq!(result.status, "max_steps_exceeded");
    }

    #[tokio::test]
    async fn segment_steps_cuts_machine_continuation() {
        // With segment_steps=1 the first step advances and the per-thread budget
        // is hit before a terminal node — the walker cuts a machine continuation
        // (request_continuation succeeds) and settles `continued` rather than
        // running on toward max_steps. The successor would resume from the
        // checkpoint the last commit_step wrote.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: loop
  max_steps: 100
  segment_steps: 1
  nodes:
    loop:
      action: {item_id: "tool:test/noop"}
      next:
        type: unconditional
        to: loop
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![json!({})]);
        let result = w.execute(json!({}), None).await;
        assert_eq!(result.status, "continued", "got: {result:?}");
        assert!(!result.success);
        assert_eq!(result.steps, 1, "one step ran before the segment cut");
    }

    #[test]
    fn validation_rejects_missing_start() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: nonexistent
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        let result = w.validate();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn foreach_sequential_collects_results() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(
            graph,
            vec![
                json!({"value": "a"}),
                json!({"value": "b"}),
                json!({"value": "c"}),
            ],
        );
        let result = w
            .execute(json!({"inject_state": {"items": ["a", "b", "c"]}}), None)
            .await;
        assert!(result.success);
        let results = result
            .state
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    fn foreach_graph_yaml(parallel: bool, on_error: &str) -> String {
        format!(
            r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: {on_error}
  nodes:
    iterate:
      node_type: foreach
      over: "${{state.items}}"
      as: "elem"
      parallel: {parallel}
      action: {{item_id: "tool:test/echo", params: {{value: "${{elem}}"}}}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#
        )
    }

    #[tokio::test]
    async fn foreach_item_failure_on_error_fail_fails_run() {
        // A failed subprocess inside a foreach with on_error: fail must
        // fail the whole run — not complete_with_errors.
        for parallel in [false, true] {
            let graph = make_graph(&foreach_graph_yaml(parallel, "fail"));
            let w = make_walker(
                graph,
                vec![
                    json!({"value": "a"}),
                    json!({
                        "outcome_code": "exit:1", "result": null,
                        "error": {"exit_code": 1, "stderr": "boom"}
                    }),
                ],
            );
            let result = w
                .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
                .await;
            assert!(!result.success, "parallel={parallel}: should fail");
            assert_eq!(result.status, "error", "parallel={parallel}");
            let err = result.error.unwrap_or_default();
            assert!(err.contains("boom"), "parallel={parallel}: got {err}");
            assert_no_raw_template(&result.state);
        }
    }

    #[tokio::test]
    async fn foreach_item_failure_on_error_continue_records_errors() {
        for parallel in [false, true] {
            let graph = make_graph(&foreach_graph_yaml(parallel, "continue"));
            let w = make_walker(
                graph,
                vec![
                    json!({"value": "a"}),
                    json!({
                        "outcome_code": "exit:1", "result": null,
                        "error": {"exit_code": 1, "stderr": "boom"}
                    }),
                ],
            );
            let result = w
                .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
                .await;
            assert!(result.success, "parallel={parallel}");
            assert_eq!(
                result.status, "completed_with_errors",
                "parallel={parallel}"
            );
            assert_eq!(result.errors_suppressed, Some(1), "parallel={parallel}");
            let errors = result.errors.unwrap();
            assert!(errors[0].error.contains("boom"), "parallel={parallel}");
            // collect aligns: [a-result, null]
            let collected = result
                .state
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap();
            assert_eq!(collected.len(), 2, "parallel={parallel}");
            assert_eq!(collected[1], Value::Null, "parallel={parallel}");
            assert_no_raw_template(&result.state);
        }
    }

    #[tokio::test]
    async fn foreach_parallel_interp_failure_not_dispatched() {
        // Parallel foreach whose action template can't resolve must NOT
        // dispatch a raw `${...}` — the item errors and yields a null.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      parallel: true
      action: {item_id: "tool:test/echo", params: {value: "${elem.missing.deep}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        // No mock results queued: if a raw template were dispatched the
        // mock would still answer, but the item must be a recorded error.
        let w = make_walker(graph, vec![]);
        let result = w
            .execute(json!({"inject_state": {"items": ["a"]}}), None)
            .await;
        assert!(result.success);
        assert_eq!(result.errors_suppressed, Some(1));
        assert!(result.errors.unwrap()[0].error.contains("interpolation"));
        assert_no_raw_template(&result.state);
    }

    #[tokio::test]
    async fn foreach_sequential_assign_persists_to_state() {
        // Foreach `assign` must reach the committed final state.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      assign: {last_value: "${result.value}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![json!({"value": "a"}), json!({"value": "b"})]);
        let result = w
            .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
            .await;
        assert!(result.success, "got: {:?}", result.error);
        assert_eq!(
            result.state.get("last_value").and_then(|v| v.as_str()),
            Some("b"),
            "foreach assign must persist (last item wins)"
        );
    }

    #[tokio::test]
    async fn foreach_assign_failure_is_consistent_seq_and_parallel() {
        // Action succeeds but `assign` references a missing field. Under
        // on_error: continue, sequential and parallel must behave
        // identically: the item is Null in collect and one error recorded.
        let yaml = |parallel: bool| {
            format!(
                r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${{state.items}}"
      as: "elem"
      parallel: {parallel}
      action: {{item_id: "tool:test/echo", params: {{value: "${{elem}}"}}}}
      assign: {{captured: "${{result.missing.deep}}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#
            )
        };
        let run = |parallel: bool| async move {
            let graph = make_graph(&yaml(parallel));
            let w = make_walker(graph, vec![json!({"value": "a"})]);
            w.execute(json!({"inject_state": {"items": ["a"]}}), None)
                .await
        };
        let seq = run(false).await;
        let par = run(true).await;

        for (label, result) in [("seq", &seq), ("par", &par)] {
            assert!(result.success, "{label}");
            assert_eq!(result.status, "completed_with_errors", "{label}");
            assert_eq!(result.errors_suppressed, Some(1), "{label}");
            let collected = result
                .state
                .get("results")
                .and_then(|v| v.as_array())
                .unwrap();
            assert_eq!(collected, &vec![Value::Null], "{label}: item must be Null");
        }
        // Both runners agree on collect and error count.
        assert_eq!(seq.state.get("results"), par.state.get("results"));
        assert_eq!(seq.errors_suppressed, par.errors_suppressed);
    }

    #[tokio::test]
    async fn foreach_item_failure_redirects_to_handler() {
        // A node-level `on_error: <handler>` redirects the whole foreach
        // to the handler node on item failure (no suppressed errors).
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: fail
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      on_error: handler
      next:
        type: unconditional
        to: done
    handler:
      node_type: return
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(
            graph,
            vec![json!({
                "outcome_code": "exit:1", "result": null,
                "error": {"exit_code": 1, "stderr": "boom"}
            })],
        );
        let result = w
            .execute(json!({"inject_state": {"items": ["a"]}}), None)
            .await;
        assert!(result.success, "redirect handler should complete the run");
        assert_eq!(result.status, "completed");
        assert_eq!(result.errors_suppressed, None);
    }

    #[tokio::test]
    async fn on_error_continue_mode() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: continue
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      next:
        type: unconditional
        to: step2
    step2:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(
            graph,
            vec![
                json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced failure"}}),
            ],
        );
        let result = w.execute(json!({}), None).await;
        assert!(result.success);
        assert_eq!(result.status, "completed_with_errors");
        assert_eq!(result.errors_suppressed, Some(1));
    }

    #[test]
    fn cache_result_hits_cache_on_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = NodeCache {
            cache_dir: tmp.path().join("cache-test-unique-sequential"),
        };
        let action = json!({"item_id": "tool:test/echo"});
        let key = compute_cache_key("cache-test-unique-sequential", "step1", &action);

        assert!(cache.lookup(&key).is_none());

        let val = json!({"msg": "cached"});
        cache.store(&key, &val);
        let cached = cache.lookup(&key).unwrap();
        assert_eq!(cached, val);
    }

    // ── warning accumulator ─────────────────────────────────────────
    //
    // `record_callback_warning` MUST push exactly one labelled string per
    // failed callback append, and `take_warnings()` MUST drain the
    // buffer atomically. Together they ensure every callback failure at
    // an event-emit site is surfaced (via the daemon's
    // `RuntimeResult.warnings` field) rather than dropped. These tests
    // pin that wire-level drift.

    #[test]
    fn record_callback_warning_pushes_when_result_is_err() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);
        assert!(w.take_warnings().is_empty());

        w.record_callback_warning(
            "graph_step_started",
            Err(anyhow::anyhow!("event-store rejected unknown_event_type")),
        );

        let drained = w.take_warnings();
        assert_eq!(drained.len(), 1);
        assert!(
            drained[0].contains("graph_step_started")
                && drained[0].contains("event-store rejected"),
            "warning must carry both the event label and the underlying error; got: {:?}",
            drained
        );
        // Drained: a second take must return empty.
        assert!(w.take_warnings().is_empty());
    }

    #[test]
    fn record_callback_warning_no_op_when_result_is_ok() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);

        w.record_callback_warning("tool_call_start", Ok(()));
        w.record_callback_warning("tool_call_result", Ok(()));

        assert!(
            w.take_warnings().is_empty(),
            "Ok results must NOT produce warnings"
        );
    }

    #[test]
    fn record_callback_warning_accumulates_multiple_errors() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![]);

        w.record_callback_warning("graph_started", Err(anyhow::anyhow!("a")));
        w.record_callback_warning("graph_step_started", Err(anyhow::anyhow!("b")));
        w.record_callback_warning("graph_completed", Err(anyhow::anyhow!("c")));

        let drained = w.take_warnings();
        assert_eq!(drained.len(), 3);
        assert!(drained[0].contains("graph_started"));
        assert!(drained[1].contains("graph_step_started"));
        assert!(drained[2].contains("graph_completed"));
    }

    // ── F3 tests: commit_step behavior ──────────────────────────────

    #[test]
    fn step_outcome_action_ok_captures_fields() {
        let outcome = StepOutcome::ActionOk {
            item_id: "tool:test/echo".to_string(),
            result: json!({"msg": "hello"}),
            assign: None,
            next: Some("done".to_string()),
            cache_hit: false,
            elapsed_ms: 42,
            cost: None,
        };
        match outcome {
            StepOutcome::ActionOk {
                ref item_id,
                ref next,
                elapsed_ms,
                ..
            } => {
                assert_eq!(item_id, "tool:test/echo");
                assert_eq!(next.as_deref(), Some("done"));
                assert_eq!(elapsed_ms, 42);
            }
            _ => panic!("expected ActionOk"),
        }
    }

    #[test]
    fn step_outcome_leaf_soft_error_captures_error() {
        let outcome = StepOutcome::LeafSoftError {
            item_id: "tool:test/fail".to_string(),
            error: "boom".to_string(),
            next_on_error: NextOnError::PolicyFail,
            elapsed_ms: 10,
            cost: None,
        };
        match outcome {
            StepOutcome::LeafSoftError {
                ref error,
                ref next_on_error,
                ..
            } => {
                assert_eq!(error, "boom");
                assert!(matches!(next_on_error, NextOnError::PolicyFail));
            }
            _ => panic!("expected LeafSoftError"),
        }
    }

    #[test]
    fn step_outcome_dispatch_hard_error_captures_error() {
        let outcome = StepOutcome::DispatchHardError {
            item_id: None,
            error: "permission denied".to_string(),
            next_on_error: NextOnError::Redirect("error_handler".to_string()),
            elapsed_ms: 1,
            cost: None,
        };
        match outcome {
            StepOutcome::DispatchHardError {
                item_id,
                ref error,
                ref next_on_error,
                ..
            } => {
                assert!(item_id.is_none());
                assert_eq!(error, "permission denied");
                assert!(matches!(next_on_error, NextOnError::Redirect(_)));
            }
            _ => panic!("expected DispatchHardError"),
        }
    }

    #[test]
    fn step_outcome_gate_taken_captures_target() {
        let outcome = StepOutcome::GateTaken {
            target: Some("fast_path".to_string()),
        };
        match outcome {
            StepOutcome::GateTaken { ref target } => {
                assert_eq!(target.as_deref(), Some("fast_path"));
            }
            _ => panic!("expected GateTaken"),
        }
    }

    #[test]
    fn step_outcome_foreach_done_captures_count() {
        let outcome = StepOutcome::ForeachDone {
            results: vec![json!(1), json!(2)],
            collect_key: Some("items".to_string()),
            var_name: "x".to_string(),
            assign_delta: json!({}),
            errors: Vec::new(),
            next: Some("done".to_string()),
            item_id: "tool:test/echo".to_string(),
            cost: None,
        };
        match outcome {
            StepOutcome::ForeachDone {
                ref next,
                ref collect_key,
                ..
            } => {
                assert_eq!(next.as_deref(), Some("done"));
                assert_eq!(collect_key.as_deref(), Some("items"));
            }
            _ => panic!("expected ForeachDone"),
        }
    }

    #[test]
    fn step_outcome_terminal_captures_status() {
        let outcome = StepOutcome::Terminal {
            status: "max_steps_exceeded",
            error: Some("hit limit".to_string()),
        };
        match outcome {
            StepOutcome::Terminal { status, ref error } => {
                assert_eq!(status, "max_steps_exceeded");
                assert_eq!(error.as_deref(), Some("hit limit"));
            }
            _ => panic!("expected Terminal"),
        }
    }

    #[test]
    fn next_on_error_redirect_from_node() {
        let node = GraphNode {
            on_error: Some("handler".to_string()),
            ..make_test_node()
        };
        let cfg = make_test_graph_config();
        let noe = resolve_next_on_error(&node, &cfg);
        assert!(matches!(noe, NextOnError::Redirect(ref t) if t == "handler"));
    }

    #[test]
    fn next_on_error_policy_fail_when_no_node_target() {
        let node = make_test_node();
        let cfg = GraphConfig {
            start: "x".to_string(),
            on_error: ErrorMode::Fail,
            ..make_test_graph_config()
        };
        let noe = resolve_next_on_error(&node, &cfg);
        assert!(matches!(noe, NextOnError::PolicyFail));
    }

    #[test]
    fn next_on_error_policy_continue_when_no_node_target() {
        let node = make_test_node();
        let cfg = GraphConfig {
            start: "x".to_string(),
            on_error: ErrorMode::Continue,
            ..make_test_graph_config()
        };
        let noe = resolve_next_on_error(&node, &cfg);
        assert!(matches!(noe, NextOnError::PolicyContinue));
    }

    // ── F3 commit_step tests: event ordering + checkpoint writes ─────

    /// A mock callback client that records every `append_event` call
    /// so tests can assert the exact event sequence produced by
    /// `commit_step`.
    struct RecordingMockClient {
        dispatch_results: Mutex<Vec<Value>>,
        events: Mutex<Vec<(String, String, Value, String)>>,
        /// (thread_id, status) pairs from finalize_thread calls.
        finalizations: Mutex<Vec<(String, String)>>,
        /// `TerminalCompletion.cost` (raw JSON) from finalize_thread calls.
        finalize_costs: Mutex<Vec<Option<Value>>>,
        /// Collected artifacts from publish_artifact calls.
        artifacts: Mutex<Vec<Value>>,
        /// Recorded `spawn_follow_child` requests (for follow idempotency tests).
        follow_requests: Mutex<Vec<ryeos_runtime::callback::SpawnFollowChildRequest>>,
        /// When true, `spawn_follow_child` returns an error (failed-handoff test).
        follow_should_fail: bool,
        /// Count of `dispatch_action` calls (to prove a follow resume re-dispatches
        /// nothing).
        dispatch_count: Mutex<usize>,
    }

    impl RecordingMockClient {
        fn new(dispatch_results: Vec<Value>) -> Self {
            Self {
                dispatch_results: Mutex::new(dispatch_results),
                events: Mutex::new(Vec::new()),
                finalizations: Mutex::new(Vec::new()),
                finalize_costs: Mutex::new(Vec::new()),
                artifacts: Mutex::new(Vec::new()),
                follow_requests: Mutex::new(Vec::new()),
                follow_should_fail: false,
                dispatch_count: Mutex::new(0),
            }
        }

        fn recorded_events(&self) -> Vec<(String, String, Value, String)> {
            self.events.lock().unwrap().clone()
        }

        fn dispatch_count(&self) -> usize {
            *self.dispatch_count.lock().unwrap()
        }

        fn recorded_follow_requests(
            &self,
        ) -> Vec<ryeos_runtime::callback::SpawnFollowChildRequest> {
            self.follow_requests.lock().unwrap().clone()
        }

        fn recorded_finalizations(&self) -> Vec<(String, String)> {
            self.finalizations.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for RecordingMockClient {
        async fn dispatch_action(
            &self,
            _request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            *self.dispatch_count.lock().unwrap() += 1;
            let mut results = self.dispatch_results.lock().unwrap();
            if results.is_empty() {
                Ok(json!({"thread": {}, "result": {}}))
            } else {
                Ok(json!({"thread": {}, "result": results.remove(0)}))
            }
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            thread_id: &str,
            completion: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            self.finalize_costs
                .lock()
                .unwrap()
                .push(completion.cost.clone());
            self.finalizations
                .lock()
                .unwrap()
                .push((thread_id.to_string(), completion.status));
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn request_continuation(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            thread_id: &str,
            event_type: &str,
            payload: Value,
            storage_class: &str,
        ) -> Result<Value, CallbackError> {
            self.events.lock().unwrap().push((
                thread_id.to_string(),
                event_type.to_string(),
                payload,
                storage_class.to_string(),
            ));
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: i64,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, artifact: Value) -> Result<Value, CallbackError> {
            self.artifacts.lock().unwrap().push(artifact);
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn spawn_follow_child(
            &self,
            request: ryeos_runtime::callback::SpawnFollowChildRequest,
        ) -> Result<Value, CallbackError> {
            self.follow_requests.lock().unwrap().push(request);
            if self.follow_should_fail {
                Err(CallbackError::ActionFailed {
                    code: "test".to_string(),
                    message: "simulated daemon follow failure".to_string(),
                    retryable: false,
                })
            } else {
                Ok(json!({ "phase": "waiting" }))
            }
        }
    }

    fn make_recording_callback(results: Vec<Value>) -> (CallbackClient, Arc<RecordingMockClient>) {
        let inner: Arc<RecordingMockClient> = Arc::new(RecordingMockClient::new(results));
        let client = CallbackClient::from_inner(
            inner.clone(),
            "thread-test",
            "/tmp/test-project",
            "tat-test",
        );
        (client, inner)
    }

    fn make_recording_walker(
        graph: GraphDefinition,
        results: Vec<Value>,
        checkpoint_dir: Option<&std::path::Path>,
    ) -> (Walker, Arc<RecordingMockClient>) {
        let (client, recorder) = make_recording_callback(results);
        let checkpoint = checkpoint_dir.map(|d| CheckpointWriter::new(d.to_path_buf()));
        let w = Walker::new(
            graph,
            "/tmp/test-project".to_string(),
            "thread-test".to_string(),
            client,
            checkpoint,
        );
        (w, recorder)
    }

    // ── §A per-step retry ────────────────────────────────────────────

    const RETRY_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: flaky
  nodes:
    flaky:
      action: {item_id: "tool:test/flaky"}
      retry: {attempts: 3, backoff_ms: 1}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;

    fn subprocess_failure() -> Value {
        json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1, "stderr": "boom"},
            "artifacts": [],
        })
    }

    fn subprocess_success() -> Value {
        json!({"outcome_code": null, "result": {"ok": true}, "error": null, "artifacts": []})
    }

    #[tokio::test]
    async fn retry_redispatches_until_success() {
        // First dispatch fails, the retry re-dispatches and succeeds. The
        // failed attempt consumed a walker step, so `done` is reached at step 2.
        let graph = make_graph(RETRY_YAML);
        let w = make_walker(graph, vec![subprocess_failure(), subprocess_success()]);
        let result = w.execute(json!({}), None).await;
        assert!(result.success, "retry should recover: {result:?}");
        assert_eq!(result.status, "completed");
        assert_eq!(
            result.steps, 2,
            "one failed attempt + successful re-dispatch = 2 steps to reach the return node"
        );
        // A recovered retry leaves no suppressed error behind.
        assert!(result.errors.is_none(), "recovered retry records no error");
    }

    #[tokio::test]
    async fn retry_exhausts_then_routes_on_error() {
        // attempts:2 → two dispatches, both fail, then `on_error` redirects to
        // the recover return node. The retry is bounded — it does not loop
        // forever on a persistent failure.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: flaky
  nodes:
    flaky:
      action: {item_id: "tool:test/flaky"}
      retry: {attempts: 2, backoff_ms: 1}
      on_error: recover
    recover:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![subprocess_failure(), subprocess_failure()]);
        let result = w.execute(json!({}), None).await;
        assert_eq!(result.status, "completed");
        assert_eq!(
            result.steps, 2,
            "attempt 1 (retry) + attempt 2 (exhausted → redirect) = 2 steps"
        );
    }

    #[tokio::test]
    async fn retry_emits_braid_visible_retry_event() {
        // A re-attempt emits exactly one graph_node_retry milestone carrying the
        // attempt number, the total, and the backoff — indexed (braid-visible).
        let graph = make_graph(RETRY_YAML);
        let (w, rec) = make_recording_walker(
            graph,
            vec![subprocess_failure(), subprocess_success()],
            None,
        );
        let result = w.execute(json!({}), Some("gr-retry".to_string())).await;
        assert!(result.success, "retry should recover: {result:?}");

        let events = rec.recorded_events();
        let retries: Vec<_> = events
            .iter()
            .filter(|(_, ty, _, _)| ty == "graph_node_retry")
            .collect();
        assert_eq!(
            retries.len(),
            1,
            "one failed attempt → exactly one retry event; events={events:#?}"
        );
        let (_, _, payload, storage_class) = retries[0];
        assert_eq!(payload["attempt"], 1);
        assert_eq!(payload["attempts"], 3);
        assert_eq!(payload["delay_ms"], 1);
        assert_eq!(payload["node"], "flaky");
        assert_eq!(
            storage_class, "indexed",
            "graph_node_retry is an indexed milestone"
        );
    }

    #[tokio::test]
    async fn retry_resumes_with_persisted_attempt_count() {
        // The attempt counter rides the checkpoint (v2): a walker resumed with
        // `retry_attempt: 1` on a node whose only remaining attempt fails routes
        // straight to on_error — it does NOT restart the count and retry again.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: flaky
  nodes:
    flaky:
      action: {item_id: "tool:test/flaky"}
      retry: {attempts: 2, backoff_ms: 1}
      on_error: recover
    recover:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let (w, rec) = make_recording_walker(graph, vec![subprocess_failure()], None);
        // Resume as though attempt 1 already failed pre-cut (retry_attempt: 1).
        let result = w
            .execute(
                json!({
                    "resume_state": {
                        "current_node": "flaky",
                        "step_count": 5,
                        "state": {},
                        "graph_run_id": "gr-resumed",
                        "retry_attempt": 1,
                    }
                }),
                None,
            )
            .await;
        assert_eq!(result.status, "completed", "recover is terminal");
        let events = rec.recorded_events();
        let retries = events
            .iter()
            .filter(|(_, ty, _, _)| ty == "graph_node_retry")
            .count();
        assert_eq!(
            retries, 0,
            "the persisted count was exhausted on the single remaining attempt — no new retry; \
             events={events:#?}"
        );
    }

    // ── §B2 graph hooks ──────────────────────────────────────────────

    const HOOK_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - id: notify
      event: graph_completed
      action: {item_id: "tool:test/notify", params: {}}
  nodes:
    done:
      node_type: return
"#;

    #[tokio::test]
    async fn graph_completed_hook_dispatches_through_callback() {
        // An authored graph_completed hook fires at the terminal, dispatching
        // its action through the same callback a node action uses.
        let graph = make_graph(HOOK_YAML);
        let (w, rec) = make_recording_walker(graph, vec![], None);
        let result = w.execute(json!({}), Some("gr-hook".to_string())).await;
        assert!(result.success, "graph completes: {result:?}");
        assert_eq!(
            rec.dispatch_count(),
            1,
            "the graph_completed hook must dispatch exactly once"
        );
        assert!(
            w.take_warnings().is_empty(),
            "a successful hook records no warning"
        );
    }

    #[tokio::test]
    async fn failing_hook_warns_but_does_not_fail_graph() {
        // A hook child that fails is a recorded warning, never a graph failure —
        // graph hooks are observers.
        let graph = make_graph(HOOK_YAML);
        let fail = json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1, "stderr": "hook boom"},
        });
        let (w, _rec) = make_recording_walker(graph, vec![fail], None);
        let result = w.execute(json!({}), Some("gr-hookfail".to_string())).await;
        assert!(
            result.success,
            "a failing observer hook must not fail the graph: {result:?}"
        );
        let warnings = w.take_warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("graph hook") && w.contains("graph_completed")),
            "expected a recorded hook warning, got: {warnings:?}"
        );
    }

    const FOLLOW_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;

    #[tokio::test]
    async fn follow_suspend_emits_events_and_no_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let (w, rec) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], Some(tmp.path()));
        // Status `continued` implies write_follow_checkpoint succeeded (a failed
        // checkpoint would route to a terminal error instead).
        let result = w.execute(json!({}), Some("gr-follow".to_string())).await;
        assert_eq!(result.status, "continued");
        assert!(!result.success);
        assert_eq!(result.steps, 0);
        assert!(result.result.is_none());

        let types: Vec<String> = rec
            .recorded_events()
            .into_iter()
            .map(|(_, et, _, _)| et)
            .collect();
        assert!(types.iter().any(|t| t == "graph_step_started"));
        assert!(types.iter().any(|t| t == "graph_follow_suspended"));
        // The suspend must NOT emit the normal action lifecycle — the child result
        // does not exist yet; those are emitted on resume.
        for absent in [
            "tool_call_start",
            "tool_call_result",
            "graph_step_completed",
            "graph_completed",
        ] {
            assert!(
                !types.iter().any(|t| t == absent),
                "unexpected {absent} at suspend; events: {types:?}"
            );
        }
        // Suspended, not finalized (the daemon settles `continued`).
        assert!(rec.recorded_finalizations().is_empty());
        // The handoff carried exactly the run identity that forms the follow_key.
        let reqs = rec.recorded_follow_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].graph_run_id, "gr-follow");
        assert_eq!(reqs[0].follow_node, "fetch");
        assert_eq!(reqs[0].step_count, 0);
        assert_eq!(reqs[0].child_item_ref, "directive:child");
    }

    #[tokio::test]
    async fn follow_reentry_preserves_graph_run_id() {
        // First pass under the original run id records the handoff.
        let (w1, rec1) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], None);
        let r1 = w1.execute(json!({}), Some("gr-original".to_string())).await;
        assert_eq!(r1.status, "continued");
        assert_eq!(
            rec1.recorded_follow_requests()[0].graph_run_id,
            "gr-original"
        );

        // Resume with a DIFFERENT outer run id, but resume_state carrying the
        // original (as main.rs injects it). The re-entry MUST re-drive with the
        // ORIGINAL run id so the follow_key is unchanged — otherwise it would spawn
        // a second, distinct follow child.
        let (w2, rec2) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], None);
        let resume = json!({
            "resume_state": {
                "current_node": "fetch",
                "step_count": 0,
                "state": {},
                "graph_run_id": "gr-original",
            }
        });
        let r2 = w2
            .execute(resume, Some("gr-different-outer".to_string()))
            .await;
        assert_eq!(r2.status, "continued");
        let req = &rec2.recorded_follow_requests()[0];
        assert_eq!(
            req.graph_run_id, "gr-original",
            "re-entry must reuse the original run id, not the outer one"
        );
        assert_eq!(req.follow_node, "fetch");
        assert_eq!(req.step_count, 0);
    }

    #[tokio::test]
    async fn follow_failed_handoff_terminates_error() {
        let inner: Arc<RecordingMockClient> = Arc::new(RecordingMockClient {
            follow_should_fail: true,
            ..RecordingMockClient::new(vec![])
        });
        let client = CallbackClient::from_inner(
            inner.clone(),
            "thread-test",
            "/tmp/test-project",
            "tat-test",
        );
        let w = Walker::new(
            make_graph(FOLLOW_YAML),
            "/tmp/test-project".to_string(),
            "thread-test".to_string(),
            client,
            None,
        );
        let result = w.execute(json!({}), Some("gr-fail".to_string())).await;

        // A failed handoff settles a terminal error — NEVER `continued` with no
        // child behind it.
        assert_ne!(result.status, "continued");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("follow handoff failed"),
            "expected follow-handoff error, got: {:?}",
            result.error
        );
        // The thread is finalized, not left dangling `continued`.
        assert!(!inner.recorded_finalizations().is_empty());
    }

    #[tokio::test]
    async fn follow_resume_consumes_child_result_and_completes() {
        // Resume INTO the follow node with a spliced child envelope: the node must
        // consume it (classify like a live dispatch) and run the NORMAL outcome —
        // receipt + step_completed + completion — instead of re-suspending.
        let (w, rec) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], None);
        let resume = json!({
            "resume_state": {
                "current_node": "fetch",
                "step_count": 0,
                "state": {},
                "graph_run_id": "gr-resume",
                "pending_follow": {
                    "follow_node": "fetch",
                    "step_count": 0,
                    "graph_run_id": "gr-resume",
                },
                "follow_result": { "msg": "child done" },
            }
        });
        let result = w.execute(resume, Some("gr-resume".to_string())).await;

        // Ran to completion, NOT continued — the child result was consumed.
        assert!(result.success);
        assert_eq!(result.status, "completed");

        let types: Vec<String> = rec
            .recorded_events()
            .into_iter()
            .map(|(_, et, _, _)| et)
            .collect();
        // The normal lifecycle deferred from suspend now lands on resume.
        assert!(types.iter().any(|t| t == "graph_step_completed"));
        assert!(types.iter().any(|t| t == "graph_completed"));
        // It did NOT re-suspend, issued no new follow handoff, and — critically —
        // never re-dispatched: the child already ran; the parent only consumed it.
        assert!(!types.iter().any(|t| t == "graph_follow_suspended"));
        assert!(rec.recorded_follow_requests().is_empty());
        assert_eq!(rec.dispatch_count(), 0);
    }

    const FOLLOW_ON_ERROR_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      on_error: recover
      next: {type: unconditional, to: done}
    recover:
      node_type: return
      output: "recovered"
    done:
      node_type: return
"#;

    const FOLLOW_ENV_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      env_requires: ["RYEOS_FOLLOW_TEST_DEFINITELY_UNSET"]
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;

    /// Build a resume_state params object for the `fetch` follow node with an
    /// optional child envelope.
    fn follow_resume_params(follow_result: Option<Value>) -> Value {
        let mut rs = json!({
            "current_node": "fetch",
            "step_count": 0,
            "state": {},
            "graph_run_id": "gr-resume",
            "pending_follow": {
                "follow_node": "fetch",
                "step_count": 0,
                "graph_run_id": "gr-resume",
            },
        });
        if let Some(fr) = follow_result {
            rs["follow_result"] = fr;
        }
        json!({ "resume_state": rs })
    }

    #[tokio::test]
    async fn follow_resume_success_accounts_cost() {
        // A native child envelope with cost: resume must land the receipt AND the
        // child cost in graph accounting, exactly like a live native dispatch.
        let (w, rec) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], None);
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": "child_ok",
            "outputs": {"x": 1},
            "cost": {"input_tokens": 120, "output_tokens": 45, "total_usd": 0.0012},
            "warnings": []
        });
        let result = w
            .execute(
                follow_resume_params(Some(envelope)),
                Some("gr-resume".to_string()),
            )
            .await;

        assert!(result.success);
        assert_eq!(result.status, "completed");
        // No re-dispatch, no re-suspend.
        assert_eq!(rec.dispatch_count(), 0);
        assert!(rec.recorded_follow_requests().is_empty());
        // The child cost flows into the run total + per-node costs.
        let cost = result.cost.expect("follow child cost must be accounted");
        assert_eq!(cost.input_tokens, 120);
        assert_eq!(cost.output_tokens, 45);
        assert!(!result.node_costs.is_empty());
    }

    #[tokio::test]
    async fn follow_resume_failure_routes_on_error() {
        // A native FAILURE envelope on resume must behave like a live leaf failure:
        // error receipt + graph_step_completed(error), on_error redirect taken,
        // failed-child cost preserved — and no dispatch/handoff.
        let (w, rec) = make_recording_walker(make_graph(FOLLOW_ON_ERROR_YAML), vec![], None);
        let envelope = json!({
            "success": false,
            "status": "error",
            "result": {"error": "model refused"},
            "outputs": null,
            "cost": {"input_tokens": 80, "output_tokens": 0, "total_usd": 0.0008},
            "warnings": []
        });
        let result = w
            .execute(
                follow_resume_params(Some(envelope)),
                Some("gr-resume".to_string()),
            )
            .await;

        // on_error: recover redirects to the recover return node → the run
        // completes rather than hard-failing.
        assert_eq!(result.status, "completed");
        assert_eq!(rec.dispatch_count(), 0);
        assert!(rec.recorded_follow_requests().is_empty());
        // The follow node's step recorded an ERROR completion, and the failed
        // child's cost was still accounted.
        let step_completed_error = rec
            .recorded_events()
            .into_iter()
            .any(|(_, et, payload, _)| {
                et == "graph_step_completed"
                    && payload.get("status").and_then(|s| s.as_str()) == Some("error")
            });
        assert!(
            step_completed_error,
            "expected an error graph_step_completed"
        );
        assert_eq!(
            result
                .cost
                .expect("failed child cost preserved")
                .input_tokens,
            80
        );
    }

    #[tokio::test]
    async fn follow_bare_marker_resuspends() {
        // A resume with a pending_follow marker but NO spliced result must NOT
        // consume anything — it re-drives the suspend idempotently with the
        // original run id / node / step.
        let (w, rec) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], None);
        let result = w
            .execute(follow_resume_params(None), Some("gr-resume".to_string()))
            .await;

        assert_eq!(result.status, "continued");
        assert_eq!(rec.dispatch_count(), 0);
        let reqs = rec.recorded_follow_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].graph_run_id, "gr-resume");
        assert_eq!(reqs[0].follow_node, "fetch");
        assert_eq!(reqs[0].step_count, 0);
        assert!(rec
            .recorded_events()
            .into_iter()
            .any(|(_, et, _, _)| et == "graph_follow_suspended"));
    }

    #[tokio::test]
    async fn follow_resume_ignores_failing_env_preflight() {
        // A follow-resume node with a failing env_requires must still consume the
        // stored child result — the child already ran; a parent-side env gap must
        // not turn its result into a dispatch error.
        let (w, rec) = make_recording_walker(make_graph(FOLLOW_ENV_YAML), vec![], None);
        let envelope = json!({ "result": {"ok": true} });
        let result = w
            .execute(
                follow_resume_params(Some(envelope)),
                Some("gr-resume".to_string()),
            )
            .await;

        assert!(result.success);
        assert_eq!(result.status, "completed");
        assert_eq!(rec.dispatch_count(), 0);
    }

    const TWO_FOLLOW_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch1
  nodes:
    fetch1:
      follow: true
      action: {item_id: "directive:child1", params: {}}
      next: {type: unconditional, to: fetch2}
    fetch2:
      follow: true
      action: {item_id: "directive:child2", params: {}}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;

    #[tokio::test]
    async fn two_sequential_follow_nodes_suspend_and_resume_in_order() {
        // fetch1 (follow) → fetch2 (follow) → done. Each follow node suspends; after
        // consuming its child result the graph advances to the NEXT follow node and
        // suspends again, then finally completes.

        // Pass 1: suspend at the first follow node.
        let (w1, rec1) = make_recording_walker(make_graph(TWO_FOLLOW_YAML), vec![], None);
        let r1 = w1.execute(json!({}), Some("gr-seq".to_string())).await;
        assert_eq!(r1.status, "continued");
        assert_eq!(rec1.recorded_follow_requests()[0].follow_node, "fetch1");

        // Pass 2: resume fetch1 with its child result → advance to fetch2 → suspend
        // there (a DISTINCT follow handoff, at the next step).
        let (w2, rec2) = make_recording_walker(make_graph(TWO_FOLLOW_YAML), vec![], None);
        let resume1 = json!({
            "resume_state": {
                "current_node": "fetch1",
                "step_count": 0,
                "state": {},
                "graph_run_id": "gr-seq",
                "pending_follow": { "follow_node": "fetch1", "step_count": 0, "graph_run_id": "gr-seq" },
                "follow_result": { "result": "child1 done" },
            }
        });
        let r2 = w2.execute(resume1, Some("gr-seq".to_string())).await;
        assert_eq!(
            r2.status, "continued",
            "must suspend again at the second follow node"
        );
        let req2 = rec2.recorded_follow_requests();
        assert_eq!(
            req2.len(),
            1,
            "resuming fetch1 issues exactly one new handoff (fetch2)"
        );
        assert_eq!(
            req2[0].follow_node, "fetch2",
            "the second suspend is at fetch2"
        );
        let fetch2_step = req2[0].step_count;

        // Pass 3: resume fetch2 with its child result → the graph completes.
        let (w3, _rec3) = make_recording_walker(make_graph(TWO_FOLLOW_YAML), vec![], None);
        let resume2 = json!({
            "resume_state": {
                "current_node": "fetch2",
                "step_count": fetch2_step,
                "state": {},
                "graph_run_id": "gr-seq",
                "pending_follow": { "follow_node": "fetch2", "step_count": fetch2_step, "graph_run_id": "gr-seq" },
                "follow_result": { "result": "child2 done" },
            }
        });
        let r3 = w3.execute(resume2, Some("gr-seq".to_string())).await;
        assert_eq!(
            r3.status, "completed",
            "after both follow nodes resume, the graph completes"
        );
        assert!(r3.success);
    }

    /// Assert the R3 fence order for an action-success step:
    /// graph_step_started → tool_call_start → tool_call_result → graph_step_completed
    /// followed (on advance) by checkpoint, and finally GraphCompleted on terminal.
    #[tokio::test]
    async fn commit_step_emits_events_in_fence_order() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      assign: {echo_result: "${result}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let tmp = tempfile::tempdir().unwrap();
        let (w, recorder) =
            make_recording_walker(graph, vec![json!({"msg": "hello"})], Some(tmp.path()));

        let result = w
            .execute(json!({}), Some("gr-fence-test".to_string()))
            .await;
        assert!(result.success);
        assert_eq!(result.definition_ref, "graph:test/test");
        assert_eq!(result.graph_run_id, "gr-fence-test");
        assert_eq!(
            result.definition_hash,
            lillux::cas::sha256_hex(lillux::signature::strip_signature_lines(yaml).as_bytes())
        );

        let events = recorder.recorded_events();
        let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

        for (_, event_type, payload, _) in &events {
            match event_type.as_str() {
                "graph_started"
                | "graph_completed"
                | "graph_step_started"
                | "graph_step_completed"
                | "tool_call_start"
                | "tool_call_result"
                | "graph_branch_taken"
                | "graph_foreach_iteration" => {
                    assert_eq!(
                        payload["definition_ref"].as_str(),
                        Some(result.definition_ref.as_str())
                    );
                    assert_eq!(
                        payload["definition_hash"].as_str(),
                        Some(result.definition_hash.as_str())
                    );
                }
                _ => {}
            }
        }

        for (_, event_type, payload, _) in &events {
            match event_type.as_str() {
                "graph_step_started"
                | "graph_step_completed"
                | "tool_call_start"
                | "tool_call_result"
                | "graph_branch_taken"
                | "graph_foreach_iteration" => {
                    assert_eq!(
                        payload["node_ref"].as_str(),
                        Some("graph:test/test#node:step1")
                    );
                }
                _ => {}
            }
        }

        // graph_started is emitted before the loop starts
        let idx = types.iter().position(|&t| t == "graph_started").unwrap();

        // Step 1: action node — R3 fence order
        assert_eq!(
            types[idx + 1],
            "graph_step_started",
            "fence: graph_step_started first"
        );
        assert_eq!(
            types[idx + 2],
            "tool_call_start",
            "fence: tool_call_start second"
        );
        assert_eq!(
            types[idx + 3],
            "tool_call_result",
            "fence: tool_call_result third"
        );
        assert_eq!(
            types[idx + 4],
            "graph_step_completed",
            "fence: graph_step_completed fourth"
        );

        // Return node is terminal — goes through commit_terminal directly,
        // which emits GraphCompleted but no graph_step_started for the
        // terminal step itself.
        assert_eq!(
            types[idx + 5],
            "graph_completed",
            "after step_completed, terminal emits graph_completed directly"
        );

        // GraphCompleted must appear exactly once
        let completed_count = types.iter().filter(|&&t| t == "graph_completed").count();
        assert_eq!(
            completed_count, 1,
            "GraphCompleted must be emitted exactly once, got {completed_count}"
        );

        let artifacts = recorder.artifacts.lock().unwrap();
        let receipt_artifact = artifacts
            .iter()
            .find(|a| a["artifact_type"] == "graph_node_receipt")
            .expect("action receipt artifact should be published");
        assert_eq!(
            receipt_artifact["uri"].as_str(),
            Some("graph://runs/gr-fence-test/node-receipts/0")
        );
        let receipt = &receipt_artifact["metadata"];
        assert_eq!(
            receipt["definition_ref"].as_str(),
            Some(result.definition_ref.as_str())
        );
        assert_eq!(
            receipt["definition_hash"].as_str(),
            Some(result.definition_hash.as_str())
        );
        assert_eq!(receipt["graph_run_id"].as_str(), Some("gr-fence-test"));
        assert_eq!(receipt["node"].as_str(), Some("step1"));
        assert_eq!(
            receipt["node_result_hash"].as_str(),
            Some(hash_json_value(&json!({"msg": "hello"})).as_str())
        );
    }

    /// Every non-terminal `Advance` must write a checkpoint. For a
    /// two-step graph (action → return), the final checkpoint should
    /// point at the return node. We verify via the TempDir checkpoint file.
    #[tokio::test]
    async fn commit_step_writes_checkpoint_on_every_advance() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  max_steps: 10
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: step2
    step2:
      action: {item_id: "tool:test/echo", params: {msg: world}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let tmp = tempfile::tempdir().unwrap();
        let (w, _recorder) = make_recording_walker(
            graph,
            vec![json!({"msg": "hello"}), json!({"msg": "world"})],
            Some(tmp.path()),
        );

        let result = w.execute(json!({}), Some("gr-cp-test".to_string())).await;
        assert!(result.success);

        // After step1 completes, checkpoint points at "step2" (the next node).
        // After step2 completes, checkpoint points at "done" (the return node).
        // The return node itself is terminal — no checkpoint is written for it.
        let checkpoint_file = tmp.path().join("latest.json");
        assert!(
            checkpoint_file.exists(),
            "checkpoint file must exist after graph completes"
        );
        let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
        let cp: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            cp["current_node"], "done",
            "checkpoint must point at the next cursor (done)"
        );
        assert_eq!(
            cp["step_count"], 2,
            "checkpoint step_count must be 2 (two action steps, return is terminal)"
        );
    }

    /// Gate node must produce: graph_step_started → graph_branch_taken → graph_step_completed → checkpoint.
    #[tokio::test]
    async fn gate_step_emits_lifecycle_and_checkpoint() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      assign: {mode: fast}
      next:
        type: conditional
        branches:
          - when: {path: state.mode, op: eq, value: fast}
            to: fast_path
          - to: slow_path
    fast_path:
      node_type: return
    slow_path:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let tmp = tempfile::tempdir().unwrap();
        let (w, recorder) = make_recording_walker(graph, vec![], Some(tmp.path()));

        let result = w
            .execute(
                json!({"inject_state": {"mode": "fast"}}),
                Some("gr-gate-test".to_string()),
            )
            .await;
        assert!(result.success);

        let events = recorder.recorded_events();
        let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

        // Gate lifecycle: graph_step_started → graph_branch_taken → graph_step_completed
        let step_started_idx = types
            .iter()
            .position(|&t| t == "graph_step_started")
            .unwrap();
        assert_eq!(
            types[step_started_idx + 1],
            "graph_branch_taken",
            "gate must emit graph_branch_taken after graph_step_started"
        );
        assert_eq!(
            types[step_started_idx + 2],
            "graph_step_completed",
            "gate must emit graph_step_completed after graph_branch_taken"
        );

        // Verify the branch target is correct
        let branch_event = events
            .iter()
            .find(|(_, et, _, _)| et == "graph_branch_taken")
            .unwrap();
        assert_eq!(branch_event.2["target"], "fast_path");
        assert_eq!(
            branch_event.2["node_ref"].as_str(),
            Some("graph:test/test#node:check")
        );
        assert_eq!(
            branch_event.2["target_node_ref"].as_str(),
            Some("graph:test/test#node:fast_path")
        );

        // Checkpoint must exist pointing at the next node
        let checkpoint_file = tmp.path().join("latest.json");
        assert!(
            checkpoint_file.exists(),
            "checkpoint must exist after gate step"
        );
        let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
        let cp: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(cp["current_node"], "fast_path");
        // S5: payload is versioned and carries an accounting snapshot so resume
        // restores accumulated cost rather than restarting it at zero. `total`
        // may be null (no cost-bearing node yet); `nodes` is always an array.
        assert_eq!(cp["schema_version"], GRAPH_CHECKPOINT_SCHEMA_VERSION);
        let accounting = cp
            .get("accounting")
            .expect("checkpoint must carry an accounting snapshot");
        assert!(
            accounting["nodes"].is_array(),
            "accounting.nodes must be an array: {accounting}"
        );
    }

    /// Foreach node must emit per-iteration events (graph_foreach_iteration)
    /// and collect results into state.
    #[tokio::test]
    async fn foreach_step_emits_iteration_events() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let (w, recorder) = make_recording_walker(
            graph,
            vec![
                json!({"value": "a"}),
                json!({"value": "b"}),
                json!({"value": "c"}),
            ],
            None,
        );

        let result = w
            .execute(
                json!({"inject_state": {"items": ["a", "b", "c"]}}),
                Some("gr-fe-test".to_string()),
            )
            .await;
        assert!(result.success);

        let events = recorder.recorded_events();
        let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

        // Foreach must emit per-iteration events
        let iteration_count = types
            .iter()
            .filter(|&&t| t == "graph_foreach_iteration")
            .count();
        assert_eq!(iteration_count, 3,
            "foreach must emit exactly 3 graph_foreach_iteration events for 3 items, got {iteration_count}");

        // Foreach step emits graph_step_started + graph_step_completed.
        // The return node is terminal — commit_terminal does NOT emit
        // graph_step_started for terminal steps.
        let step_started = types.iter().filter(|&&t| t == "graph_step_started").count();
        let step_completed = types
            .iter()
            .filter(|&&t| t == "graph_step_completed")
            .count();
        assert_eq!(
            step_started, 1,
            "1 foreach step (return node is terminal, no step_started)"
        );
        assert_eq!(
            step_completed, 1,
            "1 foreach step (return node is terminal, no step_completed)"
        );
    }

    #[test]
    fn node_result_hash_uses_canonical_json() {
        let mut left = serde_json::Map::new();
        left.insert("b".into(), json!(2));
        left.insert("a".into(), json!(1));

        let mut right = serde_json::Map::new();
        right.insert("a".into(), json!(1));
        right.insert("b".into(), json!(2));

        let left = Value::Object(left);
        let right = Value::Object(right);
        let expected = lillux::cas::sha256_hex(lillux::cas::canonical_json(&right).as_bytes());

        assert_eq!(hash_json_value(&left), expected);
        assert_eq!(hash_json_value(&left), hash_json_value(&right));
    }

    #[tokio::test]
    async fn action_leaf_errors_publish_error_receipts() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
"#;
        let graph = make_graph(yaml);
        let (w, recorder) = make_recording_walker(
            graph,
            vec![
                json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced"}}),
            ],
            None,
        );

        let result = w
            .execute(json!({}), Some("gr-error-receipt".to_string()))
            .await;
        assert!(!result.success);

        let artifacts = recorder.artifacts.lock().unwrap();
        let receipt_artifact = artifacts
            .iter()
            .find(|a| {
                a["artifact_type"] == "graph_node_receipt" && a["metadata"]["node"] == "step1"
            })
            .expect("error node receipt should be published");
        assert_eq!(
            receipt_artifact["uri"].as_str(),
            Some("graph://runs/gr-error-receipt/node-receipts/0")
        );
        let receipt = &receipt_artifact["metadata"];

        assert_eq!(
            receipt["definition_ref"].as_str(),
            Some(result.definition_ref.as_str())
        );
        assert_eq!(
            receipt["definition_hash"].as_str(),
            Some(result.definition_hash.as_str())
        );
        assert_eq!(receipt["graph_run_id"].as_str(), Some("gr-error-receipt"));
        assert_eq!(receipt["node_result_hash"], Value::Null);
        let receipt_error = receipt["error"].as_str().unwrap_or_default();
        assert!(
            receipt_error.contains("exit:1") && receipt_error.contains("forced"),
            "receipt error should carry the failure diagnostic, got: {receipt_error}"
        );
    }

    #[tokio::test]
    async fn action_dispatch_hard_errors_publish_error_receipts() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      env_requires: [RYEOS_TEST_MISSING_FOR_HARD_ERROR_RECEIPT]
      action: {item_id: "tool:test/env"}
"#;
        let graph = make_graph(yaml);
        let (w, recorder) = make_recording_walker(graph, vec![], None);

        let result = w
            .execute(json!({}), Some("gr-hard-error-receipt".to_string()))
            .await;
        assert!(!result.success);

        let artifacts = recorder.artifacts.lock().unwrap();
        let receipt_artifact = artifacts
            .iter()
            .find(|a| {
                a["artifact_type"] == "graph_node_receipt" && a["metadata"]["node"] == "step1"
            })
            .expect("hard-error node receipt should be published");
        assert_eq!(
            receipt_artifact["uri"].as_str(),
            Some("graph://runs/gr-hard-error-receipt/node-receipts/0")
        );
        let receipt = &receipt_artifact["metadata"];

        assert_eq!(
            receipt["definition_ref"].as_str(),
            Some(result.definition_ref.as_str())
        );
        assert_eq!(
            receipt["definition_hash"].as_str(),
            Some(result.definition_hash.as_str())
        );
        assert_eq!(
            receipt["graph_run_id"].as_str(),
            Some("gr-hard-error-receipt")
        );
        assert_eq!(receipt["node_result_hash"], Value::Null);
        assert!(receipt["error"]
            .as_str()
            .is_some_and(|err| err.contains("env preflight failed")));
    }

    #[tokio::test]
    async fn action_error_redirects_write_checkpoint() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      on_error: handler
      action: {item_id: "tool:test/fail"}
    handler:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let tmp = tempfile::tempdir().unwrap();
        let (w, _recorder) = make_recording_walker(
            graph,
            vec![
                json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced"}}),
            ],
            Some(tmp.path()),
        );

        let result = w
            .execute(json!({}), Some("gr-error-redirect".to_string()))
            .await;
        assert!(result.success);

        let checkpoint_file = tmp.path().join("latest.json");
        assert!(
            checkpoint_file.exists(),
            "redirect advance must write checkpoint"
        );
        let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
        let cp: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(cp["current_node"], "handler");
        assert_eq!(cp["step_count"], 1);
        assert_eq!(cp["graph_run_id"], "gr-error-redirect");
    }

    /// Terminal outcomes must emit GraphCompleted exactly once.
    /// Test both the success path (return node) and the error path (on_error: fail).
    #[tokio::test]
    async fn commit_step_terminates_emit_graph_completed_exactly_once() {
        // Success path
        let yaml_ok = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hi}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph_ok = make_graph(yaml_ok);
        let (w_ok, recorder_ok) = make_recording_walker(graph_ok, vec![json!({"msg": "hi"})], None);

        let result_ok = w_ok.execute(json!({}), Some("gr-t1".to_string())).await;
        assert!(result_ok.success);
        let events_ok = recorder_ok.recorded_events();
        let types_ok: Vec<&str> = events_ok.iter().map(|(_, et, _, _)| et.as_str()).collect();
        let completed_ok = types_ok.iter().filter(|&&t| t == "graph_completed").count();
        assert_eq!(
            completed_ok, 1,
            "success path: exactly 1 GraphCompleted, got {completed_ok}"
        );

        // Error path: on_error: fail with a leaf that returns status=error
        let yaml_err = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph_err = make_graph(yaml_err);
        let (w_err, recorder_err) = make_recording_walker(
            graph_err,
            vec![
                json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced"}}),
            ],
            None,
        );

        let result_err = w_err.execute(json!({}), Some("gr-t2".to_string())).await;
        assert!(!result_err.success);
        let events_err = recorder_err.recorded_events();
        let types_err: Vec<&str> = events_err.iter().map(|(_, et, _, _)| et.as_str()).collect();
        let completed_err = types_err
            .iter()
            .filter(|&&t| t == "graph_completed")
            .count();
        assert_eq!(
            completed_err, 1,
            "error path: exactly 1 GraphCompleted, got {completed_err}"
        );

        // Verify the error path's GraphCompleted carries status=error
        let events_err_full = recorder_err.recorded_events();
        let gc = events_err_full
            .iter()
            .find(|(_, et, _, _)| et == "graph_completed")
            .unwrap();
        assert_eq!(gc.2["status"], "error");
    }
}
