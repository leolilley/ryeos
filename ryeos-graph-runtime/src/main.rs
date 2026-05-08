mod cache;
mod context;
mod dispatch;
mod edges;
mod env_preflight;
mod foreach;
mod hooks;
mod knowledge;
mod model;
mod persistence;
mod resume;
mod validation;
mod walker;

use std::io::Read;

use clap::Parser;
use serde_json::{json, Value};

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::checkpoint::CheckpointWriter;
use ryeos_runtime::envelope::{EnvelopeCallback, RuntimeResult};

#[derive(Parser)]
#[command(name = "graph-runtime", about = "Native graph walker for Rye OS")]
struct Cli {
    #[arg(long)]
    graph_path: Option<String>,

    #[arg(long)]
    graph_run_id: Option<String>,

    #[arg(long)]
    daemon_socket: Option<String>,

    #[arg(long, env = "RYEOS_THREAD_ID", default_value = "graph-default")]
    thread_id: String,

    #[arg(long)]
    pre_registered: bool,

    /// Accepted for spawn-contract parity with the daemon launcher. Ignored
    /// in favour of `envelope.roots.project_root` (which is the single
    /// source of truth per C1).
    #[arg(long)]
    project_path: Option<String>,
}

/// Normalized launch data from the envelope.
struct ResolvedLaunch {
    project_root: std::path::PathBuf,
    graph_path: std::path::PathBuf,
    thread_id: String,
    graph_run_id: Option<String>,
    inputs: Value,
    previous_thread_id: Option<String>,
    parent_thread_id: Option<String>,
    depth: u32,
    hard_limits: Value,
    callback: Option<EnvelopeCallback>,
    target_digest: Option<String>,
    user_root: Option<std::path::PathBuf>,
    system_roots: Vec<std::path::PathBuf>,
    invocation_id: Option<String>,
}

