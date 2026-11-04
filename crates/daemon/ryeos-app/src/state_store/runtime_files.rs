//! Thread runtime files, runtime snapshot staging and abandoned-package discard.
use super::*;

pub(super) fn discard_abandoned_external_guest_packages(
    runtime_state: &lillux::PinnedDirectory,
) -> Result<()> {
    use std::ffi::OsStr;

    let Some(parent) = runtime_state.open_child_directory(OsStr::new("external-guest-packages"))?
    else {
        return Ok(());
    };
    parent.require_owner_private_directory()?;
    const RECOVERY_BATCH_SIZE: usize = 1024;
    const MAX_RECOVERY_BATCHES: usize = 16;
    for _ in 0..MAX_RECOVERY_BATCHES {
        let names = parent.entry_names_bounded(RECOVERY_BATCH_SIZE)?;
        if names.is_empty() {
            return Ok(());
        }
        for name in names {
            let owned_name = name.to_str().context("non-UTF8 guest package generation")?;
            let mut components = owned_name.split('.');
            anyhow::ensure!(
                components.next() == Some("guest-package")
                    && components
                        .next()
                        .and_then(|value| value.parse::<u32>().ok())
                        .is_some()
                    && components
                        .next()
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some()
                    && components.next().is_none(),
                "unexpected entry in external guest package namespace"
            );
            let child = parent
                .open_child_directory(&name)?
                .context("abandoned guest package is not a directory")?;
            child.require_owner_private_directory()?;
            anyhow::ensure!(
                child
                    .entry_names_bounded(2)?
                    .iter()
                    .all(|entry| entry == OsStr::new("payload")),
                "abandoned guest package has an unexpected entry"
            );
            child.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(1, 1))?;
            anyhow::ensure!(
                parent
                    .remove_empty_child_if_same(&name, &child)
                    .map(|receipt| receipt.is_some())?,
                "abandoned guest package changed during recovery"
            );
        }
    }
    anyhow::ensure!(
        parent.entry_names_bounded(1)?.is_empty(),
        "abandoned external guest package recovery made bounded progress; retry startup to finish"
    );
    Ok(())
}

/// The snapshot producer's private CAS realization is disposable. Startup
/// removes only this dedicated namespace after acquiring the exclusive
/// runtime-state lock; no retained product or provider journal is touched.
pub(super) fn discard_abandoned_runtime_snapshot_staging(
    runtime_state: &lillux::PinnedDirectory,
) -> Result<()> {
    use std::ffi::OsStr;

    let Some(parent) =
        runtime_state.open_child_directory(OsStr::new("runtime-snapshot-staging"))?
    else {
        return Ok(());
    };
    parent.require_owner_private_directory()?;
    for _ in 0..16 {
        let names = parent.entry_names_bounded(1024)?;
        if names.is_empty() {
            return Ok(());
        }
        for name in names {
            let value = name.to_str().context("non-UTF8 runtime snapshot stage")?;
            let id = value
                .strip_prefix("snapshot-stage-")
                .context("unexpected entry in runtime snapshot staging namespace")?;
            let parsed =
                uuid::Uuid::parse_str(id).context("invalid runtime snapshot staging generation")?;
            anyhow::ensure!(
                parsed.to_string() == id,
                "non-canonical runtime snapshot stage"
            );
            let child = parent
                .open_child_directory(&name)?
                .context("runtime snapshot stage is not a directory")?;
            child.require_owner_private_directory()?;
            child.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
                1024, 4,
            ))?;
            anyhow::ensure!(
                parent
                    .remove_empty_child_if_same(&name, &child)
                    .map(|receipt| receipt.is_some())?,
                "runtime snapshot stage changed during startup recovery"
            );
        }
    }
    anyhow::ensure!(
        parent.entry_names_bounded(1)?.is_empty(),
        "runtime snapshot staging recovery made bounded progress; retry startup"
    );
    Ok(())
}

