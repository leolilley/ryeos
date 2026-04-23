//! rye-bundle — bundle management (install, list, remove)

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rye-bundle")]
#[command(about = "Bundle management (maintenance)")]
struct Args {
    #[command(subcommand)]
    command: BundleCommand,
}

#[derive(Subcommand)]
enum BundleCommand {
    /// Install a bundle
    Install {
        /// Bundle name
        name: String,

        /// Bundle path
        #[arg(short, long)]
        path: PathBuf,
    },
    /// List installed bundles
    List,
    /// Remove a bundle
    Remove {
        /// Bundle name
        name: String,
    },
}

fn main() -> Result<()> {
    anyhow::bail!("rye-bundle is not yet implemented")
}
