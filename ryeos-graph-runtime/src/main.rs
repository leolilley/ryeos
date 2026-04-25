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
mod permissions;
mod resume;
mod validation;
mod walker;

use std::io::Read;
use std::path::PathBuf;

use clap::Parser;
use serde_json::{json, Value};

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::{EnvelopeCallback, ENVELOPE_VERSION};

#[derive(Parser)]
#[command(name = "graph-runtime", about = "Native graph walker for Rye OS")]
struct Cli {
    #[arg(long)]
    graph_path: Option<PathBuf>,

    #[arg(long)]
    project_path: Option<PathBuf>,

    #[arg(long)]
    validate: bool,

    #[arg(long)]
    graph_run_id: Option<String>,

    #[arg(long)]
    daemon_socket: Option<String>,

    #[arg(long, env = "RYE_THREAD_ID", default_value = "graph-default")]
    thread_id: String,

    #[arg(long)]
    pre_registered: bool,
}

/// Normalized launch data from either envelope or CLI flags.
struct ResolvedLaunch {
    project_root: PathBuf,
    graph_path: PathBuf,
    thread_id: String,
    graph_run_id: Option<String>,
    inputs: Value,
    previous_thread_id: Option<String>,
    parent_thread_id: Option<String>,
    parent_capabilities: Vec<String>,
    depth: u32,
    effective_caps: Vec<String>,
    hard_limits: Value,
    callback: Option<EnvelopeCallback>,
    target_digest: Option<String>,
    user_root: Option<PathBuf>,
    system_roots: Vec<PathBuf>,
    invocation_id: Option<String>,
}

fn main() -> anyhow::Result<()> {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_graph_runtime());

    let cli = Cli::parse();

    let mut stdin_data = Vec::new();
    std::io::stdin().read_to_end(&mut stdin_data)?;
    let has_stdin = !stdin_data.is_empty();

    let resolved = if has_stdin {
        resolve_from_envelope(&stdin_data, &cli)?
    } else {
        resolve_from_cli(&cli)?
    };

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
        effective_caps = ?resolved.effective_caps,
        "launch resolved"
    );

    let callback = match resolved.callback {
        Some(ref cb) => CallbackClient::new(
            cb,
            &resolved.thread_id,
            resolved.project_root.to_str().unwrap_or(""),
        ),
        None => {
            // CLI mode: construct from env or fallback
            if let Some(ref socket) = cli.daemon_socket {
                std::env::set_var("RYEOSD_SOCKET_PATH", socket);
            }
            let cb_env = EnvelopeCallback {
                socket_path: ryeos_runtime::resolve_daemon_socket_path(None),
                token: std::env::var("RYEOSD_CALLBACK_TOKEN").unwrap_or_default(),
            };
            CallbackClient::new(
                &cb_env,
                &resolved.thread_id,
                resolved.project_root.to_str().unwrap_or(""),
            )
        }
    };

    if cli.validate {
        let _rt = tokio::runtime::Runtime::new()?;
        let w = walker::Walker::new(
            graph,
            resolved.project_root.to_string_lossy().to_string(),
            resolved.thread_id,
            callback,
        );
        let result = w.validate();
        println!("{}", serde_json::to_string(&result)?);
        if !result.success {
            std::process::exit(1);
        }
        return Ok(());
    }

    let mut params = json!({
        "inputs": resolved.inputs,
        "previous_thread_id": resolved.previous_thread_id,
        "parent_thread_id": resolved.parent_thread_id,
        "parent_capabilities": resolved.parent_capabilities,
        "depth": resolved.depth,
        "effective_caps": resolved.effective_caps,
        "hard_limits": resolved.hard_limits,
    });

    if let Some(ref schema) = graph.config.config_schema {
        if let Err(err) = normalize_inputs_against_schema(&mut params, schema) {
            let result = model::GraphResult {
                success: false,
                graph_id: graph.graph_id.clone(),
                graph_run_id: String::new(),
                status: "invalid_inputs".into(),
                steps: 0,
                state: serde_json::json!({}),
                result: None,
                errors_suppressed: None,
                errors: None,
                error: Some(format!("input validation failed: {err}")),
            };
            println!("{}", serde_json::to_string(&result)?);
            std::process::exit(0);
        }
    }

    let rt = tokio::runtime::Runtime::new()?;
    let w = walker::Walker::new(
        graph,
        resolved.project_root.to_string_lossy().to_string(),
        resolved.thread_id,
        callback,
    );

    let result = rt.block_on(w.execute(params, resolved.graph_run_id));
    println!("{}", serde_json::to_string(&result)?);

    Ok(())
}

