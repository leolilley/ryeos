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
//!   - `ryeos node offline-operation facts` — print the facts a signed offline-operation request is signed against
//!   - `ryeos node offline-operation sign` — sign a request against a node's facts, away from the node
//!   - `ryeos node offline-operation submit` — send a signed request to a running node's route
//!   - `ryeos node offline-operation list` — show what a node has queued, applied and refused
//!   - `ryeos node capacity reissue` — open the next native-capacity epoch on a stopped node
//!   - `ryeos node capacity reanchor` — sign a stopped node's native-capacity account onto the filesystem its data moved to
//!   - `ryeos node reset execution-history` — explicit offline epoch retirement
//!   - `ryeos node reset execution-history-scratch` — clear stranded reset scratch
//!   - `ryeos node reset replay-indexes` — explicit clean-cut replay activation
//!   - `ryeos node reset external-content-bindings` — retire realization bindings
//!   - `ryeos node large-objects purge-quarantine` — discard large objects scrub quarantined
//!   - `ryeos node reset authorization` — retire grants and restore the operator
//!   - `ryeos node reset policy-generation` — explicit node-policy schema cut
//!   - `ryeos node policy-apply` — replace one member of the complete signed policy generation
//!   - `ryeos node host-access grant|revoke|list` — the operator's host-direct grants (a `host_access` policy apply)
//!   - `ryeos node bundle-set update` — install one exact remotely published bundle set
//!   - `ryeos node host setup` — one-time administrator-owned host association
//!   - `ryeos node host retire` — retire a stopped node's host service, or every orphaned one
//!   - `ryeos node external-execution compose` — compose and install a node-signed external placement binding on a stopped node
//!   - `ryeos node external-execution provision-credential` — provision the placement credential an installed binding names into the node vault
//!   - `ryeos node external-execution qualification-session-compose` — compose and install the node-signed qualification-only session binding on a stopped node
//!   - `ryeos node external-execution snapshot-compose` — compose and install a node-signed runtime snapshot production binding on a stopped node
//!   - `ryeos node external-execution snapshot-qualification-compose` — compose and install a node-signed snapshot qualification binding on a stopped node
//!   - `ryeos node external-execution recompose` — re-pin installed bindings to the installed bundles after a payload change on a stopped node
//!
//! One client composite is listed here for help but dispatched after the
//! verified node config loads, because it sequences daemon commands:
//!
//!   - `ryeos codex login device` — the Codex device-login ceremony (see `codex_device_login.rs`)
//!   - `ryeos external-content product publish` — produce, capture, admit and push one product (see `product_publish.rs`)
//!
//! `ryeos identity` is local as a bootstrap affordance: remote
//! operators need to copy their node public key before the daemon is running.
//!
//! The commands that change a stopped node (`allow-restart`, `capacity`,
//! `reset`, `policy-apply`, `external-execution compose` and its snapshot
//! forms) build a signed offline-operation request and apply it in process
//! through the library the daemon applies at startup; see `offline_apply.rs`.
//!
//! All other commands — including `sign`, `verify`, `fetch` — are
//! descriptor-driven and dispatched through the offline/dual path
//! (see `offline_dispatch.rs`) or forwarded to the daemon.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
    #[cfg(feature = "model-provider-setup")]
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
        tokens: &["node", "offline-operation", "facts"],
        summary: "Print the node facts a signed offline-operation request is signed against; works with the daemon stopped; --remote reads a remote's through this node",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "offline-operation", "sign"],
        summary: "Sign an offline-operation request against a node's facts, without the node",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "offline-operation", "submit"],
        summary: "Send a signed offline-operation request to a running node's route, or through this node to a remote with --remote; --stop makes the node apply it at once",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "offline-operation", "list"],
        summary: "Show what a node has queued, applied and refused as offline operations",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "allow-restart"],
        summary: "Clear a node's restart inhibition after resolving why it stopped itself",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "capacity", "reissue"],
        summary: "Open the next native-capacity epoch on a stopped node, under the operator key",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "capacity", "reanchor"],
        summary: "Anchor a stopped node's native-capacity account at the filesystem its data moved to, under the operator key",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "external-execution", "compose"],
        summary: "Compose and sign a stopped node's external-execution binding from installed bundles",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "external-execution", "provision-credential"],
        summary: "Store the placement credential for an installed binding, read from stdin",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &[
            "node",
            "external-execution",
            "qualification-session-compose",
        ],
        summary: "Compose and sign a stopped node's qualification-only session placement binding",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "external-execution", "snapshot-compose"],
        summary: "Compose and sign a stopped node's runtime snapshot production binding",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &[
            "node",
            "external-execution",
            "snapshot-qualification-compose",
        ],
        summary: "Compose and sign a stopped node's runtime snapshot qualification binding",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "external-execution", "recompose"],
        summary: "Re-pin a stopped node's bindings to its installed bundles after a payload change",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "reset", "execution-history"],
        summary: "Retire the local execution-history epoch",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "reset", "execution-history-scratch"],
        summary: "Clear execution-history reset scratch the next reset cannot settle",
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
        tokens: &["node", "large-objects", "purge-quarantine"],
        summary: "Discard the damaged large objects scrub quarantined",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "reset", "policy-generation"],
        summary: "Replace an obsolete node-policy generation",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "policy-apply"],
        summary: "Validate and install a node-owned policy",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "host-access", "grant"],
        summary: "Grant one exact host-authority tool host-direct execution",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "host-access", "revoke"],
        summary: "Remove one host_access grant",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "host-access", "list"],
        summary: "List host_access grants and whether they match a project",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "operator", "rotate"],
        summary: "Rotate the operator key on a stopped node: the old key is retired and the new key takes over its grant",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "authoring", "rotate"],
        summary: "Rotate the authoring key on a stopped node: the new key keeps the old key's scope",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["trust", "pin"],
        summary: "Pin a signer key from a PUBLISHER_TRUST.toml on a stopped node",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["trust", "scope"],
        summary: "Narrow or widen a pinned signer's scope on a stopped node",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["trust", "retire"],
        summary: "Retire a pinned signer on a stopped node; it still verifies existing items",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["trust", "revoke"],
        summary: "Revoke a pinned signer on a stopped node; it verifies nothing",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["trust", "succeed"],
        summary: "Replace a pinned signer by the key a signed succession statement names",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "bundle-set", "update"],
        summary: "Install an exact published bundle set while the node is stopped",
        category: "maintenance",
    },
    LocalCommandDescriptor {
        tokens: &["node", "host", "setup"],
        summary: "Provision one administrator-owned local hosted-worker service",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["node", "host", "retire"],
        summary: "Retire a stopped node's host service, or every orphaned one",
        category: "lifecycle",
    },
    LocalCommandDescriptor {
        tokens: &["external-content", "product", "publish"],
        summary: "Produce a named product, capture and admit it, and push it when asked",
        category: "external-content",
    },
    LocalCommandDescriptor {
        tokens: &["codex", "login", "device"],
        summary: "Log a Codex credential profile in with a device code, end to end",
        category: "credentials",
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
        #[cfg(feature = "model-provider-setup")]
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
        ("node", Some("offline-operation")) if argv.get(2).map(String::as_str) == Some("facts") => {
            run_node_offline_operation_facts_command(&argv[3..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("offline-operation")) if argv.get(2).map(String::as_str) == Some("sign") => {
            crate::lifecycle_offline_operation::run_sign(&argv[3..], console)
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("offline-operation"))
            if argv.get(2).map(String::as_str) == Some("submit") =>
        {
            crate::lifecycle_offline_operation::run_submit(&argv[3..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("offline-operation")) if argv.get(2).map(String::as_str) == Some("list") => {
            crate::lifecycle_offline_operation::run_list(&argv[3..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("allow-restart")) => {
            run_node_allow_restart_command(&argv[2..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("capacity")) if argv.get(2).map(String::as_str) == Some("reissue") => {
            run_node_capacity_reissue_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("capacity")) if argv.get(2).map(String::as_str) == Some("reanchor") => {
            run_node_capacity_reanchor_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("external-execution"))
            if argv.get(2).map(String::as_str) == Some("compose") =>
        {
            crate::lifecycle_external_compose::run_external_compose(&argv[3..], console)
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("external-execution"))
            if argv.get(2).map(String::as_str) == Some("provision-credential") =>
        {
            crate::lifecycle_external_credential::run_external_provision_credential(
                &argv[3..],
                console,
            )
            .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("external-execution"))
            if argv.get(2).map(String::as_str) == Some("qualification-session-compose") =>
        {
            crate::lifecycle_external_qualification_compose::run_qualification_session_compose(
                &argv[3..],
                console,
            )
            .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("external-execution"))
            if argv.get(2).map(String::as_str) == Some("snapshot-compose") =>
        {
            crate::lifecycle_external_snapshot_compose::run_snapshot_compose(&argv[3..], console)
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("external-execution"))
            if argv.get(2).map(String::as_str) == Some("snapshot-qualification-compose") =>
        {
            crate::lifecycle_external_snapshot_compose::run_snapshot_qualification_compose(
                &argv[3..],
                console,
            )
            .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("external-execution"))
            if argv.get(2).map(String::as_str) == Some("recompose") =>
        {
            crate::lifecycle_external_recompose::run_recompose(&argv[3..], console)
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("reset")) if argv.get(2).map(String::as_str) == Some("execution-history") => {
            run_execution_history_reset_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("reset"))
            if argv.get(2).map(String::as_str) == Some("execution-history-scratch") =>
        {
            run_execution_history_scratch_command(&argv[3..], console).map_err(map_local_err)?;
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
        ("node", Some("large-objects"))
            if argv.get(2).map(String::as_str) == Some("purge-quarantine") =>
        {
            run_node_large_object_quarantine_purge_command(&argv[3..], console)
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("reset")) if argv.get(2).map(String::as_str) == Some("policy-generation") => {
            run_node_policy_generation_reset_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("trust", Some(verb @ ("pin" | "scope" | "retire" | "revoke" | "succeed"))) => {
            crate::lifecycle_trust::run(verb, &argv[2..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("operator")) if argv.get(2).map(String::as_str) == Some("rotate") => {
            crate::lifecycle_key_rotation::run_operator(&argv[3..], console)
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("authoring")) if argv.get(2).map(String::as_str) == Some("rotate") => {
            crate::lifecycle_key_rotation::run_authoring(&argv[3..], console)
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("policy-apply")) => {
            run_node_policy_apply_command(&argv[2..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("host-access")) if argv.get(2).map(String::as_str) == Some("grant") => {
            crate::lifecycle_host_access::run_grant(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("host-access")) if argv.get(2).map(String::as_str) == Some("revoke") => {
            crate::lifecycle_host_access::run_revoke(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("host-access")) if argv.get(2).map(String::as_str) == Some("list") => {
            crate::lifecycle_host_access::run_list(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("bundle-set")) if argv.get(2).map(String::as_str) == Some("update") => {
            crate::bundle_set_update::run(&argv[3..], console)
                .await
                .map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("host")) if argv.get(2).map(String::as_str) == Some("setup") => {
            run_node_host_setup_command(&argv[3..], console).map_err(map_local_err)?;
            Ok(true)
        }
        ("node", Some("host")) if argv.get(2).map(String::as_str) == Some("retire") => {
            run_node_host_retire_command(&argv[3..], console).map_err(map_local_err)?;
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
    name = "ryeos node host setup",
    about = "Provision one explicit administrator-owned hosted-worker service",
    long_about = "Creates the supported local host association for this existing node and account. It records the selected account, durable app-root identity, installed daemon image and Lillux process-scope delegation in administrator-owned host configuration, then leaves the service down. It never reads worker input or node policy as root. This is a one-time host operation; normal ryeos start/stop/status remain unprivileged afterwards.",
    no_binary_name = true
)]
struct NodeHostSetupArgs {
    /// Existing app root (defaults to the normal local node).
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Persist this node's TCP endpoint before installing supervision.
    #[arg(long)]
    bind: Option<SocketAddr>,

    /// Persist this node's local lifecycle endpoint before supervision.
    #[arg(long)]
    uds_path: Option<PathBuf>,

    /// Required acknowledgement that this installs a root-owned service
    /// association for the current account and node.
    #[arg(long)]
    confirm: bool,
    /// Finite enclosing memory ceiling for the node and its worker scopes.
    #[arg(long)]
    node_memory_bytes: u64,

    /// Finite enclosing task ceiling for the node and its worker scopes.
    #[arg(long)]
    node_processes: u32,
}

fn run_node_host_setup_command(argv: &[String], _console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeHostSetupArgs>(argv, _console)? else {
        return Ok(());
    };
    if !args.confirm {
        anyhow::bail!("host setup requires --confirm");
    }
    let node_resources = lillux::ProcessScopeResourceLimits {
        maximum_memory_bytes: args.node_memory_bytes,
        maximum_processes: args.node_processes,
    };
    node_resources.validate().map_err(anyhow::Error::msg)?;
    let existing = ryeos_node::NodeConfig::load_local(args.app_root)?;
    ryeos_node::require_initialized(&existing.app_root)?;
    let config = ryeos_node::supervision::configure_host_endpoints(
        &existing.app_root,
        args.bind,
        args.uds_path,
    )?;
    let account = lillux::ControllerAccount::current().map_err(anyhow::Error::msg)?;
    let daemon = installed_daemon()?;
    let arguments = vec![
        "host-provision".into(),
        "--app-root".into(),
        config.app_root.into_os_string(),
        "--controller-account-json".into(),
        serde_json::to_string(&account)?.into(),
        "--node-resources-json".into(),
        serde_json::to_string(&node_resources)?.into(),
    ];
    lillux::run_as_administrator(&daemon, &arguments)
        .with_context(|| format!("run administrator host setup through {}", daemon.display()))?;
    Ok(())
}

/// The daemon installed beside this CLI: host maintenance runs exactly it.
fn installed_daemon() -> Result<PathBuf> {
    Ok(std::env::current_exe()
        .context("locate installed ryeos CLI")?
        .parent()
        .context("installed ryeos CLI has no binary directory")?
        .join("ryeosd"))
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node host retire",
    about = "Retire an administrator-owned host service",
    long_about = "Removes the native host service that `ryeos node host setup` provisioned: the service is brought down, deactivated, and its service definition and logs are removed. With --app-root (or the default node), the node must be stopped; nothing is written in its app root, and the node can still run unsupervised. With --orphaned, every RyeOS node service whose recorded app root no longer exists is listed and retired; a service whose app root exists is never touched by the sweep. Runs the installed ryeosd as administrator, like host setup.",
    no_binary_name = true
)]
struct NodeHostRetireArgs {
    /// App root whose host service to retire (defaults to the normal local node).
    #[arg(long, conflicts_with = "orphaned")]
    app_root: Option<PathBuf>,

    /// Retire every RyeOS node service whose recorded app root no longer exists.
    #[arg(long)]
    orphaned: bool,

    /// Required acknowledgement that this removes a root-owned service
    /// association.
    #[arg(long)]
    confirm: bool,
}

/// The administrator daemon arguments for one retirement request.
fn host_retire_arguments(args: &NodeHostRetireArgs) -> Result<Vec<std::ffi::OsString>> {
    if !args.confirm {
        anyhow::bail!("host retire requires --confirm");
    }
    if args.orphaned {
        return Ok(vec!["host-retire".into(), "--orphaned".into()]);
    }
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    Ok(vec![
        "host-retire".into(),
        "--app-root".into(),
        app_root.into_os_string(),
    ])
}

fn run_node_host_retire_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeHostRetireArgs>(argv, console)? else {
        return Ok(());
    };
    let arguments = host_retire_arguments(&args)?;
    if args.orphaned {
        // The listing is unprivileged; the administrator re-checks every root
        // before it retires anything.
        let orphans = ryeos_node::supervision::orphaned_host_services()?.context(
            "this host has no native service directory readable without root; nothing to list",
        )?;
        if orphans.is_empty() {
            console.text("no orphaned host services")?;
            return Ok(());
        }
        for service in &orphans {
            console.text(&format!(
                "orphaned host service {} (app root gone: {})",
                service.name,
                service
                    .app_root
                    .as_deref()
                    .map(|root| root.display().to_string())
                    .unwrap_or_default()
            ))?;
        }
    }
    let daemon = installed_daemon()?;
    lillux::run_as_administrator(&daemon, &arguments)
        .with_context(|| format!("run administrator host retire through {}", daemon.display()))?;
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset policy-generation",
    about = "Replace an obsolete node-policy generation from the image seed",
    long_about = "Explicitly replace one complete stopped-node policy generation that the current registry can no longer decode. The image seed (--source) is measured, and the publisher-signed init profile inside it selects the prospective exact bundle inventory; bundle installation and policy publication occur in the same locked init transaction, under a signed offline-operation request that pins the seed's digest. Node identity, credentials, execution history, project heads, and other state are preserved.",
    no_binary_name = true
)]
struct NodePolicyGenerationResetArgs {
    /// Image seed root containing the bundles and `.ai/node/init/profiles/`.
    /// It is measured; the request pins its digest.
    #[arg(long, default_value = "/usr/share/ryeos")]
    source: PathBuf,

    /// Exact publisher-signed profile to install (for example `full`).
    #[arg(long)]
    node_profile: String,

    /// Host class the node was initialized under; the replacement profile
    /// must declare it. A node's host class does not change.
    #[arg(long)]
    host_class: String,

    /// Additional publisher trust doc(s) needed to verify the replacement
    /// profile and selected source bundles. Each must lie inside `--source`:
    /// the seed digest is what vouches for it.
    #[arg(long = "trust-file", action = clap::ArgAction::Append)]
    trust_files: Vec<PathBuf>,

    /// Digest (`sha256:<hex>`) of the substrate image the seed belongs to.
    /// Defaults to the image the node recorded when it was initialized.
    #[arg(long)]
    substrate_image_digest: Option<String>,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Keep this section's installed policy (repeatable). Needed only for a
    /// node whose generation predates the operator record, or to keep a
    /// section the record does not name; recorded operator sections carry
    /// forward on their own.
    #[arg(long = "keep-section", action = clap::ArgAction::Append)]
    keep_sections: Vec<String>,

    /// Take this section from the new profile even if an operator applied it
    /// (repeatable).
    #[arg(long = "reset-section", action = clap::ArgAction::Append)]
    reset_sections: Vec<String>,

    /// Required acknowledgement that the node's policy generation is
    /// replaced: profile sections are taken from the seed, operator-applied
    /// sections carry forward unless reset.
    #[arg(long)]
    confirm: bool,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

impl NodePolicyGenerationResetArgs {
    fn validate(&self) -> Result<()> {
        if !self.confirm {
            anyhow::bail!("replacing the complete node-policy generation requires --confirm");
        }
        ryeos_node_runtime::node_policy::generation::validate_init_profile_name(&self.node_profile)
            .context("validate node init profile name")?;
        if !ryeos_engine::isolation::valid_host_class_name(&self.host_class) {
            anyhow::bail!("host class `{}` is not a canonical name", self.host_class);
        }
        Ok(())
    }

    /// Each trust document as a path inside the seed.
    fn trust_documents(&self) -> Result<Vec<String>> {
        self.trust_files
            .iter()
            .map(|file| {
                file.strip_prefix(&self.source)
                    .map(|relative| relative.to_string_lossy().into_owned())
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "trust file {} is outside the image seed {}; the seed digest cannot vouch for it",
                            file.display(),
                            self.source.display()
                        )
                    })
            })
            .collect()
    }
}

fn run_node_policy_generation_reset_command(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodePolicyGenerationResetArgs>(argv, console)? else {
        return Ok(());
    };
    args.validate()?;
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let trust_documents = args.trust_documents()?;
    let seed_digest = ryeos_node::offline_operation::seed::seed_tree_digest(&args.source)
        .context("measure the image seed")?;
    let substrate_image_digest = match &args.substrate_image_digest {
        Some(digest) => digest.clone(),
        None => ryeos_node::load_verified_substrate_identity(&app_root)
            .context("read the substrate image the node was initialized under; pass --substrate-image-digest")?
            .image_digest,
    };
    let policy_carry = policy_carry_choices(&args.keep_sections, &args.reset_sections);
    let arguments = serde_json::to_value(
        ryeos_node::offline_operation::handlers::PolicyReplaceArguments {
            seed_digest,
            node_profile: args.node_profile.clone(),
            host_class: args.host_class.clone(),
            substrate_image_digest: substrate_image_digest.clone(),
            trust_documents,
            policy_carry: policy_carry.clone(),
        },
    )?;
    // The request pins the generation the seed will install; the operation
    // recomputes it and refuses before any change if it differs.
    let policy_generation = ryeos_node::prospective_replacement_policy_digest(
        &app_root,
        &args.source,
        &args.node_profile,
        &args.host_class,
        &args.trust_files,
        &policy_carry,
    )
    .context("compute the policy generation the image seed would install")?;
    let host = crate::offline_apply::CliHost::new()
        .with_image_seed(args.source.clone(), substrate_image_digest);
    let applied = host.apply(
        &app_root,
        "node.reset.policy-generation",
        arguments,
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::from([("policy_generation".to_owned(), policy_generation)]),
    )?;

    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": "replaced",
            "node_profile": args.node_profile,
            "host_class": args.host_class,
            "app_root": app_root,
            "node_fingerprint": applied.result["node_fingerprint"],
            "bundles": applied.result["bundles"],
            "receipt": applied.receipt.digest()?,
        }))?;
    } else {
        console.text(&format!(
            "Node policy generation and exact bundle inventory replaced from trusted profile `{}`: {}\n",
            args.node_profile,
            app_root.display()
        ))?;
    }
    Ok(())
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

/// Largest policy source the command reads; the operation's input slot
/// allows the same.
const POLICY_SOURCE_MAX_BYTES: u64 = 256 * 1024;

fn run_node_policy_apply_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodePolicyApplyArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let policy =
        crate::offline_apply::read_input(&args.source, "policy source", POLICY_SOURCE_MAX_BYTES)?;
    // The request pins the generation it will publish. Computing it writes
    // nothing; the operation recomputes and refuses before any change if the
    // node's policy moved in between.
    let policy_generation =
        ryeos_node::offline_operation::handlers::prospective_policy_apply_digest(
            &app_root,
            &args.section,
            &policy,
        )?;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.policy-apply",
        serde_json::to_value(
            ryeos_node::offline_operation::handlers::PolicyApplyArguments {
                section: args.section.clone(),
            },
        )?,
        std::collections::BTreeMap::from([("policy".to_owned(), policy)]),
        std::collections::BTreeMap::from([("policy_generation".to_owned(), policy_generation)]),
    )?;
    let installed = ryeos_node_runtime::node_policy::generation::policy_directory(&app_root)
        .join(format!("{}.yaml", args.section));

    // Applying `native_capacity` also amends the capacity issuance to the
    // new policy; the operation reports that amendment beside the install.
    let capacity_amendment = applied
        .result
        .get("capacity_amendment")
        .filter(|value| !value.is_null())
        .cloned();
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": "installed",
            "section": args.section,
            "path": installed,
            "signer_fingerprint": applied.result["signer_fingerprint"],
            "receipt": applied.receipt.digest()?,
            "capacity_amendment": capacity_amendment,
        }))?;
    } else {
        console.text(&format!(
            "Installed signed node policy: {}\n",
            installed.display()
        ))?;
        if let Some(amendment) = &capacity_amendment {
            let already = amendment["already_applied"].as_bool().unwrap_or(false);
            console.text(&format!(
                "Capacity issuance {} policy {}{}\n",
                if already {
                    "already carries"
                } else {
                    "amended to"
                },
                amendment["policy_digest"].as_str().unwrap_or("?"),
                amendment["document_digest"]
                    .as_str()
                    .map(|digest| format!(" (document {digest})"))
                    .unwrap_or_default()
            ))?;
        }
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset replay-indexes",
    about = "Activate the current replay-index contract",
    long_about = "Perform the explicit clean-cut replay-index activation. The daemon must be stopped. A stale epoch retires every replay-index row, including provider-call rows, so provider-call idempotence keys are discarded: an interrupted provider call is not recognized as already made. Run it only with no execution in flight; otherwise use the execution-history reset. Current indexes are unchanged. Credentials, thread history, CAS content, sync state, admission attestations, and accounting state are preserved.",
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
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.reset.replay-indexes",
        serde_json::json!({}),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    )?;
    let report = host
        .take_captured()
        .replay_indexes
        .context("the replay-index reset reported no activation")?;
    let path = applied.result["database"].clone();
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": "activated",
            "database": path,
            "replay_indexes": report,
            "native_connections": applied.result["native_connections"],
        }))?;
    } else {
        let mut status =
            crate::tty::StatusBanner::new(crate::tty::Tone::Success, "REPLAY INDEX RESET COMPLETE");
        status.detail = path.as_str().map(str::to_owned);
        status.rows = vec![
            crate::tty::Row::key_value("replay indexes", replay_reset_summary(report)),
            crate::tty::Row::key_value(
                "preserved",
                "credentials, thread history, CAS bytes and non-replay operational state",
            ),
        ];
        console.success(&status)?;
    }
    Ok(())
}

