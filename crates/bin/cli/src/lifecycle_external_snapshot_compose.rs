//! `ryeos node external-execution snapshot-compose` and
//! `snapshot-qualification-compose`: offline composition of the two
//! node-signed bindings runtime snapshot production and qualification need.
//!
//! Both follow `compose`: authorization is filesystem access to the node's
//! keys, the command refuses unless it holds the node's state lock (a running
//! daemon would read the section while it is rewritten), the document is
//! signed by the node identity and verified through the section's own parse
//! before it is published, and the installed document id is printed. Provider,
//! plan and region names are passed through to the section's validation.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use ryeos_app::node_config::sections::runtime_snapshot_production::compose::{
    ProductionComposeReport, ProductionComposeRequest, ProductionLimits,
    compose_runtime_snapshot_production_binding,
};
use ryeos_app::node_config::sections::runtime_snapshot_qualification::compose::{
    QualificationComposeReport, QualificationComposeRequest, QualificationLimits,
    compose_runtime_snapshot_qualification_binding,
};

/// Largest input document the commands read (specs, consumer verifiers).
const MAX_INPUT_BYTES: u64 = 256 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node external-execution snapshot-compose",
    about = "Compose and install a node-signed runtime snapshot production binding on a stopped \
             node. Backend, account, adapter artifact, owner and API trust roots come from the \
             installed external-execution binding; the credential generation is fresh on every \
             run. Restart the node to load the binding.",
    no_binary_name = true
)]
struct SnapshotComposeArgs {
    /// Production binding id (the file stem under `.ai/node/runtime_snapshot_production/`).
    #[arg(long)]
    id: String,

    /// Installed external-execution binding id to take backend, account and trust from.
    #[arg(long)]
    binding: String,

    /// Snapshot production spec JSON file carried by the installed adapter.
    #[arg(long)]
    provider_spec: PathBuf,

    /// Provider sandbox group the source Sandbox and snapshot live in.
    #[arg(long)]
    sandbox_group_id: String,

    /// Provider region of the source Sandbox.
    #[arg(long)]
    region: String,

    /// Provider plan of the source Sandbox.
    #[arg(long)]
    source_plan: String,

    /// Seconds one provider contact may take; defaults to 60.
    #[arg(long)]
    contact_timeout_seconds: Option<u32>,

    /// Longest lifetime of a bootstrap source in seconds; defaults to 900.
    #[arg(long)]
    maximum_bootstrap_lifetime_seconds: Option<u32>,

    /// Largest runtime upload in bytes; defaults to the protocol maximum.
    #[arg(long)]
    maximum_upload_bytes: Option<u64>,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node external-execution snapshot-qualification-compose",
    about = "Compose and install a node-signed runtime snapshot qualification binding on a \
             stopped node, anchored to an installed production binding whose provider scope it \
             reuses. Restart the node to load the binding.",
    no_binary_name = true
)]
struct SnapshotQualificationComposeArgs {
    /// Qualification binding id (the file stem under `.ai/node/runtime_snapshot_qualification/`).
    #[arg(long)]
    id: String,

    /// Installed runtime snapshot production binding id this qualification restores.
    #[arg(long)]
    snapshot_binding: String,

    /// Lifecycle provider spec JSON file carried by the installed adapter.
    #[arg(long)]
    provider_spec: PathBuf,

    /// Content hash of the restored-runtime verifier executable.
    #[arg(long)]
    verifier: String,

    /// JSON file mapping scenario id to a consumer verifier selection.
    #[arg(long)]
    consumer_verifiers: Option<PathBuf>,

    /// Seconds one provider contact may take; defaults to 60.
    #[arg(long)]
    contact_timeout_seconds: Option<u32>,

