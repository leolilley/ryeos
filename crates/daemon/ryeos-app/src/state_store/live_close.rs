//! Consuming shutdown under the actual live Store and namespace. No retention
//! mutex spans State/Runtime native close or CAS guard acquisition.
use super::*;
use lillux_sqlite::{OriginalNativeConnectionTerminal, OriginalNativeFactory};

/// Whether an owner recorded in process `process` is only a copy here: this
/// is a forked child, supervised or not, or another process altogether. Reads
/// one atomic and the process id; no lock and no allocation, so a destructor
/// may consult it before touching anything copied.
pub(super) fn foreign_process(process: u32) -> bool {
    lillux::exec::in_forked_child() || process != std::process::id()
}

struct StoreCloseMetadata {
    original: Option<Arc<live_native::LiveStateOpen>>,
    authority: Option<ryeos_state::PinnedStateAuthority>,
    _thread_authority: Option<ThreadRuntimeAuthority>,
    _projection_health: Arc<ThreadProjectionHealth>,
    _mode: (bool, bool),
    _barrier: WriteBarrier,
    _attachment_gate: AtomicBool,
    _attachment_closed: tokio::sync::Notify,
    _attached: Mutex<BTreeMap<String, super::AttachedProcess>>,
    _live_workers: Mutex<BTreeMap<String, runtime_db::WorkerProcessRecord>>,
    _selected: Mutex<crate::selected_invocation_bookkeeping::SelectedInvocationBookkeeping>,
    _launches: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    _handlers: Mutex<HashMap<String, InProcessHandlerControl>>,
    _signer: Arc<dyn Signer>,
    _active_launch_owners: HashSet<String>,
}
impl StoreCloseMetadata {
    fn original(&self) -> Result<&Arc<live_native::LiveStateOpen>> {
        self.original
            .as_ref()
            .context("actual Store parent facade already transferred")
    }
}

struct AuxiliaryDirectorySource {
    process: u32,
    required: bool,
    pinned: Option<lillux::PinnedDirectory>,
    shared: Option<Arc<lillux::PinnedDirectory>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
impl AuxiliaryDirectorySource {
    fn new(pinned: Option<lillux::PinnedDirectory>, required: bool) -> Self {
        Self {
            process: std::process::id(),
            required,
            pinned,
            shared: None,
            registration: None,
        }
    }

    fn register(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store metadata directory source"
        );
        if !self.required {
            anyhow::ensure!(
                self.pinned.is_none() && self.shared.is_none() && self.registration.is_none(),
                "absent Store metadata directory acquired a physical source"
            );
            return Ok(());
        }
        if let Some(binding) = self.registration.as_ref() {
            anyhow::ensure!(
                self.pinned.is_none() && self.shared.is_none(),
                "registered Store metadata directory retains its source alias"
            );
            let quote = registry.membership_work_bound(1, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            registry.require_membership(binding)?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                "Store metadata directory registration changed role");
            return Ok(());
        }
        let owner = if let Some(owner) = self.shared.take() {
            owner
        } else {
            anyhow::ensure!(
                self.pinned.is_some(),
                "Store metadata directory source absent"
            );
            let bytes = std::mem::size_of::<lillux::PinnedDirectory>()
                .checked_add(2 * std::mem::size_of::<usize>())
                .context("Store metadata directory source quote overflow")?;
            registry.prepay_transfer(prepay, u64::try_from(bytes)?, 2)?;
            anyhow::ensure!(
                !foreign_process(self.process),
                "foreign Store metadata source after funding"
            );
            self.pinned
                .take()
                .map(Arc::new)
                .expect("checked Store metadata directory source")
        };
        match unsafe { registry.register_directory(owner, prepay) } {
            Ok(binding) => {
                self.registration = Some(binding);
                let binding = self
                    .registration
                    .as_ref()
                    .expect("cached Store metadata membership");
                registry.require_membership(binding)?;
                anyhow::ensure!(matches!(binding.observed_source_identity()?,
                    lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
                    "Store metadata directory registration changed role");
                Ok(())
            }
            Err((owner, error)) => {
                self.shared = Some(owner);
                Err(error)
            }
        }
    }

    fn require_retired(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store metadata directory receipt"
        );
        if !self.required {
            anyhow::ensure!(
                self.pinned.is_none() && self.shared.is_none() && self.registration.is_none(),
                "absent Store metadata directory acquired a physical source"
            );
            return Ok(());
        }
        anyhow::ensure!(
            self.pinned.is_none() && self.shared.is_none(),
            "retired Store metadata directory retains its source owner"
        );
        let registration = self
            .registration
            .as_ref()
            .context("retired Store metadata directory membership absent")?;
        anyhow::ensure!(matches!(registration.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Directory { .. }),
            "retired Store metadata directory registration changed role");
        drop(registration.receipt_in(registry)?);
        Ok(())
    }
}

struct AuxiliaryNamespaceSource {
    process: u32,
    required: bool,
    owner: Option<lillux::PinnedDirectoryLock>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
impl AuxiliaryNamespaceSource {
    fn register(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store namespace source"
        );
        if !self.required {
            anyhow::ensure!(
                self.owner.is_none() && self.registration.is_none(),
                "absent Store namespace acquired a physical source"
            );
            return Ok(());
        }
        if let Some(binding) = self.registration.as_ref() {
            anyhow::ensure!(
                self.owner.is_none(),
                "registered Store namespace retains its source alias"
            );
            let quote = registry.membership_work_bound(1, 0)?;
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            registry.require_membership(binding)?;
            anyhow::ensure!(matches!(binding.observed_source_identity()?,
                lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. }),
                "Store namespace registration changed role");
            return Ok(());
        }
        self.owner
            .as_ref()
            .context("Store namespace source absent")?;
        registry.prepay_transfer(prepay, 0, 2)?;
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store namespace after funding"
        );
        let owner = self.owner.take().expect("checked Store namespace source");
        match unsafe { registry.register_namespace(owner, prepay) } {
            Ok(binding) => {
                self.registration = Some(binding);
                Ok(())
            }
            Err((owner, error)) => {
                self.owner = Some(owner);
                Err(error)
            }
        }
    }

    fn require_retired(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store metadata namespace receipt"
        );
        if !self.required {
            anyhow::ensure!(
                self.owner.is_none() && self.registration.is_none(),
                "absent Store metadata namespace acquired a physical source"
            );
            return Ok(());
        }
        anyhow::ensure!(
            self.owner.is_none(),
            "retired Store metadata namespace retains its source owner"
        );
        let registration = self
            .registration
            .as_ref()
            .context("retired Store metadata namespace membership absent")?;
        anyhow::ensure!(matches!(registration.observed_source_identity()?,
            lillux_sqlite::original_physical_custody::OriginalPhysicalSourceIdentity::Namespace { .. }),
            "retired Store metadata namespace registration changed role");
        drop(registration.receipt_in(registry)?);
        Ok(())
    }
}

struct StoreAuxiliaryPhysicalSources {
    process: u32,
    state_directories: [AuxiliaryDirectorySource; 3],
    state_namespace: AuxiliaryNamespaceSource,
    state_recovery: Option<ryeos_state::recovery::RecoveryStore>,
    thread_directories: [AuxiliaryDirectorySource; 3],
}
impl StoreAuxiliaryPhysicalSources {
    fn capture(metadata: &mut StoreCloseMetadata) -> Result<Self> {
        let state = metadata
            .authority
            .take()
            .context("actual PinnedStateAuthority absent at Store finalization")?
            .into_finalization_sources();
        let thread = metadata._thread_authority.take();
        let thread_present = thread.is_some();
        let (thread_app, thread_runtime, thread_root) = match thread {
            Some(ThreadRuntimeAuthority {
                app_root,
                runtime_state,
                threads_root,
            }) => (Some(app_root), Some(runtime_state), threads_root),
            None => (None, None, None),
        };
        let thread_root_required = thread_root.is_some();
        let namespace_required = state.offline_namespace_lock.is_some();
        Ok(Self {
            process: std::process::id(),
            state_directories: [
                AuxiliaryDirectorySource::new(state.runtime_directory, true),
                AuxiliaryDirectorySource::new(state.cas_directory, true),
                AuxiliaryDirectorySource::new(state.refs_directory, true),
            ],
            state_namespace: AuxiliaryNamespaceSource {
                process: std::process::id(),
                required: namespace_required,
                owner: state.offline_namespace_lock,
                registration: None,
            },
            state_recovery: state.recovery,
            thread_directories: [
                AuxiliaryDirectorySource::new(thread_app, thread_present),
                AuxiliaryDirectorySource::new(thread_runtime, thread_present),
                AuxiliaryDirectorySource::new(thread_root, thread_root_required),
            ],
        })
    }

    fn register(
        &mut self,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store auxiliary physical sources"
        );
        for source in self
            .state_directories
            .iter_mut()
            .chain(self.thread_directories.iter_mut())
        {
            source.register(registry, prepay)?;
        }
        self.state_namespace.register(registry, prepay)
    }

    fn retire_recovery_aliases(
        &mut self,
        state: &mut ryeos_state::state_db::ClosedLiveStatePhysicalCustody,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store recovery aliases"
        );
        state.retire_recovery_aliases(&mut self.state_recovery, registry, prepay)
    }

    fn require_retired(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        anyhow::ensure!(
            !foreign_process(self.process),
            "foreign Store auxiliary receipt frame"
        );
        anyhow::ensure!(
            self.state_recovery.is_none(),
            "retired Store metadata retains a RecoveryStore alias"
        );
        for source in self
            .state_directories
            .iter()
            .chain(self.thread_directories.iter())
        {
            source.require_retired(registry)?;
        }
        self.state_namespace.require_retired(registry)
    }
}

