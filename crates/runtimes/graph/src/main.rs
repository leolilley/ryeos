mod cache;
mod context;
mod dispatch;
mod edges;
mod env_preflight;
mod evaluation;
#[cfg(test)]
mod expression_inventory_tests;
mod foreach;
mod hooks;
mod model;
mod persistence;
mod resume;
mod walker;

use std::io::Read;
use std::path::PathBuf;

use clap::Parser;
use serde_json::{Value, json};

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::checkpoint::CheckpointWriter;
use ryeos_runtime::envelope::{EnvelopeCallback, RuntimeResult, RuntimeResultStatus};

#[derive(Parser)]
#[command(name = "graph-runtime", about = "Native graph walker for Rye OS")]
struct Cli {
    #[arg(long)]
    graph_run_id: Option<String>,

    #[arg(long)]
    daemon_socket: Option<PathBuf>,

    #[arg(long, env = "RYEOS_THREAD_ID", default_value = "graph-default")]
    thread_id: String,

    #[arg(long)]
    pre_registered: bool,

    /// Accepted for spawn-contract parity with the daemon launcher. Ignored
    /// in favour of `envelope.roots` (which is the single source of truth
    /// per C1).
    #[arg(long)]
    project_path: Option<String>,
}

/// Normalized launch data from the envelope.
struct ResolvedLaunch {
    /// Process-visible workspace for runtime scratch/output. Durable project
    /// and callback authority stay sealed server-side and are never supplied
    /// as a host path to the runtime.
    workspace_root: std::path::PathBuf,
    /// Diagnostic source label only. Executable bytes come exclusively from
    /// the finalized composed resolution carried by the launch envelope.
    graph_source_label: String,
    thread_id: String,
    graph_run_id: Option<String>,
    inputs: Value,
    previous_thread_id: Option<String>,
    parent_thread_id: Option<String>,
    depth: u32,
    hard_limits: Value,
    callback: Option<EnvelopeCallback>,
    target_digest: Option<String>,
    invocation_id: Option<String>,
    resolution: ryeos_engine::resolution::ResolutionOutput,
    effective_definition_digest: ryeos_engine::resolution::EffectiveDefinitionDigest,
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

    let graph = model::GraphDefinition::from_effective_resolution(
        &resolved.resolution,
        &resolved.effective_definition_digest,
        Some(&resolved.graph_source_label),
    )?;

    tracing::info!(
        thread_id = %resolved.thread_id,
        graph_run_id = ?resolved.graph_run_id,
        target_digest = ?resolved.target_digest,
        invocation_id = ?resolved.invocation_id,
        graph_id = %graph.graph_id,
        declared_permissions = ?graph.declared_permissions,
        runtime_capability_requirements = ?graph.runtime_capability_requirements,
        "launch resolved"
    );

    let rt = tokio::runtime::Runtime::new()?;

    let checkpoint = CheckpointWriter::from_env();

    // rye-expr/1 resume is local-checkpoint-only. The checkpoint pins the
    // effective definition digest and language; event replay cannot prove either or
    // reconstruct candidate state and is therefore not a resume source.
    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    let callback = match resolved.callback.as_ref() {
        Some(cb) => CallbackClient::new(cb, &resolved.thread_id, &thread_auth_token),
        None => {
            let cb_env = EnvelopeCallback {
                socket_path: ryeos_runtime::resolve_daemon_socket_path(
                    cli.daemon_socket.as_deref(),
                ),
                token: std::env::var("RYEOSD_CALLBACK_TOKEN")
                    .expect("RYEOSD_CALLBACK_TOKEN must be set by daemon"),
            };
            CallbackClient::new(&cb_env, &resolved.thread_id, &thread_auth_token)
        }
    };

    // Register this process's pgid BEFORE any durable callback — the
    // checkpoint read below, and `execute()` (which can `finalize_thread`
    // on an invalid graph) both happen after this. Without it the daemon
    // cannot tell a live graph from a crashed one on restart and would resume
    // a duplicate. Resume-critical: a graph that cannot register must not run.
    rt.block_on(callback.attach_current_process())?;