    /// Longest lifetime of a qualification attempt in seconds; defaults to 900.
    #[arg(long)]
    maximum_lifetime_seconds: Option<u32>,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

fn read_bytes(path: &Path, what: &str) -> Result<Vec<u8>> {
    lillux::read_regular_file_bounded_no_follow(path, MAX_INPUT_BYTES)
        .with_context(|| format!("read {what} {}", path.display()))
}

fn parse_args<T: Parser>(argv: &[String], console: &crate::tty::Console) -> Result<Option<T>> {
    use clap::error::ErrorKind;
    match T::try_parse_from(argv) {
        Ok(args) => Ok(Some(args)),
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                console.text(&error.render().to_string())?;
                Ok(None)
            }
            _ => bail!("{error}"),
        },
    }
}

fn acquire_stopped_node(
    app_root: &Path,
    verb: &str,
) -> Result<ryeos_node_runtime::state_lock::StateLock> {
    // A running daemon holds the state lock, so composition refuses while the
    // node is up: the live node is the only reader of its own sections.
    ryeos_node_runtime::state_lock::StateLock::acquire(
        &ryeos_node_runtime::state_lock::default_lock_path(app_root),
    )
    .with_context(|| format!("{verb} requires the node to be stopped"))
}

/// Entry point for `snapshot-compose`. `argv` excludes the verb words.
pub fn run_snapshot_compose(argv: &[String], console: &crate::tty::Console) -> Result<()> {
    let Some(args) = parse_args::<SnapshotComposeArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let state_lock = acquire_stopped_node(&app_root, "snapshot-compose")?;
    let defaults = ProductionLimits::default();
    let report = compose_runtime_snapshot_production_binding(ProductionComposeRequest {
        app_root: &app_root,
        state_lock: &state_lock,
        id: args.id.clone(),
        external_binding_id: args.binding.clone(),
        snapshot_spec: read_bytes(&args.provider_spec, "snapshot production spec")?,
        sandbox_group_id: args.sandbox_group_id.clone(),
        region: args.region.clone(),
        source_plan: args.source_plan.clone(),
        limits: ProductionLimits {
            contact_timeout_seconds: args
                .contact_timeout_seconds
                .unwrap_or(defaults.contact_timeout_seconds),
            maximum_bootstrap_lifetime_seconds: args
                .maximum_bootstrap_lifetime_seconds
                .unwrap_or(defaults.maximum_bootstrap_lifetime_seconds),
            maximum_upload_bytes: args
                .maximum_upload_bytes
                .unwrap_or(defaults.maximum_upload_bytes),
        },
    })?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "app_root": app_root,
            "id": report.id,
            "compose": report,
            "provision_command": production_provision_command(&app_root, &report),
        }))?;
        return Ok(());
    }
    console.status(&production_banner(&app_root, &report))?;
    Ok(())
}

/// Entry point for `snapshot-qualification-compose`. `argv` excludes the verb words.
pub fn run_snapshot_qualification_compose(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    let Some(args) = parse_args::<SnapshotQualificationComposeArgs>(argv, console)? else {
        return Ok(());
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root.clone())?.app_root;
    let state_lock = acquire_stopped_node(&app_root, "snapshot-qualification-compose")?;
    let defaults = QualificationLimits::default();
    let consumer_verifiers = match &args.consumer_verifiers {
        Some(path) => Some(
            serde_json::from_slice(&read_bytes(path, "consumer verifiers")?)
                .with_context(|| format!("parse consumer verifiers {}", path.display()))?,
        ),
        None => None,
    };
    let report = compose_runtime_snapshot_qualification_binding(QualificationComposeRequest {
        app_root: &app_root,
        state_lock: &state_lock,
        id: args.id.clone(),
        snapshot_binding_id: args.snapshot_binding.clone(),
        provider_spec: read_bytes(&args.provider_spec, "provider spec")?,
        verifier_artifact_hash: args.verifier.clone(),
        consumer_verifiers,
        limits: QualificationLimits {
            contact_timeout_seconds: args
                .contact_timeout_seconds
                .unwrap_or(defaults.contact_timeout_seconds),
            maximum_lifetime_seconds: args
                .maximum_lifetime_seconds
                .unwrap_or(defaults.maximum_lifetime_seconds),
        },
    })?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({
            "app_root": app_root,
            "id": report.id,
            "compose": report,
        }))?;
        return Ok(());
    }
    console.status(&qualification_banner(&app_root, &report))?;
    Ok(())
}

