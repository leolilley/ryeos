//! Protected controller authority for one external-execution occurrence.
//!
//! The controller signer and the one-use attachment capability are sealed in
//! the node vault before allocator contact.  The supervisor generates its own
//! distinct signer inside the protected occurrence; its private key never
//! enters this store.  Candidate inputs can name neither this coordinate nor
//! any plaintext it contains.

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const PHYSICAL_PREFIX: &str = "INTERNAL_PLACEMENT_VAULT_CHANNEL_";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalChannelAuthorityDocument {
    schema: u32,
    generation: String,
    owner_signing_key: String,
    bootstrap_capability: String,
}

/// Opaque app-owned coordinate.  Only the placement owner can construct it.
pub struct ExternalChannelAuthorityAccess {
    physical_key: String,
    generation: String,
}

impl ExternalChannelAuthorityAccess {
    pub(crate) fn new(generation: &str) -> Result<Self> {
        validate_hash("external channel authority generation", generation)?;
        Ok(Self {
            physical_key: format!("{PHYSICAL_PREFIX}{generation}"),
            generation: generation.to_owned(),
        })
    }

    pub(super) fn physical_key(&self) -> &str {
        &self.physical_key
    }

    pub(crate) fn generate_value(&self) -> Result<String> {
        let owner = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
        let mut bootstrap = Zeroizing::new([0_u8; 32]);
        rand::rngs::OsRng.fill_bytes(bootstrap.as_mut());
        Ok(lillux::canonical_json(&serde_json::to_value(
            ExternalChannelAuthorityDocument {
                schema: 1,
                generation: self.generation.clone(),
                owner_signing_key: STANDARD.encode(owner.to_bytes()),
                bootstrap_capability: STANDARD.encode(bootstrap.as_ref()),
            },
        )?)?)
    }

    fn decode_document(&self, value: &str) -> Result<ExternalChannelAuthorityDocument> {
        ensure!(
            !value.is_empty() && value.len() <= 4096,
            "external channel authority document exceeds bounds"
        );
        let document: ExternalChannelAuthorityDocument =
            serde_json::from_str(value).context("decode protected external channel authority")?;
        ensure!(
            document.schema == 1 && document.generation == self.generation,
            "protected external channel authority contradicts its generation"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(&document)?)? == value,
            "protected external channel authority is not canonical"
        );
        decode_32(
            "external channel owner signing key",
            &document.owner_signing_key,
        )?;
        decode_32(
            "external channel bootstrap capability",
            &document.bootstrap_capability,
        )?;
        Ok(document)
    }

    pub(crate) fn decode(&self, value: Zeroizing<String>) -> Result<ExternalChannelAuthority> {
        let document = self.decode_document(value.as_str())?;
        let owner_bytes: [u8; 32] = decode_32(
            "external channel owner signing key",
            &document.owner_signing_key,
        )?;
        let owner_signing_key = lillux::crypto::SigningKey::from_bytes(&owner_bytes);
        ensure!(
            !owner_signing_key.verifying_key().is_weak(),
            "external channel owner signing key is weak"
        );
        Ok(ExternalChannelAuthority {
            generation: document.generation,
            owner_signing_key,
            bootstrap_capability: Zeroizing::new(document.bootstrap_capability),
        })
    }

    pub(super) fn validate_value(&self, value: &str) -> Result<()> {
        self.decode_document(value).map(|_| ())
    }
}

/// Decrypted only inside the protected placement/transport owner.  This type
/// deliberately implements neither `Debug`, serialization nor cloning.
pub(crate) struct ExternalChannelAuthority {
    generation: String,
    owner_signing_key: lillux::crypto::SigningKey,
    bootstrap_capability: Zeroizing<String>,
}

impl ExternalChannelAuthority {
    pub(crate) fn generation(&self) -> &str {
        &self.generation
    }

    pub(crate) fn owner_signing_key(&self) -> &lillux::crypto::SigningKey {
        &self.owner_signing_key
    }

    pub(crate) fn owner_public_key(&self) -> String {
        STANDARD.encode(self.owner_signing_key.verifying_key().to_bytes())
    }

    pub(crate) fn bootstrap_capability(&self) -> &str {
        self.bootstrap_capability.as_str()
    }

    pub(crate) fn bootstrap_capability_hash(&self) -> String {
        lillux::sha256_hex(self.bootstrap_capability.as_bytes())
    }

