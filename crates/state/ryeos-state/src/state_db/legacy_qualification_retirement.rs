//! Exact offline retirement of predecessor product-qualification heads.
//!
//! This is a signed-head retirement, not a predecessor qualification reader.
//! The caller holds the stopped-node state lock, runtime namespace lock, and
//! exclusive CAS guard for the entire operation. An independently copied full
//! state archive is checked before the first durable no-start intent.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::StateDb;
use crate::recovery::CasMutationGuard;

const NAMESPACE: &str = "retained-product-qualifications";
// The release authority retains v5 heads; the two disposable A/B nodes retain
// v9 heads. These are exact offline retirement targets, not accepted evidence
// for current qualification. Every other predecessor or current schema stays
// outside this operation's reviewed scope.
const RETIRED_EVIDENCE_SCHEMA_V5: &str = "ryeos.product_qualification_evidence.v5";
const RETIRED_EVIDENCE_SCHEMA_V9: &str = "ryeos.product_qualification_evidence.v9";
fn reviewed_retired_evidence_schema(schema: &str) -> bool {
    schema == RETIRED_EVIDENCE_SCHEMA_V5 || schema == RETIRED_EVIDENCE_SCHEMA_V9
}
// v2 retains each moved signed head in an exact recovery slot. v1 unlink
// intents cannot be interpreted as move outcomes on retry.
const INTENT_SCHEMA: u32 = 2;
const MARKER: &str = "legacy-product-qualification-retirement.json";
const RETIRED_HEADS: &str = "legacy-product-qualification-retired-heads";
const MAX_MARKER_BYTES: u64 = 256 * 1024;
const MAX_HEAD_BYTES: u64 = 16 * 1024;
const MAX_ATTESTATION_BYTES: u64 = 1024 * 1024;
const MAX_HEADS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyQualificationHead {
    pub coordinate: String,
    pub head_sha256: String,
    pub target_hash: String,
    pub signer: String,
    pub evidence_schema: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyQualificationArchiveReceipt {
    pub archive_state_root: String,
    pub archive_device: u64,
    pub archive_inode: u64,
    pub tree_sha256: String,
    pub file_count: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyQualificationRetirementPreview {
    pub plan_sha256: String,
    pub heads: Vec<LegacyQualificationHead>,
    pub retirement_in_progress: bool,
    pub archive_required_for_confirmation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RowPhase {
    Ready,
    Removing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetirementIntent {
    schema: u32,
    plan_sha256: String,
    heads: Vec<LegacyQualificationHead>,
    archive: LegacyQualificationArchiveReceipt,
    next_row: usize,
    row_phase: RowPhase,
}

impl RetirementIntent {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == INTENT_SCHEMA,
            "unsupported qualification retirement intent"
        );
        ensure!(
            !self.heads.is_empty() && self.heads.len() <= MAX_HEADS,
            "qualification retirement intent has invalid head count"
        );
        ensure!(
            self.plan_sha256 == plan_hash(&self.heads)? && self.next_row <= self.heads.len(),
            "qualification retirement intent changed its exact plan"
        );
        ensure!(
            self.next_row < self.heads.len() || self.row_phase == RowPhase::Ready,
            "qualification retirement intent has invalid terminal row phase"
        );
        let mut previous = None::<&str>;
        for head in &self.heads {
            ensure!(
                reviewed_retired_evidence_schema(&head.evidence_schema)
                    && head.coordinate.len() == 64
                    && head
                        .coordinate
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                    && previous.is_none_or(|prior| prior < head.coordinate.as_str()),
                "qualification retirement intent has an unsupported head"
            );
            for value in [&head.head_sha256, &head.target_hash, &head.signer] {
                ensure!(
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                    "qualification retirement intent has a malformed hash"
                );
            }
            previous = Some(&head.coordinate);
        }
        ensure!(
            self.archive.file_count > 0
                && self.archive.total_bytes > 0
                && self.archive.tree_sha256.len() == 64
                && self
                    .archive
                    .tree_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "qualification retirement archive receipt is invalid"
        );
        Ok(())
    }
}

pub(super) fn retirement_in_progress(directory: &lillux::PinnedDirectory) -> Result<bool> {
    Ok(read_intent(directory)?.is_some())
}

fn read_intent(directory: &lillux::PinnedDirectory) -> Result<Option<RetirementIntent>> {
    let Some(file) = directory.open_pinned_regular(OsStr::new(MARKER), false)? else {
        return Ok(None);
    };
    let bytes = file.read_bounded(MAX_MARKER_BYTES)?;
    let intent: RetirementIntent = serde_json::from_slice(&bytes)?;
    intent.validate()?;
    ensure!(
        lillux::canonical_json(&serde_json::to_value(&intent)?)?.as_bytes() == bytes.as_slice(),
        "qualification retirement intent is not canonical"
    );
    Ok(Some(intent))
}

fn plan_hash(heads: &[LegacyQualificationHead]) -> Result<String> {
    let value = serde_json::json!({
        "domain":"ryeos.legacy_product_qualification_retirement.v1",
        "namespace":NAMESPACE,
        "heads":heads,
    });
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&value)?.as_bytes(),
    ))
}

