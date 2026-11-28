//! The configured operator's public identity, read from the node's own
//! records.
//!
//! The operator's private key lives where the operator works, never in the
//! daemon. Everything the daemon needs to know about the operator it learns
//! from public material: the operator-signed genesis record that
//! `ryeos init` writes under `.ai/node/operator/`, and the node-signed signer
//! record that pins the operator's public key in the signer registry. The
//! genesis names the configured operator by fingerprint; the registry supplies
//! the key; the genesis signature must verify under that key.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::{Signature, Verifier as _, VerifyingKey};
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::{SignerRole, load_node_verifying_key, load_signer_records, signers_dir};

const GENESIS_SCHEMA: &str = "ryeos/operator-genesis/v1";
const MAX_GENESIS_BYTES: u64 = 64 * 1024;

/// Why the configured operator's public identity cannot be established.
#[derive(Debug, thiserror::Error)]
pub enum OperatorIdentityError {
    #[error(
        "operator_genesis_unavailable: cannot read the operator genesis at {path}: {reason}. \
         Run `ryeos init` to create the node's operator identity"
    )]
    GenesisUnavailable { path: PathBuf, reason: String },
    #[error(
        "operator_genesis_invalid: the operator genesis at {path} is not a valid \
         {GENESIS_SCHEMA} record: {reason}"
    )]
    GenesisInvalid { path: PathBuf, reason: String },
    #[error(
        "operator_key_not_pinned: the operator genesis names {fingerprint}, which the node's \
         signer registry does not hold as an operator ({reason}); run `ryeos init` to pin it"
    )]
    NotPinned { fingerprint: String, reason: String },
    #[error(
        "operator_genesis_signature_invalid: the operator genesis at {path} is not signed by the \
         operator key {fingerprint} that the signer registry pins"
    )]
    GenesisSignatureInvalid { path: PathBuf, fingerprint: String },
}

impl OperatorIdentityError {
    /// Stable machine-readable code of the refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::GenesisUnavailable { .. } => "operator_genesis_unavailable",
            Self::GenesisInvalid { .. } => "operator_genesis_invalid",
            Self::NotPinned { .. } => "operator_key_not_pinned",
            Self::GenesisSignatureInvalid { .. } => "operator_genesis_signature_invalid",
        }
    }
}

/// The configured operator as the daemon may know it: a fingerprint and a
/// public key, never a signing key.
#[derive(Debug, Clone)]
pub struct OperatorPublicIdentity {
    verifying_key: VerifyingKey,
    fingerprint: String,
}

impl OperatorPublicIdentity {
    /// Read and verify the operator identity beneath `app_root`.
    pub fn load(app_root: &Path) -> Result<Self, OperatorIdentityError> {
        let genesis_path = RuntimeRoot::new(app_root.to_path_buf()).operator_genesis_path();
        let invalid = |reason: String| OperatorIdentityError::GenesisInvalid {
            path: genesis_path.clone(),
            reason,
        };
        let bytes = lillux::read_regular_file_bounded_no_follow(&genesis_path, MAX_GENESIS_BYTES)
            .map_err(|error| OperatorIdentityError::GenesisUnavailable {
            path: genesis_path.clone(),
            reason: format!("{error:#}"),
        })?;
        let mut document: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
        if document.get("schema").and_then(serde_json::Value::as_str) != Some(GENESIS_SCHEMA) {
            return Err(invalid(format!("schema is not {GENESIS_SCHEMA}")));
        }
        let fingerprint = document
            .get("operator_fingerprint")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid("operator_fingerprint is missing".to_owned()))?
            .to_owned();
        let signature = document
            .as_object_mut()
            .and_then(|object| object.remove("signature"))
            .and_then(|value| value.as_str().map(str::to_owned))
            .and_then(|value| value.strip_prefix("ed25519:").map(str::to_owned))
            .ok_or_else(|| invalid("the ed25519 signature is missing".to_owned()))?;
        let signature = base64::engine::general_purpose::STANDARD
            .decode(signature)
            .map_err(|error| invalid(format!("signature is not base64: {error}")))
            .and_then(|bytes| {
                Signature::from_slice(&bytes)
                    .map_err(|error| invalid(format!("signature is malformed: {error}")))
            })?;

        let not_pinned = |reason: String| OperatorIdentityError::NotPinned {
            fingerprint: fingerprint.clone(),
            reason,
        };
        let node_key =
            load_node_verifying_key(app_root).map_err(|error| not_pinned(format!("{error}")))?;
        let records = load_signer_records(&signers_dir(app_root), &node_key)
            .map_err(|error| not_pinned(format!("{error}")))?;
        let record = records
            .iter()
            .find(|record| record.fingerprint == fingerprint && record.role == SignerRole::Operator)
            .ok_or_else(|| not_pinned("no operator record names it".to_owned()))?;
        let verifying_key = record.verifying_key().map_err(not_pinned)?;

