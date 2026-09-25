//! App-owned lifecycle credentials, separate from provider and workload scopes.
//! A coordinate is not allocation authority. Only the protected placement owner
//! may use it, after checking installed binding and retained cleanup obligations.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlacementCredentialDocument {
    schema: u32,
    backend: String,
    account: String,
    generation: String,
    secret: String,
}

/// Opaque to other crates and runtime callbacks. No serde or public constructor.
/// This type contains no plaintext and cannot select operator/runtime secrets.
pub struct PlacementCredentialAccess {
    physical_key: String,
    backend: String,
    account: String,
    generation: String,
}

impl PlacementCredentialAccess {
    pub(crate) fn new(owner: &str, generation: &str, backend: &str, account: &str) -> Result<Self> {
        for value in [owner, generation] {
            ensure!(
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "placement credential requires canonical owner and generation hashes"
            );
        }
        for value in [backend, account] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
                "placement credential backend/account is invalid"
            );
        }
        Ok(Self {
            physical_key: format!(
                "{}{owner}_{generation}",
                ryeos_vault::policy::INTERNAL_PLACEMENT_VAULT_PREFIX
            ),
            backend: backend.to_owned(),
            account: account.to_owned(),
            generation: generation.to_owned(),
        })
    }

    pub(super) fn physical_key(&self) -> &str {
        &self.physical_key
    }

    fn decode_document(&self, value: &str) -> Result<PlacementCredentialDocument> {
        ensure!(
            !value.is_empty() && value.len() <= 256 * 1024,
            "placement credential document exceeds bounds"
        );
        let document: PlacementCredentialDocument =
            serde_json::from_str(value).context("decode protected placement credential")?;
        ensure!(
            document.schema == 1
                && document.backend == self.backend
                && document.account == self.account
                && document.generation == self.generation,
            "protected placement credential contradicts its binding"
        );
        ensure!(
            !document.secret.is_empty() && document.secret.len() <= 128 * 1024,
            "protected placement credential secret exceeds bounds"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(&document)?)? == value,
            "protected placement credential is not canonical"
        );
        Ok(document)
    }

    pub(crate) fn decode(&self, value: Zeroizing<String>) -> Result<PlacementCredential> {
        let document = self.decode_document(value.as_str())?;
        Ok(PlacementCredential {
            backend: document.backend,
            account: document.account,
            secret: Zeroizing::new(document.secret),
        })
    }

    pub(super) fn validate_value(&self, value: &str) -> Result<()> {
        self.decode_document(value).map(|_| ())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn test_value(&self, secret: &str) -> String {
        lillux::canonical_json(
            &serde_json::to_value(PlacementCredentialDocument {
                schema: 1,
                backend: self.backend.clone(),
                account: self.account.clone(),
                generation: self.generation.clone(),
                secret: secret.to_owned(),
            })
            .unwrap(),
        )
        .unwrap()
    }
}

/// Decrypted only within the protected placement owner. It has no serialization
/// or Debug representation and zeroizes the actual provider secret on drop.
pub(crate) struct PlacementCredential {
    backend: String,
    account: String,
    secret: Zeroizing<String>,
}

