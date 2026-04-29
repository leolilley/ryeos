//! Hardcoded CLI verbs that run LOCALLY without dispatching to the daemon.
//!
//! These verbs operate on operator state that exists before the daemon is
//! up (`rye init`), or that is operator-tier filesystem state the daemon
//! never owns (`rye trust pin`), or that is publisher-tier authoring state
//! disjoint from any daemon (`rye publish`). They are matched by the
//! dispatcher BEFORE the verb table lookup so they always work even on
//! a fresh checkout with no `core` bundle present.
//!
//! Each verb here parses its own argv slice with clap, runs an action in
//! `ryeos-tools`, and prints a JSON report on success.
//!
//! No daemon round-trip. No verb-table dependency. No trust-store load
//! before keys exist.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use lillux::crypto::{DecodePrivateKey, SigningKey};

use ryeos_tools::actions::init::{run_init, InitOptions};
use ryeos_tools::actions::publish::{run_publish, PublishOptions};
use ryeos_tools::actions::trust::{run_pin, PinOptions};

use crate::error::CliError;

/// Returns `Ok(true)` if the argv was handled by a local verb, `Ok(false)`
/// if no match (caller should fall through to verb-table dispatch).
///
/// Errors from a matched local verb propagate as `CliError::Local`.
pub fn try_dispatch(argv: &[String]) -> Result<bool, CliError> {
    if argv.is_empty() {
        return Ok(false);
    }
    match argv[0].as_str() {
        "init" => {
            run_init_verb(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "publish" => {
            run_publish_verb(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "trust" => {
            // Only `rye trust pin ...` is currently a local verb. If the
            // sub-token isn't `pin`, fall through to verb-table.
            if argv.len() < 2 || argv[1] != "pin" {
                return Ok(false);
            }
            run_trust_pin_verb(&argv[2..]).map_err(map_local_err)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn map_local_err(e: anyhow::Error) -> CliError {
    CliError::Local {
        detail: format!("{e:#}"),
    }
}

// ── rye init ────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "rye init",
    about = "Bootstrap user + node keys, lay down core, pin platform author key",
    no_binary_name = true
)]
struct InitArgs {
    /// Daemon state root. Defaults to XDG state dir / ryeosd.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// User space root (parent of `~/.ai/`). Defaults to $HOME.
    #[arg(long)]
    user_root: Option<PathBuf>,

    /// Where the core bundle should live. Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_data_dir: Option<PathBuf>,

    /// Source tree to copy `core` from (e.g. `/usr/share/ryeos/bundles/core`
    /// in a packaged install, or `ryeos-bundles/core` in dev).
    #[arg(long)]
    core_source: PathBuf,

    /// Source tree to copy `standard` from. Required unless `--core-only`.
    #[arg(long)]
    standard_source: Option<PathBuf>,

    /// Skip installing the standard bundle (positive framing — opt-in to bare core).
    #[arg(long)]
    core_only: bool,

    /// Force-regenerate the node signing key. Does NOT touch the user key.
    #[arg(long)]
    force_node_key: bool,
}

fn run_init_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<InitArgs>(argv)?;
    let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
    let user_root = args.user_root.unwrap_or_else(default_user_root);
    let system_data_dir = args.system_data_dir.unwrap_or_else(default_system_data_dir);

    let opts = InitOptions {
        state_dir,
        user_root,
        system_data_dir,
        core_source: args.core_source,
        standard_source: args.standard_source,
        core_only: args.core_only,
        force_node_key: args.force_node_key,
    };
    let report = run_init(&opts).context("rye init failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── rye trust pin ───────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "rye trust pin",
    about = "Pin a publisher's Ed25519 public key into the operator trust store",
    no_binary_name = true
)]
struct PinArgs {
    /// Expected fingerprint (SHA-256 hex of the raw 32-byte public key).
    fingerprint: String,

    /// Path to a file containing the public key. Accepts PEM, `ed25519:<b64>`, or raw base64.
    #[arg(long)]
    pubkey_file: PathBuf,

    /// Owner label (informational). Defaults to "third-party".
    #[arg(long, default_value = "third-party")]
    owner: String,

    /// User space root (parent of `~/.ai/`). Defaults to $HOME.
    #[arg(long)]
    user_root: Option<PathBuf>,
}

fn run_trust_pin_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<PinArgs>(argv)?;
    let user_root = args.user_root.unwrap_or_else(default_user_root);

    let report = run_pin(&PinOptions {
        user_root,
        expected_fingerprint: args.fingerprint,
        pubkey_file: args.pubkey_file,
        owner: args.owner,
    })
    .context("rye trust pin failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── rye publish ─────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "rye publish",
    about = "Bootstrap-sign + sign-items + rebuild-manifest + emit publisher trust pointer",
    no_binary_name = true
)]
struct PublishArgs {
    /// Bundle source root (the directory containing `.ai/`).
    bundle_source: PathBuf,

    /// Registry root supplying kind schemas + parsers. Defaults to `bundle_source`
    /// (suitable when publishing `core` itself).
    #[arg(long)]
    registry_root: Option<PathBuf>,

    /// Path to a PEM-encoded Ed25519 signing key.
    #[arg(long, conflicts_with = "seed")]
    key: Option<PathBuf>,

    /// Deterministic seed byte (0..=255). Mutually exclusive with --key.
    #[arg(long)]
    seed: Option<u8>,

    /// Suppress emitting `<bundle_source>/PUBLISHER_TRUST.toml`.
    #[arg(long)]
    no_trust_doc: bool,
}

fn run_publish_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<PublishArgs>(argv)?;
    let signing_key = resolve_signing_key(args.key.as_deref(), args.seed)?;
    let registry_root = args.registry_root.unwrap_or_else(|| args.bundle_source.clone());
    let report = run_publish(&PublishOptions {
        bundle_source: args.bundle_source,
        registry_root,
        signing_key,
        emit_trust_doc: !args.no_trust_doc,
    })
    .context("rye publish failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Parse argv with clap, but treat `--help` / `--version` as a successful
/// exit (print to stdout, exit 0) rather than an error. Other parse
/// failures are mapped to anyhow errors that propagate as `CliError::Local`.
fn parse_or_handle_help<P: Parser>(argv: &[String]) -> Result<P> {
    use clap::error::ErrorKind;
    match P::try_parse_from(argv) {
        Ok(p) => Ok(p),
        Err(e) => match e.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                // Clap formatted the help/version text into the error;
                // print it cleanly and exit 0.
                let s = e.render().to_string();
                print!("{s}");
                std::process::exit(0);
            }
            _ => Err(anyhow!("{e}")),
        },
    }
}

fn resolve_signing_key(key: Option<&std::path::Path>, seed: Option<u8>) -> Result<SigningKey> {
    match (key, seed) {
        (Some(_), Some(_)) => Err(anyhow!("pass either --key or --seed, not both")),
        (Some(p), None) => {
            let pem = std::fs::read_to_string(p)
                .with_context(|| format!("read {}", p.display()))?;
            SigningKey::from_pkcs8_pem(&pem)
                .with_context(|| format!("decode {}", p.display()))
        }
        (None, Some(s)) => Ok(SigningKey::from_bytes(&[s; 32])),
        (None, None) => Err(anyhow!("--key <pem> or --seed <0..=255> is required")),
    }
}

// ── Defaults ────────────────────────────────────────────────────────

fn default_state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_STATE_DIR") {
        return PathBuf::from(p);
    }
    dirs::state_dir()
        .map(|d| d.join("ryeosd"))
        .unwrap_or_else(|| PathBuf::from(".ryeosd"))
}

fn default_user_root() -> PathBuf {
    if let Ok(p) = std::env::var("USER_SPACE") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn default_system_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("RYE_SYSTEM_SPACE") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .unwrap_or_else(|| PathBuf::from(".ryeos-data"))
}
