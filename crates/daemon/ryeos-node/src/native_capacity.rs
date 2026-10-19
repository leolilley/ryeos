//! Authentic inputs for the live native-capacity producer.
//!
//! A parsed policy, a public init report or a node key alone is insufficient.
//! Capture verifies the installed operator fence and the complete node-signed
//! policy generation under the actual state exclusion. It retains the original
//! input descriptors and samples the original clock before any capacity debit.
//! This owner does not issue a budget: durable account reservation and actual
//! native lifecycle ownership are separate, required producer steps.

use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use lillux::custody::Held;
use lillux::{OpenRegularFileObservation, PinnedDirectory, PinnedRegularFile};
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::TrustStore;
use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
use ryeos_node_runtime::node_policy::{NodePolicySnapshot, NodePolicyTable};
use ryeos_node_runtime::state_lock::StateLockLease;

mod account;
mod factory;
mod late;
mod reservation;

/// A daemon composition may make at most this many complete native-finalizer
/// passes across its initial shutdown, asynchronous retry, synchronous hold,
/// and Drop fallback. The reservation quote and the live caller share this
/// bound so cleanup funding cannot be sized for a different retry count.
pub const MAX_FINALIZATION_ATTEMPTS: u8 = 4;

pub use account::NativeCapacityAccountBinding;
pub(crate) use account::load_existing_capacity_binding;
pub(crate) use account::prepare_initial_capacity_binding;
pub use account::reissue::{
    CapacityReissueEvidence, ReissueReport, ReissueRequest, read_capacity_reissuances,
    reissue_capacity,
};
pub use account::{CapacityAccountSummary, inspect_capacity_account};
pub use factory::{
    LiveNativeFactory, LiveNativeFactoryFailure, LiveNativeProducerPreparationFailure,
    LiveNativeProducerPublicationFailure, LiveProducerControlCensus, ServiceRefusalLatch,
    ServiceRefusalWake, service_refusal,
};
#[cfg(target_os = "linux")]
pub use reservation::OperatorLockCustody;
pub use reservation::RolloverClaim;
pub use reservation::{
    FinalizedLiveNative, LiveNativeCapacityReservation, LiveNativeCapacityReservationFailure,
    SettledLiveNative, quote_operator_lock_handoff, required_cleanup_occupancy,
    required_rollover_drain_margin,
};

const MAX_COMPLETION_BYTES: u64 = 256 * 1024;

/// The original no-follow input and its original parent. A retained descriptor
/// comparison refuses namespace replacement; no later pathname is reopened.
struct RetainedInput {
    parent: PinnedDirectory,
    file: PinnedRegularFile,
    descriptor: File,
    observation: OpenRegularFileObservation,
    bytes: Vec<u8>,
}

impl RetainedInput {
    fn capture(parent: PinnedDirectory, name: &OsStr, maximum: u64) -> Result<Self> {
        parent.ensure_path_binding()?;
        let file = parent
            .open_pinned_regular(name, false)?
            .context("required native admission input is absent")?;
        let descriptor = file.try_clone_descriptor()?;
        let owner = lillux::require_effective_user_owned_regular(&descriptor)?;
        file.require_owner_fixed(owner.owner())
            .map_err(anyhow::Error::msg)?;
        parent
            .require_regular_entry_descriptor_fixed(name, &descriptor)
            .map_err(anyhow::Error::msg)?;
        let observation = file.observation()?;
        let bytes = file.read_stable_bounded(&observation, maximum)?;
        let result = Self {
            parent,
            file,
            descriptor,
            observation,
            bytes,
        };
        result.require_original()?;
        Ok(result)
    }

    fn require_original(&self) -> Result<()> {
        self.parent
            .ensure_path_binding_fixed()
            .map_err(anyhow::Error::msg)?;
        self.parent
            .require_regular_entry_descriptor_fixed(self.file.name(), &self.descriptor)
            .map_err(anyhow::Error::msg)?;
        lillux::ensure_open_regular_file_unchanged(&self.descriptor, &self.observation)?;
        let owner = lillux::require_effective_user_owned_regular(&self.descriptor)?;
        self.file
            .require_owner_fixed(owner.owner())
            .map_err(anyhow::Error::msg)
    }
}

