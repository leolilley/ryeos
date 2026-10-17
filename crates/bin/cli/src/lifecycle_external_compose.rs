//! `ryeos node external-execution compose`: offline composition of a
//! node-signed external placement binding on a stopped node.
//!
//! Authorization is filesystem access to the node's keys, as for the other
//! offline lifecycle verbs. The binding is signed by the node identity, so the
//! command refuses unless it holds the node's state lock; a running daemon
//! would otherwise read the section while it is rewritten. Every composition
//! names a fresh credential generation: the provider secret is stored
//! separately with `provision-credential`, and a restart loads the binding.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use ryeos_app::node_config::sections::external_execution::compose::{
    ComposeLimits, ComposeReport, ComposeRequest, ComposeTransportRequest,
    DEFAULT_CAPACITY_GROUP, canonical_backend_id, compose_external_execution_binding,
};
use ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram;
use serde_json::Value;

/// Largest input document the command reads (settings, program, limits,
/// qualification).
const MAX_INPUT_BYTES: u64 = 256 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node external-execution compose",
    about = "Compose and install a node-signed external-execution binding on a stopped node. \
             Artifact identities come from the installed bundles; the credential generation is \
             fresh on every run. Provision the provider secret next, then restart the node.",
    no_binary_name = true
)]
struct ExternalComposeArgs {
    /// Binding id (the file stem under `.ai/node/external_execution/`).
    #[arg(long)]
    id: String,

    /// Lifecycle backend id; `render` selects render-sandbox-early-access.
    #[arg(long)]
    backend: String,

    /// Controller transport: `https` or `file-relay`.
    #[arg(long)]
    transport: String,

    /// https: controller origin, https://host[:port] only.
    #[arg(long)]
    origin: Option<String>,

    /// https: PEM file of the CERTIFICATE roots the controller is served under.
    #[arg(long)]
    roots: Option<PathBuf>,

    /// file-relay: Render owner workspace id; must equal settings `owner_id`.
    #[arg(long)]
    owner_workspace_id: Option<String>,

    /// file-relay: absolute private mailbox parent directory.
    #[arg(long)]
    private_mailbox_parent: Option<String>,

    /// Provider account label that scopes capacity and the credential.
    #[arg(long)]
    account: String,

    /// JSON or YAML file with the adapter settings object.
    #[arg(long)]
    settings: PathBuf,

    /// JSON file with the admitted candidate program this binding selects.
    #[arg(long)]
    program: PathBuf,

    /// Capacity group; defaults to `candidate-workers`.
    #[arg(long)]
    capacity_group: Option<String>,

    /// JSON file selecting a published runtime qualification, or `none`.
    #[arg(long)]
    runtime_qualification: Option<String>,

    /// Expected independently qualified guest-owner runtime manifest hash.
    #[arg(long)]
    guest_runtime_manifest_hash: Option<String>,