impl StateDb {
    /// Signed-head and target-CAS inspection only; no predecessor evidence is
    /// treated as qualification. This dry-run does not require an archive.
    pub fn preview_legacy_product_qualification_retirement(
        &self,
        node_key: &lillux::crypto::VerifyingKey,
        guard: &CasMutationGuard,
    ) -> Result<LegacyQualificationRetirementPreview> {
        self.ensure_cas_mutation_guard(guard)?;
        ensure!(
            guard.is_exclusive(),
            "qualification retirement requires exclusive CAS guard"
        );
        if let Some(intent) = read_intent(&self._recovery_directory)? {
            let node_fingerprint = lillux::crypto::fingerprint(node_key);
            ensure!(
                intent
                    .heads
                    .iter()
                    .all(|row| row.signer == node_fingerprint),
                "qualification retirement intent belongs to another node"
            );
            return Ok(LegacyQualificationRetirementPreview {
                plan_sha256: intent.plan_sha256,
                heads: intent.heads,
                retirement_in_progress: true,
                archive_required_for_confirmation: true,
            });
        }
        let heads = self.inspect_legacy_product_qualification_heads(node_key)?;
        ensure!(
            !heads.is_empty(),
            "no retired-schema product qualification heads exist"
        );
        Ok(LegacyQualificationRetirementPreview {
            plan_sha256: plan_hash(&heads)?,
            heads,
            retirement_in_progress: false,
            archive_required_for_confirmation: true,
        })
    }

    fn inspect_legacy_product_qualification_heads(
        &self,
        node_key: &lillux::crypto::VerifyingKey,
    ) -> Result<Vec<LegacyQualificationHead>> {
        let node_fingerprint = lillux::crypto::fingerprint(node_key);
        let refs = crate::refs::list_verified_generic_head_refs_in_directory(
            &self._refs_directory,
            NAMESPACE,
            &self.trust_store,
        )?;
        ensure!(
            refs.len() <= MAX_HEADS,
            "too many predecessor qualification heads"
        );
        let cas = self.pinned_cas()?;
        let mut rows = Vec::with_capacity(refs.len());
        for head in refs {
            ensure!(
                head.namespace == NAMESPACE && head.signer == node_fingerprint,
                "qualification head has another namespace or signer"
            );
            ensure!(
                head.name.len() == 64
                    && head
                        .name
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "qualification head has an invalid coordinate"
            );
            let raw = raw_head(&self._refs_directory, &head.name)?
                .context("verified qualification head disappeared")?;
            let attestation = cas
                .get_object_bounded(&head.target_hash, MAX_ATTESTATION_BYTES)?
                .context("qualification head target CAS object is missing")?;
            let attestation = crate::objects::Attestation::from_value(&attestation)?;
            attestation.verify_with_key(node_key)?;
            ensure!(
                attestation.claim == crate::external_content::products::qualification::PRODUCT_QUALIFICATION_CLAIM
                    && attestation.policy == crate::external_content::products::qualification::PRODUCT_QUALIFICATION_ATTESTATION_POLICY,
                "qualification head does not target product qualification testimony"
            );
            let schema = attestation
                .evidence
                .get("schema")
                .and_then(serde_json::Value::as_str)
                .context("retired qualification evidence has no structural schema")?;
            ensure!(
                reviewed_retired_evidence_schema(schema),
                "qualification head has an unreviewed evidence schema"
            );
            rows.push(LegacyQualificationHead {
                coordinate: head.name,
                head_sha256: lillux::sha256_hex(&raw),
                target_hash: head.target_hash,
                signer: head.signer,
                evidence_schema: schema.to_owned(),
            });
        }
        rows.sort_by(|left, right| left.coordinate.cmp(&right.coordinate));
        Ok(rows)
    }

