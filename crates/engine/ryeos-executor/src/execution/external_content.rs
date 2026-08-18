//! Verification and read-only binding of admitted external realizations.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read as _, Seek as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ryeos_engine::external_content::ExternalContentKind;
use ryeos_engine::external_realization::RealizedExternalContentSet;

/// Descriptor-pinned materializations and their exact cache-generation
/// leases. This value must live until the spawned process exits.
pub(crate) struct BoundExternalRealizations {
    mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    /// Canonical JSON of the sealed realization set, injected into the spawn
    /// env (`RYEOS_EXTERNAL_REALIZATIONS`) so a runtime can reference the
    /// identity it executes under without re-observing any content.
    sealed_set_env: String,
    _leases: Vec<fs::File>,
}

impl BoundExternalRealizations {
    pub(crate) fn mounts(&self) -> &[ryeos_engine::isolation::IsolationReadOnlyMountAuthority] {
        &self.mounts
    }

    pub(crate) fn sealed_set_env(&self) -> &str {
        &self.sealed_set_env
    }

    pub(crate) fn into_spawn_parts(
        self,
    ) -> (
        Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
        String,
        Vec<fs::File>,
    ) {
        (self.mounts, self.sealed_set_env, self._leases)
    }
}

/// Operational ceiling for the materialization cache as a whole. Redeemability
/// never depends on this cache — an evicted generation re-materializes from
/// CAS, whose blobs are capsule-protected — so the budget is free to be
/// modest. Live generations are lease-protected and never counted against
/// eviction eligibility.
const MAX_EXTERNAL_MATERIALIZATION_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A crashed build's staging directory is torn down once clearly abandoned.
/// A live staging is always younger than this: a build publishes or cleans
/// up within one materialization call.
const STALE_STAGING_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

struct ExternalMaterializationCache {
    root: PathBuf,
}

struct MaterializedExternalGeneration {
    root: lillux::PinnedDirectory,
    source_path: PathBuf,
    source: fs::File,
    leases: Vec<fs::File>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalRealizationBinding {
    /// The isolation adapter installs descriptor-pinned, read-only mounts.
    IsolationMounts,
    /// Disabled isolation has no mount namespace. A daemon-owned, otherwise
    /// empty workspace receives a private exact copy before the process is
    /// born. This preserves realization identity without writing into a live
    /// project or exposing the shared materialization cache to the child.
    PrivateWorkspace,
}

static PRIVATE_MATERIALIZATION_COPY_LIMIT: AtomicU64 = AtomicU64::new(0);

/// Arm the node-owned aggregate allowance used only when descriptor reflinks
/// are unavailable while constructing one private admitted-input root.
pub fn arm_private_materialization_copy_limit(limit: u64) -> anyhow::Result<()> {
    if limit == 0 {
        anyhow::bail!("private materialization copy limit must be greater than zero");
    }
    PRIVATE_MATERIALIZATION_COPY_LIMIT.store(limit, Ordering::Release);
    Ok(())
}

#[derive(Debug)]
pub(crate) struct PrivateMaterializationBudget {
    state: Mutex<PrivateMaterializationState>,
}

#[derive(Debug)]
struct PrivateMaterializationState {
    copy_limit_bytes: u64,
    remaining_copy_bytes: u64,
    copied_files: u64,
    copied_bytes: u64,
    reflinked_files: u64,
    materialization_micros: u64,
}

impl PrivateMaterializationBudget {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            state: Mutex::new(PrivateMaterializationState {
                copy_limit_bytes: limit,
                remaining_copy_bytes: limit,
                copied_files: 0,
                copied_bytes: 0,
                reflinked_files: 0,
                materialization_micros: 0,
            }),
        }
    }

    pub(crate) fn materialize_regular(
        &self,
        target_parent: &lillux::PinnedDirectory,
        target_name: &OsStr,
        source: &fs::File,
        expected_size: u64,
        mode: u32,
    ) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("private materialization copy budget lock is poisoned"))?;
        let outcome = target_parent.materialize_private_regular_child(
            target_name,
            source,
            expected_size,
            mode,
            &mut state.remaining_copy_bytes,
        )?;
        match outcome {
            lillux::secure_fs::PrivateFileMaterialization::Reflink => {
                state.reflinked_files = state.reflinked_files.saturating_add(1);
            }
            lillux::secure_fs::PrivateFileMaterialization::Copied => {
                state.copied_files = state.copied_files.saturating_add(1);
                state.copied_bytes = state.copied_bytes.saturating_add(expected_size);
            }
        }
        state.materialization_micros = state
            .materialization_micros
            .saturating_add(started.elapsed().as_micros().try_into().unwrap_or(u64::MAX));
        Ok(())
    }

    pub(crate) fn emit_metrics(&self, thread_id: &str) -> anyhow::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("private materialization copy budget lock is poisoned"))?;
        tracing::info!(
            target: "ryeos.metrics",
            operation = "private_input_materialization",
            thread_id,
            reflinked_files = state.reflinked_files,
            copied_files = state.copied_files,
            copied_bytes = state.copied_bytes,
            copy_limit_bytes = state.copy_limit_bytes,
            copy_remaining_bytes = state.remaining_copy_bytes,
            materialization_micros = state.materialization_micros,
            "private input materialization summary"
        );
        Ok(())
    }
}

pub(crate) fn private_materialization_budget() -> anyhow::Result<PrivateMaterializationBudget> {
    let limit = PRIVATE_MATERIALIZATION_COPY_LIMIT.load(Ordering::Acquire);
    if limit == 0 {
        anyhow::bail!("node private materialization copy limit is not armed");
    }
    Ok(PrivateMaterializationBudget::new(limit))
}

impl ExternalMaterializationCache {
    fn from_runtime_state_root(runtime_state_root: &Path) -> Self {
        Self {
            root: runtime_state_root.join("external-content-cache"),
        }
    }

