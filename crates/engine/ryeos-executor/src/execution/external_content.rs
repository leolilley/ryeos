//! Launch-time capture of declared external content.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Seek as _;
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::external_content::{
    DeclaringAuthority, ExternalContentBlobSink, ExternalContentKind, ExternalContentRoot,
    ExternalCapturePolicy, LaunchRealizationBudget,
    MAX_DECLARATION_FILE_BYTES, MAX_SYMLINK_TARGET_BYTES,
};
use ryeos_engine::external_realization::{
    ExternalRealizationProof, RealizationStore, RealizedExternalContent,
    RealizedExternalContentSet,
};

use anyhow::Context as _;

use super::PendingCasPublication;

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
    pub(crate) fn mounts(
        &self,
    ) -> &[ryeos_engine::isolation::IsolationReadOnlyMountAuthority] {
        &self.mounts
    }

    pub(crate) fn sealed_set_env(&self) -> &str {
        &self.sealed_set_env
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
const STALE_STAGING_MAX_AGE: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

struct ExternalMaterializationCache {
    root: PathBuf,
}

struct MaterializedExternalGeneration {
    source_path: PathBuf,
    source: fs::File,
    lease: fs::File,
}

impl ExternalMaterializationCache {
    fn from_app_root(app_root: &Path) -> Self {
        Self {
            root: app_root
                .join(ryeos_engine::AI_DIR)
                .join("state/cache/external-content/v1"),
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
        let lock = locks.open_regular_create(
            OsStr::new(manifest_hash),
            true,
            false,
            0o600,
        )?;
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
        let lease = leases.open_regular_create(
            OsStr::new(manifest_hash),
            true,
            false,
            0o600,
        )?;
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
            source_path,
            source,
            lease,
        })
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
                        let target = symlink_target_bytes(cas, entry)?;
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

/// Verify and materialize the exact realization set committed by a finalized
/// program. No locator or live project path is consulted for source bytes.
pub(crate) fn bind_external_realizations(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    project_path: &Path,
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
    let cache = ExternalMaterializationCache::from_app_root(&state.config.app_root);
    let mut mounts = Vec::with_capacity(realized.iter().len());
    let mut leases = Vec::with_capacity(realized.iter().len());
    for entry in realized.iter() {
        if let Some(manifest) = try_load_large_content_manifest(&cas, &entry.manifest_hash)? {
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
            // Large content never materialize-copies: every manifest entry
            // binds the store's immutable file directly, leased for the
            // spawn's lifetime. A tree shape is one mount per entry.
            let store = authority.large_object_store()?;
            let destination_root = project_path.join(&entry.mount);
            for manifest_entry in &manifest.entries {
                let leased = store
                    .lease_object(&manifest_entry.file_sha256, manifest_entry.size)
                    .with_context(|| {
                        format!(
                            "binding large object for `{}` at `{}`",
                            entry.id, manifest_entry.path
                        )
                    })?;
                let destination = match entry.kind {
                    ExternalContentKind::File => destination_root.clone(),
                    ExternalContentKind::Tree => destination_root.join(&manifest_entry.path),
                };
                let (source_path, source, lease) = leased.into_parts();
                mounts.push(
                    ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new(
                        source_path,
                        destination,
                        source,
                    ),
                );
                leases.push(lease);
            }
            continue;
        }
        let closure = ryeos_state::VerifiedExternalContentClosure::load(
            &cas,
            &entry.manifest_hash,
        )?;
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
        mounts.push(ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new(
            generation.source_path,
            project_path.join(&entry.mount),
            generation.source,
        ));
        leases.push(generation.lease);
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
    /// Manifest path → (content hash, exact size) for regular-file entries.
    files: BTreeMap<String, (String, u64)>,
    /// Whether hashes name large-object store files instead of CAS blobs.
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
            Some((blob_hash, size)) => SealedPathLookup::File {
                blob_hash: blob_hash.as_str(),
                size: *size,
                large: mount.large,
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
                self.authority
                    .ensure_guard(&self.guard)
                    .map_err(internal)?;
                let bytes = if large {
                    // Large objects live in the store, not the CAS; a store
                    // read is hash-verified here because sealed answers are
                    // exact or refused, never trusted-at-rest.
                    (|| -> anyhow::Result<Vec<u8>> {
                        let store = self.authority.large_object_store()?;
                        let leased = store.lease_object(blob_hash, size)?;
                        let mut bytes = Vec::with_capacity(size as usize);
                        use std::io::Read as _;
                        leased.file().take(max_bytes.saturating_add(1)).read_to_end(&mut bytes)?;
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
                self.authority
                    .ensure_guard(&self.guard)
                    .map_err(internal)?;
                Ok(SealedDependencyContent::Sealed(bytes))
            }
        }
    }
}

/// Sealed dependency source for one dispatched child's plan build, or `None`
/// when live bytes are authoritative for this launch.
///
/// A dispatched child executes under its parent's sealed realization set
/// unless it authors its own declaration. A declaring child's realization is
/// captured fresh from the live tree at launch, so live bytes are exactly
/// what will execute and no substitution applies. Fail-closed on broken
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
    let roots = engine.resolution_roots(match params.provenance.project_authority() {
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => None,
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
        | ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { .. } => {
            Some(params.provenance.original_project_path().to_path_buf())
        }
    });
    let contract = engine
        .kinds
        .get(&params.resolved.kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.external_content.as_ref());
    let declarer = declaring_authority(resolution, &roots)?;
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
        if let Some(manifest) = try_load_large_content_manifest(&cas, &entry.manifest_hash)? {
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
                .map(|entry| {
                    (
                        entry.path.clone(),
                        (entry.file_sha256.clone(), entry.size),
                    )
                })
                .collect();
            mounts.push(SealedRealizationMount {
                destination: project_root.join(&entry.mount),
                kind: entry.kind,
                files,
                large: true,
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
                    (
                        entry
                            .blob_hash
                            .clone()
                            .expect("validated file entry has a blob hash"),
                        entry.size.expect("validated file entry has a size"),
                    ),
                )
            })
            .collect();
        mounts.push(SealedRealizationMount {
            destination: project_root.join(&entry.mount),
            kind: entry.kind,
            files,
            large: false,
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
    cas: &lillux::CasStore,
    entry: &ryeos_state::objects::ExternalContentManifestEntry,
) -> anyhow::Result<Vec<u8>> {
    match (entry.target.as_deref(), entry.target_blob.as_deref()) {
        (Some(target), None) => Ok(target.as_bytes().to_vec()),
        (None, Some(hash)) => ryeos_state::object_closure::load_exact_cas_blob_with_cas(
            cas,
            hash,
            ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES,
        ),
        _ => anyhow::bail!("validated symlink entry {} lost its target", entry.path),
    }
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
                if actual_target != symlink_target_bytes(cas, entry)? {
                    anyhow::bail!("materialized symlink {path} contradicts its manifest");
                }
            }
        }
    }
    Ok(())
}

/// Capture output retained until the admitted capsule becomes a durable CAS
/// root. Dropping before publication retires (or conservatively abandons) the
/// staged-root lease; it never exposes an unrooted realization.
pub(crate) struct CapturedExternalRealizations {
    proof: ExternalRealizationProof,
    store: ExternalRealizationStore,
    publication: Option<PendingCasPublication>,
}

impl CapturedExternalRealizations {
    pub(crate) fn finalization_evidence(
        &self,
    ) -> (&ExternalRealizationProof, &dyn RealizationStore) {
        (&self.proof, &self.store)
    }

    pub(crate) fn into_publication(mut self) -> Option<PendingCasPublication> {
        self.publication.take()
    }
}

/// Reconstruct the finalization evidence for a sealed realization without
/// consulting any live locator. Recovery is deliberately CAS-only: a missing
/// manifest or blob is an availability failure, never permission to recapture.
pub(crate) fn recover_external_realizations(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<Option<CapturedExternalRealizations>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    else {
        return Ok(None);
    };
    let realized = RealizedExternalContentSet::from_value(value)?;
    let authority = super::pinned_state_authority(state)?;
    let store = ExternalRealizationStore::new(authority);
    let proof = ryeos_engine::external_realization::prove_external_realizations(realized, &store)?;
    Ok(Some(CapturedExternalRealizations {
        proof,
        store,
        publication: None,
    }))
}

/// Inherit the dispatching program's sealed realization verbatim.
///
/// Every descendant of one admitted solve executes against the
/// same realization — manifests *and* logical mounts — unless it authors its
/// own declaration, which replaces inheritance entirely. Redeemability is
/// re-proved from CAS under the pinned authority. No staged publication is
/// needed: the parent's capsule roots every manifest across the admission
/// window, and the child's own capsule roots them durably once sealed.
fn inherit_external_realizations(
    state: &ryeos_app::state::AppState,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    inherited: Option<&RealizedExternalContentSet>,
) -> anyhow::Result<Option<CapturedExternalRealizations>> {
    let Some(inherited) = inherited else {
        return Ok(None);
    };
    let realized = inherited.clone();
    resolution.composed.derived.insert(
        ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY.to_string(),
        realized.to_value()?,
    );
    let authority = super::pinned_state_authority(state)?;
    let store = ExternalRealizationStore::new(authority);
    let proof =
        ryeos_engine::external_realization::prove_external_realizations(realized, &store)?;
    tracing::info!(
        realization_count = proof.realized().iter().len(),
        "inherited external content realization"
    );
    Ok(Some(CapturedExternalRealizations {
        proof,
        store,
        publication: None,
    }))
}

/// Exact CAS authority used to re-prove a realization at finalization.
pub(crate) struct ExternalRealizationStore {
    authority: ryeos_state::PinnedStateAuthority,
}

impl ExternalRealizationStore {
    pub(crate) fn new(authority: ryeos_state::PinnedStateAuthority) -> Self {
        Self { authority }
    }
}

impl RealizationStore for ExternalRealizationStore {
    fn realization_available(&self, manifest_hash: &str) -> anyhow::Result<bool> {
        let guard = self.authority.acquire_shared_guard()?;
        self.authority.ensure_guard(&guard)?;
        let cas = self.authority.cas_store()?;
        if let Some(manifest) = try_load_large_content_manifest(&cas, manifest_hash)? {
            let store = self.authority.large_object_store()?;
            for entry in &manifest.entries {
                if store.object_size(&entry.file_sha256)? != Some(entry.size) {
                    anyhow::bail!(
                        "large object {} for `{}` is not in the store at its sealed size",
                        entry.file_sha256,
                        entry.path
                    );
                }
            }
            return Ok(true);
        }
        ryeos_state::VerifiedExternalContentClosure::load(&cas, manifest_hash).map(|_| true)
    }
}

struct GuardedCasBlobSink<'a> {
    guard: &'a ryeos_state::CasMutationGuard,
    cas: &'a lillux::CasStore,
    staged_roots: &'a mut ryeos_state::StagedCasRootLease,
    stored_blobs: usize,
    reused_blobs: usize,
}

impl GuardedCasBlobSink<'_> {
    fn counts(&self) -> (usize, usize) {
        (self.stored_blobs, self.reused_blobs)
    }
}

