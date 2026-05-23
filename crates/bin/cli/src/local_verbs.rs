//! Hardcoded CLI verbs that run LOCALLY without dispatching to the daemon.
//!
//! Only lifecycle verbs live here — the absolute minimum needed to manage
//! the local node before the daemon exists or is reachable:
//!
//!   - `ryeos init`   — bootstrap operator keys, trust store, and bundles
//!   - `ryeos start`  — bring the local node runtime online
//!   - `ryeos stop`   — gracefully stop the local node runtime
//!   - `ryeos status` — show local node lifecycle status
//!
//! `ryeos identity public-key` is local as a bootstrap affordance: remote
//! operators need to copy their node public key before the daemon is running.
//!
//! All other commands — including `sign`, `verify`, `fetch` — are
//! descriptor-driven and dispatched through the offline/dual path
//! (see `offline_dispatch.rs`) or forwarded to the daemon.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use ryeos_node::{LifecycleController, LifecycleStatus, LocalLifecycleEnv, StopOptions};

use crate::error::CliError;

/// Returns `Ok(true)` if the argv was handled by a local verb, `Ok(false)`
/// if no match (caller should fall through to verb-table dispatch).
///
/// Errors from a matched local verb propagate as `CliError::Local`.
pub async fn try_dispatch(argv: &[String]) -> Result<bool, CliError> {
    if argv.is_empty() {
        return Ok(false);
    }
    match argv[0].as_str() {
        "identity" if argv.get(1).map(|s| s.as_str()) == Some("public-key") => {
            run_identity_public_key_verb(&argv[2..]).map_err(map_local_err)?;
            Ok(true)
        }
        "init" => {
            run_init_verb(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "status" => {
            run_status_verb(&argv[1..]).await.map_err(map_local_err)?;
            Ok(true)
        }
        "start" => {
            run_start_verb(&argv[1..]).await.map_err(map_local_err)?;
            Ok(true)
        }
        "stop" => {
            run_stop_verb(&argv[1..]).await.map_err(map_local_err)?;
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

// ── ryeos identity public-key ─────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos identity public-key",
    about = "Print the local node public identity without contacting the daemon",
    no_binary_name = true
)]
struct IdentityPublicKeyArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

fn run_identity_public_key_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<IdentityPublicKeyArgs>(argv)?;
    let report = ryeos_tools::actions::inspect::identity::run_identity(
        ryeos_tools::actions::inspect::identity::IdentityParams {
            system_space_dir: args
                .system_space_dir
                .map(|p| p.to_string_lossy().into_owned()),
        },
    )
    .context("ryeos identity public-key failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── ryeos init ──────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos init",
    about = "Bootstrap user + node keys, discover and install bundles, pin publisher keys",
    no_binary_name = true
)]
struct InitArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,

    /// User space root (parent of `.ai/`). Defaults to the canonical user root.
    #[arg(long)]
    user_root: Option<PathBuf>,

    /// Source directory containing bundle subdirectories.
    /// Each immediate child with a `.ai/` subdirectory is installed as a bundle.
    /// Defaults to `/usr/share/ryeos` (packaged install).
    /// Override for dev (`bundles`), Docker (`/opt/ryeos`), etc.
    #[arg(long, default_value = "/usr/share/ryeos")]
    source: PathBuf,

    /// Additional publisher trust doc(s) to pin before verifying bundles.
    /// Each file should be a PUBLISHER_TRUST.toml with public_key and fingerprint.
    /// Repeatable: `--trust-file a.toml --trust-file b.toml`.
    /// Note: trust docs are also auto-discovered from bundle roots.
    #[arg(long = "trust-file", action = clap::ArgAction::Append)]
    trust_files: Vec<PathBuf>,
}

fn run_init_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<InitArgs>(argv)?;
    let system_space_dir = args
        .system_space_dir
        .unwrap_or_else(default_system_space_dir);
    let user_root = args.user_root.unwrap_or_else(default_user_root);

    let opts = ryeos_node::InitOptions {
        system_space_dir,
        user_root,
        source_dir: args.source,
        trust_files: args.trust_files,
        skip_preflight: false,
    };
    let report = ryeos_node::run_init(&opts).context("ryeos init failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── ryeos {status,start,stop} ───────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos status",
    about = "Show local node lifecycle status",
    no_binary_name = true
)]
struct StatusArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,

    /// Emit structured JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

async fn run_status_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StatusArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.system_space_dir)?);
    let status = controller.status().await.context("ryeos status failed")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_lifecycle_status(&status);
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos start",
    about = "Bring the local node runtime online",
    no_binary_name = true
)]
struct StartArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,
}

async fn run_start_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StartArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.system_space_dir)?);
    let report = controller.start().await.context("ryeos start failed")?;
    if report.already_running {
        println!("running");
    } else {
        println!("started");
    }
    print_lifecycle_status(&report.status);
    Ok(())
}

// ── ryeos stop ──────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos stop",
    about = "Gracefully stop the local node runtime",
    no_binary_name = true
)]
struct StopArgs {
    /// System space root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    system_space_dir: Option<PathBuf>,

    /// Fall back to signaling the confirmed live ryeosd process if graceful shutdown times out.
    #[arg(long)]
    force: bool,
}

async fn run_stop_verb(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StopArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.system_space_dir)?);
    let report = controller
        .stop(StopOptions {
            force: args.force,
            ..StopOptions::default()
        })
        .await
        .context("ryeos stop failed")?;
    if report.already_stopped {
        println!("already stopped");
    } else {
        println!("stopped");
    }
    print_lifecycle_status(&report.status);
    Ok(())
}

fn local_env(system_space_dir: Option<PathBuf>) -> Result<LocalLifecycleEnv> {
    LocalLifecycleEnv::load(system_space_dir)
}

fn print_lifecycle_status(status: &LifecycleStatus) {
    match status {
        LifecycleStatus::NotInitialized { diagnostics } => {
            println!("not initialized — run: ryeos init");
            println!("detail: {}", diagnostics.message);
        }
        LifecycleStatus::Stopped { system_space_dir } => {
            println!("initialized, stopped — run: ryeos start");
            println!("system space: {}", system_space_dir.display());
        }
        LifecycleStatus::Running { metadata } => {
            println!("running");
            if let Some(pid) = metadata.pid {
                println!("pid: {pid}");
            }
            if let Some(bind) = &metadata.bind {
                println!("url: http://{bind}");
            }
            if let Some(socket) = &metadata.uds_path {
                println!("socket: {}", socket.display());
            }
        }
        LifecycleStatus::Stale { diagnostics, .. } => {
            println!("stale daemon metadata — {}", diagnostics.message);
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse argv with clap, but treat `--help` / `--version` as a successful
/// exit (print to stdout, exit 0) rather than an error. Other parse
/// failures are mapped to anyhow errors that propagate as `CliError::Local`.
fn parse_or_handle_help<P: Parser>(argv: &[String]) -> Result<P> {
    use clap::error::ErrorKind;
    match P::try_parse_from(argv) {
        Ok(p) => Ok(p),
        Err(e) => match e.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                let s = e.render().to_string();
                print!("{s}");
                std::process::exit(0);
            }
            _ => Err(anyhow::anyhow!("{e}")),
        },
    }
}

fn default_system_space_dir() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .expect("could not determine XDG data directory")
}

fn default_user_root() -> PathBuf {
    ryeos_engine::roots::user_root()
        .ok()
        .unwrap_or_else(|| PathBuf::from("."))
}