    /// Simulate the signed-head closure after this namespace is retired,
    /// while every live head is still present. A failure here must precede
    /// the durable intent and the first destructive ref operation.
    fn verify_remaining_signed_head_closure(&self) -> Result<()> {
        crate::refs::validate_authoritative_ref_namespaces_in_directory(&self._refs_directory)?;
        let mut roots = BTreeSet::new();
        for head in crate::refs::list_verified_generic_head_refs_in_directory(
            &self._refs_directory,
            "",
            &self.trust_store,
        )? {
            if head.namespace != NAMESPACE {
                roots.insert(head.target_hash);
            }
        }
        for head in crate::refs::list_verified_project_head_refs_in_directory(
            &self._refs_directory,
            &self.trust_store,
        )? {
            roots.insert(head.signed_ref.target_hash);
        }
        for head in crate::refs::list_verified_deployed_project_refs_in_directory(
            &self._refs_directory,
            &self.trust_store,
        )? {
            roots.insert(head.signed_ref.target_hash);
        }
        let closure =
            crate::object_closure::collect_object_closure_with_cas(&self.pinned_cas()?, roots)?;
        if !closure.is_complete() {
            if let Some(incompatible) = closure.decisive_incompatible_current_schema() {
                bail!("post-retirement signed-head closure is incompatible: {incompatible}");
            }
            bail!(
                "post-retirement signed-head closure is incomplete: missing={}, missing_blobs={}, malformed={}, unsupported={}",
                closure.missing_objects.len(),
                closure.missing_blobs.len(),
                closure.malformed_objects.len(),
                closure.unsupported_objects.len(),
            );
        }
        Ok(())
    }

