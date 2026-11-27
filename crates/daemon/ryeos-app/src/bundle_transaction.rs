//! Crash-recoverable coordination for installed bundle trees and registrations.

use std::ops::Deref;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lillux::crypto::SigningKey;
use ryeos_node_runtime::registry::BundleRegistryMutationLock;
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
    /// The install was refused after its tree was moved into place and is
    /// being undone. Reconciliation finishes the undo and never completes the
    /// install, whatever the namespace holds.
    Abandoning,
}

/// Build the exact node-owned registration bytes used by init and ordinary
/// bundle transactions, without publishing them. The caller supplies the
/// explicit node key whose public half is already pinned by current node
/// trust; no operator or daemon-global key is discovered here.
///
/// The registration written here carries no lane marker, so it is refused for
/// a name whose current registration has one: a laned bundle is never
/// re-registered through this path.
pub fn prepare_signed_bundle_registration(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    installed_target: &Path,
    node_signing_key: &Path,
) -> Result<Vec<u8>> {
    if let Some(name) = installed_target.file_name().and_then(|name| name.to_str()) {
        refuse_laned_replacement(app_root, trust_store, name)?;
    }
    let target = installed_target
        .to_str()
        .context("bundle registration target is not UTF-8")?;
    if !installed_target.is_absolute() || target.contains('\n') || target.contains('\r') {
        anyhow::bail!("bundle registration target must be an absolute single-line path");
    }
    let metadata = std::fs::symlink_metadata(node_signing_key)
        .context("inspect explicit node registration key")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("node registration key must be a regular non-symlink file");
    }
    use lillux::crypto::DecodePrivateKey as _;
    let pem = lillux::read_regular_file_bounded_no_follow(node_signing_key, 64 * 1024)?;
    let pem = std::str::from_utf8(&pem).context("node registration key is not UTF-8 PEM")?;
    let key = SigningKey::from_pkcs8_pem(pem).context("decode explicit node registration key")?;
    let yaml = serde_yaml::to_string(&serde_json::json!({
        "kind": "node",
        "path": target,
    }))?;
    Ok(lillux::signature::sign_content(&yaml, &key, "#", None).into_bytes())
}

/// Refuse to replace or re-register a bundle whose current registration
/// carries a lane marker. A laned bundle can only be removed; the typed refusal
/// `bundle_lane_not_replaceable` (409) says so. A name with no registration
/// passes. The registrations are read through the verified bundle bootstrap
/// loader, so an unsigned or foreign-signed file is an error, not a pass.
pub fn refuse_laned_replacement(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    name: &str,
) -> Result<()> {
    let records = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root,
        trust_store,
    }
    .load_registrations()
    .context("load current bundle registrations to check lane markers")?;
    if let Some(lane) = records
        .iter()
        .find(|record| record.name == name)
        .and_then(|record| record.lane.as_ref())
    {
        return Err(anyhow::Error::new(crate::handler_error::typed_refusal(
            "bundle_lane_not_replaceable",
            409,
            format!(
                "bundle '{name}' is in the {} lane and cannot be replaced or updated in place; remove it and pull the publication again",
                lane.name
            ),
        )));
    }
    Ok(())
}

/// Refuse to put a laned bundle over a bundle a native install registered. A
/// name with no registration passes; a name registered in a lane is the
/// replacement guard's concern ([`refuse_laned_replacement`]). Read through
/// the registrations without the lane audit, under whatever lock the caller
/// holds.
pub fn refuse_native_holder(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    name: &str,
) -> Result<()> {
    let records = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root,
        trust_store,
    }
    .load_registrations()
    .context("load current bundle registrations to check who holds the name")?;
    if records
        .iter()
        .any(|record| record.name == name && record.lane.is_none())
    {
        return Err(anyhow::Error::new(crate::handler_error::typed_refusal(
            "bundle_name_held_by_native_lane",
            409,
            format!(
                "bundle '{name}' is registered on this node by a native install; a publication never takes or replaces a natively installed name"
            ),
        )));
    }
    Ok(())
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

/// A per-name bundle transaction tied to a held node-wide registry lock.
///
/// The borrow makes it impossible for normal callers to drop the registry lock
/// while continuing to mutate through this transaction.
pub struct BundleMutationTransaction<'a> {
    transaction: BundleTransaction,
    _registry_lock: &'a BundleRegistryMutationLock,
}

impl<'a> BundleMutationTransaction<'a> {
    /// Borrow the actual global registry owner through this transaction, keeping
    /// the existing global-to-per-name lock order and journal implementation.
    pub fn acquire(registry_lock: &'a BundleRegistryMutationLock, name: &str) -> Result<Self> {
        Ok(Self {
            transaction: BundleTransaction::acquire(registry_lock.app_root(), name)?,
            _registry_lock: registry_lock,
        })
    }
}

