//! `project/apply-snapshot` — apply an AI-only snapshot to a live project.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(unix)]
use std::ffi::{CString, OsStr};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use anyhow::{anyhow, Context, Result};
use lillux::cas::CasStore;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::project_deploy::{self, ProjectDeployContext};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{ItemSource, ProjectSnapshot, SourceManifest};
use ryeos_state::project_sync::ProjectSyncScope;

#[derive(serde::Deserialize)]
#[serde(transparent)]
pub struct ExplicitExpectedDeployedHash(Option<String>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub project_path: String,
    pub snapshot_hash: String,
    /// Exact deployed HEAD observed before the request. Mandatory and nullable.
    pub expected_deployed_hash: ExplicitExpectedDeployedHash,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified().map_err(|e| anyhow!(e))?;

    // Entry log: proves the request reached this handler (vs. being absorbed
    // by a dispatcher 404 / wrong route) and records who is applying what.
    tracing::info!(
        project_path = %req.project_path,
        snapshot_hash = %req.snapshot_hash,
        caller = %ctx.fingerprint,
        "project.apply-snapshot handler entered"
    );

    let project_path = canonical_existing_project_path(&req.project_path)?;
    let canonical_project_path = project_path
        .to_str()
        .ok_or_else(|| anyhow!("canonical project_path is not valid UTF-8"))?
        .to_owned();
    let project_directory = PinnedDirectory::open_canonical(&project_path)?;
    let project_hash = ryeos_state::refs::deployed_project_key(&canonical_project_path);
    let apply_lock = project_apply_lock(&project_hash);
    let _apply_guard = apply_lock.lock_owned().await;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let cas_read_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;

    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint)?;
    let pushed_head = state
        .state_store
        .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
    if pushed_head.as_deref() != Some(req.snapshot_hash.as_str()) {
        anyhow::bail!(
            "project.apply-snapshot refused: caller's staged HEAD for project '{}' is {:?}, not requested snapshot {}",
            canonical_project_path,
            pushed_head,
            req.snapshot_hash
        );
    }

    let snapshot_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        &req.snapshot_hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_obj)?;
    if snapshot.project_sync_scope != ProjectSyncScope::AiOnly {
        anyhow::bail!(
            "project.apply-snapshot only accepts ai_only snapshots in v1 (got {:?})",
            snapshot.project_sync_scope
        );
    }
    if snapshot.user_manifest_hash.is_some() {
        anyhow::bail!("project.apply-snapshot refuses snapshots with user_manifest_hash");
    }

    let manifest_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        &snapshot.project_manifest_hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )?;
    let manifest = SourceManifest::from_value(&manifest_obj)?;
    ryeos_state::project_sync::validate_project_manifest_paths(
        &manifest,
        ProjectSyncScope::AiOnly,
        Some(state.ignore_matcher.as_ref()),
    )?;

    let current_ref = state
        .state_store
        .with_state_db(|db| db.read_deployed_project_ref(&project_hash))?;
    let previous_deployed_hash = current_ref.as_ref().map(|r| r.target_hash.clone());
    let expected_deployed_hash = req.expected_deployed_hash.0.as_deref();
    if previous_deployed_hash.as_deref() != Some(req.snapshot_hash.as_str())
        && previous_deployed_hash.as_deref() != expected_deployed_hash
    {
        anyhow::bail!(
            "deployed project conflict for '{}': expected {:?}, got {:?}",
            canonical_project_path,
            expected_deployed_hash,
            previous_deployed_hash
        );
    }
    // Validate the complete typed snapshot DAG on every apply, including a
    // first deployment. Using the head itself as the search target preserves
    // the traversal while making the ancestry predicate trivially true.
    let ancestry_target = expected_deployed_hash.unwrap_or(&req.snapshot_hash);
    let contains_expected = snapshot_history_contains(&cas, &req.snapshot_hash, ancestry_target)?;
    if let Some(expected) = expected_deployed_hash {
        if !contains_expected {
            anyhow::bail!(
                "project.apply-snapshot ancestry conflict: target {} does not descend from expected deployed snapshot {}",
                req.snapshot_hash,
                expected
            );
        }
    }
    if previous_deployed_hash.as_deref() == Some(req.snapshot_hash.as_str()) {
        return Ok(serde_json::json!({
            "project_path": canonical_project_path,
            "project_hash": project_hash,
            "snapshot_hash": req.snapshot_hash,
            "previous_deployed_hash": previous_deployed_hash,
            "project_sync_scope": snapshot.project_sync_scope,
            "manifest_entries": manifest.item_source_hashes.len(),
            "idempotent": true,
        }));
    }

    // Sweep staging dirs any crashed prior apply left behind. Safe under
    // the per-project apply lock: no other apply for this project can be
    // mid-flight, and live staging dirs only exist within an apply.
    sweep_stale_staging_dirs(&project_directory)?;

    let staging = StagingDirectory::create(&project_directory)?;
    let staging_root = staging.descriptor_path()?;

    if let Err(error) = materialize_manifest_to_staging(&cas, &manifest, staging.directory()) {
        drop(cas_read_guard);
        let _ = staging.cleanup();
        return Err(error);
    }
    // The caller-scoped pushed HEAD remains a GC root while we wait for the
    // scheduler-visible commit window. Reacquire the hierarchy in canonical
    // order immediately before deployed-head publication.
    drop(cas_read_guard);

    let apply_result: Result<ApplyReport> = async {
        let deploy_ctx = ProjectDeployContext {
            project_path: &project_path,
            staging_root: &staging_root,
            manifest: &manifest,
            snapshot_hash: &req.snapshot_hash,
            project_key: &project_hash,
            caller: &ctx,
            state: &state,
        };

        // Serialize only the scheduler-visible window. plan() must see
        // the same schedule state prepare_commit() mutates, so both sit
        // under the gate, together with root swaps, ref advancement,
        // and their rollbacks. Request validation, CAS reads, and
        // staging materialization scale with project size and run
        // before the gate so timer/recovery dispatch is not blocked
        // behind large applies (per-project serialization is handled
        // by `project_apply_lock` above).
        let _scheduler_guard = state.scheduler_runtime_gate.clone().write_owned().await;
        let _cas_publish_guard = authority.acquire_shared_guard()?;
        let _permit = state
            .write_barrier
            .try_acquire()
            .map_err(|e| anyhow!("cannot acquire CAS write permit: {e}"))?;

        let deploy_plan = project_deploy::plan(&deploy_ctx)?;
        let mut root_swap = replace_managed_roots(&project_directory, staging.directory())?;
        let mut deploy_tx = match project_deploy::prepare_commit(&deploy_plan, &deploy_ctx) {
            Ok(tx) => tx,
            Err(err) => {
                root_swap.rollback();
                return Err(err);
            }
        };

        let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
        let ref_result = state.state_store.with_state_db(|db| {
            let current = db
                .read_deployed_project_ref(&project_hash)?
                .map(|head| head.target_hash);
            if current.as_deref() == Some(req.snapshot_hash.as_str()) {
                return Ok(());
            }
            if current.as_deref() != expected_deployed_hash {
                anyhow::bail!(
                    "deployed project changed during apply: expected {:?}, got {:?}",
                    expected_deployed_hash,
                    current
                );
            }
            match expected_deployed_hash {
                Some(expected) => db.advance_deployed_project_ref(
                    &project_hash,
                    &req.snapshot_hash,
                    expected,
                    &signer,
                    &_cas_publish_guard,
                ),
                None => db.write_deployed_project_ref(
                    &project_hash,
                    &req.snapshot_hash,
                    &signer,
                    &_cas_publish_guard,
                ),
            }
        });
        if let Err(err) = ref_result {
            deploy_tx.rollback(&deploy_ctx);
            root_swap.rollback();
            return Err(err);
        }

        let deploy_report = deploy_tx.report.clone();
        deploy_tx.finalize(&deploy_ctx);
        root_swap.finalize();
        let mut report = root_swap.report.clone();
        report.deploy = deploy_report;
        Ok(report)
    }
    .await;
    let cleanup = staging.cleanup();
    if let Err(err) = cleanup {
        tracing::warn!(path = %staging_root.display(), error = %err, "failed to remove project apply staging dir");
    }
    let report = apply_result?;

    Ok(serde_json::json!({
        "project_path": canonical_project_path,
        "project_hash": project_hash,
        "snapshot_hash": req.snapshot_hash,
        "previous_deployed_hash": previous_deployed_hash,
        "project_sync_scope": snapshot.project_sync_scope,
        "manifest_entries": manifest.item_source_hashes.len(),
        "files_materialized": report.files_materialized,
        "roots_replaced": report.roots_replaced,
        "roots_deleted": report.roots_deleted,
        "schedules": {
            "declared": report.deploy.schedules.declared,
            "created": report.deploy.schedules.created,
            "updated": report.deploy.schedules.updated,
            "deleted": report.deploy.schedules.deleted,
        },
    }))
}