    /// Retire only the exact reviewed predecessor heads after a full copied
    /// state archive has been checked. Any interruption leaves a startup fence.
    pub fn retire_legacy_product_qualification_heads(
        &self,
        node_key: &lillux::crypto::VerifyingKey,
        expected_plan_sha256: &str,
        archive_state_root: &Path,
        guard: &CasMutationGuard,
    ) -> Result<LegacyQualificationArchiveReceipt> {
        self.ensure_cas_mutation_guard(guard)?;
        ensure!(
            guard.is_exclusive(),
            "qualification retirement requires exclusive CAS guard"
        );
        self.verify_remaining_signed_head_closure()?;
        let archive = open_archive(archive_state_root, &self._runtime_state_directory)?;
        let mut intent = if let Some(retained) = read_intent(&self._recovery_directory)? {
            ensure!(
                retained.plan_sha256 == expected_plan_sha256,
                "qualification retirement plan changed"
            );
            let receipt = digest_tree(&archive, None)?;
            ensure!(
                receipt == retained.archive,
                "qualification retirement archive changed on retry"
            );
            retained
        } else {
            let preview = self.preview_legacy_product_qualification_retirement(node_key, guard)?;
            ensure!(
                preview.plan_sha256 == expected_plan_sha256,
                "qualification retirement dry-run plan changed"
            );
            let receipt = digest_tree(&archive, Some(&self._runtime_state_directory))?;
            let intent = RetirementIntent {
                schema: INTENT_SCHEMA,
                plan_sha256: preview.plan_sha256,
                heads: preview.heads,
                archive: receipt,
                next_row: 0,
                row_phase: RowPhase::Ready,
            };
            intent.validate()?;
            let bytes = lillux::canonical_json(&serde_json::to_value(&intent)?)?;
            ensure!(
                bytes.len() as u64 <= MAX_MARKER_BYTES,
                "qualification retirement intent exceeds bound"
            );
            self._recovery_directory
                .atomic_create_regular(OsStr::new(MARKER), bytes.as_bytes(), 0o600)?
                .context("qualification retirement intent appeared concurrently")?;
            intent
        };
        ensure!(
            intent
                .heads
                .iter()
                .all(|row| row.signer == lillux::crypto::fingerprint(node_key)),
            "qualification retirement intent belongs to another node"
        );
        // The intent precedes this directory and every move into it. A moved
        // head remains available by exact bytes after a crash, while strict
        // refs enumeration sees only the original per-head lock anchor.
        let retired_parent = self
            ._recovery_directory
            .open_or_create_child(OsStr::new(RETIRED_HEADS), 0o700)?;
        let retired_root =
            retired_parent.open_or_create_child(OsStr::new(&intent.plan_sha256), 0o700)?;
        for name in retired_root.entry_names()? {
            let coordinate = name
                .to_str()
                .context("retired qualification head has a non-UTF8 coordinate")?;
            ensure!(
                intent.heads.iter().any(|row| row.coordinate == coordinate),
                "unreviewed retired qualification head slot exists"
            );
        }
        let current = self.inspect_legacy_product_qualification_heads(node_key)?;
        for row in &current {
            ensure!(
                intent.heads.contains(row),
                "unreviewed qualification head appeared during retirement"
            );
        }
        for (index, row) in intent.heads.iter().enumerate() {
            let retired = retired_root.open_or_create_child(OsStr::new(&row.coordinate), 0o700)?;
            let retired_bytes = retired_head(&retired)?;
            if let Some(bytes) = retired_bytes.as_ref() {
                ensure!(
                    lillux::sha256_hex(bytes) == row.head_sha256,
                    "retired qualification head changed its exact bytes"
                );
            }
            let live = current.contains(row);
            ensure!(
                live != retired_bytes.is_some(),
                "qualification head has contradictory live and retired locations"
            );
            if index < intent.next_row {
                ensure!(!live, "previously retired qualification head reappeared");
            } else if index > intent.next_row
                || (index == intent.next_row && intent.row_phase == RowPhase::Ready)
            {
                ensure!(live, "qualification head moved before its removal intent");
            }
        }
        while intent.next_row < intent.heads.len() {
            let row = &intent.heads[intent.next_row];
            let retired = retired_root.open_or_create_child(OsStr::new(&row.coordinate), 0o700)?;
            if intent.row_phase == RowPhase::Ready {
                ensure!(
                    current.contains(row),
                    "qualification head disappeared before removal intent"
                );
                intent.row_phase = RowPhase::Removing;
                write_intent(&self._recovery_directory, &intent)?;
            }
            if current.contains(row) {
                let lock = crate::refs::GenericHeadLock::acquire_in_refs_directory(
                    &self._refs_directory,
                    NAMESPACE,
                    &row.coordinate,
                )?;
                let head = crate::refs::read_verified_generic_head_ref_in_directory(
                    &self._refs_directory,
                    NAMESPACE,
                    &row.coordinate,
                    &self.trust_store,
                )?
                .context("qualification head disappeared under retirement lock")?;
                ensure!(
                    head.target_hash == row.target_hash && head.signer == row.signer,
                    "qualification head changed under retirement lock"
                );
                ensure!(
                    crate::refs::move_generic_head_ref_to_retirement_directory(
                        &self._refs_directory,
                        NAMESPACE,
                        &row.coordinate,
                        &lock,
                        &retired,
                        &row.head_sha256,
                        MAX_HEAD_BYTES,
                    )?,
                    "qualification signed head could not move into its exact retirement slot"
                );
                ensure!(
                    raw_head(&self._refs_directory, &row.coordinate)?.is_none(),
                    "qualification signed head remained live after retirement move"
                );
            }
            ensure!(
                retired_head(&retired)?
                    .is_some_and(|bytes| lillux::sha256_hex(&bytes) == row.head_sha256),
                "qualification retirement slot lost its exact signed head"
            );
            intent.next_row += 1;
            intent.row_phase = RowPhase::Ready;
            write_intent(&self._recovery_directory, &intent)?;
        }
        ensure!(
            crate::refs::list_verified_generic_head_refs_in_directory(
                &self._refs_directory,
                NAMESPACE,
                &self.trust_store
            )?
            .is_empty(),
            "qualification namespace retained an unreviewed head"
        );
        crate::reachability::collect_reachable_pinned(
            &self.pinned_cas()?,
            &self._refs_directory,
            &self.trust_store,
        )
        .context(
            "current signed-head closure remains incompatible after qualification retirement",
        )?;
        ensure!(
            digest_tree(&archive, None)? == intent.archive,
            "qualification retirement archive changed before completion"
        );
        let marker = self
            ._recovery_directory
            .open_regular(OsStr::new(MARKER), false)?
            .context("qualification retirement intent disappeared before completion")?;
        self._recovery_directory
            .remove_if_same(OsStr::new(MARKER), &marker)?;
        Ok(intent.archive)
    }
}

