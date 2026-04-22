//! rye-status — inspect daemon/CAS state
//! 
//! Minimal deps. Shows:
//! - RYE_STATE root
//! - Chain count
//! - Projection status
//! - Last sync time

use anyhow::{Context, Result};
use clap::Parser;
use rye_core_tools::{get_state_root, output_info, StatusReport};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rye-status")]
#[command(about = "Show daemon and CAS state status", long_about = None)]
struct Args {
    /// Output format (json or text)
    #[arg(short, long, default_value = "text")]
    format: String,

    /// RYE_STATE directory (overrides env var)
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let state_root = args.state_dir.or_else(|| get_state_root().ok())
        .context("RYE_STATE not set and --state-dir not provided")?;

    let status = gather_status(&state_root)?;

    match args.format.as_str() {
        "json" => {
            let json = serde_json::to_string_pretty(&status)?;
            println!("{}", json);
        }
        "text" => {
            output_info(&format!("RYE_STATE: {}", state_root.display()));
            output_info(&format!("CAS root: {}", status.cas_root));
            output_info(&format!("Chains: {}", status.chains_count));
            output_info(&format!("Projection: {}", status.projection_status));
            output_info(&format!("Last updated: {}", status.last_updated));
        }
        _ => anyhow::bail!("unknown format: {}", args.format),
    }

    Ok(())
}

fn gather_status(state_root: &std::path::Path) -> Result<StatusReport> {
    let cas_root = state_root.join(".state/objects");
    let chains_count = count_chains(state_root).unwrap_or(0);
    let projection_path = state_root.join(".state/projection.sqlite3");
    let projection_status = if projection_path.exists() {
        "ok".to_string()
    } else {
        "missing".to_string()
    };

    Ok(StatusReport {
        rye_state: state_root.display().to_string(),
        cas_root: cas_root.display().to_string(),
        chains_count,
        last_updated: chrono::Utc::now().to_rfc3339(),
        projection_status,
    })
}

fn count_chains(state_root: &std::path::Path) -> Result<usize> {
    let refs_dir = state_root.join(".state/refs");
    if !refs_dir.exists() {
        return Ok(0);
    }
    let count = std::fs::read_dir(&refs_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext == "signed")
                .unwrap_or(false)
        })
        .count();
    Ok(count)
}