const STORE_CLOSE_PROGRESS_KIND: &str = "ryeos-app::store-close-progress";
const CLOSED_STORE_KIND: &str = "ryeos-app::closed-store";
const STORE_CLOSE_FAILURE_KIND: &str = "ryeos-app::store-close-failure";

/// Every owner a live Store's close holds. It has no destructor of its own:
/// it is always held in a `Held`, which drops it only when final exhaustion
/// would accept it and otherwise retains every parent, packet, registry,
/// Runtime and State owner, with the metadata's Store alias and authorities,
/// together until the process exits. Constructible only in this module.
struct StoreCloseProgress {
    store_parent: Option<live_native::LiveStateOpen>,
    store_parent_parts: Option<live_native::LiveStorePhysicalParent>,
    store_parent_finalization: Option<live_native::LiveStoreParentFinalizationParts>,
    store_destination_join:
        std::sync::OnceLock<Arc<super::live_destination_join::LiveStoreDestinationJoin>>,
    runtime_packet_parts: Option<Vec<super::live_outside::OriginalPacketPartsProgress>>,
    operational_packet_parts: Option<Vec<super::live_outside::OriginalPacketPartsProgress>>,
    scheduler_packet_parts: Option<Vec<super::live_outside::OriginalPacketPartsProgress>>,
    scheduler_unopened_database: Option<super::live_outside::UnopenedSchedulerDatabaseProgress>,
    scheduler_unopened_schema_reference:
        Option<super::live_outside::UnopenedNativeTransientProgress>,
    accounting_unopened_schema_reference:
        Option<super::live_outside::UnopenedNativeTransientProgress>,
    scheduler_admission_sources:
        Option<super::live_native::LiveSchedulerNativeAdmissionOutsideSources>,
    operational_sql_custody: Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    runtime_sql_custody: Option<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    physical_registry: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry>,
    auxiliary_physical_sources: Option<StoreAuxiliaryPhysicalSources>,
    metadata: StoreCloseMetadata,
    runtime: Option<runtime_db::RuntimeDb>,
    runtime_failure: Option<runtime_db::OriginalRuntimeCloseFailure>,
    runtime_closed: Option<runtime_db::OriginalRuntimeClosed>,
    runtime_outside: Option<runtime_db::OriginalLiveRuntimeOutsideCustody>,
    state: Option<StateDb>,
    state_failure: Option<ryeos_state::state_db::LiveStateCloseFailure>,
    state_closed: Option<ryeos_state::state_db::ClosedLiveState>,
    state_physical: Option<ryeos_state::state_db::ClosedLiveStatePhysicalCustody>,
}
impl StoreCloseProgress {
    /// Hold this progress: settled exactly when final exhaustion would accept
    /// it without a registry.
    fn held(self) -> lillux::custody::Held<StoreCloseProgress> {
        lillux::custody::Held::new(
            STORE_CLOSE_PROGRESS_KIND,
            self,
            store_close_progress_exhausted,
        )
    }
    /// The registry-free part of final exhaustion: every owner slot consumed,
    /// the destination receiver completed, and every packet, source and
    /// admission exhausted. A dropped progress settles on exactly these
    /// checks.
    fn require_exhausted_holders(&self) -> Result<()> {
        let p = self;
        anyhow::ensure!(
            p.store_parent.is_none()
                && p.store_parent_parts.is_none()
                && p.store_parent_finalization.is_none()
                && p.physical_registry.is_none()
                && p.auxiliary_physical_sources.is_none()
                && p.metadata.original.is_none()
                && p.metadata.authority.is_none()
                && p.metadata._thread_authority.is_none(),
            "closed Store retains a parent, registry or metadata source at exhaustion"
        );
        anyhow::ensure!(
            p.runtime.is_none()
                && p.runtime_failure.is_none()
                && p.runtime_closed.is_none()
                && p.runtime_outside.is_none()
                && p.state.is_none()
                && p.state_failure.is_none()
                && p.state_closed.is_none()
                && p.state_physical.is_none(),
            "closed Store retains a State or Runtime frame at exhaustion"
        );
        p.store_destination_join
            .get()
            .context("closed Store destination receiver absent at exhaustion")?
            .require_parent_completed_holders()?;
        for packets in [
            &p.runtime_packet_parts,
            &p.operational_packet_parts,
            &p.scheduler_packet_parts,
        ] {
            super::live_outside::require_original_packets_exhausted(packets)?;
        }
        super::live_outside::require_sql_custody_exhausted(&p.operational_sql_custody)?;
        super::live_outside::require_sql_custody_exhausted(&p.runtime_sql_custody)?;
        if let Some(database) = p.scheduler_unopened_database.as_ref() {
            database.require_exhausted()?;
        }
        if let Some(reference) = p.scheduler_unopened_schema_reference.as_ref() {
            reference.require_exhausted()?;
        }
        if let Some(admission) = p.scheduler_admission_sources.as_ref() {
            admission.require_exhausted()?;
        }
        if let Some(reference) = p.accounting_unopened_schema_reference.as_ref() {
            reference.require_exhausted()?;
        }
        Ok(())
    }
}
/// Settled exactly when final exhaustion would accept the progress.
fn store_close_progress_exhausted(progress: &StoreCloseProgress) -> bool {
    progress.require_exhausted_holders().is_ok()
}
/// Actual State and Runtime terminal holders plus their original authority.
/// This is neither aggregate native shutdown nor durable reservation settlement.
pub struct ClosedLiveStateStore {
    inner: lillux::custody::Held<ClosedLiveStateStoreParts>,
}

/// The close progress and the prior refusal. Constructible only in this
/// module.
struct ClosedLiveStateStoreParts {
    progress: StoreCloseProgress,
    prior_refusal: Option<anyhow::Error>,
}
impl ClosedLiveStateStoreParts {
    /// The registry-free part of final exhaustion: the progress holders and
    /// a prior refusal that retains no owner.
    fn require_exhausted_holders(&self) -> Result<()> {
        self.progress.require_exhausted_holders()?;
        ryeos_state::state_db::require_exhausted_refusals(self.prior_refusal.iter())
    }
}
/// Settled exactly when the progress inside would settle and the prior
/// refusal retains nothing; otherwise it stays retained whole, with the
/// refusal, until the process exits.
fn closed_store_exhausted(parts: &ClosedLiveStateStoreParts) -> bool {
    parts.require_exhausted_holders().is_ok()
}
impl ClosedLiveStateStore {
    fn new(progress: StoreCloseProgress, prior_refusal: Option<anyhow::Error>) -> Self {
        Self {
            inner: lillux::custody::Held::new(
                CLOSED_STORE_KIND,
                ClosedLiveStateStoreParts {
                    progress,
                    prior_refusal,
                },
                closed_store_exhausted,
            ),
        }
    }
    fn progress_mut(&mut self) -> &mut StoreCloseProgress {
        &mut self.inner.get_mut().progress
    }

    /// Move the progress and the prior refusal out; a copy in another process
    /// is refused and comes back still held.
    fn into_parts(self) -> std::result::Result<(StoreCloseProgress, Option<anyhow::Error>), Self> {
        self.inner
            .take_whole()
            .map(|parts| {
                let ClosedLiveStateStoreParts {
                    progress,
                    prior_refusal,
                } = parts;
                (progress, prior_refusal)
            })
            .map_err(|inner| Self { inner })
    }

    /// The registry-free part of final exhaustion. A dropped frame settles on
    /// exactly these checks.
    fn require_exhausted_holders(&self) -> Result<()> {
        anyhow::ensure!(
            !self.inner.is_foreign(),
            "foreign closed Store at exhaustion"
        );
        self.inner.require_exhausted_holders()
    }
}
impl ClosedLiveStateStore {
    pub(super) fn physical_registry_slot(
        &mut self,
    ) -> &mut Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry> {
        &mut self.progress_mut().physical_registry
    }

