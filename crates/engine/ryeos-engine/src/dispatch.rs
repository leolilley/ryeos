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
                let completion = dispatch_subprocess(spec, plan.debug_raw, ctx)?;
                let elapsed = start.elapsed();
                tracing::debug!(
                    cmd = %spec.cmd,
                    status = ?completion.status,
                    duration_ms = elapsed.as_millis() as u64,
                    "subprocess completed"
                );
                result = Some(completion);
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
/// Converts a finalized `PlanSubprocessSpec` into a `lillux::SubprocessRequest`.
fn dispatch_subprocess(
    spec: &PlanSubprocessSpec,
    debug_raw: bool,
    ctx: &EngineContext,
) -> Result<ExecutionCompletion, EngineError> {
    let request = sandbox_plan_request(spec, ctx)?;
    let capture = debug_raw.then(|| DebugCapture::from_spec(spec));
    let result = lillux::run(request);
    let debug = capture.map(|c| c.into_block(&result));
    let mut completion = translate_result(result);
    if let Some(debug) = debug {
        inject_debug(&mut completion, debug);
    }
    Ok(completion)
}

/// Size cap on each raw stream captured for `--debug-raw`.
const DEBUG_STDIO_CAP: usize = 16 * 1024;

/// Resolved subprocess facts captured for `--debug-raw`, paired with the
/// process result to form the debug block. Holds env *keys* only — never
/// values — and the stdio is size-capped when rendered.
struct DebugCapture {
    cmd: String,
    args: Vec<String>,
    cwd: Option<String>,
    env_keys: Vec<String>,
}

impl DebugCapture {
    fn from_spec(spec: &PlanSubprocessSpec) -> Self {
        let mut env_keys: Vec<String> = spec.env.keys().cloned().collect();
        env_keys.sort_unstable();
        Self {
            cmd: spec.cmd.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
            env_keys,
        }
    }

    fn into_block(self, result: &lillux::SubprocessResult) -> Value {
        serde_json::json!({
            "cmd": self.cmd,
            // For runtime tools the resolved interpreter IS the command.
            "interpreter": self.cmd,
            "args": self.args,
            "cwd": self.cwd,
            "env_keys": self.env_keys,
            "exit_code": result.exit_code,
            "timed_out": result.timed_out,
            "duration_ms": result.duration_ms,
            "stdout": truncate_for_error(&result.stdout, DEBUG_STDIO_CAP),
            "stderr": truncate_for_error(&result.stderr, DEBUG_STDIO_CAP),
        })
    }
}

/// Attach a `debug` block under the completion's `metadata`.
fn inject_debug(completion: &mut ExecutionCompletion, debug: Value) {
    match completion.metadata.as_mut().and_then(|m| m.as_object_mut()) {
        Some(obj) => {
            obj.insert("debug".to_string(), debug);
        }
        None => {
            completion.metadata = Some(serde_json::json!({ "debug": debug }));
        }
    }
}

