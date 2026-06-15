//! Hardcoded CLI commands that run LOCALLY without dispatching to the daemon.
//!
//! Only lifecycle commands live here — the absolute minimum needed to manage
//! the local node before the daemon exists or is reachable:
//!
//!   - `ryeos init`   — bootstrap operator keys, trust store, and bundles
//!   - `ryeos start`  — bring the local node runtime online
//!   - `ryeos stop`   — gracefully stop the local node runtime
//!   - `ryeos node status` — show local node lifecycle status
//!
//! `ryeos identity` is local as a bootstrap affordance: remote
//! operators need to copy their node public key before the daemon is running.
//!
//! All other commands — including `sign`, `verify`, `fetch` — are
//! descriptor-driven and dispatched through the offline/dual path
//! (see `offline_dispatch.rs`) or forwarded to the daemon.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use ryeos_node::{LifecycleController, LifecycleStatus, LocalLifecycleEnv, StopOptions};

use crate::error::CliError;

/// Returns `Ok(true)` if the argv was handled by a lifecycle command, `Ok(false)`
/// if no lifecycle command matched.
///
/// Errors from a matched lifecycle command propagate as `CliError::Local`.
pub async fn try_dispatch(argv: &[String]) -> Result<bool, CliError> {
    if argv.is_empty() {
        return Ok(false);
    }
    match argv[0].as_str() {
        "identity" => {
            run_identity_command(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "init" => {
            run_init_command(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        "node" | "system" if argv.get(1).map(String::as_str) == Some("status") => {
            run_status_command(&argv[2..])
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        "start" => {
            run_start_command(&argv[1..]).await.map_err(map_local_err)?;
            Ok(true)
        }
        "stop" => {
            run_stop_command(&argv[1..]).await.map_err(map_local_err)?;
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

// ── ryeos identity ────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos identity",
    about = "Print the local node public identity without contacting the daemon",
    no_binary_name = true
)]
struct IdentityArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,
}

fn run_identity_command(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<IdentityArgs>(argv)?;
    let report = ryeos_tools::actions::inspect::identity::run_identity(
        ryeos_tools::actions::inspect::identity::IdentityParams {
            app_root: args.app_root.map(|p| p.to_string_lossy().into_owned()),
            project_path: None,
        },
    )
    .context("ryeos identity failed")?;
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
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Source directory containing bundle subdirectories.
    /// Each immediate child with a `.ai/` subdirectory is installed as a bundle.
    /// Defaults to `/usr/share/ryeos` (packaged install).
    /// Override for dev (`bundles`), Docker (`/opt/ryeos`), etc.
    #[arg(long, default_value = "/usr/share/ryeos")]
    source: PathBuf,

    /// Additional publisher trust doc(s) to pin before verifying bundles.
    /// Each file should be a PUBLISHER_TRUST.toml with public_key and fingerprint.
    /// Repeatable: `--trust-file a.toml --trust-file b.toml`.
    /// Non-official/dev publisher keys must be supplied explicitly.
    #[arg(long = "trust-file", action = clap::ArgAction::Append)]
    trust_files: Vec<PathBuf>,
}

fn run_init_command(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<InitArgs>(argv)?;
    let app_root = args.app_root.unwrap_or_else(default_app_root);

    let opts = ryeos_node::InitOptions {
        app_root,
        source_dir: args.source,
        trust_files: args.trust_files,
        skip_preflight: false,
    };
    let report = ryeos_node::run_init(&opts).context("ryeos init failed")?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// ── ryeos {node status,start,stop} ──────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node status",
    about = "Show local node lifecycle status",
    no_binary_name = true
)]
struct StatusArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit structured JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

async fn run_status_command(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StatusArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
    let status = controller
        .status()
        .await
        .context("ryeos node status failed")?;
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
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// TCP bind address for ryeosd, e.g. 127.0.0.1:17400.
    /// Overrides stored config for this start invocation.
    #[arg(long)]
    bind: Option<SocketAddr>,

    /// Lifecycle/control Unix socket path for ryeosd.
    /// Useful when running a second local daemon alongside the default node.
    #[arg(long)]
    uds_path: Option<PathBuf>,
}

async fn run_start_command(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StartArgs>(argv)?;
    let env =
        LocalLifecycleEnv::load_with_overrides(args.app_root, args.bind, args.uds_path, true)?;
    let controller = LifecycleController::from_env(env);
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
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Fall back to signaling the confirmed live ryeosd process if graceful shutdown times out.
    #[arg(long)]
    force: bool,
}

async fn run_stop_command(argv: &[String]) -> Result<()> {
    let args = parse_or_handle_help::<StopArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
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

fn local_env(app_root: Option<PathBuf>) -> Result<LocalLifecycleEnv> {
    LocalLifecycleEnv::load(app_root)
}

fn print_lifecycle_status(status: &LifecycleStatus) {
    match status {
        LifecycleStatus::NotInitialized { diagnostics } => {
            println!("not initialized — run: ryeos init");
            println!("detail: {}", diagnostics.message);
        }
        LifecycleStatus::Stopped { app_root } => {
            println!("initialized, stopped — run: ryeos start");
            println!("app root: {}", app_root.display());
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

fn default_app_root() -> PathBuf {
    if let Ok(p) = std::env::var("RYEOS_APP_ROOT") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .expect("could not determine XDG data directory")
}