    fn materialize(
        &self,
        cas: &lillux::CasStore,
        closure: &ryeos_state::VerifiedExternalContentClosure,
        kind: ExternalContentKind,
    ) -> anyhow::Result<MaterializedExternalGeneration> {
        let manifest_hash = closure.manifest_hash();
        let root = lillux::PinnedDirectory::open_or_create(&self.root)?;
        let locks = root.open_or_create_child(OsStr::new(".locks"), 0o700)?;
        let lock = locks.open_regular_create(OsStr::new(manifest_hash), true, false, 0o600)?;
        #[cfg(unix)]
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        let generation = match root.open_child_directory(OsStr::new(manifest_hash))? {
            Some(existing) => match verify_materialized_tree(cas, &existing, closure.manifest()) {
                Ok(()) => existing,
                Err(error) => {
                    tracing::warn!(
                        manifest_hash,
                        %error,
                        "discarding invalid external-content materialization"
                    );
                    existing.remove_contents_recursive()?;
                    if !root.remove_empty_child_if_same(OsStr::new(manifest_hash), &existing)? {
                        anyhow::bail!(
                            "invalid external-content generation {manifest_hash} remained non-empty"
                        );
                    }
                    self.build_generation(cas, &root, closure)?
                }
            },
            None => self.build_generation(cas, &root, closure)?,
        };
        verify_materialized_tree(cas, &generation, closure.manifest())?;

        let leases = root.open_or_create_child(OsStr::new(".leases"), 0o700)?;
        let lease = leases.open_regular_create(OsStr::new(manifest_hash), true, false, 0o600)?;
        #[cfg(unix)]
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // Recency signal for the sweep: every use refreshes the lease file's
        // modification time. Best-effort — eviction order is a policy, not a
        // correctness input.
        let _ = lease.set_modified(std::time::SystemTime::now());

        let (source_path, source) = match kind {
            ExternalContentKind::Tree => (
                generation.path().to_path_buf(),
                generation.try_clone_descriptor()?,
            ),
            ExternalContentKind::File => {
                let name = OsStr::new(ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH);
                let source = generation.open_mount_entry(name)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "file realization {manifest_hash} has no materialized content entry"
                    )
                })?;
                (generation.path().join(name), source)
            }
        };
        drop(lock);
        Ok(MaterializedExternalGeneration {
            root: generation,
            source_path,
            source,
            leases: vec![lease],
        })
    }

    fn materialize_large(
        &self,
        cas: &lillux::CasStore,
        store: &ryeos_state::LargeObjectStore,
        manifest_hash: &str,
        manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
        kind: ExternalContentKind,
    ) -> anyhow::Result<MaterializedExternalGeneration> {
        let mut large_sources = BTreeMap::new();
        for entry in &manifest.entries {
            let Some(file_sha256) = entry.file_sha256.as_deref() else {
                continue;
            };
            if large_sources.contains_key(file_sha256) {
                continue;
            }
            large_sources.insert(
                file_sha256.to_owned(),
                store.lease_object(
                    file_sha256,
                    entry.size.expect("validated large file has a size"),
                )?,
            );
        }

        let root = lillux::PinnedDirectory::open_or_create(&self.root)?;
        let locks = root.open_or_create_child(OsStr::new(".locks"), 0o700)?;
        let lock = locks.open_regular_create(OsStr::new(manifest_hash), true, false, 0o600)?;
        #[cfg(unix)]
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        let generation = match root.open_child_directory(OsStr::new(manifest_hash))? {
            Some(existing) => {
                match verify_large_materialized_tree(
                    cas,
                    &existing,
                    manifest,
                    LargeMaterializationVerification::SharedObjects(&large_sources),
                ) {
                    Ok(()) => existing,
                    Err(error) => {
                        tracing::warn!(
                            manifest_hash,
                            %error,
                            "discarding invalid large-content materialization"
                        );
                        existing.remove_contents_recursive()?;
                        if !root.remove_empty_child_if_same(OsStr::new(manifest_hash), &existing)? {
                            anyhow::bail!(
                                "invalid large-content generation {manifest_hash} remained non-empty"
                            );
                        }
                        self.build_large_generation(
                            cas,
                            store,
                            &root,
                            manifest_hash,
                            manifest,
                            &large_sources,
                        )?
                    }
                }
            }
            None => self.build_large_generation(
                cas,
                store,
                &root,
                manifest_hash,
                manifest,
                &large_sources,
            )?,
        };
        verify_large_materialized_tree(
            cas,
            &generation,
            manifest,
            LargeMaterializationVerification::SharedObjects(&large_sources),
        )?;

        let leases = root.open_or_create_child(OsStr::new(".leases"), 0o700)?;
        let generation_lease =
            leases.open_regular_create(OsStr::new(manifest_hash), true, false, 0o600)?;
        #[cfg(unix)]
        if unsafe { libc::flock(generation_lease.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let _ = generation_lease.set_modified(std::time::SystemTime::now());

        let (source_path, source) = match kind {
            ExternalContentKind::Tree => (
                generation.path().to_path_buf(),
                generation.try_clone_descriptor()?,
            ),
            ExternalContentKind::File => {
                let name = OsStr::new(ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH);
                let source = generation.open_mount_entry(name)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "file realization {manifest_hash} has no materialized content entry"
                    )
                })?;
                (generation.path().join(name), source)
            }
        };
        let mut retained_leases = Vec::with_capacity(large_sources.len() + 1);
        retained_leases.push(generation_lease);
        for leased in large_sources.into_values() {
            let (_, _, lease) = leased.into_parts();
            retained_leases.push(lease);
        }
        drop(lock);
        Ok(MaterializedExternalGeneration {
            root: generation,
            source_path,
            source,
            leases: retained_leases,
        })
    }

    fn build_large_generation(
        &self,
        cas: &lillux::CasStore,
        store: &ryeos_state::LargeObjectStore,
        root: &lillux::PinnedDirectory,
        manifest_hash: &str,
        manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
        large_sources: &BTreeMap<String, ryeos_state::LeasedLargeObject>,
    ) -> anyhow::Result<lillux::PinnedDirectory> {
        let staging_name = OsString::from(format!(
            ".{manifest_hash}.staging.{}.{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let staging = root.create_child(&staging_name, 0o700)?;
        let result = (|| {
            for entry in &manifest.entries {
                let (parent, name) = ensure_materialization_parent(&staging, &entry.path)?;
                match entry.kind {
                    ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                        parent.create_child(&name, 0o755)?;
                    }
                    ryeos_state::objects::ExternalContentManifestEntryKind::File => {
                        let mode = entry.mode.expect("validated file entry has a mode");
                        if let Some(blob_hash) = entry.blob_hash.as_deref() {
                            let written = cas
                                .materialize_blob_to_new_regular(blob_hash, &parent, &name, mode)?;
                            if Some(written) != entry.size {
                                anyhow::bail!(
                                    "materialized large-tree CAS file {} has the wrong size",
                                    entry.path
                                );
                            }
                        } else {
                            let file_sha256 = entry
                                .file_sha256
                                .as_deref()
                                .expect("validated file has one storage tier");
                            let leased = large_sources.get(file_sha256).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "large-content object {file_sha256} lost its launch lease"
                                )
                            })?;
                            if mode == 0o644 {
                                store.link_object_into(
                                    file_sha256,
                                    &parent,
                                    &name,
                                    leased.file(),
                                )?;
                            } else {
                                materialize_executable_large_file(
                                    leased.file(),
                                    &parent,
                                    &name,
                                    entry.size.expect("validated file has a size"),
                                )?;
                            }
                        }
                    }
                    ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => {
                        let target = large_symlink_target_bytes(entry)?;
                        parent.create_symlink(&name, &target)?;
                    }
                }
            }
            verify_large_materialized_tree(
                cas,
                &staging,
                manifest,
                LargeMaterializationVerification::SharedObjects(large_sources),
            )?;
            root.rename_child_directory_noreplace(
                &staging_name,
                OsStr::new(manifest_hash),
                &staging,
            )?;
            root.open_child_directory(OsStr::new(manifest_hash))?
                .ok_or_else(|| anyhow::anyhow!("published large-content generation disappeared"))
        })();
        if result.is_err() {
            let _ = staging.remove_contents_recursive();
            let _ = root.remove_empty_child_if_same(&staging_name, &staging);
        }
        result
    }

    fn build_generation(
        &self,
        cas: &lillux::CasStore,
        root: &lillux::PinnedDirectory,
        closure: &ryeos_state::VerifiedExternalContentClosure,
    ) -> anyhow::Result<lillux::PinnedDirectory> {
        let manifest_hash = closure.manifest_hash();
        let staging_name = OsString::from(format!(
            ".{manifest_hash}.staging.{}.{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let staging = root.create_child(&staging_name, 0o700)?;
        let result = (|| {
            for entry in &closure.manifest().entries {
                let (parent, name) = ensure_materialization_parent(&staging, &entry.path)?;
                match entry.kind {
                    ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                        parent.create_child(&name, 0o755)?;
                    }
                    ryeos_state::objects::ExternalContentManifestEntryKind::File => {
                        let written = cas.materialize_blob_to_new_regular(
                            entry
                                .blob_hash
                                .as_deref()
                                .expect("validated file entry has a blob hash"),
                            &parent,
                            &name,
                            entry.mode.expect("validated file entry has a mode"),
                        )?;
                        if Some(written) != entry.size {
                            anyhow::bail!(
                                "materialized external file {} has size {written}, expected {:?}",
                                entry.path,
                                entry.size
                            );
                        }
                    }
                    ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => {
                        let target = symlink_target_bytes(entry)?;
                        parent.create_symlink(&name, &target)?;
                    }
                }
            }
            verify_materialized_tree(cas, &staging, closure.manifest())?;
            root.rename_child_directory_noreplace(
                &staging_name,
                OsStr::new(manifest_hash),
                &staging,
            )?;
            root.open_child_directory(OsStr::new(manifest_hash))?
                .ok_or_else(|| anyhow::anyhow!("published external-content generation disappeared"))
        })();
        if result.is_err() {
            let _ = staging.remove_contents_recursive();
            let _ = root.remove_empty_child_if_same(&staging_name, &staging);
        }
        result
    }

    fn sweep(&self) -> anyhow::Result<()> {
        self.sweep_to_budget(MAX_EXTERNAL_MATERIALIZATION_CACHE_BYTES)
    }

    /// Best-effort, lease-respecting sweep back under the given byte budget.
    ///
    /// A generation is reclaimable only when the sweep wins BOTH its build
    /// lock and its lease file exclusively without blocking. `materialize`
    /// acquires the shared lease before releasing the build lock, so a live
    /// user can never lose both races. Eviction is operational, never a
    /// correctness event: a later bind re-materializes from CAS.
    fn sweep_to_budget(&self, budget: u64) -> anyhow::Result<()> {
        let Some(root) = lillux::PinnedDirectory::open(&self.root)? else {
            return Ok(());
        };
        let locks = root.open_or_create_child(OsStr::new(".locks"), 0o700)?;
        let leases = root.open_or_create_child(OsStr::new(".leases"), 0o700)?;
        let now = std::time::SystemTime::now();
        let mut generations = Vec::new();
        let mut total_bytes = 0u64;
        for entry in root.entries_no_follow()? {
            if entry.entry_type != lillux::PinnedEntryType::Directory {
                continue;
            }
            let Some(name) = entry.name.to_str().map(str::to_owned) else {
                continue;
            };
            if name == ".locks" || name == ".leases" {
                continue;
            }
            let Some(directory) = root.open_child_directory(OsStr::new(&name))? else {
                continue;
            };
            if name.starts_with('.') {
                let abandoned = directory
                    .try_clone_descriptor()?
                    .metadata()?
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age > STALE_STAGING_MAX_AGE);
                if abandoned {
                    directory.remove_contents_recursive()?;
                    let _ = root.remove_empty_child_if_same(OsStr::new(&name), &directory);
                }
                continue;
            }
            let bytes = directory_content_bytes(&directory)?;
            let recency = leases
                .open_regular(OsStr::new(&name), false)?
                .and_then(|file| file.metadata().ok())
                .and_then(|metadata| metadata.modified().ok());
            total_bytes = total_bytes.saturating_add(bytes);
            generations.push((name, bytes, recency));
        }
        if total_bytes <= budget {
            return Ok(());
        }
        // Oldest first; a generation with no lease record sorts oldest.
        generations.sort_by_key(|(_, _, recency)| *recency);
        for (name, bytes, _) in generations {
            if total_bytes <= budget {
                break;
            }
            let lock = locks.open_regular_create(OsStr::new(&name), true, false, 0o600)?;
            #[cfg(unix)]
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                continue;
            }
            let lease = leases.open_regular_create(OsStr::new(&name), true, false, 0o600)?;
            #[cfg(unix)]
            let leased =
                unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
            #[cfg(not(unix))]
            let leased = true;
            if leased {
                drop(lease);
                drop(lock);
                continue;
            }
            let Some(directory) = root.open_child_directory(OsStr::new(&name))? else {
                continue;
            };
            directory.remove_contents_recursive()?;
            if root.remove_empty_child_if_same(OsStr::new(&name), &directory)? {
                total_bytes = total_bytes.saturating_sub(bytes);
                tracing::info!(
                    manifest_hash = %name,
                    bytes,
                    "evicted external-content materialization"
                );
            }
            drop(lease);
            drop(lock);
        }
        Ok(())
    }
}