/// Verified issuance inputs only. There is deliberately no conversion from a
/// policy DTO, public report, decoded journal or caller-supplied clock. Nor can
/// this object be converted into a prepaid native budget without the genuine
/// capacity-account producer's durable debit.
pub struct LiveNativeCapacityInputs {
    root: PinnedDirectory,
    runtime: PinnedDirectory,
    exclusion: Arc<StateLockLease>,
    completion: RetainedInput,
    policy_file: RetainedInput,
    policy: NodeNativeCapacityPolicy,
    snapshot: NodePolicySnapshot,
    node_fingerprint: String,
    // The operator key that signed the issuance; an operator re-issuance
    // document found at open must verify under exactly this key.
    operator_fingerprint: String,
    capacity_account: NativeCapacityAccountBinding,
    // Inputs and the prepaid late owner share this ONE captured clock. A late
    // owner must carry no input/account/lease backedge; terminal close requires
    // actual sole ownership after those inputs have been consumed.
    clock: Arc<lillux::time::RetainedOccupancyClock>,
    occupancy: lillux::time::OccupancyLimit,
    sql_deadline: lillux::time::MonotonicDeadline,
    cleanup_deadline: lillux::time::MonotonicDeadline,
    // In-memory BOOTTIME edge of the same original service window, checked
    // per native VM step; the full window observation runs at boundaries.
    service_edge: lillux::time::OccupancyServiceEdge,
    process: lillux::process_control::ExactProcessIdentity,
}

/// What a directory source holds until its same-registry membership is
/// cached. Settled exactly when the directory owner moved into the registry:
/// the membership alone remains and drops normally. Otherwise the original
/// directory and any membership stay retained together until the process
/// exits.
pub(super) struct DirectorySourceParts {
    owner: Option<Arc<PinnedDirectory>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}

/// Exact original directory until its same-registry membership is cached.
/// No path or identity value can construct this source.
pub(super) struct FinalizerDirectorySource {
    held: Held<DirectorySourceParts>,
}
impl FinalizerDirectorySource {
    pub(super) fn retained(owner: Arc<PinnedDirectory>) -> Self {
        const KIND: &str = "ryeos-node::native-capacity-finalizer-directory";
        Self {
            held: Held::new(
                KIND,
                DirectorySourceParts {
                    owner: Some(owner),
                    registration: None,
                },
                |parts| parts.owner.is_none(),
            ),
        }
    }

    /// True once the directory owner moved into a registry membership; only
    /// the plain registration, if any, remains.
    pub(super) fn owner_transferred(&self) -> bool {
        self.held.owner.is_none()
    }

    /// Move the open directory owner out, leaving the source without one.
    pub(super) fn take_owner(&mut self) -> Option<Arc<PinnedDirectory>> {
        self.held.get_mut().owner.take()
    }

    /// Put an owner back into an empty slot; an occupied slot refuses and
    /// hands the offered owner back, so nothing is replaced.
    pub(super) fn restore_owner(
        &mut self,
        owner: Arc<PinnedDirectory>,
    ) -> std::result::Result<(), Arc<PinnedDirectory>> {
        let parts = self.held.get_mut();
        if parts.owner.is_some() {
            return Err(owner);
        }
        parts.owner = Some(owner);
        Ok(())
    }

    /// Record the registry membership in an empty slot; an occupied slot
    /// refuses and hands the offered membership back.
    pub(super) fn set_registration(
        &mut self,
        registration: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    ) -> std::result::Result<
        (),
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    > {
        let parts = self.held.get_mut();
        if parts.registration.is_some() {
            return Err(registration);
        }
        parts.registration = Some(registration);
        Ok(())
    }
}
impl std::ops::Deref for FinalizerDirectorySource {
    type Target = DirectorySourceParts;
    fn deref(&self) -> &DirectorySourceParts {
        &self.held
    }
}

/// What a control source holds during the post-settlement registry join.
/// Settled exactly when the open source moved on: the close owner and
/// membership carry their own disposal and drop normally. Otherwise the open
/// descriptor, any close owner and any membership stay retained together
/// until the process exits.
pub(super) struct ControlSourceParts {
    pinned_file: Option<PinnedRegularFile>,
    duplicate: Option<File>,
    owner: Option<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}