fn raw_head(refs: &lillux::PinnedDirectory, coordinate: &str) -> Result<Option<Vec<u8>>> {
    let Some(generic) = refs.open_child_directory(OsStr::new("generic"))? else {
        return Ok(None);
    };
    let Some(namespace) = generic.open_child_directory(OsStr::new(NAMESPACE))? else {
        return Ok(None);
    };
    let Some(head_dir) = namespace.open_child_directory(OsStr::new(coordinate))? else {
        return Ok(None);
    };
    head_dir
        .open_pinned_regular(OsStr::new("head"), false)?
        .map(|file| file.read_bounded(MAX_HEAD_BYTES))
        .transpose()
}

fn retired_head(directory: &lillux::PinnedDirectory) -> Result<Option<Vec<u8>>> {
    for name in directory.entry_names()? {
        ensure!(
            name.as_os_str() == OsStr::new("head"),
            "retired qualification head slot has an unexpected entry"
        );
    }
    directory
        .open_pinned_regular(OsStr::new("head"), false)?
        .map(|file| file.read_bounded(MAX_HEAD_BYTES))
        .transpose()
}

fn write_intent(directory: &lillux::PinnedDirectory, intent: &RetirementIntent) -> Result<()> {
    intent.validate()?;
    let bytes = lillux::canonical_json(&serde_json::to_value(intent)?)?;
    ensure!(
        bytes.len() as u64 <= MAX_MARKER_BYTES,
        "qualification retirement intent exceeds bound"
    );
    let previous = directory
        .open_regular(OsStr::new(MARKER), false)?
        .context("qualification retirement intent disappeared")?;
    directory.atomic_write_if_same(OsStr::new(MARKER), Some(&previous), bytes.as_bytes(), 0o600)?;
    Ok(())
}

fn open_archive(path: &Path, source: &lillux::PinnedDirectory) -> Result<lillux::PinnedDirectory> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
        "archive state root must be an exact absolute path"
    );
    ensure!(
        !path.starts_with(source.path()) && !source.path().starts_with(path),
        "archive state root overlaps live state"
    );
    let archive = lillux::PinnedDirectory::open(path)?.context("archive state root is absent")?;
    archive.ensure_path_binding()?;
    ensure!(
        archive.device_inode()? != source.device_inode()?,
        "archive aliases live state root"
    );
    Ok(archive)
}

fn digest_tree(
    archive: &lillux::PinnedDirectory,
    source: Option<&lillux::PinnedDirectory>,
) -> Result<LegacyQualificationArchiveReceipt> {
    let mut hasher = Sha256::new();
    let mut files = 0u64;
    let mut bytes = 0u64;
    walk_tree(archive, source, "", &mut hasher, &mut files, &mut bytes)?;
    archive.ensure_path_binding()?;
    if let Some(source) = source {
        source.ensure_path_binding()?;
    }
    let (archive_device, archive_inode) = archive.device_inode()?;
    Ok(LegacyQualificationArchiveReceipt {
        archive_state_root: archive.path().to_string_lossy().into_owned(),
        archive_device,
        archive_inode,
        tree_sha256: format!("{:x}", hasher.finalize()),
        file_count: files,
        total_bytes: bytes,
    })
}

