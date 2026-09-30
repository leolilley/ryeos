//! Exact, credential-free delivery body for the independently admitted guest
//! restoration verifier. Lillux retains and reads the executable descriptor;
//! this module owns only bounded directory-upload tar packaging. Sealing proves
//! byte identity, not signed admission or permission to contact a provider.

use anyhow::{Result, ensure};
use ryeos_external_execution_contract::staging_package::GuestStagingEntry;
use std::io::{Read, Write};

/// Point-verified product bytes and manifests for consumer packaging. Source
/// materialization leases stay with the executor until the private copy has
/// finished. This inventory is not admission or workspace writer exclusion.
pub struct ConsumerProductInventory {
    pub entries: Vec<GuestStagingEntry>,
    pub descriptors: std::collections::BTreeMap<String, lillux::InheritedDescriptorAuthority>,
}

/// Reuse the existing bounded guest traversal and normal/large manifest
/// verifier without requiring a Worker projection or bootstrap. `name` is the
/// fixed single-component archive root selected by the enclosing protocol.
pub fn inventory_consumer_product(
    name: &str,
    input: &ryeos_external_execution_contract::GuestMountInput,
    source: &lillux::InheritedDescriptorAuthority,
    manifest: &lillux::InheritedDescriptorAuthority,
    maximum_entries: usize,
    maximum_regular_bytes: u64,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<ConsumerProductInventory> {
    use ryeos_external_execution_contract::{
        GuestMountAccess, GuestMountContentAuthority, GuestMountKind, GuestMountRole,
        GuestProductManifestKind,
    };
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid consumer product archive root"
    );
    ensure!(
        input.role == GuestMountRole::Product && input.access == GuestMountAccess::ReadOnly,
        "consumer archive accepts only read-only products"
    );
    ensure!(
        source.inherited_descriptor().map_err(anyhow::Error::msg)? == input.descriptor,
        "consumer product source descriptor changed"
    );
    let GuestMountContentAuthority::ProductManifest {
        manifest_kind,
        manifest_hash,
        manifest_descriptor,
        manifest_bytes,
    } = &input.content_authority
    else {
        anyhow::bail!("consumer product has no retained manifest");
    };
    ensure!(
        manifest
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?
            == *manifest_descriptor,
        "consumer product manifest descriptor changed"
    );
    ensure!(
        input
            .bytes
            .checked_add(*manifest_bytes)
            .is_some_and(|total| total <= maximum_regular_bytes),
        "consumer product and manifest exceed content budget"
    );
    let (record, _) =
        manifest.read_regular_file_stable_bounded((*manifest_bytes).min(8 * 1024 * 1024))?;
    ensure!(
        record.len() as u64 == *manifest_bytes && lillux::sha256_hex(&record) == *manifest_hash,
        "consumer manifest inventory digest changed"
    );
    let record: serde_json::Value = serde_json::from_slice(&record)?;
    let content_entries = record
        .get("entry_count")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("consumer manifest has no entry count"))?;
    let overhead = if input.kind == GuestMountKind::Directory {
        2
    } else {
        1
    };
    ensure!(
        content_entries
            .checked_add(overhead)
            .is_some_and(|count| count <= maximum_entries as u64),
        "consumer product inventory exceeds entry budget"
    );
    let verify = || {
        ryeos_state::external_content::realization_verification::verify_staged_external_realization(
            source,
            manifest,
            match manifest_kind {
                GuestProductManifestKind::Content => {
                    ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
                }
                GuestProductManifestKind::LargeContent => {
                    ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                }
            },
            manifest_hash,
            *manifest_bytes,
            match input.kind {
                GuestMountKind::Directory => ryeos_state::objects::ExternalContentKind::Tree,
                GuestMountKind::RegularFile => ryeos_state::objects::ExternalContentKind::File,
            },
            input.bytes,
        )
    };
    ensure!(
        !deadline.has_elapsed(),
        "consumer inventory deadline exceeded"
    );
    verify()?;
    let mut entries = Vec::new();
    let mut total = 0;
    let mut descriptors = std::collections::BTreeMap::new();
    match input.kind {
        GuestMountKind::RegularFile => {
            ensure!(
                input.normalized_mode == Some(source.regular_file_observation()?.portable_mode()?),
                "consumer product file mode differs from retained input"
            );
            crate::guest_package_producer::inventory_root_file(
                source,
                name,
                &mut entries,
                &mut total,
                maximum_regular_bytes,
                deadline,
            )?;
            descriptors.insert(name.into(), source.clone());
        }
        GuestMountKind::Directory => {
            ensure!(
                input.normalized_mode.is_none(),
                "consumer tree has a file mode"
            );
            let root = source
                .try_clone_pinned_directory(std::path::PathBuf::from("<consumer-product>"))?;
            crate::guest_package_producer::inventory_tree(
                &root,
                name,
                // Product manifests bind children, not the container root.
                // The importer owns this read-only staging root. Use the
                // portable product-root mode, unlike a private base root;
                // this does not attest the source root's ownership or mode.
                0o755,
                true,
                &mut entries,
                &mut total,
                maximum_regular_bytes,
                deadline,
            )?;
            for entry in &entries {
                if let GuestStagingEntry::RegularFile { path, .. } = entry {
                    let relative = path
                        .strip_prefix(name)
                        .and_then(|path| path.strip_prefix('/'))
                        .ok_or_else(|| {
                            anyhow::anyhow!("consumer inventory lost its product root")
                        })?;
                    let file = root
                        .open_pinned_regular_descendant(std::path::Path::new(relative), false)?
                        .ok_or_else(|| anyhow::anyhow!("consumer inventory file disappeared"))?;
                    descriptors.insert(path.clone(), file.inherited_descriptor_authority()?);
                }
            }
        }
    }
    ensure!(
        total == input.bytes,
        "consumer inventory product byte count changed"
    );
    let record = format!("{name}-manifest.json");
    crate::guest_package_producer::inventory_root_file(
        manifest,
        &record,
        &mut entries,
        &mut total,
        maximum_regular_bytes,
        deadline,
    )?;
    descriptors.insert(record, manifest.clone());
    verify()?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer inventory deadline exceeded"
    );
    Ok(ConsumerProductInventory {
        entries,
        descriptors,
    })
}