impl PlacementCredential {
    pub(crate) fn backend(&self) -> &str {
        &self.backend
    }
    pub(crate) fn account(&self) -> &str {
        &self.account
    }
    pub(crate) fn secret(&self) -> &str {
        self.secret.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{
        NodeVault, SealedEnvelopeVault, VaultScope, read_explicit_secret, read_named_secret,
        read_required_secrets,
    };

    #[test]
    fn placement_generations_are_private_immutable_and_survive_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let key = lillux::vault::VaultSecretKey::generate();
        let vault = SealedEnvelopeVault::new(dir.path().join("store.enc"), key.clone());
        let old =
            PlacementCredentialAccess::new(&"a".repeat(64), &"b".repeat(64), "fixture", "account")
                .unwrap();
        let new =
            PlacementCredentialAccess::new(&"a".repeat(64), &"c".repeat(64), "fixture", "account")
                .unwrap();
        assert!(vault.placement_credential(&old).is_err());
        let old_value = old.test_value("old-secret");
        let new_value = new.test_value("new-secret");
        vault
            .provision_placement_credential(&old, &old_value)
            .unwrap();
        vault
            .provision_placement_credential(&old, &old_value)
            .unwrap();
        assert!(
            vault
                .provision_placement_credential(&old, &old.test_value("changed"))
                .is_err()
        );
        vault
            .provision_placement_credential(&new, &new_value)
            .unwrap();
        drop(vault);
        let vault = SealedEnvelopeVault::new(dir.path().join("store.enc"), key);
        assert_eq!(
            old.decode(vault.placement_credential(&old).unwrap())
                .unwrap()
                .secret(),
            "old-secret"
        );
        assert_eq!(
            new.decode(vault.placement_credential(&new).unwrap())
                .unwrap()
                .secret(),
            "new-secret"
        );
        assert!(vault.read_all("operator").unwrap().is_empty());
        assert!(vault.list_keys("operator").unwrap().is_empty());
        let name = old.physical_key();
        assert!(vault.set_secret("operator", name, "overwrite").is_err());
        assert!(vault.delete_secret("operator", name).is_err());
        assert!(read_named_secret(&vault, "operator", name).is_err());
        assert!(read_explicit_secret(&vault, "operator", name, &[]).is_err());
        assert!(read_required_secrets(&vault, "operator", &[name.to_owned()], &[]).is_err());
        let runtime = VaultScope::runtime_bundle("codex", "placement").unwrap();
        assert!(vault.get_scoped_secret(&runtime, name).is_err());
        assert!(vault.list_scoped_secret_keys(&runtime).unwrap().is_empty());
        assert!(ryeos_vault::policy::validate_key_name(name).is_err());
    }

    #[test]
    fn placement_invalid_coordinates_and_unsupported_backends_refuse() {
        for bad in [
            "".to_owned(),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(63),
        ] {
            assert!(
                PlacementCredentialAccess::new(&bad, &"b".repeat(64), "fixture", "account")
                    .is_err()
            );
            assert!(
                PlacementCredentialAccess::new(&"a".repeat(64), &bad, "fixture", "account")
                    .is_err()
            );
        }
        let access =
            PlacementCredentialAccess::new(&"a".repeat(64), &"b".repeat(64), "fixture", "account")
                .unwrap();
        assert!(
            crate::vault::EmptyVault
                .placement_credential(&access)
                .is_err()
        );
        assert!(
            crate::vault::EmptyVault
                .provision_placement_credential(&access, "secret")
                .is_err()
        );
        assert!(access.decode(Zeroizing::new("{}".into())).is_err());
        let wrong_account =
            PlacementCredentialAccess::new(&"a".repeat(64), &"b".repeat(64), "fixture", "other")
                .unwrap();
        assert!(
            wrong_account
                .decode(Zeroizing::new(access.test_value("secret")))
                .is_err()
        );
        let canonical = access.test_value("secret");
        assert!(
            access
                .decode(Zeroizing::new(format!(" {canonical}")))
                .is_err()
        );
        for changed in [
            serde_json::json!({
                "schema":2, "backend":"fixture", "account":"account",
                "generation":"b".repeat(64), "secret":"secret"
            }),
            serde_json::json!({
                "schema":1, "backend":"other", "account":"account",
                "generation":"b".repeat(64), "secret":"secret"
            }),
            serde_json::json!({
                "schema":1, "backend":"fixture", "account":"account",
                "generation":"c".repeat(64), "secret":"secret"
            }),
            serde_json::json!({
                "schema":1, "backend":"fixture", "account":"account",
                "generation":"b".repeat(64), "secret":""
            }),
            serde_json::json!({
                "schema":1, "backend":"fixture", "account":"account",
                "generation":"b".repeat(64), "secret":"secret", "extra":true
            }),
        ] {
            let value = lillux::canonical_json(&changed).unwrap();
            assert!(access.decode(Zeroizing::new(value)).is_err());
        }
        let duplicate = format!(
            "{{\"account\":\"account\",\"backend\":\"fixture\",\"generation\":\"{}\",\"schema\":1,\"schema\":1,\"secret\":\"secret\"}}",
            "b".repeat(64)
        );
        assert!(access.decode(Zeroizing::new(duplicate)).is_err());
        let oversized = access.test_value(&"x".repeat(128 * 1024 + 1));
        assert!(access.decode(Zeroizing::new(oversized)).is_err());
    }
}