fn replay_reset_summary(report: ryeos_state::operational::ReplayIndexResetReport) -> String {
    use ryeos_state::operational::ReplayIndexResetScope;
    let scope = match report.scope {
        ReplayIndexResetScope::Unchanged => "unchanged",
        ReplayIndexResetScope::AllReplayRecords => {
            "retire all replay rows, including provider-call rows"
        }
    };
    format!(
        "epoch {} -> {}: {scope}",
        report.stored_epoch, report.current_epoch
    )
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
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let (bindings, native_connections) = if args.dry_run {
        // A preview changes nothing, so it is not an operation: it only needs
        // the existing lock held read-only to prove the daemon is stopped.
        let config =
            ryeos_node_runtime::config::Config::load(&ryeos_node_runtime::config::ConfigSources {
                app_root: Some(app_root.clone()),
                ..Default::default()
            })
            .context("load local node configuration for external-content binding reset")?;
        let state_lock = ryeos_node_runtime::state_lock::StateLock::acquire_existing_read_only(
            &ryeos_node_runtime::state_lock::default_lock_path(&config.app_root),
        )
        .context("external-content binding reset requires the daemon to be stopped")?;
        let reset =
            ryeos_app::offline_native::reset_external_content_bindings(&config, &state_lock, true)
                .context("reset external-content bindings through the offline native host")?;
        (
            serde_json::json!(reset.bindings),
            serde_json::json!(reset.facts.tickets_issued),
        )
    } else {
        let host = crate::offline_apply::CliHost::new();
        let applied = host.apply(
            &app_root,
            "node.reset.external-content-bindings",
            serde_json::json!({}),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        )?;
        (
            applied.result["bindings"].clone(),
            applied.result["native_connections"].clone(),
        )
    };
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "status": if args.dry_run { "inspected" } else { "retired" },
            "bindings": bindings,
            "requires_reimport": !args.dry_run,
            "native_connections": native_connections,
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
    name = "ryeos node large-objects purge-quarantine",
    about = "Discard the damaged large objects scrub quarantined",
    long_about = "Remove every large object, and its sidecar, that `external-content scrub` moved into quarantine because its bytes no longer matched their hash. The daemon must be stopped. Quarantined copies are kept as evidence and count against the large-object budget until this is run; nothing else removes them. Served objects, bindings and staging are untouched. --dry-run only counts.",
    no_binary_name = true
)]
struct NodeLargeObjectQuarantinePurgeArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Count the quarantined objects without removing them.
    #[arg(long)]
    dry_run: bool,

    /// Required acknowledgement that the quarantined evidence is discarded.
    #[arg(long)]
    confirm: bool,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

fn run_node_large_object_quarantine_purge_command(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeLargeObjectQuarantinePurgeArgs>(argv, console)?
    else {
        return Ok(());
    };
    if !args.dry_run && !args.confirm {
        anyhow::bail!("purging the large-object quarantine requires --confirm");
    }
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let report = if args.dry_run {
        // A preview changes nothing, so it is not an operation: it only needs
        // the existing lock held read-only to prove the daemon is stopped.
        let config =
            ryeos_node_runtime::config::Config::load(&ryeos_node_runtime::config::ConfigSources {
                app_root: Some(app_root.clone()),
                ..Default::default()
            })
            .context("load local node configuration for large-object quarantine purge")?;
        let state_lock = ryeos_node_runtime::state_lock::StateLock::acquire_existing_read_only(
            &ryeos_node_runtime::state_lock::default_lock_path(&config.app_root),
        )
        .context("large-object quarantine purge requires the daemon to be stopped")?;
        serde_json::to_value(
            ryeos_app::operator_external_content::purge_quarantined_large_objects(
                &config,
                &state_lock,
                true,
            )
            .context("inspect the large-object quarantine")?,
        )?
    } else {
        let host = crate::offline_apply::CliHost::new();
        host.apply(
            &app_root,
            "node.large-objects.purge-quarantine",
            serde_json::json!({}),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        )?
        .result
    };
    if args.json {
        crate::tty::write_json(&report)?;
        return Ok(());
    }
    let objects = report["objects"].as_u64().unwrap_or_default();
    let bytes = report["bytes"].as_u64().unwrap_or_default();
    if args.dry_run {
        console.text(&format!(
            "Large-object quarantine preview: {objects} damaged object(s), {bytes} byte(s) would be removed.\n"
        ))?;
    } else {
        console.text(&format!(
            "Large-object quarantine purged: removed {objects} damaged object(s), {bytes} byte(s).\n"
        ))?;
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset authorization",
    about = "Discard every authorized-key grant and recreate only the local operator grant",
    long_about = "Perform the explicit no-backcompat authorized-key cutover. The daemon must be stopped. Every local-client, remote-node, and remote-principal grant is discarded atomically; only the configured local operator key is re-authorized. Remote nodes and forwarded operators must be authorized again.",
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
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.reset.authorization",
        serde_json::json!({}),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    )?;
    let discarded = applied.receipt.body.discarded_grant_digests.len();
    let operator = applied.result["operator_fingerprint"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "discarded_grants": discarded,
            "operator_fingerprint": operator,
            "authorized_keys_dir": applied.result["authorized_keys_dir"],
            "receipt": applied.receipt.digest()?,
        }))?;
    } else {
        let mut status = crate::tty::StatusBanner::new(
            crate::tty::Tone::Success,
            "AUTHORIZATION RESET COMPLETE",
        );
        status.rows = vec![
            crate::tty::Row::key_value("discarded grants", discarded.to_string()),
            crate::tty::Row::key_value("operator", operator),
            crate::tty::Row::key_value("remote nodes", "re-admission required"),
        ];
        console.success(&status)?;
    }
    Ok(())
}

// ── ryeos node reset execution-history-scratch ───────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset execution-history-scratch",
    about = "Clear execution-history reset scratch the next reset cannot settle",
    long_about = "Remove every offline execution-history reset record, inspection copy, reset temporary and inert staged operation file, and a thread-history discard marker of a retired schema. The daemon must be stopped. An ordinary `ryeos node reset execution-history` settles an interrupted attempt by itself; use this when it refuses and names this command. The staged operation of an operation State still marks active is kept, so the next reset resumes it. Names outside the reset's own grammar in its directories, and entries not of the kind their name declares, are left in place and reported; a reset still refuses while they remain, so remove them by hand. --dry-run only inspects.",
    no_binary_name = true
)]
struct ExecutionHistoryScratchArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Required acknowledgement that the reset scratch is removed.
    #[arg(long)]
    confirm: bool,

    /// Report what would be removed without removing anything.
    #[arg(long)]
    dry_run: bool,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

