//! Hardcoded CLI commands that run LOCALLY without dispatching to the daemon.
//!
//! Only lifecycle commands live here — the absolute minimum needed to manage
//! the local node before the daemon exists or is reachable:
//!
//!   - `ryeos init`   — bootstrap operator keys, trust store, and bundles
//!   - `ryeos setup`  — reopen optional provider and model setup
//!   - `ryeos start`  — bring the local node runtime online
//!   - `ryeos stop`   — gracefully stop the local node runtime
//!   - `ryeos node status` — show local node lifecycle status
//!   - `ryeos node doctor` — offline "why won't it start" checklist
//!   - `ryeos node reset execution-history` — explicit offline epoch retirement
//!   - `ryeos node reset replay-indexes` — explicit clean-cut replay activation
//!   - `ryeos node reset external-content-bindings` — retire realization bindings
//!   - `ryeos node reset authorization` — retire grants and restore the operator
//!   - `ryeos node policy-apply` — replace one member of the complete signed policy generation
//!
//! `ryeos identity` is local as a bootstrap affordance: remote
//! operators need to copy their node public key before the daemon is running.
//!
//! All other commands — including `sign`, `verify`, `fetch` — are
//! descriptor-driven and dispatched through the offline/dual path
//! (see `offline_dispatch.rs`) or forwarded to the daemon.

use std::fs;
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
        summary: "Initialize RyeOS with an interactive first-contact ceremony",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["setup"],
        summary: "Configure a verified model provider and default model",
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
        tokens: &["node", "reset", "execution-history"],
        summary: "Retire the local execution-history epoch",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "reset", "authorization"],
        summary: "Discard grants and recreate the local operator authorization",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "reset", "replay-indexes"],
        summary: "Discard predecessor graph/provider replay indexes",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "reset", "external-content-bindings"],
        summary: "Discard predecessor external-content bindings",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "policy-apply"],
        summary: "Validate and install a node-owned policy",
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
            run_init_command(&argv[1..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("setup", _) => {
            run_setup_command(&argv[1..], console)
                .await
                .map_err(map_local_err)?;
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
        ("node", Some("reset")) if argv.get(2).map(String::as_str) == Some("execution-history") => {
            run_execution_history_reset_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("reset")) if argv.get(2).map(String::as_str) == Some("authorization") => {
            run_node_auth_reset_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("reset")) if argv.get(2).map(String::as_str) == Some("replay-indexes") => {
            run_node_replay_reset_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("reset"))
            if argv.get(2).map(String::as_str) == Some("external-content-bindings") =>
        {
            run_node_external_content_reset_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("policy-apply")) => {
            run_node_policy_apply_command(&argv[2..], console).map_err(map_local_err)?;
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

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node policy-apply",
    about = "Validate and atomically install an operator-authored node policy",
    long_about = "Validate a stopped-node policy through its registered node-policy compiler, replace that member in the complete generation, then node-sign and atomically publish the full generation. Bundle registrations, routes, and commands are separate node configuration and remain inaccessible.",
    no_binary_name = true
)]
struct NodePolicyApplyArgs {
    /// Registered node-policy section (for example external_content).
    section: String,

    /// Unsigned YAML policy source outside the live node-policy namespace.
    source: PathBuf,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

fn run_node_policy_apply_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodePolicyApplyArgs>(argv, console)? else {
        return Ok(());
    };
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: args.app_root,
        ..Default::default()
    })
    .context("load local node location for policy apply")?;
    let _state_lock = ryeos_app::state_lock::StateLock::acquire(
        &ryeos_app::state_lock::default_lock_path(&config.app_root),
    )
    .context("node policy apply requires the daemon to be stopped")?;

    let table = ryeos_app::node_policy::NodePolicyTable::new();
    table
        .get(&args.section)
        .with_context(|| format!("unknown node-policy section `{}`", args.section))?;

    let raw = lillux::read_regular_file_to_string_no_follow(&args.source)
        .with_context(|| format!("read policy source {}", args.source.display()))?;
    let body: serde_json::Value = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse policy source {}", args.source.display()))?;
    if !body.is_object() {
        anyhow::bail!("node policy source must contain a YAML mapping");
    }
    for forbidden in ["category", "section"] {
        if body.get(forbidden).is_some() {
            anyhow::bail!("node policy source declares path-owned field `{forbidden}`");
        }
    }

    let identity = ryeos_app::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for policy apply")?;
    let trust_store = ryeos_engine::trust::TrustStore::load(None, &config.runtime_config_dir())
        .context("load trust store for current node policies")?;
    let current = ryeos_app::node_policy::generation::load_policy_generation(
        &config.app_root,
        &trust_store,
        &table,
    )?;
    let mut policies = current.policies().clone();
    policies.insert(args.section.clone(), body);
    let update = current.prepare_replacement(&table, policies, &args.source)?;
    let policy_dir = ryeos_app::node_policy::generation::publish_policy_update(
        &config.app_root,
        &update,
        &identity,
        &trust_store,
        &_state_lock,
    )
    .context("atomically publish signed node policy generation")?;
    let installed = policy_dir.join(format!("{}.yaml", args.section));

    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": "installed",
            "section": args.section,
            "path": installed,
            "signer_fingerprint": identity.fingerprint(),
        }))?;
    } else {
        console.text(&format!(
            "Installed signed node policy: {}\n",
            installed.display()
        ))?;
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset replay-indexes",
    about = "Activate the current replay-index contract",
    long_about = "Perform the explicit clean-cut replay-index activation. The daemon must be stopped. Predecessor dispatch-effect rows are discarded; provider-call evidence, thread history, CAS content, sync state, admission attestations, and accounting state are preserved.",
    no_binary_name = true
)]
struct NodeReplayResetArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Required acknowledgement that predecessor replay indexes are discarded.
    #[arg(long)]
    confirm: bool,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