    /// JSON file overriding the placement bounds; omitted fields keep defaults.
    #[arg(long)]
    limits: Option<PathBuf>,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

/// Transport selection with every flag combination checked, before any file
/// or lock is touched.
enum TransportChoice {
    Https { origin: String, roots: PathBuf },
    FileRelay { owner_workspace_id: String, private_mailbox_parent: String },
}

fn transport_choice(args: &ExternalComposeArgs) -> Result<TransportChoice> {
    match args.transport.as_str() {
        "https" => {
            ensure!(
                args.owner_workspace_id.is_none() && args.private_mailbox_parent.is_none(),
                "--transport https takes --origin and --roots, not the file-relay flags"
            );
            Ok(TransportChoice::Https {
                origin: args
                    .origin
                    .clone()
                    .context("--transport https requires --origin")?,
                roots: args
                    .roots
                    .clone()
                    .context("--transport https requires --roots")?,
            })
        }
        "file-relay" => {
            ensure!(
                args.roots.is_none(),
                "--transport file-relay carries no TLS roots; remove --roots"
            );
            ensure!(
                args.origin.is_none(),
                "--transport file-relay has no HTTPS origin; remove --origin"
            );
            Ok(TransportChoice::FileRelay {
                owner_workspace_id: args
                    .owner_workspace_id
                    .clone()
                    .context("--transport file-relay requires --owner-workspace-id")?,
                private_mailbox_parent: args
                    .private_mailbox_parent
                    .clone()
                    .context("--transport file-relay requires --private-mailbox-parent")?,
            })
        }
        other => bail!("--transport must be `https` or `file-relay`, not `{other}`"),
    }
}

fn read_text(path: &Path, what: &str) -> Result<String> {
    let bytes = lillux::read_regular_file_bounded_no_follow(path, MAX_INPUT_BYTES)
        .with_context(|| format!("read {what} {}", path.display()))?;
    String::from_utf8(bytes).with_context(|| format!("{what} {} is not UTF-8", path.display()))
}

fn read_json_document<T: serde::de::DeserializeOwned>(path: &Path, what: &str) -> Result<T> {
    let text = read_text(path, what)?;
    serde_json::from_str(&text).with_context(|| format!("parse {what} {}", path.display()))
}

/// Entry point for the registered verb. `argv` excludes the verb words.
pub fn run_external_compose(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    use clap::error::ErrorKind;
    let args = match ExternalComposeArgs::try_parse_from(argv) {
        Ok(args) => args,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                console.text(&error.render().to_string())?;
                return Ok(());
            }
            _ => bail!("{error}"),
        },
    };
    let choice = transport_choice(&args)?;
    let guest_runtime_manifest_hash = args
        .guest_runtime_manifest_hash
        .clone()
        .context("--guest-runtime-manifest-hash is required")?;
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    // A running daemon holds the state lock, so composition refuses while the
    // node is up: the live node is the only reader of its own section.
    let state_lock = ryeos_node_runtime::state_lock::StateLock::acquire(
        &ryeos_node_runtime::state_lock::default_lock_path(&app_root),
    )
    .context("external-execution compose requires the node to be stopped")?;

    let settings: Value = serde_yaml::from_str(&read_text(&args.settings, "settings")?)
        .with_context(|| format!("parse settings {}", args.settings.display()))?;
    let program: AdmittedExternalCandidateProgram = read_json_document(&args.program, "program")?;
    let limits: ComposeLimits = match &args.limits {
        Some(path) => read_json_document(path, "limits")?,
        None => ComposeLimits::default(),
    };
    let runtime_qualification: Option<Value> = match args.runtime_qualification.as_deref() {
        None | Some("none") => None,
        Some(path) => Some(read_json_document(Path::new(path), "runtime qualification")?),
    };
    let transport = match choice {
        TransportChoice::Https { origin, roots } => ComposeTransportRequest::Https {
            origin,
            roots_pem: read_text(&roots, "TLS roots")?,
        },
        TransportChoice::FileRelay {
            owner_workspace_id,
            private_mailbox_parent,
        } => ComposeTransportRequest::FileRelay {
            owner_workspace_id,
            private_mailbox_parent,
        },
    };
    let report = compose_external_execution_binding(ComposeRequest {
        app_root: &app_root,
        state_lock: &state_lock,
        id: args.id.clone(),
        backend: canonical_backend_id(&args.backend).to_owned(),
        program,
        transport,
        account: args.account.clone(),
        capacity_group: args
            .capacity_group
            .clone()
            .unwrap_or_else(|| DEFAULT_CAPACITY_GROUP.to_owned()),
        settings,
        guest_runtime_manifest_hash,
        runtime_qualification,
        limits,
    })?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "app_root": app_root,
            "compose": report,
            "provision_command": provision_command(&app_root, &report),
        }))?;
        return Ok(());
    }
    console.status(&compose_banner(&app_root, &report))?;
    Ok(())
}

fn provision_command(app_root: &Path, report: &ComposeReport) -> String {
    format!(
        "ryeos node external-execution provision-credential --binding {} --app-root {}",
        report.id,
        app_root.display()
    )
}

