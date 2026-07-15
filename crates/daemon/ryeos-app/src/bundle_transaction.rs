//! Crash-recoverable coordination for installed bundle trees and registrations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lillux::crypto::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleOperation {
    Install,
    RemoteInstall,
    Replace,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundlePhase {
    Prepared,
    Activated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    bundle_name: String,
    operation: BundleOperation,
    phase: BundlePhase,
    target_path: PathBuf,
    staging_path: Option<PathBuf>,
    registration_path: PathBuf,
    generation_digest: Option<String>,
    registration_digest: Option<String>,
    registration: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleTransactionDiagnostics {
    pub pending: Vec<String>,
    pub invalid: Vec<String>,
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
        if !valid_bundle_name(name) {
            anyhow::bail!("invalid bundle transaction name: {name}");
        }
        let target = app_root.join(".ai/bundles").join(name);
        let journal = journal_directory(app_root).join(format!("{name}.json"));
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

    pub fn begin_present(
        &self,
        operation: BundleOperation,
        staging: &Path,
        registration: serde_json::Value,
    ) -> Result<()> {
        if !matches!(
            operation,
            BundleOperation::Install | BundleOperation::RemoteInstall | BundleOperation::Replace
        ) {
            anyhow::bail!("present transaction requires an install or replace operation");
        }
        let expected_staging = self.staging_path(operation)?;
        if staging != expected_staging {
            anyhow::bail!(
                "bundle staging path {} does not match derived path {}",
                staging.display(),
                expected_staging.display()
            );
        }
        let generation_digest = tree_digest(staging)?;
        let registration_digest = registration_digest(&registration)?;
        self.write_journal(&Journal {
            bundle_name: self.name.clone(),
            operation,
            phase: BundlePhase::Prepared,
            target_path: self.target.clone(),
            staging_path: Some(expected_staging),
            registration_path: self.registration_path(),
            generation_digest: Some(generation_digest),
            registration_digest: Some(registration_digest),
            registration: Some(registration),
        })
    }

    pub fn begin_remove(&self) -> Result<()> {
        self.write_journal(&Journal {
            bundle_name: self.name.clone(),
            operation: BundleOperation::Remove,
            phase: BundlePhase::Prepared,
            target_path: self.target.clone(),
            staging_path: None,
            registration_path: self.registration_path(),
            generation_digest: None,
            registration_digest: None,
            registration: None,
        })
    }

    pub fn mark_activated(&self) -> Result<()> {
        let mut journal = self.read_validated_journal()?;
        if journal.operation == BundleOperation::Remove {
            anyhow::bail!("remove transaction has no tree activation phase");
        }
        let expected = journal
            .generation_digest
            .as_deref()
            .context("present transaction missing generation digest")?;
        if tree_digest(&self.target)? != expected {
            anyhow::bail!("activated bundle generation digest does not match journal");
        }
        journal.phase = BundlePhase::Activated;
        self.write_journal(&journal)
    }

    pub fn commit_present(&self, signing_key: &SigningKey) -> Result<PathBuf> {
        let journal = self.read_validated_journal()?;
        if journal.phase != BundlePhase::Activated {
            anyhow::bail!("cannot commit a bundle transaction before activation");
        }
        self.complete_present(&journal, signing_key)?;
        self.finish()?;
        Ok(self.registration_path())
    }

    pub fn commit_absent(&self) -> Result<()> {
        let journal = self.read_validated_journal()?;
        if journal.operation != BundleOperation::Remove {
            anyhow::bail!("commit_absent requires a remove transaction");
        }
        self.remove_registration()?;
        lillux::remove_dir_all_durable(&self.target)?;
        self.finish()
    }

    pub fn reconcile(&self, signing_key: &SigningKey) -> Result<Option<BundleOperation>> {
        if !self.journal.exists() {
            return Ok(None);
        }
        let journal = self.read_validated_journal()?;
        match journal.operation {
            BundleOperation::Remove => {
                self.remove_registration()?;
                lillux::remove_dir_all_durable(&self.target)?;
            }
            operation => {
                let expected = journal
                    .generation_digest
                    .as_deref()
                    .context("present transaction missing generation digest")?;
                let target_matches = self.target.is_dir() && tree_digest(&self.target)? == expected;
                if target_matches {
                    self.complete_present(&journal, signing_key)?;
                } else if journal.phase == BundlePhase::Activated {
                    anyhow::bail!("activated bundle transaction target digest mismatch");
                } else if operation == BundleOperation::Replace {
                    // Prepared replacement did not activate: preserve the old complete
                    // registered generation and discard only the staged candidate.
                } else {
                    self.remove_registration()?;
                    lillux::remove_dir_all_durable(&self.target)?;
                }
                if let Some(staging) = &journal.staging_path {
                    lillux::remove_dir_all_durable(staging)?;
                }
            }
        }
        self.finish()?;
        Ok(Some(journal.operation))
    }

    fn complete_present(&self, journal: &Journal, signing_key: &SigningKey) -> Result<()> {
        let expected = journal
            .generation_digest
            .as_deref()
            .context("present transaction missing generation digest")?;
        if tree_digest(&self.target)? != expected {
            anyhow::bail!("installed bundle generation digest does not match journal");
        }
        let registration = journal
            .registration
            .as_ref()
            .context("present transaction missing registration")?;
        if registration_digest(registration)?
            != journal
                .registration_digest
                .as_deref()
                .context("present transaction missing registration digest")?
        {
            anyhow::bail!("bundle registration digest does not match journal");
        }
        self.write_registration(registration, signing_key)?;
        Ok(())
    }

    fn read_validated_journal(&self) -> Result<Journal> {
        let raw = std::fs::read(&self.journal)
            .with_context(|| format!("read bundle transaction {}", self.journal.display()))?;
        let journal: Journal = serde_json::from_slice(&raw)
            .with_context(|| format!("parse bundle transaction {}", self.journal.display()))?;
        validate_journal(&self.app_root, &self.name, &journal)?;
        Ok(journal)
    }

    fn write_journal(&self, journal: &Journal) -> Result<()> {
        validate_journal(&self.app_root, &self.name, journal)?;
        let bytes = serde_json::to_vec_pretty(journal)?;
        lillux::atomic_write_private(&self.journal, &bytes)
            .with_context(|| format!("write bundle transaction {}", self.journal.display()))
    }

    fn staging_path(&self, operation: BundleOperation) -> Result<PathBuf> {
        let parent = self
            .target
            .parent()
            .context("bundle target has no parent")?;
        match operation {
            BundleOperation::Install | BundleOperation::Replace => {
                Ok(parent.join(format!(".{}.staging", self.name)))
            }
            BundleOperation::RemoteInstall => {
                Ok(parent.join(format!(".{}.remote-staging", self.name)))
            }
            BundleOperation::Remove => anyhow::bail!("remove has no staging path"),
        }
    }

    fn write_registration(
        &self,
        registration: &serde_json::Value,
        signing_key: &SigningKey,
    ) -> Result<()> {
        let yaml = serde_yaml::to_string(registration)?;
        let signed = lillux::signature::sign_content(&yaml, signing_key, "#", None);
        lillux::atomic_write_private(&self.registration_path(), signed.as_bytes())?;
        Ok(())
    }

    fn remove_registration(&self) -> Result<()> {
        Ok(lillux::remove_file_durable(&self.registration_path())?)
    }

    fn registration_path(&self) -> PathBuf {
        self.app_root
            .join(".ai/node/bundles")
            .join(format!("{}.yaml", self.name))
    }

    fn finish(&self) -> Result<()> {
        Ok(lillux::remove_file_durable(&self.journal)?)
    }
}

pub fn inspect_bundle_transactions(app_root: &Path) -> Result<BundleTransactionDiagnostics> {
    let directory = journal_directory(app_root);
    if !directory.exists() {
        return Ok(BundleTransactionDiagnostics {
            pending: Vec::new(),
            invalid: Vec::new(),
        });
    }
    let mut pending = Vec::new();
    let mut invalid = Vec::new();
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(bundle_name) = name.strip_suffix(".json") else {
            invalid.push(name);
            continue;
        };
        let parsed = if entry.file_type()?.is_file() && valid_bundle_name(bundle_name) {
            std::fs::read(entry.path())
                .ok()
                .and_then(|raw| serde_json::from_slice::<Journal>(&raw).ok())
                .filter(|journal| validate_journal(app_root, bundle_name, journal).is_ok())
        } else {
            None
        };
        if parsed.is_some() {
            pending.push(bundle_name.to_string());
        } else {
            invalid.push(name);
        }
    }
    pending.sort();
    invalid.sort();
    Ok(BundleTransactionDiagnostics { pending, invalid })
}