/// One process-local, private materialization destination. Its product bytes
/// are retained by CAS, not by this directory. Drop is best-effort; startup
/// recovery is authoritative for a crash or failed cleanup.
pub struct RuntimeSnapshotStageLease {
    pub(super) parent: lillux::PinnedDirectory,
    pub(super) stage: lillux::PinnedDirectory,
    pub(super) name: std::ffi::OsString,
}

impl RuntimeSnapshotStageLease {
    pub fn root(&self) -> &lillux::PinnedDirectory {
        &self.stage
    }
    pub fn product_name(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new("product")
    }
}

impl Drop for RuntimeSnapshotStageLease {
    fn drop(&mut self) {
        let cleanup = self
            .stage
            .remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(1024, 4))
            .and_then(|_| {
                self.parent
                    .remove_empty_child_if_same(&self.name, &self.stage)
                    .map(|receipt| receipt.is_some())
                    .and_then(|removed| {
                        anyhow::ensure!(removed, "runtime snapshot stage changed before cleanup");
                        Ok(())
                    })
            });
        if let Err(error) = cleanup {
            tracing::warn!(error = %error, "runtime snapshot stage requires startup cleanup");
        }
    }
}

pub(super) fn validate_thread_id_path_component(thread_id: &str) -> Result<()> {
    let mut components = Path::new(thread_id).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(component)), None)
            if !component.is_empty() && component == std::ffi::OsStr::new(thread_id) =>
        {
            Ok(())
        }
        _ => bail!("invalid authoritative thread ID for runtime cleanup: {thread_id}"),
    }
}

pub(super) struct ThreadRuntimeRemoval {
    pub(super) threads_root: lillux::PinnedDirectory,
    pub(super) directory: lillux::PinnedDirectory,
    pub(super) name: std::ffi::OsString,
}

pub(super) struct ThreadRuntimeAuthority {
    pub(super) app_root: lillux::PinnedDirectory,
    pub(super) runtime_state: lillux::PinnedDirectory,
    pub(super) threads_root: Option<lillux::PinnedDirectory>,
}

impl ThreadRuntimeAuthority {
    pub(super) fn capture(
        app_root: &Path,
        runtime_state: &lillux::PinnedDirectory,
        create_threads_root: bool,
    ) -> Result<Self> {
        let app_root = lillux::PinnedDirectory::open(app_root)?.ok_or_else(|| {
            anyhow!(
                "app runtime root is absent while capturing cleanup authority: {}",
                app_root.display()
            )
        })?;
        let ai_root = app_root
            .open_child_directory(std::ffi::OsStr::new(ryeos_engine::AI_DIR))?
            .ok_or_else(|| anyhow!("app root has no .ai directory"))?;
        let configured_runtime_state = ai_root
            .open_child_directory(std::ffi::OsStr::new("state"))?
            .ok_or_else(|| anyhow!("app root has no .ai/state directory"))?;
        if !configured_runtime_state.is_same_directory(runtime_state)? {
            bail!(
                "runtime state directory does not belong to captured app root: app_root={}, runtime_state={}",
                app_root.path().display(),
                runtime_state.path().display()
            );
        }
        let threads_name = std::ffi::OsStr::new("threads");
        let threads_root = if create_threads_root {
            Some(app_root.open_or_create_child(threads_name, 0o700)?)
        } else {
            app_root.open_child_directory(threads_name)?
        };
        if threads_root.is_none() && app_root.open_regular(threads_name, false)?.is_some() {
            bail!("thread state root is not a directory");
        }
        Ok(Self {
            app_root,
            runtime_state: runtime_state.try_clone()?,
            threads_root,
        })
    }

