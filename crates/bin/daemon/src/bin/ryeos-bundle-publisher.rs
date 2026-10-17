//! Standalone constrained bundle-publisher process.
use anyhow::Context as _;
use clap::Parser;
use ryeos_app::{bundle_publication::{
        producer::LocalConstrainedPublisherAuthority,
        standalone_publisher::{
            ManifestOnlyTreePublisher, StandalonePublisherPolicy, StandalonePublisherProof,
            observe_current_publisher_tool, observe_publisher_tool,
        },
    }, state_store::NodeIdentitySigner};
use ryeos_node_runtime::{identity::NodeIdentity};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Debug, Parser)]
#[command(name = "ryeos-bundle-publisher")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7411")]
    bind: SocketAddr,
    #[arg(long, required_unless_present_any = ["prepare_policy", "measure_tool"])]
    publisher_key: Option<PathBuf>,
    #[arg(long, required_unless_present_any = ["prepare_policy", "measure_tool"])]
    bearer_file: Option<PathBuf>,
    #[arg(long, required_unless_present_any = ["prepare_policy", "measure_tool"])]
    cas_root: Option<PathBuf>,
    #[arg(long, required_unless_present_any = ["prepare_policy", "measure_tool"])]
    policy: Option<PathBuf>,
    /// Installed bundle-release bundle root; its signed payload ownership
    /// (signed by the catalog publisher) bounds every release input's
    /// payloads and Cargo features.
    #[arg(long, required_unless_present_any = ["prepare_policy", "measure_tool"])]
    bundle_release_root: Option<PathBuf>,
    /// Validate an operator-authored bundle_publication YAML section and emit
    /// the matching standalone publisher JSON policy; never start a listener.
    #[arg(long, requires = "catalog_namespace", conflicts_with_all = ["publisher_key", "bearer_file", "cas_root", "policy", "bundle_release_root", "measure_tool"])]
    prepare_policy: Option<PathBuf>,
    #[arg(long, requires = "prepare_policy")]
    catalog_namespace: Option<String>,
    /// Measure this exact publisher executable and emit the closed operation
    /// definition and artifact identities without loading keys or starting a
    /// listener.
    #[arg(long, conflicts_with_all = ["publisher_key", "bearer_file", "cas_root", "policy", "bundle_release_root", "prepare_policy", "catalog_namespace"])]
    measure_tool: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(path) = &args.measure_tool {
        println!(
            "{}",
            serde_json::to_string_pretty(&observe_publisher_tool(path)?)?
        );
        return Ok(());
    }
    if let Some(path) = &args.prepare_policy {
        let section = serde_yaml::from_slice(&std::fs::read(path)?)?;
        let policy = StandalonePublisherPolicy::from_node_policy(
            &section,
            args.catalog_namespace
                .as_deref()
                .context("catalog namespace is required")?,
        )?;
        println!("{}", serde_json::to_string_pretty(&policy)?);
        return Ok(());
    }
    let publisher_key = args.publisher_key.context("publisher key is required")?;
    let bearer_file = args.bearer_file.context("bearer file is required")?;
    let cas_root = args.cas_root.context("CAS root is required")?;
    let policy_path = args.policy.context("publisher policy is required")?;
    let bundle_release_root = args
        .bundle_release_root
        .context("bundle-release root is required")?;
    anyhow::ensure!(
        args.bind.ip().is_loopback(),
        "plain HTTP publisher must bind to loopback"
    );
    anyhow::ensure!(cas_root.is_dir(), "publication CAS root is absent");
    let identity = NodeIdentity::load(&publisher_key).context("load publisher key")?;
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let bearer = std::fs::read_to_string(&bearer_file).context("read bearer file")?;
    let bearer = bearer.trim_end_matches(['\r', '\n']).to_owned();
    let policy: StandalonePublisherPolicy = serde_json::from_slice(
        &std::fs::read(&policy_path).context("read standalone publisher policy")?,
    )
    .context("decode standalone publisher policy")?;
    policy.validate()?;
    require_publisher_identity(
        &policy.catalog_publisher_fingerprint,
        identity.fingerprint(),
    )?;
    // The payload ownership is trusted only when the catalog publisher itself
    // signed it: the publisher key is the sole signer admitted here.
    let ownership_trust =
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: identity.signing_key().verifying_key(),
            label: None,
        }]);
    let ownership = ryeos_app::bundle_publication::admitted_build::load_installed_payload_ownership(
        &bundle_release_root,
        &ownership_trust,
    )?;
    let observed_tool = observe_current_publisher_tool()?;
    policy.require_observed_tool(&observed_tool)?;
    let cas_root = lillux::PinnedDirectory::open(&cas_root)?.context("pin publication CAS root")?;
    let cas = Arc::new(lillux::CasStore::from_pinned_root(cas_root));
    let proof = Arc::new(StandalonePublisherProof::new(
        Arc::clone(&cas),
        policy.clone(),
        ownership.clone(),
    )?);
    let tree = Arc::new(ManifestOnlyTreePublisher::new(
        Arc::clone(&cas),
        identity,
        policy,
        ownership,
    )?);
    let authority = Arc::new(LocalConstrainedPublisherAuthority::new_with_cas(
        cas,
        tree,
        proof.clone(),
        proof,
        signer,
    )?);
    let state = ryeos_api::publisher_server::PublisherServerState::new(authority, bearer)?;
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, ryeos_api::publisher_server::router(state)).await?;
    Ok(())
}

fn require_publisher_identity(expected: &str, actual: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual == expected,
        "loaded publisher signing key does not match the catalog publisher fingerprint"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_needs_no_key_or_server_resources() {
        assert!(
            Args::try_parse_from([
                "publisher",
                "--prepare-policy",
                "policy.yaml",
                "--catalog-namespace",
                "official"
            ])
            .is_ok()
        );
        assert!(Args::try_parse_from(["publisher", "--prepare-policy", "policy.yaml"]).is_err());
        assert!(Args::try_parse_from(["publisher"]).is_err());
        assert!(
            Args::try_parse_from([
                "publisher",
                "--prepare-policy",
                "policy.yaml",
                "--catalog-namespace",
                "official",
                "--publisher-key",
                "secret.pem"
            ])
            .is_err()
        );
    }

    #[test]
    fn deny_all_bootstrap_cannot_produce_publisher_authority() {
        let section = serde_yaml::from_str("schema: 2\ncatalogs: []\n").unwrap();
        assert!(StandalonePublisherPolicy::from_node_policy(&section, "official").is_err());
    }

    #[test]
    fn tool_measurement_mode_needs_no_secret_or_server_resources() {
        assert!(Args::try_parse_from(["publisher", "--measure-tool", "/publisher"]).is_ok());
        assert!(
            Args::try_parse_from([
                "publisher",
                "--measure-tool",
                "/publisher",
                "--publisher-key",
                "/secret"
            ])
            .is_err()
        );
    }

    #[test]
    fn publisher_key_must_match_policy_before_serving() {
        let expected = "a".repeat(64);
        assert!(require_publisher_identity(&expected, &expected).is_ok());
        assert!(require_publisher_identity(&expected, &"b".repeat(64)).is_err());
    }
}