/// Ephemeral custody of a streamed consumer upload. This is not an attempt
/// journal or admission proof. The retained qualification attempt owns contact
/// idempotency; the caller retains materialization leases while constructing it.
pub struct PreparedConsumerArchive {
    parent: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    root: lillux::PinnedDirectory,
    payload: lillux::PinnedRegularFile,
    bytes: u64,
    sha256: String,
    discarded: bool,
}

impl PreparedConsumerArchive {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Rejoin private byte custody to the exact immutable consumer attempt.
    /// The caller must separately authenticate the challenge and claim contact;
    /// deserializing it does not grant admission or permission to upload.
    pub fn delivery_descriptor_for_challenge(
        &self,
        challenge: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge,
    ) -> Result<lillux::InheritedDescriptorAuthority> {
        challenge.validate()?;
        ensure!(
            challenge.intent.upload_bytes == self.bytes
                && challenge.intent.upload_sha256 == self.sha256,
            "consumer delivery differs from immutable attempt upload"
        );
        self.delivery_descriptor()
    }

    /// The delivery reader must finish its own exact stable stream. A registered
    /// descriptor permits byte delivery, not another upload after uncertainty.
    pub fn delivery_descriptor(&self) -> Result<lillux::InheritedDescriptorAuthority> {
        let observation = self.payload.observation()?;
        ensure!(
            observation.size() == self.bytes
                && self.payload.digest_stable_exact(&observation)? == self.sha256,
            "prepared consumer archive changed before delivery"
        );
        self.payload.inherited_descriptor_authority()
    }

    pub fn discard(mut self) -> Result<()> {
        self.remove()?;
        self.discarded = true;
        Ok(())
    }

    fn remove(&self) -> Result<()> {
        self.root
            .remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(1, 1))?;
        ensure!(
            self.parent
                .remove_empty_child_if_same(&self.name, &self.root)?,
            "private consumer archive generation changed before cleanup"
        );
        Ok(())
    }
}