/// Per-project apply serialization. A tokio mutex (not std) because
/// holders await the scheduler runtime gate while holding it, and the
/// handler future must stay `Send`. Lock order is always apply lock
/// first, then the runtime gate — never the reverse.
pub(crate) fn project_apply_lock(project_hash: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|e| e.into_inner());
    locks
        .entry(project_hash.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Shared by project handlers: absolute, canonicalized, existing
/// directory — fail loud otherwise.
pub(crate) fn canonical_existing_project_path(project_path: &str) -> Result<PathBuf> {
    let path = Path::new(project_path);
    if !path.is_absolute() {
        anyhow::bail!("project_path '{}' is not absolute", project_path);
    }
    let canonical = path.canonicalize().with_context(|| {
        format!(
            "cannot canonicalize project_path '{}'; ensure it exists",
            project_path
        )
    })?;
    if !canonical.is_dir() {
        anyhow::bail!("project_path '{}' is not a directory", canonical.display());
    }
    Ok(canonical)
}

pub(crate) fn snapshot_history_contains(cas: &CasStore, head: &str, wanted: &str) -> Result<bool> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Visiting,
        Complete,
    }

    let limits = ryeos_state::object_closure::ObjectClosureLimits::default();
    let mut pending = vec![(head.to_string(), false)];
    let mut states = HashMap::<String, VisitState>::new();
    let mut contains = false;
    while let Some((hash, exiting)) = pending.pop() {
        if exiting {
            states.insert(hash, VisitState::Complete);
            continue;
        }
        match states.get(&hash) {
            Some(VisitState::Visiting) => {
                anyhow::bail!("project snapshot history contains a cycle at {hash}")
            }
            Some(VisitState::Complete) => continue,
            None => {}
        }
        if hash == wanted {
            contains = true;
        }
        states.insert(hash.clone(), VisitState::Visiting);
        if states.len() > limits.max_objects {
            anyhow::bail!("project snapshot ancestry exceeds the object traversal limit");
        }
        let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            cas,
            &hash,
            limits.max_object_bytes,
        )?;
        let snapshot = ProjectSnapshot::from_value(&value)?;
        let mut parents = snapshot.parent_hashes;
        if parents.len() > limits.max_links_per_object {
            anyhow::bail!("project snapshot has too many parent links");
        }
        parents.sort();
        pending.push((hash, true));
        for parent in parents.into_iter().rev() {
            pending.push((parent, false));
        }
    }
    Ok(contains)
}