    pub(super) fn consume_retired_auxiliary_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        let progress = self.progress_mut();
        let sources = progress
            .auxiliary_physical_sources
            .as_ref()
            .context("actual Store auxiliary source frame absent after registry retirement")?;
        sources.require_retired(registry)?;
        // All owners and registrations were checked above. This consuming
        // transition releases only the now-receipted source handles; the
        // original directory and namespace descriptors were closed by the
        // same registry before this call.
        progress.auxiliary_physical_sources = None;
        Ok(())
    }

    /// Retire State-generated Bound metadata pins, then end State's remaining
    /// strong native owners, before the region seal. The State frame must
    /// already be in physical custody; refusal retains it.
    pub(super) fn retire_state_native_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_metadata_pin_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> Result<
            Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>,
        >,
    ) -> Result<()> {
        let StoreCloseProgress {
            state_physical,
            physical_registry,
            ..
        } = self.progress_mut();
        let registry = physical_registry.as_ref().context(
            "closed Store physical registry absent before State native-source retirement",
        )?;
        state_physical
            .as_mut()
            .context("closed Store State physical frame absent before native-source retirement")?
            .retire_native_sources(
                factory,
                terminal,
                registry,
                prepay,
                issue_metadata_pin_inputs,
            )
    }

    pub(super) fn require_pre_seal_complete(&self) -> Result<()> {
        let p = &self.inner.progress;
        let registry = p
            .physical_registry
            .as_ref()
            .context("closed Store physical registry absent at pre-seal census")?;
        anyhow::ensure!(
            p.store_parent.is_none()
                && p.store_parent_parts.is_none()
                && p.metadata.original.is_none(),
            "closed Store parent owner remains outside its finalization parts"
        );
        let parts = p
            .store_parent_finalization
            .as_ref()
            .context("closed Store parent finalization parts absent at pre-seal census")?;
        anyhow::ensure!(
            parts.native_authorities_retired
                && parts.factory.is_none()
                && parts.lease_authority.is_none(),
            "closed Store parent retains native authorities before the region seal"
        );
        let join = p
            .store_destination_join
            .get()
            .context("closed Store destination receiver absent at pre-seal census")?;
        anyhow::ensure!(
            Arc::ptr_eq(&parts.join, join),
            "closed Store parent frame changed its destination receiver"
        );
        join.require_parent_completed_in_registry(registry)?;
        for (packets, source_kind) in [
            (
                &p.runtime_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Runtime,
            ),
            (
                &p.operational_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Operational,
            ),
            (
                &p.scheduler_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Scheduler,
            ),
        ] {
            super::live_outside::require_original_packet_pre_seal_complete(packets, source_kind)?;
        }
        anyhow::ensure!(
            p.scheduler_unopened_database
                .as_ref()
                .is_none_or(super::live_outside::UnopenedSchedulerDatabaseProgress::is_joined)
                && p.scheduler_unopened_schema_reference
                    .as_ref()
                    .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined)
                && p.scheduler_admission_sources.as_ref().is_none_or(
                    super::live_native::LiveSchedulerNativeAdmissionOutsideSources::is_joined
                )
                && p.accounting_unopened_schema_reference
                    .as_ref()
                    .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined),
            "closed Store Scheduler or accounting unopened, reference or admission source is not joined before the region seal"
        );
        self.require_state_native_sources_retired()
    }

    pub(super) fn require_state_native_sources_retired(&self) -> Result<()> {
        let state = self
            .inner
            .progress
            .state_physical
            .as_ref()
            .context("closed Store State physical frame absent before seal")?;
        anyhow::ensure!(
            state.native_sources_retired(),
            "closed Store seal precedes State native-source retirement"
        );
        anyhow::ensure!(
            state.live_parent_completed()? && state.recovery_aliases_retired(),
            "closed Store seal precedes the State live-parent or recovery-alias join"
        );
        Ok(())
    }

    pub(super) fn consume_retired_state_projection_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let state = self
            .progress_mut()
            .state_physical
            .as_mut()
            .context("closed Store State physical frame absent after registry retirement")?;
        state.consume_retired_projection_sources(registry, prepay)
    }

    pub(super) fn consume_retired_state_root_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let progress = self.progress_mut();
        anyhow::ensure!(
            progress
                .runtime_packet_parts
                .iter()
                .all(|packets| super::live_outside::packet_source_parts_consumed(Some(packets)))
                && progress.operational_packet_parts.iter().all(|packets| {
                    super::live_outside::packet_source_parts_consumed(Some(packets))
                })
                && progress
                    .scheduler_packet_parts
                    .as_ref()
                    .is_none_or(|packets| packets
                        .iter()
                        .all(|packet| packet.source_parts_consumed())),
            "State and Operational roots cannot retire before Store packet source receipts"
        );
        progress
            .state_physical
            .as_mut()
            .context("closed Store State physical frame absent after registry retirement")?
            .consume_retired_root_sources(registry, prepay)
    }

    pub(super) fn consume_retired_runtime_shared_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.progress_mut()
            .runtime_outside
            .as_mut()
            .context("closed Store Runtime outside frame absent after registry retirement")?
            .consume_retired_shared_roots(registry, prepay)
    }

    pub(super) fn consume_retired_parent_finalization_parts(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        let progress = self.progress_mut();
        anyhow::ensure!(
            progress.store_parent.is_none()
                && progress.store_parent_parts.is_none()
                && progress.metadata.original.is_none()
                && progress.metadata.authority.is_none()
                && progress.metadata._thread_authority.is_none(),
            "retired Store parent frame retains an untransferred owner"
        );
        let parts = progress
            .store_parent_finalization
            .as_ref()
            .context("retired Store parent finalization frame absent")?;
        let join = progress
            .store_destination_join
            .get()
            .context("retired Store destination receiver absent")?;
        anyhow::ensure!(
            Arc::ptr_eq(&parts.join, join),
            "retired Store parent frame changed its destination receiver"
        );
        parts.require_retired(registry)?;
        // Native-dependent authorities were consumed before region disposal.
        // This post-disposal transition checks only the cached registry
        // identity and actual physical receipts, then drops the remaining
        // ordinary metadata frame. The receiver remains separately cached
        // until its packet continuations are explicitly consumed.
        progress.store_parent_finalization = None;
        Ok(())
    }

    /// Consume the unopened Scheduler root registrations (direct and failed
    /// admission) with their exact close receipts from the retired registry.
    pub(super) fn consume_retired_scheduler_sources(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        let progress = self.progress_mut();
        if let Some(database) = progress.scheduler_unopened_database.as_mut() {
            database.consume_retired(registry)?;
        }
        if let Some(admission) = progress.scheduler_admission_sources.as_mut() {
            admission.consume_retired(registry)?;
        }
        Ok(())
    }

    /// Consume the State and Runtime outside frames after their receipt
    /// stages. Each frame refuses intact and is restored to its slot.
    pub(super) fn consume_retired_owner_frames(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let progress = self.progress_mut();
        if let Some(state) = progress.state_physical.take()
            && let Err((state, error)) = state.into_exhausted(registry, prepay)
        {
            progress.state_physical = Some(state);
            return Err(error);
        }
        if let Some(runtime) = progress.runtime_outside.take()
            && let Err((runtime, error)) = runtime.into_exhausted()
        {
            progress.runtime_outside = Some(runtime);
            return Err(error);
        }
        Ok(())
    }

    fn require_exhausted(
        &self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> Result<()> {
        self.require_exhausted_holders()?;
        self.inner
            .progress
            .store_destination_join
            .get()
            .context("closed Store destination receiver absent at exhaustion")?
            .require_parent_completed_in_retired_registry(registry)
    }

    /// Final consumption of the closed Store graph. Every owner slot must
    /// already be consumed; the remaining fields are receipts, identities and
    /// in-memory metadata. Refusal returns the graph intact.
    pub(super) fn into_exhausted(
        self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
    ) -> std::result::Result<super::live_outside::StoreExhausted, (Self, anyhow::Error)> {
        let exhausted = match self
            .require_exhausted(registry)
            .and_then(|()| super::live_outside::StoreExhausted::new(registry))
        {
            Ok(exhausted) => exhausted,
            Err(error) => return Err((self, error)),
        };
        let (progress, prior_refusal) = match self.into_parts() {
            Ok(parts) => parts,
            Err(this) => return Err((this, anyhow!("closed Store belongs to another process"))),
        };
        let StoreCloseProgress {
            store_parent,
            store_parent_parts,
            store_parent_finalization,
            store_destination_join,
            runtime_packet_parts,
            operational_packet_parts,
            scheduler_packet_parts,
            scheduler_unopened_database,
            scheduler_unopened_schema_reference,
            accounting_unopened_schema_reference,
            scheduler_admission_sources,
            operational_sql_custody,
            runtime_sql_custody,
            physical_registry,
            auxiliary_physical_sources,
            metadata,
            runtime,
            runtime_failure,
            runtime_closed,
            runtime_outside,
            state,
            state_failure,
            state_closed,
            state_physical,
        } = progress;
        drop(store_parent);
        drop(store_parent_parts);
        drop(store_parent_finalization);
        drop(store_destination_join);
        super::live_outside::exhaust_original_packets(runtime_packet_parts);
        super::live_outside::exhaust_original_packets(operational_packet_parts);
        super::live_outside::exhaust_original_packets(scheduler_packet_parts);
        if let Some(database) = scheduler_unopened_database {
            database.exhaust();
        }
        if let Some(reference) = scheduler_unopened_schema_reference {
            reference.exhaust();
        }
        if let Some(admission) = scheduler_admission_sources {
            admission.exhaust();
        }
        if let Some(reference) = accounting_unopened_schema_reference {
            reference.exhaust();
        }
        super::live_outside::exhaust_sql_custody(operational_sql_custody);
        super::live_outside::exhaust_sql_custody(runtime_sql_custody);
        drop(physical_registry);
        drop(auxiliary_physical_sources);
        let StoreCloseMetadata {
            original,
            authority,
            _thread_authority,
            _projection_health,
            _mode: _,
            _barrier,
            _attachment_gate: _,
            _attachment_closed,
            _attached,
            _live_workers,
            _selected,
            _launches,
            _handlers,
            _signer,
            _active_launch_owners,
        } = metadata;
        drop(original);
        drop(authority);
        drop(_thread_authority);
        // In-memory projection health, barrier, bookkeeping, abort handles,
        // handler controls and signer; none owns a descriptor.
        drop(_projection_health);
        drop(_barrier);
        drop(_attached);
        drop(_live_workers);
        drop(_selected);
        drop(_launches);
        drop(_handlers);
        drop(_signer);
        drop(_active_launch_owners);
        drop(runtime);
        drop(runtime_failure);
        drop(runtime_closed);
        drop(runtime_outside);
        drop(state);
        drop(state_failure);
        drop(state_closed);
        drop(state_physical);
        drop(prior_refusal);
        Ok(exhausted)
    }

    pub(super) fn scheduler_native_store_handoff_receipt<'store>(
        &'store self,
        handoff_id: &ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<ryeos_scheduler::db::SchedulerNativeStoreHandoffReceipt<'store>> {
        let p = &self.inner.progress;
        super::live_outside::issue_scheduler_store_handoff_receipt(
            self,
            p.scheduler_packet_parts.as_ref(),
            p.scheduler_unopened_database.as_ref(),
            p.scheduler_unopened_schema_reference.as_ref(),
            p.scheduler_admission_sources.as_ref(),
            p.physical_registry.as_ref(),
            handoff_id,
            factory,
            aggregate,
        )
    }

    /// Move the entire real opening-owner body to this already cached frame.
    /// This is parent custody, never a native/physical/account terminal receipt.
    pub(super) fn advance_store_parent(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.inner.progress.store_parent.is_some()
            || self.inner.progress.store_parent_parts.is_some()
        {
            anyhow::ensure!(
                self.inner.progress.metadata.original.is_none(),
                "Store retains transferred body and duplicate facade"
            );
            let p = self.progress_mut();
            live_native::register_store_parent_roots(
                &mut p.store_parent,
                &mut p.store_parent_parts,
                p.physical_registry
                    .as_mut()
                    .context("actual Store physical registry absent")?,
                factory,
                aggregate,
                prepay,
            )?;
            return Ok(());
        }
        let p = self.progress_mut();
        anyhow::ensure!(
            p.runtime.is_none()
                && p.runtime_failure.is_none()
                && p.runtime_closed.is_none()
                && p.runtime_outside.is_some()
                && p.state.is_none()
                && p.state_failure.is_none()
                && p.state_closed.is_none()
                && p.state_physical.is_some(),
            "actual Store child native/physical frame handoff incomplete"
        );
        let registry = p
            .physical_registry
            .as_ref()
            .context("actual Store physical inventory absent")?;
        let state = p
            .state_physical
            .as_ref()
            .expect("checked actual State physical frame");
        state.require_original(factory, aggregate)?;
        state.require_operational_registered(registry, prepay)?;
        p.runtime_outside
            .as_ref()
            .expect("checked actual Runtime outside frame")
            .require_shared_roots_registered(registry, prepay)?;
        anyhow::ensure!(
            p.runtime_sql_custody.as_ref().is_some_and(|v| v.is_empty())
                && p.operational_sql_custody
                    .as_ref()
                    .is_some_and(|v| v.is_empty())
                && p.runtime_packet_parts.is_some()
                && p.operational_packet_parts.is_some(),
            "actual Store retains unnormalized native packets"
        );
        super::live_outside::require_original_packet_sources_transferred(
            &p.runtime_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Runtime,
        )?;
        super::live_outside::require_original_packet_sources_transferred(
            &p.operational_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Operational,
        )?;
        if p.scheduler_packet_parts.is_some() {
            super::live_outside::require_original_packet_sources_transferred(
                &p.scheduler_packet_parts,
                factory,
                aggregate,
                super::live_outside::OriginalPacketSourceKind::Scheduler,
            )?;
        }
        anyhow::ensure!(
            p.scheduler_unopened_database
                .as_ref()
                .is_none_or(super::live_outside::UnopenedSchedulerDatabaseProgress::is_joined),
            "unopened Scheduler database is not joined to the Store inventory"
        );
        anyhow::ensure!(
            p.scheduler_unopened_schema_reference
                .as_ref()
                .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined),
            "unused Scheduler schema input is not joined to the Store authority"
        );
        anyhow::ensure!(
            p.scheduler_admission_sources.as_ref().is_none_or(
                super::live_native::LiveSchedulerNativeAdmissionOutsideSources::is_joined
            ),
            "failed Scheduler admission is not joined to the Store inventory"
        );
        anyhow::ensure!(
            p.accounting_unopened_schema_reference
                .as_ref()
                .is_none_or(super::live_outside::UnopenedNativeTransientProgress::is_joined),
            "unused accounting schema input is not joined to the Store authority"
        );
        let join = p
            .store_destination_join
            .get()
            .context("actual Store receiver absent")?;
        join.require_original_origin(factory, aggregate)?;
        if let Some(original) = p.metadata.original.as_ref() {
            original.require_close_factory(factory)?;
            join.require_original_parent(original, factory, aggregate)?;
            let quote = live_native::quote_store_parent_facade_retirement();
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(quote.bytes, quote.steps)
            })?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            join.require_original_parent(original, factory, aggregate)?;
            // The complete Store frame and receiver are already retained by
            // the caller. Remove only the duplicate facade after all checks.
            drop(p.metadata.original.take());
        }
        join.transfer_original_parent(factory, aggregate, &mut p.store_parent, prepay)?;
        live_native::register_store_parent_roots(
            &mut p.store_parent,
            &mut p.store_parent_parts,
            p.physical_registry
                .as_mut()
                .context("actual Store physical registry absent")?,
            factory,
            aggregate,
            prepay,
        )
    }

    pub(super) fn advance_scheduler_packet_parts(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        handoff: Option<(
            ryeos_scheduler::db::SchedulerNativeStoreHandoffId,
            ryeos_scheduler::db::SchedulerNativeStoreSourceRole,
        )>,
    ) -> Result<()> {
        let p = self.progress_mut();
        super::live_outside::prepare_scheduler_packet_parts(
            source,
            &mut p.scheduler_packet_parts,
            factory,
            aggregate,
            prepay,
            handoff,
        )?;
        let registry = p
            .physical_registry
            .as_mut()
            .context("actual Store physical registry absent for Scheduler source")?;
        super::live_outside::register_original_packet_bound_roots(
            &mut p.scheduler_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Scheduler,
            registry,
            prepay,
        )
    }

    pub(super) fn advance_unopened_scheduler_database(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeBoundInputs>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        handoff_id: Option<ryeos_scheduler::db::SchedulerNativeStoreHandoffId>,
    ) -> Result<()> {
        let p = self.progress_mut();
        let expected = p.metadata.original.as_ref();
        let registry = p
            .physical_registry
            .as_mut()
            .context("actual Store physical registry absent for unopened Scheduler source")?;
        super::live_outside::advance_unopened_scheduler_database(
            source,
            &mut p.scheduler_unopened_database,
            expected,
            factory,
            aggregate,
            registry,
            prepay,
            handoff_id,
        )
    }

    pub(super) fn advance_unopened_scheduler_transient(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeTransientInputs>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let p = self.progress_mut();
        let expected = p.metadata.original.as_ref();
        super::live_outside::advance_unopened_scheduler_transient(
            source,
            &mut p.scheduler_unopened_schema_reference,
            expected,
            factory,
            aggregate,
            prepay,
        )
    }

    pub(super) fn advance_unopened_accounting_transient(
        &mut self,
        source: &mut Option<lillux_sqlite::OriginalNativeTransientInputs>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let p = self.progress_mut();
        let expected = p.metadata.original.as_ref();
        super::live_outside::advance_unopened_native_transient(
            source,
            &mut p.accounting_unopened_schema_reference,
            lillux_sqlite::OriginalNativeDomain::NodeAccountingSchemaReference,
            expected,
            factory,
            aggregate,
            prepay,
        )
    }

    pub(super) fn advance_scheduler_admission_sources(
        &mut self,
        source: &mut Option<super::live_native::LiveSchedulerNativeAdmissionOutsideSources>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let p = self.progress_mut();
        if source.is_some() && p.scheduler_admission_sources.is_none() {
            let bytes = std::mem::size_of::<
                super::live_native::LiveSchedulerNativeAdmissionOutsideSources,
            >();
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                prepay(u64::try_from(bytes)?, 8)
            })?;
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            p.scheduler_admission_sources = source.take();
        }
        anyhow::ensure!(
            source.is_none(),
            "failed Scheduler admission has conflicting Store progress"
        );
        if let Some(progress) = p.scheduler_admission_sources.as_mut() {
            let expected = p.metadata.original.as_ref();
            let registry = p
                .physical_registry
                .as_mut()
                .context("actual Store physical registry absent for failed Scheduler admission")?;
            progress.advance(expected, factory, aggregate, registry, prepay)?;
        }
        Ok(())
    }

    pub(super) fn consume_retired_packet_destinations(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let StoreCloseProgress {
            runtime_packet_parts,
            operational_packet_parts,
            scheduler_packet_parts,
            ..
        } = self.progress_mut();
        super::live_outside::consume_retired_original_packet_destinations(
            runtime_packet_parts,
            registry,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            prepay,
        )?;
        super::live_outside::consume_retired_original_packet_destinations(
            operational_packet_parts,
            registry,
            super::live_outside::OriginalPacketSourceKind::Operational,
            prepay,
        )?;
        super::live_outside::consume_retired_original_packet_destinations(
            scheduler_packet_parts,
            registry,
            super::live_outside::OriginalPacketSourceKind::Scheduler,
            prepay,
        )
    }

    pub(super) fn consume_retired_packet_bound_roots(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let StoreCloseProgress {
            runtime_packet_parts,
            operational_packet_parts,
            scheduler_packet_parts,
            ..
        } = self.progress_mut();
        super::live_outside::consume_retired_original_packet_bound_roots(
            runtime_packet_parts,
            registry,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            prepay,
        )?;
        super::live_outside::consume_retired_original_packet_bound_roots(
            operational_packet_parts,
            registry,
            super::live_outside::OriginalPacketSourceKind::Operational,
            prepay,
        )?;
        super::live_outside::consume_retired_original_packet_bound_roots(
            scheduler_packet_parts,
            registry,
            super::live_outside::OriginalPacketSourceKind::Scheduler,
            prepay,
        )
    }

    pub(super) fn consume_retired_packet_metadata_pins(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let StoreCloseProgress {
            runtime_packet_parts,
            operational_packet_parts,
            scheduler_packet_parts,
            ..
        } = self.progress_mut();
        super::live_outside::consume_retired_original_packet_metadata_pins(
            runtime_packet_parts,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            registry,
            prepay,
        )?;
        super::live_outside::consume_retired_original_packet_metadata_pins(
            operational_packet_parts,
            super::live_outside::OriginalPacketSourceKind::Operational,
            registry,
            prepay,
        )?;
        super::live_outside::consume_retired_original_packet_metadata_pins(
            scheduler_packet_parts,
            super::live_outside::OriginalPacketSourceKind::Scheduler,
            registry,
            prepay,
        )
    }

    pub(super) fn consume_retired_packet_source_parts(
        &mut self,
        registry: &lillux_sqlite::original_physical_custody::RetiredOriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<u8> {
        let StoreCloseProgress {
            runtime_packet_parts,
            operational_packet_parts,
            scheduler_packet_parts,
            ..
        } = self.progress_mut();
        let mut failures = 0u8;
        for (packets, source_kind) in [
            (
                runtime_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Runtime,
            ),
            (
                operational_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Operational,
            ),
            (
                scheduler_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Scheduler,
            ),
        ] {
            failures = failures
                .checked_add(
                    super::live_outside::consume_retired_original_packet_source_parts(
                        packets,
                        source_kind,
                        registry,
                        prepay,
                    )?,
                )
                .context("Store packet acquisition-failure count overflow")?;
        }
        Ok(failures)
    }

    pub(super) fn complete_store_parent_join(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
        issue_metadata_pin_inputs: &dyn Fn(
            &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
            &[Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 2],
        ) -> Result<
            Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>,
        >,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let p = self.progress_mut();
        if p.store_parent_finalization.is_none() {
            anyhow::ensure!(
                p.store_parent.is_none(),
                "actual Store parent body remains outside its physical frame"
            );
            let parent = p
                .store_parent_parts
                .take()
                .context("actual Store physical parent parts absent at completion")?;
            match parent.into_finalization_parts(factory, aggregate, prepay) {
                Ok(parts) => p.store_parent_finalization = Some(parts),
                Err((parent, error)) => {
                    p.store_parent_parts = Some(parent);
                    return Err(error);
                }
            }
        }
        let registry = p
            .physical_registry
            .as_mut()
            .context("actual Store physical registry absent at parent completion")?;
        let registry_identity = registry.identity()?;
        let parts = p
            .store_parent_finalization
            .as_ref()
            .context("actual Store finalization parts were not retained")?;
        if parts.native_authorities_retired {
            parts.join.require_parent_completed_in_registry(registry)?;
        } else {
            parts
                .join
                .complete_parent_outside(parts, registry, factory, aggregate, prepay)?;
        }

        // Nested packet roots can join only after the same Store parent has
        // been consumed and the continuation's pending barrier is cleared.
        super::live_outside::register_original_packet_destination_roots(
            &mut p.runtime_packet_parts,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            registry,
            prepay,
        )?;
        super::live_outside::require_original_packet_root_handoffs(
            &p.runtime_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Runtime,
        )?;
        super::live_outside::retire_original_packet_native_references(
            &mut p.runtime_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            prepay,
        )?;
        super::live_outside::retire_original_packet_metadata_pins(
            &mut p.runtime_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            prepay,
            &registry_identity,
            issue_metadata_pin_inputs,
        )?;
        super::live_outside::register_original_packet_destination_roots(
            &mut p.operational_packet_parts,
            super::live_outside::OriginalPacketSourceKind::Operational,
            registry,
            prepay,
        )?;
        super::live_outside::require_original_packet_root_handoffs(
            &p.operational_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Operational,
        )?;
        super::live_outside::retire_original_packet_native_references(
            &mut p.operational_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Operational,
            prepay,
        )?;
        super::live_outside::retire_original_packet_metadata_pins(
            &mut p.operational_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Operational,
            prepay,
            &registry_identity,
            issue_metadata_pin_inputs,
        )?;
        // A Store with no selected Scheduler database has no Scheduler packet
        // inventory; the same optional census applies on the failed-startup
        // path and in the pre-seal/transfer checks.
        if p.scheduler_packet_parts.is_some() {
            super::live_outside::register_original_packet_destination_roots(
                &mut p.scheduler_packet_parts,
                super::live_outside::OriginalPacketSourceKind::Scheduler,
                registry,
                prepay,
            )?;
            super::live_outside::require_original_packet_root_handoffs(
                &p.scheduler_packet_parts,
                factory,
                aggregate,
                super::live_outside::OriginalPacketSourceKind::Scheduler,
            )?;
            super::live_outside::retire_original_packet_native_references(
                &mut p.scheduler_packet_parts,
                factory,
                aggregate,
                super::live_outside::OriginalPacketSourceKind::Scheduler,
                prepay,
            )?;
            super::live_outside::retire_original_packet_metadata_pins(
                &mut p.scheduler_packet_parts,
                factory,
                aggregate,
                super::live_outside::OriginalPacketSourceKind::Scheduler,
                prepay,
                &registry_identity,
                issue_metadata_pin_inputs,
            )?;
        }
        let registry = p
            .physical_registry
            .as_mut()
            .context("actual Store physical registry absent before retirement permit")?;
        super::live_outside::prepare_original_packet_destination_retirements(
            &mut p.runtime_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            registry,
            prepay,
        )?;
        super::live_outside::prepare_original_packet_destination_retirements(
            &mut p.operational_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Operational,
            registry,
            prepay,
        )?;
        super::live_outside::prepare_original_packet_destination_retirements(
            &mut p.scheduler_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Scheduler,
            registry,
            prepay,
        )?;
        p.store_parent_finalization
            .as_mut()
            .context("actual Store finalization parts absent before native authority retirement")?
            .retire_native_authorities(registry, factory, aggregate, prepay)
    }

    /// # Safety
    /// The caller is the original settled-capacity finalizer. It has durably
    /// settled the account and this same retained registry has retired every
    /// non-operator descriptor. The canonical operator owner remains in the
    /// registry for its subsequent close stages.
    pub(super) unsafe fn retire_settled_exclusion_alias(
        &mut self,
        admission_alias: Option<&Arc<ryeos_node_runtime::state_lock::StateLockLease>>,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let StoreCloseProgress {
            physical_registry,
            store_parent_finalization,
            ..
        } = self.progress_mut();
        let registry = physical_registry
            .as_ref()
            .context("actual Store physical registry absent before exclusion-alias retirement")?;
        let parts = store_parent_finalization
            .as_mut()
            .context("actual Store finalization parts absent before exclusion-alias retirement")?;
        unsafe { parts.retire_settled_exclusion_alias(admission_alias, registry, prepay) }
    }

    fn advance_auxiliary_physical_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.inner.progress.auxiliary_physical_sources.is_none() {
            let sources =
                StoreAuxiliaryPhysicalSources::capture(&mut self.progress_mut().metadata)?;
            self.progress_mut().auxiliary_physical_sources = Some(sources);
        }
        let StoreCloseProgress {
            auxiliary_physical_sources,
            physical_registry,
            state_physical,
            ..
        } = self.progress_mut();
        let registry = physical_registry
            .as_mut()
            .context("actual Store physical registry absent")?;
        let sources = auxiliary_physical_sources
            .as_mut()
            .context("actual Store auxiliary source frame absent")?;
        sources.register(registry, prepay)?;
        let state = state_physical
            .as_mut()
            .context("actual State physical frame absent")?;
        sources.retire_recovery_aliases(state, registry, prepay)
    }

    #[cfg(target_os = "linux")]
    pub(super) unsafe fn register_operator_lock_alias(
        &mut self,
        owner: &Arc<lillux::ExactExclusiveFileLock>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        owner.require_original_process()?;
        if let Some(original) = self.inner.progress.metadata.original.as_ref() {
            anyhow::ensure!(
                original.factory.same_original(factory),
                "actual Store operator lock handoff changed factory"
            );
        } else {
            self.inner
                .progress
                .store_destination_join
                .get()
                .context("actual transferred Store receiver absent")?
                .require_original_origin(factory, aggregate)?;
        }
        super::live_outside::require_physical_registry(
            &self.inner.progress.physical_registry,
            factory,
            aggregate,
        )?;
        let p = self.progress_mut();
        let registry = p
            .physical_registry
            .as_mut()
            .context("actual Store physical registry absent")?;
        p.store_parent_parts
            .as_mut()
            .context("actual Store physical parent parts absent")?
            .register_exclusion(owner, factory, aggregate, registry, prepay)?;
        // register_exclusion records this exact canonical owner in the SAME
        // registry. Re-registering the same Arc here billed a duplicate scan
        // on every retry without adding an owner or retirement obligation.
        Ok(())
    }

    /// Register the actual completed Runtime frame in the SAME outside
    /// inventory. Parent ownership and the unsealed census remain retained.
    pub(super) unsafe fn register_runtime_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        if self.inner.progress.metadata.original.is_none() {
            self.advance_store_parent(factory, aggregate, prepay)?;
            // Parent transfer may have succeeded before an auxiliary
            // descriptor or RecoveryStore-alias handoff refused. Resume that
            // cached source frame on retry instead of treating parent
            // completion as completion of the whole Store census.
            return self.advance_auxiliary_physical_sources(factory, aggregate, prepay);
        }
        anyhow::ensure!(
            self.inner
                .progress
                .metadata
                .original()?
                .factory
                .same_original(factory),
            "live Runtime physical handoff changed actual Store factory"
        );
        super::live_outside::require_physical_registry(
            &self.inner.progress.physical_registry,
            factory,
            aggregate,
        )?;
        let StoreCloseProgress {
            metadata,
            state_closed,
            state_physical,
            runtime_outside,
            runtime_sql_custody,
            operational_sql_custody,
            runtime_packet_parts,
            operational_packet_parts,
            physical_registry,
            ..
        } = self.progress_mut();
        let registry = physical_registry
            .as_mut()
            .context("actual Store physical registry absent")?;
        let outside = runtime_outside
            .as_mut()
            .context("actual Runtime outside source absent")?;
        if runtime_sql_custody.is_none() {
            let quote = live_native::quote_store_packet_vector_handoff();
            registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
            // Retain the whole original vector in an inline slot before any
            // subsequent root effect. No packet or native affinity is discarded.
            let packets = unsafe { outside.take_original_sql_custody() }?;
            *runtime_sql_custody = Some(packets);
        }
        super::live_outside::prepare_original_packet_parts(
            runtime_sql_custody,
            runtime_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            prepay,
        )?;
        super::live_outside::register_original_packet_bound_roots(
            runtime_packet_parts,
            factory,
            aggregate,
            super::live_outside::OriginalPacketSourceKind::Runtime,
            registry,
            prepay,
        )?;
        if let Some(state) = state_physical.as_ref() {
            state.require_original(factory, aggregate)?;
            state.require_operational_registered(registry, prepay)?;
        } else {
            let state = state_closed
                .as_mut()
                .context("actual closed State source absent")?;
            if operational_sql_custody.is_none() {
                let quote = live_native::quote_store_packet_vector_handoff();
                registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
                let packets = unsafe { state.take_operational_sql_custody(factory, aggregate) }?;
                // Cache the entire vector before root registration can refuse.
                *operational_sql_custody = Some(packets);
            }
            super::live_outside::prepare_original_packet_parts(
                operational_sql_custody,
                operational_packet_parts,
                factory,
                aggregate,
                super::live_outside::OriginalPacketSourceKind::Operational,
                prepay,
            )?;
            super::live_outside::register_original_packet_bound_roots(
                operational_packet_parts,
                factory,
                aggregate,
                super::live_outside::OriginalPacketSourceKind::Operational,
                registry,
                prepay,
            )?;
            unsafe {
                state.register_operational_shared_roots(factory, aggregate, registry, prepay)
            }?;
        }
        // SAFETY: the whole same closed Store remains cached by its actual
        // outside supervisor; this original registry is not sealed here.
        unsafe { outside.register_shared_roots(factory, aggregate, registry, prepay) }?;
        // Success covers the actual Runtime continuation in this SAME registry,
        // not merely a successful registration attempt or its slot count.
        outside.require_shared_roots_registered(registry, prepay)?;
        if state_physical.is_none() {
            let source = state_closed
                .take()
                .context("actual closed State source absent")?;
            match source.into_physical_custody(factory, aggregate, registry, prepay) {
                Ok(frame) => *state_physical = Some(frame),
                Err((source, error)) => {
                    *state_closed = Some(source);
                    return Err(error);
                }
            }
        }
        let state = state_physical
            .as_mut()
            .context("actual State physical frame absent")?;
        state.require_original(factory, aggregate)?;
        // SAME complete frame is cached above. Actual native/destination
        // handoffs and both Runtime/Operational predicates preceded this step;
        // no service can acquire a new source after aggregate acknowledgement.
        unsafe { state.advance_parent(factory, aggregate, prepay) }?;
        unsafe { state.register_parent_roots(factory, aggregate, registry, prepay) }?;
        let quote = live_native::quote_store_child_parent_alias_retirement();
        registry.prepay_transfer(prepay, quote.bytes, quote.steps)?;
        let store: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> =
            metadata.original()?.clone();
        let lease: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
            metadata.original()?.clone();
        state.retire_parent_host_aliases(factory, aggregate, &store, &lease, registry, prepay)?;
        drop(store);
        drop(lease);
        self.advance_store_parent(factory, aggregate, prepay)?;
        self.advance_auxiliary_physical_sources(factory, aggregate, prepay)
    }
    /// Move the SAME actual Runtime frame only after the genuine aggregate.
    /// The whole outside frame is cached before native receipt consumption.
    fn advance_runtime_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        anyhow::ensure!(
            self.inner
                .progress
                .metadata
                .original()?
                .factory
                .same_original(factory),
            "live Runtime source handoff changed actual Store factory"
        );
        let StoreCloseProgress {
            runtime_closed,
            runtime_outside,
            ..
        } = self.progress_mut();
        if let Some(outside) = runtime_outside {
            return outside.advance_native_sources(factory, aggregate, prepay);
        }
        let source = runtime_closed
            .take()
            .context("actual closed Runtime source absent")?;
        match source.into_live_outside_custody(factory, aggregate, prepay) {
            Ok(outside) => *runtime_outside = Some(outside),
            Err((source, error)) => {
                *runtime_closed = Some(source);
                return Err(error);
            }
        }
        runtime_outside
            .as_mut()
            .context("actual live Runtime outside source absent")?
            .advance_native_sources(factory, aggregate, prepay)
    }
    pub(super) fn advance_state_projection_sources(
        &mut self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        if self.inner.progress.metadata.original.is_none() {
            return self
                .inner
                .progress
                .store_destination_join
                .get()
                .context("actual transferred Store receiver absent")?
                .require_original_origin(factory, terminal);
        }
        if let Some(state) = self.inner.progress.state_physical.as_ref() {
            state.require_original(factory, terminal)?;
            return self.advance_runtime_sources(factory, terminal, prepay);
        }
        let state = self
            .progress_mut()
            .state_closed
            .as_mut()
            .context("actual closed State absent")?;
        state.advance_projection_sources(factory, terminal, prepay)?;
        state.advance_schema_sources(factory, terminal, prepay)?;
        state.advance_destination_input_sources(factory, terminal, prepay)?;
        state.advance_operational_outside(factory, terminal, prepay)?;
        self.advance_runtime_sources(factory, terminal, prepay)
    }
    pub(super) fn prepare_destination_join(
        &self,
        factory: &Arc<OriginalNativeFactory>,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        terminal.require_original(factory)?;
        if self.inner.progress.metadata.original.is_none() {
            return self
                .inner
                .progress
                .store_destination_join
                .get()
                .context("actual transferred Store receiver absent")?
                .require_original_origin(factory, terminal);
        }
        anyhow::ensure!(
            self.inner
                .progress
                .metadata
                .original()?
                .factory
                .same_original(factory),
            "closed Store selected foreign factory"
        );
        super::live_destination_join::prepare_store_destination_join(
            &self.inner.progress.store_destination_join,
            self.inner.progress.metadata.original()?,
            factory,
            terminal,
            prepay,
        )?;
        if let Some(state) = self.inner.progress.state_physical.as_ref() {
            return state.require_original(factory, terminal);
        }
        let state = self
            .inner
            .progress
            .state_closed
            .as_ref()
            .context("actual closed State absent")?;
        let parent: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> =
            self.inner.progress.metadata.original()?.clone();
        state.require_original_store_parent(&parent, factory)?;
        state.prepare_destination_join(factory, terminal, prepay)
    }
    /// Transfer the SAME original live graph only after genuine same-factory
    /// native acknowledgement. All State, Runtime, authority and physical
    /// holders remain owned; this is not descriptor retirement or settlement.
    pub fn into_outside_custody(
        self,
        terminal: &lillux_sqlite::OriginalNativeTerminal,
    ) -> std::result::Result<super::LiveStateStoreOutsideCustody, Self> {
        let Some(original) = self.inner.progress.metadata.original.as_ref() else {
            return Err(self);
        };
        if terminal.require_original(&original.factory).is_err() {
            return Err(self);
        }
        Ok(super::live_outside::LiveStateStoreOutsideCustody::closed(
            self,
        ))
    }

    pub fn generation_lease_terminals(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>> {
        self.native_terminals(factory)?;
        self.inner
            .progress
            .state_closed
            .as_ref()
            .context("actual closed State absent")?
            .generation_lease_terminals()
    }
    pub fn prior_refusal(&self) -> Option<&anyhow::Error> {
        self.inner.prior_refusal.as_ref()
    }
    pub fn native_terminals(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        factory.require_original_process()?;
        anyhow::ensure!(
            self.inner.progress.runtime_outside.is_none(),
            "Runtime source already transferred after aggregate acknowledgement"
        );
        self.inner
            .progress
            .metadata
            .original()?
            .require_close_factory(factory)?;
        let mut terminals = self
            .inner
            .progress
            .runtime_closed
            .as_ref()
            .context("live Runtime terminal absent")?
            .native_terminals()?;
        terminals.extend(
            self.inner
                .progress
                .state_closed
                .as_ref()
                .context("live State terminal absent")?
                .native_terminals()?,
        );
        for terminal in &terminals {
            terminal.require_original(factory)?;
        }
        Ok(terminals)
    }
}
/// Native census of a Store closed in process: what an in-process host
/// records before it finishes its backend.
pub struct StateStoreCensus {
    pub terminals: Vec<OriginalNativeConnectionTerminal>,
    pub leases: Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
    /// The generation lease owners behind `leases`, to retire against the
    /// finished native backend before the host releases.
    pub lease_retirements: Vec<ryeos_state::state_db::GenerationLeaseRetirement>,
    /// The refusal an earlier close attempt recorded before this one completed.
    pub prior_refusal: Option<anyhow::Error>,
}