impl Drop for PreparedConsumerArchive {
    fn drop(&mut self) {
        if !self.discarded {
            let _ = self.remove();
        }
    }
}

/// Create a bounded private disk-backed archive rather than copying a complete
/// runtime into a Vec or memfd. Caller supplies a genuinely private scratch
/// parent, not an immutable realization or live project directory. This returns
/// no Ready/qualification claim; guest import and manifest checks are still due.
pub fn prepare_private_consumer_archive(
    parent: &lillux::PinnedDirectory,
    entries: &[GuestStagingEntry],
    descriptors: &std::collections::BTreeMap<&str, &lillux::InheritedDescriptorAuthority>,
    budget: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<PreparedConsumerArchive> {
    budget.validate()?;
    let maximum_framed_bytes = budget.maximum_framed_bytes;
    parent.require_owner_private_directory()?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer archive deadline exceeded"
    );
    let owner = parent.try_clone()?;
    let (name, root) = parent.create_unique_child("consumer-archive", 0o700)?;
    let result = (|| {
        root.require_owner_private_directory()?;
        let output =
            root.open_regular_create(std::ffi::OsStr::new("payload"), true, true, 0o600)?;
        let output = write_consumer_inventory_archive(
            output,
            entries,
            descriptors,
            budget.maximum_entries,
            budget.maximum_regular_bytes,
            maximum_framed_bytes,
            deadline,
        )?;
        lillux::set_open_regular_file_mode(&output, 0o400)?;
        output.sync_all()?;
        drop(output);
        root.sync_tree_bounded(lillux::DirectoryTraversalBudget::new(1, 1))?;
        let payload = root
            .open_pinned_regular(std::ffi::OsStr::new("payload"), false)?
            .ok_or_else(|| anyhow::anyhow!("private consumer archive disappeared"))?;
        let observation = payload.observation()?;
        let bytes = observation.size();
        ensure!(
            bytes > 0 && bytes <= maximum_framed_bytes,
            "consumer archive length exceeds bound"
        );
        let sha256 = payload.digest_stable_exact(&observation)?;
        ensure!(
            !deadline.has_elapsed(),
            "consumer archive deadline exceeded"
        );
        Ok((payload, bytes, sha256))
    })();
    match result {
        Ok((payload, bytes, sha256)) => Ok(PreparedConsumerArchive {
            parent: owner,
            name,
            root,
            payload,
            bytes,
            sha256,
            discarded: false,
        }),
        Err(error) => {
            let cleanup = (|| -> Result<()> {
                root.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
                    1, 1,
                ))?;
                ensure!(
                    owner.remove_empty_child_if_same(&name, &root)?,
                    "private consumer archive generation changed before cleanup"
                );
                Ok(())
            })();
            if let Err(cleanup) = cleanup {
                return Err(error.context(format!(
                    "private consumer archive cleanup failed: {cleanup:#}"
                )));
            }
            Err(error)
        }
    }
}

/// One exact regular member of a consumer archive. Admission and retained
/// materialization leases belong to the caller, not this packaging value.
pub struct ConsumerArchiveFile<'a> {
    pub path: &'a str,
    pub mode: u32,
    pub bytes: u64,
    pub sha256: &'a str,
    pub descriptor: &'a lillux::InheritedDescriptorAuthority,
}

