//! Hardcoded CLI commands that run LOCALLY without dispatching to the daemon.
//!
//! Only lifecycle commands live here — the absolute minimum needed to manage
//! the local node before the daemon exists or is reachable:
//!
//!   - `ryeos init`   — bootstrap operator keys, trust store, and bundles
//!   - `ryeos start`  — bring the local node runtime online
//!   - `ryeos stop`   — gracefully stop the local node runtime
//!   - `ryeos node status` — show local node lifecycle status
//!   - `ryeos node doctor` — offline "why won't it start" checklist
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
    match (argv[0].as_str(), argv.get(1).map(String::as_str)) {
        ("identity", _) => {
            run_identity_command(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        ("init", _) => {
            run_init_command(&argv[1..]).map_err(map_local_err)?;
            Ok(true)
        }
        ("node" | "system", Some("status")) => {
            run_status_command(&argv[2..])
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node" | "system", Some("doctor")) => {
            run_node_doctor_command(&argv[2..])
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("start", _) => {
            run_start_command(&argv[1..]).await.map_err(map_local_err)?;
            Ok(true)
        }
        ("stop", _) => {
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
    let report = ryeos_core_tools::actions::inspect::identity::run_identity(
        ryeos_core_tools::actions::inspect::identity::IdentityParams {
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

// ── ryeos node doctor ───────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node doctor",
    about = "Offline node-environment checklist: init state, lifecycle, sockets, \
             storage, installed bundles — one command answering \"why won't it start\"",
    no_binary_name = true
)]
struct NodeDoctorArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit the structured JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,

    /// Skip the per-installed-bundle doctor pass (environment checks only).
    #[arg(long)]
    no_bundles: bool,
}

/// Node-environment doctor. Deliberately hardcoded (not descriptor-driven):
/// descriptor resolution needs verified installed bundles and a reachable
/// registry — exactly the machinery this command exists to diagnose when
/// broken. Every check degrades independently; the command itself only
/// errors when it cannot even load config.
async fn run_node_doctor_command(argv: &[String]) -> Result<()> {
    use ryeos_core_tools::actions::doctor::{CheckResult, FAIL, NA, OK, WARN};

    let args = parse_or_handle_help::<NodeDoctorArgs>(argv)?;
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
    let config = controller.config().clone();
    let mut checks: Vec<CheckResult> = Vec::new();

    // 1. Init state — keys, trust store, bundles dir.
    let initialized = match controller.init_state() {
        Ok(ryeos_node::InitState::Initialized) => {
            checks.push(check("init", OK, serde_json::json!({})));
            true
        }
        Ok(ryeos_node::InitState::NotInitialized { diagnostics }) => {
            checks.push(check(
                "init",
                FAIL,
                serde_json::json!({
                    "code": format!("{:?}", diagnostics.code),
                    "message": diagnostics.message,
                    "fix": "run: ryeos init",
                }),
            ));
            false
        }
        Err(e) => {
            checks.push(check(
                "init",
                FAIL,
                serde_json::json!({ "error": format!("{e:#}") }),
            ));
            false
        }
    };

    // 2. Lifecycle status + binary/metadata skew (the stale-daemon detection
    //    `ryeos node status` warns about, as a first-class check).
    let mut daemon_running = false;
    let mut daemon_stale_pid: Option<u32> = None;
    let mut daemon_stale = false;
    match controller.status().await {
        Ok(LifecycleStatus::Running { metadata }) => {
            daemon_running = true;
            let current = ryeos_app::build_info::get();
            let skew = is_revision_skew(metadata.revision.as_deref(), current.revision)
                || ryeosd_installed_after_daemon_started(&metadata);
            if skew {
                checks.push(check(
                    "daemon",
                    WARN,
                    serde_json::json!({
                        "state": "running",
                        "running_revision": metadata.revision,
                        "installed_revision": current.revision,
                        "note": "the running daemon is an older build than the installed binary",
                        "fix": "ryeos stop && ryeos start",
                    }),
                ));
            } else {
                checks.push(check(
                    "daemon",
                    OK,
                    serde_json::json!({ "state": "running", "pid": metadata.pid }),
                ));
            }
        }
        Ok(LifecycleStatus::Stopped { .. }) => {
            checks.push(check(
                "daemon",
                OK,
                serde_json::json!({ "state": "stopped" }),
            ));
        }
        Ok(LifecycleStatus::Stale {
            metadata,
            diagnostics,
        }) => {
            daemon_stale = true;
            daemon_stale_pid = metadata.pid;
            checks.push(check(
                "daemon",
                WARN,
                serde_json::json!({
                    "state": "stale",
                    "message": diagnostics.message,
                    "note": "metadata says running but the daemon is not responding",
                }),
            ));
        }
        Ok(LifecycleStatus::Unresponsive {
            metadata,
            diagnostics,
        }) => {
            checks.push(check(
                "daemon",
                WARN,
                serde_json::json!({
                    "state": "unresponsive",
                    "pid": metadata.pid,
                    "message": diagnostics.message,
                    "note": "control probe timed out against a live socket — likely busy, not dead",
                }),
            ));
        }
        Ok(LifecycleStatus::Starting {
            pid, started_at, ..
        }) => {
            checks.push(check(
                "daemon",
                WARN,
                serde_json::json!({
                    "state": "starting",
                    "pid": pid,
                    "started_at": started_at,
                    "note": "boot in progress (e.g. projection catch-up) — control socket not up yet; wait for readiness",
                }),
            ));
        }
        Ok(LifecycleStatus::NotInitialized { .. }) => {
            // Covered by the init check; don't double-report.
            checks.push(check(
                "daemon",
                NA,
                serde_json::json!({ "state": "not initialized" }),
            ));
        }
        Err(e) => {
            checks.push(check(
                "daemon",
                FAIL,
                serde_json::json!({ "error": format!("{e:#}") }),
            ));
        }
    }

    // 3. App-root storage: a write probe covers both permissions and a full
    //    disk — the two storage reasons a start fails. On an uninitialized
    //    node the app root may not exist yet; the init check already carries
    //    the one real fix, so don't pile misdiagnoses on top of it.
    if !initialized {
        checks.push(check(
            "storage",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    } else {
        let probe = config
            .app_root
            .join(format!(".doctor-probe-{}", std::process::id()));
        match std::fs::write(&probe, b"probe").and_then(|()| std::fs::remove_file(&probe)) {
            Ok(()) => checks.push(check(
                "storage",
                OK,
                serde_json::json!({ "app_root": config.app_root, "write_probe": "ok" }),
            )),
            Err(e) => checks.push(check(
                "storage",
                FAIL,
                serde_json::json!({
                    "app_root": config.app_root,
                    "error": format!("{e}"),
                    "note": "app root is not writable (permissions or disk full)",
                }),
            )),
        }
    }

    // 4. Socket bindability — only meaningful when nothing should be holding
    //    them. A running daemon holding both is the healthy case; a STALE
    //    daemon (metadata present, not responding) may be hung-but-alive and
    //    still holding both, so a bind failure there must NOT prescribe
    //    deleting the socket file out from under it.
    if daemon_running {
        checks.push(check(
            "sockets",
            OK,
            serde_json::json!({ "note": "held by the running daemon" }),
        ));
    } else if daemon_stale {
        checks.push(check(
            "sockets",
            NA,
            serde_json::json!({
                "note": format!(
                    "daemon state is stale — a hung daemon{} may still hold the \
                     sockets; run `ryeos stop` (or kill the pid) and re-run doctor",
                    daemon_stale_pid
                        .map(|p| format!(" (recorded pid {p})"))
                        .unwrap_or_default()
                ),
            }),
        ));
    } else if !initialized {
        checks.push(check(
            "sockets",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    } else {
        let mut detail = serde_json::Map::new();
        let mut status = OK;
        match std::net::TcpListener::bind(config.bind) {
            Ok(l) => {
                drop(l);
                detail.insert(
                    "tcp".into(),
                    serde_json::json!({ "bind": config.bind.to_string(), "status": "bindable" }),
                );
            }
            Err(e) => {
                status = FAIL;
                detail.insert(
                    "tcp".into(),
                    serde_json::json!({
                        "bind": config.bind.to_string(),
                        "error": format!("{e}"),
                        "note": "another process holds the port",
                    }),
                );
            }
        }
        match std::os::unix::net::UnixListener::bind(&config.uds_path) {
            Ok(l) => {
                drop(l);
                // Binding created the socket file; remove the probe artifact.
                let _ = std::fs::remove_file(&config.uds_path);
                detail.insert(
                    "uds".into(),
                    serde_json::json!({ "path": config.uds_path, "status": "bindable" }),
                );
            }
            Err(e) => {
                status = FAIL;
                detail.insert("uds".into(), serde_json::json!({
                    "path": config.uds_path,
                    "error": format!("{e}"),
                    "note": "with no daemon running this is usually a stale socket file — remove it",
                }));
            }
        }
        checks.push(check("sockets", status, serde_json::Value::Object(detail)));
    }

    // 5. Verified node config + per-bundle doctor. Requires init; degrades to
    //    n/a rather than piling failures onto an uninitialized node.
    if initialized {
        match crate::node_descriptors::load_verified_snapshot(&config.app_root) {
            Ok(snapshot) => {
                let roots: Vec<PathBuf> = snapshot.bundles.iter().map(|b| b.path.clone()).collect();
                checks.push(check(
                    "node_config",
                    OK,
                    serde_json::json!({ "bundles": roots.len() }),
                ));
                if !args.no_bundles {
                    let operator_config_root =
                        ryeos_engine::roots::RuntimeRoot::new(config.app_root.clone()).config();
                    for record in &snapshot.bundles {
                        // Static checks only (no offline engine): the doctor
                        // must stay fast and dependency-free; import dry-runs
                        // belong to the bundle-level `ryeos doctor <source>`.
                        let report = ryeos_core_tools::actions::doctor::run_doctor(
                            Err("node doctor runs static checks only"),
                            &record.path,
                            &roots,
                            &operator_config_root,
                        );
                        checks.push(check(
                            &format!("bundle:{}", record.name),
                            if report.ok { OK } else { FAIL },
                            serde_json::json!({
                                "path": record.path,
                                "failed": report
                                    .checks
                                    .iter()
                                    .filter(|c| c.status == FAIL)
                                    .map(|c| serde_json::json!({ "check": c.name, "detail": c.detail }))
                                    .collect::<Vec<_>>(),
                            }),
                        ));
                    }
                }
            }
            Err(e) => {
                checks.push(check(
                    "node_config",
                    FAIL,
                    serde_json::json!({
                        "error": format!("{e:#}"),
                        "note": "installed bundle registrations failed verification",
                    }),
                ));
            }
        }
    } else {
        checks.push(check(
            "node_config",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    let ok = checks.iter().all(|c| c.status != FAIL);
    let report = serde_json::json!({
        "app_root": config.app_root,
        "ok": ok,
        "checks": checks
            .iter()
            .map(|c| serde_json::json!({ "name": c.name, "status": c.status, "detail": c.detail }))
            .collect::<Vec<_>>(),
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("node doctor — {}", config.app_root.display());
        for c in &checks {
            let glyph = match c.status.as_str() {
                s if s == OK => "✓",
                s if s == FAIL => "✗",
                s if s == WARN => "⚠",
                _ => "·",
            };
            println!("  {glyph} {:<24} {}", c.name, c.status);
            if c.status != OK {
                println!("      {}", c.detail);
            }
        }
    }
    if ok {
        Ok(())
    } else {
        anyhow::bail!("node doctor found failing checks (rerun with --json for detail)")
    }
}

/// Build a check row in core-tools doctor vocabulary (its constructor is
/// module-private; the fields are the contract).
fn check(
    name: &str,
    status: &str,
    detail: serde_json::Value,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    ryeos_core_tools::actions::doctor::CheckResult {
        name: name.to_string(),
        status: status.to_string(),
        detail,
    }
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
        warn_if_stale_daemon(&report.status);
    } else {
        println!("started");
    }
    print_lifecycle_status(&report.status);
    Ok(())
}

/// When `ryeos start` finds a daemon already running, warn loudly if that daemon
/// is an older build than the installed binaries — the classic footgun where an
/// install replaced `ryeosd` on disk but did not cycle the running daemon, so it
/// keeps holding the state lock and serving stale behavior. `ryeos` and `ryeosd`
/// are built and installed together, so this binary's build info stands in for
/// the on-disk `ryeosd`.
///
/// Two independent signals, either of which fires the warning:
///   1. the daemon recorded a different VCS revision (or none — a build from
///      before revisions were tracked is necessarily older);
///   2. the on-disk `ryeosd` is newer than the daemon's own metadata file, which
///      it writes once at startup — i.e. the binary was installed after the
///      daemon started. This catches a rebuild at the same commit, which the
///      revision check alone cannot see.
fn warn_if_stale_daemon(status: &LifecycleStatus) {
    let LifecycleStatus::Running { metadata } = status else {
        return;
    };
    let current = ryeos_app::build_info::get();

    let revision_skew = is_revision_skew(metadata.revision.as_deref(), current.revision);
    let binary_is_newer = ryeosd_installed_after_daemon_started(metadata);

    if !revision_skew && !binary_is_newer {
        return;
    }

    eprintln!();
    eprintln!("⚠  the running daemon is an older build than the installed binary.");
    eprintln!(
        "   running:   revision {}",
        metadata.revision.as_deref().unwrap_or("unknown")
    );
    eprintln!("   installed: revision {}", current.revision);
    eprintln!("   It holds the state lock, so newly installed changes will NOT take");
    eprintln!("   effect until it is cycled:  ryeos stop && ryeos start");
}

/// Whether the daemon's recorded revision indicates an older build than this
/// one. A missing recorded revision (a daemon built before revisions were
/// tracked) counts as skew; an "unknown" current revision (git unavailable at
/// build time) can't discriminate, so it never fires on its own.
fn is_revision_skew(recorded: Option<&str>, current: &str) -> bool {
    match recorded {
        Some(rev) => current != "unknown" && rev != current,
        None => true,
    }
}

/// True when the on-disk `ryeosd` (sibling of this `ryeos` binary) has a newer
/// mtime than the daemon's `daemon.json`, which is written once when the daemon
/// starts. Any failure to resolve either path or its mtime returns false — a
/// best-effort diagnostic must never block or mislead `start`.
fn ryeosd_installed_after_daemon_started(metadata: &ryeos_node::DaemonMetadata) -> bool {
    let Some(ryeosd) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ryeosd")))
        .filter(|p| p.exists())
    else {
        return false;
    };
    let daemon_json = ryeos_node::DaemonMetadata::path(&metadata.app_root);
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (mtime(&ryeosd), mtime(&daemon_json)) {
        (Some(binary), Some(started)) => binary > started,
        _ => false,
    }
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
        LifecycleStatus::Unresponsive {
            metadata,
            diagnostics,
        } => {
            println!("running but not answering — {}", diagnostics.message);
            if let Some(pid) = metadata.pid {
                println!("pid: {pid}");
            }
            println!("likely busy; retry shortly (do not start a second daemon)");
        }
        LifecycleStatus::Starting {
            pid, started_at, ..
        } => {
            println!("starting — daemon (pid {pid}) is booting, control socket not up yet");
            println!("since: {started_at}");
            println!(
                "boot can take minutes after a deploy (projection catch-up); wait for readiness"
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_skew_detects_mismatch_and_missing() {
        // Same revision → not skewed.
        assert!(!is_revision_skew(Some("abc123def456"), "abc123def456"));
        // Different revision → skewed.
        assert!(is_revision_skew(Some("oldsha000000"), "newsha111111"));
        // No recorded revision (older daemon predating the field) → skewed.
        assert!(is_revision_skew(None, "abc123def456"));
        // Current revision unknown (git unavailable at build): a recorded
        // revision can't be discriminated, but a missing one still skews.
        assert!(!is_revision_skew(Some("abc123def456"), "unknown"));
        assert!(is_revision_skew(None, "unknown"));
    }
}
