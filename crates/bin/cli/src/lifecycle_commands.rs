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
//!   - `ryeos node gc` — explicit offline recovery/GC that must work when boot fails
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

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ReportedLocalFailure(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCommandDescriptor {
    pub tokens: &'static [&'static str],
    pub summary: &'static str,
    pub category: &'static str,
}

const LOCAL_COMMANDS: &[LocalCommandDescriptor] = &[
    LocalCommandDescriptor {
        tokens: &["identity"],
        summary: "Print the local node public identity",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["init"],
        summary: "Bootstrap local node state and packaged bundles",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["start"],
        summary: "Bring the local node runtime online",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["stop"],
        summary: "Gracefully stop the local node runtime",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "status"],
        summary: "Show local node lifecycle status",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "doctor"],
        summary: "Diagnose local node startup and config",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "gc"],
        summary: "Run explicit offline node garbage collection",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["help"],
        summary: "Open the compact TTY help screen",
        category: "meta",
    },
    LocalCommandDescriptor {
        tokens: &["help", "--all"],
        summary: "Print the exhaustive CLI reference",
        category: "meta",
    },
    LocalCommandDescriptor {
        tokens: &["commands"],
        summary: "Print the full verified command list",
        category: "meta",
    },
];

pub fn local_command_descriptors() -> &'static [LocalCommandDescriptor] {
    LOCAL_COMMANDS
}

