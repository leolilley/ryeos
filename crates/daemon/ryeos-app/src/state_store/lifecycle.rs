//! Opening and closing the store, native admissions and census retention.
use super::*;

/// A package is usable only by the live activation owner that prepared its
/// inherited descriptor. After an exclusive runtime-state takeover, every
/// remaining package is abandoned; never retain its bootstrap secret across
/// daemon restart. Refuse unexpected shapes rather than sweeping unrelated
/// state or following a replaced path.
/// Hand every terminal to an in-process census; return the refused ones
/// with their reasons.
pub(super) fn retain_all_terminals(
    terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    retain: ryeos_state::operational::RetainTerminal<'_>,
) -> std::result::Result<(), (Vec<lillux_sqlite::OriginalNativeConnectionTerminal>, String)> {
    let mut refused = Vec::new();
    let mut reasons = Vec::new();
    for terminal in terminals {
        if let Err((terminal, reason)) = retain(terminal) {
            refused.push(terminal);
            reasons.push(format!("{reason:#}"));
        }
    }
    if refused.is_empty() {
        Ok(())
    } else {
        Err((refused, reasons.join("; ")))
    }
}

/// Close an in-process Runtime store into its host's census. A refusal
/// carries the remaining owners.
pub(super) fn close_runtime_into(
    runtime: runtime_db::RuntimeDb,
    factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    retain: ryeos_state::operational::RetainTerminal<'_>,
) -> Result<()> {
    use ryeos_state::operational::CarriedOwners;
    let closed = match runtime.close_original_runtime(Arc::clone(factory)) {
        Ok(closed) => closed,
        Err(failure) => {
            let error = anyhow!("close in-process Runtime store: {failure:#}");
            return Err(CarriedOwners::carry(error, failure));
        }
    };
    let terminals = match closed.native_terminals() {
        Ok(terminals) => terminals,
        Err(refusal) => {
            return Err(CarriedOwners::carry(
                refusal.context("closed Runtime census refused"),
                closed,
            ));
        }
    };
    if let Err((refused, reasons)) = retain_all_terminals(terminals, retain) {
        return Err(CarriedOwners::carry(
            anyhow!("Runtime terminal not retained: {reasons}"),
            (closed, refused),
        ));
    }
    // Its terminals are in the census; the closed facade ends here.
    drop(closed);
    Ok(())
}

/// Records one closed projection generation lease in an in-process host's
/// census; a refusal hands it back with the reason.
pub type RetainLease<'a> = &'a dyn Fn(
    Arc<lillux_sqlite::ClosedOriginalProjectionLease>,
) -> std::result::Result<
    (),
    (
        Arc<lillux_sqlite::ClosedOriginalProjectionLease>,
        anyhow::Error,
    ),
>;

/// Hands a closed State's generation lease owners to an in-process host,
/// which retires them against its finished native backend before release.
pub type RetireLeasesAfterBackend<'a> =
    &'a dyn Fn(ryeos_state::state_db::GenerationLeaseRetirement);

/// Settle a closed in-process State's census: every terminal reaches
/// `retain`, every lease `retain_lease` and the lease owners
/// `retire_after`; a recorded earlier refusal is an error. A State refused
/// before close is carried whole; a refusal that already carries its owners
/// passes through.
pub(super) fn retain_state_census(
    closed: std::result::Result<
        ryeos_state::state_db::LiveStateCensus,
        (Option<StateDb>, anyhow::Error),
    >,
    retain: ryeos_state::operational::RetainTerminal<'_>,
    retain_lease: RetainLease<'_>,
    retire_after: RetireLeasesAfterBackend<'_>,
    what: &str,
) -> Result<()> {
    use ryeos_state::operational::CarriedOwners;
    let census = match closed {
        Ok(census) => census,
        Err((Some(state), refusal)) => {
            return Err(CarriedOwners::carry(
                refusal.context(format!("close {what}")),
                state,
            ));
        }
        Err((None, refusal)) => return Err(refusal.context(format!("close {what}"))),
    };
    let ryeos_state::state_db::LiveStateCensus {
        terminals,
        leases,
        lease_retirement,
        prior_refusal,
    } = census;
    retire_after(lease_retirement);
    let mut refused_leases = Vec::new();
    let mut reasons = Vec::new();
    for lease in leases {
        if let Err((lease, reason)) = retain_lease(lease) {
            refused_leases.push(lease);
            reasons.push(format!("{reason:#}"));
        }
    }
    let refused_terminals = match retain_all_terminals(terminals, retain) {
        Ok(()) => Vec::new(),
        Err((refused, terminal_reasons)) => {
            reasons.push(terminal_reasons);
            refused
        }
    };
    if !reasons.is_empty() {
        return Err(CarriedOwners::carry(
            anyhow!("{what} census not retained: {}", reasons.join("; ")),
            (refused_terminals, refused_leases),
        ));
    }
    match prior_refusal {
        Some(prior) => Err(prior.context(format!("{what} recorded an earlier refusal"))),
        None => Ok(()),
    }
}