fn run_node_replay_reset_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeReplayResetArgs>(argv, console)? else {
        return Ok(());
    };
    if !args.confirm {
        anyhow::bail!("resetting predecessor replay indexes requires --confirm");
    }
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: args.app_root,
        ..Default::default()
    })
    .context("load local node configuration for replay-index reset")?;
    let _state_lock = ryeos_app::state_lock::StateLock::acquire(
        &ryeos_app::state_lock::default_lock_path(&config.app_root),
    )
    .context("replay-index reset requires the daemon to be stopped")?;
    let path = config
        .runtime_state_dir()
        .join(ryeos_state::operational::OPERATIONAL_DB_FILENAME);
    let db = ryeos_state::OperationalDb::open_for_explicit_replay_reset(&path)
        .with_context(|| format!("activate replay indexes in {}", path.display()))?;
    drop(db);
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": "activated",
            "database": path,
            "discarded": ["dispatch_effect_records"],
            "preserved": ["provider_call_records"],
        }))?;
    } else {
        console.text(&format!(
            "Replay indexes activated: {}\nPredecessor dispatch-effect records were discarded; provider-call records, thread history, and other operational state were preserved.\n",
            path.display()
        ))?;
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset external-content-bindings",
    about = "Discard predecessor external-content bindings",
    long_about = "Perform the explicit clean-cut external-content manifest activation. The daemon must be stopped. Every predecessor external-content binding head is retired; CAS objects remain reclaimable and all required content must be imported and rebound under the current schema.",
    no_binary_name = true
)]
struct NodeExternalContentResetArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Inspect the number of binding heads without retiring them.
    #[arg(long)]
    dry_run: bool,

    /// Required acknowledgement that every external-content binding is discarded.
    #[arg(long)]
    confirm: bool,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

