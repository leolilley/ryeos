//! Protected controller authority for one local external-candidate connector.
//!
//! This capability is deliberately separate from the remote supervisor's
//! attachment bootstrap.  It authenticates one controller-local companion
//! process to one retained external execution and grants no placement,
//! provider, signing, publication, or remote-channel authority.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use zeroize::Zeroizing;

const PHYSICAL_PREFIX: &str = "INTERNAL_PLACEMENT_VAULT_CONNECTOR_";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalConnectorCapabilityDocument {
    schema: u32,
    generation: String,
    capability: String,
}

/// Opaque app-owned coordinate. Only the external-session owner can construct
/// it from the exact retained connector generation.
pub struct ExternalConnectorCapabilityAccess {
    physical_key: String,
    generation: String,
}

impl ExternalConnectorCapabilityAccess {
    pub(crate) fn new(generation: &str) -> Result<Self> {
        validate_hash("external connector capability generation", generation)?;
        Ok(Self {
            physical_key: format!("{PHYSICAL_PREFIX}{generation}"),
            generation: generation.to_owned(),
        })
    }

    pub(super) fn physical_key(&self) -> &str {
        &self.physical_key
    }

    pub(crate) fn generate_value(&self) -> Result<String> {
        let capability = Zeroizing::new(lillux::crypto::generate_random_bytes::<32>());
        Ok(lillux::canonical_json(&serde_json::to_value(
            ExternalConnectorCapabilityDocument {
                schema: 1,
                generation: self.generation.clone(),
                capability: STANDARD.encode(capability.as_ref()),
            },
        )?)?)
    }

    fn decode_document(&self, value: &str) -> Result<ExternalConnectorCapabilityDocument> {
        ensure!(
            !value.is_empty() && value.len() <= 1024,
            "external connector capability document exceeds bounds"
        );
        let document: ExternalConnectorCapabilityDocument = serde_json::from_str(value)
            .context("decode protected external connector capability")?;
        ensure!(
            document.schema == 1 && document.generation == self.generation,
            "protected external connector capability contradicts its generation"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(&document)?)? == value,
            "protected external connector capability is not canonical"
        );
        decode_capability(&document.capability)?;
        Ok(document)
    }

    pub(crate) fn decode(&self, value: Zeroizing<String>) -> Result<ExternalConnectorCapability> {
        let document = self.decode_document(value.as_str())?;
        Ok(ExternalConnectorCapability {
            capability: Zeroizing::new(document.capability),
        })
    }

    pub(super) fn validate_value(&self, value: &str) -> Result<()> {
        self.decode_document(value).map(|_| ())
    }
}

/// Decrypted only while preparing or authenticating the exact local connector.
/// It deliberately implements neither `Debug`, serialization, nor cloning.
pub(crate) struct ExternalConnectorCapability {
    capability: Zeroizing<String>,
}

impl ExternalConnectorCapability {
    pub(crate) fn expose_for_connector_configuration(&self) -> &str {
        self.capability.as_str()
    }

    pub(crate) fn capability_hash(&self) -> Result<String> {
        let capability = decode_capability(self.capability.as_str())?;
        Ok(lillux::sha256_hex(capability.as_slice()))
    }

    pub(crate) fn authenticate_hash(&self, presented_hash: &str) -> Result<()> {
        validate_hash("presented external connector capability", presented_hash)?;
        let expected = self.capability_hash()?;
        ensure!(
            bool::from(expected.as_bytes().ct_eq(presented_hash.as_bytes())),
            "external connector capability is not authorized"
        );
        Ok(())
    }
}

fn decode_capability(value: &str) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        value.len() == 44,
        "external connector capability has the wrong encoded length"
    );
    let bytes = Zeroizing::new(
        STANDARD
            .decode(value)
            .context("decode external connector capability")?,
    );
    ensure!(
        bytes.len() == 32 && STANDARD.encode(bytes.as_slice()) == value,
        "external connector capability is not canonical"
    );
    Ok(bytes)
}

