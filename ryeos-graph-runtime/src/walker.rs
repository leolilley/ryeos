#[cfg(test)]
use std::collections::HashMap;
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
use crate::hooks;
use crate::knowledge;
use crate::model::*;
use crate::persistence;
use crate::validation::analyze_graph;
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::checkpoint::CheckpointWriter;
use ryeos_runtime::events::RuntimeEventType;

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
        next: Option<String>,
        cache_hit: bool,
        elapsed_ms: u64,
    },
    /// Action node ran but the leaf reported `status == "error"`.
    LeafSoftError {
        item_id: String,
        error: String,
        next_on_error: NextOnError,
        elapsed_ms: u64,
    },
    /// Dispatch failed before the leaf returned anything (transport,
    /// permission, env preflight).
    DispatchHardError {
        item_id: Option<String>,
        error: String,
        next_on_error: NextOnError,
        elapsed_ms: u64,
    },
    /// Gate node: condition evaluation picked `target`.
    GateTaken {
        target: Option<String>,
    },
    /// Foreach node completed all iterations.
    ForeachDone {
        results: Vec<Value>,
        collect_key: Option<String>,
        var_name: String,
        next: Option<String>,
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
    Advance { next_node: String, next_step: u32 },
    Terminate(GraphResult),
}

pub struct Walker {
    graph: GraphDefinition,
    project_path: String,
    thread_id: String,
    client: CallbackClient,
    checkpoint: Option<CheckpointWriter>,
    /// Accumulated non-fatal callback drift surfaced during a single
    /// `execute` run. Replaces the V5.4-era `let _ =
    /// self.client.append_event(...)` silent-drop pattern (V5.5 P0
    /// remediation). Drained by `take_warnings()` after `execute`
    /// returns so the daemon-side launcher can attach them to
    /// `RuntimeResult.warnings`.
    ///
    /// `Mutex` interior mutability lets the emitter (`record_callback_warning`)
    /// run with `&self`, which keeps `execute` non-mutable and
    /// avoids fighting the long-lived `&self.graph.config` borrow
    /// taken at the top of the run loop. The lock is held for a
    /// single push and never across an `await`.
    warnings: Mutex<Vec<String>>,
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
    pub hook_list: &'a [Value],
    pub inputs: &'a Value,
}