    #[cfg(test)]
    pub(crate) fn test_fixture(generation: &str) -> Self {
        Self {
            generation: generation.to_owned(),
            owner_signing_key: lillux::crypto::SigningKey::from_bytes(&[19; 32]),
            bootstrap_capability: Zeroizing::new(STANDARD.encode([23_u8; 32])),
        }
    }
}

fn decode_32(label: &str, value: &str) -> Result<[u8; 32]> {
    ensure!(value.len() == 44, "{label} has the wrong encoded length");
    let bytes = STANDARD
        .decode(value)
        .with_context(|| format!("decode {label}"))?;
    ensure!(STANDARD.encode(&bytes) == value, "{label} is not canonical");
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{label} has the wrong decoded length"))
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
    use crate::vault::{NodeVault, SealedEnvelopeVault};

    #[test]
    fn authority_is_canonical_distinct_and_bound_to_generation() {
        let first = ExternalChannelAuthorityAccess::new(&"a".repeat(64)).unwrap();
        let second = ExternalChannelAuthorityAccess::new(&"b".repeat(64)).unwrap();
        let value = first.generate_value().unwrap();
        let decoded = first.decode(Zeroizing::new(value.clone())).unwrap();
        assert_eq!(decoded.generation(), "a".repeat(64));
        assert_eq!(decoded.owner_public_key().len(), 44);
        assert_eq!(decoded.bootstrap_capability().len(), 44);
        assert!(lillux::valid_hash(&decoded.bootstrap_capability_hash()));
        assert!(second.decode(Zeroizing::new(value)).is_err());
        assert_ne!(decoded.owner_public_key(), STANDARD.encode([0_u8; 32]));
    }

    #[test]
    fn invalid_coordinates_and_noncanonical_documents_refuse() {
        for generation in [
            "".to_owned(),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(63),
        ] {
            assert!(ExternalChannelAuthorityAccess::new(&generation).is_err());
        }
        let access = ExternalChannelAuthorityAccess::new(&"a".repeat(64)).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&access.generate_value().unwrap()).unwrap();
        value["extra"] = serde_json::json!(true);
        assert!(
            access
                .decode(Zeroizing::new(serde_json::to_string(&value).unwrap()))
                .is_err()
        );
    }

    #[test]
    fn sealed_generation_is_atomic_immutable_and_operator_invisible() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store.enc");
        let secret_key = lillux::vault::VaultSecretKey::generate();
        let vault = SealedEnvelopeVault::new(store_path.clone(), secret_key.clone());
        let access = ExternalChannelAuthorityAccess::new(&"a".repeat(64)).unwrap();
        let first = vault.ensure_external_channel_authority(&access).unwrap();
        let second = vault.ensure_external_channel_authority(&access).unwrap();
        assert_eq!(first.as_str(), second.as_str());
        let decoded = access.decode(second).unwrap();
        assert!(lillux::valid_hash(&decoded.bootstrap_capability_hash()));
        assert!(vault.read_all("operator").unwrap().is_empty());
        assert!(vault.list_keys("operator").unwrap().is_empty());
        assert!(
            vault
                .get_scoped_secret(
                    &crate::vault::VaultScope::operator_env("operator"),
                    access.physical_key(),
                )
                .is_err()
        );
        assert!(
            vault
                .get_scoped_secret(
                    &crate::vault::VaultScope::runtime_bundle("core", "fixture").unwrap(),
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

        let different = access.generate_value().unwrap();
        assert!(
            vault
                .provision_external_channel_authority(&access, &different)
                .is_err()
        );
        assert_eq!(
            vault.external_channel_authority(&access).unwrap().as_str(),
            first.as_str()
        );

        drop(vault);
        let reopened = SealedEnvelopeVault::new(store_path, secret_key);
        assert_eq!(
            reopened
                .external_channel_authority(&access)
                .unwrap()
                .as_str(),
            first.as_str()
        );
    }

    #[test]
    fn concurrent_ensure_selects_one_immutable_authority() {
        let dir = tempfile::tempdir().unwrap();
        let vault = std::sync::Arc::new(SealedEnvelopeVault::new(
            dir.path().join("store.enc"),
            lillux::vault::VaultSecretKey::generate(),
        ));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let vault = vault.clone();
            handles.push(std::thread::spawn(move || {
                let access = ExternalChannelAuthorityAccess::new(&"a".repeat(64)).unwrap();
                let value = vault.ensure_external_channel_authority(&access).unwrap();
                lillux::sha256_hex(value.as_bytes())
            }));
        }
        let digests: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(digests.iter().all(|digest| digest == &digests[0]));
        assert_eq!(
            vault.read_all("operator").unwrap(),
            std::collections::HashMap::new()
        );
    }
}