/// One durable process attachment as it was committed: the exact identity
/// and the cancellation policy of its launch. Read without SQL by a stop
/// whose service SQL is refused.
#[derive(Debug, Clone)]
pub struct AttachedProcess {
    pub thread_id: String,
    /// `None` only for a durable attachment row that carries no identity;
    /// such an attachment can never be proved dead from memory.
    pub process_identity: Option<crate::process::ExecutionProcessIdentity>,
    pub cancellation_mode: Option<ryeos_engine::contracts::CancellationMode>,
}

/// Rebuild the attachment records from durable runtime rows.
pub(super) fn attached_processes_from_runtime(
    runtime_db: &runtime_db::RuntimeDb,
) -> Result<BTreeMap<String, AttachedProcess>> {
    let mut attached = BTreeMap::new();
    for thread_id in runtime_db.list_attached_thread_ids()? {
        let (process_identity, cancellation_mode) = match runtime_db.get_runtime_info(&thread_id)? {
            Some(runtime) => (
                runtime.process_identity,
                runtime
                    .launch_metadata
                    .and_then(|metadata| metadata.cancellation_mode),
            ),
            None => (None, None),
        };
        attached.insert(
            thread_id.clone(),
            AttachedProcess {
                thread_id,
                process_identity,
                cancellation_mode,
            },
        );
    }
    Ok(attached)
}

/// Rebuild the live dedicated worker records from durable runtime rows.
pub(super) fn live_workers_from_runtime(
    runtime_db: &runtime_db::RuntimeDb,
) -> Result<BTreeMap<String, runtime_db::WorkerProcessRecord>> {
    Ok(runtime_db
        .live_worker_processes()?
        .into_iter()
        .map(|record| (record.worker_instance_id.clone(), record))
        .collect())
}

pub(super) fn with_execution_schema_cutover_hint(error: anyhow::Error) -> anyhow::Error {
    if error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<ryeos_state::IncompatibleCurrentObjectSchema>())
        .any(ryeos_state::IncompatibleCurrentObjectSchema::is_predecessor)
    {
        error.context(format!(
            "authoritative execution history predates the exact current contract; RyeOS will not reinterpret or rewrite it in place. Stop the daemon and run `{}` to retire that history epoch, then restart",
            crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND
        ))
    } else {
        error
    }
}

pub(super) fn ensure_current_external_content_bindings(state: &StateDb) -> Result<()> {
    let namespace = ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE;
    let heads = state.list_generic_head_refs(namespace)?;
    let epoch = state.external_content_binding_schema_epoch()?;
    if heads.is_empty() && epoch.is_none() {
        return Ok(());
    }
    let current_epoch = ryeos_state::objects::EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH;
    if epoch != Some(current_epoch) {
        bail!(
            "external-content binding epoch is {}, expected {current_epoch}; stop the daemon and run `ryeos node reset external-content-bindings --dry-run`, then `ryeos node reset external-content-bindings --confirm`",
            epoch.map_or_else(|| "absent".to_owned(), |value| value.to_string())
        );
    }
    let cas = state.pinned_authority()?.cas_store()?;
    for head in heads {
        let value = cas
            .get_object(&head.target_hash)?
            .ok_or_else(|| anyhow!("external-content binding head target is absent"))?;
        let binding = ryeos_state::objects::ExternalContentBinding::from_value(&value)?;
        if head.namespace != namespace || head.name != binding.binding_subject_id {
            bail!("external-content binding head coordinates are inconsistent");
        }
        if binding.state != ryeos_state::objects::ExternalContentBindingState::Active {
            continue;
        }
        if let Some(source_projection) = binding.consumer.source_closure() {
            let source_value = cas
                .get_object(&source_projection.binding_hash)?
                .ok_or_else(|| anyhow!("project external-content source binding is absent"))?;
            let source_binding =
                ryeos_state::objects::EffectiveSourceBinding::from_value(&source_value)?;
            if source_binding.digest()? != source_projection.binding_hash
                || source_binding.content_manifest_hash != source_projection.content_manifest_hash
                || source_binding.owner_key()? != source_projection.owner_key
            {
                bail!("project external-content source projection is inconsistent");
            }
            let source_manifest_value = cas
                .get_object(&source_binding.content_manifest_hash)?
                .ok_or_else(|| anyhow!("project external-content source manifest is absent"))?;
            let source_manifest =
                ryeos_state::objects::SourceClosureManifest::from_value(&source_manifest_value)?;
            source_binding.validate_content_manifest(&source_manifest)?;
            if source_manifest.entries.len() != source_projection.file_count
                || source_manifest.totals.total_bytes != source_projection.total_bytes
            {
                bail!("project external-content source totals are inconsistent");
            }
        }
        let manifest = cas
            .get_object(&binding.manifest_hash)?
            .ok_or_else(|| anyhow!("active external-content binding manifest is absent"))?;
        let expected_schema = match binding.manifest_kind.as_str() {
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
                ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA
            }
            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
                ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA
            }
            _ => bail!("active external-content binding names an unsupported manifest kind"),
        };
        if manifest.get("schema").and_then(serde_json::Value::as_str) != Some(expected_schema) {
            bail!(
                "external-content binding state predates the current manifest schema; stop the daemon and run `ryeos node reset external-content-bindings --dry-run`, then `ryeos node reset external-content-bindings --confirm`"
            );
        }
    }
    Ok(())
}