/// Stream exact descriptor-backed members into a private unpublished writer.
/// The caller supplies protected content/framing budgets and must discard the
/// writer on error. This is packaging only: it neither admits inputs nor
/// authorizes contact. No Worker activation coordinates are synthesized.
pub fn write_consumer_archive<W: Write>(
    output: W,
    files: &[ConsumerArchiveFile<'_>],
    maximum_entries: usize,
    maximum_regular_bytes: u64,
    maximum_framed_bytes: u64,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<W> {
    ensure!(
        !files.is_empty() && files.len() <= maximum_entries,
        "consumer archive member count exceeds its bound"
    );
    let entries: Vec<_> = files
        .iter()
        .map(|file| GuestStagingEntry::RegularFile {
            path: file.path.into(),
            mode: file.mode,
            bytes: file.bytes,
            sha256: file.sha256.into(),
        })
        .collect();
    let descriptors = files
        .iter()
        .map(|file| (file.path, file.descriptor))
        .collect();
    write_consumer_inventory_archive(
        output,
        &entries,
        &descriptors,
        maximum_entries,
        maximum_regular_bytes,
        maximum_framed_bytes,
        deadline,
    )
}

/// Stream the shared inventory language without Worker bootstrap or activation
/// fields. Symlinks are inert archive members; guest import must not follow them
/// and must verify each complete product manifest before installing any tree.
pub fn write_consumer_inventory_archive<W: Write>(
    output: W,
    entries: &[GuestStagingEntry],
    descriptors: &std::collections::BTreeMap<&str, &lillux::InheritedDescriptorAuthority>,
    maximum_entries: usize,
    maximum_regular_bytes: u64,
    maximum_framed_bytes: u64,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<W> {
    ensure!(
        !entries.is_empty() && entries.len() <= maximum_entries,
        "consumer archive member count exceeds its bound"
    );
    let mut paths = std::collections::BTreeMap::new();
    let mut total = 0u64;
    let mut regular_count = 0usize;
    for entry in entries {
        let path = entry.path();
        ensure!(
            !path.is_empty()
                && path.len() <= 4096
                && path.split('/').all(|part| !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.contains('\\')
                    && !part.contains('\0'))
                && paths.insert(path, entry).is_none(),
            "consumer archive has an unsafe or duplicate member"
        );
        match entry {
            GuestStagingEntry::RegularFile {
                mode,
                bytes,
                sha256,
                ..
            } => {
                ensure!(
                    mode & !0o777 == 0 && mode & 0o400 != 0,
                    "invalid consumer archive mode"
                );
                ensure!(lillux::valid_hash(sha256), "invalid consumer member digest");
                ensure!(
                    descriptors.contains_key(path),
                    "consumer archive file has no descriptor"
                );
                regular_count += 1;
                total = total
                    .checked_add(*bytes)
                    .ok_or_else(|| anyhow::anyhow!("consumer archive size overflow"))?;
            }
            GuestStagingEntry::Directory { mode, .. } => {
                ensure!(
                    mode & !0o777 == 0 && mode & 0o500 == 0o500,
                    "invalid consumer archive directory mode"
                );
            }
            GuestStagingEntry::Symlink { target, .. } => {
                ensure!(
                    path.contains('/')
                        && !target.is_empty()
                        && target.len() <= 4096
                        && !target.contains('\0')
                        && !target.contains('\\')
                        && !target.starts_with('/'),
                    "unsafe consumer archive symlink"
                );
                let mut depth = path.split('/').count() - 1;
                for part in target.split('/') {
                    match part {
                        ".." => {
                            ensure!(
                                depth > 1,
                                "consumer archive symlink escapes its content root"
                            );
                            depth -= 1;
                        }
                        "" | "." => {}
                        _ => depth += 1,
                    }
                }
            }
        }
    }
    ensure!(
        regular_count == descriptors.len(),
        "consumer archive has extra descriptors"
    );
    ensure!(
        total <= maximum_regular_bytes,
        "consumer archive exceeds content budget"
    );
    for path in paths.keys() {
        for (offset, _) in path.match_indices('/') {
            ensure!(
                paths
                    .get(&path[..offset])
                    .is_none_or(|entry| matches!(entry, GuestStagingEntry::Directory { .. })),
                "consumer archive member shadows a parent"
            );
        }
    }
    for (path, entry) in &paths {
        if matches!(entry, GuestStagingEntry::Symlink { .. }) {
            let root = path.split('/').next().expect("validated nonempty path");
            ensure!(
                matches!(paths.get(root), Some(GuestStagingEntry::Directory { .. })),
                "consumer archive symlink has no content-root directory"
            );
        }
    }
    let writer = ArchiveBudgetWriter {
        inner: output,
        remaining: maximum_framed_bytes,
        deadline,
    };
    let mut archive = tar::Builder::new(writer);
    // Canonical lexical order puts a directory before its descendants and
    // makes archive identity independent of caller enumeration order.
    for (path, entry) in paths {
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        match entry {
            GuestStagingEntry::RegularFile {
                mode,
                bytes,
                sha256,
                ..
            } => {
                let mut reader = descriptors[path].stable_regular_reader_exact(
                    *bytes,
                    sha256,
                    maximum_regular_bytes,
                )?;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(*bytes);
                header.set_mode(*mode);
                header.set_cksum();
                archive.append_data(
                    &mut header,
                    path,
                    ArchiveDeadlineReader {
                        inner: &mut reader,
                        deadline,
                    },
                )?;
                reader.finish()?;
            }
            GuestStagingEntry::Directory { mode, .. } => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_mode(*mode);
                header.set_cksum();
                archive.append_data(&mut header, path, std::io::empty())?;
            }
            GuestStagingEntry::Symlink { target, .. } => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                archive.append_link(&mut header, path, target)?;
            }
        }
    }
    let writer = archive.into_inner()?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer archive deadline exceeded"
    );
    Ok(writer.inner)
}

