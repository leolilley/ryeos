//! Exact-set stopped-node activation journal and recovery decisions.
//!
//! The operator-signed whole-init completion is the commit authority. Journal
//! phase is progress evidence only and can never decide whether old or new is
//! active after a crash.

use anyhow::bail;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

use crate::bundle_transaction::BundleRegistryMutationLock;

const JOURNAL_MAX_BYTES: u64 = 1024 * 1024;
const COMPLETION_MAX_BYTES: usize = 256 * 1024;

/// Canonical installed identity of one bundle. The whole-set digest commits
/// to filesystem bytes and signed registration bytes, not publisher names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledBundleIdentity {
    pub bundle_name: String,
    pub tree_digest: String,
    pub registration_digest: String,
}

/// Hash a complete, sorted prospective installed set. This is the sole
/// constructor for `ActiveBundleSelection::installed_set_digest`.
pub fn installed_set_digest(
    identities: impl IntoIterator<Item = InstalledBundleIdentity>,
) -> anyhow::Result<String> {
    let mut identities = identities.into_iter().collect::<Vec<_>>();
    if identities.is_empty() || identities.len() > 1024 {
        bail!("installed bundle set must be bounded and nonempty");
    }
    identities.sort_by(|left, right| left.bundle_name.cmp(&right.bundle_name));
    let mut previous: Option<&str> = None;
    for identity in &identities {
        if identity.bundle_name.is_empty()
            || identity.bundle_name.len() > 64
            || !identity.bundle_name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            bail!("installed bundle set has an invalid bundle name");
        }
        if previous.is_some_and(|name| name == identity.bundle_name.as_str()) {
            bail!("installed bundle set repeats a bundle name");
        }
        require_hash("installed tree", &identity.tree_digest)?;
        require_hash("installed registration", &identity.registration_digest)?;
        previous = Some(&identity.bundle_name);
    }
    let value = serde_json::json!({
        "schema": "ryeos.installed_bundle_set.v1",
        "bundles": identities,
    });
    Ok(lillux::cas::sha256_hex(
        lillux::canonical_json(&value)?.as_bytes(),
    ))
}

pub fn journal_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("transactions/bundle-set.json")
}

pub fn init_completion_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("config/onboarding/init-completion.json")
}

pub fn active_selection_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/active-bundle-selection.json")
}

fn staged_selection_path(app_root: &Path, transaction_id: &str) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("transactions/bundle-set")
        .join(transaction_id)
        .join("new/active-bundle-selection.json")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveBundleSelection {
    pub schema: u32,
    pub selection_hash: String,
    pub installed_set_digest: String,
}

impl ActiveBundleSelection {
    pub const SCHEMA: u32 = 1;

    fn validate(&self) -> anyhow::Result<()> {
        if self.schema != Self::SCHEMA {
            bail!("unsupported active bundle-selection schema");
        }
        require_hash("active selection", &self.selection_hash)?;
        require_hash("active installed set", &self.installed_set_digest)
    }
}

/// Durable compare-and-swap owner for the local active selection. Staging is
/// separate from publication so the signed whole-init completion remains the
/// transaction commit fence.
#[derive(Debug, Clone)]
pub struct ActiveBundleSelectionStore {
    app_root: PathBuf,
}

impl ActiveBundleSelectionStore {
    pub fn new(app_root: &Path) -> Self {
        Self {
            app_root: app_root.to_owned(),
        }
    }

    pub fn load(&self) -> anyhow::Result<Option<ActiveBundleSelection>> {
        load_active_selection_file(&active_selection_path(&self.app_root))
    }

    pub fn stage(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        journal.validate()?;
        let current = self.load()?;
        if current.as_ref().map(|value| value.selection_hash.as_str())
            != journal.expected_active_selection.as_deref()
        {
            bail!("active bundle selection changed since admission");
        }
        if let Some(current) = &current {
            if current.installed_set_digest != journal.old_installed_set_digest {
                bail!("active installed-set digest disagrees with transaction predecessor");
            }
        }
        let next = ActiveBundleSelection {
            schema: ActiveBundleSelection::SCHEMA,
            selection_hash: journal.selection_hash.clone(),
            installed_set_digest: journal.new_installed_set_digest.clone(),
        };
        next.validate()?;
        let path = staged_selection_path(&self.app_root, &journal.transaction_id);
        create_parent(&path)?;
        if std::fs::symlink_metadata(&path).is_ok() {
            bail!("staged active bundle selection already exists");
        }
        lillux::atomic_write_private(&path, &serde_json::to_vec_pretty(&next)?)?;
        Ok(())
    }

    pub fn commit_new(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        journal.validate()?;
        let expected = ActiveBundleSelection {
            schema: ActiveBundleSelection::SCHEMA,
            selection_hash: journal.selection_hash.clone(),
            installed_set_digest: journal.new_installed_set_digest.clone(),
        };
        if self.load()?.as_ref() == Some(&expected) {
            return Ok(());
        }
        let staged = staged_selection_path(&self.app_root, &journal.transaction_id);
        if load_active_selection_file(&staged)?.as_ref() != Some(&expected) {
            bail!("staged active bundle selection is absent or disagrees with transaction");
        }
        let current = self.load()?;
        if current.as_ref().map(|value| value.selection_hash.as_str())
            != journal.expected_active_selection.as_deref()
        {
            bail!("active bundle selection CAS failed at commit");
        }
        let target = active_selection_path(&self.app_root);
        create_parent(&target)?;
        lillux::rename_path_durable(&staged, &target)?;
        Ok(())
    }