const STAGING_PREFIX: &str = ".ryeos-ai-sync-staging-";

/// An opened directory is the mutation authority. The display path is only
/// diagnostic; every authoritative lookup and mutation is relative to `file`.
#[derive(Debug)]
struct PinnedDirectory {
    #[cfg(unix)]
    file: File,
    display_path: PathBuf,
}

impl PinnedDirectory {
    fn open_canonical(path: &Path) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = path;
            anyhow::bail!(
                "secure descriptor-relative project apply is unavailable on this platform"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .with_context(|| format!("open canonical project directory {}", path.display()))?;
            if !file.metadata()?.file_type().is_dir() {
                anyhow::bail!("project path is not a directory: {}", path.display());
            }
            let pinned = Self {
                file,
                display_path: path.to_path_buf(),
            };
            let resolved = pinned
                .descriptor_path()?
                .canonicalize()
                .with_context(|| format!("resolve pinned project directory {}", path.display()))?;
            if resolved != path {
                anyhow::bail!(
                    "project directory changed while being pinned: expected {}, opened {}",
                    path.display(),
                    resolved.display()
                );
            }
            Ok(pinned)
        }
    }

    #[cfg(unix)]
    fn from_file(file: File, display_path: PathBuf) -> Self {
        Self { file, display_path }
    }

    #[cfg(unix)]
    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            display_path: self.display_path.clone(),
        })
    }

    fn descriptor_path(&self) -> Result<PathBuf> {
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!(
                "secure descriptor-relative project apply requires Linux descriptor paths"
            );
        }
        #[cfg(target_os = "linux")]
        {
            Ok(PathBuf::from(format!(
                "/proc/self/fd/{}",
                self.file.as_raw_fd()
            )))
        }
    }
}

