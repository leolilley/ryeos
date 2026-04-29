//! Maintainer-only bundle manifest and signing tool.
//!
//! This is **not** part of the data-driven `rye` CLI's verb plane. It is
//! a publisher/operator workflow that operates on a bundle source tree
//! (e.g. `ryeos-bundles/standard/`).
//!
//! Verbs:
//!   rebuild-manifest — rebuild CAS blobs, MANIFEST.json, refs
//!   sign-items       — batch-sign all items in a bundle with the author key
//!
//! Item signing uses the explicit `--key` and `--registry-root` flags.
//! No ambient machine state — the author key and the kind schema root
//! must be provided explicitly.
//!
//! Usage:
//!     rye-bundle-tool rebuild-manifest --source <bundle-root> \
//!         (--seed <0..=255> | --key <pem-path>)
//!     rye-bundle-tool sign-items --source <bundle-root> \
//!         --registry-root <signed-core> (--seed <0..=255> | --key <pem-path>)

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rye-bundle-tool",
    about = "Maintainer-only bundle manifest tool (not part of the rye CLI verb plane)",
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

    /// Batch-sign every signable item in a bundle using the author key.
    /// Validates metadata anchoring before signing. Refuses on first failure.
    SignItems {
        /// Bundle root (the directory that contains `.ai/`) to sign.
        #[arg(long)]
        source: PathBuf,

        /// Signed bundle providing kind schemas for validation (e.g. signed core).
        /// When signing core's own items, pass --registry-root pointing at the same
        /// core bundle (after its kind schemas are bootstrap-signed).
        #[arg(long)]
        registry_root: PathBuf,

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
            let signing_key = resolve_key("rebuild-manifest", key.as_deref(), seed)?;
            let report = ryeos_tools::actions::build_bundle::rebuild_bundle_manifest(
                &source,
                &signing_key,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::SignItems {
            source,
            registry_root,
            key,
            seed,
        } => {
            let signing_key = resolve_key("sign-items", key.as_deref(), seed)?;
            let report = ryeos_tools::actions::sign_bundle::sign_bundle_items(
                &source,
                &registry_root,
                &signing_key,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.failed.is_empty() {
                Ok(())
            } else {
                anyhow::bail!(
                    "sign-items failed for {} of {} item(s)",
                    report.failed.len(),
                    report.total()
                );
            }
        }
    }
}

fn resolve_key(
    verb: &str,
    key: Option<&std::path::Path>,
    seed: Option<u8>,
) -> anyhow::Result<lillux::crypto::SigningKey> {
    match (key, seed) {
        (Some(_), Some(_)) => anyhow::bail!("{verb}: pass either --key or --seed, not both"),
        (Some(p), None) => ryeos_tools::actions::build_bundle::load_signing_key(p),
        (None, Some(s)) => Ok(ryeos_tools::actions::build_bundle::signing_key_from_seed(s)),
        (None, None) => anyhow::bail!("{verb}: --key <pem> or --seed <0..=255> is required"),
    }
}
