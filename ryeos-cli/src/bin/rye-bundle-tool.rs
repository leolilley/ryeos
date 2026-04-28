//! Maintainer-only bundle authoring tool.
//!
//! This is **not** part of the data-driven `rye` CLI's verb plane. It is
//! a publisher/operator workflow that operates on a bundle source tree
//! (e.g. `ryeos-bundles/standard/`) — it walks `<bundle>/.ai/bin/<triple>/`,
//! rebuilds CAS blobs / `ItemSource` objects / per-triple `MANIFEST.json`
//! sidecars, the top-level `SourceManifest`, and the
//! `<bundle>/.ai/refs/bundles/manifest` ref.
//!
//! The daemon never runs this; `service:*` is the wrong place because
//! `service:*` operates on `state_dir` (a daemon/node concept), whereas
//! this rebuild operates on a publisher source tree.
//!
//! Usage:
//!     rye-bundle-tool rebuild-manifest --source <bundle-root> \
//!         (--seed <0..=255> | --key <pem-path>)

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rye-bundle-tool",
    about = "Maintainer-only bundle authoring tool (not part of the rye CLI verb plane)",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Rebuild a bundle's MANIFEST.json + refs/bundles/manifest + CAS objects
    /// from the binaries on disk under `<source>/.ai/bin/<triple>/*`.
    RebuildManifest {
        /// Bundle root (the directory that contains `.ai/`).
        #[arg(long)]
        source: PathBuf,

        /// Path to a PEM-encoded Ed25519 signing key. Mutually exclusive with --seed.
        #[arg(long, conflicts_with = "seed")]
        key: Option<PathBuf>,

        /// Deterministic signing key seed byte (0..=255). Mutually exclusive with --key.
        #[arg(long)]
        seed: Option<u8>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rye-bundle-tool: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::RebuildManifest { source, key, seed } => {
            let signing_key = match (key.as_ref(), seed) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("rebuild-manifest: pass either --key or --seed, not both")
                }
                (Some(p), None) => ryeos_tools::actions::build_bundle::load_signing_key(p)?,
                (None, Some(s)) => ryeos_tools::actions::build_bundle::signing_key_from_seed(s),
                (None, None) => anyhow::bail!(
                    "rebuild-manifest: --key <pem> or --seed <0..=255> is required"
                ),
            };
            let report = ryeos_tools::actions::build_bundle::rebuild_bundle_manifest(
                &source,
                &signing_key,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}