#[derive(Debug)]
struct StagingDirectory {
    parent: PinnedDirectory,
    directory: PinnedDirectory,
    name: OsString,
}

impl StagingDirectory {
    fn create(project: &PinnedDirectory) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = project;
            anyhow::bail!(
                "secure descriptor-relative project apply is unavailable on this platform"
            );
        }
        #[cfg(unix)]
        {
            for _ in 0..128 {
                let name = OsString::from(format!(
                    "{STAGING_PREFIX}{:016x}",
                    rand::Rng::gen::<u64>(&mut rand::thread_rng())
                ));
                let name_c = os_c_string(&name)?;
                let created =
                    unsafe { libc::mkdirat(project.file.as_raw_fd(), name_c.as_ptr(), 0o700) };
                if created != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    return Err(error).context("create pinned project apply staging directory");
                }
                let directory = open_child_directory(&project.file, &name)?
                    .ok_or_else(|| anyhow!("new project apply staging directory disappeared"))?;
                return Ok(Self {
                    parent: project.try_clone()?,
                    directory: PinnedDirectory::from_file(
                        directory,
                        project.display_path.join(&name),
                    ),
                    name,
                });
            }
            anyhow::bail!("could not allocate a unique project apply staging directory")
        }
    }

    fn directory(&self) -> &PinnedDirectory {
        &self.directory
    }

    fn descriptor_path(&self) -> Result<PathBuf> {
        self.directory.descriptor_path()
    }

    fn cleanup(&self) -> Result<()> {
        #[cfg(not(unix))]
        {
            anyhow::bail!(
                "secure descriptor-relative project apply is unavailable on this platform"
            );
        }
        #[cfg(unix)]
        {
            remove_entry_at(&self.parent.file, &self.name).with_context(|| {
                format!(
                    "remove staging directory {}",
                    self.directory.display_path.display()
                )
            })
        }
    }
}

#[cfg(unix)]
fn os_c_string(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).context("filesystem name contains NUL")
}

#[cfg(unix)]
fn directory_names(directory: &File) -> Result<Vec<OsString>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = directory;
        anyhow::bail!("secure directory enumeration requires Linux descriptor paths");
    }
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let mut names = fs::read_dir(&path)
            .with_context(|| format!("enumerate pinned directory {}", path.display()))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok(names)
    }
}

#[cfg(unix)]
fn open_child_directory(parent: &File, name: &OsStr) -> Result<Option<File>> {
    let name_c = os_c_string(name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("open pinned child directory '{}'", name.to_string_lossy()));
    }
    Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
}

#[cfg(unix)]
fn open_relative_directory(
    root: &PinnedDirectory,
    relative: &Path,
    create: bool,
) -> Result<Option<File>> {
    use std::path::Component;

    let mut directory = root.file.try_clone()?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            anyhow::bail!(
                "descriptor-relative path contains an unsafe component: {}",
                relative.display()
            );
        };
        match open_child_directory(&directory, name)? {
            Some(child) => directory = child,
            None if !create => return Ok(None),
            None => {
                let name_c = os_c_string(name)?;
                if unsafe { libc::mkdirat(directory.as_raw_fd(), name_c.as_ptr(), 0o755) } != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error).with_context(|| {
                            format!(
                                "create pinned directory component '{}'",
                                name.to_string_lossy()
                            )
                        });
                    }
                }
                directory = open_child_directory(&directory, name)?
                    .ok_or_else(|| anyhow!("created directory component disappeared"))?;
            }
        }
    }
    Ok(Some(directory))
}

#[cfg(unix)]
fn open_relative_parent(
    root: &PinnedDirectory,
    relative: &Path,
    create: bool,
) -> Result<Option<(File, OsString)>> {
    use std::path::Component;

    let name = match relative.file_name() {
        Some(name) => name.to_os_string(),
        None => anyhow::bail!("descriptor-relative path has no final name"),
    };
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        anyhow::bail!(
            "descriptor-relative path contains an unsafe component: {}",
            relative.display()
        );
    }
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    Ok(open_relative_directory(root, parent, create)?.map(|directory| (directory, name)))
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    Directory,
    Regular,
    Symlink,
    Other,
}