impl StateStore {
    /// A Store opened through the native gate on an in-process host: Runtime,
    /// State, Operational and the projection each on a ticket of `factory`,
    /// exactly as a live node opens them, without the node finalizer. Close
    /// it with [`StateStore::close_in_process_census`] into that host's census.
    /// # Safety
    /// `factory` must be the admitted factory of an in-process host this
    /// caller owns and finishes, `lease_authority` must admit only that
    /// factory's generation leases, and `exclusion` must be the exclusion of
    /// this exact App root, held for the Store's whole life.
    pub unsafe fn new_in_process_native(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        exclusion: Arc<ryeos_node_runtime::state_lock::StateLockLease>,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Self> {
        // SAFETY: the caller upholds the contract of this constructor, which is
        // the same contract the bound constructor requires.
        unsafe {
            Self::new_in_process_native_with_bundle_admission_trust(
                app_root,
                runtime_state_dir,
                runtime_db_path,
                signer,
                write_barrier,
                head_trust,
                BundleAdmissionTrust::unbound(),
                exclusion,
                factory,
                lease_authority,
                deadline,
            )
        }
    }

    /// [`Self::new_in_process_native`] bound to the Bundle admission trust a
    /// node would bind, so retained Bundle executors revalidate against it.
    /// # Safety
    /// The same as [`Self::new_in_process_native`].
    pub unsafe fn new_in_process_native_with_bundle_admission_trust(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        bundle_admission_trust: BundleAdmissionTrust,
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
            None,
            original,
        )
    }

