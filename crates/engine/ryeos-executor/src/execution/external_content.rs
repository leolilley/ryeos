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

use super::PendingCasPublication;

/// Descriptor-pinned materializations and their exact cache-generation
/// leases. This value must live until the spawned process exits.
pub(crate) struct BoundExternalRealizations {
    mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    _leases: Vec<fs::File>,
}

impl BoundExternalRealizations {
    pub(crate) fn mounts(
        &self,
    ) -> &[ryeos_engine::isolation::IsolationReadOnlyMountAuthority] {
        &self.mounts
    }
}

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
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let cache = ExternalMaterializationCache::from_app_root(&state.config.app_root);
    let mut mounts = Vec::with_capacity(realized.iter().len());
    let mut leases = Vec::with_capacity(realized.iter().len());
    for entry in realized.iter() {
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
    Ok(Some(BoundExternalRealizations {
        mounts,
        _leases: leases,
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

/// Capture the effective declaration list and write its identity-only
/// realization set into the reserved derived slot.
pub(crate) fn capture_external_realizations(
    state: &ryeos_app::state::AppState,
    engine: &ryeos_engine::engine::Engine,
    kind: &str,
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
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
        return Ok(None);
    };

    let authority = super::pinned_state_authority(state)?;
    let proof_authority = authority.try_clone()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .try_acquire()
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

    for declaration in &declarations {
        let base_path = resolve_named_root(engine, roots, &declaration.locator.root)?;
        let base = lillux::PinnedDirectory::open(&base_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "external content root `{}` is unavailable",
                declaration.locator.root.label()
            )
        })?;
        let capture_policy =
            ExternalCapturePolicy::for_declaration(declaration, state.ignore_matcher.as_ref())?;
        let manifest = match declaration.kind {
            ExternalContentKind::Tree => {
                let declared_root = open_directory_relative(&base, &declaration.locator.path)?;
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
                let (parent, name) = open_file_parent(&base, &declaration.locator.path)?;
                let file = parent.open_regular(OsStr::new(name), false)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "external content file `{}` is unavailable",
                        declaration.locator.path
                    )
                })?;
                let manifest = ryeos_engine::external_content::build_file_manifest(
                    file,
                    &declaration.locator.path,
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