fn production_provision_command(app_root: &Path, report: &ProductionComposeReport) -> String {
    format!(
        "ryeos node external-execution provision-credential --section runtime_snapshot_production --binding {} --app-root {}",
        report.id,
        app_root.display()
    )
}

fn production_banner(
    app_root: &Path,
    report: &ProductionComposeReport,
) -> crate::tty::StatusBanner {
    let mut banner = crate::tty::StatusBanner::new(
        crate::tty::Tone::Success,
        format!("SNAPSHOT PRODUCTION BINDING COMPOSED: {}", report.id),
    );
    for (key, value) in [
        ("app root", app_root.display().to_string()),
        ("binding", report.path.display().to_string()),
        ("backend", report.backend.clone()),
        ("account", report.account.clone()),
        ("sandbox group", report.provider_group_id.clone()),
        (
            "credential generation",
            report.credential_generation.clone(),
        ),
        ("snapshot spec", report.snapshot_spec_sha256.clone()),
        ("binding digest", report.binding_digest.clone()),
    ] {
        banner.rows.push(crate::tty::Row::key_value(key, value));
    }
    banner.rows.push(crate::tty::Row::text(format!(
        "this generation has no secret yet; next: pipe the provider secret into `{}`",
        production_provision_command(app_root, report)
    )));
    banner.rows.push(crate::tty::Row::text(
        "restart the node to load the binding; composing again names a new credential generation",
    ));
    banner
}