    pub fn restore_old(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        journal.validate()?;
        let current = self.load()?;
        let current_hash = current.as_ref().map(|value| value.selection_hash.as_str());
        if current_hash != journal.expected_active_selection.as_deref()
            && current_hash != Some(journal.selection_hash.as_str())
        {
            bail!("active bundle selection is unrelated to either transaction generation");
        }
        if current_hash == journal.expected_active_selection.as_deref()
            && current
                .as_ref()
                .is_some_and(|value| value.installed_set_digest != journal.old_installed_set_digest)
        {
            bail!("active predecessor installed-set digest disagrees with transaction");
        }
        if current_hash == Some(journal.selection_hash.as_str())
            && current
                .as_ref()
                .is_some_and(|value| value.installed_set_digest != journal.new_installed_set_digest)
        {
            bail!("active successor installed-set digest disagrees with transaction");
        }
        match &journal.expected_active_selection {
            Some(selection_hash) => {
                let old = ActiveBundleSelection {
                    schema: ActiveBundleSelection::SCHEMA,
                    selection_hash: selection_hash.clone(),
                    installed_set_digest: journal.old_installed_set_digest.clone(),
                };
                lillux::atomic_write_private(
                    &active_selection_path(&self.app_root),
                    &serde_json::to_vec_pretty(&old)?,
                )?;
            }
            None => remove_file_if_present(&active_selection_path(&self.app_root))?,
        }
        remove_file_if_present(&staged_selection_path(
            &self.app_root,
            &journal.transaction_id,
        ))
    }
}

fn load_active_selection_file(path: &Path) -> anyhow::Result<Option<ActiveBundleSelection>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("unsafe active bundle-selection file")
        }
        Ok(metadata) if metadata.len() > 16 * 1024 => {
            bail!("active bundle-selection file exceeds size bound")
        }
        Ok(_) => {}
    }
    let value: ActiveBundleSelection = serde_json::from_slice(
        &lillux::read_regular_file_bounded_no_follow(path, 16 * 1024)?,
    )?;
    value.validate()?;
    Ok(Some(value))
}

pub fn store_journal(app_root: &Path, journal: &BundleSetJournal) -> anyhow::Result<()> {
    journal.validate()?;
    let bytes = serde_json::to_vec_pretty(journal)?;
    if bytes.len() as u64 > JOURNAL_MAX_BYTES {
        bail!("bundle-set transaction journal exceeds size bound");
    }
    let path = journal_path(app_root);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("bundle-set journal has no parent"))?;
    std::fs::create_dir_all(parent)?;
    lillux::atomic_write_private(&path, &bytes)?;
    Ok(())
}

pub fn consume_journal(app_root: &Path) -> anyhow::Result<()> {
    let journal = load_journal(app_root)?;
    let path = journal_path(app_root);
    lillux::remove_file_durable(&path)?;
    if let Some(journal) = journal {
        let transaction_root = recovery_path(
            app_root,
            &format!(
                "{}/transactions/bundle-set/{}",
                ryeos_engine::AI_DIR,
                journal.transaction_id
            ),
        )?;
        match std::fs::symlink_metadata(&transaction_root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                lillux::remove_dir_all_durable(&transaction_root)?;
            }
            Ok(_) => bail!("bundle-set transaction root is not a safe directory"),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub fn load_journal(app_root: &Path) -> anyhow::Result<Option<BundleSetJournal>> {
    let path = journal_path(app_root);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("unsafe bundle-set transaction journal")
        }
        Ok(metadata) if metadata.len() > JOURNAL_MAX_BYTES => {
            bail!("bundle-set transaction journal exceeds size bound")
        }
        Ok(_) => {}
    }
    let bytes = lillux::read_regular_file_bounded_no_follow(&path, JOURNAL_MAX_BYTES)?;
    let journal: BundleSetJournal = serde_json::from_slice(&bytes)?;
    journal.validate()?;
    Ok(Some(journal))
}

