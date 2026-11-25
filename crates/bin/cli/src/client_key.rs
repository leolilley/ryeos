//! Which key the CLI acts with, and what that key may do to a node's files.
//!
//! `RYEOS_CLIENT_KEY` names the key a client signs with. Everything that goes
//! through the daemon is then authorized by that key's grant, like any other
//! key's. Commands that act directly on a node's files (rotate a key, pin a
//! signer, register a project, reset state) have no daemon to check them: they
//! need the node's operator key, so with another key selected they refuse
//! with `operator_key_required` instead of acting.
//!
//! This is a check on a key, not on what kind of thing holds it. It does not
//! hide the node's own key files from a process that can read them: on a node
//! that does not enclose its processes, a process of the same OS user can still
//! read the operator key, and only running it as another user or inside the
//! enclosure keeps it out.

use std::path::Path;

use ryeos_engine::trust::{SignerRole, SignerStatus, TrustStore};

use crate::error::CliError;
use crate::transport::signing::{CLIENT_KEY_ENV, client_signing_key_path};

fn refusal(detail: String) -> CliError {
    CliError::Local { detail }
}

/// The operator-key requirement for a key with this fingerprint under this
/// node's signer registry.
pub fn operator_key_required(fingerprint: &str, trust: &TrustStore) -> Result<(), CliError> {
    match trust.signer(fingerprint) {
        Some(record)
            if record.role == SignerRole::Operator && record.status == SignerStatus::Active =>
        {
            Ok(())
        }
        _ => Err(refusal(format!(
            "operator_key_required: this command acts on the node's own files and needs the \
             node's operator key, but {CLIENT_KEY_ENV} selects key {fingerprint}, which is not an \
             active operator signer of this node. Run it without {CLIENT_KEY_ENV}, as the \
             operator"
        ))),
    }
}

/// Require that the key this process acts with is the node's active operator
/// signer. With no key selected the operator key is the key, and this passes.
pub fn require_operator_key(app_root: &Path) -> Result<(), CliError> {
    if std::env::var_os(CLIENT_KEY_ENV).is_none() {
        return Ok(());
    }
    let path = client_signing_key_path(app_root)?;
    let key = lillux::crypto::load_signing_key(&path).map_err(|error| {
        refusal(format!(
            "client_key_unreadable: the key {} selected by {CLIENT_KEY_ENV} cannot be loaded: \
             {error:#}",
            path.display()
        ))
    })?;
    let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
    let trust = TrustStore::load_node_registry(app_root).map_err(|error| {
        refusal(format!(
            "operator_key_required: the node's signer registry cannot be read to check the \
             selected key: {error}"
        ))
    })?;
    operator_key_required(&fingerprint, &trust)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::trust::{SignerRecord, SignerScope};

    fn store(
        operator: &lillux::crypto::SigningKey,
        node: &lillux::crypto::SigningKey,
    ) -> TrustStore {
        let mut record = SignerRecord::new(
            &operator.verifying_key(),
            "operator",
            SignerRole::Operator,
            SignerScope::operator_default(),
            SignerStatus::Active,
        );
        record.event = "e".repeat(64);
        TrustStore::from_registry(node.verifying_key(), vec![record]).unwrap()
    }

    #[test]
    fn only_an_active_operator_signer_passes() {
        let node = lillux::crypto::SigningKey::from_bytes(&[1; 32]);
        let operator = lillux::crypto::SigningKey::from_bytes(&[2; 32]);
        let other = lillux::crypto::SigningKey::from_bytes(&[3; 32]);
        let trust = store(&operator, &node);
        let fp = |key: &lillux::crypto::SigningKey| {
            lillux::signature::compute_fingerprint(&key.verifying_key())
        };
        operator_key_required(&fp(&operator), &trust).unwrap();
        let refused = operator_key_required(&fp(&other), &trust)
            .unwrap_err()
            .to_string();
        assert!(refused.starts_with("operator_key_required"), "{refused}");
        assert!(refused.contains(&fp(&other)), "{refused}");
    }
}
