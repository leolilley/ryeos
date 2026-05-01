//! Plan execution — dispatches plan nodes via Lillux.
//!
//! The engine interprets plan nodes and delegates subprocess management
//! to Lillux. All OS-level process mechanics (setsid, process groups,
//! cross-platform handling, timeout enforcement) are Lillux's responsibility.
//!
//! The dispatch layer consumes `PlanSubprocessSpec` from the plan builder —
//! a fully resolved, template-expanded spawn description. No interpreter
//! branching or script_path construction happens here.

use serde_json::Value;

use crate::contracts::{
    EngineContext, ExecutionCompletion, ExecutionPlan, PlanNode, PlanSubprocessSpec,
    ThreadTerminalStatus,
};
use crate::error::EngineError;

/// Execute a plan by dispatching its nodes via Lillux.
pub fn execute_plan(
    plan: &ExecutionPlan,
    ctx: &EngineContext,
) -> Result<ExecutionCompletion, EngineError> {
    let mut result: Option<ExecutionCompletion> = None;

    for node in &plan.nodes {
        match node {
            PlanNode::DispatchSubprocess { spec, .. } => {
                tracing::info!(cmd = %spec.cmd, "launching subprocess");
                let start = std::time::Instant::now();
                let completion = dispatch_subprocess(spec, ctx)?;
                let elapsed = start.elapsed();
                tracing::debug!(
                    cmd = %spec.cmd,
                    status = ?completion.status,
                    duration_ms = elapsed.as_millis() as u64,
                    "subprocess completed"
                );
                result = Some(completion);
            }
            PlanNode::SpawnChild { child_ref, .. } => {
                return Err(EngineError::Internal(format!(
                    "SpawnChild not yet supported (child_ref={child_ref})"
                )));
            }
            PlanNode::Complete { .. } => {
                return Ok(result.unwrap_or(ExecutionCompletion {
                    status: ThreadTerminalStatus::Completed,
                    outcome_code: None,
                    result: None,
                    error: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    continuation_request: None,
                    metadata: None,
                }));
            }
        }
    }

    Ok(result.unwrap_or(ExecutionCompletion {
        status: ThreadTerminalStatus::Completed,
        outcome_code: None,
        result: None,
        error: None,
        artifacts: Vec::new(),
        final_cost: None,
        continuation_request: None,
        metadata: None,
    }))
}

/// Dispatch a subprocess plan node via Lillux.
///
/// Converts a `PlanSubprocessSpec` into a `lillux::SubprocessRequest`,
/// injecting daemon context bindings (RYE_THREAD_ID, RYE_CHAIN_ROOT_ID).
fn dispatch_subprocess(
    spec: &PlanSubprocessSpec,

    ctx: &EngineContext,
) -> Result<ExecutionCompletion, EngineError> {
    let request = spec_to_request(spec, ctx)?;
    let result = lillux::run(request);
    Ok(translate_result(result))
}

/// Convert a `PlanSubprocessSpec` + daemon context into a `lillux::SubprocessRequest`.
fn spec_to_request(
    spec: &PlanSubprocessSpec,
    ctx: &EngineContext,
) -> Result<lillux::SubprocessRequest, EngineError> {
    // Build env: spec.env + daemon context bindings
    let mut envs: Vec<(String, String)> = spec.env.iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Daemon context bindings (always injected, override any spec values)
    envs.push(("RYE_THREAD_ID".to_owned(), ctx.thread_id.clone()));
    envs.push(("RYE_CHAIN_ROOT_ID".to_owned(), ctx.chain_root_id.clone()));

    Ok(lillux::SubprocessRequest {
        cmd: spec.cmd.clone(),
        args: spec.args.clone(),
        cwd: spec.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
        envs,
        stdin_data: spec.stdin_data.clone(),
        timeout: spec.timeout_secs as f64,
    })
}