        let canonical = lillux::canonical_json(&document)
            .map_err(|error| invalid(format!("cannot canonicalize: {error}")))?;
        verifying_key
            .verify(canonical.as_bytes(), &signature)
            .map_err(|_| OperatorIdentityError::GenesisSignatureInvalid {
                path: genesis_path.clone(),
                fingerprint: fingerprint.clone(),
            })?;
        Ok(Self {
            verifying_key,
            fingerprint,
        })
    }

    /// The operator's fingerprint, without the `fp:` prefix.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The operator's principal id, `fp:<fingerprint>`.
    pub fn principal_id(&self) -> String {
        format!("fp:{}", self.fingerprint)
    }

    /// Whether `key` (a principal id, `fp:<fingerprint>`) is this operator's
    /// own key. The one comparison of a caller's key with the configured local
    /// operator; it knows nothing of origin, so a forwarded remote operator is
    /// decided by the caller, never here.
    pub fn is_key(&self, key: &str) -> bool {
        self.principal_id() == key
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }
}

/// Fixtures that stand in for `ryeos init`: they mint an operator key where
/// the CLI would hold it and write the public records the daemon reads.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use crate::identity::NodeIdentity;
    use ryeos_engine::trust::SignerScope;

    /// Create the operator key under the layout path, its genesis, and its
    /// node-signed registry record. The node public identity must already be
    /// written. Returns the operator key, which a test plays the CLI with.
    pub fn install_operator(app_root: &Path, node: &NodeIdentity) -> NodeIdentity {
        let runtime = RuntimeRoot::new(app_root.to_path_buf());
        let operator = NodeIdentity::create(&runtime.operator_signing_key_path()).unwrap();
        write_operator_records(app_root, node, &operator);
        operator
    }

    /// Load the operator key a fixture installed.
    pub fn load_operator(app_root: &Path) -> anyhow::Result<NodeIdentity> {
        NodeIdentity::load(&RuntimeRoot::new(app_root.to_path_buf()).operator_signing_key_path())
    }

    /// Write the genesis naming `fingerprint`, signed by `signer`.
    pub fn write_genesis(app_root: &Path, fingerprint: &str, signer: &lillux::crypto::SigningKey) {
        use lillux::crypto::Signer as _;
        let runtime = RuntimeRoot::new(app_root.to_path_buf());
        let mut genesis = serde_json::json!({
            "schema": GENESIS_SCHEMA,
            "operator_fingerprint": fingerprint,
            "created_at": lillux::time::iso8601_now(),
            "ceremony_version": "1",
        });
        let canonical = lillux::canonical_json(&genesis).unwrap();
        let signature = signer.sign(canonical.as_bytes());
        genesis["signature"] = serde_json::Value::String(format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
        ));
        let path = runtime.operator_genesis_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&genesis).unwrap()).unwrap();
    }

    /// Write the genesis and registry record for `operator`.
    pub fn write_operator_records(app_root: &Path, node: &NodeIdentity, operator: &NodeIdentity) {
        write_operator_records_for_key(app_root, node.signing_key(), operator.signing_key());
    }

    /// [`write_operator_records`] for bare keys.
    pub fn write_operator_records_for_key(
        app_root: &Path,
        node_key: &lillux::crypto::SigningKey,
        operator_key: &lillux::crypto::SigningKey,
    ) {
        let verifying = operator_key.verifying_key();
        write_genesis(
            app_root,
            &lillux::sha256_hex(verifying.as_bytes()),
            operator_key,
        );
        ryeos_engine::test_support::pin_signer_with_node_key(
            app_root,
            node_key,
            &verifying,
            "operator",
            SignerRole::Operator,
            SignerScope::operator_default(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;

    fn node(root: &Path) -> NodeIdentity {
        let node = NodeIdentity::create(&root.join("node.pem")).unwrap();
        node.write_public_identity(&ryeos_engine::trust::node_public_identity_path(root))
            .unwrap();
        node
    }

    #[test]
    fn loads_fingerprint_and_key_from_public_records() {
        let root = tempfile::tempdir().unwrap();
        let node = node(root.path());
        let operator = test_support::install_operator(root.path(), &node);
        let loaded = OperatorPublicIdentity::load(root.path()).unwrap();
        assert_eq!(loaded.fingerprint(), operator.fingerprint());
        assert_eq!(loaded.principal_id(), operator.principal_id());
        assert_eq!(loaded.verifying_key(), operator.verifying_key());
    }

    #[test]
    fn missing_genesis_is_refused_with_the_cure() {
        let root = tempfile::tempdir().unwrap();
        node(root.path());
        let error = OperatorPublicIdentity::load(root.path()).unwrap_err();
        assert_eq!(error.code(), "operator_genesis_unavailable");
        assert!(error.to_string().contains("ryeos init"), "{error}");
    }

    #[test]
    fn genesis_signed_by_another_key_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let node = node(root.path());
        test_support::install_operator(root.path(), &node);
        // A genesis that names the pinned operator but is signed by a stranger.
        let stranger = NodeIdentity::create(&root.path().join("stranger.pem")).unwrap();
        let operator = test_support::load_operator(root.path()).unwrap();
        test_support::write_genesis(root.path(), operator.fingerprint(), stranger.signing_key());
        let error = OperatorPublicIdentity::load(root.path()).unwrap_err();
        assert_eq!(error.code(), "operator_genesis_signature_invalid");
    }

    #[test]
    fn an_unpinned_operator_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let node = node(root.path());
        let operator = NodeIdentity::create(&root.path().join("operator.pem")).unwrap();
        // Genesis only, no registry record.
        test_support::write_operator_records(root.path(), &node, &operator);
        std::fs::remove_dir_all(signers_dir(root.path())).unwrap();
        let error = OperatorPublicIdentity::load(root.path()).unwrap_err();
        assert_eq!(error.code(), "operator_key_not_pinned");
    }
}