fn run_execution_history_scratch_command(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<ExecutionHistoryScratchArgs>(argv, console)? else {
        return Ok(());
    };
    if !args.dry_run && !args.confirm {
        anyhow::bail!("clearing execution-history reset scratch requires --confirm");
    }
    let report = if args.dry_run {
        // A preview changes nothing, so it is not an operation.
        ryeos_app::execution_history_reset::repair_execution_history_scratch(
            &ryeos_app::execution_history_reset::ExecutionHistoryScratchRepairOptions {
                app_root: args.app_root,
                dry_run: true,
            },
        )
        .context("execution-history scratch repair failed")?
    } else {
        let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
        let host = crate::offline_apply::CliHost::new();
        host.apply(
            &app_root,
            "node.reset.execution-history-scratch",
            serde_json::json!({}),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        )?;
        host.take_captured()
            .scratch
            .context("the scratch repair reported nothing")?
    };
    if args.json {
        crate::tty::write_json(&report)?;
        return Ok(());
    }
    let location = |location: ryeos_app::execution_history_reset::ScratchLocation| match location {
        ryeos_app::execution_history_reset::ScratchLocation::Ledger => "ledger",
        ryeos_app::execution_history_reset::ScratchLocation::Inspections => "inspections",
        ryeos_app::execution_history_reset::ScratchLocation::RuntimeNamespace => "runtime",
    };
    let list = |entries: &[ryeos_app::execution_history_reset::ScratchEntry]| {
        if entries.is_empty() {
            "none".to_owned()
        } else {
            entries
                .iter()
                .map(|entry| format!("{}/{}", location(entry.location), entry.name))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut status = crate::tty::StatusBanner::new(
        if report.left.is_empty() {
            crate::tty::Tone::Success
        } else {
            crate::tty::Tone::Warning
        },
        if report.dry_run {
            "EXECUTION HISTORY SCRATCH PREVIEW"
        } else {
            "EXECUTION HISTORY SCRATCH CLEARED"
        },
    );
    status.detail = Some(report.app_root.display().to_string());
    status.rows = vec![
        crate::tty::Row::key_value(
            if report.dry_run {
                "would remove"
            } else {
                "removed"
            },
            list(&report.removed),
        ),
        crate::tty::Row::key_value(
            "left in place",
            if report.left.is_empty() {
                list(&report.left)
            } else {
                format!(
                    "{}; a reset still refuses while these remain: remove them by hand",
                    list(&report.left)
                )
            },
        ),
        crate::tty::Row::key_value(
            "retired discard marker",
            report.retired_marker_schema.map_or_else(
                || "none".to_owned(),
                |schema| format!("schema {schema} cleared"),
            ),
        ),
        crate::tty::Row::key_value(
            "active operation kept",
            report
                .active_operation
                .clone()
                .unwrap_or_else(|| "none".to_owned()),
        ),
    ];
    console.success(&status)?;
    Ok(())
}

// ── ryeos node reset execution-history ───────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node reset execution-history",
    about = "Retire the local execution-history epoch while the daemon is stopped",
    long_about = "Retire every authoritative thread-chain head, clear execution recovery rows/files and scheduler fire history, activate stale replay indexes, and publish an empty current thread projection. Replay activation of a stale epoch retires every replay row, including provider-call rows. This is an offline schema/authority reset, not storage garbage collection. Principal and deployed project HEADs are preserved unless --include-project-heads is selected. Node identity, trust, config, installed bundles, vault data, signed schedule definitions, operational sync/admission state, and independently retained logs/caches are preserved. Restart the daemon and run ordinary `ryeos maintenance gc` later to reclaim newly unreachable CAS storage.",
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

    /// Exact predecessor runtime schema epoch for an idempotent hosted cut.
    #[arg(long = "schema-cut-from", requires = "schema_cut_to")]
    schema_cut_from: Option<u32>,

    /// Exact current runtime schema epoch for an idempotent hosted cut.
    #[arg(long = "schema-cut-to", requires = "schema_cut_from")]
    schema_cut_to: Option<u32>,

    /// Attest that this host has rebooted since the Runtime store's process
    /// scope lifetime fence was written. A fence this binary cannot read is
    /// then discarded instead of refusing the reset; where its boot id is
    /// still readable it must differ from the current boot. Recorded in the
    /// report.
    #[arg(long = "discard-unreadable-lifetime-fence-after-reboot")]
    discard_unreadable_lifetime_fence_after_reboot: bool,
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
        if matches!((self.schema_cut_from, self.schema_cut_to), (Some(from), Some(to)) if from == to)
        {
            anyhow::bail!("execution-history schema cut must name distinct epochs");
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

    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let progress =
        crate::tty::ExecutionHistoryResetProgress::new(!args.json, console.capabilities());
    let (report, progress) = if args.dry_run {
        // A preview changes nothing, so it is not an operation; it only needs
        // the existing lock held read-only.
        let options =
            ryeos_app::execution_history_reset::ExecutionHistoryResetOptions {
                app_root: Some(app_root.clone()),
                dry_run: true,
                discard_project_heads: args.include_project_heads,
                schema_cut: args
                    .schema_cut_from
                    .zip(args.schema_cut_to)
                    .map(|(from, to)| {
                        ryeos_app::execution_history_reset::ExecutionHistorySchemaCut { from, to }
                    }),
                discard_unreadable_lifetime_fence_after_reboot: args
                    .discard_unreadable_lifetime_fence_after_reboot,
            };
        // The stopped node's installed, trusted composition decides whether
        // the scheduler participant takes part, exactly as daemon startup
        // decides it. An unselected scheduler is never opened or created; the
        // reset then proves the runtime holds no scheduler namespace instead.
        let participant = crate::offline_apply::scheduler_participant(&app_root)?;
        let mut progress = progress;
        let report = match progress.as_mut() {
            Some(progress) => {
                let mut observer =
                    |event: &ryeos_app::execution_history_reset::ExecutionHistoryResetProgress| {
                        progress.observe(event);
                    };
                ryeos_app::execution_history_reset::run_execution_history_reset_with_progress(
                    &options,
                    participant.as_deref(),
                    &mut observer,
                )
            }
            None => ryeos_app::execution_history_reset::run_execution_history_reset(
                &options,
                participant.as_deref(),
            ),
        }
        .context("offline execution-history reset failed")?;
        (report, progress)
    } else {
        let host = crate::offline_apply::CliHost::new().with_progress(progress);
        host.apply(
            &app_root,
            "node.reset.execution-history",
            serde_json::to_value(
                ryeos_node::offline_operation::handlers::ExecutionHistoryResetArguments {
                    discard_project_heads: args.include_project_heads,
                    schema_cut: args
                        .schema_cut_from
                        .zip(args.schema_cut_to)
                        .map(|(from, to)| {
                            ryeos_node::offline_operation::handlers::SchemaCutArguments { from, to }
                        }),
                    discard_unreadable_lifetime_fence_after_reboot: args
                        .discard_unreadable_lifetime_fence_after_reboot,
                },
            )?,
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        )?;
        host.finish_progress()?;
        let report = host
            .take_captured()
            .execution_history
            .context("the reset reported nothing")?;
        (report, None)
    };
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
        } else if !report.performed {
            "EXECUTION HISTORY SCHEMA ALREADY CURRENT"
        } else {
            "EXECUTION HISTORY RESET COMPLETE"
        },
    );
    status.detail = Some(report.app_root.display().to_string());
    status.rows = vec![
        crate::tty::Row::key_value("chain heads", report.chain_heads.to_string()),
        crate::tty::Row::key_value("project heads", report.project_heads.to_string()),
        crate::tty::Row::key_value(
            "replay indexes",
            replay_reset_summary(report.replay_indexes),
        ),
        crate::tty::Row::key_value(
            "chain/recovery artifacts",
            (report.chain_ref_artifacts + report.pending_transitions).to_string(),
        ),
        crate::tty::Row::key_value("retirement fences", report.retirement_fences.to_string()),
        crate::tty::Row::key_value(
            "evidence obligations",
            report.evidence_obligations.to_string(),
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
            report.scheduler_rows.total_rows().map_or_else(
                || "unavailable (incompatible schema)".to_string(),
                |rows| rows.to_string(),
            ),
        ),
        crate::tty::Row::key_value(
            "scheduler journal artifacts",
            report.scheduler_journal_artifacts.to_string(),
        ),
        crate::tty::Row::key_value(
            "old projection stores",
            report.projection.superseded_instances_deleted.to_string(),
        ),
        crate::tty::Row::key_value(
            "unreadable lifetime fence",
            match &report.lifetime_fence_discard {
                None => "none".to_owned(),
                Some(discard) => match (&discard.stored_boot_id, discard.boot_id_verified) {
                    (Some(boot_id), true) => format!(
                        "discarded after attested reboot; stored boot {boot_id} differs from the current boot ({})",
                        discard.reason
                    ),
                    _ => format!(
                        "discarded on the operator's reboot attestation alone; its boot id is unreadable ({})",
                        discard.reason
                    ),
                },
            },
        ),
        crate::tty::Row::key_value(
            "Q local custody settled",
            match &report.qualification_custody {
                None => "none open".to_owned(),
                Some(custody) => format!(
                    "{} Q local start(s): {} workspace root(s) and {} state root(s) {}, {} absent; {} guest scratch row(s): {} root(s) {}, {} absent",
                    custody.local_starts,
                    custody.workspace_roots_removed,
                    custody.state_roots_removed,
                    if custody.performed {
                        "removed"
                    } else {
                        "to remove"
                    },
                    custody.workspace_roots_absent + custody.state_roots_absent,
                    custody.scratch_rows,
                    custody.scratch_roots_removed,
                    if custody.performed {
                        "removed"
                    } else {
                        "to remove"
                    },
                    custody.scratch_roots_absent,
                ),
            },
        ),
        crate::tty::Row::key_value("reservation released", report.released_sha256.clone()),
        crate::tty::Row::key_value(
            "interrupted attempts settled",
            if report.recovered.is_empty() {
                "none".to_owned()
            } else {
                report
                    .recovered
                    .iter()
                    .map(|recovered| {
                        let applied = match recovered.applied {
                            Some(ryeos_app::execution_history_reset::RecoveredApplied::Applied) => ", applied",
                            Some(ryeos_app::execution_history_reset::RecoveredApplied::Resumed) => ", resumed",
                            Some(ryeos_app::execution_history_reset::RecoveredApplied::NotApplied) | None => "",
                        };
                        let outcome = match recovered.outcome {
                            ryeos_app::execution_history_reset::RecoveredReleaseOutcome::Completed => "completed",
                            ryeos_app::execution_history_reset::RecoveredReleaseOutcome::Abandoned => "abandoned",
                        };
                        format!("{} ({outcome}{applied})", recovered.operation_id)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
    ];
    if report.performed {
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
            system_space_dir: args.app_root.map(|p| p.to_string_lossy().into_owned()),
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

    /// TCP endpoint persisted in this node's bootstrap configuration.
    #[arg(long)]
    bind: Option<SocketAddr>,

    /// Local lifecycle endpoint persisted with the TCP endpoint.
    #[arg(long)]
    uds_path: Option<PathBuf>,

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

    /// Host class naming the kind of host this node runs on (for example
    /// `local` for a host the operator runs, `hosted` for a hosting
    /// provider's container). Required with --node-profile; it must be a
    /// class the profile declares, and it selects that profile's floor for
    /// the class. It is fixed at initialization.
    #[arg(long, requires = "node_profile")]
    host_class: Option<String>,

    /// Immutable OCI digest of the substrate image executing this node.
    #[arg(long, requires = "substrate_protocol")]
    substrate_image_digest: Option<String>,

    /// Nonzero bundle compatibility protocol implemented by that substrate.
    #[arg(long, requires = "substrate_image_digest")]
    substrate_protocol: Option<u32>,

    /// Replace an existing complete policy generation from --node-profile in
    /// this same stopped-node init transaction.
    #[arg(
        long,
        requires_all = ["node_profile", "confirm_node_policy_generation_replacement"]
    )]
    replace_node_policy_generation: bool,

    /// Keep this section's installed policy during the replacement
    /// (repeatable). Recorded operator sections carry forward on their own.
    #[arg(
        long = "keep-section",
        action = clap::ArgAction::Append,
        requires = "replace_node_policy_generation"
    )]
    keep_sections: Vec<String>,

    /// Take this section from the new profile even if an operator applied it
    /// (repeatable).
    #[arg(
        long = "reset-section",
        action = clap::ArgAction::Append,
        requires = "replace_node_policy_generation"
    )]
    reset_sections: Vec<String>,

    /// Required acknowledgement for --replace-node-policy-generation.
    #[arg(long, requires = "replace_node_policy_generation")]
    confirm_node_policy_generation_replacement: bool,

    /// Move the recorded substrate identity to the image named by
    /// --substrate-image-digest during that replacement. Only the image
    /// digest may differ from the record.
    #[arg(
        long,
        requires_all = [
            "replace_node_policy_generation",
            "substrate_image_digest",
            "confirm_substrate_identity_replacement"
        ]
    )]
    replace_substrate_identity: bool,

    /// Required acknowledgement for --replace-substrate-identity.
    #[arg(long, requires = "replace_substrate_identity")]
    confirm_substrate_identity_replacement: bool,

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

    let substrate_identity = match (args.substrate_image_digest, args.substrate_protocol) {
        (Some(digest), Some(protocol)) => Some(ryeos_node::SubstrateIdentity::new(
            digest,
            protocol,
            args.node_profile.clone(),
            args.host_class.clone(),
        )?),
        (None, None) => None,
        _ => unreachable!("clap enforces paired substrate identity arguments"),
    };
    let opts = ryeos_node::InitOptions {
        app_root,
        bind: args.bind,
        uds_path: args.uds_path,
        source_dir: args.source,
        trust_files: args.trust_files,
        node_profile: args.node_profile,
        host_class: args.host_class,
        substrate_identity,
        replace_node_policy_generation: args.replace_node_policy_generation,
        policy_carry: policy_carry_choices(&args.keep_sections, &args.reset_sections),
        replace_substrate_identity: args.replace_substrate_identity,
        confirm_substrate_identity_replacement: args.confirm_substrate_identity_replacement,
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
            crate::tty::Row::key_value("authoring", report.authoring_key_fingerprint),
            crate::tty::Row::key_value("vault", report.vault_pubkey_fingerprint),
            crate::tty::Row::key_value("bundles", report.bundles_installed.join(", ")),
        ];
        if let Some(replacement) = &report.substrate_image_replacement {
            status.rows.push(crate::tty::Row::key_value(
                "substrate image",
                format!(
                    "{} -> {}",
                    replacement.previous_image_digest, replacement.new_image_digest
                ),
            ));
        }
        if let Some(amendment) = &report.capacity_amendment {
            status.rows.push(crate::tty::Row::key_value(
                "capacity issuance",
                if amendment.already_applied {
                    format!("policy {} already in force", amendment.policy_digest)
                } else {
                    format!("amended to policy {}", amendment.policy_digest)
                },
            ));
        }
        console.success(&status)?;
    }
    Ok(())
}

// ── ryeos setup ────────────────────────────────────────────────────

#[cfg(feature = "model-provider-setup")]
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

#[cfg(feature = "model-provider-setup")]
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
    let app_root = LocalLifecycleEnv::selected_app_root(args.app_root)?;
    let status = match LocalLifecycleEnv::load(Some(app_root.clone())) {
        Ok(env) => LifecycleController::from_env(env)
            .status()
            .await
            .context("ryeos node status failed")?,
        Err(config_error) => {
            // Bootstrap configuration cannot provide a trustworthy endpoint,
            // but a configured supervisor has independent root-owned failure
            // testimony. Read it only through the exact protected association;
            // never invent a direct-node endpoint or launch fallback here.
            let supervised_failure =
                match ryeos_node::supervision::InstalledService::discover_app_root(&app_root)? {
                    Some(service) => {
                        service.check_supervisor()?;
                        if service.desired_state()? == ryeos_node::supervision::DesiredState::Up {
                            service.launch_failure_status_without_config(&app_root)?
                        } else {
                            None
                        }
                    }
                    None => None,
                };
            supervised_failure
                .or_else(|| ryeos_node::status::retained_startup_failure(&app_root))
                .with_context(|| {
                    format!(
                        "node bootstrap configuration is invalid and no retained startup failure is available: {config_error:#}"
                    )
                })?
        }
    };
    if args.json {
        crate::tty::write_json(&status)?;
    } else {
        render_lifecycle_status(console, &status)?;
    }
    Ok(())
}

// ── ryeos node offline-operation facts ──────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node offline-operation facts",
    about = "Print the node facts a signed offline-operation request is signed against",
    long_about = "Read-only. Prints every precondition fact of the node and the ledger head, and the operations this build can apply with the facts each pins. Works with the daemon stopped, so an operator can sign a request for a node that cannot start. The daemon prints the same facts in its log on every start. With --remote <name> it prints a remote's facts instead, read through this operator's running node under the delegation stored for that remote; `--json` then writes the document `sign --facts` reads.",
    no_binary_name = true
)]
struct NodeOfflineFactsArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Read the facts of the remote of this name through this operator's own
    /// node. The stored delegation for it must lend
    /// `ryeos.execute.service.offline-operations/facts`.
    #[arg(long)]
    remote: Option<String>,

    /// The key that will sign requests (PKCS#8 PEM): the facts name the
    /// authority it holds on the node, and with --remote it also
    /// authenticates the call to this operator's node. Defaults to the
    /// operator key under the app root.
    #[arg(long)]
    signing_key: Option<PathBuf>,

    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

async fn run_node_offline_operation_facts_command(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeOfflineFactsArgs>(argv, console)? else {
        return Ok(());
    };
    if let Some(remote) = &args.remote {
        return crate::lifecycle_offline_operation::run_remote_facts(
            remote,
            args.app_root,
            args.signing_key,
            args.json,
            console,
        )
        .await;
    }
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root)?.app_root;
    let facts = ryeos_node::offline_operation::read_facts(&app_root)
        .context("measure the node's offline-operation facts")?;
    // The authority the signing key holds here, so `sign --facts` signs under
    // it. Without a key to name, the facts name no signer and `sign` refuses
    // them rather than guess.
    let key_path = args
        .signing_key
        .clone()
        .unwrap_or_else(|| crate::transport::signing::operator_signing_key_path(&app_root));
    let signer = match lillux::crypto::load_signing_key(&key_path) {
        Ok(key) => Some(ryeos_node::offline_operation::deliver::local_signer_report(
            &app_root,
            &lillux::signature::compute_fingerprint(&key.verifying_key()),
        )?),
        Err(error) if args.signing_key.is_some() => {
            return Err(error)
                .with_context(|| format!("load the signing key {}", key_path.display()));
        }
        Err(_) => None,
    };
    let operations = ryeos_node::offline_operation::table::operations();
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "app_root": app_root,
            "node_fingerprint": ryeos_node::offline_operation::read_node_fingerprint(&app_root)?,
            "facts": facts,
            "signer": signer,
            "operations": operations.iter().map(|spec| serde_json::json!({
                "operation": spec.id,
                "scope": spec.scope(),
                "authority_bearing": spec.authority_bearing,
                "local_signer_only": spec.local_signer_only,
                "pins": ryeos_node::offline_operation::facts::MANDATORY_FACTS
                    .iter()
                    .chain(spec.preconditions)
                    .collect::<Vec<_>>(),
                "inputs": spec.inputs.iter().map(|slot| serde_json::json!({
                    "slot": slot.name,
                    "max_bytes": slot.max_bytes,
                    "required": slot.required,
                })).collect::<Vec<_>>(),
                "expected_result": spec.expected_result,
            })).collect::<Vec<_>>(),
        }))?;
        return Ok(());
    }
    let mut status =
        crate::tty::StatusBanner::new(crate::tty::Tone::Neutral, "OFFLINE-OPERATION FACTS");
    status.detail = Some(app_root.display().to_string());
    status.rows = facts
        .iter()
        .map(|(fact, value)| crate::tty::Row::key_value(fact.as_str(), value.clone()))
        .collect();
    console.status(&status)?;
    Ok(())
}

// ── ryeos node allow-restart ────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node allow-restart",
    about = "Clear the restart inhibition a node records when it stops itself to avoid a \
             restart loop. Resolve the cause first: `ryeos node doctor` shows it.",
    no_binary_name = true
)]
struct NodeAllowRestartArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

fn run_node_allow_restart_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeAllowRestartArgs>(argv, console)? else {
        return Ok(());
    };
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
    let app_root = controller.config().app_root.clone();
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.allow-restart",
        serde_json::json!({}),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    )?;
    let cleared = &applied.result["cleared"];
    if args.json {
        crate::tty::write_json(&serde_json::json!({ "app_root": app_root, "cleared": cleared }))?;
        return Ok(());
    }
    let status = if cleared.is_null() {
        let mut banner =
            crate::tty::StatusBanner::new(crate::tty::Tone::Neutral, "NO RESTART INHIBITION");
        banner.detail = Some(format!("{} has nothing to clear", app_root.display()));
        banner
    } else {
        let mut banner =
            crate::tty::StatusBanner::new(crate::tty::Tone::Success, "RESTART ALLOWED");
        banner.detail = Some(format!(
            "cleared `{}` recorded at {}; the next start may proceed",
            crate::offline_apply::text(cleared, "reason").unwrap_or("unknown"),
            crate::offline_apply::text(cleared, "recorded_at").unwrap_or("unknown")
        ));
        banner
    };
    console.status(&status)?;
    Ok(())
}

// ── ryeos node capacity reissue ─────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node capacity reissue",
    about = "Open the next native-capacity epoch on a stopped node. The signed policy must \
             pre-authorise operator re-issuance; the operator key signs a document naming \
             the exact account state it renews. Restart inhibition is not cleared.",
    no_binary_name = true
)]
struct NodeCapacityReissueArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Why the operator opens another epoch; recorded in the signed document
    /// and the evidence log.
    #[arg(long)]
    reason: String,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

fn run_node_capacity_reissue_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeCapacityReissueArgs>(argv, console)? else {
        return Ok(());
    };
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
    let app_root = controller.config().app_root.clone();
    // The signed request carries the reason; the library signs it with the
    // operator key on this node's volume and applies it under the state lock a
    // running daemon would hold.
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.capacity.reissue",
        serde_json::to_value(ryeos_node::offline_operation::handlers::CapacityArguments {
            reason: args.reason.clone(),
        })?,
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    )?;
    let report: ryeos_node::native_capacity::ReissueReport =
        serde_json::from_value(applied.result).context("read the re-issuance report")?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({ "app_root": app_root, "reissue": report }))?;
        return Ok(());
    }
    console.status(&capacity_reissue_banner(&app_root, &report))?;
    Ok(())
}

