//! rye-rebuild — rebuild projection from CAS

use anyhow::{Context, Result};
use clap::Parser;
use ryeos_tools::get_state_root;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rye-rebuild")]
#[command(about = "Rebuild projection.sqlite3 from CAS (maintenance, daemon must be stopped)")]
struct Args {
    /// RYE_STATE directory
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Verify integrity after rebuild
    #[arg(long)]
    verify: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let state_root = args.state_dir.or_else(|| get_state_root().ok())
        .context("RYE_STATE not set and --state-dir not provided")?;

    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");
    let projection_path = state_root.join("projection.sqlite3");

    // Rebuild
    let projection = ryeos_state::ProjectionDb::open(&projection_path)?;
    let report = ryeos_state::rebuild::rebuild_projection(&projection, &cas_root, &refs_root)?;

    println!("Chains rebuilt: {}", report.chains_rebuilt);
    println!("Threads restored: {}", report.threads_restored);
    println!("Events projected: {}", report.events_projected);

    // Optionally verify
    if args.verify && report.chains_rebuilt > 0 {
        println!("\nVerifying reachability...");
        let reachable = ryeos_state::reachability::collect_reachable(&cas_root, &refs_root)?;
        println!(
            "Reachable objects: {}, blobs: {}",
            reachable.object_hashes.len(),
            reachable.blob_hashes.len(),
        );
    }

    Ok(())
}
