use std::sync::Arc;
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
use crate::permissions;
use crate::validation::analyze_graph;
use rye_runtime::callback_client::CallbackClient;

pub struct Walker {
    graph: GraphDefinition,
    project_path: String,
    thread_id: String,
    client: CallbackClient,
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

impl Walker {
    pub fn new(
        graph: GraphDefinition,
        project_path: String,
        thread_id: String,
        client: CallbackClient,
    ) -> Self {
        Self {
            graph,
            project_path,
            thread_id,
            client,
        }
    }

    pub fn validate(&self) -> GraphResult {
        let result = analyze_graph(&self.graph);
        GraphResult {
            success: result.ok(),
            graph_id: self.graph.graph_id.clone(),
            graph_run_id: String::new(),
            status: if result.ok() {
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
                format!("{}{}{}", self.graph.graph_id, chrono::Utc::now().timestamp_millis(), rand::random::<u32>()).as_bytes()
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
            let _ = self.client.finalize_thread("failed").await;
            guard.finalized = true;
            return result;
        }

        let exec_ctx = context::execution_context_from_envelope(
            params.get("parent_thread_id").and_then(|v| v.as_str()).map(String::from),
            params.get("parent_capabilities").and_then(|v| v.as_array()).map(|arr| {
                arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            }),
            params.get("depth").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            vec![], // effective_caps from graph permissions
            json!({}),
        );

        // Also try the legacy resolution for CLI mode
        let exec_ctx = if exec_ctx.parent_thread_id.is_none() && exec_ctx.capabilities.is_empty() {
            context::resolve_execution_context(
                &params,
                std::path::Path::new(&self.project_path),
                &self.graph.permissions,
            )
        } else {
            exec_ctx
        };

        let _ = self.client.mark_running().await;

        let cfg = &self.graph.config;
        let inputs = params.get("inputs").cloned().unwrap_or(json!({}));
        let mut state = json!({});

        if let Some(ref defaults) = params.get("inject_state") {
            merge_into(&mut state, defaults);
        }

        let mut current = cfg.start.clone();
        let mut step: u32 = 0;
        let mut suppressed_errors: Vec<ErrorRecord> = Vec::new();
        let mut receipts: Vec<NodeReceipt> = Vec::new();
        let cache = NodeCache::new(&self.graph.graph_id);

        let hook_list: Vec<Value> = self.graph.config.hooks.clone().unwrap_or_default();

        if let Ok(Some(resume)) = crate::resume::load_resume_state(&self.project_path, &graph_run_id) {
                current = resume.current_node;
                step = resume.step_count;
                state = resume.state;
                tracing::info!(
                    node = %current,
                    step,
                    "resuming graph from checkpoint"
                );
            }

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
            let _ = self.client.append_event("graph_started", json!({
                "graph_id": self.graph.graph_id,
                "graph_run_id": &graph_run_id,
            })).await;
        }

        while step < cfg.max_steps {
            let node = match cfg.nodes.get(&current) {
                Some(n) => n,
                None => {
                    let result = GraphResult {
                        success: false,
                        graph_id: self.graph.graph_id.clone(),
                        graph_run_id,
                        status: "error".into(),
                        steps: step,
                        state,
                        result: None,
                        errors_suppressed: None,
                        errors: None,
                        error: Some(format!("node '{current}' not found")),
                    };
                    let _ = self.client.finalize_thread("failed").await;
                    guard.finalized = true;
                    return result;
                }
            };

            match node.node_type {
                NodeType::Return => {
                    let output = if let Some(ref tpl) = node.output {
                        let ctx = WalkContext {
                            state: state.clone(),
                            inputs: inputs.clone(),
                            result: None,
                        };
                        rye_runtime::interpolate(
                            &Value::String(tpl.clone()),
                            &ctx.as_context(),
                        )
                        .unwrap_or(Value::String(tpl.clone()))
                    } else {
                        state.clone()
                    };

                    let status = if suppressed_errors.is_empty() {
                        "completed".to_string()
                    } else {
                        "completed_with_errors".to_string()
                    };

                    {
                        let hook_ctx = hooks::HookContext {
                            graph_id: &self.graph.graph_id,
                            graph_run_id: &graph_run_id,
                            thread_id: &self.thread_id,
                            step,
                            current_node: &current,
                            state: &state,
                        };
                        hooks::fire_hook(&hook_list, "graph_completed", &hook_ctx);
                        let _ = self.client.append_event("graph_completed", json!({
                            "graph_run_id": &graph_run_id,
                            "status": &status,
                            "steps": step,
                        })).await;
                    }

                    let graph_result = GraphResult {
                        success: true,
                        graph_id: self.graph.graph_id.clone(),
                        graph_run_id: graph_run_id.clone(),
                        status,
                        steps: step,
                        state: state.clone(),
                        result: Some(output),
                        errors_suppressed: if suppressed_errors.is_empty() {
                            None
                        } else {
                            Some(suppressed_errors.len())
                        },
                        errors: if suppressed_errors.is_empty() {
                            None
                        } else {
                            Some(suppressed_errors)
                        },
                        error: None,
                    };

                    let _ = knowledge::write_knowledge_transcript(
                        &self.project_path,
                        &self.graph.graph_id,
                        &graph_run_id,
                        &serde_json::to_string(&graph_result).unwrap_or_default(),
                    );

                    let _ = self.client.publish_artifact(json!({
                        "artifact_type": "graph_transcript",
                        "uri": format!("graph://{}/runs/{}", self.graph.graph_id, graph_run_id),
                    })).await;

                    let _ = self.client.finalize_thread("completed").await;
                    guard.finalized = true;
                    return graph_result;
                }

                NodeType::Gate => {
                    if let Some(ref assign) = node.assign {
                        let ctx = WalkContext {
                            state: state.clone(),
                            inputs: inputs.clone(),
                            result: None,
                        };
                        let interpolated = rye_runtime::interpolate(assign, &ctx.as_context())
                            .unwrap_or(assign.clone());
                        merge_into(&mut state, &interpolated);
                    }

                    let next = edges::evaluate_next(node, &state, &inputs);
                    step += 1;
                    current = match next {
                        Some(n) => n,
                        None => {
                            return self.make_terminal_finalized(
                                &graph_run_id, step, state,
                                suppressed_errors, "completed", &mut guard,
                            ).await;
                        }
                    };
                    continue;
                }

                NodeType::Foreach => {
                    let over_expr = node.over.as_deref().unwrap_or("${state.items}");
                    let ctx = WalkContext {
                        state: state.clone(),
                        inputs: inputs.clone(),
                        result: None,
                    };
                    let over_val = rye_runtime::interpolate(
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
                    let collect_key = node.collect.clone();
                    let parallel = node.parallel;

                    let results = if parallel {
                        foreach::run_foreach_parallel(
                            &items, &var, &node, &state, &inputs,
                            &self.thread_id, &self.project_path,
                            self.client.clone(), Arc::new(exec_ctx.clone()),
                        ).await
                    } else {
                        foreach::run_foreach_sequential(
                            &items, &var, &node, &mut state, &inputs,
                            &self.thread_id, &self.project_path,
                            &self.client, Some(&exec_ctx),
                        ).await
                    };

                    if let Some(ref key) = collect_key {
                        state.as_object_mut()
                            .unwrap_or(&mut serde_json::Map::new())
                            .insert(key.clone(), Value::Array(results));
                    }

                    state.as_object_mut()
                        .unwrap_or(&mut serde_json::Map::new())
                        .remove(&var);

                    let next = edges::evaluate_next(node, &state, &inputs);
                    step += 1;
                    current = match next {
                        Some(n) => n,
                        None => {
                            return self.make_terminal_finalized(
                                &graph_run_id, step, state,
                                suppressed_errors, "completed", &mut guard,
                            ).await;
                        }
                    };
                    continue;
                }

                NodeType::Action => {}
            }

            let start = Instant::now();
            let mut cache_hit = false;

            let action = match &node.action {
                Some(a) => a.clone(),
                None => {
                    step += 1;
                    let next = edges::evaluate_next(node, &state, &inputs);
                    current = match next {
                        Some(n) => n,
                        None => {
                            return self.make_terminal_finalized(
                                &graph_run_id, step, state,
                                suppressed_errors, "completed", &mut guard,
                            ).await;
                        }
                    };
                    continue;
                }
            };

            let item_id = action.get("item_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if let Err(err_msg) = permissions::check_permission(
                &exec_ctx.capabilities,
                item_id,
            ) {
                let elapsed = start.elapsed().as_millis() as u64;
                if let Some(ref on_err_target) = node.on_error {
                    receipts.push(NodeReceipt {
                        node: current.clone(),
                        step,
                        result_hash: None,
                        cache_hit: false,
                        elapsed_ms: elapsed,
                        error: Some(err_msg.clone()),
                    });
                    step += 1;
                    current = on_err_target.clone();
                    continue;
                }
                match cfg.on_error {
                    ErrorMode::Continue => {
                        suppressed_errors.push(ErrorRecord {
                            step,
                            node: current.clone(),
                            error: err_msg.clone(),
                        });
                        receipts.push(NodeReceipt {
                            node: current.clone(),
                            step,
                            result_hash: None,
                            cache_hit: false,
                            elapsed_ms: elapsed,
                            error: Some(err_msg),
                        });
                        let next = edges::evaluate_next(node, &state, &inputs);
                        step += 1;
                        current = match next {
                            Some(n) => n,
                            None => {
                                return self.make_terminal_finalized(
                                    &graph_run_id, step, state,
                                    suppressed_errors, "completed", &mut guard,
                                ).await;
                            }
                        };
                        continue;
                    }
                    ErrorMode::Fail => {
                        return self.make_error_finalized(
                            &graph_run_id, step, state,
                            &format!("node '{}' failed: {}", current, err_msg),
                            &mut guard,
                        ).await;
                    }
                }
            }

            let ctx = WalkContext {
                state: state.clone(),
                inputs: inputs.clone(),
                result: None,
            };

            let interpolated_action = rye_runtime::interpolate_action(
                &action, &ctx.as_context(),
            )
            .unwrap_or(action.clone());

            let stripped_action = strip_none_values(&interpolated_action);

            if let Err(env_err) = env_preflight::check_env_requires(
                &self.graph.config.env_requires,
                &node.env_requires,
            ) {
                let elapsed = start.elapsed().as_millis() as u64;
                let err_msg = format!("env preflight failed: {env_err}");
                if let Some(ref on_err_target) = node.on_error {
                    receipts.push(NodeReceipt {
                        node: current.clone(),
                        step,
                        result_hash: None,
                        cache_hit: false,
                        elapsed_ms: elapsed,
                        error: Some(err_msg.clone()),
                    });
                    step += 1;
                    current = on_err_target.clone();
                    continue;
                }
                match cfg.on_error {
                    ErrorMode::Continue => {
                        suppressed_errors.push(ErrorRecord {
                            step,
                            node: current.clone(),
                            error: err_msg.clone(),
                        });
                        receipts.push(NodeReceipt {
                            node: current.clone(),
                            step,
                            result_hash: None,
                            cache_hit: false,
                            elapsed_ms: elapsed,
                            error: Some(err_msg),
                        });
                        let next = edges::evaluate_next(node, &state, &inputs);
                        step += 1;
                        current = match next {
                            Some(n) => n,
                            None => {
                                return self.make_terminal_finalized(
                                    &graph_run_id, step, state,
                                    suppressed_errors, "completed", &mut guard,
                                ).await;
                            }
                        };
                        continue;
                    }
                    ErrorMode::Fail => {
                        return self.make_error_finalized(
                            &graph_run_id, step, state,
                            &err_msg, &mut guard,
                        ).await;
                    }
                }
            }

            let result = if node.is_cacheable() {
                let cache_key = compute_cache_key(
                    &self.graph.graph_id,
                    &current,
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
                        Some(&exec_ctx),
                    ).await;
                    if let Ok(ref val) = res {
                        let unwrapped = dispatch::unwrap_result(val);
                        let is_error = unwrapped.get("status")
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
                    Some(&exec_ctx),
                ).await.ok()
            };

            let elapsed = start.elapsed().as_millis() as u64;

            match result {
                Some(val) => {
                    let unwrapped = dispatch::unwrap_result(&val);

                    let is_error = unwrapped.get("status")
                        .and_then(|s| s.as_str())
                        .map(|s| s == "error")
                        .unwrap_or(false);

                    if is_error {
                        let err_str = unwrapped.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("dispatch returned error status")
                            .to_string();
                        let error_msg = Some(err_str.clone());

                        {
                            let hook_ctx = hooks::HookContext {
                                graph_id: &self.graph.graph_id,
                                graph_run_id: &graph_run_id,
                                thread_id: &self.thread_id,
                                step,
                                current_node: &current,
                                state: &state,
                            };
                            hooks::fire_hook(&hook_list, "error", &hook_ctx);
                            let _ = self.client.append_event("error", json!({
                                "node": &current,
                                "step": step,
                                "error": &err_str,
                            })).await;
                        }

                        if let Some(ref on_err_target) = node.on_error {
                            receipts.push(NodeReceipt {
                                node: current.clone(),
                                step,
                                result_hash: None,
                                cache_hit: false,
                                elapsed_ms: elapsed,
                                error: Some(err_str),
                            });
                            step += 1;
                            current = on_err_target.clone();
                            continue;
                        }

                        match cfg.on_error {
                            ErrorMode::Continue => {
                                suppressed_errors.push(ErrorRecord {
                                    step,
                                    node: current.clone(),
                                    error: err_str,
                                });
                                receipts.push(NodeReceipt {
                                    node: current.clone(),
                                    step,
                                    result_hash: None,
                                    cache_hit: false,
                                    elapsed_ms: elapsed,
                                    error: error_msg,
                                });
                                let next = edges::evaluate_next(node, &state, &inputs);
                                step += 1;
                                current = match next {
                                    Some(n) => n,
                                    None => {
                                        return self.make_terminal_finalized(
                                            &graph_run_id, step, state,
                                            suppressed_errors, "completed", &mut guard,
                                        ).await;
                                    }
                                };
                            }
                            ErrorMode::Fail => {
                                return self.make_error_finalized(
                                    &graph_run_id, step, state,
                                    &format!("node '{}' failed: {}", current, err_str),
                                    &mut guard,
                                ).await;
                            }
                        }
                    } else {
                        if let Some(ref assign) = node.assign {
                            let assign_ctx = WalkContext {
                                state: state.clone(),
                                inputs: inputs.clone(),
                                result: Some(unwrapped.clone()),
                            };
                            let interpolated = rye_runtime::interpolate(
                                assign, &assign_ctx.as_context(),
                            )
                            .unwrap_or(assign.clone());
                            merge_into(&mut state, &interpolated);
                        }

                        receipts.push(NodeReceipt {
                            node: current.clone(),
                            step,
                            result_hash: None,
                            cache_hit,
                            elapsed_ms: elapsed,
                            error: None,
                        });

                        let _ = persistence::write_node_receipt(
                            &self.project_path,
                            &graph_run_id,
                            receipts.last().unwrap(),
                        );

                        let _ = persistence::write_checkpoint(
                            &self.project_path,
                            &graph_run_id,
                            &current,
                            step,
                            &state,
                        );

                        {
                            let hook_ctx = hooks::HookContext {
                                graph_id: &self.graph.graph_id,
                                graph_run_id: &graph_run_id,
                                thread_id: &self.thread_id,
                                step,
                                current_node: &current,
                                state: &state,
                            };
                            hooks::fire_hook(&hook_list, "after_step", &hook_ctx);
                            let _ = self.client.append_event("after_step", json!({
                                "node": &current,
                                "step": step,
                            })).await;
                        }

                        let next = edges::evaluate_next_with_result(node, &state, &inputs, &unwrapped);
                        step += 1;
                        current = match next {
                            Some(n) => n,
                            None => {
                                return self.make_terminal_finalized(
                                    &graph_run_id, step, state,
                                    suppressed_errors, "completed", &mut guard,
                                ).await;
                            }
                        };
                    }
                }
                None => {
                    let err_str = "dispatch failed".to_string();
                    let error_msg = Some(err_str.clone());

                    if let Some(ref on_err_target) = node.on_error {
                        receipts.push(NodeReceipt {
                            node: current.clone(),
                            step,
                            result_hash: None,
                            cache_hit: false,
                            elapsed_ms: elapsed,
                            error: Some(err_str),
                        });
                        step += 1;
                        current = on_err_target.clone();
                        continue;
                    }

                    match cfg.on_error {
                        ErrorMode::Continue => {
                            suppressed_errors.push(ErrorRecord {
                                step,
                                node: current.clone(),
                                error: err_str,
                            });
                            receipts.push(NodeReceipt {
                                node: current.clone(),
                                step,
                                result_hash: None,
                                cache_hit: false,
                                elapsed_ms: elapsed,
                                error: error_msg,
                            });

                            let next = edges::evaluate_next(node, &state, &inputs);
                            step += 1;
                            current = match next {
                                Some(n) => n,
                                None => {
                                    return self.make_terminal_finalized(
                                        &graph_run_id, step, state,
                                        suppressed_errors, "completed", &mut guard,
                                    ).await;
                                }
                            };
                        }
                        ErrorMode::Fail => {
                            return self.make_error_finalized(
                                &graph_run_id, step, state,
                                &format!("node '{}' failed: {}", current, err_str),
                                &mut guard,
                            ).await;
                        }
                    }
                }
            }
        }

        let status_msg = format!("exceeded max_steps ({})", cfg.max_steps);

        {
            let hook_ctx = hooks::HookContext {
                graph_id: &self.graph.graph_id,
                graph_run_id: &graph_run_id,
                thread_id: &self.thread_id,
                step,
                current_node: "",
                state: &state,
            };
            hooks::fire_hook(&hook_list, "limit", &hook_ctx);
            let _ = self.client.append_event("limit", json!({
                "step": step,
                "max_steps": cfg.max_steps,
            })).await;
        }

        let result = GraphResult {
            success: false,
            graph_id: self.graph.graph_id.clone(),
            graph_run_id: graph_run_id.clone(),
            status: "max_steps_exceeded".into(),
            steps: step,
            state: state.clone(),
            result: None,
            errors_suppressed: None,
            errors: None,
            error: Some(status_msg),
        };

        {
            let hook_ctx = hooks::HookContext {
                graph_id: &self.graph.graph_id,
                graph_run_id: &graph_run_id,
                thread_id: &self.thread_id,
                step,
                current_node: "",
                state: &state,
            };
            hooks::fire_hook(&hook_list, "graph_completed", &hook_ctx);
        }

        let _ = knowledge::write_knowledge_transcript(
            &self.project_path,
            &self.graph.graph_id,
            &graph_run_id,
            &serde_json::to_string(&result).unwrap_or_default(),
        );

        let _ = self.client.finalize_thread("failed").await;
        guard.finalized = true;
        result
    }

    /// Terminal path: finalize as completed with proper lifecycle.
    async fn make_terminal_finalized(
        &self,
        graph_run_id: &str,
        steps: u32,
        state: Value,
        suppressed_errors: Vec<ErrorRecord>,
        base_status: &str,
        guard: &mut RunGuard,
    ) -> GraphResult {
        let result = make_terminal(&self.graph, graph_run_id, steps, state, suppressed_errors, base_status);
        let _ = self.client.finalize_thread("completed").await;
        guard.finalized = true;
        result
    }

    /// Error path: finalize as failed with proper lifecycle.
    async fn make_error_finalized(
        &self,
        graph_run_id: &str,
        steps: u32,
        state: Value,
        error: &str,
        guard: &mut RunGuard,
    ) -> GraphResult {
        let result = GraphResult {
            success: false,
            graph_id: self.graph.graph_id.clone(),
            graph_run_id: graph_run_id.to_string(),
            status: "error".into(),
            steps,
            state,
            result: None,
            errors_suppressed: None,
            errors: None,
            error: Some(error.to_string()),
        };
        let _ = self.client.finalize_thread("failed").await;
        guard.finalized = true;
        result
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

fn make_terminal(
    graph: &GraphDefinition,
    run_id: &str,
    steps: u32,
    state: Value,
    suppressed_errors: Vec<ErrorRecord>,
    base_status: &str,
) -> GraphResult {
    let status = if suppressed_errors.is_empty() {
        base_status.to_string()
    } else {
        "completed_with_errors".to_string()
    };
    GraphResult {
        success: true,
        graph_id: graph.graph_id.clone(),
        graph_run_id: run_id.to_string(),
        status,
        steps,
        state,
        result: None,
        errors_suppressed: if suppressed_errors.is_empty() {
            None
        } else {
            Some(suppressed_errors.len())
        },
        errors: if suppressed_errors.is_empty() {
            None
        } else {
            Some(suppressed_errors)
        },
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rye_runtime::callback::{CallbackError, DispatchActionRequest};
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
    impl rye_runtime::callback::RuntimeCallbackAPI for MockClient {
        async fn dispatch_action(
            &self,
            _request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            let mut results = self.results.lock().unwrap();
            if results.is_empty() {
                Ok(json!({"status": "ok", "data": {}}))
            } else {
                Ok(results.remove(0))
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
        async fn reserve_budget(&self, _: &str, _: f64) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn report_budget(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn release_budget(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_budget(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn set_facets(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    fn make_callback(results: Vec<Value>) -> CallbackClient {
        let inner: Arc<dyn rye_runtime::callback::RuntimeCallbackAPI> = Arc::new(MockClient::new(results));
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
        )
    }

    #[tokio::test]
    async fn simple_action_to_return() {
        let yaml = r#"
category: test
permissions:
  - rye.execute.*
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      assign: {echo_result: "${result}"}
      next: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![json!({"data": {"msg": "hello"}})]);
        let result = w.execute(json!({}), None).await;
        assert!(result.success);
        assert_eq!(result.status, "completed");
        assert_eq!(result.steps, 1);
    }

    #[tokio::test]
    async fn gate_node_conditional_routing() {
        let yaml = r#"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      assign: {mode: fast}
      next:
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
category: test
permissions:
  - rye.execute.*
config:
  start: loop
  max_steps: 3
  nodes:
    loop:
      action: {item_id: "tool:test/noop"}
      next: loop
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![
            json!({"data": {}}),
            json!({"data": {}}),
            json!({"data": {}}),
            json!({"data": {}}),
        ]);
        let result = w.execute(json!({}), None).await;
        assert!(!result.success);
        assert_eq!(result.status, "max_steps_exceeded");
    }

    #[test]
    fn validation_rejects_missing_start() {
        let yaml = r#"
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
category: test
permissions:
  - rye.execute.*
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      collect: "results"
      next: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![
            json!({"data": {"value": "a"}}),
            json!({"data": {"value": "b"}}),
            json!({"data": {"value": "c"}}),
        ]);
        let result = w.execute(json!({"inject_state": {"items": ["a", "b", "c"]}}), None).await;
        assert!(result.success);
        let results = result.state.get("results").and_then(|v| v.as_array()).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn on_error_continue_mode() {
        let yaml = r#"
category: test
permissions:
  - rye.execute.*
config:
  start: step1
  on_error: continue
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      next: step2
    step2:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let w = make_walker(graph, vec![
            json!({"status": "error", "data": {"error": "forced failure"}}),
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

        let val = json!({"status": "ok", "data": {"msg": "cached"}});
        cache.store(&key, &val);
        let cached = cache.lookup(&key).unwrap();
        assert_eq!(cached, val);
    }
}