#[cfg(unix)]
fn entry_kind_at(parent: &File, name: &OsStr) -> Result<Option<EntryKind>> {
    let name_c = os_c_string(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("inspect descriptor-relative filesystem entry");
    }
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    Ok(Some(if file_type == libc::S_IFDIR {
        EntryKind::Directory
    } else if file_type == libc::S_IFREG {
        EntryKind::Regular
    } else if file_type == libc::S_IFLNK {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }))
}

#[cfg(unix)]
fn remove_entry_at(parent: &File, name: &OsStr) -> Result<()> {
    let Some(kind) = entry_kind_at(parent, name)? else {
        return Ok(());
    };
    let name_c = os_c_string(name)?;
    if kind == EntryKind::Directory {
        let directory = open_child_directory(parent, name)?.ok_or_else(|| {
            anyhow!("directory entry disappeared during descriptor-relative removal")
        })?;
        for child in directory_names(&directory)? {
            remove_entry_at(&directory, &child)?;
        }
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("remove descriptor-relative directory");
        }
    } else if unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("remove descriptor-relative filesystem entry");
    }
    Ok(())
}

fn sweep_stale_staging_dirs(project: &PinnedDirectory) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = project;
        anyhow::bail!("secure descriptor-relative project apply is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        for name in directory_names(&project.file)? {
            if name
                .to_str()
                .is_some_and(|name| name.starts_with(STAGING_PREFIX))
            {
                if let Err(error) = remove_entry_at(&project.file, &name) {
                    tracing::warn!(
                        path = %project.display_path.join(&name).display(),
                        %error,
                        "failed to remove stale staging dir"
                    );
                }
            }
        }
        Ok(())
    }
}

fn materialize_manifest_to_staging(
    cas: &CasStore,
    manifest: &SourceManifest,
    staging_root: &PinnedDirectory,
) -> Result<usize> {
    let mut count = 0usize;
    for (rel_path, item_hash) in &manifest.item_source_hashes {
        // Floors (secrets/node-owned) are enforced regardless; ignore was
        // already validated against the live matcher by the caller.
        ryeos_state::project_sync::validate_project_manifest_path(
            rel_path,
            ProjectSyncScope::AiOnly,
            None,
        )?;
        let limits = ryeos_state::object_closure::ObjectClosureLimits::default();
        let item_obj = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            cas,
            item_hash,
            limits.max_object_bytes,
        )?;
        let item = ItemSource::from_value(&item_obj)?;
        if item.item_ref != *rel_path {
            anyhow::bail!(
                "manifest path '{}' does not match item_source identity '{}'",
                rel_path,
                item.item_ref
            );
        }
        let blob = ryeos_state::object_closure::load_exact_cas_blob_with_cas(
            cas,
            &item.content_blob_hash,
            limits.max_blob_bytes,
        )?;
        if lillux::cas::sha256_hex(&blob) != item.integrity {
            anyhow::bail!("integrity mismatch for '{}'", rel_path);
        }

        write_staged_regular_file(staging_root, Path::new(rel_path), &blob, item.mode)?;
        count += 1;
    }
    Ok(count)
}

fn write_staged_regular_file(
    staging_root: &PinnedDirectory,
    relative: &Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (staging_root, relative, bytes, mode);
        anyhow::bail!("secure descriptor-relative project apply is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        let (parent, name) = open_relative_parent(staging_root, relative, true)?
            .ok_or_else(|| anyhow!("staging parent could not be created"))?;
        let name_c = os_c_string(&name)?;
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("create staged regular file {}", relative.display()));
        }
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let write_result = (|| -> Result<()> {
            file.write_all(bytes)?;
            let mode = mode.unwrap_or(0o644) & 0o777;
            if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
                return Err(std::io::Error::last_os_error()).context("set staged file mode");
            }
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), 0) };
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = mode.unwrap_or(0o644) & 0o777;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
struct ApplyReport {
    files_materialized: usize,
    roots_replaced: usize,
    roots_deleted: usize,
    deploy: project_deploy::ProjectDeployReport,
}

#[derive(Debug)]
struct RootSwap {
    #[cfg(unix)]
    parent: File,
    dest_name: OsString,
    backup_name: Option<OsString>,
    display_dest: PathBuf,
    installed: bool,
}