struct ArchiveBudgetWriter<W> {
    inner: W,
    remaining: u64,
    deadline: lillux::time::MonotonicDeadline,
}

impl<W: Write> Write for ArchiveBudgetWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.deadline.has_elapsed() || bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other(
                "consumer archive deadline or framing budget exceeded",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written as u64;
        if self.deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "consumer archive deadline exceeded during write",
            ));
        }
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()?;
        if self.deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "consumer archive deadline exceeded during flush",
            ));
        }
        Ok(())
    }
}

struct ArchiveDeadlineReader<'a, R> {
    inner: &'a mut R,
    deadline: lillux::time::MonotonicDeadline,
}

impl<R: Read> Read for ArchiveDeadlineReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "consumer archive deadline exceeded",
            ));
        }
        let count = self.inner.read(bytes)?;
        if self.deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "consumer archive deadline exceeded during read",
            ));
        }
        Ok(count)
    }
}

pub use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_VERIFIER_REMOTE_NAME, MAX_RESTORATION_VERIFIER_BYTES,
    RESTORATION_VERIFIER_REMOTE_DIRECTORY, RESTORATION_VERIFIER_REMOTE_NAME,
};

pub struct SealedQualificationVerifierUpload {
    descriptor: lillux::InheritedDescriptorAuthority,
    bytes: u64,
    sha256: String,
}