/// Actual account/input descriptor during the post-settlement registry join.
/// Each transition keeps either the authentic open source, its close owner, or
/// its exact registry membership until the next consuming stage succeeds.
pub(super) struct FinalizerControlSource {
    held: Held<ControlSourceParts>,
}
impl FinalizerControlSource {
    fn held(parts: ControlSourceParts) -> Self {
        const KIND: &str = "ryeos-node::native-capacity-finalizer-control";
        Self {
            held: Held::new(KIND, parts, |parts| {
                parts.pinned_file.is_none() && parts.duplicate.is_none()
            }),
        }
    }
    pub(super) fn pinned_file(file: PinnedRegularFile) -> Self {
        Self::held(ControlSourceParts {
            pinned_file: Some(file),
            duplicate: None,
            owner: None,
            registration: None,
        })
    }
    pub(super) fn duplicate(file: File) -> Self {
        Self::held(ControlSourceParts {
            pinned_file: None,
            duplicate: Some(file),
            owner: None,
            registration: None,
        })
    }

    /// True once the open source moved into its close owner or registry
    /// membership; neither the original pin nor the duplicate remains.
    pub(super) fn source_transferred(&self) -> bool {
        self.held.pinned_file.is_none() && self.held.duplicate.is_none()
    }

    /// Move the original pinned descriptor out.
    pub(super) fn take_pinned_file(&mut self) -> Option<PinnedRegularFile> {
        self.held.get_mut().pinned_file.take()
    }

    /// Move the captured duplicate descriptor out.
    pub(super) fn take_duplicate(&mut self) -> Option<File> {
        self.held.get_mut().duplicate.take()
    }

    /// Move the close owner out.
    pub(super) fn take_owner(
        &mut self,
    ) -> Option<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>> {
        self.held.get_mut().owner.take()
    }

    /// Put a close owner into an empty slot; an occupied slot refuses and
    /// hands the offered owner back, so nothing is replaced.
    pub(super) fn set_owner(
        &mut self,
        owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> std::result::Result<(), Arc<lillux_sqlite::RetainedOriginalFinalizerFile>> {
        let parts = self.held.get_mut();
        if parts.owner.is_some() {
            return Err(owner);
        }
        parts.owner = Some(owner);
        Ok(())
    }

    /// Record the registry membership in an empty slot; an occupied slot
    /// refuses and hands the offered membership back.
    pub(super) fn set_registration(
        &mut self,
        registration: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    ) -> std::result::Result<
        (),
        lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    > {
        let parts = self.held.get_mut();
        if parts.registration.is_some() {
            return Err(registration);
        }
        parts.registration = Some(registration);
        Ok(())
    }
}
impl std::ops::Deref for FinalizerControlSource {
    type Target = ControlSourceParts;
    fn deref(&self) -> &ControlSourceParts {
        &self.held
    }
}

/// The slots of the input finalization frame, in release order. Settled
/// exactly when consumed: nothing remains to drop. Otherwise every retained
/// source and alias stays retained together until the process exits.
pub(super) struct InputSourcesParts {
    root: Option<FinalizerDirectorySource>,
    runtime: Option<FinalizerDirectorySource>,
    completion_parent: Option<FinalizerDirectorySource>,
    completion_file: Option<FinalizerControlSource>,
    completion_duplicate: Option<FinalizerControlSource>,
    policy_parent: Option<FinalizerDirectorySource>,
    policy_file: Option<FinalizerControlSource>,
    policy_duplicate: Option<FinalizerControlSource>,
    exclusion_alias: Option<Arc<StateLockLease>>,
    clock_alias: Option<Arc<lillux::time::RetainedOccupancyClock>>,
}
impl InputSourcesParts {
    fn consumed(&self) -> bool {
        self.root.is_none()
            && self.runtime.is_none()
            && self.completion_parent.is_none()
            && self.completion_file.is_none()
            && self.completion_duplicate.is_none()
            && self.policy_parent.is_none()
            && self.policy_file.is_none()
            && self.policy_duplicate.is_none()
            && self.exclusion_alias.is_none()
            && self.clock_alias.is_none()
    }
}

/// Original admission descriptors after account settlement. These affine
/// fields are handed to the same preallocated registry as the Store graph;
/// the held frame keeps any source that has not yet registered from being
/// mistaken for an observed descriptor close during refusal handling.
pub(super) struct NativeCapacityInputFinalizationSources {
    held: Held<InputSourcesParts>,
}
impl NativeCapacityInputFinalizationSources {
    fn held(parts: InputSourcesParts) -> Self {
        const KIND: &str = "ryeos-node::native-capacity-input-sources";
        Self {
            held: Held::new(KIND, parts, InputSourcesParts::consumed),
        }
    }

