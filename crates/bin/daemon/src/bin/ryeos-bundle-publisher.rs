//! Standalone constrained bundle-publisher process.
use anyhow::Context as _;
use clap::Parser;
use ryeos_app::{
    bundle_publication::{
        producer::LocalConstrainedPublisherAuthority,
        standalone_publisher::{
            ManifestOnlyTreePublisher, StandalonePublisherPolicy, StandalonePublisherProof,
        },
    },
    identity::NodeIdentity,
    state_store::NodeIdentitySigner,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Debug, Parser)]
#[command(name = "ryeos-bundle-publisher")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:7411")]
    bind: SocketAddr,
    #[arg(long, required_unless_present = "prepare_policy")]
    publisher_key: Option<PathBuf>,
    #[arg(long, required_unless_present = "prepare_policy")]
    bearer_file: Option<PathBuf>,
    #[arg(long, required_unless_present = "prepare_policy")]
    cas_root: Option<PathBuf>,
    #[arg(long, required_unless_present = "prepare_policy")]
    policy: Option<PathBuf>,
    /// Validate an operator-authored bundle_publication YAML section and emit
    /// the matching standalone publisher JSON policy; never start a listener.
    #[arg(long, requires = "catalog_namespace", conflicts_with_all = ["publisher_key", "bearer_file", "cas_root", "policy"])]
    prepare_policy: Option<PathBuf>,
    #[arg(long, requires = "prepare_policy")]
    catalog_namespace: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
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
    let cas_root = lillux::PinnedDirectory::open(&cas_root)?.context("pin publication CAS root")?;
    let cas = Arc::new(lillux::CasStore::from_pinned_root(cas_root));
    let proof = Arc::new(StandalonePublisherProof::new(
        Arc::clone(&cas),
        policy.clone(),
    )?);
    let tree = Arc::new(ManifestOnlyTreePublisher::new(
        Arc::clone(&cas),
        identity,
        policy,
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
        let section = serde_yaml::from_str("schema: 1\ncatalogs: []\n").unwrap();
        assert!(StandalonePublisherPolicy::from_node_policy(&section, "official").is_err());
    }
}