impl ExternalContentBlobSink for GuardedCasBlobSink<'_> {
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        if expected_size > MAX_DECLARATION_FILE_BYTES {
            anyhow::bail!(
                "external content file {path} exceeds {MAX_DECLARATION_FILE_BYTES} bytes"
            );
        }
        let outcome = self.cas.put_blob_from_open_regular_bounded(
            file,
            Path::new(path),
            MAX_DECLARATION_FILE_BYTES,
        )?;
        self.staged_roots
            .protect_blob_hash_admitted(self.guard, &outcome.hash)?;
        if outcome.created {
            self.stored_blobs += 1;
        } else {
            self.reused_blobs += 1;
        }
        Ok((outcome.hash, outcome.size))
    }

    fn store_target(&mut self, target: &[u8], path: &str) -> anyhow::Result<String> {
        if target.is_empty() || target.len() > MAX_SYMLINK_TARGET_BYTES || target.contains(&0) {
            anyhow::bail!("external content symlink {path} has an invalid target");
        }
        let expected = lillux::sha256_hex(target);
        let existed = self.cas.has_blob(&expected)?;
        let hash = self
            .staged_roots
            .store_blob_admitted(self.guard, self.cas, target)?;
        if existed {
            self.reused_blobs += 1;
        } else {
            self.stored_blobs += 1;
        }
        Ok(hash)
    }
}