    /// True once every source and both aliases moved on: the frame holds
    /// nothing, which is what final consumption requires of it.
    pub(super) fn consumed(&self) -> bool {
        self.held.consumed()
    }

    /// The four directory slots, in registration order.
    pub(super) fn directory_slots(&mut self) -> [&mut Option<FinalizerDirectorySource>; 4] {
        let parts = self.held.get_mut();
        [
            &mut parts.root,
            &mut parts.runtime,
            &mut parts.completion_parent,
            &mut parts.policy_parent,
        ]
    }

    /// The four control slots, in registration order.
    pub(super) fn control_slots(&mut self) -> [&mut Option<FinalizerControlSource>; 4] {
        let parts = self.held.get_mut();
        [
            &mut parts.completion_file,
            &mut parts.completion_duplicate,
            &mut parts.policy_file,
            &mut parts.policy_duplicate,
        ]
    }

    /// Move the admission exclusion alias out.
    pub(super) fn take_exclusion_alias(&mut self) -> Option<Arc<StateLockLease>> {
        self.held.get_mut().exclusion_alias.take()
    }

    /// Move the clock alias out.
    pub(super) fn take_clock_alias(&mut self) -> Option<Arc<lillux::time::RetainedOccupancyClock>> {
        self.held.get_mut().clock_alias.take()
    }

    /// Put the clock alias back into an empty slot; an occupied slot refuses
    /// and hands the offered alias back, so nothing is replaced.
    pub(super) fn restore_clock_alias(
        &mut self,
        alias: Arc<lillux::time::RetainedOccupancyClock>,
    ) -> std::result::Result<(), Arc<lillux::time::RetainedOccupancyClock>> {
        let parts = self.held.get_mut();
        if parts.clock_alias.is_some() {
            return Err(alias);
        }
        parts.clock_alias = Some(alias);
        Ok(())
    }

    /// End every source slot and the exclusion alias after their receipts
    /// were validated. Each source is itself held, so ending a slot is
    /// supervised; the clock alias is left for its own retirement.
    pub(super) fn end_validated_sources(&mut self) {
        let parts = self.held.get_mut();
        parts.root = None;
        parts.runtime = None;
        parts.completion_parent = None;
        parts.completion_file = None;
        parts.completion_duplicate = None;
        parts.policy_parent = None;
        parts.policy_file = None;
        parts.policy_duplicate = None;
        parts.exclusion_alias = None;
    }
}
impl std::ops::Deref for NativeCapacityInputFinalizationSources {
    type Target = InputSourcesParts;
    fn deref(&self) -> &InputSourcesParts {
        &self.held
    }
}

fn quote_native_capacity_input_finalization_sources()
-> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let directory_owner = std::mem::size_of::<PinnedDirectory>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(std::mem::align_of::<PinnedDirectory>() - 1))
        .context("native input directory owner layout overflow")?;
    let directory_bytes = directory_owner
        .checked_mul(4)
        .context("native input directory source quote overflow")?;
    let bytes = u64::try_from(std::mem::size_of::<InputSourcesParts>())?
        .checked_add(u64::try_from(directory_bytes)?)
        .context("native input finalization source quote overflow")?;
    // One bounded outside-work unit for each of four directories, four file
    // descriptors and two retained alias transitions. Registry scans and
    // actual close work are separately charged by their respective owners.
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes, steps: 10 })
}

impl RetainedInput {
    fn into_finalization_parts(self) -> (PinnedDirectory, PinnedRegularFile, File) {
        let Self {
            parent,
            file,
            descriptor,
            observation: _,
            bytes: _,
        } = self;
        (parent, file, descriptor)
    }
}