impl ClosedLiveStateStore {
    /// End a closed Store whose sources never left it, yielding its native
    /// census: the Runtime terminals and the State census. The roots and
    /// namespace end with this call. A Store that began any outside handoff
    /// is refused whole; a refused State census retains the close progress
    /// for [`LiveStateStoreCloseFailure::retry`].
    pub fn into_census(
        mut self,
        factory: Arc<OriginalNativeFactory>,
    ) -> std::result::Result<StateStoreCensus, LiveStateStoreCloseFailure> {
        let checked = (|| -> Result<Vec<OriginalNativeConnectionTerminal>> {
            factory.require_original_process()?;
            self.inner
                .progress
                .metadata
                .original()?
                .require_close_factory(&factory)?;
            let p = &self.inner.progress;
            anyhow::ensure!(
                p.store_parent.is_none()
                    && p.store_parent_parts.is_none()
                    && p.store_parent_finalization.is_none()
                    && p.store_destination_join.get().is_none()
                    && p.runtime_packet_parts.is_none()
                    && p.operational_packet_parts.is_none()
                    && p.scheduler_packet_parts.is_none()
                    && p.scheduler_unopened_database.is_none()
                    && p.scheduler_unopened_schema_reference.is_none()
                    && p.accounting_unopened_schema_reference.is_none()
                    && p.scheduler_admission_sources.is_none()
                    && p.operational_sql_custody.is_none()
                    && p.runtime_sql_custody.is_none()
                    && p.physical_registry.is_none()
                    && p.auxiliary_physical_sources.is_none()
                    && p.runtime.is_none()
                    && p.runtime_failure.is_none()
                    && p.runtime_outside.is_none()
                    && p.state.is_none()
                    && p.state_failure.is_none()
                    && p.state_physical.is_none()
                    && p.state_closed.is_some(),
                "closed Store began an outside source handoff"
            );
            let terminals = p
                .runtime_closed
                .as_ref()
                .context("live Runtime terminal absent")?
                .native_terminals()?;
            for terminal in &terminals {
                terminal.require_original(&factory)?;
            }
            Ok(terminals)
        })();
        let runtime_terminals = match checked {
            Ok(terminals) => terminals,
            Err(error) => {
                return Err(LiveStateStoreCloseFailure::new(
                    error,
                    factory,
                    StoreCloseCustody::Closed(self),
                ));
            }
        };
        let state_closed = self
            .progress_mut()
            .state_closed
            .take()
            .expect("checked closed State present");
        let state_census = match state_closed.into_census(Arc::clone(&factory)) {
            Ok(census) => census,
            Err(failure) => {
                let error = anyhow!("in-process State census refused: {failure}");
                self.progress_mut().state_failure = Some(failure);
                let (progress, prior_refusal) = match self.into_parts() {
                    Ok(parts) => parts,
                    Err(this) => {
                        return Err(LiveStateStoreCloseFailure::new(
                            error,
                            factory,
                            StoreCloseCustody::Closed(this),
                        ));
                    }
                };
                return Err(LiveStateStoreCloseFailure::new(
                    prior_refusal.unwrap_or(error),
                    factory,
                    StoreCloseCustody::Progress(progress.held()),
                ));
            }
        };
        let (progress, prior_refusal) = match self.into_parts() {
            Ok(parts) => parts,
            Err(this) => {
                return Err(LiveStateStoreCloseFailure::new(
                    anyhow!("closed Store belongs to another process"),
                    factory,
                    StoreCloseCustody::Closed(this),
                ));
            }
        };
        let StoreCloseProgress {
            store_parent,
            store_parent_parts,
            store_parent_finalization,
            store_destination_join,
            runtime_packet_parts,
            operational_packet_parts,
            scheduler_packet_parts,
            scheduler_unopened_database,
            scheduler_unopened_schema_reference,
            accounting_unopened_schema_reference,
            scheduler_admission_sources,
            operational_sql_custody,
            runtime_sql_custody,
            physical_registry,
            auxiliary_physical_sources,
            metadata,
            runtime,
            runtime_failure,
            runtime_closed,
            runtime_outside,
            state,
            state_failure,
            state_closed,
            state_physical,
        } = progress;
        // Every outside slot was checked empty above; the census holds every
        // native owner, and the Runtime close context ends with its roots.
        drop(store_parent);
        drop(store_parent_parts);
        drop((
            store_parent_finalization,
            store_destination_join,
            runtime_packet_parts,
            operational_packet_parts,
            scheduler_packet_parts,
            scheduler_unopened_database,
            scheduler_unopened_schema_reference,
            accounting_unopened_schema_reference,
            scheduler_admission_sources,
            operational_sql_custody,
            runtime_sql_custody,
            physical_registry,
            auxiliary_physical_sources,
        ));
        drop((runtime, runtime_failure, runtime_closed, runtime_outside));
        drop((state, state_failure, state_closed, state_physical));
        let StoreCloseMetadata {
            original,
            authority,
            _thread_authority,
            _projection_health,
            _mode: _,
            _barrier,
            _attachment_gate: _,
            _attachment_closed,
            _attached,
            _live_workers,
            _selected,
            _launches,
            _handlers,
            _signer,
            _active_launch_owners,
        } = metadata;
        drop((
            original,
            authority,
            _thread_authority,
            _projection_health,
            _barrier,
            _attachment_closed,
            _attached,
            _live_workers,
            _selected,
            _launches,
            _handlers,
            _signer,
            _active_launch_owners,
        ));
        let ryeos_state::state_db::LiveStateCensus {
            terminals: state_terminals,
            leases,
            lease_retirement,
            prior_refusal: state_prior_refusal,
        } = state_census;
        let mut terminals = runtime_terminals;
        terminals.extend(state_terminals);
        Ok(StateStoreCensus {
            terminals,
            leases,
            lease_retirements: vec![lease_retirement],
            prior_refusal: prior_refusal.or(state_prior_refusal),
        })
    }
}