/// Convert a `PlanSubprocessSpec` + daemon context into a `lillux::SubprocessRequest`.
fn spec_to_request(spec: &PlanSubprocessSpec) -> Result<lillux::SubprocessRequest, EngineError> {
    let envs: Vec<(String, String)> = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

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
                "stdout": truncate_for_error(&result.stdout, 2000),
                "stderr": truncate_for_error(&result.stderr, 2000),
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

    // Process-level failure (non-zero exit). stdout+stderr ride in `error`.
    if !result.success {
        return ExecutionCompletion {
            status: ThreadTerminalStatus::Failed,
            outcome_code: Some(format!("exit:{}", result.exit_code)),
            result: None,
            error: Some(serde_json::json!({
                "exit_code": result.exit_code,
                "stdout": truncate_for_error(&result.stdout, 2000),
                "stderr": truncate_for_error(&result.stderr, 2000),
            })),
            artifacts: Vec::new(),
            final_cost: None,
            continuation_request: None,
            metadata: Some(base_metadata(&result)),
        };
    }

    // Process exited 0. Parse stdout as the tool result JSON — but a
    // catch-and-report tool may still signal failure *inside* that JSON
    // while the process exits clean.
    let parsed = match serde_json::from_str::<Value>(&result.stdout) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::trace!(
                stdout_len = result.stdout.len(),
                "subprocess stdout is not valid JSON, wrapping as string: {e}"
            );
            None
        }
    };
    let result_value = parsed
        .clone()
        .unwrap_or_else(|| Value::String(result.stdout.clone()));

    // Soft failure: exit 0, but the result shape reports the tool failed.
    // The tool's real error (Python traceback, `logger.error(...)`) was
    // written to stderr — captured by lillux, but dropped on the success
    // path. Retain a bounded TAIL of stderr (where the error lands) in
    // `error`, which IS persisted into the run record; `metadata` is not.
    // Flipping to Failed also lines up with the graph runtime, which keys
    // subprocess-leaf failure off `error` being non-null.
    if parsed.as_ref().is_some_and(result_reports_failure) {
        return ExecutionCompletion {
            status: ThreadTerminalStatus::Failed,
            outcome_code: Some(format!("exit:{}", result.exit_code)),
            result: Some(result_value),
            error: Some(serde_json::json!({
                "exit_code": result.exit_code,
                "soft_failure": true,
                "stdout": truncate_for_error(&result.stdout, 2000),
                "stderr": truncate_tail_for_error(&result.stderr, STDERR_TAIL_CAP),
            })),
            artifacts: Vec::new(),
            final_cost: None,
            continuation_request: None,
            metadata: Some(base_metadata(&result)),
        };
    }

    // Genuine success. Keep stderr OUT of the durable record so healthy
    // runs don't spam logs — `base_metadata` notes only its byte count.
    ExecutionCompletion {
        status: ThreadTerminalStatus::Completed,
        outcome_code: Some(format!("exit:{}", result.exit_code)),
        result: Some(result_value),
        error: None,
        artifacts: Vec::new(),
        final_cost: None,
        continuation_request: None,
        metadata: Some(base_metadata(&result)),
    }
}

/// Max bytes of the stderr tail retained on a soft-failure completion.
/// A tool's real error lands at the *end* of stderr, so we keep the tail.
const STDERR_TAIL_CAP: usize = 8 * 1024;

/// Standard completion metadata for a process that ran to exit (not timed
/// out). Notes `stderr_bytes` when stderr is non-empty so a healthy run's
/// stray diagnostics are visible interactively without persisting the body.
fn base_metadata(result: &lillux::SubprocessResult) -> Value {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "duration_ms".to_owned(),
        serde_json::json!(result.duration_ms),
    );
    meta.insert("exit_code".to_owned(), serde_json::json!(result.exit_code));
    meta.insert("pid".to_owned(), serde_json::json!(result.pid));
    if !result.stderr.is_empty() {
        meta.insert(
            "stderr_bytes".to_owned(),
            serde_json::json!(result.stderr.len()),
        );
    }
    Value::Object(meta)
}