    /// Confirm the public namespace still names the captured inodes. The
    /// reopened path is diagnostic only; every destructive operation below is
    /// rooted in the retained descriptors.
    pub(super) fn ensure_current_binding(&self) -> Result<()> {
        let current_app =
            lillux::PinnedDirectory::open(self.app_root.path())?.ok_or_else(|| {
                anyhow!(
                    "captured app runtime root disappeared: {}",
                    self.app_root.path().display()
                )
            })?;
        if !self.app_root.is_same_directory(&current_app)? {
            bail!(
                "app runtime root changed after cleanup authority was captured: {}",
                self.app_root.path().display()
            );
        }

        let current_threads = self
            .app_root
            .open_child_directory(std::ffi::OsStr::new("threads"))?;
        let bindings = (&self.threads_root, current_threads);
        match bindings {
            (Some(expected), Some(current)) if expected.is_same_directory(&current)? => Ok(()),
            (None, None) => Ok(()),
            _ => bail!(
                "thread runtime root changed after cleanup authority was captured: {}",
                self.app_root.path().join("threads").display()
            ),
        }
    }
}

pub(super) fn count_runtime_tree_entries(directory: &lillux::PinnedDirectory) -> Result<usize> {
    let mut count = 1usize;
    for name in directory.entry_names()? {
        let child_count = match directory.open_entry(&name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                count_runtime_tree_entries(&child)?
            }
            Some(lillux::PinnedDirectoryEntry::Regular(_)) => 1,
            // An entry removed after enumeration is already absent. Links and
            // special files fail in the no-follow mixed-entry open above.
            None => continue,
        };
        count = count
            .checked_add(child_count)
            .ok_or_else(|| anyhow!("runtime file count overflow"))?;
    }
    Ok(count)
}

/// Resolve exactly the per-thread daemon state directories named by signed
/// chain truth beneath the startup-captured thread-root inode. This never
/// reopens a pathname as authority, never walks the global thread directory,
/// and rejects links or special entries.
pub(super) fn inspect_thread_runtime_files(
    authority: &ThreadRuntimeAuthority,
    thread_ids: &[String],
) -> Result<Vec<ThreadRuntimeRemoval>> {
    authority.ensure_current_binding()?;
    let Some(threads_root) = authority.threads_root.as_ref() else {
        return Ok(Vec::new());
    };

    let mut paths = Vec::new();
    for thread_id in thread_ids {
        validate_thread_id_path_component(thread_id)?;
        let name = std::ffi::OsString::from(thread_id);
        let Some(directory) = threads_root.open_child_directory(&name)? else {
            if threads_root.open_regular(&name, false)?.is_some() {
                bail!("thread runtime cleanup target is not a directory: {thread_id}");
            }
            continue;
        };
        // Inspect the complete tree before the signed head is unlinked. This
        // fails closed on links/special entries while retaining exact handles
        // for the later post-boundary cleanup.
        let _entries = count_runtime_tree_entries(&directory)?;
        paths.push(ThreadRuntimeRemoval {
            threads_root: threads_root.try_clone()?,
            directory,
            name,
        });
    }
    Ok(paths)
}

pub(super) fn delete_runtime_tree_contents(directory: &lillux::PinnedDirectory) -> Result<usize> {
    let mut deleted = 0usize;
    for name in directory.entry_names()? {
        let entry = directory.open_entry(&name, false)?;
        match entry {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                deleted = deleted
                    .checked_add(delete_runtime_tree_contents(&child)?)
                    .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
                if !directory
                    .remove_empty_child_if_same(&name, &child)
                    .map(|receipt| receipt.is_some())?
                {
                    bail!(
                        "thread runtime directory changed during cleanup: {}",
                        child.path().display()
                    );
                }
                deleted = deleted
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
            }
            Some(lillux::PinnedDirectoryEntry::Regular(file)) => {
                directory.remove_if_same(&name, &file)?;
                deleted = deleted
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
            }
            None => {}
        }
    }
    Ok(deleted)
}

pub(super) fn delete_thread_runtime_files(paths: &[ThreadRuntimeRemoval]) -> Result<usize> {
    let mut deleted = 0usize;
    for target in paths {
        deleted = deleted
            .checked_add(delete_runtime_tree_contents(&target.directory)?)
            .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
        if !target
            .threads_root
            .remove_empty_child_if_same(&target.name, &target.directory)
            .map(|receipt| receipt.is_some())?
        {
            bail!(
                "thread runtime cleanup target changed during deletion: {}",
                target.directory.path().display()
            );
        }
        deleted = deleted
            .checked_add(1)
            .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
    }
    Ok(deleted)
}