fn main() -> anyhow::Result<()> {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_graph_runtime());

    let cli = Cli::parse();

    let mut stdin_data = Vec::new();
    std::io::stdin().read_to_end(&mut stdin_data)?;
    if stdin_data.is_empty() {
        anyhow::bail!("graph runtime requires LaunchEnvelope on stdin");
    }

    let resolved = resolve_from_envelope(&stdin_data, &cli)?;

    let raw = std::fs::read_to_string(&resolved.graph_path)?;
    let graph = model::GraphDefinition::from_yaml(
        &raw,
        Some(&resolved.graph_path.to_string_lossy()),
    )?;

    tracing::info!(
        thread_id = %resolved.thread_id,
        graph_run_id = ?resolved.graph_run_id,
        target_digest = ?resolved.target_digest,
        invocation_id = ?resolved.invocation_id,
        user_root = ?resolved.user_root,
        system_roots = ?resolved.system_roots,
        graph_id = %graph.graph_id,
        declared_permissions = ?graph.declared_permissions,
        "launch resolved"
    );

    let rt = tokio::runtime::Runtime::new()?;

    let checkpoint = CheckpointWriter::from_env();

    // Resume precedence (D10):
    // 1. RYEOS_RESUME=1 + local checkpoint → checkpoint wins
    // 2. RYEOS_RESUME=1 + no local checkpoint → explicit fallback to replay
    // 3. Both unavailable + resume requested → fail loud
    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    let callback = match resolved.callback.as_ref() {
        Some(cb) => CallbackClient::new(
            cb,
            &resolved.thread_id,
            resolved.project_root.to_str().unwrap_or(""),
            &thread_auth_token,
        ),
        None => {
            if let Some(ref socket) = cli.daemon_socket {
                std::env::set_var("RYEOSD_SOCKET_PATH", socket);
            }
            let cb_env = EnvelopeCallback {
                socket_path: ryeos_runtime::resolve_daemon_socket_path(None),
                token: std::env::var("RYEOSD_CALLBACK_TOKEN")
                    .expect("RYEOSD_CALLBACK_TOKEN must be set by daemon"),
            };
            CallbackClient::new(
                &cb_env,
                &resolved.thread_id,
                resolved.project_root.to_str().unwrap_or(""),
                &thread_auth_token,
            )
        }
    };

    // V5.5 D10 resume precedence (the rule itself lives in
    // `resume::decide_resume_source` — pure function, unit-tested):
    //   1. RYEOS_RESUME=1 + local CheckpointWriter payload → checkpoint wins.
    //   2. RYEOS_RESUME=1 + no local checkpoint → fall back to replay-events.
    //   3. RYEOS_RESUME=1 + neither available → fail loud (NO silent cold-start).
    //   4. RYEOS_RESUME unset → cold start (None).
    let resume_requested = CheckpointWriter::is_resume();
    let local_checkpoint: Option<Value> = if resume_requested {
        checkpoint
            .as_ref()
            .and_then(|w| w.load_latest().transpose())
            .transpose()?
    } else {
        None
    };
    let replay_state: Option<resume::ResumeState> = if resume_requested
        && local_checkpoint.is_none()
    {
        // D12: replay resume keys on thread_id only, not graph_run_id.
        // The launcher doesn't supply graph_run_id; thread is the actual
        // partition; matched event payload reconstructs the run_id.
        tracing::warn!(
            thread_id = %resolved.thread_id,
            "RYEOS_RESUME=1 but no local checkpoint; falling back to replay-events resume"
        );
        rt.block_on(resume::load_resume_state(
            &callback,
            &resolved.thread_id,
        ))?
    } else {
        None
    };

    let resume_state: Option<resume::ResumeState> = match resume::decide_resume_source(
        resume_requested,
        local_checkpoint.is_some(),
        replay_state.is_some(),
    ) {
        resume::ResumeSource::ColdStart => None,
        resume::ResumeSource::LocalCheckpoint => {
            tracing::info!("resuming from local checkpoint");
            Some(resume::from_checkpoint_value(
                local_checkpoint
                    .as_ref()
                    .expect("LocalCheckpoint variant requires payload"),
            )?)
        }
        resume::ResumeSource::ReplayFallback => Some(
            replay_state.expect("ReplayFallback variant requires replay state"),
        ),
        resume::        ResumeSource::NoSourceAvailable => {
            anyhow::bail!(
                "RYEOS_RESUME=1 but no resume source is available for thread '{}': \
                 local checkpoint absent and replay-events reconstruction found \
                 no graph_step_started for this thread; \
                 refusing silent cold-start (V5.5 D10)",
                resolved.thread_id
            );
        }
    };

    // If we got a resume state, inject it so the walker picks up where it left off.
    if let Some(ref rs) = resume_state {
        tracing::info!(
            node = %rs.current_node,
            step = rs.step_count,
            "resuming graph"
        );
    }

    let mut params = json!({
        "inputs": resolved.inputs,
        "previous_thread_id": resolved.previous_thread_id,
        "parent_thread_id": resolved.parent_thread_id,
        "depth": resolved.depth,
        "hard_limits": resolved.hard_limits,
    });

    // Inject resume state so walker.rs picks it up
    if let Some(ref rs) = resume_state {
        params["resume_state"] = json!({
            "current_node": rs.current_node,
            "step_count": rs.step_count,
            "state": rs.state,
        });
    }

    if let Some(ref schema) = graph.config.config_schema {
        if let Err(err) = normalize_inputs_against_schema(&mut params, schema) {
            let runtime_result = make_error_runtime_result(
                &resolved.thread_id,
                "invalid_inputs",
                &format!("input validation failed: {err}"),
            );
            println!("{}", serde_json::to_string(&runtime_result)?);
            std::process::exit(0);
        }
    }

    let rt = tokio::runtime::Runtime::new()?;

    let w = walker::Walker::new(
        graph,
        resolved.project_root.to_string_lossy().to_string(),
        resolved.thread_id.clone(),
        callback,
        checkpoint,
    );

    let graph_result = rt.block_on(w.execute(params, resolved.graph_run_id));
    // V5.5 P0 #3: pull non-fatal callback drift the walker
    // accumulated during the run. Empty on a clean run.
    let warnings = w.take_warnings();

    // D1 / B5: ship the structured GraphResult through verbatim.
    // Daemon parses RuntimeResult and forwards `result` into the
    // `/execute` response, so HTTP callers see the typed graph
    // result (success/status/state/path) without re-parsing JSON
    // out of a string.
    let runtime_result = RuntimeResult {
        success: graph_result.success,
        status: graph_result.status.clone(),
        thread_id: resolved.thread_id.clone(),
        result: Some(serde_json::to_value(&graph_result)?),
        outputs: serde_json::Value::Null,
        cost: None,
        warnings,
    };

    println!("{}", serde_json::to_string(&runtime_result)?);

    Ok(())
}