pub fn observe_completion(
    app_root: &Path,
    journal: &BundleSetJournal,
) -> anyhow::Result<CompletionObservation> {
    let path = init_completion_path(app_root);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CompletionObservation::Absent);
        }
        Err(_) => return Ok(CompletionObservation::CorruptOrUnrelated),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Ok(CompletionObservation::CorruptOrUnrelated);
        }
        Ok(_) => {}
    }
    let bytes = match lillux::read_regular_file_bounded_no_follow(&path, JOURNAL_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(CompletionObservation::CorruptOrUnrelated),
    };
    let digest = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(if digest == journal.old_completion_bytes_hash {
        CompletionObservation::ExactOld
    } else if digest == journal.new_completion_bytes_hash {
        CompletionObservation::ExactNew
    } else {
        CompletionObservation::CorruptOrUnrelated
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleSetPhase {
    Prepared,
    Materialized,
    Admitted,
    TreesActivated,
    RegistrationsCommitted,
    ActiveSelectionStaged,
    CompletionPublicationStarted,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSetJournal {
    pub schema: u32,
    pub transaction_id: String,
    pub selection_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub expected_active_selection: Option<String>,
    pub old_completion_hash: String,
    pub old_completion_bytes_hash: String,
    pub old_completion_bytes_base64: String,
    pub new_completion_hash: String,
    pub new_completion_bytes_hash: String,
    pub new_completion_bytes_base64: String,
    pub old_installed_set_digest: String,
    pub new_installed_set_digest: String,
    pub actions: Vec<BundleSetAction>,
    pub phase: BundleSetPhase,
    /// Set only after startup has been permitted to execute the new selection.
    /// V1 never interprets filesystem restoration as safe rollback afterward.
    pub new_selection_may_have_executed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleSetActionKind {
    Add,
    Replace,
    Remove,
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSetAction {
    pub bundle_name: String,
    pub kind: BundleSetActionKind,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub old_tree_digest: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub new_tree_digest: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    /// SHA-256 of the exact signed registration file bytes.
    pub old_registration_digest: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    /// SHA-256 of the exact signed registration file bytes.
    pub new_registration_digest: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub old_tree_backup_path: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub new_tree_staging_path: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub old_registration_backup_path: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub new_registration_staging_path: Option<String>,
}

/// Caller-owned input to the stopped-node apply boundary. `new_*_source`
/// paths are consumed into transaction-owned staging. They must already carry
/// the exact admitted tree and signed registration described by the digests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedBundleSetAction {
    pub bundle_name: String,
    pub kind: BundleSetActionKind,
    pub old_tree_digest: Option<String>,
    pub new_tree_digest: Option<String>,
    pub old_registration_digest: Option<String>,
    pub new_registration_digest: Option<String>,
    pub new_tree_source: Option<PathBuf>,
    pub new_registration_source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoppedBundleSetApplyRequest {
    pub transaction_id: String,
    pub selection_hash: String,
    pub expected_active_selection: Option<String>,
    pub old_completion_hash: String,
    pub old_completion_bytes: Vec<u8>,
    pub new_completion_hash: String,
    pub new_completion_bytes: Vec<u8>,
    pub old_installed_set_digest: String,
    pub new_installed_set_digest: String,
    pub actions: Vec<PreparedBundleSetAction>,
}

/// Authority hooks kept outside the filesystem transaction owner. Admission
/// must validate the complete prospective graph. Selection staging must enforce
/// the exact expected-active-selection fence. Completion verification must use
/// pinned operator public trust.
pub trait StoppedBundleSetApplyAuthority {
    fn admit_complete_set(&self, journal: &BundleSetJournal) -> anyhow::Result<()>;
    fn stage_active_selection(&self, journal: &BundleSetJournal) -> anyhow::Result<()>;
    fn verify_new_completion(
        &self,
        app_root: &Path,
        journal: &BundleSetJournal,
    ) -> anyhow::Result<()>;
    fn commit_active_selection(&self, journal: &BundleSetJournal) -> anyhow::Result<()>;
}

/// Apply one already authorized exact set while the caller holds exclusive
/// stopped-node authority. This function additionally requires the global
/// bundle-registry mutation lock and owns all journal/rename ordering.
pub fn apply_stopped_bundle_set(
    app_root: &Path,
    registry_lock: &BundleRegistryMutationLock,
    request: StoppedBundleSetApplyRequest,
    authority: &dyn StoppedBundleSetApplyAuthority,
) -> anyhow::Result<()> {
    registry_lock.ensure_protects_app_root(app_root)?;
    if load_journal(app_root)?.is_some() {
        bail!("an unfinished bundle-set transaction already exists");
    }
    let (mut journal, candidate_sources) = prepare_journal_layout(app_root, request)?;
    store_journal(app_root, &journal)?;

    materialize_new_candidates(app_root, &journal, candidate_sources)?;
    journal.advance(BundleSetPhase::Materialized)?;
    store_journal(app_root, &journal)?;

    authority.admit_complete_set(&journal)?;
    journal.advance(BundleSetPhase::Admitted)?;
    store_journal(app_root, &journal)?;

    activate_trees(app_root, &journal)?;
    journal.advance(BundleSetPhase::TreesActivated)?;
    store_journal(app_root, &journal)?;

    commit_registrations(app_root, &journal)?;
    journal.advance(BundleSetPhase::RegistrationsCommitted)?;
    store_journal(app_root, &journal)?;

    authority.stage_active_selection(&journal)?;
    journal.advance(BundleSetPhase::ActiveSelectionStaged)?;
    store_journal(app_root, &journal)?;

    journal.advance(BundleSetPhase::CompletionPublicationStarted)?;
    store_journal(app_root, &journal)?;
    write_completion(app_root, &journal.new_completion_bytes_base64)?;
    authority.verify_new_completion(app_root, &journal)?;
    authority.commit_active_selection(&journal)?;
    journal.advance(BundleSetPhase::Complete)?;
    store_journal(app_root, &journal)?;
    consume_journal(app_root)
}

impl BundleSetJournal {
    pub const SCHEMA: u32 = 2;

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != Self::SCHEMA {
            bail!("unsupported bundle-set transaction journal schema");
        }
        for (label, value) in [
            ("transaction id", &self.transaction_id),
            ("selection", &self.selection_hash),
            ("old completion", &self.old_completion_hash),
            ("old completion bytes", &self.old_completion_bytes_hash),
            ("new completion", &self.new_completion_hash),
            ("new completion bytes", &self.new_completion_bytes_hash),
            ("old installed set", &self.old_installed_set_digest),
            ("new installed set", &self.new_installed_set_digest),
        ] {
            require_hash(label, value)?;
        }
        if let Some(expected) = &self.expected_active_selection {
            require_hash("expected active selection", expected)?;
        }
        if self.actions.is_empty() || self.actions.len() > 1024 {
            bail!("bundle-set transaction requires a bounded nonempty action manifest");
        }
        let mut previous = None;
        for action in &self.actions {
            action.validate()?;
            let prefix = format!(
                "{}/transactions/bundle-set/{}/",
                ryeos_engine::AI_DIR,
                self.transaction_id
            );
            for path in [
                action.old_tree_backup_path.as_deref(),
                action.new_tree_staging_path.as_deref(),
                action.old_registration_backup_path.as_deref(),
                action.new_registration_staging_path.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !path.starts_with(&prefix) {
                    bail!("bundle-set recovery coordinate escapes transaction namespace");
                }
            }
            for (actual, expected) in [
                (
                    action.old_tree_backup_path.as_deref(),
                    format!("{prefix}old/trees/{}", action.bundle_name),
                ),
                (
                    action.new_tree_staging_path.as_deref(),
                    format!("{prefix}new/trees/{}", action.bundle_name),
                ),
                (
                    action.old_registration_backup_path.as_deref(),
                    format!("{prefix}old/registrations/{}.yaml", action.bundle_name),
                ),
                (
                    action.new_registration_staging_path.as_deref(),
                    format!("{prefix}new/registrations/{}.yaml", action.bundle_name),
                ),
            ] {
                if actual.is_some_and(|actual| actual != expected) {
                    bail!("bundle-set recovery coordinate disagrees with bundle action");
                }
            }
            if previous
                .as_deref()
                .is_some_and(|name| name >= action.bundle_name.as_str())
            {
                bail!("bundle-set actions must be strictly sorted and unique");
            }
            previous = Some(action.bundle_name.clone());
        }
        if self.old_completion_hash == self.new_completion_hash
            || self.old_installed_set_digest == self.new_installed_set_digest
        {
            bail!("bundle-set transaction must describe an actual transition");
        }
        validate_completion_bytes(
            "old completion",
            &self.old_completion_bytes_base64,
            &self.old_completion_bytes_hash,
        )?;
        validate_completion_bytes(
            "new completion",
            &self.new_completion_bytes_base64,
            &self.new_completion_bytes_hash,
        )?;
        if self.new_selection_may_have_executed && self.phase < BundleSetPhase::Complete {
            bail!("new selection execution is permitted only after committed completion");
        }
        Ok(())
    }

    pub fn advance(&mut self, next: BundleSetPhase) -> anyhow::Result<()> {
        self.validate()?;
        let expected = match self.phase {
            BundleSetPhase::Prepared => BundleSetPhase::Materialized,
            BundleSetPhase::Materialized => BundleSetPhase::Admitted,
            BundleSetPhase::Admitted => BundleSetPhase::TreesActivated,
            BundleSetPhase::TreesActivated => BundleSetPhase::RegistrationsCommitted,
            BundleSetPhase::RegistrationsCommitted => BundleSetPhase::ActiveSelectionStaged,
            BundleSetPhase::ActiveSelectionStaged => BundleSetPhase::CompletionPublicationStarted,
            BundleSetPhase::CompletionPublicationStarted => BundleSetPhase::Complete,
            BundleSetPhase::Complete => bail!("completed bundle-set transaction cannot advance"),
        };
        if next != expected {
            bail!("bundle-set transaction phase must advance exactly once");
        }
        self.phase = next;
        Ok(())
    }

    pub fn mark_new_selection_executed(&mut self) -> anyhow::Result<()> {
        self.validate()?;
        if self.phase != BundleSetPhase::Complete {
            bail!("new selection cannot execute before completion commit");
        }
        self.new_selection_may_have_executed = true;
        Ok(())
    }
}

impl BundleSetAction {
    fn validate(&self) -> anyhow::Result<()> {
        if self.bundle_name.is_empty()
            || self.bundle_name.len() > 64
            || !self.bundle_name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            bail!("bundle-set action has an invalid bundle name");
        }
        for (label, value) in [
            ("old tree", self.old_tree_digest.as_deref()),
            ("new tree", self.new_tree_digest.as_deref()),
            ("old registration", self.old_registration_digest.as_deref()),
            ("new registration", self.new_registration_digest.as_deref()),
        ] {
            if let Some(value) = value {
                require_hash(label, value)?;
            }
        }
        let shape = (
            self.old_tree_digest.is_some() && self.old_registration_digest.is_some(),
            self.new_tree_digest.is_some() && self.new_registration_digest.is_some(),
        );
        let expected = match self.kind {
            BundleSetActionKind::Add => (false, true),
            BundleSetActionKind::Replace => (true, true),
            BundleSetActionKind::Remove => (true, false),
            BundleSetActionKind::Keep => (true, true),
        };
        if shape != expected {
            bail!("bundle-set action digest shape disagrees with operation");
        }
        if self.kind == BundleSetActionKind::Keep
            && (self.old_tree_digest != self.new_tree_digest
                || self.old_registration_digest != self.new_registration_digest)
        {
            bail!("kept bundle must preserve exact tree and registration identities");
        }
        let old_paths =
            self.old_tree_backup_path.is_some() && self.old_registration_backup_path.is_some();
        let new_paths =
            self.new_tree_staging_path.is_some() && self.new_registration_staging_path.is_some();
        let expected_paths = match self.kind {
            BundleSetActionKind::Add => (false, true),
            BundleSetActionKind::Replace => (true, true),
            BundleSetActionKind::Remove => (true, false),
            BundleSetActionKind::Keep => (false, false),
        };
        if (old_paths, new_paths) != expected_paths {
            bail!("bundle-set recovery coordinate shape disagrees with operation");
        }
        for (label, path) in [
            ("old tree backup", self.old_tree_backup_path.as_deref()),
            ("new tree staging", self.new_tree_staging_path.as_deref()),
            (
                "old registration backup",
                self.old_registration_backup_path.as_deref(),
            ),
            (
                "new registration staging",
                self.new_registration_staging_path.as_deref(),
            ),
        ] {
            if let Some(path) = path {
                validate_recovery_relative_path(label, path)?;
            }
        }
        Ok(())
    }
}

fn prepare_journal_layout(
    app_root: &Path,
    request: StoppedBundleSetApplyRequest,
) -> anyhow::Result<(BundleSetJournal, Vec<(String, PathBuf, PathBuf)>)> {
    require_hash("transaction id", &request.transaction_id)?;
    if request.old_completion_bytes.len() > COMPLETION_MAX_BYTES
        || request.new_completion_bytes.len() > COMPLETION_MAX_BYTES
    {
        bail!("bundle-set completion bytes exceed size bound");
    }
    let current_completion = lillux::read_regular_file_bounded_no_follow(
        &init_completion_path(app_root),
        COMPLETION_MAX_BYTES as u64,
    )?;
    if current_completion != request.old_completion_bytes {
        bail!("old completion bytes disagree with the current commit fence");
    }
    let old_bytes_hash = format!("{:x}", Sha256::digest(&request.old_completion_bytes));
    let new_bytes_hash = format!("{:x}", Sha256::digest(&request.new_completion_bytes));
    if request.old_completion_hash != old_bytes_hash
        || request.new_completion_hash != new_bytes_hash
    {
        bail!("bundle-set completion identity must hash the exact signed document bytes");
    }
    let root = format!(
        "{}/transactions/bundle-set/{}",
        ryeos_engine::AI_DIR,
        request.transaction_id
    );
    let mut actions = Vec::with_capacity(request.actions.len());
    let mut candidate_sources = Vec::new();
    for prepared in request.actions {
        let has_new_sources =
            prepared.new_tree_source.is_some() && prepared.new_registration_source.is_some();
        if has_new_sources
            != matches!(
                prepared.kind,
                BundleSetActionKind::Add | BundleSetActionKind::Replace
            )
        {
            bail!("prepared candidate source shape disagrees with operation");
        }
        if let Some(source) = &prepared.new_tree_source {
            require_tree_digest(source, prepared.new_tree_digest.as_deref().unwrap())?;
        }
        if let Some(source) = &prepared.new_registration_source {
            require_file_digest(source, prepared.new_registration_digest.as_deref().unwrap())?;
        }
        let old = matches!(
            prepared.kind,
            BundleSetActionKind::Replace | BundleSetActionKind::Remove
        );
        let new = matches!(
            prepared.kind,
            BundleSetActionKind::Add | BundleSetActionKind::Replace
        );
        let bundle_name = prepared.bundle_name;
        if let (Some(tree), Some(registration)) =
            (prepared.new_tree_source, prepared.new_registration_source)
        {
            candidate_sources.push((bundle_name.clone(), tree, registration));
        }
        actions.push(BundleSetAction {
            bundle_name: bundle_name.clone(),
            kind: prepared.kind,
            old_tree_digest: prepared.old_tree_digest,
            new_tree_digest: prepared.new_tree_digest,
            old_registration_digest: prepared.old_registration_digest,
            new_registration_digest: prepared.new_registration_digest,
            old_tree_backup_path: old.then(|| format!("{root}/old/trees/{bundle_name}")),
            new_tree_staging_path: new.then(|| format!("{root}/new/trees/{bundle_name}")),
            old_registration_backup_path: old
                .then(|| format!("{root}/old/registrations/{bundle_name}.yaml")),
            new_registration_staging_path: new
                .then(|| format!("{root}/new/registrations/{bundle_name}.yaml")),
        });
    }
    actions.sort_by(|left, right| left.bundle_name.cmp(&right.bundle_name));
    let journal = BundleSetJournal {
        schema: BundleSetJournal::SCHEMA,
        transaction_id: request.transaction_id,
        selection_hash: request.selection_hash,
        expected_active_selection: request.expected_active_selection,
        old_completion_hash: request.old_completion_hash,
        old_completion_bytes_hash: old_bytes_hash,
        old_completion_bytes_base64: base64::engine::general_purpose::STANDARD
            .encode(request.old_completion_bytes),
        new_completion_hash: request.new_completion_hash,
        new_completion_bytes_hash: new_bytes_hash,
        new_completion_bytes_base64: base64::engine::general_purpose::STANDARD
            .encode(request.new_completion_bytes),
        old_installed_set_digest: request.old_installed_set_digest,
        new_installed_set_digest: request.new_installed_set_digest,
        actions,
        phase: BundleSetPhase::Prepared,
        new_selection_may_have_executed: false,
    };
    journal.validate()?;
    // Existing live identities must match the admitted predecessor before any
    // candidate is moved or journal is published.
    for action in &journal.actions {
        let live_tree = recovery_path(
            app_root,
            &format!("{}/bundles/{}", ryeos_engine::AI_DIR, action.bundle_name),
        )?;
        let live_registration = recovery_path(
            app_root,
            &format!(
                "{}/node/bundles/{}.yaml",
                ryeos_engine::AI_DIR,
                action.bundle_name
            ),
        )?;
        if action.old_tree_digest.is_some() {
            require_tree_digest(&live_tree, action.old_tree_digest.as_deref().unwrap())?;
            require_file_digest(
                &live_registration,
                action.old_registration_digest.as_deref().unwrap(),
            )?;
        } else if live_tree.exists() || live_registration.exists() {
            bail!("add action collides with installed bundle state");
        }
    }
    Ok((journal, candidate_sources))
}

fn materialize_new_candidates(
    app_root: &Path,
    journal: &BundleSetJournal,
    sources: Vec<(String, PathBuf, PathBuf)>,
) -> anyhow::Result<()> {
    for (name, tree_source, registration_source) in sources {
        let action = journal
            .actions
            .iter()
            .find(|action| action.bundle_name == name)
            .ok_or_else(|| anyhow::anyhow!("prepared candidate has no journal action"))?;
        let tree_target =
            recovery_path(app_root, action.new_tree_staging_path.as_deref().unwrap())?;
        let registration_target = recovery_path(
            app_root,
            action.new_registration_staging_path.as_deref().unwrap(),
        )?;
        create_parent(&tree_target)?;
        create_parent(&registration_target)?;
        lillux::rename_path_noreplace_durable(&tree_source, &tree_target)?;
        lillux::rename_path_noreplace_durable(&registration_source, &registration_target)?;
        require_tree_digest(&tree_target, action.new_tree_digest.as_deref().unwrap())?;
        require_file_digest(
            &registration_target,
            action.new_registration_digest.as_deref().unwrap(),
        )?;
    }
    Ok(())
}

fn activate_trees(app_root: &Path, journal: &BundleSetJournal) -> anyhow::Result<()> {
    for action in &journal.actions {
        if matches!(action.kind, BundleSetActionKind::Keep) {
            continue;
        }
        let live = recovery_path(
            app_root,
            &format!("{}/bundles/{}", ryeos_engine::AI_DIR, action.bundle_name),
        )?;
        if let Some(backup) = action.old_tree_backup_path.as_deref() {
            let backup = recovery_path(app_root, backup)?;
            create_parent(&backup)?;
            if live.exists() && !backup.exists() {
                lillux::rename_path_noreplace_durable(&live, &backup)?;
            }
            require_tree_digest(&backup, action.old_tree_digest.as_deref().unwrap())?;
        }
        if let Some(staging) = action.new_tree_staging_path.as_deref() {
            let staging = recovery_path(app_root, staging)?;
            if !live.exists() {
                lillux::rename_path_noreplace_durable(&staging, &live)?;
            }
            require_tree_digest(&live, action.new_tree_digest.as_deref().unwrap())?;
        }
    }
    Ok(())
}

fn commit_registrations(app_root: &Path, journal: &BundleSetJournal) -> anyhow::Result<()> {
    for action in &journal.actions {
        if matches!(action.kind, BundleSetActionKind::Keep) {
            continue;
        }
        let live = recovery_path(
            app_root,
            &format!(
                "{}/node/bundles/{}.yaml",
                ryeos_engine::AI_DIR,
                action.bundle_name
            ),
        )?;
        if let Some(backup) = action.old_registration_backup_path.as_deref() {
            let backup = recovery_path(app_root, backup)?;
            create_parent(&backup)?;
            if live.exists() && !backup.exists() {
                lillux::rename_path_noreplace_durable(&live, &backup)?;
            }
            require_file_digest(&backup, action.old_registration_digest.as_deref().unwrap())?;
        }
        if let Some(staging) = action.new_registration_staging_path.as_deref() {
            let staging = recovery_path(app_root, staging)?;
            if !live.exists() {
                lillux::rename_path_noreplace_durable(&staging, &live)?;
            }
            require_file_digest(&live, action.new_registration_digest.as_deref().unwrap())?;
        }
    }
    Ok(())
}

fn create_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("transaction path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    Ok(())
}

fn validate_completion_bytes(
    label: &str,
    encoded: &str,
    expected_hash: &str,
) -> anyhow::Result<()> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!("{label} bytes are not canonical base64"))?;
    if bytes.len() > COMPLETION_MAX_BYTES {
        bail!("{label} exceeds size bound");
    }
    if base64::engine::general_purpose::STANDARD.encode(&bytes) != encoded {
        bail!("{label} bytes are not canonical base64");
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected_hash {
        bail!("{label} bytes disagree with journal digest");
    }
    Ok(())
}

fn validate_recovery_relative_path(label: &str, value: &str) -> anyhow::Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 512
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("{label} must be a bounded canonical relative path");
    }
    Ok(())
}