/// Inspect or clear the complete per-thread runtime directory for the explicit
/// offline all-history GC path. The `threads/` root itself remains as the
/// current empty runtime namespace.
pub(crate) fn discard_all_thread_runtime_files(
    app_root: &Path,
    runtime_state: &lillux::PinnedDirectory,
    dry_run: bool,
) -> Result<usize> {
    let authority = ThreadRuntimeAuthority::capture(app_root, runtime_state, false)?;
    authority.ensure_current_binding()?;
    let Some(threads_root) = authority.threads_root.as_ref() else {
        return Ok(0);
    };
    if dry_run {
        return count_runtime_tree_entries(threads_root).map(|count| count.saturating_sub(1));
    }
    delete_runtime_tree_contents(threads_root)
}

impl StateStore {
    pub fn begin_runtime_snapshot_stage(&self) -> Result<RuntimeSnapshotStageLease> {
        use std::ffi::OsStr;
        let runtime = &self.thread_runtime_authority()?.runtime_state;
        let parent = runtime.open_or_create_child(OsStr::new("runtime-snapshot-staging"), 0o700)?;
        parent.require_owner_private_directory()?;
        let name = std::ffi::OsString::from(format!("snapshot-stage-{}", uuid::Uuid::new_v4()));
        let stage = parent.create_child(&name, 0o700)?;
        stage.require_owner_private_directory()?;
        Ok(RuntimeSnapshotStageLease {
            parent,
            stage,
            name,
        })
    }

    pub(super) fn thread_runtime_authority(&self) -> Result<&ThreadRuntimeAuthority> {
        self.thread_runtime_authority
            .as_ref()
            .ok_or_else(|| anyhow!("this StateStore has no destructive thread-runtime authority"))
    }

    /// Production startup under its actual prepaid factory and node exclusion.
    /// The StateStore opening owner admits Runtime in the original namespace.
    /// # Safety
    /// Supply the real original node issuer as lease authority for this same
    /// admitted factory and process, with its authentic funding, captured
    /// service/cleanup clocks and retained exclusions. A trait implementation
    /// or decoded report cannot establish these facts. Keep original custody
    /// through failed startup, shutdown and actual resource settlement.
    pub unsafe fn new_live_with_head_trust_and_recovery_observer(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        bundle_admission_trust: BundleAdmissionTrust,
        recovery_observer: Arc<dyn ryeos_state::ProjectionRecoveryObserver>,
        exclusion: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Self> {
        let original = live_native::LiveStateOpen::capture(
            &app_root,
            &runtime_state_dir,
            &runtime_db_path,
            exclusion,
            factory,
            lease_authority,
            Arc::clone(&signer),
            Arc::clone(&head_trust),
            deadline,
        )?;
        Self::open(
            app_root,
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier,
            head_trust,
            bundle_admission_trust,
            Some(recovery_observer),
            original,
        )
    }

    /// Offline projection-rebuild control store on an in-process native host,
    /// in the stopped-node maintenance shape: the caller holds the runtime
    /// namespace lock. The authority roots and recovery journal open with a
    /// transient projection on a host ticket, and the established Runtime
    /// store opens existing-only on another, for liveness. Nothing is
    /// selected, replayed or rebuilt by opening; every unrelated mutation API
    /// stays fail-closed. Close it with
    /// [`StateStore::close_projection_rebuild_native`].
    /// # Safety
    /// The caller holds the exclusive node StateLock and `namespace`, the
    /// exclusive lock of `runtime_state_directory`, for the Store's whole
    /// life; `factory` and `destination` come from one in-process host the
    /// caller funded under that exclusion, and `destination` revalidates it on
    /// every native boundary. `retain` records terminals in that host's census.
    pub unsafe fn new_for_projection_rebuild_native(
        app_root: PathBuf,
        runtime_state_directory: lillux::PinnedDirectory,
        namespace: lillux::PinnedDirectoryLock,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        destination: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        deadline: lillux::time::MonotonicDeadline,
        retain: ryeos_state::operational::RetainTerminal<'_>,
        retain_lease: RetainLease<'_>,
        retire_after: RetireLeasesAfterBackend<'_>,
    ) -> Result<Self> {
        let state_factory = Arc::clone(factory);
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            Self::open_offline_native_store(
                app_root,
                runtime_state_directory,
                namespace,
                runtime_db_path,
                signer,
                write_barrier,
                factory,
                destination,
                deadline,
                retain,
                retain_lease,
                retire_after,
                true,
                move |directory, namespace| {
                    // Covered by the unsafe block above: the caller holds the StateLock and `namespace`,
                    // the host was funded under that exclusion, and the lease
                    // authority admits only this host's factory.
                    StateDb::open_offline_native_for_rebuild(
                        directory,
                        namespace,
                        head_trust,
                        &state_factory,
                        lease_authority,
                    )
                },
            )
        }
    }

