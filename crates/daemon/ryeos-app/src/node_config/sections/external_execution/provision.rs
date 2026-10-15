//! Offline provisioning of a placement credential into the node vault.
//!
//! The installed, node-signed binding names the owner, generation, backend and
//! account the credential belongs to, so the operator supplies only the secret.
//! Provisioning is insert-only: the same secret again is a no-op and a
//! different secret for an existing generation is refused. A new secret needs
//! a newly composed binding generation. Reports never carry the secret or the
//! physical vault key.

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use zeroize::Zeroizing;

use super::{
    BindingDocument, InstalledExternalExecutionBinding, SECTION_NAME, binding_digest,
    capacity_owner,
};
use crate::vault::{NodeVault as _, SealedEnvelopeVault};

/// Largest accepted provider secret, matching the protected document bound.
pub const MAX_PLACEMENT_SECRET_BYTES: usize = 128 * 1024;

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
    pub binding_id: String,
    pub owner: String,
    pub generation: String,
    pub backend: String,
    pub account: String,
    pub outcome: ProvisionOutcome,
}

/// Provision `secret` for the installed binding `binding_id` under `app_root`.
/// Works whether or not the daemon is running; the vault store lock serializes
/// writers and the daemon reads the credential when it next needs it.
pub fn provision_installed_placement_credential(
    app_root: &std::path::Path,
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
    let binding = read_installed_binding(app_root, binding_id)?;
    // Reverify the exact signed source, document, capacity owner and digest
    // before opening a vault that may perform recovery writes.
    binding.retained_generation()?;
    let access = binding.credential_access()?;
    let vault = SealedEnvelopeVault::load(app_root)?;
    let outcome = if vault.placement_credential(&access).is_ok() {
        ProvisionOutcome::AlreadyPresent
    } else {
        ProvisionOutcome::Inserted
    };
    let value = access.encode_secret(secret);
    vault.provision_placement_credential(&access, value.as_str())?;
    Ok(ProvisionReport {
        binding_id: binding.id.clone(),
        owner: binding.capacity_owner.clone(),
        generation: binding.document.credential_generation.clone(),
        backend: binding.document.backend.clone(),
        account: binding.document.account.clone(),
        outcome,
    })
}

/// Read and verify the installed binding the way the node-config loader does:
/// bounded no-follow read, trusted signature, current-node signer, then the
/// same document parse and validation.
fn read_installed_binding(
    app_root: &std::path::Path,
    binding_id: &str,
) -> Result<InstalledExternalExecutionBinding> {
    ensure!(
        !binding_id.is_empty()
            && binding_id.len() <= 128
            && !binding_id.starts_with('.')
            && binding_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        "external-execution binding id is invalid"
    );
    let path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join(SECTION_NAME)
        .join(format!("{binding_id}.yaml"));
    let Some(bytes) = lillux::read_optional_regular_file_bounded_no_follow(
        &path,
        ryeos_node_runtime::node_document::MAX_ITEM_BYTES,
    )
    .with_context(|| format!("read external-execution binding {}", path.display()))?
    else {
        bail!("external-execution binding `{binding_id}` is not installed");
    };
    let content = std::str::from_utf8(&bytes).context("external-execution binding is not UTF-8")?;
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(
        &app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/identity/private_key.pem"),
    )?;
    let envelope = ryeos_engine::contracts::SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let header = ryeos_engine::item_resolution::parse_signature_header(content, &envelope)
        .context("external-execution binding has no valid signature line")?;
    ensure!(
        header.signer_fingerprint == identity.fingerprint(),
        "external-execution binding must be signed by the current node {}; got {}",
        identity.fingerprint(),
        header.signer_fingerprint
    );
    let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
        ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: *identity.verifying_key(),
            label: None,
        },
    ]);
    let (class, _) =
        ryeos_engine::trust::verify_item_signature(content, &header, &envelope, &trust)
            .context("verify external-execution binding signature")?;
    ensure!(
        class == ryeos_engine::contracts::TrustClass::Trusted,
        "external-execution binding signature is not trusted"
    );
    let body: serde_json::Value = serde_yaml::from_str(
        &ryeos_node_runtime::node_config_source::strip_signature(content),
    )
    .context("parse external-execution binding body")?;
    let document: BindingDocument =
        serde_json::from_value(body).context("invalid external placement binding")?;
    document.validate()?;
    let signer = identity.fingerprint().to_owned();
    let digest = binding_digest(binding_id, &signer, content)?;
    let capacity_owner = capacity_owner(&signer, &document)?;
    Ok(InstalledExternalExecutionBinding {
        id: binding_id.to_owned(),
        document,
        signed_source: content.into(),
        signer,
        signer_verifying_key: identity.verifying_key().to_bytes(),
        digest,
        capacity_owner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::placement::PlacementCredentialAccess;

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
        let error =
            provision_installed_placement_credential(node.root.path(), ID, &secret("x"))
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
            provision_installed_placement_credential(node.root.path(), ID, &secret(""))
                .is_err()
        );
        let oversize = secret(&"x".repeat(MAX_PLACEMENT_SECRET_BYTES + 1));
        assert!(
            provision_installed_placement_credential(node.root.path(), ID, &oversize).is_err()
        );
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
}