/// Result of verifying the completion record at startup. Verification includes
/// its operator signature and exact canonical body/hash identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionObservation {
    ExactOld,
    ExactNew,
    Absent,
    CorruptOrUnrelated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    RestoreExactOld,
    CompleteExactNew,
    RefuseOperatorRepair,
}

/// Decide recovery from the signed completion fence, never from journal phase.
pub fn decide_recovery(
    journal: &BundleSetJournal,
    completion: CompletionObservation,
) -> anyhow::Result<RecoveryDecision> {
    journal.validate()?;
    if journal.new_selection_may_have_executed {
        return match completion {
            CompletionObservation::ExactNew => Ok(RecoveryDecision::CompleteExactNew),
            _ => Ok(RecoveryDecision::RefuseOperatorRepair),
        };
    }
    Ok(match completion {
        CompletionObservation::ExactNew => RecoveryDecision::CompleteExactNew,
        CompletionObservation::ExactOld => RecoveryDecision::RestoreExactOld,
        CompletionObservation::Absent
            if journal.phase < BundleSetPhase::CompletionPublicationStarted =>
        {
            RecoveryDecision::RestoreExactOld
        }
        CompletionObservation::Absent | CompletionObservation::CorruptOrUnrelated => {
            RecoveryDecision::RefuseOperatorRepair
        }
    })
}