impl LiveNativeCapacityInputs {
    /// Called from the real composition after signed bootstrap and before its
    /// first SQLite call. Capture has no signing, journal publication or native
    /// SQLite effects. The composition keeps an alias of the returned owner
    /// across the separate irreversible account reservation and retains it
    /// once a typed reservation owner holds the other reference; a refusal
    /// before any account owner exists releases the alias, as a refusal of
    /// capture itself does, because nothing has been charged for it yet.
    pub fn capture(app_root: &Path, exclusion: Arc<StateLockLease>) -> Result<Arc<Self>> {
        // Authentication and preparation consume the same original interval.
        // Capture before path construction, input reads or owned policy work;
        // learning the signed bound later never renews this start instant.
        let clock =
            Arc::new(lillux::time::RetainedOccupancyClock::capture().map_err(anyhow::Error::msg)?);
        exclusion.ensure_protects_app_root(app_root)?;
        let root = PinnedDirectory::open(app_root)?.context("native node root is absent")?;
        let roots = RuntimeRoot::new(app_root.to_owned());
        let runtime =
            PinnedDirectory::open(&roots.state())?.context("native runtime root is absent")?;
        let completion = RetainedInput::capture(
            PinnedDirectory::open(&roots.config().join("onboarding"))?
                .context("initialization fence directory is absent")?,
            OsStr::new("init-completion.json"),
            MAX_COMPLETION_BYTES,
        )?;
        let report = crate::init::verify_init_completion(app_root)?
            .context("live native admission requires completed operator initialization")?;
        ensure!(
            lillux::sha256_hex(&completion.bytes) == report.completion_digest,
            "initialization verifier did not select the retained fence"
        );
        let trust = TrustStore::load(None, &roots.config())?;
        let snapshot = ryeos_node_runtime::node_policy::load_snapshot(
            app_root,
            &trust,
            &NodePolicyTable::new(),
        )?;
        ensure!(
            snapshot.generation_digest() == report.policy_generation_digest,
            "native policy generation differs from completed initialization"
        );
        let policy = snapshot.require::<NodeNativeCapacityPolicy>()?.clone();
        let policy_file = RetainedInput::capture(
            PinnedDirectory::open(
                &ryeos_node_runtime::node_policy::generation::policy_directory(app_root),
            )?
            .context("native policy directory is absent")?,
            OsStr::new("native_capacity.yaml"),
            ryeos_node_runtime::node_document::MAX_ITEM_BYTES,
        )?;
        let verified = ryeos_node_runtime::node_document::verify_pinned_signed_yaml(
            &policy_file.file,
            &trust,
        )?;
        ensure!(
            verified.signer_fingerprint == report.node_fingerprint,
            "native allowance was not issued by the initialized node"
        );
        let pinned_policy: NodeNativeCapacityPolicy = serde_json::from_value(verified.body)?;
        pinned_policy.validate()?;
        ensure!(
            pinned_policy == policy,
            "native allowance changed during capture"
        );
        // Bind the authenticated signed duration to the entry-captured clock.
        // Initialization and retries consume this same interval; Ready does
        // not reset it. Capture itself never claims a capacity debit.
        let occupancy = lillux::time::OccupancyLimit::new(
            clock.start().clone(),
            policy
                .incarnation
                .maximum_occupancy_ms
                .checked_mul(1_000_000)
                .context("native occupancy overflow")?,
        )
        .map_err(anyhow::Error::msg)?;
        let (sql_deadline, cleanup_deadline) = clock
            .original_local_deadlines(
                &occupancy,
                Duration::from_millis(policy.incarnation.cleanup_occupancy_ms),
            )
            .map_err(anyhow::Error::msg)?;
        let service_edge = clock
            .service_edge(
                &occupancy,
                Duration::from_millis(policy.incarnation.cleanup_occupancy_ms),
            )
            .map_err(anyhow::Error::msg)?;
        let process = lillux::process_control::capture_current_process_identity()
            .map_err(anyhow::Error::msg)?;
        let result = Arc::new(Self {
            root,
            runtime,
            exclusion,
            completion,
            policy_file,
            policy,
            snapshot,
            node_fingerprint: report.node_fingerprint,
            operator_fingerprint: report.operator_fingerprint,
            capacity_account: report.capacity_account,
            clock,
            occupancy,
            sql_deadline,
            cleanup_deadline,
            service_edge,
            process,
        });
        result.require_original(false)?;
        Ok(result)
    }