fn walk_tree(
    archive: &lillux::PinnedDirectory,
    source: Option<&lillux::PinnedDirectory>,
    prefix: &str,
    hasher: &mut Sha256,
    files: &mut u64,
    bytes: &mut u64,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let archived_metadata = archive.try_clone_descriptor()?.metadata()?;
    let mode = archived_metadata.mode() & 0o7777;
    if let Some(source) = source {
        let source_metadata = source.try_clone_descriptor()?.metadata()?;
        ensure!(
            mode == source_metadata.mode() & 0o7777
                && archived_metadata.uid() == source_metadata.uid()
                && archived_metadata.gid() == source_metadata.gid()
                && (archived_metadata.dev(), archived_metadata.ino())
                    != (source_metadata.dev(), source_metadata.ino()),
            "archive directory metadata differs or aliases source"
        );
    }
    hasher.update(b"M\0");
    hasher.update(prefix.as_bytes());
    hasher.update(b"\0");
    hasher.update(mode.to_be_bytes());
    hasher.update(archived_metadata.uid().to_be_bytes());
    hasher.update(archived_metadata.gid().to_be_bytes());
    let mut names = archive.entry_names()?;
    names.sort();
    if let Some(source) = source {
        let mut source_names = source.entry_names()?;
        source_names.sort();
        ensure!(
            names == source_names,
            "archive state tree differs from source entries"
        );
    }
    for name in names {
        let part = name
            .to_str()
            .context("archive state has a non-UTF8 entry")?;
        let relative = if prefix.is_empty() {
            part.to_owned()
        } else {
            format!("{prefix}/{part}")
        };
        let archived = archive
            .open_entry(&name, false)?
            .context("archive state entry disappeared")?;
        let original = source
            .map(|dir| dir.open_entry(&name, false))
            .transpose()?
            .flatten();
        match (archived, original) {
            (
                lillux::PinnedDirectoryEntry::Directory(next),
                Some(lillux::PinnedDirectoryEntry::Directory(original)),
            ) => {
                hasher.update(b"D\0");
                hasher.update(relative.as_bytes());
                hasher.update(b"\0");
                walk_tree(&next, Some(&original), &relative, hasher, files, bytes)?;
            }
            (lillux::PinnedDirectoryEntry::Directory(next), None) if source.is_none() => {
                hasher.update(b"D\0");
                hasher.update(relative.as_bytes());
                hasher.update(b"\0");
                walk_tree(&next, None, &relative, hasher, files, bytes)?;
            }
            (
                lillux::PinnedDirectoryEntry::Regular(archived),
                Some(lillux::PinnedDirectoryEntry::Regular(original)),
            ) => {
                let (length, mode, uid, gid, digest) = digest_file(&archived, Some(&original))?;
                add_file_digest(
                    hasher, &relative, length, mode, uid, gid, &digest, files, bytes,
                )?;
            }
            (lillux::PinnedDirectoryEntry::Regular(archived), None) if source.is_none() => {
                let (length, mode, uid, gid, digest) = digest_file(&archived, None)?;
                add_file_digest(
                    hasher, &relative, length, mode, uid, gid, &digest, files, bytes,
                )?;
            }
            _ => bail!("archive state entry differs from source type: {relative}"),
        }
    }
    Ok(())
}