fn run_node_external_content_reset_command(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeExternalContentResetArgs>(argv, console)? else {
        return Ok(());
    };
    if !args.dry_run && !args.confirm {
        anyhow::bail!("resetting predecessor external-content bindings requires --confirm");
    }
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: args.app_root,
        ..Default::default()
    })
    .context("load local node configuration for external-content binding reset")?;
    let bindings =
        ryeos_app::operator_external_content::discard_binding_heads_offline(&config, args.dry_run)?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": if args.dry_run { "inspected" } else { "retired" },
            "bindings": bindings,
            "requires_reimport": !args.dry_run,
        }))?;
    } else if args.dry_run {
        console.text(&format!(
            "External-content binding reset preview: {bindings} binding head(s) would be retired.\n"
        ))?;
    } else {
        console.text(&format!(
            "External-content binding cutover complete: retired {bindings} binding head(s).\nAll required realizations must be imported and rebound before launch.\n"
        ))?;
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset authorization",
    about = "Discard every authorized-key grant and recreate only the local operator grant",
    long_about = "Perform the explicit no-backcompat authorized-key cutover. The daemon must be stopped. Every local-client, remote-node, and remote-operator grant is discarded atomically; only the configured local operator key is re-authorized. Remote nodes and forwarded operators must be authorized again.",
    no_binary_name = true
)]
struct NodeAuthResetArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Required acknowledgement that every existing authorization is discarded.
    #[arg(long)]
    confirm: bool,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

fn run_node_auth_reset_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeAuthResetArgs>(argv, console)? else {
        return Ok(());
    };
    if !args.confirm {
        anyhow::bail!("resetting every authorized key requires --confirm");
    }
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: args.app_root,
        ..Default::default()
    })
    .context("load local node configuration for authorization reset")?;
    let _state_lock = ryeos_app::state_lock::StateLock::acquire(
        &ryeos_app::state_lock::default_lock_path(&config.app_root),
    )
    .context("authorization reset requires the daemon to be stopped")?;

    let node = ryeos_app::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for authorization reset")?;
    let operator = ryeos_app::identity::NodeIdentity::load(&config.operator_signing_key_path)
        .context("load operator identity for authorization reset")?;
    let parent = config
        .authorized_keys_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("authorized-key namespace has no parent directory"))?;
    fs::create_dir_all(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(&config.authorized_keys_dir)
        && (metadata.file_type().is_symlink() || !metadata.file_type().is_dir())
    {
        anyhow::bail!(
            "authorized-key namespace is not a regular directory: {}",
            config.authorized_keys_dir.display()
        );
    }
    let nonce = std::process::id();
    let staging = parent.join(format!(".authorized_keys.reset-{nonce}"));
    if staging.exists() {
        anyhow::bail!(
            "authorization reset staging paths already exist; inspect {}",
            parent.display()
        );
    }
    fs::create_dir(&staging)?;
    let operator_key = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        operator.verifying_key().as_bytes(),
    );
    ryeos_app::identity::write_authorized_key_toml(
        &staging,
        operator.fingerprint(),
        &operator_key,
        &["*".to_string()],
        "bootstrap-authorized-user",
        operator.fingerprint(),
        &lillux::time::iso8601_now(),
        node.signing_key(),
        ryeos_app::identity::WildcardPolicy::AllowBootstrap,
    )?;
    lillux::sync_tree_durable(&staging)
        .context("durably flush reset authorized-key namespace before publication")?;
    let discarded = if config.authorized_keys_dir.exists() {
        let count = fs::read_dir(&config.authorized_keys_dir)?.count();
        // Exchange keeps the live namespace present at every instant. The
        // exchanged staging path becomes the retired tree only after the new
        // namespace is durably published in the parent directory.
        lillux::atomic_exchange_paths(&config.authorized_keys_dir, &staging)
            .map_err(anyhow::Error::from)
            .context("atomically publish reset authorized-key namespace")?;
        lillux::remove_dir_all_durable(&staging)
            .context("durably remove retired authorized-key namespace")?;
        count
    } else {
        lillux::rename_path_noreplace_durable(&staging, &config.authorized_keys_dir)
            .map_err(anyhow::Error::from)
            .context("durably publish initial authorized-key namespace")?;
        0
    };
    let result = serde_json::json!({
        "discarded_grants": discarded,
        "operator_fingerprint": operator.fingerprint(),
        "authorized_keys_dir": config.authorized_keys_dir,
    });
    if args.json {
        crate::tty::write_json(&result)?;
    } else {
        let mut status = crate::tty::StatusBanner::new(
            crate::tty::Tone::Success,
            "AUTHORIZATION RESET COMPLETE",
        );
        status.rows = vec![
            crate::tty::Row::key_value("discarded grants", discarded.to_string()),
            crate::tty::Row::key_value("operator", operator.fingerprint()),
            crate::tty::Row::key_value("remote nodes", "re-admission required"),
        ];
        console.success(&status)?;
    }
    Ok(())
}