/// V1 has no post-execution rollback authorization.
pub fn authorize_restore_old(journal: &BundleSetJournal) -> anyhow::Result<()> {
    journal.validate()?;
    if journal.new_selection_may_have_executed {
        bail!("post-execution bundle-set rollback is unsupported in v1");
    }
    Ok(())
}

/// Exact filesystem owner used by startup recovery. Implementations consume
/// only the journal action manifest; they must verify every materialized tree
/// and registration digest before making it visible.
pub trait BundleSetRecoveryExecutor {
    fn restore_exact_old(&self, journal: &BundleSetJournal) -> anyhow::Result<()>;
    fn complete_exact_new(&self, journal: &BundleSetJournal) -> anyhow::Result<()>;
}

/// Startup executor for the layouts the current journal can prove without
/// inventing unstored filesystem coordinates.
///
/// Before `TreesActivated`, restore-old is exact while the old completion is
/// still present because no installed tree or registration has changed. An
/// absent completion cannot be reconstructed from its digest. Once activation
/// begins, v1's action manifest has digests but no durable backup paths, so
/// recovery refuses instead of guessing.
/// Exact-new completion is already fenced by the byte hash of the signed whole-
/// init completion; ordinary bootstrap verification immediately revalidates its
/// registrations and policy generation after this journal is consumed.
#[derive(Debug, Clone)]
pub struct BootstrapBundleSetRecoveryExecutor {
    app_root: PathBuf,
}

