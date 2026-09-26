//! Controller-side inventory and streaming of one exact guest package.
//!
//! This does not select project paths or contact a provider. The caller owns
//! an unpublished private writer, an operation deadline, and the eventual
//! exact retained upload digest. On any error it must discard the partial
//! writer; a successful stream alone is not an activation receipt.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_external_execution_contract::staging_package::{
    GUEST_STAGING_PACKAGE_SCHEMA, GuestStagingEntry, GuestStagingExpected,
    GuestStagingPackageManifest, GuestStagingStreamWriter, MAX_GUEST_STAGING_ENTRIES,
};
use ryeos_external_execution_contract::{GuestMountContentAuthority, GuestMountKind};

use crate::guest_inputs::ExternalGuestInputAuthority;
use crate::{
    guest_content::recheck_staged_guest_content, guest_staging::stage_uploaded_guest_package,
};

/// Private, locally preflighted package. Delivery must still stream the exact
/// pinned inode under `bytes`/`sha256`; neither this value nor a provider
/// acknowledgement is a guest Ready claim.
pub struct PreparedGuestPackage {
    parent: lillux::PinnedDirectory,
    name: OsString,
    root: lillux::PinnedDirectory,
    payload: lillux::PinnedRegularFile,
    manifest: GuestStagingPackageManifest,
    manifest_sha256: String,
    bytes: u64,
    sha256: String,
    discarded: bool,
}

impl PreparedGuestPackage {
    pub fn payload(&self) -> &lillux::PinnedRegularFile {
        &self.payload
    }

    pub fn manifest(&self) -> &GuestStagingPackageManifest {
        &self.manifest
    }

    /// Digest of the canonical inventory that the guest must re-import.
    /// This is distinct from the framed payload digest. The eventual durable
    /// activation must bind both so neither a changed inventory nor changed
    /// file bytes can silently substitute for the prepared package.
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Register the exact prepared inode for adapter delivery. The adapter
    /// must stream it under `bytes` and `sha256` and finish its stable reader;
    /// this does not authorize a pathname reopen. This descriptor can be
    /// requested more than once: durable activation, not this getter, must
    /// prevent a second provider upload after uncertain contact.
    pub fn delivery_descriptor(&self) -> Result<lillux::InheritedDescriptorAuthority> {
        let observation = self.payload.observation()?;
        ensure!(
            observation.size() == self.bytes
                && self.payload.digest_stable_exact(&observation)? == self.sha256,
            "prepared guest package changed before descriptor handoff"
        );
        self.payload.inherited_descriptor_authority()
    }

    /// Remove the exact private package generation after delivery settles.
    pub fn discard(mut self) -> Result<()> {
        remove_private_package(&self.parent, &self.name, &self.root)?;
        self.discarded = true;
        Ok(())
    }
}

impl Drop for PreparedGuestPackage {
    fn drop(&mut self) {
        if !self.discarded {
            // A failed early activation path must not retain a private copy of
            // the bootstrap secret. Explicit discard still reports failures;
            // Drop is only the final best-effort guard for error/unwind paths.
            let _ = remove_private_package(&self.parent, &self.name, &self.root);
        }
    }
}

/// Materialize, pin, re-import, and verify the final package before any
/// provider contact. A drifted product/source tree cannot leak through an
/// upload that would only be rejected later by the guest.
pub fn prepare_private_guest_package(
    parent: &lillux::PinnedDirectory,
    inputs: &ExternalGuestInputAuthority,
    bootstrap: &lillux::InheritedDescriptorAuthority,
    supervisor: &lillux::InheritedDescriptorAuthority,
    launcher: &lillux::InheritedDescriptorAuthority,
    expected: &GuestStagingExpected<'_>,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<PreparedGuestPackage> {
    parent.require_owner_private_directory()?;
    let owner = parent.try_clone()?;
    let (name, root) = parent.create_unique_child("guest-package", 0o700)?;
    let result = (|| {
        root.require_owner_private_directory()?;
        let output = root.open_regular_create(OsStr::new("payload"), true, true, 0o600)?;
        let (output, manifest) = write_guest_package(
            output, inputs, bootstrap, supervisor, launcher, expected, deadline,
        )?;
        lillux::set_open_regular_file_mode(&output, 0o400)?;
        output.sync_all()?;
        drop(output);
        root.sync_tree_bounded(lillux::DirectoryTraversalBudget::new(1, 1))?;
        let payload = root
            .open_pinned_regular(OsStr::new("payload"), false)?
            .context("private guest package disappeared")?;
        let observation = payload.observation()?;
        let bytes = observation.size();
        ensure!(
            bytes == manifest.framed_bytes()? && bytes <= expected.maximum_framed_bytes,
            "private guest package length changed"
        );
        let sha256 = payload.digest_stable_exact(&observation)?;
        require_time(deadline)?;
        let stage =
            stage_uploaded_guest_package(&payload, bytes, &sha256, parent, expected, deadline)?;
        let check = (|| {
            ensure!(
                stage.manifest() == &manifest,
                "local guest package import changed its inventory"
            );
            recheck_staged_guest_content(&stage, inputs.projection())
        })();
        let cleanup = stage.discard();
        if let Err(error) = cleanup {
            return Err(error.context("local guest package preflight cleanup failed"));
        }
        check?;
        require_time(deadline)?;
        let manifest_sha256 = manifest.identity_digest()?;
        Ok((payload, manifest, manifest_sha256, bytes, sha256))
    })();
    match result {
        Ok((payload, manifest, manifest_sha256, bytes, sha256)) => Ok(PreparedGuestPackage {
            parent: owner,
            name,
            root,
            payload,
            manifest,
            manifest_sha256,
            bytes,
            sha256,
            discarded: false,
        }),
        Err(error) => {
            if let Err(cleanup) = remove_private_package(&owner, &name, &root) {
                return Err(
                    error.context(format!("private guest package cleanup failed: {cleanup:#}"))
                );
            }
            Err(error)
        }
    }
}