/// Does this parsed tool result *itself* report failure, despite the
/// process exiting 0? The catch-and-report convention: a JSON object that
/// sets `success: false`, carries a non-null `error`, or a non-empty
/// `errors` array. Mirrors the graph runtime's subprocess-leaf failure
/// signal (a non-null `error`).
fn result_reports_failure(parsed: &Value) -> bool {
    let Some(obj) = parsed.as_object() else {
        return false;
    };
    if obj.get("success").and_then(Value::as_bool) == Some(false) {
        return true;
    }
    if obj.get("error").is_some_and(|e| !e.is_null()) {
        return true;
    }
    obj.get("errors")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

/// A spawned but not-yet-completed execution.
/// The daemon can inspect pid/pgid before calling `wait()`.
pub struct SpawnedExecution {
    pub pid: u32,
    pub pgid: i64,
    running: lillux::RunningProcess,
    /// Present only under `--debug-raw`; consumed in [`wait`](Self::wait) to
    /// build the debug block once the process result is available.
    debug: Option<DebugCapture>,
}

impl SpawnedExecution {
    /// Block until the subprocess completes and return the completion.
    pub fn wait(self) -> ExecutionCompletion {
        let result = self.running.wait();
        let debug = self.debug.map(|c| c.into_block(&result));
        let mut completion = translate_result(result);
        if let Some(debug) = debug {
            inject_debug(&mut completion, debug);
        }
        completion
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
                return spawn_subprocess(spec, plan.debug_raw, ctx);
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
    debug_raw: bool,
    ctx: &EngineContext,
) -> Result<SpawnedExecution, EngineError> {
    let request = sandbox_plan_request(spec, ctx)?;
    let debug = debug_raw.then(|| DebugCapture::from_spec(spec));

    match lillux::spawn(request) {
        Ok(running) => {
            let pid = running.pid;
            let pgid = running.pgid;
            Ok(SpawnedExecution {
                pid,
                pgid,
                running,
                debug,
            })
        }
        Err(err_result) => Err(EngineError::ExecutionFailed {
            reason: format!("subprocess spawn failed: {}", err_result.stderr),
        }),
    }
}

fn sandbox_plan_request(
    spec: &PlanSubprocessSpec,
    ctx: &EngineContext,
) -> Result<lillux::SubprocessRequest, EngineError> {
    let request = spec_to_request(spec)?;
    let project_path = spec
        .cwd
        .as_deref()
        .ok_or_else(|| EngineError::SandboxPolicyRefused {
            reason: "executable plan requires an explicit working directory".to_string(),
        })?;
    crate::subprocess_spec::sandbox_lillux_request(
        request,
        ctx.sandbox_enabled,
        &ctx.app_root,
        project_path,
        "tool:ryeos/internal/plan-subprocess",
        &ctx.thread_id,
    )
}

/// Truncate a string for inclusion in error payloads.
/// Returns the original if already short enough; otherwise returns
/// the first `max_len` chars + "… (truncated, N bytes total)".
fn truncate_for_error(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_owned()
    } else {
        // Walk back to a char boundary so we never slice mid-codepoint.
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… (truncated, {} bytes total)", &s[..end], s.len())
    }
}

/// Retain the LAST `max_len` bytes of `s` (the tail), where a tool's real
/// error — a Python traceback, a final `logger.error(...)` — usually
/// lands. Char-boundary safe; prefixes a marker when bytes were dropped.
fn truncate_tail_for_error(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_owned();
    }
    // Walk forward to a char boundary so we never slice mid-codepoint.
    let mut start = s.len() - max_len;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("… (truncated, {} bytes total)\n{}", s.len(), &s[start..])
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

    fn host_executable(name: &str) -> String {
        let search_path = std::env::var_os("PATH").expect("test PATH is set");
        std::env::split_paths(&search_path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| std::fs::canonicalize(candidate).ok())
            .unwrap_or_else(|| panic!("test executable `{name}` is not available on PATH"))
            .to_string_lossy()
            .into_owned()
    }

    fn test_engine_context() -> EngineContext {
        let app_root = tempdir();
        let policy_dir = app_root.join(".ai/node");
        fs::create_dir_all(&policy_dir).unwrap();
        fs::write(
            policy_dir.join("sandbox.yaml"),
            "version: 1\nbackend_path: /usr/bin/bwrap\nallow_network: false\nwritable_paths: [\"{project}\"]\nallowed_env: [\"*\"]\nmax_open_files: 128\nmax_processes: 32\n",
        )
        .unwrap();
        EngineContext {
            app_root,
            sandbox_enabled: false,
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
            debug_raw: false,
        }
    }

    #[test]
    fn dispatch_echo() {
        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: Box::new(PlanSubprocessSpec {
                    cmd: "/bin/echo".into(),
                    args: vec!["hello world".into()],
                    cwd: Some(tempdir()),
                    env: HashMap::new(),
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
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
        assert!(completion.metadata.as_ref().unwrap()["pid"]
            .as_u64()
            .is_some());
        // No flag ⇒ no debug block (the normal path is untouched).
        assert!(
            completion.metadata.as_ref().unwrap().get("debug").is_none(),
            "debug block must be absent without --debug-raw"
        );
    }

    #[test]
    fn debug_raw_attaches_debug_block_with_keys_not_values() {
        let mut env = HashMap::new();
        env.insert(
            "EXAMPLE_SECRET".to_string(),
            "super-secret-value".to_string(),
        );
        let mut plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: Box::new(PlanSubprocessSpec {
                    cmd: "/bin/echo".into(),
                    args: vec!["hello debug".into()],
                    cwd: Some(tempdir()),
                    env,
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
                tool_path: None,
                executor_chain: Vec::new(),
            },
            PlanNode::Complete {
                id: PlanNodeId("complete:test".into()),
            },
        ]);
        plan.debug_raw = true;

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();
        let debug = completion.metadata.as_ref().unwrap()["debug"].clone();
        assert_eq!(debug["cmd"], "/bin/echo");
        assert_eq!(debug["interpreter"], "/bin/echo");
        assert_eq!(debug["args"][0], "hello debug");
        assert_eq!(debug["exit_code"], 0);
        assert_eq!(debug["timed_out"], false);
        assert!(debug["stdout"].as_str().unwrap().contains("hello debug"));
        assert_eq!(debug["env_keys"][0], "EXAMPLE_SECRET");
        // The block carries env KEYS only — never values.
        assert!(
            !debug.to_string().contains("super-secret-value"),
            "debug block must not leak env values: {debug}"
        );
    }

    #[test]
    fn dispatch_with_cmd_python() {
        let dir = tempdir();
        let script = dir.join("test.py");
        fs::write(
            &script,
            "import json; print(json.dumps({'status': 'ok'}))\n",
        )
        .unwrap();

        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: Box::new(PlanSubprocessSpec {
                    cmd: host_executable("python3"),
                    args: vec![script.to_string_lossy().to_string()],
                    cwd: Some(dir),
                    env: HashMap::new(),
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
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
                spec: Box::new(PlanSubprocessSpec {
                    cmd: "/bin/false".into(),
                    args: Vec::new(),
                    cwd: Some(tempdir()),
                    env: HashMap::new(),
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
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

    /// A tool that logs its real error to stderr and returns
    /// `{"success": false}` while EXITING 0 (the catch-and-report pattern)
    /// must produce a Failed completion whose `error` carries the stderr
    /// tail — otherwise the error is undebuggable on the scheduled path.
    #[test]
    fn dispatch_soft_failure_surfaces_stderr_tail() {
        let dir = tempdir();
        let script = dir.join("soft_fail.py");
        fs::write(
            &script,
            "import sys, json\n\
             sys.stderr.write('UPSERT_MARKER: Upsert failed (500): boom body\\n')\n\
             print(json.dumps({'success': False, \
             'error_summary': {'store_or_schedule_failed: RuntimeError': 3}}))\n",
        )
        .unwrap();

        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: Box::new(PlanSubprocessSpec {
                    cmd: host_executable("python3"),
                    args: vec![script.to_string_lossy().to_string()],
                    cwd: Some(dir),
                    env: HashMap::new(),
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
                tool_path: None,
                executor_chain: Vec::new(),
            },
            PlanNode::Complete {
                id: PlanNodeId("complete:test".into()),
            },
        ]);

        let ctx = test_engine_context();
        let completion = execute_plan(&plan, &ctx).unwrap();

        // Exit 0 but the result says it failed → marked Failed.
        assert_eq!(completion.status, ThreadTerminalStatus::Failed);
        let error = completion
            .error
            .as_ref()
            .expect("soft failure carries error");
        assert_eq!(error["soft_failure"], true);
        assert_eq!(error["exit_code"], 0);
        // The tool's real error — only ever written to stderr — is retained.
        assert!(
            error["stderr"].as_str().unwrap().contains("UPSERT_MARKER"),
            "stderr tail must carry the tool's logged error: {error}"
        );
        // The tool's structured result is still preserved for diagnosis.
        let result = completion.result.as_ref().expect("result preserved");
        assert_eq!(result["success"], false);
    }

    /// A clean tool (exit 0, no failure signal in its result) stays
    /// Completed and keeps stderr OUT of the durable `error` — only its
    /// byte count is noted in metadata.
    #[test]
    fn dispatch_clean_success_does_not_persist_stderr() {
        let dir = tempdir();
        let script = dir.join("noisy_ok.py");
        fs::write(
            &script,
            "import sys, json\n\
             sys.stderr.write('NOISE: progress 1/3\\n')\n\
             print(json.dumps({'success': True, 'rows': 10}))\n",
        )
        .unwrap();

        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: Box::new(PlanSubprocessSpec {
                    cmd: host_executable("python3"),
                    args: vec![script.to_string_lossy().to_string()],
                    cwd: Some(dir),
                    env: HashMap::new(),
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
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
        assert!(completion.error.is_none(), "clean run must not set error");
        // stderr body is not persisted, but its size is noted.
        let meta = completion.metadata.as_ref().unwrap();
        assert!(meta["stderr_bytes"].as_u64().unwrap() > 0);
        assert!(
            !meta.to_string().contains("NOISE"),
            "clean-run stderr body must not be retained: {meta}"
        );
    }

    #[test]
    fn result_reports_failure_predicate() {
        assert!(result_reports_failure(
            &serde_json::json!({"success": false})
        ));
        assert!(result_reports_failure(
            &serde_json::json!({"error": "boom"})
        ));
        assert!(result_reports_failure(
            &serde_json::json!({"errors": ["a"]})
        ));
        // Healthy / absent / null-error shapes are NOT failures.
        assert!(!result_reports_failure(
            &serde_json::json!({"success": true})
        ));
        assert!(!result_reports_failure(
            &serde_json::json!({"error": null, "errors": []})
        ));
        assert!(!result_reports_failure(&serde_json::json!({"rows": 10})));
        // Non-object results never trip the predicate.
        assert!(!result_reports_failure(&serde_json::json!("ok")));
        assert!(!result_reports_failure(&serde_json::json!([1, 2, 3])));
    }

    #[test]
    fn truncate_tail_keeps_the_end() {
        let s = "abcdefghij"; // 10 bytes
                              // Short input is returned whole.
        assert_eq!(truncate_tail_for_error(s, 100), s);
        // Long input keeps the LAST max_len bytes plus a marker.
        let out = truncate_tail_for_error(s, 4);
        assert!(out.ends_with("ghij"), "tail must be the end: {out}");
        assert!(out.contains("truncated, 10 bytes total"));
        // Multi-byte boundary: cutting through a 'é' (2 bytes) must not panic
        // and must yield valid UTF-8.
        let multi = "ααααα"; // 10 bytes, 5 chars
        let out = truncate_tail_for_error(multi, 5);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn dispatch_env_bindings() {
        let dir = tempdir();
        let script = dir.join("env_test.py");
        fs::write(
            &script,
            "import os, json; print(json.dumps({'tid': os.environ.get('RYEOS_THREAD_ID', ''), 'ref': os.environ.get('RYEOS_ITEM_REF', '')}))\n",
        )
        .unwrap();

        let mut env = HashMap::new();
        env.insert("RYEOS_ITEM_REF".into(), "tool:my_tool".into());
        env.insert("RYEOS_THREAD_ID".into(), "thread:test".into());

        let plan = make_plan(vec![
            PlanNode::DispatchSubprocess {
                id: PlanNodeId("entry:test".into()),
                spec: Box::new(PlanSubprocessSpec {
                    cmd: host_executable("python3"),
                    args: vec![script.to_string_lossy().to_string()],
                    cwd: Some(dir),
                    env,
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
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
                spec: Box::new(PlanSubprocessSpec {
                    cmd: "/nonexistent/binary".into(),
                    args: Vec::new(),
                    cwd: Some(tempdir()),
                    env: HashMap::new(),
                    env_sources: HashMap::new(),
                    stdin_data: None,
                    timeout_secs: 300,
                    execution: Default::default(),
                }),
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
        assert_eq!(completion.outcome_code.as_deref(), Some("exit:-1"));
        let error = completion.error.expect("spawn failure carries error");
        assert_eq!(error["exit_code"], -1);
        assert!(
            error["stderr"]
                .as_str()
                .is_some_and(|stderr| stderr.contains("Failed to spawn")),
            "spawn error must be preserved: {error}"
        );
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
    fn spec_to_request_preserves_finalized_env() {
        let mut env = HashMap::new();
        env.insert("RYEOS_THREAD_ID".into(), "thread:test".into());
        env.insert("RYEOS_CHAIN_ROOT_ID".into(), "chain:test".into());
        let spec = PlanSubprocessSpec {
            cmd: "/bin/echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            env,
            env_sources: HashMap::new(),
            stdin_data: None,
            timeout_secs: 60,
            execution: Default::default(),
        };
        let request = spec_to_request(&spec).unwrap();
        assert_eq!(request.cmd, "/bin/echo");
        assert_eq!(request.timeout, 60.0);
        let env_map: HashMap<String, String> = request.envs.into_iter().collect();
        assert_eq!(env_map.get("RYEOS_THREAD_ID").unwrap(), "thread:test");
        assert_eq!(env_map.get("RYEOS_CHAIN_ROOT_ID").unwrap(), "chain:test");
    }
}