fn resolve_from_envelope(stdin_data: &[u8], cli: &Cli) -> anyhow::Result<ResolvedLaunch> {
    let envelope: ryeos_runtime::envelope::LaunchEnvelope = serde_json::from_slice(stdin_data)
        .map_err(|e| anyhow::anyhow!("invalid envelope: {e}"))?;

    if envelope.envelope_version != ENVELOPE_VERSION {
        anyhow::bail!(
            "unsupported envelope version: {} (expected {})",
            envelope.envelope_version,
            ENVELOPE_VERSION,
        );
    }

    let graph_path = cli.graph_path.clone()
        .unwrap_or_else(|| envelope.roots.project_root.join(&envelope.target.path));

    Ok(ResolvedLaunch {
        project_root: envelope.roots.project_root.clone(),
        graph_path,
        thread_id: envelope.thread_id.clone(),
        graph_run_id: cli.graph_run_id.clone(),
        inputs: envelope.request.inputs.clone(),
        previous_thread_id: envelope.request.previous_thread_id.clone(),
        parent_thread_id: envelope.request.parent_thread_id.clone(),
        parent_capabilities: envelope.request.parent_capabilities.unwrap_or_default(),
        depth: envelope.request.depth,
        effective_caps: envelope.policy.effective_caps.clone(),
        hard_limits: serde_json::to_value(&envelope.policy.hard_limits).unwrap_or(json!({})),
        callback: Some(envelope.callback),
        target_digest: Some(envelope.target.digest.clone()),
        user_root: envelope.roots.user_root.clone(),
        system_roots: envelope.roots.system_roots.clone(),
        invocation_id: Some(envelope.invocation_id.clone()),
    })
}

fn resolve_from_cli(cli: &Cli) -> anyhow::Result<ResolvedLaunch> {
    let project_path = cli.project_path.clone()
        .ok_or_else(|| anyhow::anyhow!("project-path required via --project-path or envelope"))?;
    let graph_path = cli.graph_path.clone()
        .ok_or_else(|| anyhow::anyhow!("graph-path required via --graph-path or envelope"))?;

    Ok(ResolvedLaunch {
        project_root: project_path,
        graph_path,
        thread_id: cli.thread_id.clone(),
        graph_run_id: cli.graph_run_id.clone(),
        inputs: json!({}),
        previous_thread_id: None,
        parent_thread_id: None,
        parent_capabilities: vec![],
        depth: 0,
        effective_caps: vec![],
        hard_limits: json!({}),
        callback: None,
        target_digest: None,
        user_root: None,
        system_roots: vec![],
        invocation_id: None,
    })
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
                // inputs is not an object — skip property checks
                return Ok(());
            }
        };

        for (name, prop_schema) in props {
            if let Some(val) = inputs.get(name) {
                // 2. Type-check provided value
                if let Some(expected_type) = prop_schema.get("type").and_then(|t| t.as_str()) {
                    let type_ok = match expected_type {
                        "string" => val.is_string(),
                        "number" => val.is_number(),
                        "integer" => val.is_i64() || val.is_u64(),
                        "boolean" => val.is_boolean(),
                        "array" => val.is_array(),
                        "object" => val.is_object(),
                        _ => true, // unknown type, pass through
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
                // 3. Apply default if absent
                if let Some(default) = prop_schema.get("default") {
                    inputs.insert(name.clone(), default.clone());
                }
                // Absent + no default + not required → ignore
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
}