fn capacity_reissue_banner(
    app_root: &std::path::Path,
    report: &ryeos_node::native_capacity::ReissueReport,
) -> crate::tty::StatusBanner {
    let mut banner = if report.already_applied {
        let mut banner = crate::tty::StatusBanner::new(
            crate::tty::Tone::Neutral,
            format!(
                "ALREADY RE-ISSUED: epoch {} → {}",
                report.from_epoch, report.to_epoch
            ),
        );
        banner.detail = Some(
            "the account already carries this re-issuance; nothing new was written".to_owned(),
        );
        banner
    } else {
        crate::tty::StatusBanner::new(
            crate::tty::Tone::Success,
            format!(
                "CAPACITY RE-ISSUED: epoch {} → {}",
                report.from_epoch, report.to_epoch
            ),
        )
    };
    banner.rows.push(crate::tty::Row::key_value(
        "app root",
        app_root.display().to_string(),
    ));
    banner.rows.push(crate::tty::Row::key_value(
        "document",
        report.document_digest.clone(),
    ));
    if let Some(settled) = &report.settled_incarnation {
        banner.rows.push(crate::tty::Row::key_value(
            "settled incarnation",
            settled.clone(),
        ));
    }
    banner.rows.push(crate::tty::Row::text(
        "restart inhibition is not cleared; run `ryeos node allow-restart` when the cause is resolved",
    ));
    banner
}

// ── ryeos node capacity reanchor ────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node capacity reanchor",
    about = "Anchor a stopped node's native-capacity account at the filesystem its data \
             moved to. The operator key signs a document naming the exact prior binding \
             and the newly measured root and runtime identities; boot accepts the account \
             at the new place only through it. Refused while an incarnation debit is \
             outstanding.",
    no_binary_name = true
)]
struct NodeCapacityReanchorArgs {
    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Why the operator moves the account; recorded in the signed document
    /// and the evidence log.
    #[arg(long)]
    reason: String,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

fn run_node_capacity_reanchor_command(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_or_render_help::<NodeCapacityReanchorArgs>(argv, console)? else {
        return Ok(());
    };
    let controller = LifecycleController::from_env(local_env(args.app_root)?);
    let app_root = controller.config().app_root.clone();
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.capacity.reanchor",
        serde_json::to_value(ryeos_node::offline_operation::handlers::CapacityArguments {
            reason: args.reason.clone(),
        })?,
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    )?;
    let report: ryeos_node::native_capacity::ReanchorReport =
        serde_json::from_value(applied.result).context("read the re-anchor report")?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({ "app_root": app_root, "reanchor": report }))?;
        return Ok(());
    }
    console.status(&capacity_reanchor_banner(&app_root, &report))?;
    Ok(())
}