/// Sum of regular-file bytes under one materialized generation, walked
/// descriptor-relative. Symlink targets and directory entries are noise at
/// this granularity.
fn directory_content_bytes(directory: &lillux::PinnedDirectory) -> anyhow::Result<u64> {
    let mut total = 0u64;
    for entry in directory.entries_no_follow()? {
        match entry.entry_type {
            lillux::PinnedEntryType::Directory => {
                if let Some(child) = directory.open_child_directory(&entry.name)? {
                    total = total.saturating_add(directory_content_bytes(&child)?);
                }
            }
            lillux::PinnedEntryType::Regular => {
                if let Some(file) = directory.open_regular(&entry.name, false)? {
                    total = total.saturating_add(file.metadata()?.len());
                }
            }
            _ => {}
        }
    }
    Ok(total)
}

fn open_materialization_parent(
    root: &lillux::PinnedDirectory,
    relative: &str,
) -> anyhow::Result<(lillux::PinnedDirectory, OsString)> {
    let mut components = relative.split('/').peekable();
    let mut parent = root.try_clone()?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((parent, OsString::from(component)));
        }
        parent = parent
            .open_child_directory(OsStr::new(component))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "verified external materialization lost directory component `{component}`"
                )
            })?;
    }
    anyhow::bail!("external materialization path is empty")
}

fn copy_open_regular_to_new(
    source: &fs::File,
    target_parent: &lillux::PinnedDirectory,
    target_name: &OsStr,
    expected_size: u64,
    mode: u32,
    budget: &PrivateMaterializationBudget,
) -> anyhow::Result<()> {
    budget.materialize_regular(target_parent, target_name, source, expected_size, mode)?;
    Ok(())
}

fn copy_materialized_tree(
    source: &lillux::PinnedDirectory,
    target: &lillux::PinnedDirectory,
    manifest: &ryeos_state::objects::ExternalContentManifestObject,
    budget: &PrivateMaterializationBudget,
) -> anyhow::Result<()> {
    for entry in &manifest.entries {
        let (target_parent, target_name) = ensure_materialization_parent(target, &entry.path)?;
        match entry.kind {
            ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                target_parent.create_child(&target_name, 0o755)?;
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::File => {
                let (source_parent, source_name) =
                    open_materialization_parent(source, &entry.path)?;
                let source_file = source_parent
                    .open_regular(&source_name, false)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "verified external materialization lost file {}",
                            entry.path
                        )
                    })?;
                copy_open_regular_to_new(
                    &source_file,
                    &target_parent,
                    &target_name,
                    entry.size.expect("validated file entry has a size"),
                    entry.mode.expect("validated file entry has a mode"),
                    budget,
                )?;
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => {
                let (source_parent, source_name) =
                    open_materialization_parent(source, &entry.path)?;
                let target_bytes = source_parent
                    .read_symlink_target(
                        &source_name,
                        ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "verified external materialization lost symlink {}",
                            entry.path
                        )
                    })?;
                target_parent.create_symlink(&target_name, &target_bytes)?;
            }
        }
    }
    Ok(())
}

fn copy_large_materialized_tree(
    source: &lillux::PinnedDirectory,
    target: &lillux::PinnedDirectory,
    manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
    budget: &PrivateMaterializationBudget,
) -> anyhow::Result<()> {
    for entry in &manifest.entries {
        let (target_parent, target_name) = ensure_materialization_parent(target, &entry.path)?;
        match entry.kind {
            ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                target_parent.create_child(&target_name, 0o755)?;
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::File => {
                let (source_parent, source_name) =
                    open_materialization_parent(source, &entry.path)?;
                let source_file = source_parent
                    .open_regular(&source_name, false)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "verified large-content materialization lost file {}",
                            entry.path
                        )
                    })?;
                copy_open_regular_to_new(
                    &source_file,
                    &target_parent,
                    &target_name,
                    entry.size.expect("validated file entry has a size"),
                    entry.mode.expect("validated file entry has a mode"),
                    budget,
                )?;
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => {
                let (source_parent, source_name) =
                    open_materialization_parent(source, &entry.path)?;
                let target_bytes = source_parent
                    .read_symlink_target(
                        &source_name,
                        ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "verified large-content materialization lost symlink {}",
                            entry.path
                        )
                    })?;
                target_parent.create_symlink(&target_name, &target_bytes)?;
            }
        }
    }
    Ok(())
}

fn verify_open_regular_exact(
    file: &mut fs::File,
    expected_size: u64,
    expected_mode: u32,
    expected_digest: &str,
) -> anyhow::Result<()> {
    file.rewind()?;
    let (digest, metadata) = lillux::digest_open_regular_file_stable_exact(file, expected_size)?;
    if digest != expected_digest
        || lillux::normalized_portable_regular_mode(&metadata)? != expected_mode
    {
        anyhow::bail!("private external realization file contradicts its admitted manifest");
    }
    Ok(())
}