    /// Strict standalone verification store on an in-process native host, in
    /// the stopped-node maintenance shape: the caller holds the runtime
    /// namespace lock. Only the established selected projection generation
    /// opens, read-only, on host tickets; the established Runtime store opens
    /// existing-only for the same liveness reads the live node makes, and
    /// every mutation API is rejected at its common permit gate. Close it
    /// with [`StateStore::close_offline_native_control`].
    /// # Safety
    /// As [`StateStore::new_for_projection_rebuild_native`]; `lease_authority`
    /// must admit only `factory`'s generation leases.
    pub unsafe fn new_for_projection_verification_native(
        app_root: PathBuf,
        runtime_state_directory: lillux::PinnedDirectory,
        namespace: lillux::PinnedDirectoryLock,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        destination: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        deadline: lillux::time::MonotonicDeadline,
        retain: ryeos_state::operational::RetainTerminal<'_>,
        retain_lease: RetainLease<'_>,
        retire_after: RetireLeasesAfterBackend<'_>,
    ) -> Result<Self> {
        let state_factory = Arc::clone(factory);
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            Self::open_offline_native_store(
                app_root,
                runtime_state_directory,
                namespace,
                runtime_db_path,
                signer,
                write_barrier,
                factory,
                destination,
                deadline,
                retain,
                retain_lease,
                retire_after,
                false,
                move |directory, namespace| {
                    // Covered by the unsafe block above: the caller holds the StateLock and `namespace`;
                    // the lease authority admits only this host's factory.
                    StateDb::open_offline_native_verification(
                        directory,
                        namespace,
                        head_trust,
                        &state_factory,
                        lease_authority,
                    )
                },
            )
        }
    }

    /// The shared body of the stopped-node native control stores: Runtime
    /// opens existing-only for liveness, and `open_state` opens the State
    /// side on the same host under the same held `namespace`.
    /// # Safety
    /// As [`StateStore::new_for_projection_rebuild_native`].
    pub(super) unsafe fn open_offline_native_store(
        app_root: PathBuf,
        runtime_state_directory: lillux::PinnedDirectory,
        namespace: lillux::PinnedDirectoryLock,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        destination: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        deadline: lillux::time::MonotonicDeadline,
        retain: ryeos_state::operational::RetainTerminal<'_>,
        retain_lease: RetainLease<'_>,
        retire_after: RetireLeasesAfterBackend<'_>,
        allow_projection_rebuild: bool,
        open_state: impl FnOnce(
            lillux::PinnedDirectory,
            &lillux::PinnedDirectoryLock,
        ) -> Result<StateDb>,
    ) -> Result<Self> {
        use ryeos_state::operational::CarriedOwners;
        namespace.ensure_protects(&runtime_state_directory)?;
        let thread_runtime_authority =
            ThreadRuntimeAuthority::capture(&app_root, &runtime_state_directory, false)?;
        let root = Arc::new(runtime_state_directory.try_clone()?);
        anyhow::ensure!(
            runtime_db_path.parent() == Some(root.path()),
            "native rebuild Runtime database must belong to the locked State namespace"
        );
        let name = runtime_db_path
            .file_name()
            .context("Runtime database path has no basename")?;
        // SAFETY: the caller's funded host issues these tickets under the
        // exclusion this function's contract requires; the fixed Runtime role
        // and the existing-only mode are chosen here.
        let reference_inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference,
                destination.clone(),
            )
        }?;
        let reference = match ryeos_state::sqlite_schema::prepare_native_schema_sql_reference(
            Arc::clone(factory),
            reference_inputs,
            lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference,
            &runtime_db::runtime_schema_sql(),
        ) {
            Ok(reference) => reference,
            Err(failure) => {
                return Err(ryeos_state::operational::release_schema_reference_failure(
                    factory, failure, retain, "Runtime",
                ));
            }
        };
        // SAFETY: as above.
        let issued = unsafe {
            lillux_sqlite::OriginalNativeBoundInputs::issued(
                Arc::clone(&root),
                name,
                lillux_sqlite::OriginalNativeBoundMode::Existing,
                lillux_sqlite::OriginalNativeDomain::NodeRuntime,
                destination.clone(),
                None,
            )
        };
        let inputs = match issued {
            Ok(inputs) => inputs,
            Err(error) => {
                // The closed reference's terminal joins the census before the
                // refusal returns.
                let (_definition, terminal) = reference.into_original_parts();
                return Err(match retain(terminal) {
                    Ok(()) => error,
                    Err((terminal, reason)) => CarriedOwners::carry(
                        error.context(format!(
                            "Runtime schema reference terminal not retained: {reason:#}"
                        )),
                        terminal,
                    ),
                });
            }
        };
        let runtime_db = match runtime_db::RuntimeDb::open_live_with_namespace_authority(
            &runtime_db_path,
            Arc::clone(&root),
            namespace.clone(),
            Arc::clone(factory),
            inputs,
            deadline,
            reference,
        ) {
            Ok(db) => db,
            Err(failure) => {
                let original = anyhow!("open Runtime store for projection rebuild: {failure:#}");
                return Err(match failure.retry_close() {
                    Ok(released) => match released.native_terminals(factory) {
                        Ok(terminals) => match retain_all_terminals(terminals, retain) {
                            Ok(()) => {
                                drop(released);
                                original
                            }
                            Err((refused, reasons)) => CarriedOwners::carry(
                                original
                                    .context(format!("Runtime terminal not retained: {reasons}")),
                                (released, refused),
                            ),
                        },
                        Err(refusal) => CarriedOwners::carry(
                            original
                                .context(format!("released Runtime census refused: {refusal:#}")),
                            released,
                        ),
                    },
                    Err(failure) => CarriedOwners::carry(
                        original.context("refused Runtime acquisition release retained"),
                        failure,
                    ),
                });
            }
        };
        drop(destination);
        let opened = (|| -> Result<_> {
            let attached_processes = attached_processes_from_runtime(&runtime_db)?;
            let live_workers = live_workers_from_runtime(&runtime_db)?;
            let state_db = open_state(runtime_state_directory, &namespace)?;
            Ok((attached_processes, live_workers, state_db))
        })();
        let (attached_processes, live_workers, state_db) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                return Err(match close_runtime_into(runtime_db, factory, retain) {
                    Ok(()) => error,
                    Err(close) => {
                        close.context(format!("projection rebuild open refused: {error:#}"))
                    }
                });
            }
        };
        let projection_health = Arc::new(ThreadProjectionHealth::default());
        let state_authority = match state_db.pinned_authority() {
            Ok(authority) => authority,
            Err(error) => {
                let closed = if allow_projection_rebuild {
                    state_db.close_offline_native(factory)
                } else {
                    state_db.close_verification_native(factory)
                };
                let state = retain_state_census(
                    closed,
                    retain,
                    retain_lease,
                    retire_after,
                    "refused control-store State",
                );
                let runtime = close_runtime_into(runtime_db, factory, retain);
                return Err(match state.and(runtime) {
                    Ok(()) => error,
                    Err(close) => {
                        close.context(format!("projection rebuild open refused: {error:#}"))
                    }
                });
            }
        };
        Ok(Self {
            live_native_open: None,
            state_authority,
            bundle_admission_trust: BundleAdmissionTrust::unbound(),
            thread_runtime_authority: Some(thread_runtime_authority),
            inner: Mutex::new(Inner {
                state_db,
                runtime_db,
                signer,
                active_launch_owners: HashSet::new(),
            }),
            projection_health,
            read_only: true,
            allow_projection_rebuild,
            write_barrier,
            process_attachment_admission_open: AtomicBool::new(true),
            external_guest_owner_reports: Mutex::new(std::collections::BTreeMap::new()),
            process_attachment_admission_closed: tokio::sync::Notify::new(),
            fenced_chain_kinds: std::sync::OnceLock::new(),
            attached_processes: Mutex::new(attached_processes),
            live_workers: Mutex::new(live_workers),
            selected_invocations: Mutex::new(
                crate::selected_invocation_bookkeeping::SelectedInvocationBookkeeping::new(),
            ),
            launch_task_abort_handles: Mutex::new(HashMap::new()),
            active_in_process_handlers: Mutex::new(HashMap::new()),
        })
    }

    /// Close a Store opened by [`StateStore::new_for_projection_rebuild_native`]
    /// or [`StateStore::new_for_projection_verification_native`] into its
    /// host's census: every State and Runtime terminal reaches `retain` and
    /// every closed generation lease reaches `retain_lease`. The Store must be
    /// the sole alias. A refusal carries every remaining owner.
    pub fn close_offline_native_control(
        self: Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        retain: ryeos_state::operational::RetainTerminal<'_>,
        retain_lease: RetainLease<'_>,
        retire_after: RetireLeasesAfterBackend<'_>,
    ) -> Result<()> {
        use ryeos_state::operational::CarriedOwners;
        let store = match Arc::try_unwrap(self) {
            Ok(store) => store,
            Err(shared) => {
                return Err(CarriedOwners::carry(
                    anyhow!("projection rebuild Store still has service owners"),
                    shared,
                ));
            }
        };
        if store.live_native_open.is_some() || !store.read_only {
            return Err(CarriedOwners::carry(
                anyhow!("Store was not opened as a native stopped-node control store"),
                store,
            ));
        }
        let StateStore {
            live_native_open,
            state_authority,
            bundle_admission_trust,
            thread_runtime_authority,
            inner,
            projection_health,
            read_only: _,
            allow_projection_rebuild,
            write_barrier,
            process_attachment_admission_open: _,
            external_guest_owner_reports: _,
            process_attachment_admission_closed,
            fenced_chain_kinds,
            attached_processes,
            live_workers,
            selected_invocations,
            launch_task_abort_handles,
            active_in_process_handlers,
        } = store;
        let Inner {
            state_db,
            runtime_db,
            signer,
            active_launch_owners,
        } = inner
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // In-memory registries, admission and bookkeeping own no descriptor.
        drop((
            live_native_open,
            bundle_admission_trust,
            projection_health,
            write_barrier,
            process_attachment_admission_closed,
            fenced_chain_kinds,
            attached_processes,
            live_workers,
            selected_invocations,
            launch_task_abort_handles,
            active_in_process_handlers,
            signer,
            active_launch_owners,
        ));
        let closed = if allow_projection_rebuild {
            state_db.close_offline_native(factory)
        } else {
            state_db.close_verification_native(factory)
        };
        let state = retain_state_census(
            closed,
            retain,
            retain_lease,
            retire_after,
            "control-store State",
        );
        let runtime = close_runtime_into(runtime_db, factory, retain);
        // The pinned authority roots and the thread-runtime authority end
        // after both databases closed.
        drop((state_authority, thread_runtime_authority));
        state.and(runtime)
    }
}