fn resolve_from_envelope(stdin_data: &[u8], cli: &Cli) -> anyhow::Result<ResolvedLaunch> {
    let envelope: ryeos_runtime::envelope::LaunchEnvelope = serde_json::from_slice(stdin_data)
        .map_err(|e| anyhow::anyhow!("invalid envelope: {e}"))?;

    let graph_path = cli.graph_path.clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| envelope.resolution.root.source_path.clone());

    Ok(ResolvedLaunch {
        project_root: envelope.roots.project_root.clone(),
        graph_path,
        thread_id: envelope.thread_id.clone(),
        graph_run_id: cli.graph_run_id.clone(),
        inputs: envelope.request.inputs.clone(),
        previous_thread_id: envelope.request.previous_thread_id.clone(),
        parent_thread_id: envelope.request.parent_thread_id.clone(),
        depth: envelope.request.depth,
        hard_limits: serde_json::to_value(&envelope.policy.hard_limits).unwrap_or(json!({})),
        callback: Some(envelope.callback),
        target_digest: Some(envelope.resolution.root.raw_content_digest.clone()),
        user_root: envelope.roots.user_root.clone(),
        system_roots: envelope.roots.system_roots.clone(),
        invocation_id: Some(envelope.invocation_id.clone()),
    })
}

fn make_error_runtime_result(thread_id: &str, status: &str, error: &str) -> RuntimeResult {
    RuntimeResult {
        success: false,
        status: status.to_string(),
        thread_id: thread_id.to_string(),
        result: Some(json!(error)),
        outputs: serde_json::Value::Null,
        cost: None,
        warnings: Vec::new(),
    }
}

