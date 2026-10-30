//! `ryeos node external-execution qualification-session-compose`: offline
//! composition of the node-signed binding a consumer qualification's Worker
//! session is placed under.
//!
//! Like `compose`, the command is a request builder: the offline-operation
//! library applies it under the node's state lock, the document is signed by
//! the node identity and verified through the section's own parse before it is
//! published, and the installed document id is printed. The requirement
//! digest, Worker profile hash, artifact identities and configuration
//! destination are read on the node; the operator names the provider facts,
//! the policy, the runtime product and the Render settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use ryeos_app::node_config::sections::external_execution::compose::ComposeLimits;
use ryeos_app::node_config::sections::external_execution::compose_qualification::QualificationBindingReport;
use ryeos_node::offline_operation::handlers::QualificationSessionComposeArguments;

use crate::lifecycle_external_snapshot_compose::{
    FactsChoice, facts_choice_of, parse_args, read_bytes,
};

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node external-execution qualification-session-compose",
    about = "Compose and install the node-signed qualification-only session binding a consumer \
             qualification's Worker is placed under, on a stopped node. Backend, account, owner \
             and provider API trust roots come either from an installed external-execution \
             binding (--binding) or from --backend, --account, --owner-id and --roots together. \
             The Worker requirement digest and profile hash are read from the signed policy the \
             installed bundles carry, and artifact identities and the configuration destination \
             from the installed bundles and provider declaration. The credential generation is \
             fresh on every run; provision it with `provision-credential`, then restart the node.",
    no_binary_name = true
)]
struct QualificationSessionComposeArgs {
    /// Binding id (the file stem under `.ai/node/external_execution/`).
    #[arg(long)]
    id: String,

    /// Installed external-execution binding id to take backend, account, owner and trust
    /// roots from. Exclusive with --backend, --account, --owner-id and --roots.
    #[arg(long)]
    binding: Option<String>,

    /// Lifecycle backend id; `render` selects render-sandbox-early-access.
    #[arg(long)]
    backend: Option<String>,

    /// Provider account label that scopes capacity and the credential.
    #[arg(long)]
    account: Option<String>,

    /// Render owner id; it is also the file-relay owner workspace.
    #[arg(long)]
    owner_id: Option<String>,

    /// PEM file of the CERTIFICATE roots the provider API is served under.
    #[arg(long)]
    roots: Option<PathBuf>,

    /// Signed qualification policy the installed bundles carry, as
    /// `config:<category>/<name>`.
    #[arg(long)]
    policy_ref: String,

    /// Id of the installed provider declaration the Worker names.
    #[arg(long)]
    provider_declaration_id: String,

    /// Manifest hash of the runtime product being qualified.
    #[arg(long)]
    runtime_manifest_hash: String,

    /// Manifest hash of the measured guest-owner runtime the provider snapshot was produced
    /// from, when the qualified runtime is not itself the snapshot's owner. Defaults to
    /// --runtime-manifest-hash.
    #[arg(long)]
    guest_runtime_manifest_hash: Option<String>,

    /// SHA-256 of the executable member inside that runtime product.
    #[arg(long)]
    runtime_member_sha256: String,

    /// Provider snapshot (`snp-...`) the sandbox is restored from.
    #[arg(long)]
    snapshot_id: String,

    /// Provider plan of the sandbox.
    #[arg(long)]
    plan: String,

    /// Provider region of the sandbox.
    #[arg(long)]
    region: String,

    /// Absolute private mailbox parent directory of the file relay.
    #[arg(long)]
    private_mailbox_parent: String,

    /// Capacity group; defaults to `candidate-workers`.
    #[arg(long)]
    capacity_group: Option<String>,