fn publish_private_generation<P, V, E>(
    workspace: &lillux::PinnedDirectory,
    mount: &str,
    kind: ExternalContentKind,
    populate: P,
    verify: V,
    verify_existing: E,
) -> anyhow::Result<()>
where
    P: FnOnce(&lillux::PinnedDirectory) -> anyhow::Result<()>,
    V: FnOnce(&lillux::PinnedDirectory) -> anyhow::Result<()>,
    E: Fn(&mut lillux::PinnedDirectoryEntry) -> anyhow::Result<()>,
{
    let (parent, mount_name) = ensure_materialization_parent(workspace, mount)?;
    let mut existing = parent.open_entry(&mount_name, false)?;
    if existing
        .as_mut()
        .is_some_and(|entry| verify_existing(entry).is_ok())
    {
        return Ok(());
    }
    let staging_name = OsString::from(format!(
        ".external-realization.{}.{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let staging = parent.create_child(&staging_name, 0o700)?;
    let mut published = false;
    let result = (|| {
        populate(&staging)?;
        verify(&staging)?;
        if let Some(existing) = existing.as_mut() {
            // The destination belongs to this admitted realization. A pinned
            // project copy may already contain live-source or runtime-created
            // bytes at that path; those bytes are not part of the admitted
            // identity and must never remain visible as an ambient fallback.
            // Recheck after staging so a concurrent exact publisher wins
            // without being replaced.
            if verify_existing(existing).is_ok() {
                staging.remove_contents_recursive()?;
                if !parent.remove_empty_child_if_same(&staging_name, &staging)? {
                    anyhow::bail!(
                        "unused external realization staging directory remained non-empty"
                    );
                }
                return Ok(());
            }
            match existing {
                lillux::PinnedDirectoryEntry::Directory(directory) => {
                    directory.remove_contents_recursive()?;
                    if !parent.remove_empty_child_if_same(&mount_name, directory)? {
                        anyhow::bail!(
                            "private external realization destination remained non-empty: {}",
                            parent.path().join(&mount_name).display()
                        );
                    }
                }
                lillux::PinnedDirectoryEntry::Regular(file) => {
                    parent.remove_if_same(&mount_name, file)?;
                }
            }
        }
        match kind {
            ExternalContentKind::Tree => {
                parent.rename_child_directory_noreplace(&staging_name, &mount_name, &staging)?;
                published = true;
            }
            ExternalContentKind::File => {
                let content_name =
                    OsStr::new(ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH);
                let content = staging.open_regular(content_name, false)?.ok_or_else(|| {
                    anyhow::anyhow!("verified file realization lost its content entry")
                })?;
                if !parent.publish_regular_link_from(
                    &mount_name,
                    &staging,
                    content_name,
                    &content,
                )? {
                    anyhow::bail!(
                        "private external realization destination already exists: {}",
                        parent.path().join(&mount_name).display()
                    );
                }
                published = true;
                if !parent.remove_empty_child_if_same(&staging_name, &staging)? {
                    anyhow::bail!("file realization staging directory remained non-empty");
                }
            }
        }
        Ok(())
    })();
    if result.is_err() && !published {
        let _ = staging.remove_contents_recursive();
        let _ = parent.remove_empty_child_if_same(&staging_name, &staging);
    }
    result
}

/// Verify and materialize the exact realization set committed by a finalized
/// program. No locator or live project path is consulted for source bytes.
pub(crate) fn bind_external_realizations(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    project_path: &Path,
) -> anyhow::Result<Option<BoundExternalRealizations>> {
    bind_external_realizations_with(
        state,
        resolution,
        project_path,
        ExternalRealizationBinding::IsolationMounts,
        None,
    )
}

/// Exact project-relative roots populated from separately admitted external
/// realizations. Native private-copy fold-back excludes these operational
/// shadows so input realizations never become project output bytes.
pub(crate) fn admitted_realization_mounts(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<Vec<String>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    else {
        return Ok(Vec::new());
    };
    let realized = RealizedExternalContentSet::from_value(value)?;
    let mut mounts = realized
        .iter()
        .map(|entry| entry.mount.clone())
        .collect::<Vec<_>>();
    mounts.sort();
    mounts.dedup();
    Ok(mounts)
}

pub(crate) fn bind_external_realizations_in_private_workspace_with_budget(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
    budget: &PrivateMaterializationBudget,
) -> anyhow::Result<Option<BoundExternalRealizations>> {
    bind_external_realizations_with(
        state,
        resolution,
        workspace,
        ExternalRealizationBinding::PrivateWorkspace,
        Some(budget),
    )
}

fn bind_external_realizations_with(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    project_path: &Path,
    binding: ExternalRealizationBinding,
    budget: Option<&PrivateMaterializationBudget>,
) -> anyhow::Result<Option<BoundExternalRealizations>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    else {
        return Ok(None);
    };
    let realized = RealizedExternalContentSet::from_value(value)?;
    if realized.is_empty() {
        return Ok(None);
    }
    let sealed_set_env = lillux::cas::canonical_json(&realized.to_value()?)?;
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let cache =
        ExternalMaterializationCache::from_runtime_state_root(&state.config.runtime_state_dir());
    let private_workspace = match binding {
        ExternalRealizationBinding::IsolationMounts => None,
        ExternalRealizationBinding::PrivateWorkspace => {
            Some(lillux::PinnedDirectory::open(project_path)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "private external-realization workspace does not exist: {}",
                    project_path.display()
                )
            })?)
        }
    };
    let private_budget = if private_workspace.is_some() {
        Some(budget.ok_or_else(|| {
            anyhow::anyhow!("private external realization binding has no copy budget")
        })?)
    } else {
        None
    };
    let mut mounts = Vec::with_capacity(realized.iter().len());
    let mut leases = Vec::with_capacity(realized.iter().len());
    for entry in realized.iter() {
        if let Some(manifest) =
            ryeos_state::objects::load_if_large_content_manifest(&cas, &entry.manifest_hash)?
        {
            if manifest.entry_count != entry.entry_count
                || manifest.total_bytes != entry.total_bytes
            {
                anyhow::bail!(
                    "external realization `{}` contradicts manifest {} statistics",
                    entry.id,
                    entry.manifest_hash
                );
            }
            if entry.kind == ExternalContentKind::File && !manifest.is_file_shaped() {
                anyhow::bail!(
                    "external realization `{}` is file-shaped but manifest {} is not",
                    entry.id,
                    entry.manifest_hash
                );
            }
            let store = authority.large_object_store()?;
            let generation = cache.materialize_large(
                &cas,
                &store,
                &entry.manifest_hash,
                &manifest,
                entry.kind,
            )?;
            if let Some(workspace) = private_workspace.as_ref() {
                let expected_file = if entry.kind == ExternalContentKind::File {
                    manifest.entries.iter().find(|manifest_entry| {
                        manifest_entry.path
                            == ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH
                    })
                } else {
                    None
                };
                publish_private_generation(
                    workspace,
                    &entry.mount,
                    entry.kind,
                    |staging| {
                        copy_large_materialized_tree(
                            &generation.root,
                            staging,
                            &manifest,
                            private_budget.expect("private binding has a budget"),
                        )
                    },
                    |staging| {
                        verify_large_materialized_tree(
                            &cas,
                            staging,
                            &manifest,
                            LargeMaterializationVerification::PrivateDigest,
                        )
                    },
                    |existing| match (entry.kind, existing) {
                        (
                            ExternalContentKind::Tree,
                            lillux::PinnedDirectoryEntry::Directory(directory),
                        ) => verify_large_materialized_tree(
                            &cas,
                            directory,
                            &manifest,
                            LargeMaterializationVerification::PrivateDigest,
                        ),
                        (
                            ExternalContentKind::File,
                            lillux::PinnedDirectoryEntry::Regular(file),
                        ) => {
                            let expected = expected_file.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "file-shaped large realization has no canonical file entry"
                                )
                            })?;
                            verify_open_regular_exact(
                                file,
                                expected.size.expect("validated file has a size"),
                                expected.mode.expect("validated file has a mode"),
                                expected
                                    .blob_hash
                                    .as_deref()
                                    .or(expected.file_sha256.as_deref())
                                    .expect("validated file has one content identity"),
                            )
                        }
                        _ => anyhow::bail!(
                            "private external realization destination has the wrong entry type"
                        ),
                    },
                )?;
            } else {
                mounts.push(
                    ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new(
                        generation.source_path,
                        project_path.join(&entry.mount),
                        generation.source,
                    ),
                );
            }
            leases.extend(generation.leases);
            continue;
        }
        let closure =
            ryeos_state::VerifiedExternalContentClosure::load(&cas, &entry.manifest_hash)?;
        if closure.manifest().entry_count != entry.entry_count
            || closure.manifest().total_bytes != entry.total_bytes
        {
            anyhow::bail!(
                "external realization `{}` contradicts manifest {} statistics",
                entry.id,
                entry.manifest_hash
            );
        }
        let generation = cache.materialize(&cas, &closure, entry.kind)?;
        if let Some(workspace) = private_workspace.as_ref() {
            let expected_file = if entry.kind == ExternalContentKind::File {
                closure.manifest().entries.iter().find(|manifest_entry| {
                    manifest_entry.path
                        == ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH
                })
            } else {
                None
            };
            publish_private_generation(
                workspace,
                &entry.mount,
                entry.kind,
                |staging| {
                    copy_materialized_tree(
                        &generation.root,
                        staging,
                        closure.manifest(),
                        private_budget.expect("private binding has a budget"),
                    )
                },
                |staging| verify_materialized_tree(&cas, staging, closure.manifest()),
                |existing| match (entry.kind, existing) {
                    (
                        ExternalContentKind::Tree,
                        lillux::PinnedDirectoryEntry::Directory(directory),
                    ) => verify_materialized_tree(&cas, directory, closure.manifest()),
                    (ExternalContentKind::File, lillux::PinnedDirectoryEntry::Regular(file)) => {
                        let expected = expected_file.ok_or_else(|| {
                            anyhow::anyhow!("file-shaped realization has no canonical file entry")
                        })?;
                        verify_open_regular_exact(
                            file,
                            expected.size.expect("validated file has a size"),
                            expected.mode.expect("validated file has a mode"),
                            expected
                                .blob_hash
                                .as_deref()
                                .expect("validated regular file has a blob identity"),
                        )
                    }
                    _ => anyhow::bail!(
                        "private external realization destination has the wrong entry type"
                    ),
                },
            )?;
        } else {
            mounts.push(
                ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new(
                    generation.source_path,
                    project_path.join(&entry.mount),
                    generation.source,
                ),
            );
        }
        leases.extend(generation.leases);
    }
    authority.ensure_guard(&guard)?;
    // This launch's generations are lease-protected above, so the sweep can
    // only reclaim idle history. Failure to sweep never fails a launch.
    if let Err(error) = cache.sweep() {
        tracing::warn!(%error, "external-content materialization sweep failed");
    }
    Ok(Some(BoundExternalRealizations {
        mounts,
        sealed_set_env,
        _leases: leases,
    }))
}

/// Dependency bytes for plan-build verification, sourced from the sealed
/// realization set the dispatched child will execute under.
///
/// Plan build runs in the daemon, where the runtime's read-only realization
/// mounts do not exist; the live project tree can differ from the sealed
/// content without changing what executes. This source answers exactly like
/// the mounts will: a path under a mount destination resolves to the
/// manifest's CAS blob, a mount-covered path with no manifest entry resolves
/// to nothing, and paths no mount covers stay with their live (or admitted
/// project) bytes.
pub(crate) struct SealedRealizationDependencyBytes {
    authority: ryeos_state::PinnedStateAuthority,
    guard: ryeos_state::CasMutationGuard,
    cas: lillux::CasStore,
    mounts: Vec<SealedRealizationMount>,
}

struct SealedRealizationMount {
    /// Absolute mount destination under the launch's project root.
    destination: PathBuf,
    kind: ExternalContentKind,
    /// Manifest path → exact regular-file bytes and their storage tier.
    files: BTreeMap<String, SealedRealizationFile>,
}

struct SealedRealizationFile {
    content_hash: String,
    size: u64,
    large: bool,
}

enum SealedPathLookup<'a> {
    Uncovered,
    Absent,
    File {
        blob_hash: &'a str,
        size: u64,
        large: bool,
    },
}