impl BootstrapBundleSetRecoveryExecutor {
    pub fn new(app_root: &Path) -> Self {
        Self {
            app_root: app_root.to_owned(),
        }
    }
}

impl BundleSetRecoveryExecutor for BootstrapBundleSetRecoveryExecutor {
    fn restore_exact_old(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        for action in journal.actions.iter().rev() {
            restore_old_action(&self.app_root, action)?;
        }
        ActiveBundleSelectionStore::new(&self.app_root).restore_old(journal)?;
        write_completion(&self.app_root, &journal.old_completion_bytes_base64)?;
        Ok(())
    }

    fn complete_exact_new(&self, journal: &BundleSetJournal) -> anyhow::Result<()> {
        for action in &journal.actions {
            complete_new_action(&self.app_root, action)?;
        }
        write_completion(&self.app_root, &journal.new_completion_bytes_base64)?;
        ActiveBundleSelectionStore::new(&self.app_root).commit_new(journal)?;
        Ok(())
    }
}

fn restore_old_action(app_root: &Path, action: &BundleSetAction) -> anyhow::Result<()> {
    let live_tree = recovery_path(
        app_root,
        &format!("{}/bundles/{}", ryeos_engine::AI_DIR, action.bundle_name),
    )?;
    let live_registration = recovery_path(
        app_root,
        &format!(
            "{}/node/bundles/{}.yaml",
            ryeos_engine::AI_DIR,
            action.bundle_name
        ),
    )?;
    match action.kind {
        BundleSetActionKind::Add => {
            remove_tree_if_present(&live_tree)?;
            remove_file_if_present(&live_registration)?;
        }
        BundleSetActionKind::Replace | BundleSetActionKind::Remove => {
            let tree_backup =
                recovery_path(app_root, action.old_tree_backup_path.as_deref().unwrap())?;
            let registration_backup = recovery_path(
                app_root,
                action.old_registration_backup_path.as_deref().unwrap(),
            )?;
            install_exact_tree(
                &tree_backup,
                &live_tree,
                action.old_tree_digest.as_deref().unwrap(),
            )?;
            install_exact_file(
                &registration_backup,
                &live_registration,
                action.old_registration_digest.as_deref().unwrap(),
            )?;
        }
        BundleSetActionKind::Keep => {
            require_tree_digest(&live_tree, action.old_tree_digest.as_deref().unwrap())?;
            require_file_digest(
                &live_registration,
                action.old_registration_digest.as_deref().unwrap(),
            )?;
        }
    }
    Ok(())
}