enum StoreCloseCustody {
    Shared(Arc<StateStore>),
    Store(StateStore),
    Progress(lillux::custody::Held<StoreCloseProgress>),
    Closed(ClosedLiveStateStore),
}

/// No drop settles a refused close: the custody always holds the live Store,
/// its close progress or its closed frame, each owning the namespace
/// exclusion and native owners that only a retried close, the returned Store
/// or a census may end. Constructible only in this module.
struct LiveStateStoreCloseFailureParts {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    custody: Mutex<StoreCloseCustody>,
}
pub struct LiveStateStoreCloseFailure {
    inner: lillux::custody::Held<LiveStateStoreCloseFailureParts>,
}
impl LiveStateStoreCloseFailure {
    fn new(
        error: anyhow::Error,
        factory: Arc<OriginalNativeFactory>,
        custody: StoreCloseCustody,
    ) -> Self {
        Self {
            inner: lillux::custody::Held::until_taken(
                STORE_CLOSE_FAILURE_KIND,
                LiveStateStoreCloseFailureParts {
                    error,
                    factory,
                    custody: Mutex::new(custody),
                },
            ),
        }
    }
    /// Move every field out; a copy in another process is refused and comes
    /// back still held.
    fn into_parts(
        self,
    ) -> std::result::Result<
        (
            anyhow::Error,
            Arc<OriginalNativeFactory>,
            Mutex<StoreCloseCustody>,
        ),
        Self,
    > {
        self.inner
            .take_whole()
            .map(|parts| {
                let LiveStateStoreCloseFailureParts {
                    error,
                    factory,
                    custody,
                } = parts;
                (error, factory, custody)
            })
            .map_err(|inner| Self { inner })
    }
}
impl std::fmt::Debug for LiveStateStoreCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStateStoreCloseFailure")
            .field("error", &self.inner.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for LiveStateStoreCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "live Store close retained: {:#}", self.inner.error)
    }
}
impl std::error::Error for LiveStateStoreCloseFailure {}
impl LiveStateStoreCloseFailure {
    /// A refused pre-close audit returns the SAME Store to its supervisor so
    /// existing execution/recovery obligations can finish. Once either native
    /// owner has transferred, only the retained close retry is permitted.
    pub fn into_unstarted(self) -> std::result::Result<Arc<StateStore>, Self> {
        if self.inner.is_foreign() || self.inner.factory.require_original_process().is_err() {
            return Err(self);
        }
        let (error, factory, custody) = self.into_parts()?;
        let custody = custody.into_inner().unwrap_or_else(|p| p.into_inner());
        match custody {
            StoreCloseCustody::Shared(store) => Ok(store),
            StoreCloseCustody::Store(store) => Ok(Arc::new(store)),
            custody => Err(Self::new(error, factory, custody)),
        }
    }