/// Fetch `digest` and decode it as a large-content manifest, or `None` when
/// the object is absent or is some other kind. Routing is data: the pinned
/// manifest names its own tier.
fn try_load_large_content_manifest(
    cas: &lillux::CasStore,
    digest: &str,
) -> anyhow::Result<Option<ryeos_state::objects::ExternalLargeContentManifestObject>> {
    let Some(value) = cas.get_object(digest)? else {
        return Ok(None);
    };
    if value.get("kind").and_then(serde_json::Value::as_str)
        != Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND)
    {
        return Ok(None);
    }
    Ok(Some(
        ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&value)?,
    ))
}

/// Seal one pinned large-content declaration without touching any live tree:
/// the manifest is already admitted content, so admission proves the grant,
/// the shape, and store residency, then roots the manifest object.
#[allow(clippy::too_many_arguments)]
fn seal_pinned_large_realization(
    declaration: &ryeos_engine::external_content::ExternalContentDeclaration,
    digest: &str,
    manifest: ryeos_state::objects::ExternalLargeContentManifestObject,
    contract: Option<&ryeos_engine::kind_registry::ExecutionExternalContentDecl>,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::CasStore,
    sink: &mut GuardedCasBlobSink<'_>,
    large_total: &mut u64,
) -> anyhow::Result<RealizedExternalContent> {
    let grant = contract
        .and_then(|contract| contract.large_content.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "external content `{}` pins a large-content manifest but its signed kind \
                 declares no `execution.external_content.large_content` grant",
                declaration.id
            )
        })?;
    if declaration.locator.is_some() {
        anyhow::bail!(
            "external content `{}` pins a large-content manifest and must not carry a \
             source locator: large bytes bind from the store, not from a live tree",
            declaration.id
        );
    }
    *large_total = large_total
        .checked_add(manifest.total_bytes)
        .ok_or_else(|| anyhow::anyhow!("large-content realization byte total overflow"))?;
    let ceiling = grant
        .max_total_bytes
        .unwrap_or(ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES);
    if *large_total > ceiling {
        anyhow::bail!(
            "large-content realizations total {large_total} bytes; this kind's grant admits \
             {ceiling}"
        );
    }
    match declaration.kind {
        ExternalContentKind::File => {
            if !manifest.is_file_shaped() {
                anyhow::bail!(
                    "external content `{}` declares kind file but manifest {digest} is not \
                     file-shaped",
                    declaration.id
                );
            }
        }
        ExternalContentKind::Tree => {}
    }
    let store = authority.large_object_store()?;
    for entry in &manifest.entries {
        match store.object_size(&entry.file_sha256)? {
            Some(size) if size == entry.size => {}
            Some(size) => anyhow::bail!(
                "large object {} for `{}` is {size} bytes in the store; the manifest sealed {}",
                entry.file_sha256,
                entry.path,
                entry.size
            ),
            None => anyhow::bail!(
                "large object {} for `{}` is not in the store; ingest it before pinning",
                entry.file_sha256,
                entry.path
            ),
        }
    }
    let stored = sink
        .staged_roots
        .store_object_admitted(guard, cas, &manifest.to_value()?)?;
    if stored != digest {
        anyhow::bail!(
            "large-content manifest {digest} re-stored as {stored}; canonical identity broke"
        );
    }
    Ok(RealizedExternalContent {
        id: declaration.id.clone(),
        kind: declaration.kind,
        mode: declaration.mode,
        manifest_hash: digest.to_string(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
        mount: declaration.mount.clone(),
    })
}