fn remove_private_package(
    parent: &lillux::PinnedDirectory,
    name: &OsStr,
    root: &lillux::PinnedDirectory,
) -> Result<()> {
    root.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(1, 1))?;
    ensure!(
        parent.remove_empty_child_if_same(name, root)?,
        "private guest package generation changed before cleanup"
    );
    Ok(())
}

enum RootSource<'a> {
    File(&'a lillux::InheritedDescriptorAuthority),
    Directory(lillux::PinnedDirectory),
}

struct DeadlineReader<'a, R> {
    inner: &'a mut R,
    deadline: lillux::time::MonotonicDeadline,
}

impl<R: Read> Read for DeadlineReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "guest package production deadline expired",
            ));
        }
        self.inner.read(buffer)
    }
}

/// Derive and write the complete uncompressed package from exact retained
/// authorities. No file is reopened through a diagnostic or live-project
/// pathname. The writer must be private and unpublished until the caller
/// durably pins its final length and full digest.
pub(crate) fn write_guest_package<W: Write>(
    output: W,
    inputs: &ExternalGuestInputAuthority,
    bootstrap: &lillux::InheritedDescriptorAuthority,
    supervisor: &lillux::InheritedDescriptorAuthority,
    launcher: &lillux::InheritedDescriptorAuthority,
    expected: &GuestStagingExpected<'_>,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<(W, GuestStagingPackageManifest)> {
    require_time(deadline)?;
    ensure!(
        inputs.projection() == expected.inputs,
        "guest package authority differs from retained activation projection"
    );
    let mut roots = BTreeMap::<String, RootSource<'_>>::new();
    let mut entries = Vec::new();
    let mut total_regular_bytes = 0_u64;

    let base = inputs
        .base_snapshot()
        .try_clone_pinned_directory(PathBuf::from("<guest-package-base>"))?;
    inventory_tree(
        &base,
        "base",
        false,
        &mut entries,
        &mut total_regular_bytes,
        expected.maximum_regular_bytes,
        deadline,
    )?;
    require_time(deadline)?;
    let measurement = ryeos_project_capture::inspect_project_snapshot_transfer(
        &base,
        &expected.inputs.base_snapshot.snapshot_hash,
    )?;
    require_time(deadline)?;
    let retained_base = &expected.inputs.base_snapshot;
    ensure!(
        measurement.closure_digest == retained_base.closure_digest
            && measurement.object_count == retained_base.object_count
            && measurement.blob_count == retained_base.blob_count
            && measurement.total_bytes == retained_base.total_bytes,
        "guest package base CAS differs from retained snapshot measurement"
    );
    roots.insert("base".into(), RootSource::Directory(base));

    for (name, authority) in [
        ("bootstrap", bootstrap),
        ("launcher", launcher),
        ("supervisor", supervisor),
    ] {
        inventory_root_file(
            authority,
            name,
            &mut entries,
            &mut total_regular_bytes,
            expected.maximum_regular_bytes,
            deadline,
        )?;
        roots.insert(name.into(), RootSource::File(authority));
    }

    if let Some(output_authority) = inputs.workspace_outputs() {
        inventory_root_file(
            output_authority,
            "workspace_outputs",
            &mut entries,
            &mut total_regular_bytes,
            expected.maximum_regular_bytes,
            deadline,
        )?;
        roots.insert(
            "workspace_outputs".into(),
            RootSource::File(output_authority),
        );
    }

    for (index, input) in expected.inputs.inputs.iter().enumerate() {
        if matches!(
            input.content_authority,
            GuestMountContentAuthority::PrivateScratch { .. }
        ) {
            continue;
        }
        let name = format!("input-{index:02}");
        let authority = inputs
            .input(index)
            .context("retained guest input descriptor disappeared")?;
        match input.kind {
            GuestMountKind::RegularFile => {
                inventory_root_file(
                    authority,
                    &name,
                    &mut entries,
                    &mut total_regular_bytes,
                    expected.maximum_regular_bytes,
                    deadline,
                )?;
                roots.insert(name, RootSource::File(authority));
            }
            GuestMountKind::Directory => {
                let root = authority.try_clone_pinned_directory(PathBuf::from(format!(
                    "<guest-package-input-{index}>"
                )))?;
                inventory_tree(
                    &root,
                    &name,
                    matches!(
                        input.content_authority,
                        GuestMountContentAuthority::ProductManifest { .. }
                    ),
                    &mut entries,
                    &mut total_regular_bytes,
                    expected.maximum_regular_bytes,
                    deadline,
                )?;
                roots.insert(name, RootSource::Directory(root));
            }
        }
    }

    for (index, _) in expected.inputs.record_descriptors().enumerate() {
        let name = format!("record-{index:02}");
        let authority = inputs
            .content_record(index)
            .context("retained guest content record disappeared")?;
        inventory_root_file(
            authority,
            &name,
            &mut entries,
            &mut total_regular_bytes,
            expected.maximum_regular_bytes,
            deadline,
        )?;
        roots.insert(name, RootSource::File(authority));
    }

    entries.sort_by(|left, right| left.path().cmp(right.path()));
    let manifest = GuestStagingPackageManifest {
        schema: GUEST_STAGING_PACKAGE_SCHEMA,
        activation_request_digest: expected.activation_request_digest.into(),
        guest_input_identity: expected.inputs.identity_digest()?,
        bootstrap_sha256: expected.bootstrap_sha256.into(),
        supervisor_sha256: expected.supervisor_sha256.into(),
        launcher_sha256: expected.launcher_sha256.into(),
        total_regular_bytes,
        entries,
    };
    ensure!(
        manifest.framed_bytes()? <= expected.maximum_framed_bytes,
        "guest package exceeds admitted framed byte bound"
    );
    let mut stream = GuestStagingStreamWriter::new(output, manifest.clone(), expected)?;
    while let Some(entry) = stream.next_file() {
        require_time(deadline)?;
        let GuestStagingEntry::RegularFile {
            path,
            bytes,
            sha256,
            ..
        } = entry
        else {
            unreachable!("stream writer selects only regular files")
        };
        let (root_name, relative) = path
            .split_once('/')
            .map_or((path.as_str(), None), |(root, relative)| {
                (root, Some(relative))
            });
        match roots
            .get(root_name)
            .context("guest package source disappeared")?
        {
            RootSource::File(authority) => {
                ensure!(
                    relative.is_none(),
                    "guest package descends through a file root"
                );
                let mut reader = authority.stable_regular_reader_exact(
                    *bytes,
                    sha256,
                    expected.maximum_regular_bytes,
                )?;
                stream.copy_next_file(&mut DeadlineReader {
                    inner: &mut reader,
                    deadline,
                })?;
                reader.finish()?;
            }
            RootSource::Directory(root) => {
                let relative = relative.context("guest package directory has no file path")?;
                let source = root
                    .open_pinned_regular_descendant(Path::new(relative), false)?
                    .context("guest package source file disappeared")?;
                let mut reader =
                    source.stable_reader_exact(*bytes, sha256, expected.maximum_regular_bytes)?;
                stream.copy_next_file(&mut DeadlineReader {
                    inner: &mut reader,
                    deadline,
                })?;
                reader.finish()?;
            }
        }
    }
    require_time(deadline)?;
    Ok((stream.finish()?, manifest))
}