impl SealedQualificationVerifierUpload {
    pub fn descriptor(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.descriptor
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

pub fn seal_restoration_verifier_upload(
    executable: &lillux::InheritedDescriptorAuthority,
    expected_executable_sha256: &str,
) -> Result<SealedQualificationVerifierUpload> {
    executable.require_owned_executable()?;
    seal_verifier_upload(
        executable,
        expected_executable_sha256,
        RESTORATION_VERIFIER_REMOTE_NAME,
    )
}

/// The caller owns signed consumer-artifact admission and retained source
/// proof. A foreign-target payload remains sealed data on the controller;
/// it is not promoted to a controller executable. The closed entrypoint cannot
/// overwrite the prerequisite verifier.
pub fn seal_consumer_verifier_upload(
    executable: &lillux::InheritedDescriptorAuthority,
    expected_executable_sha256: &str,
) -> Result<SealedQualificationVerifierUpload> {
    seal_verifier_upload(
        executable,
        expected_executable_sha256,
        CONSUMER_VERIFIER_REMOTE_NAME,
    )
}

fn seal_verifier_upload(
    executable: &lillux::InheritedDescriptorAuthority,
    expected_executable_sha256: &str,
    remote_name: &'static str,
) -> Result<SealedQualificationVerifierUpload> {
    ensure!(
        lillux::valid_hash(expected_executable_sha256),
        "restoration verifier executable digest is invalid"
    );
    let (binary, observation) =
        executable.read_regular_file_stable_bounded(MAX_RESTORATION_VERIFIER_BYTES)?;
    ensure!(
        !binary.is_empty()
            && binary.len() as u64 <= MAX_RESTORATION_VERIFIER_BYTES
            && lillux::sha256_hex(&binary) == expected_executable_sha256
            && executable.digest_regular_file_stable_exact(&observation)?
                == expected_executable_sha256,
        "restoration verifier differs from its admitted executable"
    );
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(binary.len() as u64);
    header.set_mode(0o500);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, remote_name, binary.as_slice())?;
    let bytes = archive.into_inner()?;
    ensure!(
        bytes.len() as u64 <= MAX_RESTORATION_VERIFIER_BYTES + 16 * 1024,
        "restoration verifier upload exceeds its bound"
    );
    let descriptor = lillux::sealed_memfd(c"ryeos-restoration-verifier-upload", &bytes)
        .map_err(anyhow::Error::msg)?;
    Ok(SealedQualificationVerifierUpload {
        descriptor,
        bytes: bytes.len() as u64,
        sha256: lillux::sha256_hex(&bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn private_consumer_archive_retains_exact_delivery_and_cleans_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = lillux::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let (_, parent) = parent
            .create_unique_child("private-archive-fixture", 0o700)
            .unwrap();
        parent.require_owner_private_directory().unwrap();
        let descriptor = lillux::sealed_memfd(c"private-consumer-fixture", b"x").unwrap();
        let entries = [GuestStagingEntry::RegularFile {
            path: "input".into(),
            mode: 0o400,
            bytes: 1,
            sha256: lillux::sha256_hex(b"x"),
        }];
        let descriptors = std::collections::BTreeMap::from([("input", &descriptor)]);
        let deadline =
            || lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let prepared = prepare_private_consumer_archive(
            &parent,
            &entries,
            &descriptors,
            &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget::new(1, 1, 4096).unwrap(),
            deadline(),
        )
        .unwrap();
        let delivery = prepared.delivery_descriptor().unwrap();
        use ryeos_external_execution_contract::restored_runtime_measurement::{
            ConsumerArchiveBudget, ConsumerRuntimeChallenge, ConsumerRuntimeVerificationCoordinate,
            ConsumerRuntimeVerifierSelection, RemoteVerificationPurpose,
            RestoredVerifierAttemptIntent,
        };
        let mut intent = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: "a".repeat(64),
            restored_occurrence_id: "fixture-occurrence".into(),
            verifier_artifact_hash: "b".repeat(64),
            upload_sha256: prepared.sha256().into(),
            upload_bytes: prepared.bytes(),
            attempt_deadline_ms: 1,
            purpose: RemoteVerificationPurpose::ConsumerRuntime {
                coordinate: ConsumerRuntimeVerificationCoordinate {
                    schema: 1,
                    accepted_root_id: "T-fixture-root".into(),
                    accepted_capsule_hash: "c".repeat(64),
                    qualification_purpose_digest: "d".repeat(64),
                    scenario_id: "fixture-consumer".into(),
                    scenario_source_digest: "e".repeat(64),
                    subject_digest: "f".repeat(64),
                    use_digest: "1".repeat(64),
                    prerequisite_measurement_attempt_id: "2".repeat(64),
                    prerequisite_measurement_observation_digest: "3".repeat(64),
                },
                nonce_hex: "4".repeat(64),
                guest_runtime_manifest_hash: "5".repeat(64),
            },
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let challenge = ConsumerRuntimeChallenge {
            schema: 1,
            intent,
            selection: ConsumerRuntimeVerifierSelection {
                scenario_source_digest: "e".repeat(64),
                verifier_artifact_hash: "b".repeat(64),
                archive_budget: ConsumerArchiveBudget::new(1, 1, 4096).unwrap(),
            },
        };
        prepared
            .delivery_descriptor_for_challenge(&challenge)
            .unwrap();
        let mut changed = challenge.clone();
        changed.intent.upload_sha256 = "9".repeat(64);
        assert!(
            prepared
                .delivery_descriptor_for_challenge(&changed)
                .is_err()
        );
        changed = challenge;
        changed.intent.upload_bytes += 1;
        assert!(
            prepared
                .delivery_descriptor_for_challenge(&changed)
                .is_err()
        );
        let mut reader = delivery
            .stable_regular_reader_exact(prepared.bytes(), prepared.sha256(), 4096)
            .unwrap();
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        reader.finish().unwrap();
        assert_eq!(lillux::sha256_hex(&body), prepared.sha256());
        prepared.discard().unwrap();
        assert!(parent.entries_no_follow_bounded(1).unwrap().is_empty());
        assert!(
            prepare_private_consumer_archive(
                &parent,
                &entries,
                &descriptors,
                &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget::new(1, 1, 1025).unwrap(),
                deadline()
            )
            .is_err()
        );
        assert!(parent.entries_no_follow_bounded(1).unwrap().is_empty());
    }

    #[test]
    fn consumer_tree_inventory_is_order_independent_and_keeps_symlinks_inert() {
        let descriptor = lillux::sealed_memfd(c"consumer-tree-fixture", b"x").unwrap();
        let entries = vec![
            GuestStagingEntry::Symlink {
                path: "tree/link".into(),
                target: "bin/tool".into(),
            },
            GuestStagingEntry::RegularFile {
                path: "tree/bin/tool".into(),
                mode: 0o755,
                bytes: 1,
                sha256: lillux::sha256_hex(b"x"),
            },
            GuestStagingEntry::Directory {
                path: "tree/bin".into(),
                mode: 0o755,
            },
            GuestStagingEntry::Directory {
                path: "tree".into(),
                mode: 0o755,
            },
        ];
        let descriptors = std::collections::BTreeMap::from([("tree/bin/tool", &descriptor)]);
        let deadline =
            || lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let body = write_consumer_inventory_archive(
            Vec::new(),
            &entries,
            &descriptors,
            4,
            1,
            8192,
            deadline(),
        )
        .unwrap();
        let reversed: Vec<_> = entries.iter().cloned().rev().collect();
        assert_eq!(
            body,
            write_consumer_inventory_archive(
                Vec::new(),
                &reversed,
                &descriptors,
                4,
                1,
                8192,
                deadline()
            )
            .unwrap()
        );
        let mut archive = tar::Archive::new(body.as_slice());
        let members: Vec<_> = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(members.len(), 4);
        assert!(members[3].header().entry_type().is_symlink());
        assert_eq!(
            members[3].link_name().unwrap().unwrap().as_ref(),
            std::path::Path::new("bin/tool")
        );
        for target in ["/absolute", "../outside", "bin/../../outside"] {
            let mut changed = entries.clone();
            changed[0] = GuestStagingEntry::Symlink {
                path: "tree/link".into(),
                target: target.into(),
            };
            assert!(
                write_consumer_inventory_archive(
                    Vec::new(),
                    &changed,
                    &descriptors,
                    4,
                    1,
                    8192,
                    deadline()
                )
                .is_err()
            );
        }
        let root_link = [GuestStagingEntry::Symlink {
            path: "tree".into(),
            target: "other-root".into(),
        }];
        assert!(
            write_consumer_inventory_archive(
                Vec::new(),
                &root_link,
                &std::collections::BTreeMap::new(),
                1,
                0,
                8192,
                deadline()
            )
            .is_err()
        );
    }

    #[test]
    fn streamed_consumer_archive_preserves_exact_members_and_refuses_changed_bytes() {
        let descriptor = lillux::sealed_memfd(c"consumer-stream-fixture", b"exact-input").unwrap();
        let digest = lillux::sha256_hex(b"exact-input");
        let file = ConsumerArchiveFile {
            path: "inputs/runtime/bin/codex",
            mode: 0o755,
            bytes: 11,
            sha256: &digest,
            descriptor: &descriptor,
        };
        let deadline =
            || lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let body = write_consumer_archive(Vec::new(), &[file], 1, 11, 4096, deadline()).unwrap();
        let mut archive = tar::Archive::new(body.as_slice());
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.path().unwrap().as_ref(),
            std::path::Path::new("inputs/runtime/bin/codex")
        );
        assert_eq!(entry.header().mode().unwrap(), 0o755);
        let mut restored = Vec::new();
        entry.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, b"exact-input");
        assert!(entries.next().is_none());
        let wrong = "0".repeat(64);
        let changed = ConsumerArchiveFile {
            path: "input",
            mode: 0o400,
            bytes: 11,
            sha256: &wrong,
            descriptor: &descriptor,
        };
        assert!(write_consumer_archive(Vec::new(), &[changed], 1, 11, 4096, deadline()).is_err());
    }

    #[test]
    fn streamed_consumer_archive_enforces_paths_and_all_budgets() {
        let descriptor = lillux::sealed_memfd(c"consumer-bounds-fixture", b"x").unwrap();
        let digest = lillux::sha256_hex(b"x");
        let file = |path| ConsumerArchiveFile {
            path,
            mode: 0o400,
            bytes: 1,
            sha256: &digest,
            descriptor: &descriptor,
        };
        let deadline =
            || lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        for path in [
            "",
            "/absolute",
            "../escape",
            "a/./b",
            "a//b",
            "a\\b",
            "a\0b",
        ] {
            assert!(
                write_consumer_archive(Vec::new(), &[file(path)], 1, 1, 4096, deadline()).is_err(),
                "{path:?}"
            );
        }
        for paths in [["a", "a"], ["a", "a/b"], ["a/b", "a"]] {
            assert!(
                write_consumer_archive(
                    Vec::new(),
                    &[file(paths[0]), file(paths[1])],
                    2,
                    2,
                    4096,
                    deadline()
                )
                .is_err()
            );
        }
        assert!(write_consumer_archive(Vec::new(), &[file("x")], 0, 1, 4096, deadline()).is_err());
        assert!(write_consumer_archive(Vec::new(), &[file("x")], 1, 0, 4096, deadline()).is_err());
        assert!(write_consumer_archive(Vec::new(), &[file("x")], 1, 1, 512, deadline()).is_err());
        assert!(
            write_consumer_archive(
                Vec::new(),
                &[file("x")],
                1,
                1,
                4096,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO)
            )
            .is_err()
        );
    }