/// Capture the effective declaration list and write its identity-only
/// realization set into the reserved derived slot.
pub(crate) fn capture_external_realizations(
    state: &ryeos_app::state::AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    inherited: Option<&RealizedExternalContentSet>,
) -> anyhow::Result<Option<CapturedExternalRealizations>> {
    let contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.external_content.as_ref());
    let declarer = declaring_authority(resolution, roots)?;
    let Some(declarations) = ryeos_engine::external_content::declarations_from_composed(
        &resolution.composed.composed,
        contract,
        declarer,
    )? else {
        // No authored declaration: a descendant seals its dispatching
        // program's realization; an independent root seals nothing.
        return inherit_external_realizations(state, resolution, inherited);
    };

    let authority = super::pinned_state_authority(state)?;
    let proof_authority = authority.try_clone()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire CAS write permit: {error}"))?;
    let cas = authority.cas_store()?;
    let mut staged_roots = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(&guard, "external-content-realization")?;
    let mut budget = LaunchRealizationBudget::default();
    let mut realized = Vec::with_capacity(declarations.len());
    let mut sink = GuardedCasBlobSink {
        guard: &guard,
        cas: &cas,
        staged_roots: &mut staged_roots,
        stored_blobs: 0,
        reused_blobs: 0,
    };

    let mut large_total = 0u64;
    for declaration in &declarations {
        if declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && let Some(digest) = declaration.digest.as_deref()
            && let Some(large_manifest) = try_load_large_content_manifest(&cas, digest)?
        {
            let realized_entry = seal_pinned_large_realization(
                declaration,
                digest,
                large_manifest,
                contract,
                &authority,
                &guard,
                &cas,
                &mut sink,
                &mut large_total,
            )?;
            realized.push(realized_entry);
            continue;
        }
        let locator = declaration.locator.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "external content `{}` has no source locator and its digest does not \
                 resolve to a large-content manifest; nothing can be captured",
                declaration.id
            )
        })?;
        let base_path = resolve_named_root(engine, roots, &locator.root)?;
        let base = lillux::PinnedDirectory::open(&base_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "external content root `{}` is unavailable",
                locator.root.label()
            )
        })?;
        let capture_policy =
            ExternalCapturePolicy::for_declaration(declaration, state.ignore_matcher.as_ref())?;
        let manifest = match declaration.kind {
            ExternalContentKind::Tree => {
                let declared_root = open_directory_relative(&base, &locator.path)?;
                let manifest = ryeos_engine::external_content::build_manifest(
                    &declared_root,
                    &declaration.exclude,
                    &capture_policy,
                    &mut budget,
                    &mut sink,
                )?;
                declared_root.ensure_path_binding()?;
                manifest
            }
            ExternalContentKind::File => {
                let (parent, name) = open_file_parent(&base, &locator.path)?;
                let file = parent.open_regular(OsStr::new(name), false)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "external content file `{}` is unavailable",
                        locator.path
                    )
                })?;
                let manifest = ryeos_engine::external_content::build_file_manifest(
                    file,
                    &locator.path,
                    &mut budget,
                    &mut sink,
                )?;
                parent.ensure_path_binding()?;
                manifest
            }
        };
        base.ensure_path_binding()?;
        let manifest_hash = sink.staged_roots.store_object_admitted(
            &guard,
            &cas,
            &serde_json::to_value(&manifest)?,
        )?;
        let verified = ryeos_state::VerifiedExternalContentClosure::load(&cas, &manifest_hash)?;
        if verified.manifest() != &manifest {
            anyhow::bail!(
                "stored external content manifest {manifest_hash} differs from its captured value"
            );
        }
        if declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && declaration.digest.as_deref() != Some(manifest_hash.as_str())
        {
            anyhow::bail!(
                "pinned external content `{}` expected {}, observed {manifest_hash}",
                declaration.id,
                declaration.digest.as_deref().unwrap_or("<missing>")
            );
        }
        realized.push(RealizedExternalContent {
            id: declaration.id.clone(),
            kind: declaration.kind,
            mode: declaration.mode,
            manifest_hash,
            entry_count: manifest.entry_count,
            total_bytes: manifest.total_bytes,
            mount: declaration.mount.clone(),
        });
    }

    let realized = RealizedExternalContentSet::new(realized)?;
    resolution.composed.derived.insert(
        ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY.to_string(),
        realized.to_value()?,
    );
    let store = ExternalRealizationStore::new(proof_authority);
    let proof = ryeos_engine::external_realization::prove_external_realizations(
        realized,
        &store,
    )?;
    let (stored_blobs, reused_blobs) = sink.counts();
    tracing::info!(
        kind,
        declaration_count = declarations.len(),
        stored_blobs,
        reused_blobs,
        "captured external content realization"
    );
    drop(sink);
    drop(_permit);
    drop(guard);

    Ok(Some(CapturedExternalRealizations {
        proof,
        store,
        publication: Some(PendingCasPublication {
            authority,
            staged_roots: Some(staged_roots),
        }),
    }))
}