/// Returns `Ok(true)` if the argv was handled by a lifecycle command, `Ok(false)`
/// if no lifecycle command matched.
///
/// Errors from a matched lifecycle command propagate as `CliError::Local`.
pub async fn try_dispatch(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<bool, CliError> {
    if argv.is_empty() {
        return Ok(false);
    }
    match (argv[0].as_str(), argv.get(1).map(String::as_str)) {
        ("identity", _) => {
            run_identity_command(&argv[1..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("init", _) => {
            run_init_command(&argv[1..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("status")) => {
            run_status_command(&argv[2..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("doctor")) => {
            run_node_doctor_command(&argv[2..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("gc")) => {
            run_node_gc_command(&argv[2..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("start", _) => {
            run_start_command(&argv[1..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("stop", _) => {
            run_stop_command(&argv[1..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

// ── ryeos node gc ──────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node gc",
    about = "Run bootstrap-safe offline node garbage collection",
    long_about = "Run bootstrap-safe offline node garbage collection. The thread-history mode retires every authoritative thread-chain head, clears execution recovery rows/files and scheduler fire history, and publishes an empty current thread projection. Node identity, trust, config, installed bundles, vault data, signed schedule definitions, project heads, operational sync/admission state, and independently retained logs/caches are preserved.",
    no_binary_name = true
)]
struct NodeGcArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Retire every local thread chain and its execution recovery history.
    #[arg(long)]
    discard_thread_history: bool,

    /// Required acknowledgement for destructive thread-history retirement.
    #[arg(long)]
    confirm_discard_thread_history: bool,

    /// Inspect and report without mutating any store.
    #[arg(long)]
    dry_run: bool,

    /// Physically sweep newly unreachable CAS objects after retiring roots.
    /// Omit for the fast startup-recovery path; normal maintenance can sweep later.
    #[arg(long)]
    sweep_cas: bool,

    /// Emit structured JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

impl NodeGcArgs {
    fn validate(&self) -> Result<()> {
        if !self.discard_thread_history {
            anyhow::bail!(
                "no offline GC operation selected; pass --discard-thread-history (use --dry-run to inspect first)"
            );
        }
        if !self.dry_run && !self.confirm_discard_thread_history {
            anyhow::bail!(
                "discarding all thread history requires --confirm-discard-thread-history"
            );
        }
        if self.dry_run && self.sweep_cas {
            anyhow::bail!(
                "--sweep-cas cannot be combined with --dry-run; inspect history first, then sweep only with the confirmed discard"
            );
        }
        Ok(())
    }
}

fn run_node_gc_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeGcArgs>(argv, console)? else {
        return Ok(());
    };
    args.validate()?;

    let options = ryeos_app::offline_gc::OfflineThreadHistoryGcOptions {
        app_root: args.app_root,
        dry_run: args.dry_run,
        sweep_cas: args.sweep_cas,
    };
    let mut progress = crate::tty::OfflineGcProgress::new(!args.json, console.capabilities());
    let report = match progress.as_mut() {
        Some(progress) => {
            let mut observer = |event: &ryeos_app::offline_gc::OfflineThreadHistoryGcProgress| {
                progress.observe(event);
            };
            ryeos_app::offline_gc::run_offline_thread_history_gc_with_progress(
                &options,
                &mut observer,
            )
        }
        None => ryeos_app::offline_gc::run_offline_thread_history_gc(&options),
    }
    .context("offline node GC failed")?;
    if args.json {
        crate::tty::write_json(&report)?;
        return Ok(());
    }

    if let Some(progress) = progress {
        progress.finish()?;
    }
    let mut status = crate::tty::StatusBanner::new(
        crate::tty::Tone::Success,
        if report.dry_run {
            "HISTORY SCAN COMPLETE"
        } else {
            "HISTORY CLEAR COMPLETE"
        },
    );
    status.detail = Some(report.app_root.display().to_string());
    status.rows = vec![
        crate::tty::Row::key_value("chain heads", report.chain_heads.to_string()),
        crate::tty::Row::key_value(
            "chain/recovery artifacts",
            (report.chain_ref_artifacts + report.pending_transitions).to_string(),
        ),
        crate::tty::Row::key_value("runtime rows", report.runtime_rows.total_rows().to_string()),
        crate::tty::Row::key_value(
            "thread runtime artifacts",
            report.thread_runtime_artifacts.to_string(),
        ),
        crate::tty::Row::key_value(
            "scheduler rows",
            report.scheduler_rows.total_rows().to_string(),
        ),
        crate::tty::Row::key_value(
            "scheduler journal artifacts",
            report.scheduler_journal_artifacts.to_string(),
        ),
        crate::tty::Row::key_value(
            "old projection stores",
            report.projection.superseded_instances_deleted.to_string(),
        ),
    ];
    if let Some(sweep) = report.cas_sweep.as_ref() {
        status.rows.push(crate::tty::Row::key_value(
            "CAS swept",
            format!(
                "{} objects, {} blobs ({} bytes)",
                sweep.deleted_objects, sweep.deleted_blobs, sweep.freed_bytes
            ),
        ));
    } else if !report.dry_run {
        status.rows.push(crate::tty::Row::key_value(
            "CAS sweep",
            "deferred (run normal maintenance GC later)",
        ));
    }
    console.success(&status)?;
    Ok(())
}

fn map_local_err(e: anyhow::Error) -> CliError {
    if let Some(error) = e.downcast_ref::<ReportedLocalFailure>() {
        return CliError::Reported {
            detail: error.to_string(),
        };
    }
    if let Some(error) = e.downcast_ref::<std::io::Error>() {
        return CliError::Io(std::io::Error::new(error.kind(), error.to_string()));
    }
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

    /// Emit the exact structured identity document.
    #[arg(long)]
    json: bool,
}

fn run_identity_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<IdentityArgs>(argv, console)? else {
        return Ok(());
    };
    let report = ryeos_core_tools::actions::inspect::identity::run_identity(
        ryeos_core_tools::actions::inspect::identity::IdentityParams {
            app_root: args.app_root.map(|p| p.to_string_lossy().into_owned()),
            project_path: None,
        },
    )
    .context("ryeos identity failed")?;
    if args.json {
        crate::tty::write_json(&report)?;
    } else {
        let mut section = crate::tty::Section::named("node");
        if let Some(values) = report.as_object() {
            for (key, value) in values {
                let rendered = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                section.rows.push(crate::tty::Row::key_value(key, rendered));
            }
        }
        let mut document = crate::tty::Document::titled("NODE IDENTITY");
        document.sections.push(section);
        console.document(&document)?;
    }
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

    /// Emit the exact structured initialization report.
    #[arg(long)]
    json: bool,
}

fn run_init_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<InitArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = args.app_root.unwrap_or_else(default_app_root);

    let opts = ryeos_node::InitOptions {
        app_root,
        source_dir: args.source,
        trust_files: args.trust_files,
        skip_preflight: false,
    };
    let mut progress = if args.json {
        None
    } else {
        console.progress(
            crate::tty::OperationKind::Install,
            "initializing node state",
        )?
    };
    let report = if let Some(progress) = progress.as_mut() {
        ryeos_node::run_init_with_progress(&opts, |event| {
            let label = match event.phase {
                ryeos_node::InitPhase::PreparingLayout => "preparing node layout",
                ryeos_node::InitPhase::InitializingIdentity => "initializing operator identity",
                ryeos_node::InitPhase::PinningTrust => "pinning publisher trust",
                ryeos_node::InitPhase::DiscoveringBundles => "discovering bundle sources",
                ryeos_node::InitPhase::VerifyingBundles => "verifying bundle signatures",
                ryeos_node::InitPhase::InstallingBundles => "installing bundles",
                ryeos_node::InitPhase::InitializingVault => "initializing vault identity",
                ryeos_node::InitPhase::Finalizing => "verifying initialized state",
            };
            match (event.completed, event.total) {
                (Some(completed), Some(total)) => {
                    progress.update_determinate(label, completed, total, event.detail.as_deref())?
                }
                _ => progress.update(label, event.detail.as_deref())?,
            }
            Ok(())
        })
    } else {
        ryeos_node::run_init(&opts)
    }
    .context("ryeos init failed")?;
    if let Some(progress) = progress {
        progress.finish()?;
    }
    if args.json {
        crate::tty::write_json(&report)?;
    } else {
        let mut status =
            crate::tty::StatusBanner::new(crate::tty::Tone::Success, "INITIALIZATION COMPLETE");
        status.detail = Some(format!(
            "{} bundles installed",
            report.bundles_installed.len()
        ));
        status.rows = vec![
            crate::tty::Row::key_value("app root", report.app_root.display().to_string()),
            crate::tty::Row::key_value("operator", report.user_key_fingerprint),
            crate::tty::Row::key_value("node", report.node_key_fingerprint),
            crate::tty::Row::key_value("vault", report.vault_pubkey_fingerprint),
            crate::tty::Row::key_value("bundles", report.bundles_installed.join(", ")),
        ];
        console.success(&status)?;
    }
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

async fn run_status_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<StatusArgs>(argv, console)? else {
        return Ok(());
    };
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
    let status = controller
        .status()
        .await
        .context("ryeos node status failed")?;
    if args.json {
        crate::tty::write_json(&status)?;
    } else {
        render_lifecycle_status(console, &status)?;
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
async fn run_node_doctor_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    use ryeos_core_tools::actions::doctor::{CheckResult, FAIL, NA, OK, WARN};

    let Some(args) = parse_or_render_help::<NodeDoctorArgs>(argv, console)? else {
        return Ok(());
    };
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
        Ok(LifecycleStatus::Running { metadata, .. }) => {
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
                    "note": "a live socket did not provide a usable current lifecycle response; do not start a replacement",
                }),
            ));
        }
        Ok(LifecycleStatus::Starting {
            metadata, startup, ..
        }) => {
            checks.push(check(
                "daemon",
                WARN,
                serde_json::json!({
                    "state": "starting",
                    "pid": metadata.pid,
                    "started_at": metadata.started_at,
                    "phase": startup.phase,
                    "elapsed_ms": startup.elapsed_ms,
                    "progress": startup,
                    "note": "boot in progress; wait for readiness",
                }),
            ));
        }
        Ok(LifecycleStatus::Failed { metadata, startup }) => {
            checks.push(check(
                "daemon",
                FAIL,
                serde_json::json!({
                    "state": "failed",
                    "pid": metadata.pid,
                    "started_at": metadata.started_at,
                    "phase": startup.phase,
                    "elapsed_ms": startup.elapsed_ms,
                    "error": startup.error,
                    "fix": "inspect the startup error, then run: ryeos stop",
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

    // Use the same policy loader as daemon and offline execution. Disabled is
    // an explicit, healthy opt-out; enforced policy failures remain fail-closed.
    if initialized {
        let policy_path = config
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/isolation.yaml");
        match inspect_isolation_policy(&config.app_root) {
            Ok(inspection) => checks.push(check(
                "isolation",
                inspection.status,
                inspection.detail,
            )),
            Err(error) => checks.push(check(
                "isolation",
                FAIL,
                serde_json::json!({
                    "policy": policy_path,
                    "error": format!("{error:#}"),
                    "fix": "repair `.ai/node/isolation.yaml` (or set its mode to `disabled`); then run `ryeos node doctor` again",
                }),
            )),
        }
    } else {
        checks.push(check(
            "isolation",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    if initialized {
        match ryeos_app::bundle_transaction::inspect_bundle_transactions(&config.app_root) {
            Ok(diagnostics) if !diagnostics.invalid.is_empty() => checks.push(check(
                "bundle_transactions",
                FAIL,
                serde_json::json!({
                    "pending": diagnostics.pending,
                    "invalid": diagnostics.invalid,
                    "note": "invalid transaction journals block fail-closed startup; inspect or remove them only after verifying bundle tree and registration state",
                }),
            )),
            Ok(diagnostics) if !diagnostics.pending.is_empty() => checks.push(check(
                "bundle_transactions",
                WARN,
                serde_json::json!({
                    "pending": diagnostics.pending,
                    "invalid": [],
                    "fix": "start the node to reconcile interrupted bundle transactions before registry loading",
                }),
            )),
            Ok(diagnostics) => checks.push(check(
                "bundle_transactions",
                OK,
                serde_json::json!({
                    "pending": diagnostics.pending,
                    "invalid": diagnostics.invalid,
                }),
            )),
            Err(error) => checks.push(check(
                "bundle_transactions",
                FAIL,
                serde_json::json!({ "error": format!("{error:#}") }),
            )),
        }
    } else {
        checks.push(check(
            "bundle_transactions",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
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
        let isolation = ryeos_app::engine_init::load_locked_registered_isolation(&config.app_root)
            .map_err(|error| error.to_string());
        let snapshot = match &isolation {
            Ok(runtime) => crate::node_descriptors::load_verified_snapshot_with_trust(
                &config.app_root,
                runtime
                    .registered_generation_node_trust()
                    .expect("locked isolation runtime retains node trust"),
            ),
            Err(error) => Err(anyhow::anyhow!(error.clone())),
        };
        match snapshot {
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
                        // Skip import dry-runs, but parser-backed verification
                        // still uses the node's immutable isolation snapshot.
                        let report = ryeos_core_tools::actions::doctor::run_doctor(
                            Err("node doctor runs static checks only"),
                            isolation
                                .as_ref()
                                .map(std::sync::Arc::clone)
                                .map_err(String::as_str),
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
        crate::tty::write_json(&report)?;
    } else {
        let mut section = crate::tty::Section::named("checks");
        for c in &checks {
            let tone = match c.status.as_str() {
                s if s == OK => crate::tty::Tone::Success,
                s if s == FAIL => crate::tty::Tone::Failure,
                s if s == WARN => crate::tty::Tone::Warning,
                _ => crate::tty::Tone::Neutral,
            };
            section
                .rows
                .push(crate::tty::Row::key_value(&c.name, &c.status).with_tone(tone));
            if c.status != OK || c.name == "isolation" {
                section.rows.push(
                    crate::tty::Row::text(format!("{}: {}", c.name, c.detail))
                        .with_tone(crate::tty::Tone::Secondary),
                );
            }
        }
        let mut document =
            crate::tty::Document::titled(format!("NODE DOCTOR — {}", config.app_root.display()));
        document.sections.push(section);
        console.document(&document)?;
        if ok {
            let mut summary =
                crate::tty::StatusBanner::new(crate::tty::Tone::Success, "DOCTOR PASSED");
            summary.detail = Some(format!("{} checks", checks.len()));
            console.status(&summary)?;
        } else {
            let mut summary =
                crate::tty::StatusBanner::new(crate::tty::Tone::Failure, "DOCTOR FAILED");
            summary.detail = Some(format!("{} checks", checks.len()));
            summary.rows.push(crate::tty::Row::text(
                "rerun with --json for the complete structured report",
            ));
            console.status(&summary)?;
        }
    }
    if ok {
        Ok(())
    } else {
        Err(ReportedLocalFailure("node doctor found failing checks".to_string()).into())
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

#[derive(Debug)]
struct IsolationPolicyInspection {
    detail: serde_json::Value,
    status: &'static str,
}

fn inspect_isolation_policy(app_root: &std::path::Path) -> Result<IsolationPolicyInspection> {
    use ryeos_core_tools::actions::doctor::{NA, OK};
    use ryeos_engine::isolation::IsolationMode;

    let runtime = ryeos_app::engine_init::load_locked_registered_isolation(app_root)?;
    let inspection = runtime.inspection();
    let enforced = runtime.mode() == IsolationMode::Enforce;
    let open_files_status = match (enforced, inspection.limits.open_files) {
        (false, _) => "inactive",
        (true, None) => "not_configured",
        (true, Some(_)) => "enforced_on_spawn",
    };
    Ok(IsolationPolicyInspection {
        detail: serde_json::json!({
            "policy": runtime.source(),
            "version": runtime.version(),
            "mode": runtime.mode(),
            "policy_digest": runtime.digest(),
            "backend": inspection.backend,
            "backend_status": inspection.backend.status,
            "filesystem": inspection.filesystem,
            "network": inspection.network,
            "environment": inspection.environment,
            "limits": inspection.limits,
            "limit_enforcement": {
                "open_files": {
                    "configured": inspection.limits.open_files,
                    "status": open_files_status,
                    "runtime_mechanism": if enforced && inspection.limits.open_files.is_some() {
                        Some("RLIMIT_NOFILE (installed before exec; spawn fails closed on error)")
                    } else {
                        None
                    }
                },
                "captured_output": {
                    "stdout_bytes": inspection.limits.stdout_bytes,
                    "stderr_bytes": inspection.limits.stderr_bytes,
                    "status": "enforced_while_draining",
                    "runtime_mechanism": "bounded stdout/stderr retention with continued draining and workload termination on overflow",
                },
                "verified_artifacts": {
                    "file_bytes": inspection.limits.verified_artifact_file_bytes,
                    "total_bytes": inspection.limits.verified_artifact_total_bytes,
                    "files": inspection.limits.verified_artifact_files,
                    "status": if enforced { "enforced_on_materialization" } else { "inactive" },
                    "runtime_mechanism": if enforced {
                        Some("metadata/read caps plus synchronized per-runtime file and byte accounting")
                    } else {
                        None
                    }
                }
            },
        }),
        status: if enforced { OK } else { NA },
    })
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

async fn run_start_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<StartArgs>(argv, console)? else {
        return Ok(());
    };
    let env =
        LocalLifecycleEnv::load_with_overrides(args.app_root, args.bind, args.uds_path, true)?;
    let controller = LifecycleController::from_env(env);
    let mut progress = crate::tty::LifecycleProgress::new(
        crate::tty::LifecycleProgressAction::Boot,
        console.capabilities(),
    );
    let report = match progress.as_mut() {
        Some(progress) => controller.start_with_progress(progress).await,
        None => controller.start().await,
    }
    .context("ryeos start failed")?;
    if let Some(progress) = progress {
        progress.finish_start(&report)?;
        warn_if_stale_daemon(console, &report.status)?;
        return Ok(());
    }
    if report.already_running {
        let status = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "RUNNING");
        console.status(&status)?;
        warn_if_stale_daemon(console, &report.status)?;
    } else {
        let status = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "STARTED");
        console.status(&status)?;
    }
    render_lifecycle_status(console, &report.status)?;
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
fn warn_if_stale_daemon(console: &crate::tty::Console, status: &LifecycleStatus) -> Result<()> {
    let LifecycleStatus::Running { metadata, .. } = status else {
        return Ok(());
    };
    let current = ryeos_app::build_info::get();

    let revision_skew = is_revision_skew(metadata.revision.as_deref(), current.revision);
    let binary_is_newer = ryeosd_installed_after_daemon_started(metadata);

    if !revision_skew && !binary_is_newer {
        return Ok(());
    }
    let mut diagnostic = crate::tty::Diagnostic::warning(
        "the running daemon is an older build than the installed binary",
    );
    diagnostic.context = vec![
        format!(
            "running revision {}",
            metadata.revision.as_deref().unwrap_or("unknown")
        ),
        format!("installed revision {}", current.revision),
        "newly installed changes do not take effect while the old daemon holds the state lock"
            .to_string(),
    ];
    diagnostic.hint = Some(crate::tty::Hint::new("run `ryeos stop && ryeos start`"));
    console.warning(&diagnostic)?;
    Ok(())
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

async fn run_stop_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<StopArgs>(argv, console)? else {
        return Ok(());
    };
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
    let options = StopOptions {
        force: args.force,
        ..StopOptions::default()
    };
    let mut progress = crate::tty::LifecycleProgress::new(
        crate::tty::LifecycleProgressAction::Shutdown,
        console.capabilities(),
    );
    let report = match progress.as_mut() {
        Some(progress) => controller.stop_with_progress(options, progress).await,
        None => controller.stop(options).await,
    }
    .context("ryeos stop failed")?;
    if let Some(progress) = progress {
        progress.finish_stop(&report)?;
        return Ok(());
    }
    if report.already_stopped {
        let status = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "ALREADY STOPPED");
        console.status(&status)?;
    } else {
        let status = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "STOPPED");
        console.status(&status)?;
    }
    render_lifecycle_status(console, &report.status)?;
    Ok(())
}

fn local_env(app_root: Option<PathBuf>) -> Result<LocalLifecycleEnv> {
    LocalLifecycleEnv::load(app_root)
}

fn render_lifecycle_status(console: &crate::tty::Console, status: &LifecycleStatus) -> Result<()> {
    let mut banner = match status {
        LifecycleStatus::NotInitialized { diagnostics } => {
            let mut banner = crate::tty::StatusBanner::new(
                crate::tty::Tone::Warning,
                "NOT INITIALIZED — RUN: RYEOS INIT",
            );
            banner
                .rows
                .push(crate::tty::Row::key_value("detail", &diagnostics.message));
            banner
        }
        LifecycleStatus::Stopped { app_root } => {
            let mut banner = crate::tty::StatusBanner::new(
                crate::tty::Tone::Neutral,
                "INITIALIZED, STOPPED — RUN: RYEOS START",
            );
            banner.rows.push(crate::tty::Row::key_value(
                "app root",
                app_root.display().to_string(),
            ));
            banner
        }
        LifecycleStatus::Running {
            metadata, ready_at, ..
        } => {
            let mut banner = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "RUNNING");
            if let Some(pid) = metadata.pid {
                banner
                    .rows
                    .push(crate::tty::Row::key_value("pid", pid.to_string()));
            }
            if let Some(bind) = &metadata.bind {
                banner
                    .rows
                    .push(crate::tty::Row::key_value("url", format!("http://{bind}")));
            }
            if let Some(socket) = &metadata.uds_path {
                banner.rows.push(crate::tty::Row::key_value(
                    "socket",
                    socket.display().to_string(),
                ));
            }
            banner
                .rows
                .push(crate::tty::Row::key_value("ready since", ready_at));
            banner
        }
        LifecycleStatus::Stale { diagnostics, .. } => {
            let mut banner =
                crate::tty::StatusBanner::new(crate::tty::Tone::Warning, "STALE DAEMON METADATA");
            banner.detail = Some(diagnostics.message.clone());
            banner
        }
        LifecycleStatus::Unresponsive {
            metadata,
            diagnostics,
        } => {
            let mut banner = crate::tty::StatusBanner::new(
                crate::tty::Tone::Failure,
                "LIVE DAEMON CONTROL IS UNUSABLE",
            );
            banner.detail = Some(diagnostics.message.clone());
            if let Some(pid) = metadata.pid {
                banner
                    .rows
                    .push(crate::tty::Row::key_value("pid", pid.to_string()));
            }
            banner.rows.push(crate::tty::Row::text(
                "retry if busy, otherwise inspect or stop it (do not start a second daemon)",
            ));
            banner
        }
        LifecycleStatus::Starting {
            metadata, startup, ..
        } => {
            let pid = metadata.pid.unwrap_or_default();
            let mut banner = crate::tty::StatusBanner::new(
                crate::tty::Tone::Active,
                format!(
                    "STARTING — DAEMON (PID {pid}) IS IN {}",
                    startup.phase.as_str()
                ),
            );
            if let Some(started_at) = &metadata.started_at {
                banner
                    .rows
                    .push(crate::tty::Row::key_value("since", started_at));
            }
            banner.rows.push(crate::tty::Row::key_value(
                "elapsed",
                format!("{}ms", startup.elapsed_ms),
            ));
            if let (Some(done), Some(total)) = (startup.chains_done, startup.chains_total) {
                banner.rows.push(crate::tty::Row::key_value(
                    "chains",
                    format!("{done}/{total}"),
                ));
            }
            if let Some(message) = &startup.message {
                banner
                    .rows
                    .push(crate::tty::Row::key_value("detail", message));
            }
            banner
                .rows
                .push(crate::tty::Row::text("wait for readiness"));
            banner
        }
        LifecycleStatus::Failed { metadata, startup } => {
            let pid = metadata.pid.unwrap_or_default();
            let mut banner = crate::tty::StatusBanner::new(
                crate::tty::Tone::Failure,
                format!("FAILED — DAEMON (PID {pid}) COULD NOT START"),
            );
            banner
                .rows
                .push(crate::tty::Row::key_value("phase", startup.phase.as_str()));
            banner.rows.push(crate::tty::Row::key_value(
                "error",
                startup
                    .error
                    .as_deref()
                    .unwrap_or("unknown startup failure"),
            ));
            banner.rows.push(crate::tty::Row::text(
                "run `ryeos stop` after inspecting the error",
            ));
            banner
        }
    };
    if matches!(status, LifecycleStatus::Running { .. }) {
        banner.tone = crate::tty::Tone::Success;
    }
    console.status(&banner)?;
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse argv with clap, but treat `--help` / `--version` as a successful
/// exit (print to stdout, exit 0) rather than an error. Other parse
/// failures are mapped to anyhow errors that propagate as `CliError::Local`.
///
/// This direct process exit is acceptable for one-shot CLI dispatch. It must be
/// converted to a returned outcome before extracting an in-process command core.
fn parse_or_render_help<P: Parser>(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<Option<P>> {
    use clap::error::ErrorKind;
    match P::try_parse_from(argv) {
        Ok(p) => Ok(Some(p)),
        Err(e) => match e.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                let s = e.render().to_string();
                console.text(&s)?;
                Ok(None)
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
    use ryeos_core_tools::actions::doctor::NA;

    fn node_gc_args(dry_run: bool, confirm: bool, sweep_cas: bool) -> NodeGcArgs {
        NodeGcArgs {
            app_root: None,
            discard_thread_history: true,
            confirm_discard_thread_history: confirm,
            dry_run,
            sweep_cas,
            json: false,
        }
    }

    #[test]
    fn node_gc_requires_an_explicit_operation_and_destructive_confirmation() {
        let mut args = node_gc_args(true, false, false);
        args.discard_thread_history = false;
        assert!(args.validate().is_err());

        assert!(node_gc_args(true, false, false).validate().is_ok());
        assert!(node_gc_args(false, false, false).validate().is_err());
        assert!(node_gc_args(false, true, false).validate().is_ok());
        assert!(node_gc_args(true, false, true).validate().is_err());
    }

    fn isolation_policy(mode: &str, open_files: Option<u64>) -> String {
        let open_files = open_files
            .map(|limit| format!("  open_files: {limit}\n"))
            .unwrap_or_else(|| "  open_files: null\n".to_string());
        let backend = if mode == "enforce" {
            "backend:\n  bundle: example-isolation-backend\n  implementation: example"
        } else {
            "backend: null"
        };
        format!(
            "version: 1\nmode: {mode}\n{backend}\nfilesystem:\n  writable:\n    - \"{{project}}\"\n  readable:\n    - \"{{node_public_identity}}\"\nnetwork:\n  mode: isolated\nenvironment:\n  allow:\n    - PATH\nlimits:\n{open_files}  stdout_bytes: 8388608\n  stderr_bytes: 8388608\n  verified_artifact_file_bytes: 67108864\n  verified_artifact_total_bytes: 268435456\n  verified_artifact_files: 4096\n",
        )
    }

    fn supported_open_file_limit() -> u64 {
        [128, 64, 32, 16, 8, 4, 2, 1, 0]
            .into_iter()
            .find(|max_open_files| {
                lillux::validate_subprocess_limits(Some(&lillux::SubprocessLimits {
                    max_open_files: Some(*max_open_files),
                    ..lillux::SubprocessLimits::default()
                }))
                .is_ok()
            })
            .expect("the current process should accept at least RLIMIT_NOFILE=0")
    }

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

    #[test]
    fn isolation_doctor_requires_a_registered_signed_backend() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".ai/node/identity")).unwrap();
        std::fs::write(
            temp.path().join(".ai/node/identity/public-identity.json"),
            "{}",
        )
        .unwrap();
        let policy = temp.path().join(".ai/node/isolation.yaml");
        let max_open_files = supported_open_file_limit();
        std::fs::write(&policy, isolation_policy("enforce", Some(max_open_files))).unwrap();

        let error = inspect_isolation_policy(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("isolation bundle"));
    }

    #[test]
    fn isolation_doctor_reports_disabled_without_inspecting_backend() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".ai/node")).unwrap();
        let policy = temp.path().join(".ai/node/isolation.yaml");
        std::fs::write(&policy, isolation_policy("disabled", Some(1024))).unwrap();

        let inspection = inspect_isolation_policy(temp.path()).unwrap();
        assert_eq!(inspection.status, NA);
        assert_eq!(inspection.detail["mode"], "disabled");
        assert_eq!(inspection.detail["backend_status"], "disabled");
        assert_eq!(
            inspection.detail["limit_enforcement"]["open_files"]["status"],
            "inactive"
        );
    }

    #[test]
    fn isolation_doctor_rejects_unknown_fields_and_missing_selected_bundle() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".ai/node")).unwrap();
        let policy = temp.path().join(".ai/node/isolation.yaml");
        std::fs::write(
            &policy,
            format!("{}unexpected: true\n", isolation_policy("disabled", None)),
        )
        .unwrap();

        let error = format!("{:#}", inspect_isolation_policy(temp.path()).unwrap_err());
        assert!(error.contains("unknown field"), "{error}");

        std::fs::write(&policy, isolation_policy("enforce", None)).unwrap();
        assert!(inspect_isolation_policy(temp.path())
            .unwrap_err()
            .to_string()
            .contains("isolation bundle"));
    }
}