struct CommitTerminalInput<'a> {
    pub graph_run_id: &'a str,
    pub steps: u32,
    pub state: &'a mut Value,
    pub suppressed_errors: &'a mut Vec<ErrorRecord>,
    pub base_status: &'a str,
    pub error: Option<&'a str>,
    pub guard: &'a mut RunGuard,
    pub hook_list: &'a [Value],
    pub current_node_id: &'a str,
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
        }
    }

    /// Drain the accumulated callback-drift warnings. Called by the
    /// graph-runtime binary's `main.rs` after `execute` returns so the
    /// drift can be threaded into `RuntimeResult.warnings`. Replaces
    /// the previous silent-drop semantics where event-store rejection
    /// (or transport hiccups) were dropped on the floor with `let _ =`.
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
    pub async fn execute(
        &self,
        params: Value,
        graph_run_id: Option<String>,
    ) -> GraphResult {
        tracing::info!(
            graph_id = %self.graph.graph_id,
            version = %self.graph.version,
            file_path = ?self.graph.file_path,
            "graph loaded"
        );

        let mut guard = RunGuard { finalized: false };

        let graph_run_id = graph_run_id.unwrap_or_else(|| {
            format!("gr-{}", &lillux::cas::sha256_hex(
                format!("{}{}{}", self.graph.graph_id, lillux::time::timestamp_millis(), rand::random::<u32>()).as_bytes()
            )[..12])
        });

        let validation = analyze_graph(&self.graph);
        if !validation.errors.is_empty() {
            let result = GraphResult {
                success: false,
                graph_id: self.graph.graph_id.clone(),
                graph_run_id,
                status: "invalid".into(),
                steps: 0,
                state: json!({}),
                result: None,
                errors_suppressed: None,
                errors: None,
                error: Some(validation.errors.join("; ")),
            };
            let r = self.client.finalize_thread("failed").await;
            self.record_callback_warning("finalize_thread", r.map(|_| ()));
            guard.finalized = true;
            return result;
        }

        // D16: the daemon enforces capabilities at the callback boundary.
        // The walker does NOT self-police. One source of truth, one gate.
        // graph_permissions composer). The daemon enforces caps at the
        // callback boundary — the walker does NOT self-police. One
        // source of truth, one gate (the daemon).

        let exec_ctx = context::execution_context_from_envelope(
            params.get("parent_thread_id").and_then(|v| v.as_str()).map(String::from),
            params.get("depth").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            json!({}),
        );

        let r = self.client.mark_running().await;
        self.record_callback_warning("mark_running", r.map(|_| ()));

        let cfg = &self.graph.config;
        let inputs = params.get("inputs").cloned().unwrap_or(json!({}));
        let mut state = json!({});

        if let Some(defaults) = params.get("inject_state") {
            merge_into(&mut state, defaults);
        }

        let mut current = cfg.start.clone();
        let mut step: u32 = 0;
        let mut suppressed_errors: Vec<ErrorRecord> = Vec::new();
        let mut receipts: Vec<NodeReceipt> = Vec::new();
        let cache = NodeCache::new(&self.graph.graph_id);

        let hook_list: Vec<Value> = self.graph.config.hooks.clone().unwrap_or_default();

        // Resume state injected by main.rs (from CheckpointWriter or replay fallback).
        // No silent cold-start when RYE_RESUME=1 — main.rs handles that.
        if let Some(resume_val) = params.get("resume_state") {
            if let Some(node) = resume_val.get("current_node").and_then(|v| v.as_str()) {
                current = node.to_string();
                step = resume_val.get("step_count").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                state = resume_val.get("state").cloned().unwrap_or(json!({}));
                tracing::info!(
                    node = %current,
                    step,
                    "resuming graph from injected state"
                );
            }
        }

        // Emit graph_started event (before the loop — not per-step).
        {
            let hook_ctx = hooks::HookContext {
                graph_id: &self.graph.graph_id,
                graph_run_id: &graph_run_id,
                thread_id: &self.thread_id,
                step: 0,
                current_node: &cfg.start,
                state: &state,
            };
            hooks::fire_hook(&hook_list, "graph_started", &hook_ctx);
            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphStarted,
                    json!({
                        "graph_id": self.graph.graph_id,
                        "graph_run_id": &graph_run_id,
                    }),
                )
                .await;
            self.record_callback_warning("graph_started", r);
        }

        // ── F3 main loop: run_node_body → commit_step ───────────
        // Every iteration produces exactly one StepOutcome and routes
        // through commit_step. ALL persistence happens there.
        while step < cfg.max_steps {
            let node = match cfg.nodes.get(&current) {
                Some(n) => n,
                None => {
                    // Node not found is a terminal error — route through
                    // commit_step so it gets proper lifecycle.
                    let outcome = StepOutcome::Terminal {
                        status: "error",
                        error: Some(format!("node '{current}' not found")),
                    };
                     match self.commit_step(
                        CommitStepInput {
                            graph_run_id: &graph_run_id,
                            step,
                            current: &current,
                            state: &mut state,
                            receipts: &mut receipts,
                            suppressed_errors: &mut suppressed_errors,
                            outcome,
                            guard: &mut guard,
                            hook_list: &hook_list,
                            inputs: &inputs,
                        },
                    ).await {
                        CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
                        CommitResult::Terminate(result) => return result,
                    }
                }
            };

            let outcome = self.run_node_body(
                RunNodeBodyContext {
                    current: &current,
                    node,
                    cfg,
                    step,
                    state: &state,
                    inputs: &inputs,
                    exec_ctx: &exec_ctx,
                    cache: &cache,
                    graph_run_id: &graph_run_id,
                },
            ).await;

            match self.commit_step(
                CommitStepInput {
                    graph_run_id: &graph_run_id,
                    step,
                    current: &current,
                    state: &mut state,
                    receipts: &mut receipts,
                    suppressed_errors: &mut suppressed_errors,
                    outcome,
                    guard: &mut guard,
                    hook_list: &hook_list,
                    inputs: &inputs,
                },
            ).await {
                CommitResult::Advance { next_node, next_step } => {
                    current = next_node;
                    step = next_step;
                }
                CommitResult::Terminate(result) => return result,
            }
        }

        // Max steps exceeded — terminal via commit_step.
        let outcome = StepOutcome::Terminal {
            status: "max_steps_exceeded",
            error: Some(format!("exceeded max_steps ({})", cfg.max_steps)),
        };
        match self.commit_step(
            CommitStepInput {
                graph_run_id: &graph_run_id,
                step,
                current: "",
                state: &mut state,
                receipts: &mut receipts,
                suppressed_errors: &mut suppressed_errors,
                outcome,
                guard: &mut guard,
                hook_list: &hook_list,
                inputs: &inputs,
            },
        ).await {
            CommitResult::Advance { .. } => unreachable!("Terminal always terminates"),
            CommitResult::Terminate(result) => result,
        }
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
        } = ctx;
        let start = Instant::now();

        match node.node_type {
            NodeType::Return => {
                StepOutcome::Terminal {
                    status: "completed",
                    error: None,
                }
            }

            NodeType::Gate => {
                // Gate: evaluate conditions and pick a branch target.
                let target = edges::evaluate_next(node, state, inputs);
                StepOutcome::GateTaken { target }
            }

            NodeType::Foreach => {
                let over_expr = node.over.as_deref().unwrap_or("${state.items}");
                let ctx = WalkContext {
                    state: state.clone(),
                    inputs: inputs.clone(),
                    result: None,
                };
                let over_val = ryeos_runtime::interpolate(
                    &Value::String(over_expr.to_string()),
                    &ctx.as_context(),
                )
                .unwrap_or(Value::Array(vec![]));

                let items = match over_val {
                    Value::Array(arr) => arr,
                    Value::String(s) => {
                        if s.contains(',') {
                            s.split(',').map(|x| Value::String(x.trim().to_string())).collect()
                        } else {
                            vec![Value::String(s)]
                        }
                    }
                    other => vec![other],
                };

                let var = node.foreach_var().to_string();
                let parallel = node.parallel;

                let results = if parallel {
                    foreach::run_foreach_parallel(
                        foreach::ForeachContext {
                            items: &items, var: &var, node,
                            thread_id: &self.thread_id, project_path: &self.project_path,
                            client: &self.client, exec_ctx: Some(exec_ctx),
                        },
                        state, inputs,
                        self.client.clone(), Arc::new(exec_ctx.clone()),
                    ).await
                } else {
                    foreach::run_foreach_sequential(
                        foreach::ForeachContext {
                            items: &items, var: &var, node,
                            thread_id: &self.thread_id, project_path: &self.project_path,
                            client: &self.client, exec_ctx: Some(exec_ctx),
                        },
                        &mut state.clone(), inputs,
                    ).await
                };

                let next = edges::evaluate_next(node, state, inputs);
                StepOutcome::ForeachDone {
                    results,
                    collect_key: node.collect.clone(),
                    var_name: var,
                    next,
                }
            }

            NodeType::Action => {
                self.run_action_body(RunNodeBodyContext {
                    current, node, cfg, step, state, inputs, exec_ctx, cache, graph_run_id,
                }, start).await
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
            step: _step,
            state,
            inputs,
            exec_ctx,
            cache,
            graph_run_id: _graph_run_id,
        } = ctx;
        let action = match &node.action {
            Some(a) => a.clone(),
            None => {
                // Action node with no action — treat as terminal.
                let next = edges::evaluate_next(node, state, inputs);
                return match next {
                    Some(n) => StepOutcome::ActionOk {
                        item_id: String::new(),
                        result: json!({}),
                        next: Some(n),
                        cache_hit: false,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    },
                    None => StepOutcome::Terminal {
                        status: "completed",
                        error: None,
                    },
                };
            }
        };

        let item_id = action.get("item_id")
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
        };

        let interpolated_action = ryeos_runtime::interpolate_action(
            &action, &ctx.as_context(),
        )
            .unwrap_or(action.clone());

        let stripped_action = strip_none_values(&interpolated_action);

        // Env preflight
        if let Err(env_err) = env_preflight::check_env_requires(
            &self.graph.config.env_requires,
            &node.env_requires,
        ) {
            let err_msg = format!("env preflight failed: {env_err}");
            return StepOutcome::DispatchHardError {
                item_id: Some(item_id),
                error: err_msg,
                next_on_error: resolve_next_on_error(node, cfg),
                elapsed_ms: elapsed,
            };
        }

        // Dispatch
        let mut cache_hit = false;
        let result = if node.is_cacheable() {
            let cache_key = compute_cache_key(
                &self.graph.graph_id,
                current,
                &stripped_action,
            );
            if let Some(cached) = cache.lookup(&cache_key) {
                cache_hit = true;
                Some(cached)
            } else {
                let res = dispatch::dispatch_action(
                    &self.client,
                    &stripped_action,
                    &self.thread_id,
                    &self.project_path,
                    Some(exec_ctx),
                ).await;
                if let Ok(ref val) = res {
                    let is_error = val.get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s == "error")
                        .unwrap_or(false);
                    if !is_error {
                        cache.store(&cache_key, val);
                    }
                }
                res.ok()
            }
        } else {
            dispatch::dispatch_action(
                &self.client,
                &stripped_action,
                &self.thread_id,
                &self.project_path,
                Some(exec_ctx),
            ).await.ok()
        };

        let elapsed = start.elapsed().as_millis() as u64;

        match result {
            None => {
                StepOutcome::DispatchHardError {
                    item_id: Some(item_id),
                    error: "dispatch failed".to_string(),
                    next_on_error: resolve_next_on_error(node, cfg),
                    elapsed_ms: elapsed,
                }
            }
            Some(val) => {
                let is_error = val.get("status")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "error")
                    .unwrap_or(false);

                if is_error {
                    let err_str = val.get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("dispatch returned error status")
                        .to_string();
                    StepOutcome::LeafSoftError {
                        item_id,
                        error: err_str,
                        next_on_error: resolve_next_on_error(node, cfg),
                        elapsed_ms: elapsed,
                    }
                } else {
                    let next = edges::evaluate_next_with_result(node, state, inputs, &val);
                    StepOutcome::ActionOk {
                        item_id,
                        result: val,
                        next,
                        cache_hit,
                        elapsed_ms: elapsed,
                    }
                }
            }
        }
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
            hook_list,
            inputs,
        } = input;
        match outcome {
            StepOutcome::Terminal { status, error } => {
                self.commit_terminal(
                    CommitTerminalInput {
                        graph_run_id,
                        steps: step,
                        state,
                        suppressed_errors,
                        base_status: status,
                        error: error.as_deref(),
                        guard,
                        hook_list,
                        current_node_id: current,
                    },
                ).await
            }

            StepOutcome::GateTaken { target } => {
                // Gate lifecycle: graph_step_started → graph_branch_taken → graph_step_completed → checkpoint
                self.emit_graph_step_started(graph_run_id, step, current).await;
                self.emit_graph_branch_taken(graph_run_id, step, current, target.as_deref()).await;
                self.emit_graph_step_completed(graph_run_id, step, current, "ok", None).await;

                match target {
                    Some(next_node) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            &next_node,
                            next_step,
                            state,
                            guard,
                            hook_list,
                        ).await
                    }
                    None => {
                        self.commit_terminal(
                            CommitTerminalInput {
                                graph_run_id,
                                steps: step + 1,
                                state,
                                suppressed_errors,
                                base_status: "completed",
                                error: None,
                                guard,
                                hook_list,
                                current_node_id: current,
                            },
                        ).await
                    }
                }
            }

            StepOutcome::ForeachDone {
                ref results,
                ref collect_key,
                ref var_name,
                ref next,
            } => {
                // Foreach lifecycle: graph_step_started → (per-iteration
                // graph_foreach_iteration events) → graph_step_completed →
                // checkpoint
                self.emit_graph_step_started(graph_run_id, step, current).await;

                // Emit per-iteration events from the aggregated results.
                // Each result corresponds to one item that was iterated over.
                for (i, _result) in results.iter().enumerate() {
                    let r = self
                        .client
                        .append_runtime_event(
                            RuntimeEventType::GraphForeachIteration,
                            json!({
                                "graph_run_id": graph_run_id,
                                "node": current,
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
                // Remove the iteration variable from state.
                if let Some(obj) = state.as_object_mut() {
                    obj.remove(var_name);
                }

                self.emit_graph_step_completed(
                    graph_run_id,
                    step,
                    current,
                    "ok",
                    None,
                ).await;

                match next {
                    Some(next_node) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            next_node,
                            next_step,
                            state,
                            guard,
                            hook_list,
                        ).await
                    }
                    None => {
                        self.commit_terminal(
                            CommitTerminalInput {
                                graph_run_id,
                                steps: step + 1,
                                state,
                                suppressed_errors,
                                base_status: "completed",
                                error: None,
                                guard,
                                hook_list,
                                current_node_id: current,
                            },
                        ).await
                    }
                }
            }

            StepOutcome::ActionOk {
                ref item_id,
                ref result,
                ref next,
                cache_hit,
                elapsed_ms,
            } => {
                // R3 fence order:
                // graph_step_started → tool_call_start → (dispatch in run_node_body) →
                // tool_call_result → state mutation → receipt → graph_step_completed → checkpoint
                self.emit_graph_step_started(graph_run_id, step, current).await;
                self.emit_tool_call_start(graph_run_id, step, current, item_id).await;
                self.emit_tool_call_result(graph_run_id, step, current, item_id, "ok").await;

                // State mutation
                if let Some(node) = self.graph.config.nodes.get(current) {
                    if let Some(ref assign) = node.assign {
                        let assign_ctx = WalkContext {
                            state: state.clone(),
                            inputs: inputs.clone(),
                            result: Some(result.clone()),
                        };
                        let interpolated = ryeos_runtime::interpolate(
                            assign, &assign_ctx.as_context(),
                        )
                            .unwrap_or(assign.clone());
                        merge_into(state, &interpolated);
                    }
                }

                // Receipt
                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    result_hash: None,
                    cache_hit,
                    elapsed_ms,
                    error: None,
                });
                let r = persistence::write_node_receipt(
                    &self.client,
                    graph_run_id,
                    receipts.last().unwrap(),
                ).await;
                self.record_callback_warning("write_node_receipt", r.map(|_| ()));

                self.emit_graph_step_completed(graph_run_id, step, current, "ok", None).await;

                match next {
                    Some(next_node) => {
                        let next_step = step + 1;
                        self.write_checkpoint_or_error(
                            graph_run_id,
                            next_node,
                            next_step,
                            state,
                            guard,
                            hook_list,
                        ).await
                    }
                    None => {
                        self.commit_terminal(
                            CommitTerminalInput {
                                graph_run_id,
                                steps: step + 1,
                                state,
                                suppressed_errors,
                                base_status: "completed",
                                error: None,
                                guard,
                                hook_list,
                                current_node_id: current,
                            },
                        ).await
                    }
                }
            }

            StepOutcome::LeafSoftError {
                ref item_id,
                ref error,
                ref next_on_error,
                elapsed_ms,
            } => {
                // Soft error: dispatch succeeded but leaf returned error.
                // graph_step_started → tool_call_start → tool_call_result(error) → graph_step_completed(error) → [redirect/continue/fail]
                self.emit_graph_step_started(graph_run_id, step, current).await;
                self.emit_tool_call_start(graph_run_id, step, current, item_id).await;
                self.emit_tool_call_result(graph_run_id, step, current, item_id, "error").await;

                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    result_hash: None,
                    cache_hit: false,
                    elapsed_ms,
                    error: Some(error.clone()),
                });

                self.emit_graph_step_completed(graph_run_id, step, current, "error", Some(error)).await;

                match next_on_error {
                    NextOnError::Redirect(target) => {
                        CommitResult::Advance {
                            next_node: target.clone(),
                            next_step: step + 1,
                        }
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
                        ) {
                            Some(next_node) => {
                                let next_step = step + 1;
                                // Checkpoint on continue-advance too
                                self.write_checkpoint_or_error(
                                    graph_run_id,
                                    &next_node,
                                    next_step,
                                    state,
                                    guard,
                                    hook_list,
                                ).await
                            }
                            None => {
                                self.commit_terminal(
                                    CommitTerminalInput {
                                        graph_run_id,
                                        steps: step + 1,
                                        state,
                                        suppressed_errors,
                                        base_status: "completed",
                                        error: None,
                                        guard,
                                        hook_list,
                                        current_node_id: current,
                                    },
                                ).await
                            }
                        }
                    }
                    NextOnError::PolicyFail => {
                        self.commit_terminal(
                            CommitTerminalInput {
                                graph_run_id,
                                steps: step,
                                state,
                                suppressed_errors,
                                base_status: "error",
                                error: Some(&format!("node '{}' failed: {}", current, error)),
                                guard,
                                hook_list,
                                current_node_id: current,
                            },
                        ).await
                    }
                }
            }

            StepOutcome::DispatchHardError {
                item_id,
                ref error,
                ref next_on_error,
                elapsed_ms,
            } => {
                // Hard error: dispatch failed before leaf returned.
                // graph_step_started → tool_call_start → tool_call_result(dispatch_failed) → graph_step_completed(error)
                let item_str = item_id.as_deref().unwrap_or("");
                self.emit_graph_step_started(graph_run_id, step, current).await;
                if !item_str.is_empty() {
                    self.emit_tool_call_start(graph_run_id, step, current, item_str).await;
                }
                if !item_str.is_empty() {
                    self.emit_tool_call_result(graph_run_id, step, current, item_str, "dispatch_failed").await;
                }

                receipts.push(NodeReceipt {
                    node: current.to_string(),
                    step,
                    result_hash: None,
                    cache_hit: false,
                    elapsed_ms,
                    error: Some(error.clone()),
                });

                self.emit_graph_step_completed(graph_run_id, step, current, "error", Some(error)).await;

                match next_on_error {
                    NextOnError::Redirect(target) => {
                        CommitResult::Advance {
                            next_node: target.clone(),
                            next_step: step + 1,
                        }
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
                        ) {
                            Some(next_node) => {
                                let next_step = step + 1;
                                self.write_checkpoint_or_error(
                                    graph_run_id,
                                    &next_node,
                                    next_step,
                                    state,
                                    guard,
                                    hook_list,
                                ).await
                            }
                            None => {
                                self.commit_terminal(
                                    CommitTerminalInput {
                                        graph_run_id,
                                        steps: step + 1,
                                        state,
                                        suppressed_errors,
                                        base_status: "completed",
                                        error: None,
                                        guard,
                                        hook_list,
                                        current_node_id: current,
                                    },
                                ).await
                            }
                        }
                    }
                    NextOnError::PolicyFail => {
                        self.commit_terminal(
                            CommitTerminalInput {
                                graph_run_id,
                                steps: step,
                                state,
                                suppressed_errors,
                                base_status: "error",
                                error: Some(&format!("node '{}' failed: {}", current, error)),
                                guard,
                                hook_list,
                                current_node_id: current,
                            },
                        ).await
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
            hook_list,
            current_node_id,
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
            _ => (false, "error".to_string()),
        };

        // Output: ONLY populated when a return node declares an explicit
        // `output:` template. Otherwise `result` stays `None` and
        // consumers read from `state` — eliminates the historical
        // "GraphResult.state == GraphResult.result" duplication that
        // surfaced as `body.result.result.result == body.result.result.state`
        // on the wire (smell flagged during phase 4c review).
        //
        // G3: use the current cursor (deterministic) instead of
        // nodes.values().find() which iterates HashMap in random order.
        let output: Option<Value> = if success && base_status == "completed" {
            self.graph
                .config
                .nodes
                .get(current_node_id)
                .filter(|n| n.node_type == NodeType::Return)
                .and_then(|n| n.output.as_ref())
                .map(|tpl| {
                    let ctx = WalkContext {
                        state: state.clone(),
                        inputs: Value::Object(Default::default()),
                        result: None,
                    };
                    ryeos_runtime::interpolate(
                        &Value::String(tpl.clone()),
                        &ctx.as_context(),
                    )
                        .unwrap_or(Value::String(tpl.clone()))
                })
        } else {
            None
        };

        let graph_result = GraphResult {
            success,
            graph_id: self.graph.graph_id.clone(),
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
            error: error.map(String::from),
        };

        // Emit GraphCompleted event.
        {
            let hook_ctx = hooks::HookContext {
                graph_id: &self.graph.graph_id,
                graph_run_id,
                thread_id: &self.thread_id,
                step: steps,
                current_node: "",
                state,
            };
            hooks::fire_hook(hook_list, "graph_completed", &hook_ctx);
            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphCompleted,
                    json!({
                        "graph_run_id": graph_run_id,
                        "status": &status,
                        "steps": steps,
                    }),
                )
                .await;
            self.record_callback_warning("graph_completed", r);
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
        let r = self.client.publish_artifact(json!({
            "artifact_type": "graph_transcript",
            "uri": format!("graph://{}/runs/{}", self.graph.graph_id, graph_run_id),
        })).await;
        self.record_callback_warning("publish_artifact", r.map(|_| ()));

        // Finalize thread.
        let thread_status = if success { "completed" } else { "failed" };
        let r = self.client.finalize_thread(thread_status).await;
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
        guard: &mut RunGuard,
        _hook_list: &[Value],
    ) -> CommitResult {
        if let Err(e) = self
            .write_checkpoint(graph_run_id, next_node, next_step, state)
            .await
        {
            // Checkpoint failure is a hard error — resume correctness
            // is contractual (R4).
            let graph_result = GraphResult {
                success: false,
                graph_id: self.graph.graph_id.clone(),
                graph_run_id: graph_run_id.to_string(),
                status: "error".into(),
                steps: next_step,
                state: state.clone(),
                result: None,
                errors_suppressed: None,
                errors: None,
                error: Some(format!("checkpoint write failed: {e}")),
            };

            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphCompleted,
                    json!({
                        "graph_run_id": graph_run_id,
                        "status": "error",
                        "steps": next_step,
                    }),
                )
                .await;
            self.record_callback_warning("graph_completed", r);

            let r = self.client.finalize_thread("failed").await;
            self.record_callback_warning("finalize_thread", r.map(|_| ()));
            guard.finalized = true;

            return CommitResult::Terminate(graph_result);
        }

        CommitResult::Advance {
            next_node: next_node.to_string(),
            next_step,
        }
    }

    // ── Event emission helpers (all route through record_callback_warning) ──

    async fn emit_graph_step_started(&self, graph_run_id: &str, step: u32, current: &str) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::GraphStepStarted,
                json!({
                    "graph_run_id": graph_run_id,
                    "node": current,
                    "step": step,
                }),
            )
            .await;
        self.record_callback_warning("graph_step_started", r);
    }

    async fn emit_tool_call_start(&self, graph_run_id: &str, step: u32, current: &str, item_id: &str) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::ToolCallStart,
                json!({
                    "graph_run_id": graph_run_id,
                    "node": current,
                    "step": step,
                    "item_id": item_id,
                }),
            )
            .await;
        self.record_callback_warning("tool_call_start", r);
    }

    async fn emit_tool_call_result(&self, graph_run_id: &str, step: u32, current: &str, item_id: &str, status: &str) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::ToolCallResult,
                json!({
                    "graph_run_id": graph_run_id,
                    "node": current,
                    "step": step,
                    "item_id": item_id,
                    "status": status,
                }),
            )
            .await;
        self.record_callback_warning("tool_call_result", r);
    }

    async fn emit_graph_step_completed(&self, graph_run_id: &str, step: u32, current: &str, status: &str, error: Option<&str>) {
        {
            let hook_ctx = hooks::HookContext {
                graph_id: &self.graph.graph_id,
                graph_run_id,
                thread_id: &self.thread_id,
                step,
                current_node: current,
                state: &json!({}),
            };
            hooks::fire_hook(
                &[],
                "graph_step_completed",
                &hook_ctx,
            );
        }
        let mut payload = json!({
            "graph_run_id": graph_run_id,
            "node": current,
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

    async fn emit_graph_branch_taken(&self, graph_run_id: &str, step: u32, current: &str, target: Option<&str>) {
        if let Some(t) = target {
            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphBranchTaken,
                    json!({
                        "graph_run_id": graph_run_id,
                        "node": current,
                        "step": step,
                        "target": t,
                    }),
                )
                .await;
            self.record_callback_warning("graph_branch_taken", r);
        }
    }

    /// Write a local checkpoint using the daemon-provided CheckpointWriter.
    async fn write_checkpoint(
        &self,
        graph_run_id: &str,
        next_node: &str,
        next_step: u32,
        state: &Value,
    ) -> anyhow::Result<()> {
        let Some(writer) = &self.checkpoint else { return Ok(()); };

        writer.write(&json!({
            "graph_run_id": graph_run_id,
            "current_node": next_node,
            "step_count": next_step,
            "state": state,
            "written_at": lillux::time::iso8601_now(),
        }))
    }
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
        Value::Array(arr) => {
            Value::Array(arr.iter().map(strip_none_values).collect())
        }
        other => other.clone(),
    }
}