fn declaring_authority<'a>(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    roots: &'a ryeos_engine::item_resolution::ResolutionRoots,
) -> anyhow::Result<DeclaringAuthority<'a>> {
    match resolution.root.source_space {
        ItemSpace::Project => Ok(DeclaringAuthority::Project),
        ItemSpace::Node => Ok(DeclaringAuthority::Node),
        ItemSpace::Bundle => {
            let root = roots
                .ordered
                .iter()
                .filter(|root| root.space == ItemSpace::Bundle)
                .find(|root| resolution.root.source_path.starts_with(&root.ai_root))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "bundle-authored external content has no exact registered bundle root"
                    )
                })?;
            let name = root.label.strip_prefix("bundle:").ok_or_else(|| {
                anyhow::anyhow!("registered bundle root has a non-canonical label")
            })?;
            Ok(DeclaringAuthority::Bundle(name))
        }
    }
}

fn resolve_named_root(
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    root: &ExternalContentRoot,
) -> anyhow::Result<PathBuf> {
    match root {
        ExternalContentRoot::ProjectAi => roots
            .ordered
            .iter()
            .find(|candidate| candidate.space == ItemSpace::Project)
            .map(|candidate| candidate.ai_root.clone())
            .ok_or_else(|| anyhow::anyhow!("project_ai external content root is unavailable")),
        ExternalContentRoot::ProjectFiles => roots
            .ordered
            .iter()
            .find(|candidate| candidate.space == ItemSpace::Project)
            .and_then(|candidate| candidate.ai_root.parent().map(Path::to_path_buf))
            .ok_or_else(|| anyhow::anyhow!("project_files external content root is unavailable")),
        ExternalContentRoot::NodeFiles => engine
            .node_config_root()
            .ok_or_else(|| anyhow::anyhow!("node_files external content root is unavailable")),
        ExternalContentRoot::Bundle(name) => roots
            .ordered
            .iter()
            .find(|candidate| candidate.label == format!("bundle:{name}"))
            .and_then(|candidate| candidate.ai_root.parent().map(Path::to_path_buf))
            .ok_or_else(|| {
                anyhow::anyhow!("bundle:{name} external content root is unavailable")
            }),
    }
}