fn complete_new_action(app_root: &Path, action: &BundleSetAction) -> anyhow::Result<()> {
    let live_tree = recovery_path(
        app_root,
        &format!("{}/bundles/{}", ryeos_engine::AI_DIR, action.bundle_name),
    )?;
    let live_registration = recovery_path(
        app_root,
        &format!(
            "{}/node/bundles/{}.yaml",
            ryeos_engine::AI_DIR,
            action.bundle_name
        ),
    )?;
    match action.kind {
        BundleSetActionKind::Add | BundleSetActionKind::Replace => {
            let tree_staging =
                recovery_path(app_root, action.new_tree_staging_path.as_deref().unwrap())?;
            let registration_staging = recovery_path(
                app_root,
                action.new_registration_staging_path.as_deref().unwrap(),
            )?;
            install_exact_tree(
                &tree_staging,
                &live_tree,
                action.new_tree_digest.as_deref().unwrap(),
            )?;
            install_exact_file(
                &registration_staging,
                &live_registration,
                action.new_registration_digest.as_deref().unwrap(),
            )?;
        }
        BundleSetActionKind::Remove => {
            remove_tree_if_present(&live_tree)?;
            remove_file_if_present(&live_registration)?;
        }
        BundleSetActionKind::Keep => {
            require_tree_digest(&live_tree, action.new_tree_digest.as_deref().unwrap())?;
            require_file_digest(
                &live_registration,
                action.new_registration_digest.as_deref().unwrap(),
            )?;
        }
    }
    Ok(())
}

fn recovery_path(app_root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    validate_recovery_relative_path("recovery coordinate", relative)?;
    let mut current = app_root.to_owned();
    for component in Path::new(relative).components() {
        let std::path::Component::Normal(component) = component else {
            unreachable!()
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("bundle-set recovery coordinate contains a symlink")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn install_exact_tree(source: &Path, target: &Path, expected: &str) -> anyhow::Result<()> {
    if target.is_dir() && crate::bundle_transaction::tree_digest(target)? == expected {
        return Ok(());
    }
    require_tree_digest(source, expected)?;
    remove_tree_if_present(target)?;
    lillux::rename_path_durable(source, target)?;
    require_tree_digest(target, expected)
}

fn install_exact_file(source: &Path, target: &Path, expected: &str) -> anyhow::Result<()> {
    if target.is_file() && file_digest(target)? == expected {
        return Ok(());
    }
    require_file_digest(source, expected)?;
    remove_file_if_present(target)?;
    lillux::rename_path_durable(source, target)?;
    require_file_digest(target, expected)
}

fn require_tree_digest(path: &Path, expected: &str) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || crate::bundle_transaction::tree_digest(path)? != expected
    {
        bail!("bundle-set recovery tree digest mismatch");
    }
    Ok(())
}

fn require_file_digest(path: &Path, expected: &str) -> anyhow::Result<()> {
    if !path.is_file() || file_digest(path)? != expected {
        bail!("bundle-set recovery registration digest mismatch");
    }
    Ok(())
}

fn file_digest(path: &Path) -> anyhow::Result<String> {
    let bytes = lillux::read_regular_file_bounded_no_follow(path, JOURNAL_MAX_BYTES)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn remove_tree_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            lillux::remove_dir_all_durable(path)
        }
        Ok(_) => bail!("bundle-set recovery tree target is not a safe directory"),
        Err(error) => Err(error.into()),
    }
}