    /// JSON file overriding the placement bounds; omitted fields keep defaults. The export
    /// budget is always signed as zero.
    #[arg(long)]
    limits: Option<PathBuf>,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

/// Entry point for the registered verb. `argv` excludes the verb words.
pub fn run_qualification_session_compose(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_args::<QualificationSessionComposeArgs>(argv, console)? else {
        return Ok(());
    };
    let choice = facts_choice_of(
        &args.binding,
        &args.backend,
        &args.account,
        &args.owner_id,
        &args.roots,
    )?;
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let mut inputs = BTreeMap::new();
    if let Some(path) = &args.limits {
        // Parsed here only to refuse a malformed document before a request is
        // built; the operation parses the same bytes again.
        let bytes = read_bytes(path, "limits")?;
        let _: ComposeLimits = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse limits {}", path.display()))?;
        inputs.insert("limits".to_owned(), bytes);
    }
    let (binding, backend, account, owner_id) = match choice {
        FactsChoice::Binding(binding) => (Some(binding), None, None, None),
        FactsChoice::Stated {
            backend,
            account,
            owner_id,
            roots,
        } => {
            inputs.insert("tls_roots".to_owned(), read_bytes(&roots, "TLS roots")?);
            (None, Some(backend), Some(account), Some(owner_id))
        }
    };
    let host = crate::offline_apply::CliHost::new();
    let applied = host.apply(
        &app_root,
        "node.external-execution.qualification-session-compose",
        serde_json::to_value(QualificationSessionComposeArguments {
            id: args.id.clone(),
            binding,
            backend,
            account,
            owner_id,
            policy_ref: args.policy_ref.clone(),
            provider_declaration_id: args.provider_declaration_id.clone(),
            runtime_manifest_hash: args.runtime_manifest_hash.clone(),
            guest_runtime_manifest_hash: args.guest_runtime_manifest_hash.clone(),
            runtime_member_sha256: args.runtime_member_sha256.clone(),
            snapshot_id: args.snapshot_id.clone(),
            plan: args.plan.clone(),
            region: args.region.clone(),
            private_mailbox_parent: args.private_mailbox_parent.clone(),
            capacity_group: args.capacity_group.clone(),
        })?,
        inputs,
        BTreeMap::new(),
    )?;
    let report: QualificationBindingReport =
        serde_json::from_value(applied.result).context("read the composition report")?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "app_root": app_root,
            "id": report.id,
            "compose": report,
            "provision_command": provision_command(&app_root, &report),
        }))?;
        return Ok(());
    }
    console.status(&compose_banner(&app_root, &report))?;
    Ok(())
}

fn provision_command(app_root: &Path, report: &QualificationBindingReport) -> String {
    format!(
        "ryeos node external-execution provision-credential --binding {} --app-root {}",
        report.id,
        app_root.display()
    )
}