#[derive(Debug)]
struct PreparedRootSwap {
    swaps: Vec<RootSwap>,
    report: ApplyReport,
    finalized: bool,
}

impl PreparedRootSwap {
    fn rollback(&mut self) {
        rollback_swaps(&self.swaps);
        self.finalized = true;
    }

    fn finalize(&mut self) {
        for swap in &self.swaps {
            #[cfg(unix)]
            if let Some(backup) = &swap.backup_name {
                if let Err(error) = remove_entry_at(&swap.parent, backup) {
                    tracing::warn!(
                        path = %swap.display_dest.display(),
                        %error,
                        "failed to remove project apply backup"
                    );
                }
            }
        }
        self.finalized = true;
    }
}

impl Drop for PreparedRootSwap {
    fn drop(&mut self) {
        if !self.finalized {
            rollback_swaps(&self.swaps);
        }
    }
}

fn replace_managed_roots(
    project_root: &PinnedDirectory,
    staging_root: &PinnedDirectory,
) -> Result<PreparedRootSwap> {
    #[cfg(not(unix))]
    {
        let _ = (project_root, staging_root);
        anyhow::bail!("secure descriptor-relative project apply is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        let mut swaps: Vec<RootSwap> = Vec::new();
        let mut report = ApplyReport {
            files_materialized: count_files(staging_root)?,
            ..ApplyReport::default()
        };

        let result = (|| -> Result<()> {
            for rel_root in ryeos_state::project_sync::materialized_project_ai_surface_roots() {
                let relative = Path::new(rel_root);
                let (live_parent, dest_name) = open_relative_parent(project_root, relative, true)?
                    .ok_or_else(|| anyhow!("live managed-root parent could not be created"))?;
                let staged_parent = open_relative_parent(staging_root, relative, false)?;
                let staged_exists = match staged_parent.as_ref() {
                    Some((parent, name)) => match entry_kind_at(parent, name)? {
                        Some(EntryKind::Directory) => true,
                        Some(_) => anyhow::bail!(
                            "staged managed root '{}' is not a real directory",
                            rel_root
                        ),
                        None => false,
                    },
                    None => false,
                };
                let dest_kind = entry_kind_at(&live_parent, &dest_name)?;
                if matches!(dest_kind, Some(EntryKind::Symlink)) {
                    anyhow::bail!(
                        "managed root path '{}' is a symlink; refusing apply",
                        rel_root
                    );
                }
                if matches!(dest_kind, Some(EntryKind::Regular | EntryKind::Other)) {
                    anyhow::bail!(
                        "managed root path '{}' is not a directory; refusing apply",
                        rel_root
                    );
                }
                let rollback_parent = live_parent.try_clone()?;
                let backup_name = if dest_kind.is_some() {
                    Some(backup_entry(&live_parent, &dest_name)?)
                } else {
                    None
                };
                let display_dest = project_root.display_path.join(relative);
                let swap_idx = swaps.len();
                swaps.push(RootSwap {
                    parent: rollback_parent,
                    dest_name: dest_name.clone(),
                    backup_name,
                    display_dest: display_dest.clone(),
                    installed: false,
                });

                if staged_exists {
                    let (staged_parent, staged_name) = staged_parent
                        .as_ref()
                        .expect("staged_exists requires a staged parent");
                    rename_noreplace(staged_parent, staged_name, &live_parent, &dest_name)
                        .with_context(|| {
                            format!("install managed root {}", display_dest.display())
                        })?;
                    report.roots_replaced += 1;
                    swaps[swap_idx].installed = true;
                } else {
                    report.roots_deleted += usize::from(swaps[swap_idx].backup_name.is_some());
                }
            }
            Ok(())
        })();

        if let Err(err) = result {
            rollback_swaps(&swaps);
            return Err(err);
        }

        Ok(PreparedRootSwap {
            swaps,
            report,
            finalized: false,
        })
    }
}

fn rollback_swaps(swaps: &[RootSwap]) {
    for swap in swaps.iter().rev() {
        #[cfg(unix)]
        {
            if swap.installed {
                if let Err(error) = remove_entry_at(&swap.parent, &swap.dest_name) {
                    tracing::error!(
                        path = %swap.display_dest.display(),
                        %error,
                        "failed to remove installed managed root during rollback"
                    );
                }
            }
            if let Some(backup) = &swap.backup_name {
                if let Err(error) =
                    rename_noreplace(&swap.parent, backup, &swap.parent, &swap.dest_name)
                {
                    tracing::error!(
                        path = %swap.display_dest.display(),
                        %error,
                        "failed to restore managed root backup during rollback"
                    );
                }
            }
        }
    }
}

#[cfg(unix)]
fn backup_entry(parent: &File, name: &OsStr) -> Result<OsString> {
    let stem = name
        .to_str()
        .ok_or_else(|| anyhow!("managed-root name is not valid UTF-8"))?;
    for _ in 0..128 {
        let backup = OsString::from(format!(
            ".{stem}.ryeos-backup-{:016x}",
            rand::Rng::gen::<u64>(&mut rand::thread_rng())
        ));
        match rename_noreplace(parent, name, parent, &backup) {
            Ok(()) => return Ok(backup),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("backup managed root"),
        }
    }
    anyhow::bail!("could not allocate a unique managed-root backup name")
}

#[cfg(unix)]
fn rename_noreplace(
    source_parent: &File,
    source_name: &OsStr,
    target_parent: &File,
    target_name: &OsStr,
) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source_parent, source_name, target_parent, target_name);
        anyhow::bail!("secure no-replace rename requires Linux renameat2");
    }
    #[cfg(target_os = "linux")]
    {
        let source_name = os_c_string(source_name)?;
        let target_name = os_c_string(target_name)?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                source_parent.as_raw_fd(),
                source_name.as_ptr(),
                target_parent.as_raw_fd(),
                target_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("descriptor-relative rename");
        }
        Ok(())
    }
}