// ── ryeos node reset execution-history ───────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset execution-history",
    about = "Retire the local execution-history epoch while the daemon is stopped",
    long_about = "Retire every authoritative thread-chain head, clear execution recovery rows/files and scheduler fire history, and publish an empty current thread projection. This is an offline schema/authority reset, not storage garbage collection. Principal and deployed project HEADs are preserved unless --include-project-heads is selected. Node identity, trust, config, installed bundles, vault data, signed schedule definitions, operational sync/admission state, and independently retained logs/caches are preserved. Restart the daemon and run ordinary `ryeos maintenance gc` later to reclaim newly unreachable CAS storage.",
    no_binary_name = true
)]
struct ExecutionHistoryResetArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Also retire every principal and deployed project HEAD for an immutable object-schema cutover.
    #[arg(long = "include-project-heads")]
    include_project_heads: bool,

    /// Required acknowledgement for destructive execution-history retirement.
    #[arg(long)]
    confirm: bool,

    /// Required acknowledgement for destructive project-HEAD retirement.
    #[arg(long = "confirm-project-heads")]
    confirm_project_heads: bool,

    /// Inspect and report without mutating any store.
    #[arg(long)]
    dry_run: bool,

    /// Emit structured JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

impl ExecutionHistoryResetArgs {
    fn validate(&self) -> Result<()> {
        if !self.dry_run && !self.confirm {
            anyhow::bail!("resetting all execution history requires --confirm");
        }
        if self.include_project_heads && !self.dry_run && !self.confirm_project_heads {
            anyhow::bail!(
                "resetting principal and deployed project HEADs requires --confirm-project-heads"
            );
        }
        Ok(())
    }
}

fn run_execution_history_reset_command(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<ExecutionHistoryResetArgs>(argv, console)? else {
        return Ok(());
    };
    args.validate()?;

    let options = ryeos_app::execution_history_reset::ExecutionHistoryResetOptions {
        app_root: args.app_root,
        dry_run: args.dry_run,
        discard_project_heads: args.include_project_heads,
    };
    let mut progress =
        crate::tty::ExecutionHistoryResetProgress::new(!args.json, console.capabilities());
    let report = match progress.as_mut() {
        Some(progress) => {
            let mut observer =
                |event: &ryeos_app::execution_history_reset::ExecutionHistoryResetProgress| {
                    progress.observe(event);
                };
            ryeos_app::execution_history_reset::run_execution_history_reset_with_progress(
                &options,
                &mut observer,
            )
        }
        None => ryeos_app::execution_history_reset::run_execution_history_reset(&options),
    }
    .context("offline execution-history reset failed")?;
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
            "EXECUTION HISTORY SCAN COMPLETE"
        } else {
            "EXECUTION HISTORY RESET COMPLETE"
        },
    );
    status.detail = Some(report.app_root.display().to_string());
    status.rows = vec![
        crate::tty::Row::key_value("chain heads", report.chain_heads.to_string()),
        crate::tty::Row::key_value("project heads", report.project_heads.to_string()),
        crate::tty::Row::key_value(
            "chain/recovery artifacts",
            (report.chain_ref_artifacts + report.pending_transitions).to_string(),
        ),
        crate::tty::Row::key_value(
            "runtime rows",
            report.runtime_rows.total_rows().map_or_else(
                || "unavailable (incompatible schema)".to_string(),
                |rows| rows.to_string(),
            ),
        ),
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
    if !report.dry_run {
        status.rows.push(crate::tty::Row::key_value(
            "storage reclamation",
            "run `ryeos maintenance gc` after restart",
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

    /// Explicit publisher-signed node init profile from the source-root init
    /// namespace (for example `hosted-workflow`). Required on fresh nodes.
    #[arg(long)]
    node_profile: Option<String>,

    /// Emit the exact structured initialization report.
    #[arg(long)]
    json: bool,

    /// Run the typed initialization transaction without interactive prompts.
    /// Package installers and automation should always pass this flag or
    /// `--json` instead of relying on terminal detection.
    #[arg(long, conflicts_with = "json")]
    non_interactive: bool,
}

async fn run_init_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<InitArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = args.app_root.unwrap_or_else(default_app_root);

    let opts = ryeos_node::InitOptions {
        app_root,
        source_dir: args.source,
        trust_files: args.trust_files,
        node_profile: args.node_profile,
        skip_preflight: false,
    };

    if !args.json
        && !args.non_interactive
        && console.capabilities().interactive()
        && !crate::tty::onboarding_flow::supported_geometry()
    {
        anyhow::bail!(
            "interactive onboarding requires a terminal of at least 40x12; resize it or pass --non-interactive/--json explicitly"
        );
    }

    if !args.json
        && !args.non_interactive
        && console.capabilities().interactive()
        && crate::tty::onboarding_flow::supported_geometry()
    {
        return crate::tty::onboarding_flow::run(
            console,
            crate::tty::onboarding_flow::OnboardingOptions { init: opts },
        )
        .await;
    }

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

// ── ryeos setup ────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos setup",
    about = "Configure a verified model provider and default model",
    no_binary_name = true
)]
struct SetupArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,
}