    /// Join the actual loaded startup identity and policy generation to the
    /// retained verified fence before any irreversible capacity reservation.
    pub fn require_loaded_startup(
        &self,
        identity: &ryeos_node_runtime::identity::NodeIdentity,
        policy: &NodePolicySnapshot,
    ) -> Result<()> {
        self.require_original(false)?;
        ensure!(
            identity.fingerprint() == self.node_fingerprint,
            "loaded startup identity differs from original native admission"
        );
        ensure!(
            policy.generation_digest() == self.snapshot.generation_digest(),
            "loaded startup policy differs from original native admission"
        );
        self.require_original(false)
    }

    /// Revalidate the retained original cleanup interval for its outside
    /// supervisor. This observation issues no budget, ticket or renewed window;
    /// each resource transition must still authenticate its actual owner.
    pub fn require_original_cleanup(&self) -> Result<()> {
        self.require_original(true)
    }

    /// Consume the verified input frame only after the real account is settled.
    /// The original pins, actual duplicates, exclusion alias and shared clock
    /// remain explicitly retained until the finalizer transfers or closes them.
    pub(super) fn into_finalization_sources(
        self,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<NativeCapacityInputFinalizationSources, (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_original(true)?;
            let quote = quote_native_capacity_input_finalization_sources()?;
            prepay(quote.bytes, quote.steps)?;
            self.require_original(true)
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }

        let Self {
            root,
            runtime,
            exclusion,
            completion,
            policy_file,
            clock,
            policy: _,
            snapshot: _,
            node_fingerprint: _,
            operator_fingerprint: _,
            capacity_account: _,
            occupancy: _,
            sql_deadline: _,
            cleanup_deadline: _,
            service_edge: _,
            process: _,
        } = self;
        let (completion_parent, completion_file, completion_duplicate) =
            completion.into_finalization_parts();
        let (policy_parent, policy_file, policy_duplicate) = policy_file.into_finalization_parts();
        Ok(NativeCapacityInputFinalizationSources::held(
            InputSourcesParts {
                root: Some(FinalizerDirectorySource::retained(Arc::new(root))),
                runtime: Some(FinalizerDirectorySource::retained(runtime.into())),
                completion_parent: Some(FinalizerDirectorySource::retained(Arc::new(
                    completion_parent,
                ))),
                completion_file: Some(FinalizerControlSource::pinned_file(completion_file)),
                completion_duplicate: Some(FinalizerControlSource::duplicate(completion_duplicate)),
                policy_parent: Some(FinalizerDirectorySource::retained(Arc::new(policy_parent))),
                policy_file: Some(FinalizerControlSource::pinned_file(policy_file)),
                policy_duplicate: Some(FinalizerControlSource::duplicate(policy_duplicate)),
                exclusion_alias: Some(exclusion),
                clock_alias: Some(clock),
            },
        ))
    }

    pub(crate) fn require_original(&self, cleanup: bool) -> Result<()> {
        // Refuse inherited custody before inspecting exclusions, namespaces or
        // admission descriptors. Child cleanup cannot traverse original owners.
        self.require_original_process()?;
        self.exclusion.ensure_protects_app_root(self.root.path())?;
        self.root
            .ensure_path_binding_fixed()
            .map_err(anyhow::Error::msg)?;
        self.runtime
            .ensure_path_binding_fixed()
            .map_err(anyhow::Error::msg)?;
        self.completion.require_original()?;
        self.policy_file.require_original()?;
        let window = self
            .clock
            .window_fixed(
                &self.occupancy,
                Duration::from_millis(self.policy.incarnation.cleanup_occupancy_ms),
            )
            .map_err(anyhow::Error::msg)?;
        ensure!(
            matches!(window, lillux::time::OccupancyWindowState::Service { .. })
                || (cleanup
                    && matches!(window, lillux::time::OccupancyWindowState::Cleanup { .. })),
            "original native admission interval exhausted"
        );
        Ok(())
    }

    fn require_original_process(&self) -> Result<()> {
        lillux::process_control::require_current_process_identity_fixed(&self.process)
            .map_err(anyhow::Error::msg)
    }