fn inventory_root_file(
    authority: &lillux::InheritedDescriptorAuthority,
    path: &str,
    entries: &mut Vec<GuestStagingEntry>,
    total: &mut u64,
    maximum_regular_bytes: u64,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    require_time(deadline)?;
    let observation = authority.regular_file_observation()?;
    require_size_budget(*total, observation.size(), maximum_regular_bytes)?;
    let mode = observation.permission_mode()?;
    match path {
        "bootstrap" => ensure!(mode == 0o600, "guest bootstrap must remain owner-private"),
        "supervisor" | "launcher" => ensure!(
            matches!(mode, 0o500 | 0o700 | 0o755),
            "guest executable artifact lost its executable mode"
        ),
        _ => {}
    }
    let sha256 = authority.digest_regular_file_stable_exact(&observation)?;
    require_time(deadline)?;
    push_regular(
        entries,
        total,
        path.to_owned(),
        mode,
        observation.size(),
        sha256,
    )
}

fn inventory_tree(
    root: &lillux::PinnedDirectory,
    name: &str,
    allows_symlinks: bool,
    entries: &mut Vec<GuestStagingEntry>,
    total: &mut u64,
    maximum_regular_bytes: u64,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    push_entry(
        entries,
        GuestStagingEntry::Directory {
            path: name.into(),
            mode: 0o700,
        },
    )?;
    inventory_children(
        root,
        name,
        allows_symlinks,
        0,
        entries,
        total,
        maximum_regular_bytes,
        deadline,
    )
}

