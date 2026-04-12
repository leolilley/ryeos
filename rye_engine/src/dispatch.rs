//! Plan execution — dispatches plan nodes via Lillux.
//!
//! The engine interprets plan nodes and delegates subprocess management
//! to Lillux. All OS-level process mechanics (setsid, process groups,
//! cross-platform handling, timeout enforcement) are Lillux's responsibility.

use serde_json::Value;

use crate::contracts::{
    EngineContext, ExecutionCompletion, ExecutionPlan, PlanNode, ThreadTerminalStatus,
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
            PlanNode::DispatchSubprocess {
                script_path,
                interpreter,
                working_directory,
                environment,
                arguments,
                runtime_bindings,
                ..
            } => {
                tracing::info!(script_path = %script_path.display(), "launching subprocess");
                let start = std::time::Instant::now();
                let completion = dispatch_subprocess(
                    script_path,
                    interpreter.as_deref(),
                    working_directory.as_ref().and_then(|p| p.to_str()),
                    environment,
                    arguments,
                    runtime_bindings,
                    ctx,
                )?;
                let elapsed = start.elapsed();
                tracing::debug!(
                    script_path = %script_path.display(),
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
fn dispatch_subprocess(
    script_path: &std::path::Path,
    interpreter: Option<&str>,
    working_directory: Option<&str>,
    environment: &std::collections::HashMap<String, String>,
    arguments: &[String],
    runtime_bindings: &std::collections::HashMap<String, String>,
    ctx: &EngineContext,
) -> Result<ExecutionCompletion, EngineError> {
    let script = script_path.to_str().ok_or_else(|| EngineError::ExecutionFailed {
        reason: format!("script path is not valid UTF-8: {}", script_path.display()),
    })?;

    // Build the command: interpreter + script, or just script
    let (cmd, mut args) = match interpreter {
        Some(interp) => (interp.to_owned(), vec![script.to_owned()]),
        None => (script.to_owned(), Vec::new()),
    };
    args.extend_from_slice(arguments);

    // Merge environment: plan env + runtime bindings + context
    let mut envs: Vec<(String, String)> = environment
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Runtime bindings from plan (daemon-injected)
    for (k, v) in runtime_bindings {
        envs.push((k.clone(), v.clone()));
    }

    // Context bindings
    envs.push(("RYE_THREAD_ID".to_owned(), ctx.thread_id.clone()));
    envs.push(("RYE_CHAIN_ROOT_ID".to_owned(), ctx.chain_root_id.clone()));

    let request = lillux::SubprocessRequest {
        cmd,
        args,
        cwd: working_directory.map(String::from),
        envs,
        stdin_data: None,
        timeout: 300.0,
    };

    let result = lillux::run(request);

    // Translate SubprocessResult → ExecutionCompletion
    if result.timed_out {
        return Ok(ExecutionCompletion {
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
        });
    }

    // Try to parse stdout as JSON; fall back to raw string
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

    Ok(ExecutionCompletion {
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
    })
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
            budget: None,
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
                script_path: PathBuf::from("/bin/echo"),
                interpreter: None,
                working_directory: None,
                environment: HashMap::new(),
                arguments: vec!["hello world".into()],
                runtime_bindings: HashMap::new(),
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
    fn dispatch_with_interpreter() {
        let dir = tempdir();
        let script = dir.join("test.py");
        fs::write(&script, "import json; print(json.dumps({'status': 'ok'}))\n").unwrap();

        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                script_path: script,
                interpreter: Some("python3".into()),
                working_directory: Some(dir),
                environment: HashMap::new(),
                arguments: Vec::new(),
                runtime_bindings: HashMap::new(),
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
                script_path: PathBuf::from("/bin/false"),
                interpreter: None,
                working_directory: None,
                environment: HashMap::new(),
                arguments: Vec::new(),
                runtime_bindings: HashMap::new(),
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
                script_path: script,
                interpreter: Some("python3".into()),
                working_directory: Some(dir),
                environment: env,
                arguments: Vec::new(),
                runtime_bindings: HashMap::new(),
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
                script_path: PathBuf::from("/nonexistent/binary"),
                interpreter: None,
                working_directory: None,
                environment: HashMap::new(),
                arguments: Vec::new(),
                runtime_bindings: HashMap::new(),
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
}