fn compose_banner(
    app_root: &Path,
    report: &QualificationBindingReport,
) -> crate::tty::StatusBanner {
    let mut banner = crate::tty::StatusBanner::new(
        crate::tty::Tone::Success,
        format!("QUALIFICATION SESSION BINDING COMPOSED: {}", report.id),
    );
    for (key, value) in [
        ("app root", app_root.display().to_string()),
        ("binding", report.path.display().to_string()),
        ("backend", report.backend.clone()),
        ("account", report.account.clone()),
        ("capacity group", report.capacity_group.clone()),
        ("provider", report.provider_declaration_id.clone()),
        ("requirement digest", report.requirement_digest.clone()),
        ("worker profile", report.worker_profile_hash.clone()),
        ("runtime manifest", report.runtime_manifest_hash.clone()),
        ("guest-owner manifest", report.guest_runtime_manifest_hash.clone()),
        ("runtime member", report.runtime_member_sha256.clone()),
        ("snapshot", report.snapshot_id.clone()),
        (
            "credential generation",
            report.credential_generation.clone(),
        ),
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

    const REQUIRED: [&str; 22] = [
        "--id",
        "qualification",
        "--binding",
        "operator",
        "--policy-ref",
        "config:codex/consumer-qualification-policy",
        "--provider-declaration-id",
        "codex-hosted",
        "--runtime-manifest-hash",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--runtime-member-sha256",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "--snapshot-id",
        "snp-exact",
        "--plan",
        "standard",
        "--region",
        "oregon",
        "--private-mailbox-parent",
        "/run/ryeos/private-relay",
        "--capacity-group",
        "qualification-workers",
    ];

    #[test]
    fn args_require_every_stated_field_and_reject_extras() {
        let parsed = QualificationSessionComposeArgs::try_parse_from(argv(&REQUIRED)).unwrap();
        assert_eq!(parsed.snapshot_id, "snp-exact");
        assert_eq!(
            parsed.capacity_group.as_deref(),
            Some("qualification-workers")
        );
        // The binding is one of two sources of provider facts and the
        // capacity group is optional.
        for drop in 0..REQUIRED.len() / 2 {
            if matches!(REQUIRED[drop * 2], "--binding" | "--capacity-group") {
                continue;
            }
            let kept: Vec<&str> = REQUIRED
                .chunks(2)
                .enumerate()
                .filter(|(index, _)| *index != drop)
                .flat_map(|(_, pair)| pair.iter().copied())
                .collect();
            assert!(
                QualificationSessionComposeArgs::try_parse_from(argv(&kept)).is_err(),
                "dropping {} was accepted",
                REQUIRED[drop * 2]
            );
        }
        for extra in [["--program", "p.json"], ["--settings", "s.json"]] {
            let mut parts = REQUIRED.to_vec();
            parts.extend_from_slice(&extra);
            assert!(QualificationSessionComposeArgs::try_parse_from(argv(&parts)).is_err());
        }
        // The guest-owner manifest is optional: absent it follows the runtime
        // manifest (resolved by the offline handler), stated it is carried.
        let args = QualificationSessionComposeArgs::try_parse_from(argv(&REQUIRED)).unwrap();
        assert!(args.guest_runtime_manifest_hash.is_none());
        let mut with_guest = REQUIRED.to_vec();
        with_guest.extend_from_slice(&["--guest-runtime-manifest-hash", "g"]);
        let args = QualificationSessionComposeArgs::try_parse_from(argv(&with_guest)).unwrap();
        assert_eq!(args.guest_runtime_manifest_hash.as_deref(), Some("g"));
    }

    #[test]
    fn a_refused_facts_choice_touches_neither_the_node_nor_the_inputs() {
        let root = tempfile::tempdir().unwrap();
        let app_root = root.path().to_str().unwrap();
        let mut mixed = vec!["--app-root", app_root, "--json", "--account", "acct"];
        mixed.extend_from_slice(&REQUIRED);
        let error = run_qualification_session_compose(&argv(&mixed), &console()).unwrap_err();
        assert!(
            format!("{error:#}").contains("remove --backend"),
            "{error:#}"
        );
        let mut partial = vec!["--app-root", app_root, "--json", "--backend", "render"];
        partial.extend_from_slice(&REQUIRED[..2]);
        partial.extend_from_slice(&REQUIRED[4..]);
        let error = run_qualification_session_compose(&argv(&partial), &console()).unwrap_err();
        assert!(
            format!("{error:#}").contains("need --account, --owner-id, --roots too"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_malformed_limits_file_is_refused_before_the_node_is_touched() {
        let root = tempfile::tempdir().unwrap();
        let limits = root.path().join("limits.json");
        std::fs::write(&limits, br#"{"max_actve": 3}"#).unwrap();
        let node = tempfile::tempdir().unwrap();
        let mut parts = vec![
            "--app-root",
            node.path().to_str().unwrap(),
            "--limits",
            limits.to_str().unwrap(),
        ];
        parts.extend_from_slice(&REQUIRED);
        let error = run_qualification_session_compose(&argv(&parts), &console()).unwrap_err();
        assert!(format!("{error:#}").contains("parse limits"), "{error:#}");
        assert!(!node.path().join(".ai/node/external_execution").exists());
    }

    #[test]
    fn the_banner_names_the_installed_document_and_carries_no_secret() {
        let report = QualificationBindingReport {
            id: "qualification".into(),
            path: PathBuf::from("/srv/node/.ai/node/external_execution/qualification.yaml"),
            backend: "render-sandbox-early-access".into(),
            account: "acct".into(),
            capacity_group: "qualification-workers".into(),
            provider_declaration_id: "codex-hosted".into(),
            requirement_digest: "a".repeat(64),
            worker_profile_hash: "b".repeat(64),
            runtime_manifest_hash: "c".repeat(64),
            guest_runtime_manifest_hash: "c".repeat(64),
            runtime_member_sha256: "d".repeat(64),
            snapshot_id: "snp-exact".into(),
            credential_generation: "e".repeat(64),
            settings_digest: "f".repeat(64),
            binding_digest: "1".repeat(64),
            capacity_owner: "2".repeat(64),
        };
        let banner = compose_banner(Path::new("/srv/node"), &report);
        assert_eq!(
            banner.heading,
            "QUALIFICATION SESSION BINDING COMPOSED: qualification"
        );
        assert!(
            banner
                .rows
                .iter()
                .any(|row| row.key.as_deref() == Some("snapshot") && row.value == "snp-exact")
        );
        assert!(banner.rows.iter().any(|row| {
            row.key.is_none()
                && row
                    .value
                    .contains("provision-credential --binding qualification --app-root /srv/node")
                && !row.value.contains("--section")
        }));
    }
}