/// Translate a Lillux SubprocessResult into an ExecutionCompletion.
fn translate_result(result: lillux::SubprocessResult) -> ExecutionCompletion {
    if result.timed_out {
        return ExecutionCompletion {
            status: ThreadTerminalStatus::Killed,
            outcome_code: Some("timeout".to_owned()),
            result: None,
            error: Some(serde_json::json!({
                "message": "subprocess timed out",
                "stdout": result.stdout,
                "stderr": result.stderr,
            })),
            artifacts: Vec::new(),
            final_cost: None,
            continuation_request: None,
            metadata: Some(serde_json::json!({
                "duration_ms": result.duration_ms,
                "pid": result.pid,
            })),
        };
    }

    let result_value = if result.success {
        let parsed = serde_json::from_str::<Value>(&result.stdout).ok();
        Some(parsed.unwrap_or(Value::String(result.stdout.clone())))
    } else {
        None
    };

    let error_value = if !result.success {
        Some(serde_json::json!({
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
        }))
    } else {
        None
    };

    ExecutionCompletion {
        status: if result.success {
            ThreadTerminalStatus::Completed
        } else {
            ThreadTerminalStatus::Failed
        },
        outcome_code: Some(format!("exit:{}", result.exit_code)),
        result: result_value,
        error: error_value,
        artifacts: Vec::new(),
        final_cost: None,
        continuation_request: None,
        metadata: Some(serde_json::json!({
            "duration_ms": result.duration_ms,
            "exit_code": result.exit_code,
            "pid": result.pid,
        })),
    }
}

/// A spawned but not-yet-completed execution.
/// The daemon can inspect pid/pgid before calling `wait()`.
pub struct SpawnedExecution {
    pub pid: u32,
    pub pgid: i64,
    running: lillux::RunningProcess,
}

impl SpawnedExecution {
    /// Block until the subprocess completes and return the completion.
    pub fn wait(self) -> ExecutionCompletion {
        let result = self.running.wait();
        translate_result(result)
    }
}

/// Spawn a plan's subprocess without waiting for completion.
/// Returns the SpawnedExecution handle with pid/pgid accessible immediately.
pub fn spawn_plan(
    plan: &ExecutionPlan,
    ctx: &EngineContext,
) -> Result<SpawnedExecution, EngineError> {
    if let Some(node) = plan.nodes.first() {
        match node {
            PlanNode::DispatchSubprocess { spec, .. } => {
                return spawn_subprocess(spec, ctx);
            }
            PlanNode::SpawnChild { child_ref, .. } => {
                return Err(EngineError::Internal(format!(
                    "SpawnChild not yet supported (child_ref={child_ref})"
                )));
            }
            PlanNode::Complete { .. } => {
                return Err(EngineError::Internal(
                    "plan has no subprocess node to spawn".to_string(),
                ));
            }
        }
    }
    Err(EngineError::Internal("empty plan".to_string()))
}