/// Normalize inputs against a shallow JSON Schema:
/// 1. Enforce `required` fields
/// 2. Type-check provided fields against `type`
/// 3. Apply `default` for absent non-required fields
fn normalize_inputs_against_schema(params: &mut Value, schema: &Value) -> anyhow::Result<()> {
    let mut input_obj = params.get("inputs").cloned().unwrap_or(json!({}));

    // 1. Enforce required
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for field in required {
            if let Some(name) = field.as_str() {
                if input_obj.get(name).is_none() {
                    anyhow::bail!("missing required input: {name}");
                }
            }
        }
    }

    // 2 & 3. Type-check provided, apply defaults for absent
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        let inputs = match input_obj.as_object_mut() {
            Some(obj) => obj,
            None => {
                return Ok(());
            }
        };

        for (name, prop_schema) in props {
            if let Some(val) = inputs.get(name) {
                if let Some(expected_type) = prop_schema.get("type").and_then(|t| t.as_str()) {
                    let type_ok = match expected_type {
                        "string" => val.is_string(),
                        "number" => val.is_number(),
                        "integer" => val.is_i64() || val.is_u64(),
                        "boolean" => val.is_boolean(),
                        "array" => val.is_array(),
                        "object" => val.is_object(),
                        _ => true,
                    };
                    if !type_ok {
                        anyhow::bail!(
                            "input '{}' expected type '{}', got '{}'",
                            name,
                            expected_type,
                            match val {
                                Value::Null => "null",
                                Value::Bool(_) => "boolean",
                                Value::Number(_) => "number",
                                Value::String(_) => "string",
                                Value::Array(_) => "array",
                                Value::Object(_) => "object",
                            }
                        );
                    }
                }
            } else {
                if let Some(default) = prop_schema.get("default") {
                    inputs.insert(name.clone(), default.clone());
                }
            }
        }

        params["inputs"] = Value::Object(inputs.clone());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_inputs_enforces_required() {
        let schema = json!({
            "required": ["name", "email"],
            "properties": {
                "name": {"type": "string"},
                "email": {"type": "string"},
            }
        });

        let mut params = json!({"inputs": {"name": "test"}});
        let result = normalize_inputs_against_schema(&mut params, &schema);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("email"));

        let mut params = json!({"inputs": {"name": "test", "email": "a@b.com"}});
        assert!(normalize_inputs_against_schema(&mut params, &schema).is_ok());
    }

    #[test]
    fn normalize_inputs_applies_defaults() {
        let schema = json!({
            "properties": {
                "name": {"type": "string"},
                "verbose": {"type": "boolean", "default": false},
            }
        });

        let mut params = json!({"inputs": {"name": "test"}});
        normalize_inputs_against_schema(&mut params, &schema).unwrap();
        assert_eq!(params["inputs"]["verbose"], false);
    }

    #[test]
    fn normalize_inputs_type_checks() {
        let schema = json!({
            "properties": {
                "count": {"type": "integer"},
            }
        });

        let mut params = json!({"inputs": {"count": "not a number"}});
        let result = normalize_inputs_against_schema(&mut params, &schema);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expected type 'integer'"));
    }

    #[test]
    fn normalize_inputs_allows_absent_optional() {
        let schema = json!({
            "properties": {
                "name": {"type": "string"},
                "optional_field": {"type": "string"},
            }
        });

        let mut params = json!({"inputs": {"name": "test"}});
        assert!(normalize_inputs_against_schema(&mut params, &schema).is_ok());
        assert!(params["inputs"].get("optional_field").is_none());
    }

    #[test]
    fn make_error_runtime_result_shapes_correctly() {
        let rr = make_error_runtime_result("T-1", "bad_input", "missing field");
        assert!(!rr.success);
        assert_eq!(rr.status, "bad_input");
        assert_eq!(rr.thread_id, "T-1");
        // D1: result is `Option<Value>`; error string wraps as
        // `Value::String(...)`.
        assert_eq!(rr.result, Some(json!("missing field")));
        assert!(rr.warnings.is_empty());
    }

    #[test]
    fn cli_accepts_project_path_without_error() {
        // F1 pin: the daemon passes --project-path to every native runtime.
        // The graph CLI MUST accept this flag (clap rejects unknown args
        // with a non-zero exit before main runs).
        let cli = Cli::try_parse_from([
            "graph-runtime",
            "--project-path", "/tmp/test-project",
            "--thread-id", "T-f1-test",
            "--pre-registered",
        ]);
        assert!(cli.is_ok(), "graph CLI must accept --project-path");
        let parsed = cli.unwrap();
        assert_eq!(parsed.project_path.as_deref(), Some("/tmp/test-project"));
    }

    #[test]
    fn cli_accepts_all_daemon_spawn_flags() {
        // F1 pin: the full set of flags the daemon passes must parse clean.
        let cli = Cli::try_parse_from([
            "graph-runtime",
            "--project-path", "/tmp/project",
            "--thread-id", "T-full",
            "--pre-registered",
            "--graph-path", "/tmp/graph.yaml",
            "--graph-run-id", "GR-42",
            "--daemon-socket", "/tmp/daemon.sock",
        ]);
        assert!(cli.is_ok(), "graph CLI must accept all daemon flags");
    }

    #[test]
    fn stdout_contract_is_runtime_result_not_graph_result() {
        // Verify that a RuntimeResult round-trips through JSON correctly.
        // D1: `result` carries the typed `GraphResult` value, not a
        // stringified JSON blob. The daemon parses RuntimeResult, so
        // the structured payload survives the wire.
        let graph_result_value = json!({
            "success": true,
            "status": "completed",
        });
        let rr = RuntimeResult {
            success: true,
            status: "completed".into(),
            thread_id: "T-test".into(),
            result: Some(graph_result_value.clone()),
            outputs: Value::Null,
            cost: None,
            warnings: Vec::new(),
        };
        let json_str = serde_json::to_string(&rr).unwrap();
        let parsed: RuntimeResult = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.status, "completed");
        assert_eq!(parsed.result, Some(graph_result_value));
        assert!(parsed.result.is_some());
    }
}