pub fn reconcile_all_bundle_transactions(
    app_root: &Path,
    signing_key: &SigningKey,
) -> Result<Vec<String>> {
    let diagnostics = inspect_bundle_transactions(app_root)?;
    if !diagnostics.invalid.is_empty() {
        anyhow::bail!(
            "invalid bundle transaction journals: {}",
            diagnostics.invalid.join(", ")
        );
    }
    for name in &diagnostics.pending {
        BundleTransaction::acquire(app_root, name)?.reconcile(signing_key)?;
    }
    Ok(diagnostics.pending)
}

fn validate_journal(app_root: &Path, name: &str, journal: &Journal) -> Result<()> {
    if journal.bundle_name != name || !valid_bundle_name(&journal.bundle_name) {
        anyhow::bail!("bundle transaction name mismatch");
    }
    let transaction_target = app_root.join(".ai/bundles").join(name);
    let registration = app_root
        .join(".ai/node/bundles")
        .join(format!("{name}.yaml"));
    if journal.target_path != transaction_target || journal.registration_path != registration {
        anyhow::bail!("bundle transaction contains non-derived paths");
    }
    let expected_staging = match journal.operation {
        BundleOperation::Install | BundleOperation::Replace => Some(
            app_root
                .join(".ai/bundles")
                .join(format!(".{name}.staging")),
        ),
        BundleOperation::RemoteInstall => Some(
            app_root
                .join(".ai/bundles")
                .join(format!(".{name}.remote-staging")),
        ),
        BundleOperation::Remove => None,
    };
    if journal.staging_path != expected_staging {
        anyhow::bail!("bundle transaction staging path mismatch");
    }
    let present = journal.operation != BundleOperation::Remove;
    if !present && journal.phase != BundlePhase::Prepared {
        anyhow::bail!("remove transaction has an invalid phase");
    }
    if present
        != (journal.generation_digest.is_some()
            && journal.registration_digest.is_some()
            && journal.registration.is_some())
    {
        anyhow::bail!("bundle transaction payload does not match operation");
    }
    for digest in [
        journal.generation_digest.as_deref(),
        journal.registration_digest.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            anyhow::bail!("bundle transaction contains an invalid digest");
        }
    }
    Ok(())
}