fn spawn_subprocess(
    spec: &PlanSubprocessSpec,
    ctx: &EngineContext,
) -> Result<SpawnedExecution, EngineError> {
    let request = spec_to_request(spec, ctx)?;

    match lillux::spawn(request) {
        Ok(running) => {
            let pid = running.pid;
            let pgid = running.pgid;
            Ok(SpawnedExecution { pid, pgid, running })
        }
        Err(err_result) => Err(EngineError::ExecutionFailed {
            reason: format!("subprocess spawn failed: {}", err_result.stderr),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_dispatch_test_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_engine_context() -> EngineContext {
        EngineContext {
            thread_id: "thread:test".into(),
            chain_root_id: "chain:test".into(),
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            upstream_site_id: None,
            upstream_thread_id: None,
            continuation_from_id: None,
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".into(),
                scopes: vec!["execute".into()],
            }),
            project_context: ProjectContext::None,
            launch_mode: LaunchMode::Inline,
        }
    }

    fn make_plan(nodes: Vec<PlanNode>) -> ExecutionPlan {
        ExecutionPlan {
            plan_id: "plan:test".into(),
            root_executor_id: "@test".into(),
            root_ref: "tool:test".into(),
            item_kind: "tool".into(),
            thread_kind: Some("tool".into()),
            nodes,
            entrypoint: PlanNodeId("entry:test".into()),
            capabilities: PlanCapabilities::default(),
            materialization_requirements: Vec::new(),
            cache_key: "test".into(),
            executor_chain: vec!["@test".into()],
        }
    }

    #[test]
    fn dispatch_echo() {
        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: PlanSubprocessSpec {
                    cmd: "/bin/echo".into(),
                    args: vec!["hello world".into()],
                    cwd: None,
                    env: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                },
                tool_path: None,
                executor_chain: Vec::new(),
            },
            PlanNode::Complete {
                id: PlanNodeId("complete:test".into()),
            },
        ]);

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();
        assert_eq!(completion.status, ThreadTerminalStatus::Completed);
        assert!(completion.metadata.as_ref().unwrap()["pid"].as_u64().is_some());
    }

    #[test]
    fn dispatch_with_cmd_python() {
        let dir = tempdir();
        let script = dir.join("test.py");
        fs::write(&script, "import json; print(json.dumps({'status': 'ok'}))\n").unwrap();

        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: PlanSubprocessSpec {
                    cmd: "python3".into(),
                    args: vec![script.to_string_lossy().to_string()],
                    cwd: Some(dir),
                    env: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                },
                tool_path: None,
                executor_chain: Vec::new(),
            },
            PlanNode::Complete {
                id: PlanNodeId("complete:test".into()),
            },
        ]);

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();
        assert_eq!(completion.status, ThreadTerminalStatus::Completed);
        let result = completion.result.unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[test]
    fn dispatch_failure() {
        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: PlanSubprocessSpec {
                    cmd: "/bin/false".into(),
                    args: Vec::new(),
                    cwd: None,
                    env: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                },
                tool_path: None,
                executor_chain: Vec::new(),
            },
            PlanNode::Complete {
                id: PlanNodeId("complete:test".into()),
            },
        ]);

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();
        assert_eq!(completion.status, ThreadTerminalStatus::Failed);
        assert!(completion.error.is_some());
    }

    #[test]
    fn dispatch_env_bindings() {
        let dir = tempdir();
        let script = dir.join("env_test.py");
        fs::write(
            &script,
            "import os, json; print(json.dumps({'tid': os.environ.get('RYE_THREAD_ID', ''), 'ref': os.environ.get('RYE_ITEM_REF', '')}))\n",
        )
        .unwrap();

        let mut env = HashMap::new();
        env.insert("RYE_ITEM_REF".into(), "tool:my_tool".into());

        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: PlanSubprocessSpec {
                    cmd: "python3".into(),
                    args: vec![script.to_string_lossy().to_string()],
                    cwd: Some(dir),
                    env,
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                },
                tool_path: None,
                executor_chain: Vec::new(),
            },
            PlanNode::Complete {
                id: PlanNodeId("complete:test".into()),
            },
        ]);

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();
        assert_eq!(completion.status, ThreadTerminalStatus::Completed);
        let result = completion.result.unwrap();
        assert_eq!(result["ref"], "tool:my_tool");
        assert_eq!(result["tid"], "thread:test");
    }

    #[test]
    fn dispatch_nonexistent_binary() {
        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: PlanSubprocessSpec {
                    cmd: "/nonexistent/binary".into(),
                    args: Vec::new(),
                    cwd: None,
                    env: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                },
                tool_path: None,
                executor_chain: Vec::new(),
            },
            PlanNode::Complete {
                id: PlanNodeId("complete:test".into()),
            },
        ]);

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();
        // Lillux returns a failed result for spawn errors
        assert!(!matches!(completion.status, ThreadTerminalStatus::Completed));
    }

    #[test]
    fn complete_only_plan() {
        let plan = make_plan(vec![PlanNode::Complete {
            id: PlanNodeId("complete:test".into()),
        }]);

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();
        assert_eq!(completion.status, ThreadTerminalStatus::Completed);
        assert!(completion.result.is_none());
    }

    #[test]
    fn spec_to_request_injects_context_bindings() {
        let spec = PlanSubprocessSpec {
            cmd: "/bin/echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            env: HashMap::new(),
            stdin_data: None,
            timeout_secs: 60,
            execution: Default::default(),
        };
        let ctx = test_engine_context();
        let request = spec_to_request(&spec, &ctx).unwrap();
        assert_eq!(request.cmd, "/bin/echo");
        assert_eq!(request.timeout, 60.0);
        // Context bindings must be present
        let env_map: HashMap<String, String> = request.envs.into_iter().collect();
        assert_eq!(env_map.get("RYE_THREAD_ID").unwrap(), "thread:test");
        assert_eq!(env_map.get("RYE_CHAIN_ROOT_ID").unwrap(), "chain:test");
    }
}