    /// In-memory service-edge check for one native VM step: one BOOTTIME read,
    /// no file, procfs or digest work.
    pub(crate) fn require_service_edge(&self) -> Result<()> {
        self.service_edge
            .require_service_fixed()
            .map_err(anyhow::Error::msg)
    }
}

/// Serializes the tests that count abandoned custody, which is process-wide
/// and only ever grows.
#[cfg(test)]
pub(super) fn abandoned_custody_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const DIRECTORY: &str = "ryeos-node::native-capacity-finalizer-directory";
    const CONTROL: &str = "ryeos-node::native-capacity-finalizer-control";
    const FRAME: &str = "ryeos-node::native-capacity-input-sources";

    fn empty_frame() -> NativeCapacityInputFinalizationSources {
        NativeCapacityInputFinalizationSources::held(InputSourcesParts {
            root: None,
            runtime: None,
            completion_parent: None,
            completion_file: None,
            completion_duplicate: None,
            policy_parent: None,
            policy_file: None,
            policy_duplicate: None,
            exclusion_alias: None,
            clock_alias: None,
        })
    }

    #[test]
    fn dropped_directory_source_retains_its_owner_until_transferred() {
        let _serial = abandoned_custody_serial();
        let directory = tempfile::tempdir().unwrap();
        let owner = Arc::new(PinnedDirectory::open(directory.path()).unwrap().unwrap());
        let source = FinalizerDirectorySource::retained(Arc::clone(&owner));
        assert!(!source.owner_transferred());
        let before = lillux::custody::abandoned_custody().count(DIRECTORY);
        drop(source);
        assert_eq!(
            lillux::custody::abandoned_custody().count(DIRECTORY),
            before + 1
        );
        assert_eq!(
            Arc::strong_count(&owner),
            2,
            "the directory alias stays retained"
        );

        let mut transferred = FinalizerDirectorySource::retained(Arc::clone(&owner));
        drop(transferred.take_owner());
        assert!(transferred.owner_transferred());
        drop(transferred);
        assert_eq!(
            lillux::custody::abandoned_custody().count(DIRECTORY),
            before + 1
        );
        assert_eq!(Arc::strong_count(&owner), 2);
    }

    #[test]
    fn dropped_control_source_retains_its_descriptor_until_transferred() {
        let _serial = abandoned_custody_serial();
        let directory = tempfile::tempdir().unwrap();
        let file = File::create(directory.path().join("control")).unwrap();
        let source = FinalizerControlSource::duplicate(file);
        assert!(!source.source_transferred());
        let before = lillux::custody::abandoned_custody().count(CONTROL);
        drop(source);
        assert_eq!(
            lillux::custody::abandoned_custody().count(CONTROL),
            before + 1
        );

        let mut transferred = FinalizerControlSource::duplicate(
            File::create(directory.path().join("transferred")).unwrap(),
        );
        drop(transferred.take_duplicate());
        assert!(transferred.source_transferred());
        drop(transferred);
        assert_eq!(
            lillux::custody::abandoned_custody().count(CONTROL),
            before + 1
        );
    }

    #[test]
    fn dropped_input_frame_is_retained_whole_until_consumed() {
        let _serial = abandoned_custody_serial();
        let directory = tempfile::tempdir().unwrap();
        let owner = Arc::new(PinnedDirectory::open(directory.path()).unwrap().unwrap());
        let consumed = empty_frame();
        assert!(consumed.consumed());
        let before = lillux::custody::abandoned_custody();
        drop(consumed);
        let unchanged = lillux::custody::abandoned_custody();
        assert_eq!(unchanged.count(FRAME), before.count(FRAME));

        let mut frame = empty_frame();
        frame.held.get_mut().root = Some(FinalizerDirectorySource::retained(Arc::clone(&owner)));
        assert!(!frame.consumed());
        drop(frame);
        let after = lillux::custody::abandoned_custody();
        assert_eq!(after.count(FRAME), before.count(FRAME) + 1);
        // The directory source went with its frame; its own Drop never ran.
        assert_eq!(after.count(DIRECTORY), before.count(DIRECTORY));
        assert_eq!(
            Arc::strong_count(&owner),
            2,
            "the root alias stays retained"
        );
    }
}
