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
use std::sync::Arc;

use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};

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

#[derive(Debug, Deserialize)]
struct LaunchEnvelope {
    #[allow(dead_code)]
    envelope_version: u32,
    #[allow(dead_code)]
    invocation_id: String,
    thread_id: Option<String>,
    target: Option<EnvelopeTarget>,
    roots: Option<EnvelopeRoots>,
    request: Option<EnvelopeRequest>,
    #[allow(dead_code)]
    policy: Option<Value>,
    #[allow(dead_code)]
    callback: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeTarget {
    #[allow(dead_code)]
    item_id: String,
    #[allow(dead_code)]
    kind: String,
    path: String,
    #[allow(dead_code)]
    digest: String,
}

#[derive(Debug, Deserialize)]
struct EnvelopeRoots {
    project_root: PathBuf,
    #[allow(dead_code)]
    user_root: Option<PathBuf>,
    #[allow(dead_code)]
    system_roots: Option<Vec<PathBuf>>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeRequest {
    inputs: Option<Value>,
    #[allow(dead_code)]
    previous_thread_id: Option<String>,
    #[allow(dead_code)]
    parent_thread_id: Option<String>,
    #[allow(dead_code)]
    parent_capabilities: Option<Vec<String>>,
    #[allow(dead_code)]
    depth: Option<u32>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("graph_runtime=info")),
        )
        .init();

    let cli = Cli::parse();

    let mut stdin_data = String::new();
    let has_stdin = !atty_check();
    let envelope: Option<LaunchEnvelope> = if has_stdin {
        std::io::stdin().read_to_string(&mut stdin_data)?;
        serde_json::from_str(&stdin_data).ok()
    } else {
        None
    };

    let project_path = cli.project_path
        .or_else(|| envelope.as_ref().and_then(|e| e.roots.as_ref().map(|r| r.project_root.clone())))
        .ok_or_else(|| anyhow::anyhow!("project-path required (via --project-path or LaunchEnvelope)"))?;

    let graph_path = cli.graph_path
        .or_else(|| envelope.as_ref().and_then(|e| e.target.as_ref().map(|t| PathBuf::from(&t.path))))
        .ok_or_else(|| anyhow::anyhow!("graph-path required (via --graph-path or LaunchEnvelope)"))?;

    let thread_id = envelope
        .as_ref()
        .and_then(|e| e.thread_id.clone())
        .unwrap_or(cli.thread_id.clone());

    let raw = std::fs::read_to_string(&graph_path)?;
    let graph = model::GraphDefinition::from_yaml(
        &raw,
        Some(&graph_path.to_string_lossy()),
    )?;

    if let Some(ref socket) = cli.daemon_socket {
        std::env::set_var("RYEOSD_SOCKET_PATH", socket);
    }
    if let Some(ref env) = envelope {
        if let Some(ref cb) = env.callback {
            if let Some(token) = cb.get("token").and_then(|t| t.as_str()) {
                std::env::set_var("RYEOSD_CALLBACK_TOKEN", token);
            }
            if let Some(socket) = cb.get("socket_path").and_then(|s| s.as_str()) {
                if std::env::var("RYEOSD_SOCKET_PATH").is_err() {
                    std::env::set_var("RYEOSD_SOCKET_PATH", socket);
                }
            }
        }
    }
    let client = rye_runtime::client_from_env();

    if cli.validate {
        let _rt = tokio::runtime::Runtime::new()?;
        let w = walker::Walker::new(
            graph,
            project_path.to_string_lossy().to_string(),
            thread_id,
            Arc::from(client),
        );
        let result = w.validate();
        println!("{}", serde_json::to_string(&result)?);
        if !result.success {
            std::process::exit(1);
        }
        return Ok(());
    }

    let params: Value = envelope
        .as_ref()
        .and_then(|e| e.request.as_ref())
        .map(|r| serde_json::json!({
            "inputs": r.inputs.clone().unwrap_or(json!({})),
            "previous_thread_id": r.previous_thread_id,
            "parent_thread_id": r.parent_thread_id,
            "parent_capabilities": r.parent_capabilities,
            "depth": r.depth.unwrap_or(0),
        }))
        .unwrap_or_else(|| {
            if has_stdin {
                serde_json::from_str(&stdin_data).unwrap_or_default()
            } else {
                json!({})
            }
        });

    if let Some(ref schema) = graph.config.config_schema {
        if let Err(err) = validate_inputs_against_schema(&params, schema) {
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
        project_path.to_string_lossy().to_string(),
        thread_id,
        Arc::from(client),
    );

    let graph_run_id = cli.graph_run_id.or_else(|| {
        envelope.as_ref().and_then(|e| e.thread_id.clone())
    });

    let result = rt.block_on(w.execute(params, graph_run_id));
    println!("{}", serde_json::to_string(&result)?);

    Ok(())
}

fn atty_check() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn validate_inputs_against_schema(inputs: &Value, schema: &Value) -> anyhow::Result<()> {
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        let input_obj = inputs.get("inputs").unwrap_or(inputs);
        for field in required {
            if let Some(name) = field.as_str() {
                if input_obj.get(name).is_none() {
                    anyhow::bail!("missing required input: {name}");
                }
            }
        }
    }

    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        let input_obj = inputs.get("inputs").unwrap_or(inputs);
        for (name, _prop_schema) in props {
            if input_obj.get(name).is_none() {
                anyhow::bail!("input '{name}' missing and no default application available yet");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_inputs_against_schema_required() {
        let schema = json!({
            "required": ["name", "email"],
            "properties": {
                "name": {"type": "string"},
                "email": {"type": "string"},
            }
        });

        let result = validate_inputs_against_schema(&json!({"inputs": {"name": "test"}}), &schema);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("email"));

        let result = validate_inputs_against_schema(
            &json!({"inputs": {"name": "test", "email": "a@b.com"}}),
            &schema,
        );
        assert!(result.is_ok());
    }
}