/// Resolve one absolute path against the mount table. With nested mounts the
/// most specific destination answers for its subtree, mirroring how mount
/// layering composes at execution.
fn locate_sealed_path<'a>(
    mounts: &'a [SealedRealizationMount],
    absolute_path: &Path,
) -> SealedPathLookup<'a> {
    let mut best: Option<(&SealedRealizationMount, Option<String>)> = None;
    for mount in mounts {
        let key = match mount.kind {
            ExternalContentKind::File => {
                if absolute_path != mount.destination {
                    continue;
                }
                Some(ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH.to_string())
            }
            ExternalContentKind::Tree => match absolute_path.strip_prefix(&mount.destination) {
                // The destination itself is the mounted directory, and a
                // non-UTF-8 relative path can never name a manifest entry;
                // both are covered without sealing a file.
                Ok(relative) if relative.as_os_str().is_empty() => None,
                Ok(relative) => relative.to_str().map(str::to_owned),
                Err(_) => continue,
            },
        };
        if best.as_ref().is_none_or(|(current, _)| {
            mount.destination.as_os_str().len() > current.destination.as_os_str().len()
        }) {
            best = Some((mount, key));
        }
    }
    match best {
        None => SealedPathLookup::Uncovered,
        Some((mount, Some(key))) => match mount.files.get(&key) {
            Some(file) => SealedPathLookup::File {
                blob_hash: file.content_hash.as_str(),
                size: file.size,
                large: file.large,
            },
            None => SealedPathLookup::Absent,
        },
        Some((_, None)) => SealedPathLookup::Absent,
    }
}

impl ryeos_engine::project_content::SealedDependencyBytes for SealedRealizationDependencyBytes {
    fn sealed_bytes(
        &self,
        absolute_path: &Path,
        max_bytes: u64,
    ) -> Result<
        ryeos_engine::project_content::SealedDependencyContent,
        ryeos_engine::error::EngineError,
    > {
        use ryeos_engine::project_content::SealedDependencyContent;
        let internal = |error: anyhow::Error| {
            ryeos_engine::error::EngineError::Internal(format!(
                "sealed realization bytes for {}: {error:#}",
                absolute_path.display()
            ))
        };
        match locate_sealed_path(&self.mounts, absolute_path) {
            SealedPathLookup::Uncovered => Ok(SealedDependencyContent::Uncovered),
            SealedPathLookup::Absent => Ok(SealedDependencyContent::Absent),
            SealedPathLookup::File {
                blob_hash,
                size,
                large,
            } => {
                if size > max_bytes {
                    return Err(internal(anyhow::anyhow!(
                        "sealed dependency is {size} bytes, over the {max_bytes}-byte ceiling"
                    )));
                }
                self.authority.ensure_guard(&self.guard).map_err(internal)?;
                let bytes = if large {
                    // Large objects live in the store, not the CAS; a store
                    // read is hash-verified here because sealed answers are
                    // exact or refused, never trusted-at-rest.
                    (|| -> anyhow::Result<Vec<u8>> {
                        let store = self.authority.large_object_store()?;
                        let leased = store.lease_object(blob_hash, size)?;
                        let mut bytes = Vec::with_capacity(size as usize);
                        use std::io::Read as _;
                        leased
                            .file()
                            .take(max_bytes.saturating_add(1))
                            .read_to_end(&mut bytes)?;
                        if bytes.len() as u64 != size
                            || lillux::cas::sha256_hex(&bytes) != blob_hash
                        {
                            anyhow::bail!(
                                "large object {blob_hash} contradicts its sealed identity"
                            );
                        }
                        Ok(bytes)
                    })()
                    .map_err(internal)?
                } else {
                    ryeos_state::object_closure::load_exact_cas_blob_with_cas(
                        &self.cas, blob_hash, max_bytes,
                    )
                    .map_err(internal)?
                };
                self.authority.ensure_guard(&self.guard).map_err(internal)?;
                Ok(SealedDependencyContent::Sealed(bytes))
            }
        }
    }
}

/// Sealed dependency source for one dispatched child's plan build, or `None`
/// when live bytes are authoritative for this launch.
///
/// A dispatched child executes under its parent's sealed realization set
/// unless it authors its own declaration. The daemon admits a declaring
/// child's realization from the same authoritative live generation during
/// launch finalization, so no inherited substitution applies here. Fail-closed on broken
/// lineage: a dispatching parent without an admitted capsule is an error,
/// never an empty inheritance.
pub(crate) fn sealed_dependency_bytes_for_child_dispatch(
    state: &ryeos_app::state::AppState,
    params: &super::runner::ExecutionParams,
) -> anyhow::Result<Option<SealedRealizationDependencyBytes>> {
    let Some(parent_thread_id) = params.parent_thread_id.as_deref() else {
        return Ok(None);
    };
    let Some(admission) = params.resolved.root_admission.as_ref() else {
        return Ok(None);
    };
    let engine = admission.request_engine();
    let resolution = admission.resolution_output();
    let contract = engine
        .kinds
        .get(&params.resolved.kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.external_content.as_ref());
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)?;
    if ryeos_engine::external_content::declarations_from_composed(
        &resolution.composed.composed,
        contract,
        declarer,
    )?
    .is_some()
    {
        return Ok(None);
    }
    let realized = state
        .state_store
        .admitted_launch_capsule(parent_thread_id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "dispatching parent {parent_thread_id} has no authoritative admitted launch capsule"
            )
        })?
        .external_realization_set()?;
    let Some(realized) = realized else {
        return Ok(None);
    };
    if realized.is_empty() {
        return Ok(None);
    }
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let project_root = params.provenance.effective_path();
    let mut mounts = Vec::with_capacity(realized.iter().len());
    for entry in realized.iter() {
        if let Some(manifest) =
            ryeos_state::objects::load_if_large_content_manifest(&cas, &entry.manifest_hash)?
        {
            if manifest.entry_count != entry.entry_count
                || manifest.total_bytes != entry.total_bytes
            {
                anyhow::bail!(
                    "external realization `{}` contradicts manifest {} statistics",
                    entry.id,
                    entry.manifest_hash
                );
            }
            let files = manifest
                .entries
                .iter()
                .filter(|entry| {
                    entry.kind == ryeos_state::objects::ExternalContentManifestEntryKind::File
                })
                .map(|entry| {
                    let (content_hash, large) =
                        match (entry.blob_hash.as_ref(), entry.file_sha256.as_ref()) {
                            (Some(hash), None) => (hash.clone(), false),
                            (None, Some(hash)) => (hash.clone(), true),
                            _ => unreachable!("validated large manifest file has one storage tier"),
                        };
                    (
                        entry.path.clone(),
                        SealedRealizationFile {
                            content_hash,
                            size: entry.size.expect("validated file entry has a size"),
                            large,
                        },
                    )
                })
                .collect();
            mounts.push(SealedRealizationMount {
                destination: project_root.join(&entry.mount),
                kind: entry.kind,
                files,
            });
            continue;
        }
        let closure =
            ryeos_state::VerifiedExternalContentClosure::load(&cas, &entry.manifest_hash)?;
        if closure.manifest().entry_count != entry.entry_count
            || closure.manifest().total_bytes != entry.total_bytes
        {
            anyhow::bail!(
                "external realization `{}` contradicts manifest {} statistics",
                entry.id,
                entry.manifest_hash
            );
        }
        let files = closure
            .manifest()
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == ryeos_state::objects::ExternalContentManifestEntryKind::File
            })
            .map(|entry| {
                (
                    entry.path.clone(),
                    SealedRealizationFile {
                        content_hash: entry
                            .blob_hash
                            .clone()
                            .expect("validated file entry has a blob hash"),
                        size: entry.size.expect("validated file entry has a size"),
                        large: false,
                    },
                )
            })
            .collect();
        mounts.push(SealedRealizationMount {
            destination: project_root.join(&entry.mount),
            kind: entry.kind,
            files,
        });
    }
    authority.ensure_guard(&guard)?;
    Ok(Some(SealedRealizationDependencyBytes {
        authority,
        guard,
        cas,
        mounts,
    }))
}

fn ensure_materialization_parent(
    root: &lillux::PinnedDirectory,
    relative: &str,
) -> anyhow::Result<(lillux::PinnedDirectory, OsString)> {
    let mut components = relative.split('/').peekable();
    let mut parent = root.try_clone()?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((parent, OsString::from(component)));
        }
        parent = parent.open_or_create_child(OsStr::new(component), 0o755)?;
    }
    anyhow::bail!("external materialization path is empty")
}

fn symlink_target_bytes(
    entry: &ryeos_state::objects::ExternalContentManifestEntry,
) -> anyhow::Result<Vec<u8>> {
    let target = entry
        .target
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("validated symlink entry {} lost its target", entry.path))?
        .as_bytes()
        .to_vec();
    ryeos_state::objects::validate_internal_symlink_target(&entry.path, &target)?;
    Ok(target)
}