fn registration_digest(value: &serde_json::Value) -> Result<String> {
    let canonical = lillux::canonical_json(value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn tree_digest(root: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_tree_entry(root, Path::new(""), &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_tree_entry(path: &Path, relative: &Path, hasher: &mut Sha256) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    hasher.update(relative.to_string_lossy().as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hasher.update(metadata.permissions().mode().to_le_bytes());
    }
    if metadata.is_dir() {
        hasher.update(b"d");
        let mut entries = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            hash_tree_entry(&entry.path(), &relative.join(entry.file_name()), hasher)?;
        }
    } else if metadata.is_file() {
        hasher.update(b"f");
        hasher.update(std::fs::read(path)?);
    } else if metadata.file_type().is_symlink() {
        hasher.update(b"l");
        hasher.update(std::fs::read_link(path)?.to_string_lossy().as_bytes());
    } else {
        anyhow::bail!("unsupported bundle tree entry: {}", path.display());
    }
    Ok(())
}

fn journal_directory(app_root: &Path) -> PathBuf {
    app_root.join(".ai/node/bundle-transactions")
}

fn valid_bundle_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn startup_reconciles_activated_present_transaction() {
        let root = tempfile::tempdir().unwrap();
        let key = SigningKey::generate(&mut OsRng);
        let tx = BundleTransaction::acquire(root.path(), "demo").unwrap();
        let staging = root.path().join(".ai/bundles/.demo.staging");
        std::fs::create_dir_all(staging.join(".ai")).unwrap();
        let registration = serde_json::json!({ "kind": "node", "path": tx.target() });
        tx.begin_present(BundleOperation::Install, &staging, registration)
            .unwrap();
        std::fs::rename(&staging, tx.target()).unwrap();
        tx.mark_activated().unwrap();
        drop(tx);

        assert_eq!(
            reconcile_all_bundle_transactions(root.path(), &key).unwrap(),
            vec!["demo"]
        );
        assert!(root.path().join(".ai/node/bundles/demo.yaml").is_file());
    }

    #[test]
    fn malformed_and_non_derived_journals_are_invalid() {
        let root = tempfile::tempdir().unwrap();
        let directory = journal_directory(root.path());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("old.json"), br#"{"obsolete":true}"#).unwrap();
        assert_eq!(
            inspect_bundle_transactions(root.path()).unwrap().invalid,
            vec!["old.json"]
        );
    }
}
