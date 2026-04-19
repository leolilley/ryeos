mod cache;
mod context;
mod dispatch;
mod edges;
mod foreach;
mod model;
mod permissions;
mod validation;
mod walker;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use serde_json::Value;

#[derive(Parser)]
#[command(name = "graph-runtime", about = "Native graph walker for Rye OS")]
struct Cli {
    /// Path to the graph YAML file
    #[arg(long)]
    graph_path: PathBuf,

    /// Path to the project root
    #[arg(long)]
    project_path: PathBuf,

    /// JSON parameters (passed as string)
    #[arg(long, default_value = "{}")]
    params: String,

    /// Validate only (no execution)
    #[arg(long)]
    validate: bool,

    /// Graph run ID (for resume)
    #[arg(long)]
    graph_run_id: Option<String>,

    /// Daemon socket path (overrides RYEOSD_SOCKET_PATH)
    #[arg(long)]
    daemon_socket: Option<String>,

    /// Thread ID
    #[arg(long, env = "RYE_THREAD_ID", default_value = "graph-default")]
    thread_id: String,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("graph_runtime=info")),
        )
        .init();

    let cli = Cli::parse();

    let raw = std::fs::read_to_string(&cli.graph_path)?;
    let graph = model::GraphDefinition::from_yaml(
        &raw,
        Some(&cli.graph_path.to_string_lossy()),
    )?;

    let params: Value = serde_json::from_str(&cli.params)?;

    if let Some(ref socket) = cli.daemon_socket {
        std::env::set_var("RYEOSD_SOCKET_PATH", socket);
    }
    let client = rye_runtime::client_from_env();

    let rt = tokio::runtime::Runtime::new()?;
    let w = walker::Walker::new(
        graph,
        cli.project_path.to_string_lossy().to_string(),
        cli.thread_id,
        Arc::from(client),
    );

    if cli.validate {
        let result = w.validate();
        println!("{}", serde_json::to_string(&result)?);
        if !result.success {
            std::process::exit(1);
        }
        return Ok(());
    }

    let result = rt.block_on(w.execute(params, cli.graph_run_id));
    println!("{}", serde_json::to_string(&result)?);

    if !result.success {
        std::process::exit(1);
    }

    Ok(())
}