fn open_directory_relative(
    base: &lillux::PinnedDirectory,
    relative: &str,
) -> anyhow::Result<lillux::PinnedDirectory> {
    let mut current = base.try_clone()?;
    for segment in relative.split('/') {
        current = current
            .open_child_directory(OsStr::new(segment))?
            .ok_or_else(|| {
                anyhow::anyhow!("external content directory `{relative}` is unavailable")
            })?;
    }
    Ok(current)
}

fn open_file_parent<'a>(
    base: &lillux::PinnedDirectory,
    relative: &'a str,
) -> anyhow::Result<(lillux::PinnedDirectory, &'a str)> {
    let (parent, name) = relative.rsplit_once('/').unwrap_or(("", relative));
    let parent = if parent.is_empty() {
        base.try_clone()?
    } else {
        open_directory_relative(base, parent)?
    };
    Ok((parent, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_mount(destination: &str, files: &[(&str, u64)]) -> SealedRealizationMount {
        SealedRealizationMount {
            large: false,
            destination: PathBuf::from(destination),
            kind: ExternalContentKind::Tree,
            files: files
                .iter()
                .map(|(path, size)| ((*path).to_owned(), (format!("hash-{path}"), *size)))
                .collect(),
        }
    }

    #[test]
    fn sealed_path_lookup_answers_like_the_mount_table() {
        let mounts = vec![
            tree_mount(
                "/p/.ai/tools/arc/lib",
                &[("bfs.py", 10), ("deep/util.py", 20)],
            ),
            SealedRealizationMount {
                large: false,
                destination: PathBuf::from("/p/.ai/tools/arc/featurize.py"),
                kind: ExternalContentKind::File,
                files: BTreeMap::from([(
                    ryeos_engine::external_content::FILE_REALIZATION_ENTRY_PATH.to_owned(),
                    ("hash-file".to_owned(), 5),
                )]),
            },
        ];
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/lib/bfs.py")),
            SealedPathLookup::File {
                blob_hash: "hash-bfs.py",
                size: 10,
                large: false
            }
        ));
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/lib/deep/util.py")),
            SealedPathLookup::File {
                blob_hash: "hash-deep/util.py",
                size: 20,
                large: false
            }
        ));
        // A file-kind realization answers for exactly its mount path.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/featurize.py")),
            SealedPathLookup::File {
                blob_hash: "hash-file",
                size: 5,
                large: false
            }
        ));
        // Covered but unsealed paths are absent at execution, never live.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/lib/scratch.py")),
            SealedPathLookup::Absent
        ));
        // Outside every mount, live bytes stay authoritative.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/evidence.py")),
            SealedPathLookup::Uncovered
        ));
        // A lexical prefix that is not a path-component prefix is uncovered.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/libx.py")),
            SealedPathLookup::Uncovered
        ));
    }

    #[test]
    fn nested_mounts_resolve_to_the_most_specific_destination() {
        let mounts = vec![
            tree_mount("/p/.ai/tools/arc", &[("lib/bfs.py", 1), ("run.py", 2)]),
            tree_mount("/p/.ai/tools/arc/lib", &[("other.py", 3)]),
        ];
        // The inner mount owns its subtree: bfs.py is sealed only in the
        // outer manifest, so under the inner mount it does not exist.
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/lib/bfs.py")),
            SealedPathLookup::Absent
        ));
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/lib/other.py")),
            SealedPathLookup::File {
                blob_hash: "hash-other.py",
                size: 3,
                large: false
            }
        ));
        assert!(matches!(
            locate_sealed_path(&mounts, Path::new("/p/.ai/tools/arc/run.py")),
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
            .map(|(path, bytes)| ryeos_state::objects::ExternalContentManifestEntry {
                path: (*path).to_string(),
                kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
                mode: Some(0o644),
                blob_hash: Some(cas.store_blob(bytes).unwrap()),
                size: Some(bytes.len() as u64),
                target: None,
                target_blob: None,
            })
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
        let cache = ExternalMaterializationCache::from_app_root(&app_root);
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
    fn a_file_realization_materializes_its_content_entry() {
        let (dir, cas) = temp_cas();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let cache = ExternalMaterializationCache::from_app_root(&app_root);
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
    fn sweep_reclaims_only_unleased_generations() {
        let (dir, cas) = temp_cas();
        let app_root = dir.path().join("app");
        std::fs::create_dir_all(&app_root).unwrap();
        let cache = ExternalMaterializationCache::from_app_root(&app_root);
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
