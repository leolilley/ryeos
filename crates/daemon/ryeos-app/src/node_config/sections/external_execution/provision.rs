//! Offline provisioning of a placement credential into the node vault.
//!
//! The installed, node-signed binding names the owner, generation, backend and
//! account the credential belongs to, so the operator supplies only the secret.
//! Two sections carry a credential: the external-execution placement binding
//! and the runtime snapshot production binding, each with its own owner.
//! Provisioning is insert-only: the same secret again is a no-op and a
//! different secret for an existing generation is refused. A new secret needs
//! a newly composed binding generation. Reports never carry the secret or the
//! physical vault key.

use anyhow::{Result, ensure};
use serde::Serialize;
use zeroize::Zeroizing;

use super::SECTION_NAME;
use super::compose::{load_node_identity, read_installed_binding};
use crate::node_config::sections::runtime_snapshot_production;
use crate::vault::placement::PlacementCredentialAccess;
use crate::vault::{NodeVault as _, SealedEnvelopeVault};

/// Largest accepted provider secret, matching the protected document bound.
pub const MAX_PLACEMENT_SECRET_BYTES: usize = 128 * 1024;

/// Which installed binding a credential belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionSection {
    ExternalExecution,
    RuntimeSnapshotProduction,
}

impl ProvisionSection {
    /// Section directory name, as the operator spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ExternalExecution => SECTION_NAME,
            Self::RuntimeSnapshotProduction => runtime_snapshot_production::SECTION_NAME,
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        match name {
            n if n == SECTION_NAME => Ok(Self::ExternalExecution),
            n if n == runtime_snapshot_production::SECTION_NAME => {
                Ok(Self::RuntimeSnapshotProduction)
            }
            other => anyhow::bail!(
                "--section must be `{}` or `{}`, not `{other}`",
                SECTION_NAME,
                runtime_snapshot_production::SECTION_NAME
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisionOutcome {
    Inserted,
    AlreadyPresent,
}

impl ProvisionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::AlreadyPresent => "already_present",
        }
    }
}

/// Coordinates of the provisioned credential. No secret, no physical key.
#[derive(Debug, Clone, Serialize)]
pub struct ProvisionReport {
    pub section: String,
    pub binding_id: String,
    pub owner: String,
    pub generation: String,
    pub backend: String,
    pub account: String,
    pub outcome: ProvisionOutcome,
}

/// Provision `secret` for the installed external-execution binding
/// `binding_id` under `app_root`. Works whether or not the daemon is running;
/// the vault store lock serializes writers and the daemon reads the
/// credential when it next needs it.
pub fn provision_installed_placement_credential(
    app_root: &std::path::Path,
    binding_id: &str,
    secret: &Zeroizing<String>,
) -> Result<ProvisionReport> {
    provision_installed_credential(
        app_root,
        ProvisionSection::ExternalExecution,
        binding_id,
        secret,
    )
}

/// Provision `secret` for the installed binding `binding_id` of `section`.
pub fn provision_installed_credential(
    app_root: &std::path::Path,
    section: ProvisionSection,
    binding_id: &str,
    secret: &Zeroizing<String>,
) -> Result<ProvisionReport> {
    ensure!(
        !secret.is_empty(),
        "placement credential secret must not be empty"
    );
    ensure!(
        secret.len() <= MAX_PLACEMENT_SECRET_BYTES,
        "placement credential secret exceeds {MAX_PLACEMENT_SECRET_BYTES} bytes"
    );
    let identity = load_node_identity(app_root)?;
    let (access, owner, generation, backend, account) = match section {
        ProvisionSection::ExternalExecution => {
            let binding = read_installed_binding(app_root, binding_id, &identity)?;
            // Reverify the exact signed source, document, capacity owner and
            // digest before opening a vault that may perform recovery writes.
            binding.retained_generation()?;
            let (owner, generation, backend, account) = binding.credential_coordinates();
            (
                binding.credential_access()?,
                owner,
                generation,
                backend,
                account,
            )
        }
        ProvisionSection::RuntimeSnapshotProduction => {
            let binding = runtime_snapshot_production::compose::installed_production_binding(
                app_root, binding_id, &identity,
            )?;
            binding.retained_generation()?;
            let (owner, generation, backend, account) = binding.credential_coordinates()?;
            (
                binding.credential_access()?,
                owner,
                generation,
                backend,
                account,
            )
        }
    };
    let outcome = store_secret(app_root, &access, secret)?;
    Ok(ProvisionReport {
        section: section.name().to_owned(),
        binding_id: binding_id.to_owned(),
        owner,
        generation,
        backend,
        account,
        outcome,
    })
}