    pub fn retry(self) -> std::result::Result<ClosedLiveStateStore, Self> {
        if self.inner.is_foreign() || self.inner.factory.require_original_process().is_err() {
            return Err(self);
        }
        let (error, factory, custody) = self.into_parts()?;
        advance_close(
            factory,
            custody.into_inner().unwrap_or_else(|p| p.into_inner()),
            Some(error),
        )
    }
}
fn advance_close(
    factory: Arc<OriginalNativeFactory>,
    custody: StoreCloseCustody,
    prior: Option<anyhow::Error>,
) -> std::result::Result<ClosedLiveStateStore, LiveStateStoreCloseFailure> {
    let retain =
        |error, custody| LiveStateStoreCloseFailure::new(error, Arc::clone(&factory), custody);
    if let Err(error) = factory.require_original_process() {
        return Err(retain(error, custody));
    }
    let custody = match custody {
        StoreCloseCustody::Shared(shared) => match Arc::try_unwrap(shared) {
            Ok(store) => StoreCloseCustody::Store(store),
            Err(shared) => {
                return Err(retain(
                    prior.unwrap_or_else(|| anyhow!("live Store still has service owners")),
                    StoreCloseCustody::Shared(shared),
                ));
            }
        },
        other => other,
    };
    let progress = match custody {
        StoreCloseCustody::Store(store) => {
            let checked = (|| -> Result<()> {
                store
                    .live_native_open
                    .as_ref()
                    .context("actual live Store opening owner absent")?
                    .require_close_factory(&factory)?;
                anyhow::ensure!(
                    !store.process_attachment_admission_is_open(),
                    "live Store execution admission remains open"
                );
                anyhow::ensure!(
                    store
                        .attached_processes
                        .lock()
                        .map_err(|_| anyhow!("attachment registry poisoned"))?
                        .is_empty(),
                    "live attachments remain"
                );
                anyhow::ensure!(
                    !store
                        .selected_invocations
                        .lock()
                        .map_err(|_| anyhow!("selected registry poisoned"))?
                        .has_any(),
                    "selected invocation obligations remain"
                );
                anyhow::ensure!(
                    store
                        .launch_task_abort_handles
                        .lock()
                        .map_err(|_| anyhow!("launch registry poisoned"))?
                        .is_empty(),
                    "launch task owners remain"
                );
                anyhow::ensure!(
                    store
                        .active_in_process_handlers
                        .lock()
                        .map_err(|_| anyhow!("handler registry poisoned"))?
                        .is_empty(),
                    "in-process handlers remain"
                );
                anyhow::ensure!(
                    store
                        .inner
                        .lock()
                        .map_err(|_| anyhow!("Store owner poisoned"))?
                        .active_launch_owners
                        .is_empty(),
                    "active launch obligations remain"
                );
                Ok(())
            })();
            if let Err(error) = checked {
                return Err(retain(
                    prior.unwrap_or(error),
                    StoreCloseCustody::Store(store),
                ));
            }
            let StateStore {
                live_native_open,
                state_authority,
                bundle_admission_trust,
                thread_runtime_authority,
                inner,
                projection_health,
                read_only,
                allow_projection_rebuild,
                write_barrier,
                process_attachment_admission_open,
                process_attachment_admission_closed,
                attached_processes,
                live_workers,
                selected_invocations,
                launch_task_abort_handles,
                active_in_process_handlers,
            } = store;
            // Admitting trust is an immutable signer snapshot with no native
            // custody; it ends with the Store's admission surface.
            drop(bundle_admission_trust);
            // The sole Store has passed its actual ownership audit. No Store
            // retention/SQL mutex is held through the consuming close below.
            let Inner {
                state_db,
                runtime_db,
                signer,
                active_launch_owners,
            } = inner.into_inner().unwrap_or_else(|p| p.into_inner());
            StoreCloseProgress {
                store_parent: None,
                store_parent_parts: None,
                store_parent_finalization: None,
                store_destination_join: std::sync::OnceLock::new(),
                runtime_packet_parts: None,
                operational_packet_parts: None,
                scheduler_packet_parts: None,
                scheduler_unopened_database: None,
                scheduler_unopened_schema_reference: None,
                accounting_unopened_schema_reference: None,
                scheduler_admission_sources: None,
                operational_sql_custody: None,
                runtime_sql_custody: None,
                physical_registry: None,
                auxiliary_physical_sources: None,
                metadata: StoreCloseMetadata {
                    original: Some(live_native_open.expect("validated actual live opening owner")),
                    authority: Some(state_authority),
                    _thread_authority: thread_runtime_authority,
                    _projection_health: projection_health,
                    _mode: (read_only, allow_projection_rebuild),
                    _barrier: write_barrier,
                    _attachment_gate: process_attachment_admission_open,
                    _attachment_closed: process_attachment_admission_closed,
                    _attached: attached_processes,
                    _live_workers: live_workers,
                    _selected: selected_invocations,
                    _launches: launch_task_abort_handles,
                    _handlers: active_in_process_handlers,
                    _signer: signer,
                    _active_launch_owners: active_launch_owners,
                },
                runtime: Some(runtime_db),
                runtime_failure: None,
                runtime_closed: None,
                runtime_outside: None,
                state: Some(state_db),
                state_failure: None,
                state_closed: None,
                state_physical: None,
            }
            .held()
        }
        StoreCloseCustody::Progress(progress) => progress,
        StoreCloseCustody::Closed(closed) => {
            if let Err(error) = closed.native_terminals(&factory) {
                return Err(retain(error, StoreCloseCustody::Closed(closed)));
            }
            return Ok(closed);
        }
        StoreCloseCustody::Shared(_) => unreachable!("shared custody resolved before native close"),
    };
    // An unwind through the close retains the progress whole, counted.
    let mut progress = progress;
    if let Err(error) = progress
        .metadata
        .original()
        .and_then(|original| original.require_close_factory(&factory))
    {
        return Err(retain(
            prior.unwrap_or(error),
            StoreCloseCustody::Progress(progress),
        ));
    }
    // Resolve the real CAS guard before taking either SQL owner, preserving
    // both unchanged if the exact existing anchor cannot be acquired.
    let guard = match progress
        .metadata
        .authority
        .as_ref()
        .context("actual Store pinned authority absent before close")
        .and_then(|authority| authority.acquire_exclusive_guard(false))
    {
        Ok(guard) => guard,
        Err(error) => {
            return Err(retain(
                prior.unwrap_or(error),
                StoreCloseCustody::Progress(progress),
            ));
        }
    };
    if let Some(runtime) = progress.get_mut().runtime.take() {
        match runtime.close_original_runtime(Arc::clone(&factory)) {
            Ok(closed) => progress.get_mut().runtime_closed = Some(closed),
            Err(failure) => progress.get_mut().runtime_failure = Some(failure),
        }
    } else if let Some(failure) = progress.get_mut().runtime_failure.take() {
        match failure.retry() {
            Ok(closed) => progress.get_mut().runtime_closed = Some(closed),
            Err(failure) => progress.get_mut().runtime_failure = Some(failure),
        }
    }
    if progress.runtime_failure.is_some() {
        return Err(retain(
            prior.unwrap_or_else(|| anyhow!("live Runtime close pending")),
            StoreCloseCustody::Progress(progress),
        ));
    }
    if let Some(state) = progress.get_mut().state.take() {
        match state.close_live(Arc::clone(&factory), &guard) {
            Ok(closed) => progress.get_mut().state_closed = Some(closed),
            Err(failure) => progress.get_mut().state_failure = Some(failure),
        }
    } else if let Some(failure) = progress.get_mut().state_failure.take() {
        match failure.retry(&guard) {
            Ok(closed) => progress.get_mut().state_closed = Some(closed),
            Err(failure) => progress.get_mut().state_failure = Some(failure),
        }
    }
    if progress.state_failure.is_some() {
        return Err(retain(
            prior.unwrap_or_else(|| anyhow!("live State close pending")),
            StoreCloseCustody::Progress(progress),
        ));
    }
    // The progress was held only after the original process was confirmed.
    let closed = ClosedLiveStateStore::new(live_native::take_held(progress), prior);
    if let Err(error) = closed.native_terminals(&factory) {
        return Err(retain(error, StoreCloseCustody::Closed(closed)));
    }
    Ok(closed)
}