fn count_files(root: &PinnedDirectory) -> Result<usize> {
    #[cfg(not(unix))]
    {
        let _ = root;
        anyhow::bail!("secure descriptor-relative project apply is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        fn count(directory: &File) -> Result<usize> {
            let mut total = 0usize;
            for name in directory_names(directory)? {
                match entry_kind_at(directory, &name)? {
                    Some(EntryKind::Directory) => {
                        let child = open_child_directory(directory, &name)?.ok_or_else(|| {
                            anyhow!("staging directory disappeared while counting")
                        })?;
                        total += count(&child)?;
                    }
                    Some(EntryKind::Regular) => total += 1,
                    Some(EntryKind::Other) => {
                        anyhow::bail!("staging tree contains a special filesystem entry")
                    }
                    Some(EntryKind::Symlink) => {
                        anyhow::bail!("staging tree contains a symlink")
                    }
                    None => anyhow::bail!("staging entry disappeared while counting"),
                }
            }
            Ok(total)
        }
        count(&root.file)
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:project/apply-snapshot",
    endpoint: "project.apply-snapshot",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.project/apply-snapshot"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn replace_managed_roots_deletes_missing_and_preserves_app_files() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/tools")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/config/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/node/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join(".ai/directives/old.md"), "old").unwrap();
        std::fs::write(project.path().join(".ai/tools/old.sh"), "old").unwrap();
        std::fs::write(project.path().join(".ai/config/schedules/old.yaml"), "old").unwrap();
        std::fs::write(
            project.path().join(".ai/node/schedules/runtime.yaml"),
            "runtime",
        )
        .unwrap();
        std::fs::write(project.path().join("src/index.ts"), "app").unwrap();

        let staging = TempDir::new().unwrap();
        std::fs::create_dir_all(staging.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(staging.path().join(".ai/config/schedules")).unwrap();
        std::fs::write(staging.path().join(".ai/directives/new.md"), "new").unwrap();
        std::fs::write(staging.path().join(".ai/config/schedules/new.yaml"), "new").unwrap();

        let project_root = project.path().canonicalize().unwrap();
        let staging_root = staging.path().canonicalize().unwrap();
        let project_directory = PinnedDirectory::open_canonical(&project_root).unwrap();
        let staging_directory = PinnedDirectory::open_canonical(&staging_root).unwrap();
        let mut prepared = replace_managed_roots(&project_directory, &staging_directory).unwrap();
        prepared.finalize();
        let report = prepared.report.clone();
        assert!(report.roots_replaced >= 1);
        assert!(report.roots_deleted >= 1);
        assert!(project.path().join(".ai/directives/new.md").exists());
        assert!(!project.path().join(".ai/directives/old.md").exists());
        assert!(!project.path().join(".ai/tools").exists());
        assert!(project
            .path()
            .join(".ai/config/schedules/new.yaml")
            .exists());
        assert!(!project
            .path()
            .join(".ai/config/schedules/old.yaml")
            .exists());
        assert_eq!(
            std::fs::read_to_string(project.path().join(".ai/node/schedules/runtime.yaml"))
                .unwrap(),
            "runtime"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("src/index.ts")).unwrap(),
            "app"
        );
    }

    #[test]
    fn replace_managed_roots_rejects_symlinked_live_root() {
        #[cfg(not(unix))]
        {
            return;
        }

        let project = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai")).unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join(".ai/directives")).unwrap();

        let staging = TempDir::new().unwrap();
        std::fs::create_dir_all(staging.path().join(".ai/directives")).unwrap();
        std::fs::write(staging.path().join(".ai/directives/new.md"), "new").unwrap();

        let project_root = project.path().canonicalize().unwrap();
        let staging_root = staging.path().canonicalize().unwrap();
        let project_directory = PinnedDirectory::open_canonical(&project_root).unwrap();
        let staging_directory = PinnedDirectory::open_canonical(&staging_root).unwrap();
        let err = replace_managed_roots(&project_directory, &staging_directory)
            .expect_err("symlink root must be rejected");
        assert!(format!("{err:#}").contains("symlink"));
    }

    #[test]
    fn materialize_manifest_restores_executable_mode() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        let bytes = b"#!/bin/sh\n";
        let blob_hash = cas.store_blob(bytes).unwrap();
        let item = ItemSource {
            item_ref: ".ai/tools/run.sh".into(),
            content_blob_hash: blob_hash,
            integrity: lillux::cas::sha256_hex(bytes),
            signature_info: None,
            mode: Some(0o755),
        };
        let item_hash = cas.store_object(&item.to_value()).unwrap();
        let mut map = HashMap::new();
        map.insert(".ai/tools/run.sh".to_string(), item_hash);
        let manifest = SourceManifest {
            item_source_hashes: map,
        };
        let staging = TempDir::new().unwrap();

        let staging_root = staging.path().canonicalize().unwrap();
        let staging_directory = PinnedDirectory::open_canonical(&staging_root).unwrap();
        materialize_manifest_to_staging(&cas, &manifest, &staging_directory).unwrap();
        let mode = std::fs::metadata(staging.path().join(".ai/tools/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn apply_mode_clamps_special_bits() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "content").unwrap();

        apply_mode(&path, Some(0o4755)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn preflight_schedule_declarations_accepts_valid_project_intent() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("snap-track.yaml"),
            format!(
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: snap-track-discover-feed-scrape
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {{}}
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    project_root: {}
    params:
      country: US
"#,
                project.path().display()
            ),
        )
        .unwrap();

        let count = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn preflight_schedule_declarations_rejects_duplicate_ids() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        for file in ["a.yaml", "b.yaml"] {
            std::fs::write(
                schedules.join(file),
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: duplicate-schedule
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {{}}
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    params: {{}}
"#,
            )
            .unwrap();
        }

        let err = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .expect_err("duplicate schedule ids must fail preflight");
        assert!(format!("{err:#}").contains("duplicate schedule_id"));
    }

    #[test]
    fn preflight_schedule_declarations_rejects_node_owned_fields() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("bad.yaml"),
            r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: bad-schedule
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {{}}
    schedule_type: cron
    expression: "0 */15 * * * *"
    execution:
      requester_fingerprint: fp:test
"#,
        )
        .unwrap();

        let err = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .expect_err("node-owned execution field must fail preflight");
        assert!(format!("{err:#}").contains("unknown field `execution`"));
    }

    #[test]
    fn preflight_schedule_declarations_rejects_other_project_root() {
        let project = TempDir::new().unwrap();
        let other_project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("bad-root.yaml"),
            format!(
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: wrong-root
    item_ref: graph:snap-track/discover_feed_scrape
    ref_bindings: {{}}
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    params: {{}}
    project_root: {}
"#,
                other_project.path().display()
            ),
        )
        .unwrap();

        let err = crate::project_deploy::schedules::validate_declarations_for_test(
            staging.path(),
            project.path(),
        )
        .expect_err("foreign project root must fail preflight");
        assert!(format!("{err:#}").contains("cannot target another project"));
    }
}