fn compose_banner(app_root: &Path, report: &ComposeReport) -> crate::tty::StatusBanner {
    let mut banner = crate::tty::StatusBanner::new(
        crate::tty::Tone::Success,
        format!("EXTERNAL-EXECUTION BINDING COMPOSED: {}", report.id),
    );
    for (key, value) in [
        ("app root", app_root.display().to_string()),
        ("binding", report.path.display().to_string()),
        ("backend", report.backend.clone()),
        ("account", report.account.clone()),
        ("capacity group", report.capacity_group.clone()),
        ("transport", report.transport.clone()),
        ("credential generation", report.credential_generation.clone()),
        ("binding digest", report.binding_digest.clone()),
    ] {
        banner.rows.push(crate::tty::Row::key_value(key, value));
    }
    banner.rows.push(crate::tty::Row::text(format!(
        "this generation has no secret yet; next: pipe the provider secret into `{}`",
        provision_command(app_root, report)
    )));
    banner.rows.push(crate::tty::Row::text(
        "restart the node to load the binding; composing again names a new generation and leaves earlier vault entries in place",
    ));
    banner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    fn console() -> crate::tty::Console {
        crate::tty::Console::new(crate::tty::TerminalCapabilities::plain(80))
    }

    #[test]
    fn compose_refuses_while_the_state_lock_is_held() {
        let root = tempfile::tempdir().unwrap();
        let lock_path = ryeos_node_runtime::state_lock::default_lock_path(root.path());
        let held = ryeos_node_runtime::state_lock::StateLock::acquire(&lock_path).unwrap();
        let args = argv(&[
            "--app-root",
            root.path().to_str().unwrap(),
            "--id",
            "operator",
            "--backend",
            "render",
            "--transport",
            "file-relay",
            "--owner-workspace-id",
            "render-owner",
            "--private-mailbox-parent",
            "/run/ryeos/relay",
            "--account",
            "acct",
            "--settings",
            "/nonexistent/settings.json",
            "--program",
            "/nonexistent/program.json",
            "--guest-runtime-manifest-hash",
            &"4".repeat(64),
            "--json",
        ]);
        let error = run_external_compose(&args, &console()).unwrap_err();
        assert!(
            format!("{error:#}").contains("requires the node to be stopped"),
            "{error:#}"
        );
        assert!(!root.path().join(".ai/node/external_execution").exists());
        drop(held);
    }

    #[test]
    fn transport_flags_must_match_the_selected_transport() {
        let base = ["--id", "x", "--backend", "render", "--account", "a", "--settings", "s", "--program", "p"];
        let parse = |extra: &[&str]| {
            let mut all = base.to_vec();
            all.extend_from_slice(extra);
            ExternalComposeArgs::try_parse_from(argv(&all)).unwrap()
        };
        let refused = |extra: &[&str]| transport_choice(&parse(extra)).err().map(|e| e.to_string());
        assert!(refused(&["--transport", "https", "--origin", "https://h", "--roots", "r.pem"]).is_none());
        assert!(refused(&["--transport", "https", "--origin", "https://h"]).unwrap().contains("--roots"));
        assert!(refused(&["--transport", "https", "--roots", "r.pem"]).unwrap().contains("--origin"));
        assert!(
            refused(&["--transport", "https", "--origin", "https://h", "--roots", "r.pem", "--owner-workspace-id", "o"])
                .unwrap()
                .contains("not the file-relay flags")
        );
        let relay = ["--transport", "file-relay", "--owner-workspace-id", "o", "--private-mailbox-parent", "/m"];
        assert!(refused(&relay).is_none());
        let mut with_roots = relay.to_vec();
        with_roots.extend_from_slice(&["--roots", "r.pem"]);
        assert!(refused(&with_roots).unwrap().contains("carries no TLS roots"));
        assert!(refused(&["--transport", "file-relay", "--owner-workspace-id", "o"])
            .unwrap()
            .contains("--private-mailbox-parent"));
        assert!(refused(&["--transport", "ftp"]).unwrap().contains("`https` or `file-relay`"));
    }

    #[test]
    fn banner_names_the_generation_and_the_provision_command_without_a_secret() {
        let report = ComposeReport {
            id: "operator".into(),
            path: PathBuf::from("/srv/node/.ai/node/external_execution/operator.yaml"),
            backend: "render-sandbox-early-access".into(),
            account: "acct".into(),
            capacity_group: "candidate-workers".into(),
            transport: "https".into(),
            credential_generation: "a".repeat(64),
            settings_digest: "b".repeat(64),
            binding_digest: "c".repeat(64),
            capacity_owner: "d".repeat(64),
        };
        let banner = compose_banner(Path::new("/srv/node"), &report);
        assert_eq!(banner.heading, "EXTERNAL-EXECUTION BINDING COMPOSED: operator");
        let value = |key: &str| {
            banner
                .rows
                .iter()
                .find(|row| row.key.as_deref() == Some(key))
                .map(|row| row.value.clone())
                .unwrap_or_else(|| panic!("no {key} row"))
        };
        assert_eq!(value("credential generation"), "a".repeat(64));
        assert!(banner.rows.iter().any(|row| row.key.is_none()
            && row.value.contains(
                "ryeos node external-execution provision-credential --binding operator --app-root /srv/node"
            )));
    }
}
