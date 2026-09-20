//! App-owned lifecycle credentials, separate from provider and workload scopes.
//! A coordinate is not allocation authority. Only the protected placement owner
//! may use it, after checking installed binding and retained cleanup obligations.

use anyhow::{Result, ensure};

/// Opaque to other crates and runtime callbacks. No serde or public constructor.
/// This type contains no plaintext and cannot select operator/runtime secrets.
pub struct PlacementCredentialAccess {
    physical_key: String,
}

impl PlacementCredentialAccess {
    pub(crate) fn new(owner: &str, generation: &str) -> Result<Self> {
        for value in [owner, generation] {
            ensure!(
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "placement credential requires canonical owner and generation hashes"
            );
        }
        Ok(Self {
            physical_key: format!(
                "{}{owner}_{generation}",
                ryeos_vault::policy::INTERNAL_PLACEMENT_VAULT_PREFIX
            ),
        })
    }

    pub(super) fn physical_key(&self) -> &str {
        &self.physical_key
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
        let old = PlacementCredentialAccess::new(&"a".repeat(64), &"b".repeat(64)).unwrap();
        let new = PlacementCredentialAccess::new(&"a".repeat(64), &"c".repeat(64)).unwrap();
        assert!(vault.placement_credential(&old).is_err());
        vault
            .provision_placement_credential(&old, "old-secret")
            .unwrap();
        vault
            .provision_placement_credential(&old, "old-secret")
            .unwrap();
        assert!(
            vault
                .provision_placement_credential(&old, "changed")
                .is_err()
        );
        vault
            .provision_placement_credential(&new, "new-secret")
            .unwrap();
        drop(vault);
        let vault = SealedEnvelopeVault::new(dir.path().join("store.enc"), key);
        assert_eq!(
            vault.placement_credential(&old).unwrap().as_str(),
            "old-secret"
        );
        assert_eq!(
            vault.placement_credential(&new).unwrap().as_str(),
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
            assert!(PlacementCredentialAccess::new(&bad, &"b".repeat(64)).is_err());
            assert!(PlacementCredentialAccess::new(&"a".repeat(64), &bad).is_err());
        }
        let access = PlacementCredentialAccess::new(&"a".repeat(64), &"b".repeat(64)).unwrap();
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
    }
}