#[cfg(test)]
mod finalization_source_tests {
    use super::*;

    #[test]
    fn namespace_source_remains_owned_when_precharge_refuses() {
        let directory = tempfile::tempdir().unwrap();
        let pinned = lillux::PinnedDirectory::open(directory.path())
            .unwrap()
            .expect("temporary namespace exists");
        let lock = pinned.lock_exclusive().unwrap();
        let mut source = AuxiliaryNamespaceSource {
            process: std::process::id(),
            required: true,
            owner: Some(lock),
            registration: None,
        };
        let mut registry = unsafe {
            lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::issued(
                1,
                &|_, _| Ok(()),
            )
        }
        .unwrap();

        let refused = source.register(&mut registry, &|_, _| {
            Err(anyhow!("test outside funding refusal"))
        });

        assert!(refused.is_err());
        assert!(source.owner.is_some());
        assert!(source.registration.is_none());
        assert!(pinned.try_lock_exclusive().unwrap().is_none());
    }
}
impl StateStore {
    /// Consume the actual Store only after execution/services have drained and
    /// attachment admission has closed. Arc uniqueness alone is insufficient.
    pub fn close_live(
        self: Arc<Self>,
        factory: Arc<OriginalNativeFactory>,
    ) -> std::result::Result<ClosedLiveStateStore, LiveStateStoreCloseFailure> {
        advance_close(factory, StoreCloseCustody::Shared(self), None)
    }
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const CLOSED: &str = CLOSED_STORE_KIND;
    const FAILURE: &str = STORE_CLOSE_FAILURE_KIND;

