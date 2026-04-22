//! rye-gc — garbage collect unused CAS objects

use anyhow::{Context, Result};
use clap::Parser;
use rye_core_tools::get_state_root;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rye-gc")]
#[command(about = "Garbage collect CAS objects (maintenance, daemon must be stopped)")]
struct Args {
    /// RYE_STATE directory
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Dry run (don't delete)
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct GcReport {
    roots_walked: usize,
    reachable_objects: usize,
    reachable_blobs: usize,
    deleted_objects: usize,
    deleted_blobs: usize,
    freed_bytes: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let state_root = args.state_dir.or_else(|| get_state_root().ok())
        .context("RYE_STATE not set and --state-dir not provided")?;

    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");

    if args.dry_run {
        eprintln!("Dry run mode — no files will be deleted.");
    }

    let result = rye_state::gc::run_gc(&cas_root, &refs_root, args.dry_run)?;

    let report = GcReport {
        roots_walked: result.roots_walked,
        reachable_objects: result.reachable_objects,
        reachable_blobs: result.reachable_blobs,
        deleted_objects: result.deleted_objects,
        deleted_blobs: result.deleted_blobs,
        freed_bytes: result.freed_bytes,
    };

    println!(
        "Roots walked: {} (chains + projects)",
        report.roots_walked
    );
    println!("Reachable objects: {}", report.reachable_objects);
    println!("Reachable blobs: {}", report.reachable_blobs);
    println!("Deleted objects: {}", report.deleted_objects);
    println!("Deleted blobs: {}", report.deleted_blobs);
    println!("Freed: {} bytes", report.freed_bytes);

    Ok(())
}