fn capacity_reanchor_banner(
    app_root: &std::path::Path,
    report: &ryeos_node::native_capacity::ReanchorReport,
) -> crate::tty::StatusBanner {
    let mut banner = if report.already_applied {
        let mut banner = crate::tty::StatusBanner::new(
            crate::tty::Tone::Neutral,
            "ALREADY ANCHORED: the capacity account is recorded at this place".to_owned(),
        );
        banner.detail = Some("nothing was written".to_owned());
        banner
    } else {
        crate::tty::StatusBanner::new(
            crate::tty::Tone::Success,
            "CAPACITY RE-ANCHORED: the account is now authorised at this place".to_owned(),
        )
    };
    banner.rows.push(crate::tty::Row::key_value(
        "app root",
        app_root.display().to_string(),
    ));
    if let Some(document) = &report.document_digest {
        banner
            .rows
            .push(crate::tty::Row::key_value("document", document.clone()));
    }
    if let Some(prior) = &report.prior_binding_digest {
        banner.rows.push(crate::tty::Row::key_value(
            "replaces binding",
            prior.clone(),
        ));
    }
    banner
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
    let mut live_service_window: Option<ryeos_node::ServiceWindow> = None;
    match controller.status().await {
        Ok(LifecycleStatus::Running {
            metadata,
            service_window,
            ..
        }) => {
            daemon_running = true;
            live_service_window = service_window;
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

    // What the node contains and what its host proves, as recorded and
    // signed by the daemon at its last start, beside a fresh view of the host
    // from this shell. The shell's own limits (for example locked memory)
    // can differ from the daemon's, so only the recorded report decides.
    // A node that stopped itself to avoid a restart loop refuses every start
    // until an operator clears this record.
    match ryeos_node::lifecycle_marker::read_restart_inhibition(
        &ryeos_engine::roots::RuntimeRoot::new(config.app_root.clone()).state(),
    ) {
        Ok(None) => checks.push(check("restart_inhibit", OK, serde_json::json!({}))),
        Ok(Some(record)) => checks.push(check(
            "restart_inhibit",
            FAIL,
            serde_json::json!({
                "record": record,
                "message": record.refusal(),
                "fix": "resolve the cause, then run: ryeos node allow-restart",
            }),
        )),
        Err(error) => checks.push(check(
            "restart_inhibit",
            FAIL,
            serde_json::json!({
                "error": format!("{error:#}"),
                "fix": "the record is unreadable; ryeos node allow-restart removes it",
            }),
        )),
    }

    let capability_report = if initialized {
        let (status, detail, report) = inspect_capability_report(&config.app_root);
        checks.push(check("capabilities", status, detail));
        report
    } else {
        checks.push(check(
            "capabilities",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
        None
    };

    // The native-capacity account as its anchor on disk reports it. Reading is
    // not recovery: a torn or foreign record is reported, never repaired here.
    if initialized {
        checks.push(native_capacity_check(&config.app_root));
        checks.push(service_window_check(
            daemon_running,
            live_service_window.as_ref(),
        ));
        checks.push(durable_identity_check(&config.app_root));
    } else {
        checks.push(check(
            "native_capacity",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    // The complete signed policy generation, compiled as the daemon compiles
    // it. A section this release newly requires is named with its cure.
    if initialized {
        checks.push(node_policy_check(&config.app_root));
    } else {
        checks.push(check(
            "node_policy",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    // Use the same policy loader as daemon and offline execution. A disabled
    // lane warns: runs are unconfined. Enforced policy failures remain
    // fail-closed.
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
        checks.push(host_access_check(&config.app_root));
    } else {
        checks.push(check(
            "host_access",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    // Operator key material left at an old location: init refuses it and
    // nothing reads it, so the node cannot be initialized until it is moved.
    match ryeos_node::operator_layout::check_operator_layout(&config.app_root) {
        Ok(()) => checks.push(check("operator_key", OK, serde_json::json!({}))),
        Err(error) => checks.push(check(
            "operator_key",
            FAIL,
            serde_json::json!({
                "code": error.code(),
                "error": error.to_string(),
                "fix": error.cure(),
            }),
        )),
    }

    // Remote pins: node-signed records under `.ai/node/remotes`. An unsigned
    // or edited record, or a leftover unsigned config file, is a failure with
    // the cure named.
    if initialized {
        checks.push(remote_pins_check(&config.app_root));
    } else {
        checks.push(check(
            "remote_pins",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    // The wall clock the node issues external execution windows from. Always
    // reported; a node policy that requires a synchronized clock turns an
    // unsatisfied reading into a failure.
    checks.push(wall_clock_check(&config.app_root, initialized));

    // The browser-session policy: lifetimes and the capability ceiling every
    // browser session is bounded by. Live sessions exist only in a running
    // node, so their count is not available here.
    checks.push(ui_browser_sessions_check(&config.app_root, initialized));

    // Grants that have lapsed, are about to, or are of a retired schema or
    // class. Reading them needs only the node key and the signed policy.
    if initialized {
        checks.push(authorized_keys_check(&config.app_root));
        checks.push(key_age_check(
            &config.app_root,
            lillux::time::unix_secs_now(),
        ));
    } else {
        checks.push(check(
            "authorized_keys",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
        checks.push(check(
            "key_age",
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

    // Execution-history reset scratch: an interrupted reset leaves it. The
    // next reset settles it while the node is stopped; the warning names the
    // explicit repair for what that reset cannot attribute.
    if initialized {
        let repair = ryeos_app::execution_history_reset::execution_history_scratch_repair_command();
        match ryeos_app::execution_history_reset::inspect_execution_history_scratch(&config.app_root) {
            Ok(scratch) if scratch.is_empty() => {
                checks.push(check("reset_scratch", OK, serde_json::json!({})))
            }
            Ok(scratch) => checks.push(check(
                "reset_scratch",
                WARN,
                serde_json::json!({
                    "ledger": scratch.ledger,
                    "inspections": scratch.inspections,
                    "runtime": scratch.runtime,
                    "note": "an interrupted execution-history reset left scratch (or a reset is running now)",
                    "fix": format!(
                        "stop the node. To only inspect, run `ryeos node reset execution-history-scratch --dry-run`. To settle, run `ryeos node reset execution-history`: any reset, --dry-run included, settles interrupted attempts. If it refuses, run `{repair}`, or remove by hand the foreign name the refusal names"
                    ),
                }),
            )),
            Err(error) => checks.push(check(
                "reset_scratch",
                WARN,
                serde_json::json!({
                    "error": format!("{error:#}"),
                    "fix": format!("if the next execution-history reset refuses, run `{repair}`"),
                }),
            )),
        }
    } else {
        checks.push(check(
            "reset_scratch",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    // Schedules pinned to a grant generation that no longer holds refuse
    // their next fire and pause; report them before that happens.
    #[cfg(feature = "scheduling")]
    checks.push(schedule_authority_check(&config.app_root, initialized));

    // Process quarantine: recovery holds an attached identity it cannot prove
    // gone. The daemon mirrors its runtime rows into a report after each
    // recovery pass; the rows stay the authority.
    if initialized {
        checks.push(process_quarantine_check(&config.app_root));
        checks.push(stale_publication_check(&config.app_root));
    } else {
        checks.push(check(
            "stale_publication",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
        checks.push(check(
            "process_quarantine",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        ));
    }

    // Host-wide: native services of nodes whose app root is gone. Read-only
    // enumeration of administrator-owned records; nothing is retired here.
    checks.push(host_services_check(ryeos_node::supervision::host_services()));

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
        // The callback-only socket sandboxed tools are handed; a node cannot
        // start unless this one binds too.
        let runtime_uds_path = config.runtime_uds_path();
        if !lillux::local_ipc::local_endpoint_path_addressable(&runtime_uds_path) {
            status = FAIL;
            detail.insert("uds_runtime".into(), serde_json::json!({
                "path": runtime_uds_path,
                "error": "path exceeds the local socket length limit",
                "note": "configure a shorter uds_path; the callback socket is the control socket's name plus `-runtime`",
            }));
        } else {
            match std::os::unix::net::UnixListener::bind(&runtime_uds_path) {
                Ok(l) => {
                    drop(l);
                    let _ = std::fs::remove_file(&runtime_uds_path);
                    detail.insert(
                        "uds_runtime".into(),
                        serde_json::json!({ "path": runtime_uds_path, "status": "bindable" }),
                    );
                }
                Err(e) => {
                    status = FAIL;
                    detail.insert("uds_runtime".into(), serde_json::json!({
                        "path": runtime_uds_path,
                        "error": format!("{e}"),
                        "note": "with no daemon running this is usually a stale socket file — remove it",
                    }));
                }
            }
        }
        checks.push(check("sockets", status, serde_json::Value::Object(detail)));
    }

    // Held external capacity and credential-profile locks, as the running
    // daemon sees them.
    if daemon_running {
        checks.extend(crate::doctor_external_status::checks(&config.app_root).await);
    } else {
        checks.extend(crate::doctor_external_status::not_checked(
            "these rows ask the running daemon, and the node is not running",
            initialized.then_some("start the node: ryeos start"),
        ));
    }

    // 5. Verified node config + per-bundle doctor. Requires init; degrades to
    //    n/a rather than piling failures onto an uninitialized node.
    if initialized {
        let isolation =
            ryeos_node_runtime::engine_init::load_locked_registered_isolation(&config.app_root)
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
                checks.push(external_bindings_check(
                    &config.app_root,
                    isolation
                        .as_ref()
                        .ok()
                        .and_then(|runtime| runtime.registered_generation_node_trust()),
                    &snapshot,
                ));
                checks.push(remote_content_testimonies_check(&config.app_root));
                if !args.no_bundles {
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
                            &config.app_root,
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
                let mut detail = serde_json::json!({
                    "error": format!("{e:#}"),
                    "note": "installed bundle registrations failed verification",
                });
                // A binding in a schema this node no longer admits is named
                // with the step that clears it.
                if let Some(refusal) = e.chain().find_map(|cause| {
                    cause.downcast_ref::<ryeos_app::node_config::coherence::BindingSchemaUnsupported>()
                }) {
                    detail["code"] = serde_json::json!(refusal.code());
                    detail["note"] = serde_json::json!(refusal.to_string());
                    detail["fix"] = serde_json::json!(refusal.fix());
                }
                checks.push(check("node_config", FAIL, detail));
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
            if c.name == "capabilities" {
                continue;
            }
            if c.status != OK || c.name == "isolation" {
                section.rows.push(
                    crate::tty::Row::text(format!("{}: {}", c.name, c.detail))
                        .with_tone(crate::tty::Tone::Secondary),
                );
            }
            if c.name == "remote_content_testimonies" {
                for finding in c.detail["findings"].as_array().into_iter().flatten() {
                    if finding["code"] == "ok" {
                        continue;
                    }
                    section.rows.push(
                        crate::tty::Row::text(format!(
                            "{} {} {}: {}",
                            finding["code"].as_str().unwrap_or_default(),
                            finding["lane"].as_str().unwrap_or_default(),
                            finding["coordinate_id"].as_str().unwrap_or_default(),
                            finding["fix"].as_str().unwrap_or_default(),
                        ))
                        .with_tone(crate::tty::Tone::Secondary),
                    );
                }
            }
            if c.name == "external_bindings" {
                for finding in c.detail["findings"].as_array().into_iter().flatten() {
                    section.rows.push(
                        crate::tty::Row::text(format!(
                            "{} {}/{} {}: {}",
                            finding["code"].as_str().unwrap_or_default(),
                            finding["binding"]["section"].as_str().unwrap_or_default(),
                            finding["binding"]["id"].as_str().unwrap_or_default(),
                            finding["field"].as_str().unwrap_or_default(),
                            finding["fix"].as_str().unwrap_or_default(),
                        ))
                        .with_tone(crate::tty::Tone::Secondary),
                    );
                }
            }
        }
        let mut document =
            crate::tty::Document::titled(format!("NODE DOCTOR — {}", config.app_root.display()));
        document.sections.push(section);
        document.sections.push(capability_section(
            capability_report.as_ref(),
            checks.iter().find(|c| c.name == "capabilities"),
        ));
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

/// Doctor row for pin coherence: every pin of every installed external
/// execution and runtime snapshot binding compared with the installed signed
/// bundles and the node vault, read only. FAIL names each pin the next fresh
/// operation through that binding would refuse; WARN names pins it could not
/// decide (an unreadable vault).
fn external_bindings_check(
    app_root: &std::path::Path,
    trust: Option<&ryeos_engine::trust::TrustStore>,
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, NA, OK, WARN};
    if snapshot.external_execution.is_empty()
        && snapshot.runtime_snapshot_production.is_empty()
        && snapshot.runtime_snapshot_qualification.is_empty()
    {
        return check(
            "external_bindings",
            NA,
            serde_json::json!({
                "note": "no external execution or runtime snapshot binding is installed",
            }),
        );
    }
    let Some(trust) = trust else {
        return check(
            "external_bindings",
            FAIL,
            serde_json::json!({ "error": "the locked isolation runtime retains no node trust" }),
        );
    };
    match ryeos_app::node_config::coherence::inspect_installed_binding_coherence(
        app_root, trust, snapshot,
    ) {
        Ok(report) => {
            let status = if report.has_failures() {
                FAIL
            } else if report.has_warnings() {
                WARN
            } else {
                OK
            };
            check(
                "external_bindings",
                status,
                serde_json::json!({
                    "bindings": report.bindings,
                    "findings": report.findings,
                    "not_checked": report.not_checked,
                }),
            )
        }
        Err(error) => check(
            "external_bindings",
            FAIL,
            serde_json::json!({
                "error": format!("{error:#}"),
                "note": "the installed bundles' artifacts could not be inventoried",
            }),
        ),
    }
}

/// Doctor row for accepted remote content testimonies, read offline from
/// this node's signed heads: for each content-lane acceptance, whether this
/// node holds an activation of exactly the content the origin qualified. WARN
/// names each testimony a launch or candidate-program emission would refuse
/// (`no_local_activation`, `local_activation_differs`) and every acceptance of
/// either lane recorded under a superseded schema (`acceptance_superseded`),
/// each with its fix. Advisory only: it never fails the doctor.
fn remote_content_testimonies_check(
    app_root: &std::path::Path,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{NA, OK, WARN};
    match ryeos_app::operator_external_content::remote_testimony_inspection::inspect_remote_content_testimonies(app_root) {
        Ok(report) if report.findings.is_empty() => check(
            "remote_content_testimonies",
            NA,
            serde_json::json!({ "note": "no remote content testimony is accepted on this node" }),
        ),
        Ok(report) => check(
            "remote_content_testimonies",
            if report.has_warnings() { WARN } else { OK },
            serde_json::json!({ "findings": report.findings }),
        ),
        Err(error) => check(
            "remote_content_testimonies",
            WARN,
            serde_json::json!({
                "error": format!("{error:#}"),
                "note": "accepted remote testimonies could not be inspected",
            }),
        ),
    }
}

/// Doctor row for the node-signed authorized-key grants: which have lapsed,
/// which lapse within the policy's warning window, and which are of a retired
/// schema or class or cannot be read. Stale and unreadable grants fail the row
/// because they keep the daemon from starting; lapsed and expiring grants warn.
fn authorized_keys_check(
    app_root: &std::path::Path,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, OK, WARN};
    let inspect = || -> Result<_> {
        let root = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf());
        let node = ryeos_node_runtime::identity::NodeIdentity::load(&root.node_signing_key_path())?;
        let warn_before_secs =
            ryeos_core_tools::actions::hosted_policy::load_authorization_policy(app_root)?
                .warn_before_secs;
        let inspections = ryeos_node_runtime::identity::inspect_authorized_keys(
            &root.authorized_keys_dir(),
            &node,
        )?;
        Ok((inspections, warn_before_secs))
    };
    match inspect() {
        Ok((inspections, warn_before_secs)) => {
            let report = grant_report(
                &inspections,
                warn_before_secs,
                lillux::time::unix_secs_now(),
            );
            let status = if !report.stale.is_empty() || !report.invalid.is_empty() {
                FAIL
            } else if !report.expired.is_empty() || !report.expiring.is_empty() {
                WARN
            } else {
                OK
            };
            check("authorized_keys", status, report.into_detail())
        }
        Err(error) => check(
            "authorized_keys",
            FAIL,
            serde_json::json!({ "error": format!("{error:#}") }),
        ),
    }
}

/// Doctor row for key age: the node, operator, authoring and publisher keys,
/// each with the time it entered service and its age in days. Age is a
/// report, never a failure. The row warns when the node has no active
/// operator or authoring key, because `ryeos init` creates both.
fn key_age_check(
    app_root: &std::path::Path,
    now_unix: u64,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, OK, WARN};
    match ryeos_node::key_rotation::key_ages(app_root, now_unix) {
        Ok(keys) => {
            let has_active = |role: &str| {
                keys.iter()
                    .any(|key| key.role == role && key.status == "active")
            };
            let mut missing = Vec::new();
            for role in ["operator", "authoring"] {
                if !has_active(role) {
                    missing.push(serde_json::json!({
                        "role": role,
                        "fix": "run `ryeos init` on the stopped node to create and pin the key",
                    }));
                }
            }
            let status = if missing.is_empty() { OK } else { WARN };
            check(
                "key_age",
                status,
                serde_json::json!({
                    "keys": keys,
                    "missing": missing,
                    "note": "age is reported, not judged: no signed policy sets a rotation threshold",
                    "rotate": {
                        "operator": "ryeos node operator rotate",
                        "authoring": "ryeos node authoring rotate",
                        "node": "not rotatable: the node key is the root of every node document",
                    },
                }),
            )
        }
        Err(error) => check(
            "key_age",
            FAIL,
            serde_json::json!({ "error": format!("{error:#}") }),
        ),
    }
}

/// The doctor's reading of every grant file at one instant.
#[derive(Debug, Default)]
struct GrantReport {
    grants: usize,
    expired: Vec<serde_json::Value>,
    expiring: Vec<serde_json::Value>,
    stale: Vec<serde_json::Value>,
    invalid: Vec<serde_json::Value>,
}

impl GrantReport {
    fn into_detail(self) -> serde_json::Value {
        serde_json::json!({
            "grants": self.grants,
            "expired": self.expired,
            "expiring": self.expiring,
            "stale": self.stale,
            "invalid": self.invalid,
        })
    }
}

fn grant_report(
    inspections: &[ryeos_node_runtime::identity::GrantFileInspection],
    warn_before_secs: u64,
    now: u64,
) -> GrantReport {
    use ryeos_node_runtime::identity::{GrantExpiry, GrantFileState};
    let mut report = GrantReport {
        grants: inspections.len(),
        ..GrantReport::default()
    };
    for inspection in inspections {
        match &inspection.state {
            GrantFileState::Current(grant) => {
                let GrantExpiry::At(expires) = grant.expires_at else {
                    continue;
                };
                let row = |fix: String| {
                    serde_json::json!({
                        "fingerprint": grant.fingerprint,
                        "class": grant.principal_class.as_str(),
                        "label": grant.owner,
                        "granted_by": grant.granted_by,
                        "expires_at": grant.expires_at.to_grant_text(),
                        "fix": fix,
                    })
                };
                let fix = format!(
                    "the principal that granted it or the node operator renews it: `ryeos renew-key --fingerprint {}`",
                    grant.fingerprint
                );
                if grant.expires_at.is_expired_at(now) {
                    report.expired.push(row(fix));
                } else if expires.saturating_sub(now) <= warn_before_secs {
                    report.expiring.push(row(fix));
                }
            }
            GrantFileState::Stale(error) => report.stale.push(serde_json::json!({
                "file": inspection.name,
                "error_code": error.code(),
                "fix": error.to_string(),
            })),
            GrantFileState::Invalid(error) => report.invalid.push(serde_json::json!({
                "file": inspection.name,
                "error": error,
            })),
        }
    }
    report
}

/// Doctor row for the app root's durable identity: whether persisted
/// identities survive the volume being re-attached under another device
/// number. Report only; a device-number identity warns, never fails.
fn durable_identity_check(
    app_root: &std::path::Path,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use lillux::IdentityStability::{AttachmentBound, ReattachStable};
    use ryeos_core_tools::actions::doctor::{FAIL, OK, WARN};
    match ryeos_node::capability_report::DurableIdentityFacts::observe(app_root) {
        Ok(facts) => {
            let (status, note) = match facts.app_root {
                ReattachStable => (
                    OK,
                    "the app root's durable identity is its filesystem's UUID-derived id: it survives the volume being re-attached under another device",
                ),
                AttachmentBound => (
                    WARN,
                    "the app root's filesystem type offers no UUID-derived id, so its durable identity is the device number and holds only while this attachment lasts; a re-attached volume needs `ryeos node capacity reanchor`",
                ),
            };
            check(
                "durable_identity",
                status,
                serde_json::json!({ "app_root": facts.app_root, "note": note }),
            )
        }
        Err(error) => check(
            "durable_identity",
            FAIL,
            serde_json::json!({ "error": format!("{error:#}") }),
        ),
    }
}

/// The node's wall-clock trust against the clock requirement its signed
/// execution policy declares, if any. Reporting needs no policy: without one
/// (or without an initialized node) the row is informational.
fn wall_clock_check(
    app_root: &std::path::Path,
    initialized: bool,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, OK, WARN};
    let trust = lillux::time::wall_clock_trust();
    let requirement = if initialized {
        (|| -> Result<_> {
            let isolation =
                ryeos_node_runtime::engine_init::load_locked_registered_definition_isolation(
                    app_root,
                )?;
            let node_trust = isolation
                .registered_generation_node_trust()
                .context("retained isolation generation omitted node trust")?;
            let policy = crate::node_descriptors::load_verified_policy_snapshot_with_trust(
                app_root, node_trust,
            )?;
            Ok(policy
                .require::<ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy>()?
                .external_wall_clock_requirement)
        })()
    } else {
        Ok(None)
    };
    let synced = matches!(trust, lillux::time::WallClockTrust::Synced { .. });
    match requirement {
        Ok(Some(requirement)) => match requirement.require(&trust) {
            Ok(()) => check(
                "wall_clock",
                OK,
                serde_json::json!({ "trust": trust, "requirement": requirement }),
            ),
            Err(error) => check(
                "wall_clock",
                FAIL,
                serde_json::json!({
                    "trust": trust,
                    "requirement": requirement,
                    "message": format!("{error:#}"),
                    "fix": "synchronize this host's clock, or change `external_wall_clock_requirement` in the signed execution policy",
                }),
            ),
        },
        Ok(None) if synced => check(
            "wall_clock",
            OK,
            serde_json::json!({ "trust": trust, "requirement": "none (report only)" }),
        ),
        Ok(None) => check(
            "wall_clock",
            WARN,
            serde_json::json!({
                "trust": trust,
                "requirement": "none (report only)",
                "note": "external execution windows are issued from this clock; guests tolerate a declared skew",
            }),
        ),
        Err(error) => check(
            "wall_clock",
            WARN,
            serde_json::json!({
                "trust": trust,
                "note": format!("the clock requirement could not be read: {error:#}"),
            }),
        ),
    }
}

/// Doctor row for the native-capacity account: healthy with its summary,
/// a warning with the cure when the allowance is spent and the signed policy
/// lets the operator re-issue, a failure when it does not.
/// Schedules whose pinned authorized-key grant generation is missing or
/// superseded. Each refuses its next fire and pauses itself; the owner
/// re-pins it with `ryeos scheduler reauthorize <id>`.
#[cfg(feature = "scheduling")]
fn schedule_authority_check(
    app_root: &std::path::Path,
    initialized: bool,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{NA, OK, WARN};
    if !initialized {
        return check(
            "schedule_authority",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        );
    }
    match inspect_schedule_grant_generations(app_root) {
        Ok(superseded) if superseded.is_empty() => {
            check("schedule_authority", OK, serde_json::json!({}))
        }
        Ok(superseded) => check(
            "schedule_authority",
            WARN,
            serde_json::json!({
                "superseded": superseded,
                "note": "these schedules pin a grant generation that no longer holds; each refuses its next fire and pauses itself",
                "fix": "as each schedule's owner: ryeos scheduler reauthorize <schedule_id>, then ryeos scheduler resume <schedule_id> if it was paused",
            }),
        ),
        Err(error) => check(
            "schedule_authority",
            WARN,
            serde_json::json!({ "error": format!("{error:#}") }),
        ),
    }
}

#[cfg(feature = "scheduling")]
fn inspect_schedule_grant_generations(
    app_root: &std::path::Path,
) -> anyhow::Result<Vec<ryeos_scheduler_service::SupersededScheduleGrant>> {
    let root = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf());
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(&root.node_signing_key_path())?;
    let trust_store = ryeos_engine::trust::TrustStore::load_node_registry(app_root)
        .map_err(|error| anyhow::anyhow!("load trust store: {error}"))?;
    ryeos_scheduler_service::superseded_schedule_grants(
        app_root,
        &root.authorized_keys_dir(),
        &identity,
        &trust_store,
    )
}

fn process_quarantine_check(
    app_root: &std::path::Path,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{OK, WARN};
    let state_dir = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state();
    match ryeos_app::process_quarantine_report::read_report(&state_dir) {
        Ok(None) => check("process_quarantine", OK, serde_json::json!({})),
        Ok(Some(report)) => check(
            "process_quarantine",
            WARN,
            serde_json::json!({
                "message": format!(
                    "{} thread(s) quarantined: process group unproved",
                    report.threads.len()
                ),
                "written_at": report.written_at,
                "threads": report.threads,
                "note": "recovery cannot prove these same-boot process groups gone, so it neither clears nor signals them; each recovery pass rechecks and the record clears once the group is gone",
                "fix": "inspect the group (`ps -o pid,stat,cmd -g <pgid>`); a member stuck in uninterruptible I/O clears when that I/O completes",
            }),
        ),
        Err(error) => check(
            "process_quarantine",
            WARN,
            serde_json::json!({
                "error": format!("{error:#}"),
                "path": ryeos_app::process_quarantine_report::report_path(&state_dir),
            }),
        ),
    }
}

fn stale_publication_check(
    app_root: &std::path::Path,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{OK, WARN};
    let state_dir = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state();
    match ryeos_app::stale_publication_report::read_report(&state_dir) {
        Ok(None) => check("stale_publication", OK, serde_json::json!({})),
        Ok(Some(report)) => check(
            "stale_publication",
            WARN,
            serde_json::json!({
                "message": format!(
                    "{} publish_ready session(s) whose base HEAD has moved",
                    report.sessions.len()
                ),
                "written_at": report.written_at,
                "sessions": report.sessions,
                "note": "each candidate was built on a base that is no longer the project HEAD, so it can never publish; it settles to `base_superseded` (candidate kept as evidence) on the owner's next publish",
                "fix": "the owner runs the session's publish once to settle it, then starts a new session from the current HEAD to redo the work",
            }),
        ),
        Err(error) => check(
            "stale_publication",
            WARN,
            serde_json::json!({
                "error": format!("{error:#}"),
                "path": ryeos_app::stale_publication_report::report_path(&state_dir),
            }),
        ),
    }
}

type HostAccessInspection = (
    ryeos_node_runtime::node_policy::sections::host_access::NodeHostAccessPolicy,
    ryeos_engine::isolation::IsolationPolicy,
    ryeos_engine::trust::TrustStore,
    String,
);

fn inspect_host_access(app_root: &std::path::Path) -> Result<HostAccessInspection> {
    let snapshot = crate::node_descriptors::load_verified_policy_snapshot(app_root)?;
    let policy = snapshot
        .require::<ryeos_node_runtime::node_policy::sections::host_access::NodeHostAccessPolicy>()?
        .clone();
    let lane = snapshot
        .require::<ryeos_engine::isolation::IsolationPolicy>()?
        .clone();
    let trust_store = ryeos_engine::trust::TrustStore::load_node_registry(app_root)?;
    let node_key = ryeos_node_runtime::identity::node_identity_fingerprint(app_root)?;
    Ok((policy, lane, trust_store, node_key))
}

/// The operator's host-direct grants. Each one hands an item
/// operator-equivalent authority, so the row lists them all. It warns when a
/// grant allows process-group containment (a descendant that leaves the group
/// survives cancel) and fails when a grant names an unpinned signer or the
/// node's own key, which a valid generation never contains.
fn host_access_check(app_root: &std::path::Path) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::FAIL;
    let fix = HOST_ACCESS_FIX;
    let inspected = inspect_host_access(app_root);
    match inspected {
        Ok((policy, lane, trust_store, node_key)) => {
            host_access_row(&policy, &lane, &trust_store, &node_key)
        }
        Err(error) => check(
            "host_access",
            FAIL,
            serde_json::json!({ "error": format!("{error:#}"), "fix": fix }),
        ),
    }
}

const HOST_ACCESS_FIX: &str = "ryeos node host-access grant|revoke|list (with --project to compare grants with an item's current bytes)";

/// The `host_access` doctor row for one node's grants and lane.
fn host_access_row(
    policy: &ryeos_node_runtime::node_policy::sections::host_access::NodeHostAccessPolicy,
    lane: &ryeos_engine::isolation::IsolationPolicy,
    trust_store: &ryeos_engine::trust::TrustStore,
    node_key: &str,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, OK, WARN};
    use ryeos_node_runtime::node_policy::sections::host_access::HostAccessContainment;
    let fix = HOST_ACCESS_FIX;
    let mut status = OK;
    let mut findings = Vec::new();
    let grants = policy
        .grants
        .iter()
        .map(|grant| {
            let signer = trust_store.get(&grant.signer);
            if grant.signer == node_key {
                status = FAIL;
                findings.push(format!(
                    "grant `{}` names the node's own key as signer",
                    grant.id
                ));
            } else if signer.is_none() {
                status = FAIL;
                findings.push(format!(
                    "grant `{}` names signer {}, which is not pinned",
                    grant.id, grant.signer
                ));
            } else if grant.containment == HostAccessContainment::GroupAllowed && status == OK {
                status = WARN;
            }
            if grant.containment == HostAccessContainment::GroupAllowed {
                findings.push(format!(
                    "grant `{}` allows group containment: descendants that leave the process group survive cancel",
                    grant.id
                ));
            }
            serde_json::json!({
                "id": grant.id,
                "item": grant.item,
                "signer": grant.signer,
                "signer_label": signer.and_then(|signer| signer.label.clone()),
                "source_closure": grant.source_closure,
                "runtime": grant.runtime,
                "origins": grant.origins,
                "host_env": grant.host_env,
                "containment": grant.containment,
            })
        })
        .collect::<Vec<_>>();
    check(
        "host_access",
        status,
        serde_json::json!({
            "host_class": lane.host_class,
            "host_direct": lane.host_direct,
            "grants": grants,
            "findings": findings,
            "note": "each grant runs its item on the host, unconfined, with the operator's authority",
            "fix": fix,
        }),
    )
}

fn native_capacity_check(
    app_root: &std::path::Path,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, NA, OK, WARN};
    match ryeos_node::native_capacity::inspect_capacity_account(app_root) {
        Ok(None) => check(
            "native_capacity",
            NA,
            serde_json::json!({ "note": "no capacity issuance; `ryeos init` issues it" }),
        ),
        Ok(Some(summary)) if summary.incarnations_remaining > 0 => check(
            "native_capacity",
            OK,
            serde_json::json!({ "account": summary }),
        ),
        Ok(Some(summary)) if summary.renewal.operator_reissue => check(
            "native_capacity",
            WARN,
            serde_json::json!({
                "account": summary,
                "message": "native capacity account exhausted: no incarnation remains in this epoch",
                "fix": "ryeos node capacity reissue --reason \"...\" (then ryeos node allow-restart if a restart inhibition is recorded)",
            }),
        ),
        Ok(Some(summary)) => check(
            "native_capacity",
            FAIL,
            serde_json::json!({
                "account": summary,
                "message": "native capacity account exhausted and the signed policy forbids operator re-issuance",
                "fix": "re-provision the node under a policy that pre-authorises renewal",
            }),
        ),
        Err(error) => check(
            "native_capacity",
            FAIL,
            serde_json::json!({
                "error": format!("{error:#}"),
                "fix": "the account or its anchor is unreadable or foreign. If the app root was moved to another filesystem, `ryeos node capacity reanchor --reason \"...\"` anchors it at the new place; otherwise re-provision the node, do not repair records by hand",
            }),
        ),
    }
}

/// The running incarnation's headroom before its planned rollover, as the
/// daemon reported it in this doctor's lifecycle status read. A driver
/// compares it with the declared needs of what it is about to run; the
/// doctor reports it and judges nothing.
fn service_window_check(
    daemon_running: bool,
    window: Option<&ryeos_node::ServiceWindow>,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{NA, OK};
    match (daemon_running, window) {
        (true, Some(window)) => check("service_window", OK, serde_json::json!(window)),
        (true, None) => check(
            "service_window",
            NA,
            serde_json::json!({
                "note": "the running node reports no native service window (no capacity issuance, or its service already ended)"
            }),
        ),
        (false, _) => check(
            "service_window",
            NA,
            serde_json::json!({
                "note": "the node is not running; the window opens when it starts",
                "fix": "ryeos start",
            }),
        ),
    }
}

/// Recorded, signed capability report plus a fresh probe-free generation
/// check. A refused floor fails the doctor; a missing or stale report warns.
fn inspect_capability_report(
    app_root: &std::path::Path,
) -> (
    &'static str,
    serde_json::Value,
    Option<ryeos_node::capability_report::SignedNodeCapabilityReport>,
) {
    use ryeos_core_tools::actions::doctor::{FAIL, OK, WARN};
    use ryeos_node::capability_report::SignedNodeCapabilityReport;

    let shell = lillux::process_control::HostFacts::observe(
        lillux_resident_quote().unwrap_or_default(),
        app_root,
    );
    let shell_view = shell.capabilities();
    let shell_json = serde_json::json!({
        "note": "observed from this shell, whose limits can differ from the daemon's",
        "provided": shell_view.provided,
        "missing": shell_view.missing,
        "host": shell,
    });
    match SignedNodeCapabilityReport::read_verified(app_root) {
        Ok(None) => (
            WARN,
            serde_json::json!({
                "note": "no capability report recorded yet; the daemon writes one at each start",
                "path": SignedNodeCapabilityReport::path(app_root),
                "shell": shell_json,
            }),
            None,
        ),
        Err(error) => (
            FAIL,
            serde_json::json!({
                "error": format!("{error:#}"),
                "path": SignedNodeCapabilityReport::path(app_root),
                "shell": shell_json,
            }),
            None,
        ),
        Ok(Some(signed)) => {
            // The same policy generation digest the daemon recorded.
            let policy_generation =
                ryeos_node_runtime::engine_init::load_locked_registered_isolation(app_root)
                    .ok()
                    .and_then(|runtime| runtime.digest().map(str::to_owned));
            let staleness = signed
                .staleness(policy_generation.as_deref())
                .map_err(|error| format!("{error:#}"));
            let status = match (&staleness, signed.report.floor.admitted) {
                (_, false) => FAIL,
                (Ok(None), true) => OK,
                (Ok(Some(_)) | Err(_), true) => WARN,
            };
            let detail = serde_json::json!({
                "path": SignedNodeCapabilityReport::path(app_root),
                "digest": signed.digest,
                "signer": signed.signer,
                "generated_at": signed.report.generated_at,
                "current": match &staleness {
                    Ok(None) => serde_json::json!(true),
                    Ok(Some(reason)) => serde_json::json!({ "stale": reason }),
                    Err(error) => serde_json::json!({ "error": error }),
                },
                "composition": signed.report.composition,
                "daemon_digest": signed.report.daemon_digest,
                "exposure": signed.report.exposure,
                "app_root_storage": app_root_storage_json(&signed.report),
                "credential_home": signed.report.credential_home,
                "floor": signed.report.floor,
                "provided": signed.report.provided,
                "missing": signed.report.missing,
                "capacity": signed.report.capacity,
                // The basis the report holds, as of the daemon's start: the
                // one its reservation established, or else only the plan.
                "residency_basis": match (&signed.report.established_residency, &signed.report.host.residency) {
                    (Some(proof), _) => serde_json::json!({
                        "name": proof.capability_name(),
                        "stage": "established",
                    }),
                    (None, Some(proof)) => serde_json::json!({
                        "name": proof.capability_name(),
                        "stage": "planned",
                    }),
                    (None, None) => serde_json::Value::Null,
                },
                "residency_now": residency_now(&signed.report),
                "residency_planned": signed.report.host.residency,
                "enclosure_exit": signed.report.host.enclosure_exit,
                "enclosure_exit_refusal": signed.report.host.enclosure_exit_refusal,
                "residency_established": signed.report.established_residency,
                "report": signed.report,
                "shell": shell_json,
            });
            (status, detail, Some(signed))
        }
    }
}

/// Whether the residency basis the report recorded at the daemon's start
/// still holds now, checked from this shell: a swap-absent basis ends with
/// any later `swapon`, and a swap limit can be lifted. The shell's cgroup can
/// differ from the daemon's, so a swap-limit answer describes this shell.
fn residency_now(
    report: &ryeos_node::capability_report::NodeCapabilityReport,
) -> serde_json::Value {
    let Some(proof) = report
        .established_residency
        .as_ref()
        .or(report.host.residency.as_ref())
    else {
        return serde_json::Value::Null;
    };
    match proof.confirm_swap_unreachable() {
        Ok(()) => serde_json::json!({ "holds": true, "name": proof.capability_name() }),
        Err(reason) => {
            serde_json::json!({ "holds": false, "name": proof.capability_name(), "reason": reason })
        }
    }
}

/// The app root's storage as the report recorded it, with the decision on
/// durable storage.
fn app_root_storage_json(
    report: &ryeos_node::capability_report::NodeCapabilityReport,
) -> serde_json::Value {
    use lillux::process_control::STORAGE_DURABLE_ROOT;
    serde_json::json!({
        "facts": report.host.app_root_storage,
        "decision": match (report.provided.get(STORAGE_DURABLE_ROOT), report.missing.get(STORAGE_DURABLE_ROOT)) {
            (Some(evidence), _) => serde_json::json!({ "provided": evidence }),
            (None, Some(reason)) => serde_json::json!({ "missing": reason }),
            (None, None) => serde_json::Value::Null,
        },
    })
}

/// One line for the app root's storage: the grade and basis that prove it
/// durable, or the reason nothing does.
fn app_root_storage_line(
    report: &ryeos_node::capability_report::NodeCapabilityReport,
) -> (String, crate::tty::Tone) {
    use lillux::process_control::STORAGE_DURABLE_ROOT;
    match (
        report.provided.get(STORAGE_DURABLE_ROOT),
        report.missing.get(STORAGE_DURABLE_ROOT),
    ) {
        (Some(evidence), _) => (
            format!("durable ({}): {}", evidence.source.as_str(), evidence.basis),
            crate::tty::Tone::Secondary,
        ),
        (None, Some(reason)) => (format!("not durable: {reason}"), crate::tty::Tone::Warning),
        (None, None) => ("not decided".to_owned(), crate::tty::Tone::Warning),
    }
}

/// One line for a named report entry: the grade, provider and basis that
/// prove it, or the reason nothing does.
fn decision_line(
    report: &ryeos_node::capability_report::NodeCapabilityReport,
    name: &str,
) -> (String, crate::tty::Tone) {
    match (report.provided.get(name), report.missing.get(name)) {
        (Some(evidence), _) => (
            format!(
                "{name} ({} by {}): {}",
                evidence.source.as_str(),
                evidence.provider,
                evidence.basis
            ),
            crate::tty::Tone::Secondary,
        ),
        (None, Some(reason)) => (
            format!("{name} missing: {reason}"),
            crate::tty::Tone::Warning,
        ),
        (None, None) => (format!("{name} not decided"), crate::tty::Tone::Warning),
    }
}

fn lillux_resident_quote() -> Option<u64> {
    ryeos_node::capability_report::resident_region_bytes().ok()
}

fn capability_section(
    report: Option<&ryeos_node::capability_report::SignedNodeCapabilityReport>,
    check: Option<&ryeos_core_tools::actions::doctor::CheckResult>,
) -> crate::tty::Section {
    let mut section = crate::tty::Section::named("capabilities");
    let Some(signed) = report else {
        let note = check
            .map(|check| check.detail.to_string())
            .unwrap_or_else(|| "not inspected".to_owned());
        section.rows.push(
            crate::tty::Row::text(format!("no verified capability report: {note}"))
                .with_tone(crate::tty::Tone::Warning),
        );
        return section;
    };
    let report = &signed.report;
    let composition = &report.composition;
    section.rows.push(crate::tty::Row::key_value(
        "profile",
        composition.profile.as_deref().unwrap_or("unrecorded"),
    ));
    section.rows.push(crate::tty::Row::key_value(
        "host class",
        &composition.host_class,
    ));
    section.rows.push(crate::tty::Row::key_value(
        "bundles",
        composition.bundles.join(", "),
    ));
    section.rows.push(crate::tty::Row::key_value(
        "compiled features",
        &if composition.compiled_features.is_empty() {
            "none".to_owned()
        } else {
            composition.compiled_features.join(", ")
        },
    ));
    section.rows.push(crate::tty::Row::key_value(
        "daemon binary",
        &report.daemon_digest,
    ));
    let (storage, storage_tone) = app_root_storage_line(report);
    section
        .rows
        .push(crate::tty::Row::key_value("app root storage", &storage).with_tone(storage_tone));
    for (key, name) in [
        (
            "durable state",
            lillux::process_control::PERSISTENCE_DURABLE_STATE,
        ),
        (
            "credential home",
            ryeos_node::capability_report::CREDENTIAL_PRIVATE_HOME,
        ),
    ] {
        let (text, tone) = decision_line(report, name);
        section
            .rows
            .push(crate::tty::Row::key_value(key, &text).with_tone(tone));
    }
    let exposure = &report.exposure;
    section.rows.push(crate::tty::Row::key_value(
        "listeners",
        &if exposure.listeners.is_empty() {
            "none".to_owned()
        } else {
            exposure.listeners.join(", ")
        },
    ));
    let ingress = match (&exposure.ingress, &exposure.ingress_unattested) {
        (Some(evidence), _) => format!("{}: {}", evidence.source.as_str(), evidence.basis),
        (None, reason) => format!(
            "unattested: {}",
            reason.as_deref().unwrap_or("no reason recorded")
        ),
    };
    section
        .rows
        .push(crate::tty::Row::key_value("ingress", &ingress));
    let generation = match check.map(|check| &check.detail["current"]) {
        Some(serde_json::Value::Bool(true)) => "current".to_owned(),
        Some(serde_json::Value::Object(detail)) => match (detail.get("stale"), detail.get("error"))
        {
            (Some(reason), _) => format!(
                "stale: {}; start the node to refresh it",
                reason.as_str().unwrap_or("unknown")
            ),
            (None, Some(error)) => format!("unknown: {}", error.as_str().unwrap_or("unknown")),
            (None, None) => "unknown".to_owned(),
        },
        _ => "unknown".to_owned(),
    };
    section.rows.push(crate::tty::Row::key_value(
        "report",
        format!("{} ({generation})", report.generated_at),
    ));
    let basis_name = |proof: &lillux::process_control::ResidencyProof| {
        proof.capability_name().unwrap_or("an unknown basis")
    };
    // The signed report records the basis as of the daemon's start; whether
    // it still holds is checked again now.
    let residency = match (&report.established_residency, &report.host.residency) {
        (Some(proof), _) => {
            format!(
                "{} (established, as of startup): {}",
                basis_name(proof),
                proof.basis()
            )
        }
        (None, Some(proof)) => {
            format!(
                "{} (planned before reservation, as of startup): {}",
                basis_name(proof),
                proof.basis()
            )
        }
        (None, None) => report
            .host
            .residency_refusal
            .clone()
            .unwrap_or_else(|| "not proven".to_owned()),
    };
    section
        .rows
        .push(crate::tty::Row::key_value("residency", &residency));
    if let Some(proof) = report
        .established_residency
        .as_ref()
        .or(report.host.residency.as_ref())
    {
        let (now, tone) = match proof.confirm_swap_unreachable() {
            Ok(()) => (
                format!("{} still holds", basis_name(proof)),
                crate::tty::Tone::Secondary,
            ),
            Err(reason) => (
                format!("{} no longer holds: {reason}", basis_name(proof)),
                crate::tty::Tone::Warning,
            ),
        };
        section.rows.push(
            crate::tty::Row::key_value("residency now", format!("{now} (checked from this shell)"))
                .with_tone(tone),
        );
    }
    let host = &report.host;
    let enclosure_exit = match (&host.enclosure_exit, &host.enclosure_exit_refusal) {
        (Some(evidence), _) => format!("{}: {}", evidence.source.as_str(), evidence.basis),
        (None, refusal) => format!(
            "not proven: {}",
            refusal.as_deref().unwrap_or("no reason recorded")
        ),
    };
    section.rows.push(crate::tty::Row::key_value(
        "enclosure exit",
        &enclosure_exit,
    ));
    // What a restart-generation witness recorded by this node can decide.
    let witness = match (report.generation.cgroup_enclosure, &host.enclosure_exit) {
        (Some(inode), Some(evidence)) => format!(
            "boot and container enclosure inode {inode} ({})",
            evidence.source.as_str()
        ),
        (Some(_), None) => "boot only; an enclosure change on the same boot is undecided because \
                            the node's exit is not proven to end its enclosure"
            .to_owned(),
        (None, _) => "boot only; the node is not the whole of a container enclosure".to_owned(),
    };
    section
        .rows
        .push(crate::tty::Row::key_value("generation witness", &witness));
    let floor = &report.floor;
    let floor_tone = if floor.admitted {
        crate::tty::Tone::Success
    } else {
        crate::tty::Tone::Failure
    };
    section.rows.push(
        crate::tty::Row::key_value(
            "requires",
            format!(
                "{} — {}",
                floor
                    .requires
                    .iter()
                    .map(floor_entry_text)
                    .collect::<Vec<_>>()
                    .join(", "),
                if floor.admitted { "met" } else { "NOT met" }
            ),
        )
        .with_tone(floor_tone),
    );
    for (label, met) in &floor.met_by {
        // Only an admission on a weaker grade than the entry's strongest
        // name is worth a warning.
        let strongest = floor
            .requires
            .iter()
            .find(|entry| entry.label() == *label)
            .is_some_and(|entry| lillux::process_control::met_by_strongest(entry, &met.name));
        let proof = format!("from {} ({})", met.provider, met.source.as_str());
        let (text, tone) = if strongest {
            (
                format!("  met {label} by {} {proof}", met.name),
                crate::tty::Tone::Secondary,
            )
        } else {
            (
                format!(
                    "  met {label} by {} {proof}, a weaker grade it names",
                    met.name
                ),
                crate::tty::Tone::Warning,
            )
        };
        section
            .rows
            .push(crate::tty::Row::text(text).with_tone(tone));
    }
    for (name, reason) in &floor.unmet {
        section.rows.push(
            crate::tty::Row::text(format!("  unmet {name}: {reason}"))
                .with_tone(crate::tty::Tone::Failure),
        );
    }
    for (name, evidence) in &report.provided {
        let source = evidence.source.as_str();
        section.rows.push(
            crate::tty::Row::text(format!(
                "  proves {name} ({source} by {}): {}",
                evidence.provider, evidence.basis
            ))
            .with_tone(crate::tty::Tone::Secondary),
        );
    }
    for (name, reason) in &report.missing {
        section.rows.push(
            crate::tty::Row::text(format!("  cannot prove {name}: {reason}"))
                .with_tone(crate::tty::Tone::Secondary),
        );
    }
    section
}

/// One floor entry as the doctor shows it: its label, then the grades and
/// providers it accepts when it names any.
fn floor_entry_text(entry: &lillux::process_control::CapabilityRequirement) -> String {
    let mut text = entry.label();
    let mut constraints = Vec::new();
    if !entry.accepted_grades().is_empty() {
        constraints.push(format!("accept {}", entry.accepted_grades().join("|")));
    }
    if !entry.providers().is_empty() {
        constraints.push(format!("providers {}", entry.providers().join("|")));
    }
    if !constraints.is_empty() {
        text.push_str(&format!(" [{}]", constraints.join("; ")));
    }
    text
}

/// Doctor row for the browser-session policy: present, signed, valid. The
/// live session count needs the running node and is reported as not
/// available offline.
fn ui_browser_sessions_check(
    app_root: &std::path::Path,
    initialized: bool,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, NA, OK};
    use ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy;
    if !initialized {
        return check(
            "ui_browser_sessions",
            NA,
            serde_json::json!({ "note": "not initialized" }),
        );
    }
    let loaded = (|| -> Result<UiBrowserSessionPolicy> {
        let isolation =
            ryeos_node_runtime::engine_init::load_locked_registered_definition_isolation(app_root)?;
        let node_trust = isolation
            .registered_generation_node_trust()
            .context("retained isolation generation omitted node trust")?;
        let policy = crate::node_descriptors::load_verified_policy_snapshot_with_trust(
            app_root, node_trust,
        )?;
        Ok(policy.require::<UiBrowserSessionPolicy>()?.clone())
    })();
    match loaded {
        Ok(policy) => check(
            "ui_browser_sessions",
            OK,
            serde_json::json!({
                "session_ttl_secs": policy.session_ttl_secs,
                "session_idle_secs": policy.session_idle_secs,
                "launch_token_ttl_secs": policy.launch_token_ttl_secs,
                "session_scopes": policy.session_scopes,
                "live_sessions": "n/a offline: needs the running node",
            }),
        ),
        Err(error) => check(
            "ui_browser_sessions",
            FAIL,
            serde_json::json!({
                "error": format!("{error:#}"),
                "fix": "publish a complete valid `.ai/node/policies/` generation whose ui_browser_sessions section is schema 5 (session_ttl_secs, session_idle_secs, launch_token_ttl_secs, session_scopes); then run `ryeos node doctor` again",
            }),
        ),
    }
}

/// Build a check row in core-tools doctor vocabulary (its constructor is
/// module-private; the fields are the contract).
/// The `host_services` row: WARN when a RyeOS node service records an app
/// root that no longer exists; NA when this host has no native service
/// directory readable without root.
fn host_services_check(
    listing: Result<Option<Vec<ryeos_node::supervision::NodeHostService>>>,
) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{NA, OK, WARN};
    let services = match listing {
        Ok(Some(services)) => services,
        Ok(None) => {
            return check(
                "host_services",
                NA,
                serde_json::json!({
                    "note": "no native service directory is readable without root here; orphaned host services cannot be listed",
                }),
            );
        }
        Err(error) => {
            return check(
                "host_services",
                WARN,
                serde_json::json!({
                    "error": format!("{error:#}"),
                    "note": "native host services could not be enumerated",
                }),
            );
        }
    };
    let orphaned: Vec<_> = services
        .iter()
        .filter(|service| service.orphaned())
        .map(|service| serde_json::json!({ "name": service.name, "app_root": service.app_root }))
        .collect();
    let unrecognized: Vec<_> = services
        .iter()
        .filter_map(|service| {
            service
                .problem
                .as_ref()
                .map(|problem| serde_json::json!({ "name": service.name, "problem": problem }))
        })
        .collect();
    if orphaned.is_empty() {
        return check(
            "host_services",
            OK,
            serde_json::json!({
                "services": services.len(),
                "unrecognized": unrecognized,
            }),
        );
    }
    check(
        "host_services",
        WARN,
        serde_json::json!({
            "services": services.len(),
            "orphaned": orphaned,
            "unrecognized": unrecognized,
            "note": "these host services belong to nodes whose app root no longer exists",
            "fix": "run: ryeos node host retire --orphaned --confirm",
        }),
    )
}

/// The `remote_pins` doctor row.
fn remote_pins_check(app_root: &Path) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, OK};
    match ryeos_api::remote::config::load_remotes_report(app_root) {
        Ok(report) if report.invalid.is_empty() => {
            let mut remotes: Vec<serde_json::Value> = report
                .remotes
                .values()
                .map(|loaded| {
                    serde_json::json!({
                        "name": loaded.config.name,
                        "pin_source": loaded.pin_source.as_str(),
                        "signature": "verified",
                    })
                })
                .collect();
            remotes.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            check("remote_pins", OK, serde_json::json!({ "remotes": remotes }))
        }
        Ok(report) => {
            let mut invalid: Vec<serde_json::Value> = report
                .invalid
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "name": entry.name,
                        "code": entry.code,
                        "path": entry.config_path,
                        "error": entry.error,
                        "fix": entry.repair_hint,
                    })
                })
                .collect();
            invalid.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            check(
                "remote_pins",
                FAIL,
                serde_json::json!({ "invalid_remotes": invalid }),
            )
        }
        Err(error) => {
            let (code, fix) =
                match error.downcast_ref::<ryeos_api::remote::config::RemotePinError>() {
                    Some(typed) => (Some(typed.code()), typed.to_string()),
                    None => (None, format!("{error:#}")),
                };
            check(
                "remote_pins",
                FAIL,
                serde_json::json!({ "code": code, "error": format!("{error:#}"), "fix": fix }),
            )
        }
    }
}

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

/// Compile the node's signed policy generation as the daemon does.
fn node_policy_check(app_root: &std::path::Path) -> ryeos_core_tools::actions::doctor::CheckResult {
    use ryeos_core_tools::actions::doctor::{FAIL, OK};
    match crate::node_descriptors::load_verified_policy_snapshot(app_root) {
        Ok(snapshot) => check(
            "node_policy",
            OK,
            serde_json::json!({ "generation_digest": snapshot.generation_digest() }),
        ),
        Err(error) => check("node_policy", FAIL, node_policy_failure_detail(&error)),
    }
}

/// A generation missing a newly required section gets its typed code and the
/// replacement that cures it; any other failure is reported as it is.
fn node_policy_failure_detail(error: &anyhow::Error) -> serde_json::Value {
    use ryeos_node_runtime::node_policy::generation::{
        NodePolicySectionsMissing, POLICY_GENERATION_RESET_COMMAND,
    };
    match error
        .chain()
        .find_map(|cause| cause.downcast_ref::<NodePolicySectionsMissing>())
    {
        Some(missing) => serde_json::json!({
            "code": "node_policy_section_missing",
            "sections": missing.sections,
            "error": format!("{error:#}"),
            "fix": format!(
                "stop the daemon, then run: {POLICY_GENERATION_RESET_COMMAND}"
            ),
        }),
        None => serde_json::json!({
            "error": format!("{error:#}"),
            "fix": "publish a complete valid `.ai/node/policies/` generation; then run `ryeos node doctor` again",
        }),
    }
}

fn inspect_isolation_policy(app_root: &std::path::Path) -> Result<IsolationPolicyInspection> {
    use ryeos_engine::isolation::IsolationMode;

    let runtime = ryeos_node_runtime::engine_init::load_locked_registered_isolation(app_root)?;
    let inspection = runtime.inspection();
    let enforced = runtime.mode() == IsolationMode::Enforce;
    let open_files_status = match (enforced, inspection.limits.open_files) {
        (false, _) => "inactive",
        (true, None) => "not_configured",
        (true, Some(_)) => "enforced_on_spawn",
    };
    let (status, advice) = isolation_mode_advice(enforced);
    let mut detail = serde_json::json!({
        "policy": runtime.source(),
        "version": runtime.version(),
        "mode": runtime.mode(),
        "host_direct": inspection.host_direct,
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
    });
    if let (Some(advice), Some(object)) = (advice, detail.as_object_mut()) {
        object.insert("warning".to_owned(), advice.warning.into());
        object.insert("fix".to_owned(), advice.fix.into());
    }
    Ok(IsolationPolicyInspection { detail, status })
}

struct IsolationAdvice {
    warning: &'static str,
    fix: &'static str,
}

/// The doctor status for an isolation lane, and what a disabled lane means.
/// Disabled is not a healthy state: it is an operator's explicit choice that
/// leaves every run unconfined, so it warns.
fn isolation_mode_advice(enforced: bool) -> (&'static str, Option<IsolationAdvice>) {
    use ryeos_core_tools::actions::doctor::{OK, WARN};
    if enforced {
        return (OK, None);
    }
    (
        WARN,
        Some(IsolationAdvice {
            warning: "isolation is disabled: runs execute as the daemon user, can read the operator key on the volume and write any `.ai/` path, and the live-run floors hold only under an enforced lane",
            fix: "re-initialize on a host class with an enforced lane (for example `--host-class local`), or cut a policy generation whose host class has `mode: enforce`; `--host-class local_unconfined` is the explicit opt-out that keeps this state",
        }),
    )
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

    /// Persist this TCP endpoint if the node is stopped, then start it.
    #[arg(long)]
    bind: Option<SocketAddr>,

    /// Persist this local lifecycle endpoint if the node is stopped, then start it.
    #[arg(long)]
    uds_path: Option<PathBuf>,
}

/// Apply the signed offline operations waiting for a node, before this start
/// asks whether the node is inhibited.
///
/// `ryeos start` refuses an inhibited node before it spawns `ryeosd` (a
/// supervisor would otherwise relaunch the refusing daemon in a tight loop),
/// so a request that clears the inhibition would never reach the application
/// the daemon runs at the top of its own startup. Starts driven by a
/// supervisor or an entrypoint spawn `ryeosd` directly and apply there; this
/// is the same application, run in process, so both routes behave alike. A
/// running node holds the state lock, which skips it quietly, and nothing
/// here can stop the start: an error is reported and the start goes on to its
/// own checks.
fn apply_offline_operations_before_start(app_root: Option<PathBuf>, console: &crate::tty::Console) {
    let Ok(config) = ryeos_node::NodeConfig::load_local(app_root) else {
        return;
    };
    let options = ryeos_node::offline_operation::ApplyOptions {
        lock_timeout: std::time::Duration::from_millis(200),
        ..Default::default()
    };
    let host = crate::offline_apply::CliHost::new();
    match ryeos_node::offline_operation::apply_with_options(&config.app_root, &host, &options) {
        Ok(report) if report.skipped.is_some() => {}
        Ok(report) => {
            for line in report.log_lines() {
                if !line.starts_with("offline-operations: fact ") {
                    let _ = console.info(&crate::tty::Diagnostic::info(line));
                }
            }
        }
        Err(error) => {
            let _ = console.warning(&crate::tty::Diagnostic::warning(format!(
                "offline-operations: not applied: {error:#}"
            )));
        }
    }
}

async fn run_start_command(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_or_render_help::<StartArgs>(argv, console)? else {
        return Ok(());
    };
    apply_offline_operations_before_start(args.app_root.clone(), console);
    let env = LocalLifecycleEnv::load(args.app_root)?;
    let controller = LifecycleController::from_env(env);
    let endpoints = ryeos_node::StartEndpointConfiguration {
        bind: args.bind,
        uds_path: args.uds_path,
    };
    let mut progress = crate::tty::LifecycleProgress::new(
        crate::tty::LifecycleProgressAction::Boot,
        console.capabilities(),
    );
    let report = match progress.as_mut() {
        Some(progress) => {
            controller
                .start_with_endpoint_configuration(endpoints, Some(progress))
                .await
        }
        None => {
            controller
                .start_with_endpoint_configuration(endpoints, None)
                .await
        }
    }
    .context("ryeos start failed")?;
    warn_if_last_capacity_incarnation(console, &report)?;
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

/// Warn when this start launched the node on its epoch's last native-capacity
/// incarnation: the next start is refused until the operator re-issues.
fn warn_if_last_capacity_incarnation(
    console: &crate::tty::Console,
    report: &ryeos_node::StartReport,
) -> Result<()> {
    if report.already_running {
        return Ok(());
    }
    let LifecycleStatus::Running {
        capacity: Some(capacity),
        ..
    } = &report.status
    else {
        return Ok(());
    };
    if capacity.incarnations_remaining != 0 {
        return Ok(());
    }
    let mut diagnostic = crate::tty::Diagnostic::warning(format!(
        "this start used the last native capacity incarnation of epoch {}",
        capacity.epoch
    ));
    diagnostic.hint = Some(crate::tty::Hint::new(
        "the next start is refused with native_capacity_exhausted; run `ryeos node capacity reissue --reason \"...\"` first",
    ));
    console.warning(&diagnostic)?;
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
    let options = StopOptions {
        force: args.force,
        ..StopOptions::default()
    };
    let mut progress = crate::tty::LifecycleProgress::new(
        crate::tty::LifecycleProgressAction::Shutdown,
        console.capabilities(),
    );
    let app_root = LocalLifecycleEnv::selected_app_root(args.app_root)?;
    let report = match LocalLifecycleEnv::load(Some(app_root.clone())) {
        Ok(env) => {
            let controller = LifecycleController::from_env(env);
            match progress.as_mut() {
                Some(progress) => controller.stop_with_progress(options, progress).await,
                None => controller.stop(options).await,
            }
        }
        Err(config_error) => {
            ryeos_node::stop::stop_supervised_with_invalid_config(&app_root, options.timeout)
                .await
                .with_context(|| {
                    format!(
                        "node bootstrap configuration is invalid ({config_error:#}); supervised inhibition failed"
                    )
                })
        }
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
        LifecycleStatus::Stopped {
            app_root,
            capacity,
            capacity_error,
        } => {
            let mut banner = crate::tty::StatusBanner::new(
                crate::tty::Tone::Neutral,
                "INITIALIZED, STOPPED — RUN: RYEOS START",
            );
            banner.rows.push(crate::tty::Row::key_value(
                "app root",
                app_root.display().to_string(),
            ));
            for (key, value) in ryeos_node::status::capacity_status_rows(
                capacity.as_ref(),
                capacity_error.as_deref(),
            ) {
                banner.rows.push(crate::tty::Row::key_value(key, value));
            }
            banner
        }
        LifecycleStatus::Running {
            metadata,
            ready_at,
            capacity,
            capacity_error,
            service_window,
            ..
        } => {
            let mut banner = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "RUNNING");
            for (key, value) in ryeos_node::status::capacity_status_rows(
                capacity.as_ref(),
                capacity_error.as_deref(),
            ) {
                banner.rows.push(crate::tty::Row::key_value(key, value));
            }
            for (key, value) in
                ryeos_node::status::service_window_status_rows(service_window.as_ref())
            {
                banner.rows.push(crate::tty::Row::key_value(key, value));
            }
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
pub(crate) fn parse_or_render_help<P: Parser>(
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

fn policy_carry_choices(
    keep: &[String],
    reset: &[String],
) -> ryeos_node_runtime::node_policy::generation::PolicyCarryChoices {
    ryeos_node_runtime::node_policy::generation::PolicyCarryChoices {
        keep: keep.iter().cloned().collect(),
        reset: reset.iter().cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod host_access_doctor {
        use super::*;
        use ryeos_node_runtime::node_policy::sections::host_access::{
            HostAccessContainment, HostAccessGrant, HostAccessOrigin, HostAccessRuntimePin,
            NodeHostAccessPolicy,
        };

        fn signer_key() -> lillux::crypto::SigningKey {
            lillux::crypto::SigningKey::from_bytes(&[7u8; 32])
        }

        fn pinned() -> (ryeos_engine::trust::TrustStore, String) {
            let key = signer_key().verifying_key();
            let fingerprint = lillux::signature::compute_fingerprint(&key);
            let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
                ryeos_engine::trust::TrustedSigner {
                    fingerprint: fingerprint.clone(),
                    verifying_key: key,
                    label: Some("dev publisher".into()),
                },
            ]);
            (trust, fingerprint)
        }

        fn grant(signer: &str, containment: HostAccessContainment) -> HostAccessGrant {
            HostAccessGrant {
                id: "guarded-cargo".into(),
                item: "tool:ryeos/development/dev-host/guarded-cargo".into(),
                signer: signer.to_owned(),
                source_closure: None,
                runtime: HostAccessRuntimePin {
                    runtime_ref: "tool:ryeos/core/runtimes/python/script".into(),
                    content_hash: "c".repeat(64),
                },
                origins: vec![HostAccessOrigin::Direct],
                host_env: vec!["PATH".into()],
                containment,
            }
        }

        fn row(grants: Vec<HostAccessGrant>) -> ryeos_core_tools::actions::doctor::CheckResult {
            let (trust, _) = pinned();
            let mut lane = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
            lane.host_direct = ryeos_engine::isolation::IsolationHostDirect::OperatorGranted;
            host_access_row(
                &NodeHostAccessPolicy { schema: 1, grants },
                &lane,
                &trust,
                &"e".repeat(64),
            )
        }

        #[test]
        fn doctor_host_access_row_lists_grants() {
            let (_, fingerprint) = pinned();
            let result = row(vec![grant(
                &fingerprint,
                HostAccessContainment::ScopeRequired,
            )]);
            assert_eq!(result.name, "host_access");
            assert_eq!(result.status, ryeos_core_tools::actions::doctor::OK);
            assert_eq!(result.detail["host_direct"], "operator_granted");
            let listed = &result.detail["grants"][0];
            assert_eq!(listed["id"], "guarded-cargo");
            assert_eq!(listed["signer_label"], "dev publisher");
            assert_eq!(listed["containment"], "scope_required");
            assert!(
                result.detail["fix"]
                    .as_str()
                    .unwrap()
                    .contains("host-access")
            );
            let empty = row(Vec::new());
            assert_eq!(empty.status, ryeos_core_tools::actions::doctor::OK);
        }

        #[test]
        fn doctor_host_access_warns_on_group_containment() {
            let (_, fingerprint) = pinned();
            let result = row(vec![grant(
                &fingerprint,
                HostAccessContainment::GroupAllowed,
            )]);
            assert_eq!(result.status, ryeos_core_tools::actions::doctor::WARN);
            assert!(
                result.detail["findings"][0]
                    .as_str()
                    .unwrap()
                    .contains("survive cancel")
            );
        }

        #[test]
        fn doctor_host_access_fails_on_an_unpinned_or_node_signer() {
            let unpinned = row(vec![grant(
                &"f".repeat(64),
                HostAccessContainment::ScopeRequired,
            )]);
            assert_eq!(unpinned.status, ryeos_core_tools::actions::doctor::FAIL);
            assert!(
                unpinned.detail["findings"][0]
                    .as_str()
                    .unwrap()
                    .contains("not pinned")
            );
            let node = row(vec![grant(
                &"e".repeat(64),
                HostAccessContainment::ScopeRequired,
            )]);
            assert_eq!(node.status, ryeos_core_tools::actions::doctor::FAIL);
            assert!(
                node.detail["findings"][0]
                    .as_str()
                    .unwrap()
                    .contains("own key")
            );
        }
    }

    #[test]
    fn a_disabled_isolation_lane_warns_and_names_what_it_means_and_the_fix() {
        use ryeos_core_tools::actions::doctor::{OK, WARN};
        let (status, advice) = isolation_mode_advice(false);
        assert_eq!(status, WARN);
        let advice = advice.expect("a disabled lane carries advice");
        assert!(advice.warning.contains("daemon user"), "{}", advice.warning);
        assert!(
            advice.warning.contains("operator key"),
            "{}",
            advice.warning
        );
        assert!(advice.warning.contains(".ai/"), "{}", advice.warning);
        assert!(advice.fix.contains("re-initialize"), "{}", advice.fix);
        assert!(advice.fix.contains("mode: enforce"), "{}", advice.fix);
        let (status, advice) = isolation_mode_advice(true);
        assert_eq!(status, OK);
        assert!(advice.is_none());
    }

    #[test]
    fn capacity_reissue_refuses_while_the_state_lock_is_held_and_names_the_uninitialized_node_next()
    {
        let root = tempfile::tempdir().unwrap();
        let lock_path = ryeos_node_runtime::state_lock::default_lock_path(root.path());
        let held = ryeos_node_runtime::state_lock::StateLock::acquire(&lock_path).unwrap();
        let console = crate::tty::Console::new(crate::tty::TerminalCapabilities::plain(80));
        let argv: Vec<String> = [
            "--app-root",
            root.path().to_str().unwrap(),
            "--reason",
            "unit",
            "--json",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // What a running daemon holds is exactly this lock: the command refuses
        // before it reads a key or opens the account.
        let error = run_node_capacity_reissue_command(&argv, &console).unwrap_err();
        assert!(
            format!("{error:#}").contains("state lock is held"),
            "{error:#}"
        );
        assert!(
            !root
                .path()
                .join(".ai/config/onboarding/native-capacity-reissuance.json")
                .exists()
        );
        drop(held);
        // With the lock free, the next refusal is the absent identity of an
        // uninitialized root; nothing about capacity has been touched.
        let error = run_node_capacity_reissue_command(&argv, &console).unwrap_err();
        assert!(
            format!("{error:#}").contains("not initialized"),
            "{error:#}"
        );
        assert!(!root.path().join(".ai/config/onboarding").exists());
    }

    #[test]
    fn capacity_reissue_banner_names_the_epochs_and_keeps_allow_restart_separate() {
        let report = ryeos_node::native_capacity::ReissueReport {
            from_epoch: 3,
            to_epoch: 4,
            document_digest: "d".repeat(64),
            settled_incarnation: Some("e".repeat(64)),
            already_applied: false,
        };
        let banner = capacity_reissue_banner(std::path::Path::new("/srv/node"), &report);
        assert_eq!(banner.heading, "CAPACITY RE-ISSUED: epoch 3 → 4");
        let value = |key: &str| {
            banner
                .rows
                .iter()
                .find(|row| row.key.as_deref() == Some(key))
                .map(|row| row.value.clone())
                .unwrap_or_else(|| panic!("no {key} row"))
        };
        assert_eq!(value("document"), "d".repeat(64));
        assert_eq!(value("settled incarnation"), "e".repeat(64));
        assert!(
            banner
                .rows
                .iter()
                .any(|row| row.key.is_none() && row.value.contains("allow-restart"))
        );
        let applied = capacity_reissue_banner(
            std::path::Path::new("/srv/node"),
            &ryeos_node::native_capacity::ReissueReport {
                already_applied: true,
                settled_incarnation: None,
                ..report
            },
        );
        assert_eq!(applied.heading, "ALREADY RE-ISSUED: epoch 3 → 4");
        assert!(
            applied
                .rows
                .iter()
                .all(|row| row.key.as_deref() != Some("settled incarnation"))
        );
    }

    #[test]
    fn reanchor_banner_reports_the_signed_document_or_that_nothing_moved() {
        let report = ryeos_node::native_capacity::ReanchorReport {
            document_digest: Some("a".repeat(64)),
            prior_binding_digest: Some("b".repeat(64)),
            already_applied: false,
        };
        let banner = capacity_reanchor_banner(std::path::Path::new("/srv/node"), &report);
        assert!(banner.heading.starts_with("CAPACITY RE-ANCHORED"));
        let value = |banner: &crate::tty::StatusBanner, key: &str| {
            banner
                .rows
                .iter()
                .find(|row| row.key.as_deref() == Some(key))
                .map(|row| row.value.clone())
        };
        assert_eq!(value(&banner, "document"), Some("a".repeat(64)));
        assert_eq!(value(&banner, "replaces binding"), Some("b".repeat(64)));
        let unmoved = capacity_reanchor_banner(
            std::path::Path::new("/srv/node"),
            &ryeos_node::native_capacity::ReanchorReport {
                document_digest: None,
                prior_binding_digest: None,
                already_applied: true,
            },
        );
        assert!(unmoved.heading.starts_with("ALREADY ANCHORED"));
        assert_eq!(value(&unmoved, "document"), None);
    }

    fn host_service_for(root: &std::path::Path) -> ryeos_node::supervision::NodeHostService {
        let launch: lillux::HostServiceLaunch = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "executable": "/usr/bin/ryeosd",
            "arguments": ["host-service", "--app-root", root],
            "environment": {},
            "account": {"implementation": "unix", "uid": 1000, "gid": 1000}
        }))
        .unwrap();
        // The association name is the digest host setup derives from the root.
        let name = format!(
            "ryeos-{}",
            lillux::sha256_hex(
                lillux::canonical_json(&serde_json::to_value(root).unwrap())
                    .unwrap()
                    .as_bytes()
            )
        );
        ryeos_node::supervision::NodeHostService::from_association(lillux::HostServiceAssociation {
            name,
            launch: Ok(launch),
            activated: true,
        })
    }

    #[test]
    fn doctor_host_services_warns_on_orphan() {
        use ryeos_core_tools::actions::doctor::{NA, OK, WARN};
        let directory = tempfile::tempdir().unwrap();
        let live = directory.path().join("live");
        std::fs::create_dir(&live).unwrap();
        let gone = directory.path().join("gone");

        let healthy = host_services_check(Ok(Some(vec![host_service_for(&live)])));
        assert_eq!(healthy.status, OK);
        assert_eq!(healthy.detail["services"], 1);

        let orphaned = host_services_check(Ok(Some(vec![
            host_service_for(&live),
            host_service_for(&gone),
        ])));
        assert_eq!(orphaned.name, "host_services");
        assert_eq!(orphaned.status, WARN);
        let listed = orphaned.detail["orphaned"].as_array().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["app_root"], serde_json::json!(gone));
        assert!(
            orphaned.detail["fix"]
                .as_str()
                .unwrap()
                .contains("ryeos node host retire --orphaned --confirm")
        );

        assert_eq!(host_services_check(Ok(None)).status, NA);
        assert_eq!(
            host_services_check(Err(anyhow::anyhow!("unsafe ownership"))).status,
            WARN
        );
    }

    #[test]
    fn host_retire_requires_confirm() {
        let unconfirmed = NodeHostRetireArgs::try_parse_from(["--orphaned"]).unwrap();
        let error = host_retire_arguments(&unconfirmed).unwrap_err();
        assert!(error.to_string().contains("--confirm"));

        let sweep = NodeHostRetireArgs::try_parse_from(["--orphaned", "--confirm"]).unwrap();
        assert_eq!(
            host_retire_arguments(&sweep).unwrap(),
            ["host-retire", "--orphaned"]
        );

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("node");
        let by_root = NodeHostRetireArgs::try_parse_from([
            std::ffi::OsStr::new("--app-root"),
            root.as_os_str(),
            std::ffi::OsStr::new("--confirm"),
        ])
        .unwrap();
        assert_eq!(
            host_retire_arguments(&by_root).unwrap(),
            [
                std::ffi::OsString::from("host-retire"),
                "--app-root".into(),
                root.clone().into_os_string(),
            ]
        );

        assert!(
            NodeHostRetireArgs::try_parse_from([
                std::ffi::OsStr::new("--orphaned"),
                std::ffi::OsStr::new("--app-root"),
                root.as_os_str(),
                std::ffi::OsStr::new("--confirm"),
            ])
            .is_err()
        );
    }

    #[test]
    fn doctor_has_no_remote_content_testimony_row_finding_on_an_empty_root() {
        let directory = tempfile::tempdir().unwrap();
        let check = remote_content_testimonies_check(directory.path());
        assert_eq!(check.name, "remote_content_testimonies");
        assert_eq!(check.status, ryeos_core_tools::actions::doctor::NA);
    }

    #[test]
    fn doctor_service_window_row_is_na_when_stopped() {
        use ryeos_core_tools::actions::doctor::{NA, OK};
        let stopped = service_window_check(false, None);
        assert_eq!(stopped.name, "service_window");
        assert_eq!(stopped.status, NA);
        assert_eq!(stopped.detail["fix"], "ryeos start");
        let unreported = service_window_check(true, None);
        assert_eq!(unreported.status, NA);
        let window = ryeos_node::ServiceWindow {
            seconds_until_rollover: 4_380,
            work_steps_until_rollover: 61_000_000,
            rolls_at: "2026-07-14T01:13:00Z".into(),
        };
        let running = service_window_check(true, Some(&window));
        assert_eq!(running.status, OK);
        assert_eq!(running.detail["seconds_until_rollover"], 4_380);
        assert_eq!(running.detail["work_steps_until_rollover"], 61_000_000);
        assert_eq!(running.detail["rolls_at"], "2026-07-14T01:13:00Z");
    }

    #[test]
    fn doctor_reports_whether_the_app_root_identity_survives_a_reattach() {
        let directory = tempfile::tempdir().unwrap();
        let check = durable_identity_check(directory.path());
        assert_eq!(check.name, "durable_identity");
        assert!(
            check.status == ryeos_core_tools::actions::doctor::OK
                || check.status == ryeos_core_tools::actions::doctor::WARN,
            "{}",
            check.detail
        );
        let stability = check.detail["app_root"].as_str().unwrap();
        assert!(
            ["reattach_stable", "attachment_bound"].contains(&stability),
            "{stability}"
        );
        let missing = durable_identity_check(&directory.path().join("absent"));
        assert_eq!(missing.status, ryeos_core_tools::actions::doctor::FAIL);
    }

    #[test]
    fn doctor_names_the_cure_for_a_newly_required_policy_section() {
        use ryeos_node_runtime::node_policy::generation::{
            NodePolicySectionsMissing, POLICY_GENERATION_RESET_COMMAND,
        };
        let error = anyhow::Error::new(NodePolicySectionsMissing {
            directory: PathBuf::from("/node/.ai/node/policies"),
            sections: vec!["event_storage".to_owned()],
        })
        .context("load verified node policy generation");
        let detail = node_policy_failure_detail(&error);
        assert_eq!(detail["code"], "node_policy_section_missing");
        assert_eq!(detail["sections"], serde_json::json!(["event_storage"]));
        assert!(
            detail["fix"]
                .as_str()
                .unwrap()
                .contains(POLICY_GENERATION_RESET_COMMAND)
        );

        let other = node_policy_failure_detail(&anyhow::anyhow!("signature mismatch"));
        assert!(other.get("code").is_none());
        assert!(
            other["error"]
                .as_str()
                .unwrap()
                .contains("signature mismatch")
        );
    }

    #[test]
    fn doctor_reports_browser_session_policy_as_not_applicable_before_init() {
        let directory = tempfile::tempdir().unwrap();
        let row = ui_browser_sessions_check(directory.path(), false);
        assert_eq!(row.name, "ui_browser_sessions");
        assert_eq!(row.status, ryeos_core_tools::actions::doctor::NA);
    }

    #[test]
    fn doctor_renders_composition_storage_and_exposure_rows() {
        use lillux::process_control::{
            CapabilityEvidence, HostFacts, ProofSource, STORAGE_DURABLE_ROOT,
        };
        use ryeos_node::capability_report::{
            FloorAdmission, NodeCapabilityReport, NodeComposition, NodeExposure, ReportGeneration,
            SignedNodeCapabilityReport,
        };
        let mut missing = std::collections::BTreeMap::new();
        missing.insert(
            STORAGE_DURABLE_ROOT.to_owned(),
            "the app root /data/app is on overlay (the container's writable layer)".to_owned(),
        );
        missing.insert(
            lillux::process_control::PERSISTENCE_DURABLE_STATE.to_owned(),
            "no persistent disk; 24 h hard lifetime (fixture)".to_owned(),
        );
        let report = NodeCapabilityReport {
            schema: ryeos_node::capability_report::CAPABILITY_REPORT_SCHEMA.to_owned(),
            node_fingerprint: "fixture".to_owned(),
            daemon_digest: "sha256:abc".to_owned(),
            generated_at: "fixture".to_owned(),
            generation: ReportGeneration {
                boot_id: "fixture".to_owned(),
                cgroup_enclosure: None,
            },
            subject: "node".to_owned(),
            composition: NodeComposition {
                profile: Some("hosted-node".to_owned()),
                host_class: "hosted".to_owned(),
                bundles: vec!["core".to_owned()],
                compiled_features: vec!["model-provider-setup".to_owned(), "scheduling".to_owned()],
                policy_generation: "fixture".to_owned(),
            },
            exposure: NodeExposure::unattested(vec!["[::]:8080".to_owned()]),
            host: HostFacts::observe(0, std::path::Path::new("/")),
            credential_home: lillux::process_control::PrivateDirectoryFacts::observe(
                std::path::Path::new("/"),
                std::path::Path::new("/"),
            ),
            durable_identity: ryeos_node::capability_report::DurableIdentityFacts {
                app_root: lillux::IdentityStability::ReattachStable,
            },
            provided: std::collections::BTreeMap::new(),
            missing,
            capacity: std::collections::BTreeMap::new(),
            floor: FloorAdmission {
                requires: Vec::new(),
                admitted: true,
                met_by: std::collections::BTreeMap::new(),
                unmet: std::collections::BTreeMap::new(),
            },
            established_residency: None,
        };
        let row = |section: &crate::tty::Section, key: &str| {
            section
                .rows
                .iter()
                .find(|row| row.key.as_deref() == Some(key))
                .map(|row| row.value.clone())
                .unwrap_or_else(|| panic!("no {key} row in {section:?}"))
        };
        let signed = SignedNodeCapabilityReport {
            report: report.clone(),
            digest: "sha256:fixture".to_owned(),
            signer: "fp:fixture".to_owned(),
            signature: String::new(),
        };
        let section = capability_section(Some(&signed), None);
        assert_eq!(row(&section, "host class"), "hosted");
        assert_eq!(
            row(&section, "compiled features"),
            "model-provider-setup, scheduling"
        );
        assert_eq!(row(&section, "daemon binary"), "sha256:abc");
        assert!(
            row(&section, "app root storage")
                .starts_with("not durable: the app root /data/app is on overlay")
        );
        assert_eq!(
            row(&section, "durable state"),
            "persistence:durable_state missing: no persistent disk; 24 h hard lifetime (fixture)"
        );
        assert_eq!(
            row(&section, "credential home"),
            "credential:private_home not decided"
        );
        assert_eq!(row(&section, "listeners"), "[::]:8080");
        assert_eq!(
            row(&section, "ingress"),
            "unattested: no provider attests what fronts this listener"
        );
        let json = app_root_storage_json(&report);
        assert!(
            json["decision"]["missing"]
                .as_str()
                .unwrap()
                .contains("overlay"),
            "{json}"
        );

        let mut durable = report;
        durable.missing.clear();
        durable.provided.insert(
            STORAGE_DURABLE_ROOT.to_owned(),
            CapabilityEvidence {
                source: ProofSource::Measured,
                provider: lillux::process_control::HOST_PROVIDER.to_owned(),
                basis: "ext4 at /data, durable by mount".to_owned(),
            },
        );
        durable.exposure.listeners.clear();
        let signed = SignedNodeCapabilityReport {
            report: durable,
            digest: signed.digest,
            signer: signed.signer,
            signature: signed.signature,
        };
        let section = capability_section(Some(&signed), None);
        assert_eq!(
            row(&section, "app root storage"),
            "durable (measured): ext4 at /data, durable by mount"
        );
        assert_eq!(row(&section, "listeners"), "none");
    }

    #[test]
    fn replay_reset_reporting_distinguishes_current_and_skipped_epochs() {
        use ryeos_state::operational::{ReplayIndexResetReport, ReplayIndexResetScope};
        for (scope, expected) in [
            (ReplayIndexResetScope::Unchanged, "unchanged"),
            (
                ReplayIndexResetScope::AllReplayRecords,
                "retire all replay rows, including provider-call rows",
            ),
        ] {
            let report = ReplayIndexResetReport {
                stored_epoch: 1,
                current_epoch: 3,
                scope,
            };
            assert!(replay_reset_summary(report).contains(expected));
            let encoded = serde_json::to_value(report).unwrap();
            assert_eq!(encoded["stored_epoch"], 1);
            assert_eq!(encoded["current_epoch"], 3);
        }
    }

    fn execution_history_reset_args(dry_run: bool, confirm: bool) -> ExecutionHistoryResetArgs {
        ExecutionHistoryResetArgs {
            app_root: None,
            include_project_heads: false,
            confirm,
            confirm_project_heads: false,
            dry_run,
            json: false,
            schema_cut_from: None,
            schema_cut_to: None,
            discard_unreadable_lifetime_fence_after_reboot: false,
        }
    }

    fn policy_generation_reset_args(confirm: bool) -> NodePolicyGenerationResetArgs {
        NodePolicyGenerationResetArgs {
            source: PathBuf::from("/usr/share/ryeos"),
            node_profile: "full".to_string(),
            host_class: "local".to_string(),
            trust_files: Vec::new(),
            substrate_image_digest: None,
            app_root: None,
            keep_sections: Vec::new(),
            reset_sections: Vec::new(),
            confirm,
            json: false,
        }
    }

    #[test]
    fn policy_generation_schema_cut_requires_explicit_confirmation() {
        assert!(policy_generation_reset_args(false).validate().is_err());
        assert!(policy_generation_reset_args(true).validate().is_ok());

        let mut invalid = policy_generation_reset_args(true);
        invalid.node_profile = "../full".to_string();
        assert!(invalid.validate().is_err());

        let mut noncanonical = policy_generation_reset_args(true);
        noncanonical.host_class = "Local".to_string();
        assert!(noncanonical.validate().is_err());
    }

    #[test]
    fn init_takes_a_host_class_only_with_a_node_profile() {
        use clap::Parser as _;
        let parsed =
            InitArgs::try_parse_from(["--node-profile", "core", "--host-class", "hosted"]).unwrap();
        assert_eq!(parsed.host_class.as_deref(), Some("hosted"));
        assert!(InitArgs::try_parse_from(["--host-class", "hosted"]).is_err());
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

    fn doctor_grant(
        seed: u8,
        expires_at: ryeos_node_runtime::identity::GrantExpiry,
    ) -> ryeos_node_runtime::identity::GrantFileInspection {
        use ryeos_node_runtime::identity::{
            AuthorizedBy, AuthorizedKeyPrincipalClass, GrantFileInspection, GrantFileState,
            VerifiedAuthorizedKeyGrant,
        };
        let key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]).verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&key);
        GrantFileInspection {
            name: fingerprint.clone(),
            state: GrantFileState::Current(Box::new(VerifiedAuthorizedKeyGrant {
                public_key: key,
                scopes: vec!["ryeos.execute.service.vault/list".to_owned()],
                owner: format!("grant-{seed}"),
                principal_class: AuthorizedKeyPrincipalClass::RemoteNode,
                configured_origin_site_id: Some("site:source".to_owned()),
                source_file_hash: "0".repeat(64),
                fingerprint,
                granted_by: format!("fp:{}", "a".repeat(64)),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
                expires_at,
                authorized_by: AuthorizedBy::principal_request(&"a".repeat(64), None),
            })),
        }
    }

    #[test]
    fn doctor_grant_report_lists_expired_expiring_stale_and_invalid_grants() {
        use ryeos_node_runtime::identity::{
            AuthorizedKeyError, GrantExpiry, GrantFileInspection, GrantFileState,
        };
        let now = 1_000_000;
        let inspections = vec![
            doctor_grant(1, GrantExpiry::At(now - 1)),
            doctor_grant(2, GrantExpiry::At(now + 100)),
            doctor_grant(3, GrantExpiry::At(now + 10_000)),
            doctor_grant(4, GrantExpiry::Never),
            GrantFileInspection {
                name: "old".to_owned(),
                state: GrantFileState::Stale(AuthorizedKeyError::StaleSchema {
                    fingerprint: "old".to_owned(),
                    seen: 2,
                    admitted: 3,
                }),
            },
            GrantFileInspection {
                name: "broken".to_owned(),
                state: GrantFileState::Invalid("tampered key file".to_owned()),
            },
        ];
        let report = grant_report(&inspections, 3_600, now);
        assert_eq!(report.grants, 6);
        assert_eq!(report.expired.len(), 1);
        assert_eq!(report.expiring.len(), 1);
        assert_eq!(report.stale.len(), 1);
        assert_eq!(report.invalid.len(), 1);
        assert_eq!(report.stale[0]["error_code"], "grant_schema_stale");
        assert!(
            report.expired[0]["fix"]
                .as_str()
                .unwrap()
                .contains("ryeos renew-key")
        );
    }
}