    /// Close a Store opened by [`StateStore::new_in_process_native`] into its
    /// native census: every connection terminal and closed generation lease,
    /// for the in-process host to record before it finishes its backend. The
    /// usual close audit applies; a refusal retains the whole close custody.
    pub fn close_in_process_census(
        self: Arc<Self>,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> std::result::Result<StateStoreCensus, LiveStateStoreCloseFailure> {
        self.close_live(Arc::clone(&factory))?.into_census(factory)
    }

    pub(super) fn open(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        bundle_admission_trust: BundleAdmissionTrust,
        recovery_observer: Option<Arc<dyn ryeos_state::ProjectionRecoveryObserver>>,
        live: Arc<live_native::LiveStateOpen>,
    ) -> Result<Self> {
        let (runtime_state_directory, runtime_state_lock) = (live.directory()?, live.namespace());
        discard_abandoned_external_guest_packages(&runtime_state_directory)
            .context("discard abandoned secret-bearing guest packages")?;
        discard_abandoned_runtime_snapshot_staging(&runtime_state_directory)
            .context("discard abandoned runtime snapshot stages")?;
        let thread_runtime_authority =
            ThreadRuntimeAuthority::capture(&app_root, &runtime_state_directory, true)?;
        ryeos_state::CasMutationGuard::ensure_anchor(&runtime_state_dir)
            .context("initialize persistent CAS mutation lock anchor")?;
        ryeos_state::gc::GcLock::ensure_anchor(&runtime_state_dir)
            .context("initialize persistent GC lock anchor")?;

        let projection_health = Arc::new(ThreadProjectionHealth::default());
        // Runtime state must be readable before projection recovery can decide
        // whether a headless Set's replaceable rows are safe to discard. The
        // opening owner already bound the Runtime path to its State namespace.
        let _ = runtime_db_path;
        // Establish the real outside checkout before schema/native Runtime
        // acquisition, so early failures reach the same startup recovery owner.
        let mut custody = live_native::StateOpenCustody::new(live);
        let opened_runtime = custody
            .original()
            .expect("actual Store opening owner")
            .open_runtime();
        let runtime_db = match opened_runtime {
            Ok(runtime) => runtime,
            Err(error) => return Err(custody.fail(error)),
        };
        custody.set_runtime(runtime_db);
        // Launch claims are durable owner evidence. They must survive process
        // restart so reconciliation can revoke the exact abandoned claim after
        // proving its attached process identity dead (or classify a genuinely
        // pre-attach window). Never erase ownership history wholesale here.
        let state_db = custody
            .original()
            .expect("actual Store opening owner")
            .open_state(
                runtime_state_directory,
                runtime_state_lock,
                projection_health.clone(),
                head_trust,
                recovery_observer,
                custody.runtime().expect("acquired Runtime owner"),
            )
            .map_err(with_execution_schema_cutover_hint);
        let state_db = match state_db {
            Ok(state_db) => state_db,
            Err(error) => return Err(custody.fail(error)),
        };
        custody.set_state(state_db);
        let completed = (|| -> Result<_> {
            let state_db = custody.state().expect("acquired State owner");
            let runtime_db = custody.runtime().expect("acquired Runtime owner");
            reconcile_credential_profile_authority(state_db, runtime_db)
                .context("reconcile stable credential-profile authority")?;
            ensure_current_external_content_bindings(state_db)?;
            projection_health
                .observe_pending_transitions(state_db.pending_chain_transitions()?.len());
            let state_authority = state_db.pinned_authority()?;
            let attached_processes = attached_processes_from_runtime(runtime_db)?;
            let live_workers = live_workers_from_runtime(runtime_db)?;
            Ok((state_authority, attached_processes, live_workers))
        })();
        let (state_authority, attached_processes, live_workers) = match completed {
            Ok(completed) => completed,
            Err(error) => return Err(custody.fail(error)),
        };
        let state_db = custody.take_state().expect("completed State owner");
        let runtime_db = custody.take_runtime().expect("completed Runtime owner");
        // Transfer the actual opening owner from the same successful checkout;
        // `live` already moved into custody before State/recovery effects.
        let live_native_open = custody.take_original();
        Ok(Self {
            live_native_open,
            state_authority,
            bundle_admission_trust,
            thread_runtime_authority: Some(thread_runtime_authority),
            inner: Mutex::new(Inner {
                state_db,
                runtime_db,
                signer,
                active_launch_owners: HashSet::new(),
            }),
            projection_health,
            read_only: false,
            allow_projection_rebuild: false,
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

    /// Issue the fixed scheduler role once from the same live namespace and
    /// factory used for State/Runtime. Offline stores cannot issue admission.
    pub fn live_scheduler_native_admission(&self) -> Result<LiveSchedulerNativeAdmission> {
        anyhow::ensure!(
            self.process_attachment_admission_is_open(),
            "StateStore startup/service admission is closed"
        );
        self.live_native_open
            .as_ref()
            .context("live StateStore admission owner absent")?
            .scheduler_admission()
    }

    /// Issue the fixed accounting ledger role once from the same live
    /// namespace and factory used for State/Runtime.
    pub fn live_accounting_native_admission(&self) -> Result<LiveAccountingNativeAdmission> {
        anyhow::ensure!(
            self.process_attachment_admission_is_open(),
            "StateStore startup/service admission is closed"
        );
        self.live_native_open
            .as_ref()
            .context("live StateStore admission owner absent")?
            .accounting_admission()
    }

    /// Monotonic local denial for composition shutdown, without Store locks
    /// or native effects. Existing tickets still need their actual drain.
    pub fn close_live_scheduler_metadata_admission(&self) {
        if let Some(original) = &self.live_native_open {
            original.close_scheduler_admission();
        }
    }

    /// Publish current-daemon ownership before an in-process handler root
    /// becomes visible. A duplicate registration would make ownership
    /// ambiguous and therefore fails closed.
    pub fn register_in_process_handler(&self, thread_id: &str) -> Result<InProcessHandlerControl> {
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("in-process handler registration is closed for daemon shutdown");
        }
        let mut active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("in-process handler registration is closed for daemon shutdown");
        }
        if active.contains_key(thread_id) {
            bail!("in-process handler is already registered for thread {thread_id}");
        }
        let control = InProcessHandlerControl::new();
        active.insert(thread_id.to_string(), control.clone());
        Ok(control)
    }
}