    // A resumed rye-expr/1 run requires its identity-bearing local checkpoint.
    // No event-replay fallback exists after the clean language cutover.
    let resume_requested = CheckpointWriter::is_resume();
    let local_checkpoint: Option<Value> = if resume_requested {
        checkpoint
            .as_ref()
            .and_then(|w| w.load_latest().transpose())
            .transpose()?
    } else {
        None
    };
    let resume_state: Option<resume::ResumeState> =
        match resume::decide_resume_source(resume_requested, local_checkpoint.is_some()) {
            resume::ResumeSource::ColdStart => None,
            resume::ResumeSource::LocalCheckpoint => {
                tracing::info!("resuming from local checkpoint");
                Some(resume::from_checkpoint_value(
                    local_checkpoint
                        .as_ref()
                        .expect("LocalCheckpoint variant requires payload"),
                    &graph,
                )?)
            }
            resume::ResumeSource::NoSourceAvailable => {
                anyhow::bail!(
                    "{}: RYEOS_RESUME=1 but thread '{}' has no identity-bearing local \
                 checkpoint; event replay cannot reconstruct rye-expr/1 state or \
                 verify its signed definition; start a new graph run",
                    resume::RESTART_REQUIRED,
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

    // Inject the complete identity-bearing resume DTO. The walker parses this
    // exact shape again and verifies the signed definition/language identity;
    // do not project a weaker cursor-only object here.
    if let Some(ref rs) = resume_state {
        params["resume_state"] = serde_json::to_value(rs)?;
    }

    if let Some(ref schema) = graph.config.config_schema
        && let Err(err) = normalize_inputs_against_schema(&mut params, schema)
    {
        let runtime_result = make_error_runtime_result(
            &resolved.thread_id,
            &format!("input validation failed: {err}"),
        );
        println!("{}", serde_json::to_string(&runtime_result)?);
        std::process::exit(0);
    }

    // Cooperative cancel: SIGTERM sets a flag the walker checks at each node
    // boundary, finalizing `cancelled` cleanly — mirroring the directive
    // runtime's SIGTERM handling. Without this a daemon graceful-cancel SIGTERM
    // would kill the graph process mid-node. `signal_hook::flag::register` sets
    // the atomic at signal-delivery time (async-signal-safe) and replaces the
    // default terminate action; an uncatchable SIGKILL remains the hard-kill
    // backstop when the grace period expires.
    let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, cancel_flag.clone()).map_err(
        |e| anyhow::anyhow!("failed to install SIGTERM cooperative-cancel handler: {e}"),
    )?;

    let w = walker::Walker::new(
        graph,
        resolved.workspace_root.to_string_lossy().to_string(),
        resolved.thread_id.clone(),
        callback,
        checkpoint,
    )
    .with_cancel_flag(cancel_flag);

    let graph_result = rt.block_on(w.execute(params, resolved.graph_run_id));
    // V5.5 P0 #3: pull non-fatal callback drift the walker
    // accumulated during the run. Empty on a clean run.
    let warnings = w.take_warnings();

    // D1 / B5: ship the structured GraphResult through verbatim.
    // Daemon parses RuntimeResult and forwards `result` into the
    // `/execute` response, so HTTP callers see the typed graph
    // result (success/status/state/path) without re-parsing JSON
    // out of a string.
    let status = runtime_status_for_graph(graph_result.status);
    let runtime_result = RuntimeResult {
        success: status.is_success(),
        status,
        thread_id: resolved.thread_id.clone(),
        result: Some(serde_json::to_value(&graph_result)?),
        outputs: serde_json::Value::Null,
        // Surface the graph's aggregate token/spend so the daemon's
        // `/execute` response carries non-null cost for a graph that
        // invoked cost-bearing children (directives/sub-graphs).
        cost: graph_result.cost.clone(),
        warnings,
    };

    println!("{}", serde_json::to_string(&runtime_result)?);

    Ok(())
}

fn resolve_from_envelope(stdin_data: &[u8], cli: &Cli) -> anyhow::Result<ResolvedLaunch> {
    let envelope: ryeos_runtime::envelope::LaunchEnvelope =
        serde_json::from_slice(stdin_data).map_err(|e| anyhow::anyhow!("invalid envelope: {e}"))?;
    if envelope.schema_version()
        != ryeos_engine::launch_envelope_types::MANAGED_LAUNCH_ENVELOPE_SCHEMA_VERSION
    {
        anyhow::bail!("unsupported managed launch envelope schema");
    }
    if !envelope.runtime_data.is_empty() {
        anyhow::bail!(
            "graph launch runtime_data must be empty; received keys: {:?}",
            envelope.runtime_data.keys().collect::<Vec<_>>()
        );
    }

    let target_digest = envelope.resolution().root.raw_content_digest.clone();
    let resolution = envelope.resolution().clone();
    let effective_definition_digest = envelope.effective_definition_digest().clone();

    Ok(ResolvedLaunch {
        workspace_root: envelope.roots.project_root.clone(),
        graph_source_label: envelope
            .resolution()
            .root
            .source_path
            .to_string_lossy()
            .into_owned(),
        thread_id: envelope.thread_id.clone(),
        graph_run_id: cli.graph_run_id.clone(),
        inputs: envelope.request.inputs.clone(),
        previous_thread_id: envelope.request.previous_thread_id.clone(),
        parent_thread_id: envelope.request.parent_thread_id.clone(),
        depth: envelope.request.depth,
        hard_limits: serde_json::to_value(&envelope.policy.hard_limits).unwrap_or(json!({})),
        callback: Some(envelope.callback),
        target_digest: Some(target_digest),
        invocation_id: Some(envelope.invocation_id.clone()),
        resolution,
        effective_definition_digest,
    })
}

fn runtime_status_for_graph(status: model::GraphRunStatus) -> RuntimeResultStatus {
    match status {
        model::GraphRunStatus::Valid
        | model::GraphRunStatus::Completed
        | model::GraphRunStatus::CompletedWithErrors => RuntimeResultStatus::Completed,
        model::GraphRunStatus::Invalid
        | model::GraphRunStatus::Error
        | model::GraphRunStatus::MaxStepsExceeded => RuntimeResultStatus::Failed,
        model::GraphRunStatus::Continued => RuntimeResultStatus::Continued,
        model::GraphRunStatus::Cancelled => RuntimeResultStatus::Cancelled,
        model::GraphRunStatus::Killed => RuntimeResultStatus::Killed,
    }
}

fn make_error_runtime_result(thread_id: &str, error: &str) -> RuntimeResult {
    RuntimeResult {
        success: false,
        status: RuntimeResultStatus::Failed,
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
    let mut input_obj = match params.get("inputs").cloned() {
        Some(Value::Object(obj)) => Value::Object(obj),
        Some(Value::Null) | None => json!({}),
        Some(other) => other,
    };

    // 1. Enforce required
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for field in required {
            if let Some(name) = field.as_str()
                && input_obj.get(name).is_none()
            {
                anyhow::bail!("missing required input: {name}");
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
    fn normalize_inputs_applies_defaults_when_inputs_is_null() {
        let schema = json!({
            "properties": {
                "country": {"type": "string", "default": "US"},
                "max_pages": {"type": "integer", "default": 3},
            }
        });

        let mut params = json!({"inputs": null});
        normalize_inputs_against_schema(&mut params, &schema).unwrap();
        assert_eq!(params["inputs"]["country"], "US");
        assert_eq!(params["inputs"]["max_pages"], 3);
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("expected type 'integer'")
        );
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
        let rr = make_error_runtime_result("T-1", "missing field");
        assert!(!rr.success);
        assert_eq!(rr.status, RuntimeResultStatus::Failed);
        assert_eq!(rr.thread_id, "T-1");
        // D1: result is `Option<Value>`; error string wraps as
        // `Value::String(...)`.
        assert_eq!(rr.result, Some(json!("missing field")));
        assert!(rr.warnings.is_empty());
    }

    #[test]
    fn graph_outcomes_map_to_closed_runtime_terminal_statuses() {
        use model::GraphRunStatus;

        let cases = [
            (GraphRunStatus::Valid, RuntimeResultStatus::Completed),
            (GraphRunStatus::Completed, RuntimeResultStatus::Completed),
            (
                GraphRunStatus::CompletedWithErrors,
                RuntimeResultStatus::Completed,
            ),
            (GraphRunStatus::Invalid, RuntimeResultStatus::Failed),
            (GraphRunStatus::Error, RuntimeResultStatus::Failed),
            (
                GraphRunStatus::MaxStepsExceeded,
                RuntimeResultStatus::Failed,
            ),
            (GraphRunStatus::Continued, RuntimeResultStatus::Continued),
            (GraphRunStatus::Cancelled, RuntimeResultStatus::Cancelled),
            (GraphRunStatus::Killed, RuntimeResultStatus::Killed),
        ];

        for (graph_status, runtime_status) in cases {
            assert_eq!(runtime_status_for_graph(graph_status), runtime_status);
        }
    }

    #[test]
    fn cli_accepts_project_path_without_error() {
        // F1 pin: the daemon passes --project-path to every native runtime.
        // The graph CLI MUST accept this flag (clap rejects unknown args
        // with a non-zero exit before main runs).
        let cli = Cli::try_parse_from([
            "graph-runtime",
            "--project-path",
            "/tmp/test-project",
            "--thread-id",
            "T-f1-test",
            "--pre-registered",
        ]);
        assert!(cli.is_ok(), "graph CLI must accept --project-path");
        let parsed = cli.unwrap();
        assert_eq!(parsed.project_path.as_deref(), Some("/tmp/test-project"));
    }

    #[test]
    fn cli_accepts_current_daemon_spawn_flags() {
        // F1 pin: the full set of flags the daemon passes must parse clean.
        // Graph bytes are carried only in the verified launch envelope.
        let cli = Cli::try_parse_from([
            "graph-runtime",
            "--project-path",
            "/tmp/project",
            "--thread-id",
            "T-full",
            "--pre-registered",
            "--graph-run-id",
            "GR-42",
            "--daemon-socket",
            "/tmp/daemon.sock",
        ]);
        assert!(cli.is_ok(), "graph CLI must accept all daemon flags");
        assert_eq!(
            cli.unwrap().daemon_socket.as_deref(),
            Some(std::path::Path::new("/tmp/daemon.sock"))
        );
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
            status: RuntimeResultStatus::Completed,
            thread_id: "T-test".into(),
            result: Some(graph_result_value.clone()),
            outputs: Value::Null,
            cost: None,
            warnings: Vec::new(),
        };
        let json_str = serde_json::to_string(&rr).unwrap();
        let parsed: RuntimeResult = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.status, RuntimeResultStatus::Completed);
        assert_eq!(parsed.result, Some(graph_result_value));
        assert!(parsed.result.is_some());
    }
}
