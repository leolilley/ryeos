//! Crash-recoverable coordination for installed bundle trees and registrations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lillux::crypto::SigningKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredBundleState {
    Present { registration: serde_json::Value },
    Absent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    desired: DesiredBundleState,
}

pub struct BundleTransaction {
    app_root: PathBuf,
    name: String,
    target: PathBuf,
    journal: PathBuf,
    _lock: lillux::ExclusiveFileLock,
}

impl BundleTransaction {
    pub fn acquire(app_root: &Path, name: &str) -> Result<Self> {
        let target = app_root.join(".ai/bundles").join(name);
        let journal = app_root
            .join(".ai/node/bundle-transactions")
            .join(format!("{name}.json"));
        let lock = lillux::ExclusiveFileLock::acquire(&target)?;
        Ok(Self {
            app_root: app_root.to_path_buf(),
            name: name.to_string(),
            target,
            journal,
            _lock: lock,
        })
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn reconcile(&self, signing_key: &SigningKey) -> Result<Option<DesiredBundleState>> {
        if !self.journal.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&self.journal)
            .with_context(|| format!("read bundle transaction {}", self.journal.display()))?;
        let journal: Journal = serde_json::from_slice(&raw)
            .with_context(|| format!("parse bundle transaction {}", self.journal.display()))?;
        if journal.version != 1 {
            anyhow::bail!("unsupported bundle transaction version {}", journal.version);
        }
        let desired = journal.desired.clone();
        match journal.desired {
            DesiredBundleState::Present { registration } if self.target.is_dir() => {
                self.write_registration(&registration, signing_key)?;
            }
            DesiredBundleState::Present { .. } | DesiredBundleState::Absent => {
                self.remove_registration()?;
                lillux::remove_dir_all_durable(&self.target)?;
            }
        }
        self.finish()?;
        Ok(Some(desired))
    }

    pub fn begin(&self, desired: DesiredBundleState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&Journal {
            version: 1,
            desired,
        })?;
        lillux::atomic_write_private(&self.journal, &bytes)
            .with_context(|| format!("write bundle transaction {}", self.journal.display()))
    }

    pub fn commit_present(
        &self,
        registration: &serde_json::Value,
        signing_key: &SigningKey,
    ) -> Result<PathBuf> {
        let path = self.write_registration(registration, signing_key)?;
        self.finish()?;
        Ok(path)
    }

    pub fn commit_absent(&self) -> Result<()> {
        self.remove_registration()?;
        lillux::remove_dir_all_durable(&self.target)?;
        self.finish()
    }

    fn write_registration(
        &self,
        registration: &serde_json::Value,
        signing_key: &SigningKey,
    ) -> Result<PathBuf> {
        let yaml = serde_yaml::to_string(registration)?;
        let signed = lillux::signature::sign_content(&yaml, signing_key, "#", None);
        let path = self.registration_path();
        lillux::atomic_write_private(&path, signed.as_bytes())?;
        Ok(path)
    }

    fn remove_registration(&self) -> Result<()> {
        lillux::remove_file_durable(&self.registration_path())
    }

    fn registration_path(&self) -> PathBuf {
        self.app_root
            .join(".ai/node/bundles")
            .join(format!("{}.yaml", self.name))
    }

    fn finish(&self) -> Result<()> {
        lillux::remove_file_durable(&self.journal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn reconcile_present_finishes_registration_after_tree_activation() {
        let root = tempfile::tempdir().unwrap();
        let key = SigningKey::generate(&mut OsRng);
        let transaction = BundleTransaction::acquire(root.path(), "demo").unwrap();
        let registration = serde_json::json!({ "path": transaction.target() });
        transaction
            .begin(DesiredBundleState::Present {
                registration: registration.clone(),
            })
            .unwrap();
        std::fs::create_dir_all(transaction.target().join(".ai")).unwrap();

        transaction.reconcile(&key).unwrap();

        assert!(root.path().join(".ai/node/bundles/demo.yaml").is_file());
        assert!(!transaction.journal.exists());
    }

    #[test]
    fn reconcile_absent_removes_both_halves_idempotently() {
        let root = tempfile::tempdir().unwrap();
        let key = SigningKey::generate(&mut OsRng);
        let transaction = BundleTransaction::acquire(root.path(), "demo").unwrap();
        std::fs::create_dir_all(transaction.target()).unwrap();
        transaction
            .write_registration(
                &serde_json::json!({ "path": transaction.target() }),
                &key,
            )
            .unwrap();
        transaction.begin(DesiredBundleState::Absent).unwrap();

        transaction.reconcile(&key).unwrap();

        assert!(!transaction.target().exists());
        assert!(!root.path().join(".ai/node/bundles/demo.yaml").exists());
        assert!(!transaction.journal.exists());
    }
}