fn inventory_children(
    directory: &lillux::PinnedDirectory,
    parent: &str,
    allows_symlinks: bool,
    depth: usize,
    entries: &mut Vec<GuestStagingEntry>,
    total: &mut u64,
    maximum_regular_bytes: u64,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    require_time(deadline)?;
    ensure!(depth < 32, "guest package directory depth exceeds bound");
    let remaining = MAX_GUEST_STAGING_ENTRIES
        .checked_sub(entries.len())
        .context("guest package entry count overflow")?;
    let mut children = directory.entries_no_follow_bounded(remaining)?;
    children.sort_by(|left, right| left.name.cmp(&right.name));
    for child in children {
        require_time(deadline)?;
        let component = child
            .name
            .to_str()
            .context("guest package contains a non-UTF-8 entry")?;
        let path = format!("{parent}/{component}");
        match child.entry_type {
            lillux::PinnedEntryType::Directory => {
                push_entry(
                    entries,
                    GuestStagingEntry::Directory {
                        path: path.clone(),
                        mode: child.mode & 0o7777,
                    },
                )?;
                let next = directory
                    .open_child_directory(OsStr::new(component))?
                    .context("guest package directory disappeared")?;
                inventory_children(
                    &next,
                    &path,
                    allows_symlinks,
                    depth + 1,
                    entries,
                    total,
                    maximum_regular_bytes,
                    deadline,
                )?;
            }
            lillux::PinnedEntryType::Regular => {
                let file = directory
                    .open_pinned_regular(OsStr::new(component), false)?
                    .context("guest package file disappeared")?;
                let observation = file.observation()?;
                require_size_budget(*total, observation.size(), maximum_regular_bytes)?;
                let sha256 = file.digest_stable_exact(&observation)?;
                require_time(deadline)?;
                push_regular(
                    entries,
                    total,
                    path,
                    observation.permission_mode()?,
                    observation.size(),
                    sha256,
                )?;
            }
            lillux::PinnedEntryType::Symlink if allows_symlinks => {
                let target = directory
                    .read_symlink_target(OsStr::new(component), 4096)?
                    .context("guest package product symlink disappeared")?;
                push_entry(
                    entries,
                    GuestStagingEntry::Symlink {
                        path,
                        target: String::from_utf8(target)?,
                    },
                )?;
            }
            _ => bail!("guest package contains an unsupported or unauthorized entry"),
        }
    }
    Ok(())
}

fn push_regular(
    entries: &mut Vec<GuestStagingEntry>,
    total: &mut u64,
    path: String,
    mode: u32,
    bytes: u64,
    sha256: String,
) -> Result<()> {
    *total = total
        .checked_add(bytes)
        .context("guest package regular byte total overflow")?;
    push_entry(
        entries,
        GuestStagingEntry::RegularFile {
            path,
            mode,
            bytes,
            sha256,
        },
    )
}

fn push_entry(entries: &mut Vec<GuestStagingEntry>, entry: GuestStagingEntry) -> Result<()> {
    ensure!(
        entries.len() < MAX_GUEST_STAGING_ENTRIES,
        "guest package entry count exceeds bound"
    );
    entries.push(entry);
    Ok(())
}

fn require_size_budget(current: u64, next: u64, maximum: u64) -> Result<()> {
    ensure!(
        current
            .checked_add(next)
            .is_some_and(|total| total <= maximum),
        "guest package regular byte budget exceeded before hashing"
    );
    Ok(())
}

fn require_time(deadline: lillux::time::MonotonicDeadline) -> Result<()> {
    ensure!(
        !deadline.has_elapsed(),
        "guest package production deadline expired"
    );
    Ok(())
}