fn qualification_banner(
    app_root: &Path,
    report: &QualificationComposeReport,
) -> crate::tty::StatusBanner {
    let mut banner = crate::tty::StatusBanner::new(
        crate::tty::Tone::Success,
        format!("SNAPSHOT QUALIFICATION BINDING COMPOSED: {}", report.id),
    );
    for (key, value) in [
        ("app root", app_root.display().to_string()),
        ("binding", report.path.display().to_string()),
        ("production binding", report.production_binding_id.clone()),
        (
            "production digest",
            report.production_binding_digest.clone(),
        ),
        ("provider spec", report.provider_spec_digest.clone()),
        ("verifier artifact", report.verifier_artifact_hash.clone()),
        ("binding digest", report.binding_digest.clone()),
    ] {
        banner.rows.push(crate::tty::Row::key_value(key, value));
    }
    banner.rows.push(crate::tty::Row::text(
        "restart the node to load the binding",
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

    const PRODUCTION: [&str; 12] = [
        "--id",
        "producer",
        "--binding",
        "operator",
        "--provider-spec",
        "spec.json",
        "--sandbox-group-id",
        "sbg-1",
        "--region",
        "oregon",
        "--source-plan",
        "standard",
    ];

    #[test]
    fn production_args_require_every_field_and_reject_extras() {
        let parsed = SnapshotComposeArgs::try_parse_from(argv(&PRODUCTION)).unwrap();
        assert_eq!(parsed.source_plan, "standard");
        assert_eq!(parsed.sandbox_group_id, "sbg-1");
        assert!(parsed.contact_timeout_seconds.is_none());
        for drop in 0..PRODUCTION.len() / 2 {
            let kept: Vec<&str> = PRODUCTION
                .chunks(2)
                .enumerate()
                .filter(|(index, _)| *index != drop)
                .flat_map(|(_, pair)| pair.iter().copied())
                .collect();
            assert!(
                SnapshotComposeArgs::try_parse_from(argv(&kept)).is_err(),
                "dropping {} was accepted",
                PRODUCTION[drop * 2]
            );
        }
        let mut extra = PRODUCTION.to_vec();
        extra.extend_from_slice(&["--settings", "s.json"]);
        assert!(SnapshotComposeArgs::try_parse_from(argv(&extra)).is_err());
        let mut budgets = PRODUCTION.to_vec();
        budgets.extend_from_slice(&[
            "--maximum-upload-bytes",
            "20000",
            "--contact-timeout-seconds",
            "30",
        ]);
        let parsed = SnapshotComposeArgs::try_parse_from(argv(&budgets)).unwrap();
        assert_eq!(parsed.maximum_upload_bytes, Some(20000));
        assert_eq!(parsed.contact_timeout_seconds, Some(30));
    }

    #[test]
    fn qualification_args_require_every_field_and_reject_extras() {
        let full = [
            "--id",
            "qualifier",
            "--snapshot-binding",
            "producer",
            "--provider-spec",
            "p.json",
            "--verifier",
            "hash",
        ];
        let parsed = SnapshotQualificationComposeArgs::try_parse_from(argv(&full)).unwrap();
        assert_eq!(parsed.snapshot_binding, "producer");
        assert!(parsed.consumer_verifiers.is_none());
        for drop in 0..4 {
            let kept: Vec<&str> = full
                .chunks(2)
                .enumerate()
                .filter(|(index, _)| *index != drop)
                .flat_map(|(_, pair)| pair.iter().copied())
                .collect();
            assert!(SnapshotQualificationComposeArgs::try_parse_from(argv(&kept)).is_err());
        }
        let mut extra = full.to_vec();
        extra.extend_from_slice(&["--region", "oregon"]);
        assert!(SnapshotQualificationComposeArgs::try_parse_from(argv(&extra)).is_err());
    }

    #[test]
    fn both_verbs_refuse_while_the_state_lock_is_held() {
        let root = tempfile::tempdir().unwrap();
        let lock_path = ryeos_node_runtime::state_lock::default_lock_path(root.path());
        let held = ryeos_node_runtime::state_lock::StateLock::acquire(&lock_path).unwrap();
        let app_root = root.path().to_str().unwrap();
        let production = argv(&[
            "--app-root",
            app_root,
            "--id",
            "producer",
            "--binding",
            "operator",
            "--provider-spec",
            "/nonexistent/spec.json",
            "--sandbox-group-id",
            "sbg-1",
            "--region",
            "oregon",
            "--source-plan",
            "standard",
            "--json",
        ]);
        let error = run_snapshot_compose(&production, &console()).unwrap_err();
        assert!(
            format!("{error:#}").contains("requires the node to be stopped"),
            "{error:#}"
        );
        let qualification = argv(&[
            "--app-root",
            app_root,
            "--id",
            "qualifier",
            "--snapshot-binding",
            "producer",
            "--provider-spec",
            "/nonexistent/p.json",
            "--verifier",
            "hash",
            "--json",
        ]);
        let error = run_snapshot_qualification_compose(&qualification, &console()).unwrap_err();
        assert!(
            format!("{error:#}").contains("requires the node to be stopped"),
            "{error:#}"
        );
        assert!(
            !root
                .path()
                .join(".ai/node/runtime_snapshot_production")
                .exists()
        );
        assert!(
            !root
                .path()
                .join(".ai/node/runtime_snapshot_qualification")
                .exists()
        );
        drop(held);
    }

    #[test]
    fn banners_name_the_installed_document_and_carry_no_secret() {
        let report = ProductionComposeReport {
            id: "producer".into(),
            path: PathBuf::from("/srv/node/.ai/node/runtime_snapshot_production/producer.yaml"),
            backend: "render-sandbox-early-access".into(),
            account: "acct".into(),
            provider_group_id: "sbg-1".into(),
            credential_generation: "a".repeat(64),
            snapshot_spec_sha256: "b".repeat(64),
            settings_digest: "c".repeat(64),
            binding_digest: "d".repeat(64),
        };
        let banner = production_banner(Path::new("/srv/node"), &report);
        assert_eq!(
            banner.heading,
            "SNAPSHOT PRODUCTION BINDING COMPOSED: producer"
        );
        assert!(
            banner
                .rows
                .iter()
                .any(|row| row.key.as_deref() == Some("sandbox group") && row.value == "sbg-1")
        );
        assert!(banner.rows.iter().any(|row| row.key.is_none()
            && row.value.contains(
                "provision-credential --section runtime_snapshot_production --binding producer --app-root /srv/node"
            )));
    }
}