fn store_secret(
    app_root: &std::path::Path,
    access: &PlacementCredentialAccess,
    secret: &Zeroizing<String>,
) -> Result<ProvisionOutcome> {
    let vault = SealedEnvelopeVault::load(app_root)?;
    let outcome = if vault.placement_credential(access).is_ok() {
        ProvisionOutcome::AlreadyPresent
    } else {
        ProvisionOutcome::Inserted
    };
    let value = access.encode_secret(secret);
    vault.provision_placement_credential(access, value.as_str())?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::super::{BindingDocument, InstalledExternalExecutionBinding};
    use super::*;
    use crate::node_config::sections::runtime_snapshot_production::compose::{
        ProducerFactsSource, ProductionComposeRequest, ProductionLimits, compose_with_facts,
        tests as production_tests,
    };

    const ID: &str = "fixture-binding";

    struct Node {
        root: tempfile::TempDir,
        identity: ryeos_node_runtime::identity::NodeIdentity,
    }

    fn node() -> Node {
        let root = tempfile::tempdir().unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let key = lillux::vault::VaultSecretKey::generate();
        lillux::vault::write_secret_key(
            &ryeos_vault::paths::default_vault_secret_key_path(root.path()),
            &key,
        )
        .unwrap();
        lillux::vault::write_public_key(
            &ryeos_vault::paths::default_vault_public_key_path(root.path()),
            &key.public_key(),
        )
        .unwrap();
        Node { root, identity }
    }

    fn install(
        node: &Node,
        signer: &ryeos_node_runtime::identity::NodeIdentity,
        document: &BindingDocument,
    ) {
        let signed = lillux::signature::sign_content_at(
            &serde_yaml::to_string(document).unwrap(),
            signer.signing_key(),
            "#",
            None,
            "2026-09-23T00:00:00Z",
        );
        let dir = node.root.path().join(".ai/node/external_execution");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{ID}.yaml")), signed).unwrap();
    }

    fn fixture_document() -> BindingDocument {
        InstalledExternalExecutionBinding::test_fixture().document
    }

    fn secret(value: &str) -> Zeroizing<String> {
        Zeroizing::new(value.to_owned())
    }

    fn read_back(node: &Node, report: &ProvisionReport) -> String {
        let access = PlacementCredentialAccess::new(
            &report.owner,
            &report.generation,
            &report.backend,
            &report.account,
        )
        .unwrap();
        let vault = SealedEnvelopeVault::load(node.root.path()).unwrap();
        let decoded = access
            .decode(vault.placement_credential(&access).unwrap())
            .unwrap();
        decoded.secret().to_owned()
    }

    fn immutable(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.to_string() == "placement credential generation is immutable")
    }

    #[test]
    fn provision_then_decode_yields_the_secret() {
        let node = node();
        install(&node, &node.identity, &fixture_document());
        let report =
            provision_installed_placement_credential(node.root.path(), ID, &secret("s3cret"))
                .unwrap();
        assert_eq!(report.outcome, ProvisionOutcome::Inserted);
        assert_eq!(report.binding_id, ID);
        assert_eq!(read_back(&node, &report), "s3cret");
        let rendered = format!("{report:?}{}", serde_json::to_string(&report).unwrap());
        assert!(!rendered.contains("s3cret"));
        assert!(!rendered.contains(ryeos_vault::policy::INTERNAL_PLACEMENT_VAULT_PREFIX));
    }

    #[test]
    fn identical_rerun_is_a_no_op_and_different_value_is_refused() {
        let node = node();
        install(&node, &node.identity, &fixture_document());
        let first =
            provision_installed_placement_credential(node.root.path(), ID, &secret("s3cret"))
                .unwrap();
        let again =
            provision_installed_placement_credential(node.root.path(), ID, &secret("s3cret"))
                .unwrap();
        // The sealed store is re-encrypted on every write, so an identical rerun is
        // judged by the decrypted credential and the reported outcome, not by bytes.
        assert_eq!(again.outcome, ProvisionOutcome::AlreadyPresent);
        assert_eq!(again.generation, first.generation);
        assert_eq!(read_back(&node, &again), "s3cret");
        let error =
            provision_installed_placement_credential(node.root.path(), ID, &secret("changed"))
                .unwrap_err();
        assert!(immutable(&error), "{error:#}");
        assert_eq!(read_back(&node, &again), "s3cret");
    }

    #[test]
    fn foreign_signed_binding_is_refused_before_the_vault_is_touched() {
        let node = node();
        let foreign = ryeos_node_runtime::identity::NodeIdentity::create(
            &node.root.path().join("foreign/private_key.pem"),
        )
        .unwrap();
        install(&node, &foreign, &fixture_document());
        let error = provision_installed_placement_credential(node.root.path(), ID, &secret("x"))
            .unwrap_err();
        assert!(
            error.to_string().contains("signed by the current node"),
            "{error:#}"
        );
        assert!(!crate::vault::default_sealed_store_path(node.root.path()).exists());
    }

    #[test]
    fn missing_or_malformed_binding_is_refused() {
        let node = node();
        let error = provision_installed_placement_credential(node.root.path(), ID, &secret("x"))
            .unwrap_err();
        assert!(error.to_string().contains("is not installed"), "{error:#}");
        for bad in ["", "../escape", "a/b", ".hidden"] {
            assert!(
                provision_installed_placement_credential(node.root.path(), bad, &secret("x"))
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn empty_and_oversize_secrets_are_refused() {
        let node = node();
        install(&node, &node.identity, &fixture_document());
        assert!(
            provision_installed_placement_credential(node.root.path(), ID, &secret("")).is_err()
        );
        let oversize = secret(&"x".repeat(MAX_PLACEMENT_SECRET_BYTES + 1));
        assert!(provision_installed_placement_credential(node.root.path(), ID, &oversize).is_err());
        let exact = secret(&"x".repeat(MAX_PLACEMENT_SECRET_BYTES));
        provision_installed_placement_credential(node.root.path(), ID, &exact).unwrap();
    }

    #[test]
    fn a_new_generation_provisions_a_distinct_key_and_the_old_stays() {
        let node = node();
        let mut document = fixture_document();
        install(&node, &node.identity, &document);
        let old =
            provision_installed_placement_credential(node.root.path(), ID, &secret("old")).unwrap();
        document.credential_generation = "b".repeat(64);
        install(&node, &node.identity, &document);
        let new =
            provision_installed_placement_credential(node.root.path(), ID, &secret("new")).unwrap();
        assert_eq!(new.outcome, ProvisionOutcome::Inserted);
        assert_ne!(old.generation, new.generation);
        assert_eq!(read_back(&node, &old), "old");
        assert_eq!(read_back(&node, &new), "new");
    }

    fn install_production(node: &Node) {
        let lock = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(node.root.path()),
        )
        .unwrap();
        compose_with_facts(
            ProductionComposeRequest {
                app_root: node.root.path(),
                state_lock: &lock,
                id: ID.into(),
                facts_source: ProducerFactsSource::ExternalBinding {
                    id: "operator".into(),
                },
                snapshot_spec: production_tests::SPEC.to_vec(),
                sandbox_group_id: "sbg-exact".into(),
                region: "oregon".into(),
                source_plan: "standard".into(),
                credential_generation: crate::node_config::sections::external_execution::compose::fresh_credential_generation().unwrap(),
                limits: ProductionLimits::default(),
            },
            &node.identity,
            production_tests::facts(),
        )
        .unwrap();
    }

    #[test]
    fn a_production_binding_provisions_under_its_own_owner() {
        let node = node();
        install_production(&node);
        let report = provision_installed_credential(
            node.root.path(),
            ProvisionSection::RuntimeSnapshotProduction,
            ID,
            &secret("producer-secret"),
        )
        .unwrap();
        assert_eq!(report.section, "runtime_snapshot_production");
        assert_eq!(report.outcome, ProvisionOutcome::Inserted);
        assert_eq!(report.backend, "render-sandbox-early-access");
        // The read-back builds its access from the reported coordinates, so a
        // drifting owner formula would not find the stored secret.
        assert_eq!(read_back(&node, &report), "producer-secret");
        let again = provision_installed_credential(
            node.root.path(),
            ProvisionSection::RuntimeSnapshotProduction,
            ID,
            &secret("producer-secret"),
        )
        .unwrap();
        assert_eq!(again.outcome, ProvisionOutcome::AlreadyPresent);
        let error = provision_installed_credential(
            node.root.path(),
            ProvisionSection::RuntimeSnapshotProduction,
            ID,
            &secret("changed"),
        )
        .unwrap_err();
        assert!(immutable(&error), "{error:#}");
        let rendered = format!("{report:?}{}", serde_json::to_string(&report).unwrap());
        assert!(!rendered.contains("producer-secret"));
    }

    #[test]
    fn sections_do_not_resolve_each_others_bindings() {
        let node = node();
        install_production(&node);
        let error = provision_installed_placement_credential(node.root.path(), ID, &secret("x"))
            .unwrap_err();
        assert!(error.to_string().contains("is not installed"), "{error:#}");
        install(&node, &node.identity, &fixture_document());
        std::fs::remove_dir_all(
            node.root
                .path()
                .join(".ai/node/runtime_snapshot_production"),
        )
        .unwrap();
        let error = provision_installed_credential(
            node.root.path(),
            ProvisionSection::RuntimeSnapshotProduction,
            ID,
            &secret("x"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("is not installed"), "{error:#}");
    }

    #[test]
    fn section_names_parse_exactly() {
        for section in [
            ProvisionSection::ExternalExecution,
            ProvisionSection::RuntimeSnapshotProduction,
        ] {
            assert_eq!(ProvisionSection::parse(section.name()).unwrap(), section);
        }
        let error = ProvisionSection::parse("bundle").unwrap_err().to_string();
        assert!(error.contains("--section must be"), "{error}");
    }
}