fn large_symlink_target_bytes(
    entry: &ryeos_state::objects::ExternalLargeContentManifestEntry,
) -> anyhow::Result<Vec<u8>> {
    let target = entry
        .target
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("validated symlink entry {} lost its target", entry.path))?
        .as_bytes()
        .to_vec();
    ryeos_state::objects::validate_internal_symlink_target(&entry.path, &target)?;
    Ok(target)
}

fn materialize_executable_large_file(
    source: &fs::File,
    parent: &lillux::PinnedDirectory,
    name: &OsStr,
    expected_size: u64,
) -> anyhow::Result<()> {
    let mut source = source.try_clone()?;
    source.rewind()?;
    let mut target = parent.open_regular_create(name, true, true, 0o600)?;
    let copied = std::io::copy(
        &mut source.take(expected_size.saturating_add(1)),
        &mut target,
    )?;
    if copied != expected_size {
        anyhow::bail!(
            "large executable changed size during materialization: expected {expected_size}, copied {copied}"
        );
    }
    target.sync_all()?;
    lillux::secure_fs::set_open_regular_file_mode(&target, 0o755)?;
    target.sync_all()?;
    Ok(())
}

enum LargeMaterializationVerification<'a> {
    SharedObjects(&'a BTreeMap<String, ryeos_state::LeasedLargeObject>),
    PrivateDigest,
}

fn verify_large_materialized_tree(
    cas: &lillux::CasStore,
    root: &lillux::PinnedDirectory,
    manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
    verification: LargeMaterializationVerification<'_>,
) -> anyhow::Result<()> {
    let expected = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut observed = Vec::with_capacity(expected.len());
    verify_large_materialized_directory(cas, root, "", &expected, &verification, &mut observed)?;
    observed.sort();
    let expected_paths = expected.keys().copied().collect::<Vec<_>>();
    if observed.iter().map(String::as_str).collect::<Vec<_>>() != expected_paths {
        anyhow::bail!("materialized large-content tree has missing or extra entries");
    }
    Ok(())
}

fn verify_large_materialized_directory(
    cas: &lillux::CasStore,
    directory: &lillux::PinnedDirectory,
    prefix: &str,
    expected: &BTreeMap<&str, &ryeos_state::objects::ExternalLargeContentManifestEntry>,
    verification: &LargeMaterializationVerification<'_>,
    observed: &mut Vec<String>,
) -> anyhow::Result<()> {
    for actual in directory.entries_no_follow()? {
        let name = actual.name.to_str().ok_or_else(|| {
            anyhow::anyhow!("large-content materialization contains a non-UTF-8 filename")
        })?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let entry = expected.get(path.as_str()).ok_or_else(|| {
            anyhow::anyhow!("large-content materialization contains unexpected entry {path}")
        })?;
        observed.push(path.clone());
        match entry.kind {
            ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                if actual.entry_type != lillux::PinnedEntryType::Directory {
                    anyhow::bail!("large-content materialization entry {path} is not a directory");
                }
                let child = directory
                    .open_child_directory(&actual.name)?
                    .ok_or_else(|| anyhow::anyhow!("materialized directory {path} disappeared"))?;
                verify_large_materialized_directory(
                    cas,
                    &child,
                    &path,
                    expected,
                    verification,
                    observed,
                )?;
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::File => {
                if actual.entry_type != lillux::PinnedEntryType::Regular {
                    anyhow::bail!("large-content materialization entry {path} is not a file");
                }
                let mut file = directory
                    .open_regular(&actual.name, false)?
                    .ok_or_else(|| anyhow::anyhow!("materialized file {path} disappeared"))?;
                let metadata = file.metadata()?;
                let expected_size = entry.size.expect("validated file has a size");
                if metadata.len() != expected_size
                    || Some(lillux::normalized_portable_regular_mode(&metadata)?) != entry.mode
                {
                    anyhow::bail!("materialized large-content file {path} has wrong metadata");
                }
                if let Some(blob_hash) = entry.blob_hash.as_deref() {
                    file.rewind()?;
                    let (digest, _) =
                        lillux::digest_open_regular_file_stable_exact(&mut file, expected_size)?;
                    if digest != blob_hash {
                        anyhow::bail!("materialized CAS file {path} contradicts its manifest");
                    }
                } else {
                    let file_sha256 = entry
                        .file_sha256
                        .as_deref()
                        .expect("validated file has one storage tier");
                    if let LargeMaterializationVerification::SharedObjects(large_sources) =
                        verification
                        && entry.mode == Some(0o644)
                    {
                        let source = large_sources.get(file_sha256).ok_or_else(|| {
                            anyhow::anyhow!("large-content file {path} lost its source lease")
                        })?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt as _;
                            let source_metadata = source.file().metadata()?;
                            if metadata.dev() != source_metadata.dev()
                                || metadata.ino() != source_metadata.ino()
                            {
                                anyhow::bail!(
                                    "materialized large-content file {path} is not the leased object"
                                );
                            }
                        }
                    } else {
                        file.rewind()?;
                        let (digest, _) = lillux::digest_open_regular_file_stable_exact(
                            &mut file,
                            expected_size,
                        )?;
                        if digest != file_sha256 {
                            anyhow::bail!(
                                "materialized executable {path} contradicts its large object"
                            );
                        }
                    }
                }
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => {
                if actual.entry_type != lillux::PinnedEntryType::Symlink {
                    anyhow::bail!("large-content materialization entry {path} is not a symlink");
                }
                let target = directory
                    .read_symlink_target(
                        &actual.name,
                        ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("materialized symlink {path} disappeared"))?;
                if target != large_symlink_target_bytes(entry)? {
                    anyhow::bail!("materialized symlink {path} contradicts its manifest");
                }
            }
        }
    }
    Ok(())
}

fn verify_materialized_tree(
    cas: &lillux::CasStore,
    root: &lillux::PinnedDirectory,
    manifest: &ryeos_state::objects::ExternalContentManifestObject,
) -> anyhow::Result<()> {
    let expected = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut observed = Vec::with_capacity(expected.len());
    verify_materialized_directory(cas, root, "", &expected, &mut observed)?;
    observed.sort();
    let expected_paths = expected.keys().copied().collect::<Vec<_>>();
    if observed.iter().map(String::as_str).collect::<Vec<_>>() != expected_paths {
        anyhow::bail!("materialized external-content tree has missing or extra entries");
    }
    Ok(())
}