    #[test]
    fn consumer_delivery_cannot_overwrite_owner_measurement_entrypoint() {
        let executable =
            lillux::sealed_memfd(c"consumer-verifier-fixture", b"consumer-fixture").unwrap();
        let digest = lillux::sha256_hex(b"consumer-fixture");
        let upload = seal_consumer_verifier_upload(&executable, &digest).unwrap();
        let (body, _) = upload
            .descriptor()
            .read_regular_file_stable_bounded(upload.bytes())
            .unwrap();
        assert_eq!(lillux::sha256_hex(&body), upload.sha256());
        let mut archive = tar::Archive::new(body.as_slice());
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.path().unwrap().as_ref(),
            std::path::Path::new(CONSUMER_VERIFIER_REMOTE_NAME)
        );
        assert_ne!(
            CONSUMER_VERIFIER_REMOTE_NAME,
            RESTORATION_VERIFIER_REMOTE_NAME
        );
        assert_eq!(entry.header().mode().unwrap(), 0o500);
        let mut restored = Vec::new();
        entry.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, b"consumer-fixture");
        assert!(entries.next().is_none());
        assert!(seal_consumer_verifier_upload(&executable, &"0".repeat(64)).is_err());
    }

    #[test]
    fn exact_verifier_is_sealed_as_one_executable_tar_member() {
        let executable =
            lillux::sealed_executable_memfd(c"verifier-fixture", b"fixture-executable").unwrap();
        let digest = lillux::sha256_hex(b"fixture-executable");
        let upload = seal_restoration_verifier_upload(&executable, &digest).unwrap();
        let (body, _) = upload
            .descriptor()
            .read_regular_file_stable_bounded(upload.bytes())
            .unwrap();
        assert_eq!(lillux::sha256_hex(&body), upload.sha256());
        let mut archive = tar::Archive::new(body.as_slice());
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.path().unwrap().as_ref(),
            std::path::Path::new(RESTORATION_VERIFIER_REMOTE_NAME)
        );
        assert_eq!(entry.header().mode().unwrap(), 0o500);
        let mut restored = Vec::new();
        entry.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, b"fixture-executable");
        assert!(entries.next().is_none());
        assert!(seal_restoration_verifier_upload(&executable, &"0".repeat(64)).is_err());
    }
}