    fn abandoned(kind: &str) -> usize {
        lillux::custody::abandoned_custody().count(kind)
    }

    #[test]
    fn dropped_closed_store_is_retained() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_closed_store_is_retained");
        // The retained frame keeps the Store's namespace exclusion and native
        // context, which only the process end releases, so the scenario runs
        // in its own child process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "closed +1", "{outcome}");
            return;
        }
        let app_root = tempfile::tempdir().unwrap();
        let (closed, retained) = super::super::test_native::closed_store_for_test(app_root.path());
        assert!(closed.require_exhausted_holders().is_err());
        let before = abandoned(CLOSED);
        drop(closed);
        let delta = abandoned(CLOSED) - before;
        lillux_sqlite::test_host::end_isolated_process(
            (app_root, retained),
            &format!("closed +{delta}"),
        )
    }

    #[test]
    fn dropped_close_failure_is_retained_whole() {
        let _serial = ryeos_state::test_support::abandoned_custody_serial();
        const TEST: &str = concat!(module_path!(), "::dropped_close_failure_is_retained_whole");
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "failure +1; closed +0", "{outcome}");
            return;
        }
        let app_root = tempfile::tempdir().unwrap();
        let (closed, retained) = super::super::test_native::closed_store_for_test(app_root.path());
        let failure = LiveStateStoreCloseFailure::new(
            anyhow!("census refused"),
            Arc::clone(&retained.0),
            StoreCloseCustody::Closed(closed),
        );
        let before = (abandoned(FAILURE), abandoned(CLOSED));
        drop(failure);
        let after = (abandoned(FAILURE), abandoned(CLOSED));
        lillux_sqlite::test_host::end_isolated_process(
            (app_root, retained),
            &format!(
                "failure +{}; closed +{}",
                after.0 - before.0,
                after.1 - before.1
            ),
        )
    }
}