fn validate_hash(label: &str, value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a canonical SHA-256 digest"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{
        NodeVault, SealedEnvelopeVault, VaultScope, read_explicit_secret, read_named_secret,
        read_required_secrets,
    };

    #[test]
    fn capability_is_distinct_canonical_and_constant_time_authenticated() {
        let access = ExternalConnectorCapabilityAccess::new(&"a".repeat(64)).unwrap();
        let value = access.generate_value().unwrap();
        let capability = access.decode(Zeroizing::new(value)).unwrap();
        assert_eq!(capability.expose_for_connector_configuration().len(), 44);
        let hash = capability.capability_hash().unwrap();
        capability.authenticate_hash(&hash).unwrap();
        assert!(capability.authenticate_hash(&"b".repeat(64)).is_err());
    }

    #[test]
    fn sealed_generation_is_atomic_immutable_and_invisible() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store.enc");
        let key = lillux::vault::VaultSecretKey::generate();
        let vault = SealedEnvelopeVault::new(store_path.clone(), key.clone());
        let access = ExternalConnectorCapabilityAccess::new(&"a".repeat(64)).unwrap();
        let first = vault.ensure_external_connector_capability(&access).unwrap();
        let second = vault.ensure_external_connector_capability(&access).unwrap();
        assert_eq!(first.as_str(), second.as_str());
        let decoded = access.decode(second).unwrap();
        decoded
            .authenticate_hash(&decoded.capability_hash().unwrap())
            .unwrap();
        assert!(vault.read_all("operator").unwrap().is_empty());
        assert!(vault.list_keys("operator").unwrap().is_empty());
        assert!(read_named_secret(&vault, "operator", access.physical_key()).is_err());
        assert!(read_explicit_secret(&vault, "operator", access.physical_key(), &[]).is_err());
        assert!(
            read_required_secrets(&vault, "operator", &[access.physical_key().to_owned()], &[],)
                .is_err()
        );
        assert!(
            vault
                .get_scoped_secret(
                    &VaultScope::runtime_bundle("core", "fixture").unwrap(),
                    access.physical_key(),
                )
                .is_err()
        );
        assert!(
            vault
                .set_secret("operator", access.physical_key(), "forged")
                .is_err()
        );
        assert!(
            vault
                .delete_secret("operator", access.physical_key())
                .is_err()
        );
        assert!(
            vault
                .provision_external_connector_capability(
                    &access,
                    &access.generate_value().unwrap(),
                )
                .is_err()
        );
        assert_eq!(
            vault
                .external_connector_capability(&access)
                .unwrap()
                .as_str(),
            first.as_str()
        );

        drop(vault);
        let reopened = SealedEnvelopeVault::new(store_path, key);
        assert_eq!(
            reopened
                .external_connector_capability(&access)
                .unwrap()
                .as_str(),
            first.as_str()
        );
    }

    #[test]
    fn invalid_coordinates_documents_and_unsupported_backends_refuse() {
        for generation in [
            "".to_owned(),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(63),
        ] {
            assert!(ExternalConnectorCapabilityAccess::new(&generation).is_err());
        }
        let access = ExternalConnectorCapabilityAccess::new(&"a".repeat(64)).unwrap();
        let value = access.generate_value().unwrap();
        let parsed: ExternalConnectorCapabilityDocument = serde_json::from_str(&value).unwrap();
        let mut document: serde_json::Value = serde_json::from_str(&value).unwrap();
        document["extra"] = serde_json::json!(true);
        assert!(
            access
                .decode(Zeroizing::new(serde_json::to_string(&document).unwrap()))
                .is_err()
        );
        for changed in [
            ExternalConnectorCapabilityDocument {
                schema: 2,
                generation: parsed.generation.clone(),
                capability: parsed.capability.clone(),
            },
            ExternalConnectorCapabilityDocument {
                schema: 1,
                generation: "b".repeat(64),
                capability: parsed.capability.clone(),
            },
            ExternalConnectorCapabilityDocument {
                schema: 1,
                generation: parsed.generation.clone(),
                capability: STANDARD.encode([7_u8; 31]),
            },
            ExternalConnectorCapabilityDocument {
                schema: 1,
                generation: parsed.generation.clone(),
                capability: "!".repeat(44),
            },
        ] {
            let changed = lillux::canonical_json(&serde_json::to_value(changed).unwrap()).unwrap();
            assert!(access.decode(Zeroizing::new(changed)).is_err());
        }
        assert!(access.decode(Zeroizing::new(format!(" {value}"))).is_err());

        let dir = tempfile::tempdir().unwrap();
        let vault = SealedEnvelopeVault::new(
            dir.path().join("store.enc"),
            lillux::vault::VaultSecretKey::generate(),
        );
        let invalid = lillux::canonical_json(
            &serde_json::to_value(ExternalConnectorCapabilityDocument {
                schema: 1,
                generation: "b".repeat(64),
                capability: parsed.capability,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            vault
                .provision_external_connector_capability(&access, &invalid)
                .is_err()
        );
        assert!(vault.external_connector_capability(&access).is_err());
        assert!(
            crate::vault::EmptyVault
                .external_connector_capability(&access)
                .is_err()
        );
        assert!(
            crate::vault::EmptyVault
                .provision_external_connector_capability(&access, &value)
                .is_err()
        );
        assert!(
            crate::vault::EmptyVault
                .ensure_external_connector_capability(&access)
                .is_err()
        );
    }

    #[test]
    fn concurrent_ensure_selects_one_immutable_capability() {
        let dir = tempfile::tempdir().unwrap();
        let vault = std::sync::Arc::new(SealedEnvelopeVault::new(
            dir.path().join("store.enc"),
            lillux::vault::VaultSecretKey::generate(),
        ));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let vault = vault.clone();
            handles.push(std::thread::spawn(move || {
                let access = ExternalConnectorCapabilityAccess::new(&"a".repeat(64)).unwrap();
                let value = vault.ensure_external_connector_capability(&access).unwrap();
                lillux::sha256_hex(value.as_bytes())
            }));
        }
        let digests: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(digests.iter().all(|digest| digest == &digests[0]));
        assert!(vault.read_all("operator").unwrap().is_empty());
    }
}