async fn run_setup_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<SetupArgs>(argv, console)? else {
        return Ok(());
    };
    if !console.capabilities().interactive() || !crate::tty::onboarding_flow::supported_geometry() {
        anyhow::bail!(
            "ryeos setup requires an interactive terminal of at least 40x12; use verified config and vault operations for automation"
        );
    }
    crate::tty::onboarding_flow::run_setup(console, args.app_root.unwrap_or_else(default_app_root))
        .await
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
            let installed_revision = installed_ryeosd_revision();
            let skew =
                is_revision_skew(metadata.revision.as_deref(), installed_revision.as_deref())
                    || ryeosd_installed_after_daemon_started(&metadata);
            if skew {
                checks.push(check(
                    "daemon",
                    WARN,
                    serde_json::json!({
                        "state": "running",
                        "running_revision": metadata.revision,
                        "installed_revision": installed_revision,
                        "note": "the running daemon does not match the installed daemon binary",
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
            .join("node/policies/isolation.yaml");
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
                    "fix": "publish a complete valid `.ai/node/policies/` generation; then run `ryeos node doctor` again",
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
/// is an older build than the installed `ryeosd` — the classic footgun where an
/// install replaced the daemon binary on disk but did not cycle the running
/// process, so it keeps holding the state lock and serving stale behavior.
///
/// The querying `ryeos` binary is deliberately not used as a proxy for the
/// installed daemon. Incremental builds and package staging can legitimately
/// relink the CLI and daemon at different revisions even when the on-disk and
/// running daemon artifacts are identical.
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
    let installed_revision = installed_ryeosd_revision();

    let revision_skew =
        is_revision_skew(metadata.revision.as_deref(), installed_revision.as_deref());
    let binary_is_newer = ryeosd_installed_after_daemon_started(metadata);

    if !revision_skew && !binary_is_newer {
        return Ok(());
    }
    let mut diagnostic = crate::tty::Diagnostic::warning(
        "the running daemon does not match the installed daemon binary",
    );
    diagnostic.context = vec![
        format!(
            "running revision {}",
            metadata.revision.as_deref().unwrap_or("unknown")
        ),
        format!(
            "installed daemon revision {}",
            installed_revision.as_deref().unwrap_or("unknown")
        ),
        "newly installed changes do not take effect while the old daemon holds the state lock"
            .to_string(),
    ];
    diagnostic.hint = Some(crate::tty::Hint::new("run `ryeos stop && ryeos start`"));
    console.warning(&diagnostic)?;
    Ok(())
}

/// Whether the running daemon's recorded revision differs from the installed
/// daemon artifact. If the installed artifact cannot report a revision, the
/// revision signal is unavailable and the independent mtime signal remains.
fn is_revision_skew(recorded: Option<&str>, installed: Option<&str>) -> bool {
    match installed.filter(|revision| *revision != "unknown") {
        Some(installed) => recorded != Some(installed),
        None => false,
    }
}

/// Read build provenance from the installed `ryeosd` sibling without opening
/// node state. `ryeosd build-info` exits before configuration or state loading.
/// Any failure leaves revision comparison unavailable; lifecycle diagnostics
/// remain best-effort and still retain the independent binary-mtime signal.
fn installed_ryeosd_revision() -> Option<String> {
    let ryeosd = sibling_ryeosd_path()?;
    let output = std::process::Command::new(ryeosd)
        .args(["build-info", "--revision"])
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_installed_revision(&output.stdout)
}

fn parse_installed_revision(stdout: &[u8]) -> Option<String> {
    const MAX_REVISION_BYTES: usize = 128;
    if stdout.len() > MAX_REVISION_BYTES {
        return None;
    }
    let revision = std::str::from_utf8(stdout).ok()?.trim();
    if revision.is_empty()
        || revision == "unknown"
        || !revision.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return None;
    }
    Some(revision.to_string())
}

fn sibling_ryeosd_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|directory| directory.join("ryeosd"))
        .filter(|path| path.is_file())
}

/// True when the on-disk `ryeosd` (sibling of this `ryeos` binary) has a newer
/// mtime than the daemon's `daemon.json`, which is written once when the daemon
/// starts. Any failure to resolve either path or its mtime returns false — a
/// best-effort diagnostic must never block or mislead `start`.
fn ryeosd_installed_after_daemon_started(metadata: &ryeos_node::DaemonMetadata) -> bool {
    let Some(ryeosd) = sibling_ryeosd_path() else {
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

    fn execution_history_reset_args(dry_run: bool, confirm: bool) -> ExecutionHistoryResetArgs {
        ExecutionHistoryResetArgs {
            app_root: None,
            include_project_heads: false,
            confirm,
            confirm_project_heads: false,
            dry_run,
            json: false,
        }
    }

    #[test]
    fn execution_history_reset_requires_destructive_confirmation() {
        assert!(execution_history_reset_args(true, false).validate().is_ok());
        assert!(
            execution_history_reset_args(false, false)
                .validate()
                .is_err()
        );
        assert!(execution_history_reset_args(false, true).validate().is_ok());
    }

    #[test]
    fn project_head_cutover_requires_its_own_explicit_confirmation() {
        let mut args = execution_history_reset_args(false, true);
        args.include_project_heads = true;
        assert!(args.validate().is_err());

        args.confirm_project_heads = true;
        assert!(args.validate().is_ok());

        let mut preview = execution_history_reset_args(true, false);
        preview.include_project_heads = true;
        assert!(preview.validate().is_ok());
    }

    #[test]
    fn revision_skew_compares_running_and_installed_daemon() {
        assert!(!is_revision_skew(
            Some("abc123def456"),
            Some("abc123def456")
        ));
        assert!(is_revision_skew(Some("oldsha000000"), Some("newsha111111")));
        assert!(is_revision_skew(None, Some("abc123def456")));

        // A CLI built at another revision is irrelevant. If the installed
        // daemon revision cannot be read, this signal cannot claim skew.
        assert!(!is_revision_skew(Some("abc123def456"), None));
        assert!(!is_revision_skew(None, None));
        assert!(!is_revision_skew(Some("abc123def456"), Some("unknown")));
    }

    #[test]
    fn installed_revision_output_is_bounded_and_single_token() {
        assert_eq!(
            parse_installed_revision(b"331b98afb3b5\n").as_deref(),
            Some("331b98afb3b5")
        );
        assert_eq!(parse_installed_revision(b"unknown\n"), None);
        assert_eq!(parse_installed_revision(b"two revisions\n"), None);
        assert_eq!(parse_installed_revision(&[b'a'; 129]), None);
    }
}