fn digest_file(
    archive: &std::fs::File,
    source: Option<&std::fs::File>,
) -> Result<(u64, u32, u32, u32, String)> {
    use std::os::unix::fs::MetadataExt as _;
    // Both descriptors come from no-follow traversal of their pinned parents.
    let mut archived = archive.try_clone()?;
    archived.seek(SeekFrom::Start(0))?;
    let metadata = archived.metadata()?;
    let mode = metadata.mode() & 0o7777;
    if let Some(source) = source {
        let mut original = source.try_clone()?;
        original.seek(SeekFrom::Start(0))?;
        let source_metadata = original.metadata()?;
        ensure!(
            metadata.len() == source_metadata.len()
                && mode == source_metadata.mode() & 0o7777
                && metadata.uid() == source_metadata.uid()
                && metadata.gid() == source_metadata.gid()
                && (metadata.dev(), metadata.ino())
                    != (source_metadata.dev(), source_metadata.ino()),
            "archive state file differs or aliases source"
        );
        let mut archived_buffer = [0u8; 64 * 1024];
        let mut source_buffer = [0u8; 64 * 1024];
        let mut hasher = Sha256::new();
        let mut observed = 0u64;
        loop {
            let size = archived.read(&mut archived_buffer)?;
            let source_size = original.read(&mut source_buffer)?;
            ensure!(
                size == source_size && archived_buffer[..size] == source_buffer[..size],
                "archive state file bytes differ from source"
            );
            if size == 0 {
                break;
            }
            observed = observed
                .checked_add(size as u64)
                .context("archive file size overflow")?;
            hasher.update(&archived_buffer[..size]);
        }
        ensure!(
            observed == metadata.len(),
            "archive file changed during comparison"
        );
        return Ok((
            metadata.len(),
            mode,
            metadata.uid(),
            metadata.gid(),
            format!("{:x}", hasher.finalize()),
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut observed = 0u64;
    loop {
        let size = archived.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        observed = observed
            .checked_add(size as u64)
            .context("archive file size overflow")?;
        hasher.update(&buffer[..size]);
    }
    ensure!(
        observed == metadata.len(),
        "archive file changed during inspection"
    );
    Ok((
        metadata.len(),
        mode,
        metadata.uid(),
        metadata.gid(),
        format!("{:x}", hasher.finalize()),
    ))
}

fn add_file_digest(
    hasher: &mut Sha256,
    path: &str,
    length: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    digest: &str,
    files: &mut u64,
    bytes: &mut u64,
) -> Result<()> {
    *files = files
        .checked_add(1)
        .context("archive file count overflow")?;
    *bytes = bytes
        .checked_add(length)
        .context("archive byte count overflow")?;
    hasher.update(b"F\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(length.to_be_bytes());
    hasher.update(mode.to_be_bytes());
    hasher.update(uid.to_be_bytes());
    hasher.update(gid.to_be_bytes());
    hasher.update(digest.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(coordinate: char) -> LegacyQualificationHead {
        LegacyQualificationHead {
            coordinate: coordinate.to_string().repeat(64),
            head_sha256: "a".repeat(64),
            target_hash: "b".repeat(64),
            signer: "c".repeat(64),
            evidence_schema: RETIRED_EVIDENCE_SCHEMA_V5.into(),
        }
    }

    #[test]
    fn retirement_intent_requires_sorted_exact_retired_rows() {
        let rows = vec![head('1'), head('2')];
        let mut intent = RetirementIntent {
            schema: INTENT_SCHEMA,
            plan_sha256: plan_hash(&rows).unwrap(),
            heads: rows,
            archive: LegacyQualificationArchiveReceipt {
                archive_state_root: "/archive/state".into(),
                archive_device: 1,
                archive_inode: 2,
                tree_sha256: "d".repeat(64),
                file_count: 1,
                total_bytes: 1,
            },
            next_row: 0,
            row_phase: RowPhase::Ready,
        };
        intent.validate().unwrap();
        intent.schema = 1;
        assert!(intent.validate().is_err());
        intent.schema = INTENT_SCHEMA;
        intent.heads.reverse();
        intent.plan_sha256 = plan_hash(&intent.heads).unwrap();
        assert!(intent.validate().is_err());
        intent.heads.reverse();
        intent.plan_sha256 = plan_hash(&intent.heads).unwrap();
        intent.heads[0].evidence_schema = RETIRED_EVIDENCE_SCHEMA_V9.into();
        intent.plan_sha256 = plan_hash(&intent.heads).unwrap();
        intent.validate().unwrap();
        intent.heads[0].evidence_schema = "ryeos.product_qualification_evidence.v14".into();
        intent.plan_sha256 = plan_hash(&intent.heads).unwrap();
        assert!(intent.validate().is_err());
        intent.heads[0].evidence_schema = RETIRED_EVIDENCE_SCHEMA_V5.into();
        intent.plan_sha256 = plan_hash(&intent.heads).unwrap();
        intent.next_row = intent.heads.len();
        intent.row_phase = RowPhase::Removing;
        assert!(intent.validate().is_err());
    }

    #[test]
    fn retirement_schema_allowlist_excludes_unreviewed_predecessors_and_current() {
        assert!(reviewed_retired_evidence_schema(RETIRED_EVIDENCE_SCHEMA_V5));
        assert!(reviewed_retired_evidence_schema(RETIRED_EVIDENCE_SCHEMA_V9));
        for schema in [
            "ryeos.product_qualification_evidence.v4",
            "ryeos.product_qualification_evidence.v8",
            "ryeos.product_qualification_evidence.v10",
            "ryeos.product_qualification_evidence.v14",
        ] {
            assert!(!reviewed_retired_evidence_schema(schema));
        }
    }

    #[test]
    fn full_archive_comparison_refuses_changed_bytes_and_aliases() {
        let temp = tempfile::TempDir::new().unwrap();
        let source_path = temp.path().join("source");
        let archive_path = temp.path().join("archive");
        std::fs::create_dir_all(source_path.join("objects")).unwrap();
        std::fs::create_dir_all(archive_path.join("objects")).unwrap();
        std::fs::write(source_path.join("objects/a"), b"signed evidence").unwrap();
        std::fs::write(archive_path.join("objects/a"), b"signed evidence").unwrap();
        let source = lillux::PinnedDirectory::open(&source_path)
            .unwrap()
            .unwrap();
        let archive = open_archive(&archive_path, &source).unwrap();
        let receipt = digest_tree(&archive, Some(&source)).unwrap();
        assert_eq!(receipt, digest_tree(&archive, None).unwrap());
        assert!(open_archive(&source_path, &source).is_err());
        std::fs::write(archive_path.join("objects/a"), b"changed evidence").unwrap();
        assert!(digest_tree(&archive, Some(&source)).is_err());
        assert_ne!(receipt, digest_tree(&archive, None).unwrap());
    }
}