fn remove_file_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            lillux::remove_file_durable(path)
        }
        Ok(_) => bail!("bundle-set recovery registration target is not a safe file"),
        Err(error) => Err(error.into()),
    }
}

fn write_completion(app_root: &Path, encoded: &str) -> anyhow::Result<()> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    lillux::atomic_write_private(&init_completion_path(app_root), &bytes)
}

pub fn recover_for_bootstrap(
    app_root: &Path,
    registry_lock: &BundleRegistryMutationLock,
    executor: &dyn BundleSetRecoveryExecutor,
) -> anyhow::Result<Option<RecoveryDecision>> {
    registry_lock.ensure_protects_app_root(app_root)?;
    let Some(journal) = load_journal(app_root)? else {
        return Ok(None);
    };
    let completion = observe_completion(app_root, &journal)?;
    let decision = decide_recovery(&journal, completion)?;
    match decision {
        RecoveryDecision::RestoreExactOld => {
            authorize_restore_old(&journal)?;
            executor.restore_exact_old(&journal)?;
            consume_journal(app_root)?;
        }
        RecoveryDecision::CompleteExactNew => executor.complete_exact_new(&journal)?,
        RecoveryDecision::RefuseOperatorRepair => {
            bail!("bundle-set startup recovery requires explicit operator repair")
        }
    }
    Ok(Some(decision))
}

fn require_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be a lowercase 64-hex digest");
    }
    Ok(())
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn completion(value: &[u8]) -> (String, String) {
        (
            format!("{:x}", Sha256::digest(value)),
            base64::engine::general_purpose::STANDARD.encode(value),
        )
    }

    fn journal(phase: BundleSetPhase) -> BundleSetJournal {
        let (old_completion_bytes_hash, old_completion_bytes_base64) = completion(b"old");
        let (new_completion_bytes_hash, new_completion_bytes_base64) = completion(b"new");
        let transaction_id = hash('a');
        let recovery_root = format!(
            "{}/transactions/bundle-set/{transaction_id}",
            ryeos_engine::AI_DIR
        );
        BundleSetJournal {
            schema: BundleSetJournal::SCHEMA,
            transaction_id,
            selection_hash: hash('b'),
            expected_active_selection: Some(hash('c')),
            old_completion_hash: hash('d'),
            old_completion_bytes_hash,
            old_completion_bytes_base64,
            new_completion_hash: hash('f'),
            new_completion_bytes_hash,
            new_completion_bytes_base64,
            old_installed_set_digest: hash('2'),
            new_installed_set_digest: hash('3'),
            actions: vec![BundleSetAction {
                bundle_name: "standard".to_owned(),
                kind: BundleSetActionKind::Replace,
                old_tree_digest: Some(hash('4')),
                new_tree_digest: Some(hash('5')),
                old_registration_digest: Some(hash('6')),
                new_registration_digest: Some(hash('7')),
                old_tree_backup_path: Some(format!("{recovery_root}/old/trees/standard")),
                new_tree_staging_path: Some(format!("{recovery_root}/new/trees/standard")),
                old_registration_backup_path: Some(format!(
                    "{recovery_root}/old/registrations/standard.yaml"
                )),
                new_registration_staging_path: Some(format!(
                    "{recovery_root}/new/registrations/standard.yaml"
                )),
            }],
            phase,
            new_selection_may_have_executed: false,
        }
    }

    #[test]
    fn exact_new_completion_always_recovers_forward() {
        for phase in [
            BundleSetPhase::Prepared,
            BundleSetPhase::TreesActivated,
            BundleSetPhase::CompletionPublicationStarted,
            BundleSetPhase::Complete,
        ] {
            assert_eq!(
                decide_recovery(&journal(phase), CompletionObservation::ExactNew).unwrap(),
                RecoveryDecision::CompleteExactNew
            );
        }
    }

    #[test]
    fn absent_completion_restores_old_only_before_publication_started() {
        assert_eq!(
            decide_recovery(
                &journal(BundleSetPhase::ActiveSelectionStaged),
                CompletionObservation::Absent
            )
            .unwrap(),
            RecoveryDecision::RestoreExactOld
        );
        assert_eq!(
            decide_recovery(
                &journal(BundleSetPhase::CompletionPublicationStarted),
                CompletionObservation::Absent
            )
            .unwrap(),
            RecoveryDecision::RefuseOperatorRepair
        );
    }

    #[test]
    fn unrelated_completion_fails_closed() {
        assert_eq!(
            decide_recovery(
                &journal(BundleSetPhase::Prepared),
                CompletionObservation::CorruptOrUnrelated
            )
            .unwrap(),
            RecoveryDecision::RefuseOperatorRepair
        );
    }

    #[test]
    fn post_execution_restore_is_refused() {
        let mut value = journal(BundleSetPhase::Complete);
        value.mark_new_selection_executed().unwrap();
        assert!(authorize_restore_old(&value).is_err());
        assert_eq!(
            decide_recovery(&value, CompletionObservation::ExactOld).unwrap(),
            RecoveryDecision::RefuseOperatorRepair
        );
    }

    #[test]
    fn phases_cannot_skip() {
        let mut value = journal(BundleSetPhase::Prepared);
        assert!(value.advance(BundleSetPhase::Admitted).is_err());
        value.advance(BundleSetPhase::Materialized).unwrap();
    }

    #[test]
    fn recovery_paths_reject_parent_traversal() {
        let mut value = journal(BundleSetPhase::Prepared);
        value.actions[0].old_tree_backup_path = Some("../outside".into());
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("relative path")
        );
    }
}