fn verify_materialized_directory(
    cas: &lillux::CasStore,
    directory: &lillux::PinnedDirectory,
    prefix: &str,
    expected: &BTreeMap<&str, &ryeos_state::objects::ExternalContentManifestEntry>,
    observed: &mut Vec<String>,
) -> anyhow::Result<()> {
    for actual in directory.entries_no_follow()? {
        let name = actual.name.to_str().ok_or_else(|| {
            anyhow::anyhow!("external materialization contains a non-UTF-8 filename")
        })?;
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let entry = expected.get(path.as_str()).ok_or_else(|| {
            anyhow::anyhow!("external materialization contains unexpected entry {path}")
        })?;
        observed.push(path.clone());
        match entry.kind {
            ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                if actual.entry_type != lillux::PinnedEntryType::Directory {
                    anyhow::bail!("external materialization entry {path} is not a directory");
                }
                let child = directory
                    .open_child_directory(&actual.name)?
                    .ok_or_else(|| anyhow::anyhow!("materialized directory {path} disappeared"))?;
                verify_materialized_directory(cas, &child, &path, expected, observed)?;
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::File => {
                if actual.entry_type != lillux::PinnedEntryType::Regular {
                    anyhow::bail!("external materialization entry {path} is not a regular file");
                }
                let mut file = directory
                    .open_regular(&actual.name, false)?
                    .ok_or_else(|| anyhow::anyhow!("materialized file {path} disappeared"))?;
                file.rewind()?;
                let (digest, metadata) = lillux::digest_open_regular_file_stable_exact(
                    &mut file,
                    entry.size.expect("validated file entry has a size"),
                )?;
                if Some(digest.as_str()) != entry.blob_hash.as_deref()
                    || Some(lillux::normalized_portable_regular_mode(&metadata)?) != entry.mode
                {
                    anyhow::bail!("materialized external file {path} contradicts its manifest");
                }
            }
            ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => {
                if actual.entry_type != lillux::PinnedEntryType::Symlink {
                    anyhow::bail!("external materialization entry {path} is not a symlink");
                }
                let actual_target = directory
                    .read_symlink_target(
                        &actual.name,
                        ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?
                    .ok_or_else(|| anyhow::anyhow!("materialized symlink {path} disappeared"))?;
                if actual_target != symlink_target_bytes(entry)? {
                    anyhow::bail!("materialized symlink {path} contradicts its manifest");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_mount(destination: &str, files: &[(&str, u64)]) -> SealedRealizationMount {
        SealedRealizationMount {
            destination: PathBuf::from(destination),
            kind: ExternalContentKind::Tree,
            files: files
                .iter()
                .map(|(path, size)| {
                    (
                        (*path).to_owned(),
                        SealedRealizationFile {
                            content_hash: format!("hash-{path}"),
                            size: *size,
                            large: false,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn sealed_path_lookup_answers_like_the_mount_table() {
        let mounts = vec![
            tree_mount(
                "/workspace/content/lib",
                &[("bfs.py", 10), ("deep/util.py", 20)],
            ),
            SealedRealizationMount {
                destination: PathBuf::from("/workspace/content/entry.py"),
                kind: ExternalContentKind::File,
                files: BTreeMap::from([(
                    ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH.to_owned(),
                    SealedRealizationFile {
                        content_hash: "hash-file".to_owned(),
                        size: 5,
                        large: false,
                    },
                )]),
            },
        ];
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/lib/bfs.py")),
            SealedPathLookup::File {
                blob_hash: "hash-bfs.py",
                size: 10,
                large: false
            }
        ));
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/lib/deep/util.py")),
            SealedPathLookup::File {
                blob_hash: "hash-deep/util.py",
                size: 20,
                large: false
            }
        ));
        // A file-kind realization answers for exactly its mount path.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/entry.py")),
            SealedPathLookup::File {
                blob_hash: "hash-file",
                size: 5,
                large: false
            }
        ));
        // Covered but unsealed paths are absent at execution, never live.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/lib/scratch.py")),
            SealedPathLookup::Absent
        ));
        // Outside every mount, live bytes stay authoritative.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/evidence.py")),
            SealedPathLookup::Uncovered
        ));
        // A lexical prefix that is not a path-component prefix is uncovered.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/libx.py")),
            SealedPathLookup::Uncovered
        ));
    }

    #[test]
    fn nested_mounts_resolve_to_the_most_specific_destination() {
        let mounts = vec![
            tree_mount("/workspace/content", &[("lib/bfs.py", 1), ("run.py", 2)]),
            tree_mount("/workspace/content/lib", &[("other.py", 3)]),
        ];
        // The inner mount owns its subtree: bfs.py is sealed only in the
        // outer manifest, so under the inner mount it does not exist.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/lib/bfs.py")),
            SealedPathLookup::Absent
        ));
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/lib/other.py")),
            SealedPathLookup::File {
                blob_hash: "hash-other.py",
                size: 3,
                large: false
            }
        ));
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/workspace/content/run.py")),
            SealedPathLookup::File {
                blob_hash: "hash-run.py",
                size: 2,
                large: false
            }
        ));
    }

    // ── Materialization cache and sweep ─────────────────────────────────

    fn temp_cas() -> (tempfile::TempDir, lillux::CasStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cas");
        std::fs::create_dir_all(&root).unwrap();
        (dir, lillux::CasStore::new(root))
    }

    fn store_tree_closure(
        cas: &lillux::CasStore,
        files: &[(&str, &[u8])],
    ) -> ryeos_state::VerifiedExternalContentClosure {
        let mut entries = files
            .iter()
            .map(
                |(path, bytes)| ryeos_state::objects::ExternalContentManifestEntry {
                    path: (*path).to_string(),
                    kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
                    mode: Some(0o644),
                    blob_hash: Some(cas.store_blob(bytes).unwrap()),
                    size: Some(bytes.len() as u64),
                    target: None,
                },
            )
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let manifest = ryeos_state::objects::ExternalContentManifestObject {
            schema: ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_string(),
            kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            total_bytes: entries.iter().filter_map(|entry| entry.size).sum(),
            entries,
        };
        let hash = cas
            .store_object(&serde_json::to_value(&manifest).unwrap())
            .unwrap();
        ryeos_state::VerifiedExternalContentClosure::load(cas, &hash).unwrap()
    }

    #[test]
    fn materialize_builds_verifies_and_heals_the_generation() {
        let (dir, cas) = temp_cas();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let cache = ExternalMaterializationCache::from_runtime_state_root(&app_root);
        let closure =
            store_tree_closure(&cas, &[("alpha.txt", b"alpha"), ("omega.txt", b"omega!")]);

        let generation = cache
            .materialize(&cas, &closure, ExternalContentKind::Tree)
            .unwrap();
        let root = generation.source_path.clone();
        assert_eq!(std::fs::read(root.join("alpha.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(root.join("omega.txt")).unwrap(), b"omega!");
        drop(generation);

        // A generation that no longer matches its manifest must be discarded
        // and rebuilt, never served: the tree on disk is a cache, and the
        // manifest is the identity.
        std::fs::write(root.join("alpha.txt"), b"corrupted").unwrap();
        let healed = cache
            .materialize(&cas, &closure, ExternalContentKind::Tree)
            .unwrap();
        assert_eq!(
            std::fs::read(healed.source_path.join("alpha.txt")).unwrap(),
            b"alpha"
        );
    }

    #[test]
    fn private_workspace_receives_an_exact_independent_tree() {
        let (dir, cas) = temp_cas();
        let app_root = dir.path().join("app");
        let workspace_path = dir.path().join("workspace");
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::create_dir_all(&workspace_path).unwrap();
        let cache = ExternalMaterializationCache::from_runtime_state_root(&app_root);
        let closure = store_tree_closure(&cas, &[("value.txt", b"sealed")]);
        let generation = cache
            .materialize(&cas, &closure, ExternalContentKind::Tree)
            .unwrap();
        let workspace = lillux::PinnedDirectory::open(&workspace_path)
            .unwrap()
            .unwrap();
        let budget = PrivateMaterializationBudget::new(u64::MAX);

        publish_private_generation(
            &workspace,
            "runtime/content",
            ExternalContentKind::Tree,
            |staging| {
                copy_materialized_tree(&generation.root, staging, closure.manifest(), &budget)
            },
            |staging| verify_materialized_tree(&cas, staging, closure.manifest()),
            |existing| match existing {
                lillux::PinnedDirectoryEntry::Directory(directory) => {
                    verify_materialized_tree(&cas, directory, closure.manifest())
                }
                lillux::PinnedDirectoryEntry::Regular(_) => {
                    anyhow::bail!("tree realization destination is a regular file")
                }
            },
        )
        .unwrap();

        let private_file = workspace_path.join("runtime/content/value.txt");
        let cached_file = generation.source_path.join("value.txt");
        assert_eq!(std::fs::read(&private_file).unwrap(), b"sealed");
        publish_private_generation(
            &workspace,
            "runtime/content",
            ExternalContentKind::Tree,
            |staging| {
                copy_materialized_tree(&generation.root, staging, closure.manifest(), &budget)
            },
            |staging| verify_materialized_tree(&cas, staging, closure.manifest()),
            |existing| match existing {
                lillux::PinnedDirectoryEntry::Directory(directory) => {
                    verify_materialized_tree(&cas, directory, closure.manifest())
                }
                lillux::PinnedDirectoryEntry::Regular(_) => {
                    anyhow::bail!("tree realization destination is a regular file")
                }
            },
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_ne!(
                std::fs::metadata(&private_file).unwrap().ino(),
                std::fs::metadata(&cached_file).unwrap().ino()
            );
        }
        std::fs::write(&private_file, b"private mutation").unwrap();
        std::fs::create_dir_all(workspace_path.join("runtime/content/__pycache__")).unwrap();
        std::fs::write(
            workspace_path.join("runtime/content/__pycache__/module.pyc"),
            b"ambient bytecode",
        )
        .unwrap();
        assert_eq!(std::fs::read(cached_file).unwrap(), b"sealed");
        publish_private_generation(
            &workspace,
            "runtime/content",
            ExternalContentKind::Tree,
            |staging| {
                copy_materialized_tree(&generation.root, staging, closure.manifest(), &budget)
            },
            |staging| verify_materialized_tree(&cas, staging, closure.manifest()),
            |existing| match existing {
                lillux::PinnedDirectoryEntry::Directory(directory) => {
                    verify_materialized_tree(&cas, directory, closure.manifest())
                }
                lillux::PinnedDirectoryEntry::Regular(_) => {
                    anyhow::bail!("tree realization destination is a regular file")
                }
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(&private_file).unwrap(), b"sealed");
        assert!(
            !workspace_path.join("runtime/content/__pycache__").exists(),
            "excluded or runtime-created paths must not remain visible beside an exact realization"
        );
    }

    #[test]
    fn large_tree_materialization_preserves_modes_links_and_large_inodes() {
        let (dir, cas) = temp_cas();
        let runtime_root = dir.path().join("state");
        let runtime = lillux::PinnedDirectory::open_or_create(&runtime_root).unwrap();
        let store = ryeos_state::LargeObjectStore::open_or_create_under(&runtime).unwrap();
        let source_path = dir.path().join("large-source.bin");
        let large_bytes = b"large-object-payload-with-several-chunks";
        std::fs::write(&source_path, large_bytes).unwrap();
        let source = std::fs::File::open(&source_path).unwrap();
        let metadata = source.metadata().unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        let ingested = store
            .ingest_open_regular(
                source,
                ryeos_state::PinnedLargeObjectSourceIdentity {
                    containing_device: metadata.dev(),
                    inode: metadata.ino(),
                    size: metadata.len(),
                },
                "lib/payload.so",
                None,
            )
            .unwrap();
        let driver_bytes = b"#!/bin/sh\nexit 0\n";
        let driver = cas.store_blob(driver_bytes).unwrap();
        let mut entries = vec![
            ryeos_state::objects::ExternalLargeContentManifestEntry {
                path: "bin".to_owned(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::Dir,
                mode: None,
                blob_hash: None,
                file_sha256: None,
                size: None,
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
            },
            ryeos_state::objects::ExternalLargeContentManifestEntry {
                path: "bin/driver".to_owned(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
                mode: Some(0o755),
                blob_hash: Some(driver),
                file_sha256: None,
                size: Some(driver_bytes.len() as u64),
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
            },
            ryeos_state::objects::ExternalLargeContentManifestEntry {
                path: "lib".to_owned(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::Dir,
                mode: None,
                blob_hash: None,
                file_sha256: None,
                size: None,
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
            },
            ryeos_state::objects::ExternalLargeContentManifestEntry {
                path: "lib/payload.so".to_owned(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
                mode: Some(0o644),
                blob_hash: None,
                file_sha256: Some(ingested.file_sha256.clone()),
                size: Some(ingested.size),
                chunk_size: Some(ingested.chunk_size),
                chunk_hashes: ingested.chunk_hashes.clone(),
                target: None,
            },
            ryeos_state::objects::ExternalLargeContentManifestEntry {
                path: "lib/payload.so.1".to_owned(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::Symlink,
                mode: None,
                blob_hash: None,
                file_sha256: None,
                size: None,
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: Some("payload.so".to_owned()),
            },
        ];
        entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
        let manifest = ryeos_state::objects::ExternalLargeContentManifestObject {
            schema: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA.to_owned(),
            kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
            entry_count: entries.len(),
            total_bytes: ingested.size + driver_bytes.len() as u64,
            entries,
        };
        manifest.validate().unwrap();
        let manifest_hash = cas.store_object(&manifest.to_value().unwrap()).unwrap();
        let cache = ExternalMaterializationCache::from_runtime_state_root(&runtime_root);
        let generation = cache
            .materialize_large(
                &cas,
                &store,
                &manifest_hash,
                &manifest,
                ExternalContentKind::Tree,
            )
            .unwrap();
        let root = generation.source_path.clone();
        assert_eq!(
            std::fs::read(root.join("lib/payload.so")).unwrap(),
            large_bytes
        );
        assert_eq!(
            std::fs::read_link(root.join("lib/payload.so.1")).unwrap(),
            PathBuf::from("payload.so")
        );
        assert_eq!(
            lillux::normalized_portable_regular_mode(
                &std::fs::metadata(root.join("bin/driver")).unwrap()
            )
            .unwrap(),
            0o755
        );
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(root.join("lib/payload.so"))
                .unwrap()
                .ino(),
            store
                .lease_object(&ingested.file_sha256, ingested.size)
                .unwrap()
                .file()
                .metadata()
                .unwrap()
                .ino()
        );

        let private_path = dir.path().join("private");
        std::fs::create_dir_all(&private_path).unwrap();
        let private = lillux::PinnedDirectory::open(&private_path)
            .unwrap()
            .unwrap();
        let budget = PrivateMaterializationBudget::new(u64::MAX);
        publish_private_generation(
            &private,
            "model",
            ExternalContentKind::Tree,
            |staging| copy_large_materialized_tree(&generation.root, staging, &manifest, &budget),
            |staging| {
                verify_large_materialized_tree(
                    &cas,
                    staging,
                    &manifest,
                    LargeMaterializationVerification::PrivateDigest,
                )
            },
            |existing| match existing {
                lillux::PinnedDirectoryEntry::Directory(directory) => {
                    verify_large_materialized_tree(
                        &cas,
                        directory,
                        &manifest,
                        LargeMaterializationVerification::PrivateDigest,
                    )
                }
                lillux::PinnedDirectoryEntry::Regular(_) => {
                    anyhow::bail!("tree realization destination is a regular file")
                }
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read(private_path.join("model/lib/payload.so")).unwrap(),
            large_bytes
        );
        #[cfg(unix)]
        assert_ne!(
            std::fs::metadata(private_path.join("model/lib/payload.so"))
                .unwrap()
                .ino(),
            std::fs::metadata(root.join("lib/payload.so"))
                .unwrap()
                .ino()
        );
    }

    #[test]
    fn a_file_realization_materializes_its_content_entry() {
        let (dir, cas) = temp_cas();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let cache = ExternalMaterializationCache::from_runtime_state_root(&app_root);
        let closure = store_tree_closure(
            &cas,
            &[(
                ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH,
                b"payload",
            )],
        );

        let generation = cache
            .materialize(&cas, &closure, ExternalContentKind::File)
            .unwrap();
        assert!(
            generation
                .source_path
                .ends_with(ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH)
        );
        assert_eq!(std::fs::read(&generation.source_path).unwrap(), b"payload");
    }

    #[test]
    fn private_file_realization_publishes_at_the_mount_path() {
        let (dir, cas) = temp_cas();
        let app_root = dir.path().join("app");
        let workspace_path = dir.path().join("workspace");
        std::fs::create_dir_all(&app_root).unwrap();
        std::fs::create_dir_all(&workspace_path).unwrap();
        let cache = ExternalMaterializationCache::from_runtime_state_root(&app_root);
        let closure = store_tree_closure(
            &cas,
            &[(
                ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH,
                b"payload",
            )],
        );
        let generation = cache
            .materialize(&cas, &closure, ExternalContentKind::File)
            .unwrap();
        let workspace = lillux::PinnedDirectory::open(&workspace_path)
            .unwrap()
            .unwrap();
        let budget = PrivateMaterializationBudget::new(u64::MAX);

        publish_private_generation(
            &workspace,
            "config/model.bin",
            ExternalContentKind::File,
            |staging| {
                copy_materialized_tree(&generation.root, staging, closure.manifest(), &budget)
            },
            |staging| verify_materialized_tree(&cas, staging, closure.manifest()),
            |existing| match existing {
                lillux::PinnedDirectoryEntry::Regular(file) => {
                    let expected = closure
                        .manifest()
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.path
                                == ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH
                        })
                        .unwrap();
                    verify_open_regular_exact(
                        file,
                        expected.size.unwrap(),
                        expected.mode.unwrap(),
                        expected.blob_hash.as_deref().unwrap(),
                    )
                }
                lillux::PinnedDirectoryEntry::Directory(_) => {
                    anyhow::bail!("file realization destination is a directory")
                }
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read(workspace_path.join("config/model.bin")).unwrap(),
            b"payload"
        );
        assert!(!workspace_path.join("config/model.bin/content").exists());
        publish_private_generation(
            &workspace,
            "config/model.bin",
            ExternalContentKind::File,
            |staging| {
                copy_materialized_tree(&generation.root, staging, closure.manifest(), &budget)
            },
            |staging| verify_materialized_tree(&cas, staging, closure.manifest()),
            |existing| match existing {
                lillux::PinnedDirectoryEntry::Regular(file) => {
                    let expected = closure
                        .manifest()
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.path
                                == ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH
                        })
                        .unwrap();
                    verify_open_regular_exact(
                        file,
                        expected.size.unwrap(),
                        expected.mode.unwrap(),
                        expected.blob_hash.as_deref().unwrap(),
                    )
                }
                lillux::PinnedDirectoryEntry::Directory(_) => {
                    anyhow::bail!("file realization destination is a directory")
                }
            },
        )
        .unwrap();

        std::fs::write(workspace_path.join("config/model.bin"), b"mutated").unwrap();
        publish_private_generation(
            &workspace,
            "config/model.bin",
            ExternalContentKind::File,
            |staging| {
                copy_materialized_tree(&generation.root, staging, closure.manifest(), &budget)
            },
            |staging| verify_materialized_tree(&cas, staging, closure.manifest()),
            |existing| match existing {
                lillux::PinnedDirectoryEntry::Regular(file) => {
                    let expected = closure
                        .manifest()
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.path
                                == ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH
                        })
                        .unwrap();
                    verify_open_regular_exact(
                        file,
                        expected.size.unwrap(),
                        expected.mode.unwrap(),
                        expected.blob_hash.as_deref().unwrap(),
                    )
                }
                lillux::PinnedDirectoryEntry::Directory(_) => {
                    anyhow::bail!("file realization destination is a directory")
                }
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read(workspace_path.join("config/model.bin")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn sweep_reclaims_only_unleased_generations() {
        let (dir, cas) = temp_cas();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let cache = ExternalMaterializationCache::from_runtime_state_root(&app_root);
        let first = store_tree_closure(&cas, &[("one.txt", b"generation one")]);
        let second = store_tree_closure(&cas, &[("two.txt", b"generation two")]);

        let idle = cache
            .materialize(&cas, &first, ExternalContentKind::Tree)
            .unwrap();
        let idle_root = idle.source_path.clone();
        let live = cache
            .materialize(&cas, &second, ExternalContentKind::Tree)
            .unwrap();
        let live_root = live.source_path.clone();
        drop(idle);

        // A zero budget makes everything reclaimable, but the held lease
        // must still protect its generation.
        cache.sweep_to_budget(0).unwrap();
        assert!(!idle_root.exists());
        assert!(live_root.exists());

        drop(live);
        cache.sweep_to_budget(0).unwrap();
        assert!(!live_root.exists());
    }
}