fn compute_cache_key(graph_id: &str, node_name: &str, action: &Value) -> String {
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
    }

    impl MockClient {
        fn new(results: Vec<Value>) -> Self {
            Self {
                results: Mutex::new(results),
            }
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
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_event(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({"events": []})) }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    fn make_callback(results: Vec<Value>) -> CallbackClient {
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> = Arc::new(MockClient::new(results));
        CallbackClient::from_inner(inner, "thread-test", "/tmp/test-project")
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
            over: None,
            r#as: None,
            collect: None,
            parallel: false,
            max_concurrency: None,
            output: None,
            env_requires: Vec::new(),
        }
    }

    fn make_test_graph_config() -> GraphConfig {
        GraphConfig {
            start: "x".to_string(),
            max_steps: 100,
            on_error: ErrorMode::Fail,
            nodes: HashMap::new(),
            hooks: None,
            config_schema: None,
            env_requires: Vec::new(),
            state: None,
            max_concurrency: None,
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
        let w = make_walker(graph, vec![
            json!({}),
            json!({}),
            json!({}),
        ]);
        let result = w.execute(json!({}), None).await;
        assert!(!result.success);
        assert_eq!(result.status, "max_steps_exceeded");
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
        let w = make_walker(graph, vec![
            json!({"value": "a"}),
            json!({"value": "b"}),
            json!({"value": "c"}),
        ]);
        let result = w.execute(json!({"inject_state": {"items": ["a", "b", "c"]}}), None).await;
        assert!(result.success);
        let results = result.state.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 3);
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
        let w = make_walker(graph, vec![
            json!({"status": "error", "error": "forced failure"}),
        ]);
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

    // ── V5.5 P0 #3: warning accumulator ─────────────────────────────
    //
    // `record_callback_warning` MUST push exactly one labelled string per
    // failed callback append, and `take_warnings()` MUST drain the
    // buffer atomically. These two together replace the silent
    // `let _ = ...` drops the V5.4 walker had at every event-emit
    // site. The wire-level drift the daemon's `RuntimeResult.warnings`
    // field surfaces is what these tests pin.

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
            next: Some("done".to_string()),
            cache_hit: false,
            elapsed_ms: 42,
        };
        match outcome {
            StepOutcome::ActionOk { ref item_id, ref next, elapsed_ms, .. } => {
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
        };
        match outcome {
            StepOutcome::LeafSoftError { ref error, ref next_on_error, .. } => {
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
        };
        match outcome {
            StepOutcome::DispatchHardError { item_id, ref error, ref next_on_error, .. } => {
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
            next: Some("done".to_string()),
        };
        match outcome {
            StepOutcome::ForeachDone { ref next, ref collect_key, .. } => {
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
        /// Collected artifacts from publish_artifact calls.
        artifacts: Mutex<Vec<Value>>,
    }

    impl RecordingMockClient {
        fn new(dispatch_results: Vec<Value>) -> Self {
            Self {
                dispatch_results: Mutex::new(dispatch_results),
                events: Mutex::new(Vec::new()),
                finalizations: Mutex::new(Vec::new()),
                artifacts: Mutex::new(Vec::new()),
            }
        }

        fn recorded_events(&self) -> Vec<(String, String, Value, String)> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for RecordingMockClient {
        async fn dispatch_action(
            &self,
            _request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            let mut results = self.dispatch_results.lock().unwrap();
            if results.is_empty() {
                Ok(json!({"thread": {}, "result": {}}))
            } else {
                Ok(json!({"thread": {}, "result": results.remove(0)}))
            }
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, thread_id: &str, status: &str) -> Result<Value, CallbackError> {
            self.finalizations.lock().unwrap().push((thread_id.to_string(), status.to_string()));
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
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
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({"events": []})) }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, artifact: Value) -> Result<Value, CallbackError> {
            self.artifacts.lock().unwrap().push(artifact);
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    fn make_recording_callback(results: Vec<Value>) -> (CallbackClient, Arc<RecordingMockClient>) {
        let inner: Arc<RecordingMockClient> = Arc::new(RecordingMockClient::new(results));
        let client = CallbackClient::from_inner(inner.clone(), "thread-test", "/tmp/test-project");
        (client, inner)
    }

    fn make_recording_walker(graph: GraphDefinition, results: Vec<Value>, checkpoint_dir: Option<&std::path::Path>) -> (Walker, Arc<RecordingMockClient>) {
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
        let (w, recorder) = make_recording_walker(graph, vec![json!({"msg": "hello"})], Some(tmp.path()));

        let result = w.execute(json!({}), Some("gr-fence-test".to_string())).await;
        assert!(result.success);

        let events = recorder.recorded_events();
        let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

        // graph_started is emitted before the loop starts
        let idx = types.iter().position(|&t| t == "graph_started").unwrap();

        // Step 1: action node — R3 fence order
        assert_eq!(types[idx + 1], "graph_step_started", "fence: graph_step_started first");
        assert_eq!(types[idx + 2], "tool_call_start", "fence: tool_call_start second");
        assert_eq!(types[idx + 3], "tool_call_result", "fence: tool_call_result third");
        assert_eq!(types[idx + 4], "graph_step_completed", "fence: graph_step_completed fourth");

        // Return node is terminal — goes through commit_terminal directly,
        // which emits GraphCompleted but no graph_step_started for the
        // terminal step itself.
        assert_eq!(types[idx + 5], "graph_completed",
            "after step_completed, terminal emits graph_completed directly");

        // GraphCompleted must appear exactly once
        let completed_count = types.iter().filter(|&&t| t == "graph_completed").count();
        assert_eq!(completed_count, 1, "GraphCompleted must be emitted exactly once, got {completed_count}");
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
        assert!(checkpoint_file.exists(), "checkpoint file must exist after graph completes");
        let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
        let cp: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(cp["current_node"], "done", "checkpoint must point at the next cursor (done)");
        assert_eq!(cp["step_count"], 2, "checkpoint step_count must be 2 (two action steps, return is terminal)");
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

        let result = w.execute(json!({"inject_state": {"mode": "fast"}}), Some("gr-gate-test".to_string())).await;
        assert!(result.success);

        let events = recorder.recorded_events();
        let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

        // Gate lifecycle: graph_step_started → graph_branch_taken → graph_step_completed
        let step_started_idx = types.iter().position(|&t| t == "graph_step_started").unwrap();
        assert_eq!(types[step_started_idx + 1], "graph_branch_taken",
            "gate must emit graph_branch_taken after graph_step_started");
        assert_eq!(types[step_started_idx + 2], "graph_step_completed",
            "gate must emit graph_step_completed after graph_branch_taken");

        // Verify the branch target is correct
        let branch_event = events.iter().find(|(_, et, _, _)| et == "graph_branch_taken").unwrap();
        assert_eq!(branch_event.2["target"], "fast_path");

        // Checkpoint must exist pointing at the next node
        let checkpoint_file = tmp.path().join("latest.json");
        assert!(checkpoint_file.exists(), "checkpoint must exist after gate step");
        let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
        let cp: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(cp["current_node"], "fast_path");
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

        let result = w.execute(
            json!({"inject_state": {"items": ["a", "b", "c"]}}),
            Some("gr-fe-test".to_string()),
        ).await;
        assert!(result.success);

        let events = recorder.recorded_events();
        let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

        // Foreach must emit per-iteration events
        let iteration_count = types.iter().filter(|&&t| t == "graph_foreach_iteration").count();
        assert_eq!(iteration_count, 3,
            "foreach must emit exactly 3 graph_foreach_iteration events for 3 items, got {iteration_count}");

        // Foreach step emits graph_step_started + graph_step_completed.
        // The return node is terminal — commit_terminal does NOT emit
        // graph_step_started for terminal steps.
        let step_started = types.iter().filter(|&&t| t == "graph_step_started").count();
        let step_completed = types.iter().filter(|&&t| t == "graph_step_completed").count();
        assert_eq!(step_started, 1, "1 foreach step (return node is terminal, no step_started)");
        assert_eq!(step_completed, 1, "1 foreach step (return node is terminal, no step_completed)");
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
        assert_eq!(completed_ok, 1, "success path: exactly 1 GraphCompleted, got {completed_ok}");

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
            vec![json!({"status": "error", "error": "forced"})],
            None,
        );

        let result_err = w_err.execute(json!({}), Some("gr-t2".to_string())).await;
        assert!(!result_err.success);
        let events_err = recorder_err.recorded_events();
        let types_err: Vec<&str> = events_err.iter().map(|(_, et, _, _)| et.as_str()).collect();
        let completed_err = types_err.iter().filter(|&&t| t == "graph_completed").count();
        assert_eq!(completed_err, 1, "error path: exactly 1 GraphCompleted, got {completed_err}");

        // Verify the error path's GraphCompleted carries status=error
        let events_err_full = recorder_err.recorded_events();
        let gc = events_err_full.iter().find(|(_, et, _, _)| et == "graph_completed").unwrap();
        assert_eq!(gc.2["status"], "error");
    }
}