impl Deref for BundleMutationTransaction<'_> {
    type Target = BundleTransaction;

    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}

impl BundleTransaction {
    fn acquire(app_root: &Path, name: &str) -> Result<Self> {
        if !valid_bundle_name(name) {
            anyhow::bail!("invalid bundle transaction name: {name}");
        }
        let target = installed_bundles_directory(app_root).join(name);
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

    /// Record, durably, that a prepared install or replace is being undone.
    /// The caller does this BEFORE it moves the tree back, so a crash or a
    /// failed move leaves a journal that reconciliation turns into the undo,
    /// never into the install. Refused unless the journal is a present
    /// transaction still in its prepared phase.
    pub fn mark_abandoning(&self) -> Result<()> {
        let mut journal = self.read_validated_journal()?;
        if journal.operation == BundleOperation::Remove || journal.phase != BundlePhase::Prepared {
            anyhow::bail!("only a prepared install or replace transaction can be abandoned");
        }
        journal.phase = BundlePhase::Abandoning;
        self.write_journal(&journal)
    }

    /// Remove the journal of an abandoning transaction once the caller has put
    /// the namespace back as it was before `begin_present`. Nothing was
    /// registered, so no registration is touched.
    pub fn finish_abandoned(&self) -> Result<()> {
        let journal = self.read_validated_journal()?;
        if journal.phase != BundlePhase::Abandoning {
            anyhow::bail!("only an abandoning transaction can be finished as abandoned");
        }
        self.finish()
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
        if journal.phase == BundlePhase::Abandoning {
            self.complete_abandon(&journal)?;
            self.finish()?;
            return Ok(Some(journal.operation));
        }
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
                    // The target is not the journal's generation. Either the
                    // exchange was never made (the staging path still holds the
                    // journal's generation and the target is the previous one),
                    // or it was made and the moved tree then changed (the
                    // staging path holds the previous generation). Only the
                    // staging path's digest tells them apart.
                    let staging = journal
                        .staging_path
                        .as_deref()
                        .context("replace transaction has no staging path")?;
                    if !staging.is_dir() {
                        anyhow::bail!(
                            "prepared replacement matches neither generation: the target differs from the journal and its staging path is missing"
                        );
                    }
                    // Stated limit: the journal holds no digest of the previous
                    // generation, so a copy at the staging path that another
                    // process of the node user altered would be swapped in; that
                    // needs a writer in the bundles directory, which is already
                    // full node-user trust. A journal schema change should add it.
                    if tree_digest(staging)? != expected {
                        // The exchange was made: put the previous generation
                        // back; the changed tree then sits at the staging path
                        // and is removed below.
                        lillux::atomic_exchange_paths(&self.target, staging)
                            .context("swap the previous generation back into place")?;
                    }
                    // Otherwise nothing was activated: preserve the previous
                    // registered generation and discard the staged candidate.
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

    /// Finish an undo that was recorded and not completed. The tree that was
    /// refused is removed or swapped out; no registration is written and the
    /// install is never completed.
    fn complete_abandon(&self, journal: &Journal) -> Result<()> {
        let staging = journal
            .staging_path
            .as_deref()
            .context("abandoning transaction has no staging path")?;
        let expected = journal
            .generation_digest
            .as_deref()
            .context("present transaction missing generation digest")?;
        match journal.operation {
            BundleOperation::Replace => {
                // The refused generation is still in the target when its
                // digest is the journal's; the previous one then waits at the
                // staging path and is swapped back. Otherwise the swap was
                // already made and only the refused tree remains to remove.
                if self.target.is_dir() && tree_digest(&self.target)? == expected {
                    lillux::atomic_exchange_paths(&self.target, staging)
                        .context("swap the previous generation back into place")?;
                }
            }
            _ => lillux::remove_dir_all_durable(&self.target)?,
        }
        lillux::remove_dir_all_durable(staging)?;
        Ok(())
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
        bundle_registrations_directory(&self.app_root).join(format!("{}.yaml", self.name))
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
    let registry_lock = BundleRegistryMutationLock::acquire(app_root)?;
    let diagnostics = inspect_bundle_transactions(app_root)?;
    if !diagnostics.invalid.is_empty() {
        anyhow::bail!(
            "invalid bundle transaction journals: {}",
            diagnostics.invalid.join(", ")
        );
    }
    for name in &diagnostics.pending {
        BundleMutationTransaction::acquire(&registry_lock, name)?.reconcile(signing_key)?;
    }
    Ok(diagnostics.pending)
}

fn validate_journal(app_root: &Path, name: &str, journal: &Journal) -> Result<()> {
    if journal.bundle_name != name || !valid_bundle_name(&journal.bundle_name) {
        anyhow::bail!("bundle transaction name mismatch");
    }
    let transaction_target = installed_bundles_directory(app_root).join(name);
    let registration = bundle_registrations_directory(app_root).join(format!("{name}.yaml"));
    if journal.target_path != transaction_target || journal.registration_path != registration {
        anyhow::bail!("bundle transaction contains non-derived paths");
    }
    let expected_staging = match journal.operation {
        BundleOperation::Install | BundleOperation::Replace => {
            Some(installed_bundles_directory(app_root).join(format!(".{name}.staging")))
        }
        BundleOperation::RemoteInstall => {
            Some(installed_bundles_directory(app_root).join(format!(".{name}.remote-staging")))
        }
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

/// Compute the exact descriptor-safe bundle tree identity used by both
/// single-bundle and stopped whole-set transactions.
pub fn tree_digest(root: &Path) -> Result<String> {
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
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/bundle-transactions")
}

fn installed_bundles_directory(app_root: &Path) -> PathBuf {
    app_root.join(ryeos_engine::AI_DIR).join("bundles")
}

fn bundle_registrations_directory(app_root: &Path) -> PathBuf {
    app_root.join(ryeos_engine::AI_DIR).join("node/bundles")
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
    use ryeos_node_runtime::registry::BundleRegistryReadLock;

    #[test]
    fn first_registry_reader_establishes_shared_writer_anchor() {
        let root = tempfile::tempdir().unwrap();

        let reader = BundleRegistryReadLock::acquire(root.path()).unwrap();
        assert!(root.path().join(".ai/.bundles.lock").is_file());
        assert!(!root.path().join(".ai/bundles").exists());
        drop(reader);

        drop(BundleRegistryMutationLock::acquire(root.path()).unwrap());
    }

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

    /// A node with a native bundle `base` and a laned bundle `pulled` whose
    /// lane audit fails (the node has no publication_lane policy), so the boot
    /// loader leaves it out.
    fn node_with_a_skipped_laned_bundle(
        root: &Path,
    ) -> (
        ryeos_node_runtime::identity::NodeIdentity,
        ryeos_engine::trust::TrustStore,
    ) {
        std::fs::create_dir_all(root.join(".ai/node/identity")).unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &root.join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: identity.fingerprint().to_owned(),
                verifying_key: identity.signing_key().verifying_key(),
                label: None,
            },
        ]);
        let registrations = root.join(".ai/node/bundles");
        std::fs::create_dir_all(&registrations).unwrap();
        let lane = ryeos_engine::trust::BundleLane::publication(
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            "1.0.0",
        );
        for (name, lane) in [("base", None), ("pulled", Some(lane))] {
            let dir = root.join(".ai/bundles").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let mut body = serde_json::json!({ "kind": "node", "path": dir });
            if let Some(lane) = lane {
                body["lane"] = serde_json::to_value(lane).unwrap();
            }
            let yaml = serde_yaml::to_string(&body).unwrap();
            std::fs::write(
                registrations.join(format!("{name}.yaml")),
                lillux::signature::sign_content(&yaml, identity.signing_key(), "#", None),
            )
            .unwrap();
        }
        (identity, trust)
    }

    fn refusal_code_of(error: &anyhow::Error) -> Option<String> {
        match crate::handler_error::extract_handler_error(error) {
            Some(crate::handler_error::HandlerError::Structured { code, .. }) => Some(code),
            _ => None,
        }
    }

    #[test]
    fn a_skipped_laned_bundle_is_still_registered_laned_and_holds_its_name() {
        let root = tempfile::tempdir().unwrap();
        let (_identity, trust) = node_with_a_skipped_laned_bundle(root.path());
        let loader = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
            app_root: root.path(),
            trust_store: &trust,
        };

        // The boot loader leaves the laned bundle out and says why; the node
        // still loads the native one.
        let (loaded, skipped) = loader.load_bundle_section_reporting_skipped().unwrap();
        assert_eq!(
            loaded.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["base"]
        );
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "pulled");
        assert_eq!(skipped[0].code, "publication_lane_policy_unavailable");

        // The registrations still show it, laned.
        let registered = loader.load_registrations().unwrap();
        let pulled = registered.iter().find(|r| r.name == "pulled").unwrap();
        assert!(pulled.lane.is_some());

        // So it still refuses replacement, and a native name is still held
        // against a laned bundle.
        let error = refuse_laned_replacement(root.path(), &trust, "pulled").unwrap_err();
        assert_eq!(
            refusal_code_of(&error).as_deref(),
            Some("bundle_lane_not_replaceable")
        );
        refuse_laned_replacement(root.path(), &trust, "base").unwrap();
        let error = refuse_native_holder(root.path(), &trust, "base").unwrap_err();
        assert_eq!(
            refusal_code_of(&error).as_deref(),
            Some("bundle_name_held_by_native_lane")
        );
        refuse_native_holder(root.path(), &trust, "pulled").unwrap();
        refuse_native_holder(root.path(), &trust, "free").unwrap();
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
