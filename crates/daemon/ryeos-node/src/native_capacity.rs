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
pub use account::{CapacityAccountSummary, inspect_capacity_account};
pub use account::reissue::{CapacityReissueEvidence, ReissueReport, ReissueRequest, read_capacity_reissuances, reissue_capacity};
pub(crate) use account::load_existing_capacity_binding;
pub(crate) use account::prepare_initial_capacity_binding;
pub use factory::{LiveNativeFactory, LiveNativeFactoryFailure, LiveNativeProducerPublicationFailure, LiveNativeProducerPreparationFailure, LiveProducerControlCensus, ServiceRefusalLatch, ServiceRefusalWake, service_refusal};
pub use reservation::RolloverClaim;
pub use reservation::{FinalizedLiveNative, LiveNativeCapacityReservation, LiveNativeCapacityReservationFailure, SettledLiveNative, quote_operator_lock_handoff, required_cleanup_occupancy, required_rollover_drain_margin};

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
        file.require_owner_fixed(owner.owner()).map_err(anyhow::Error::msg)?;
        parent.require_regular_entry_descriptor_fixed(name, &descriptor).map_err(anyhow::Error::msg)?;
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
        self.parent.ensure_path_binding_fixed().map_err(anyhow::Error::msg)?;
        self.parent
            .require_regular_entry_descriptor_fixed(self.file.name(), &self.descriptor).map_err(anyhow::Error::msg)?;
        lillux::ensure_open_regular_file_unchanged(&self.descriptor, &self.observation)?;
        let owner = lillux::require_effective_user_owned_regular(&self.descriptor)?;
        self.file.require_owner_fixed(owner.owner()).map_err(anyhow::Error::msg)
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

/// Exact original directory until its same-registry membership is cached.
/// No path or identity value can construct this source.
pub(super) struct FinalizerDirectorySource {
    pub(super) owner: Option<std::mem::ManuallyDrop<Arc<PinnedDirectory>>>,
    pub(super) registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
impl FinalizerDirectorySource {
    pub(super) fn retained(owner: Arc<PinnedDirectory>) -> Self {
        Self { owner: Some(std::mem::ManuallyDrop::new(owner)), registration: None }
    }

    /// True once the directory owner moved into a registry membership; only
    /// the plain registration, if any, remains.
    pub(super) fn owner_transferred(&self) -> bool {
        self.owner.is_none()
    }
}
impl Drop for FinalizerDirectorySource {
    fn drop(&mut self) {
        // Settled exactly when the directory owner moved into the registry:
        // the membership alone remains and drops normally. The owner slot is
        // taken exactly once, here; otherwise the original directory and any
        // membership stay retained together until the process exits.
        const KIND: &str = "ryeos-node::native-capacity-finalizer-directory";
        if lillux::exec::in_forked_child() {
            // A copy in another process holds none of these owners.
            lillux::custody::relinquish_in_foreign_process((self.owner.take(), self.registration.take()));
            return;
        }
        if let Some(owner) = self.owner.take() {
            lillux::custody::abandon(KIND, (std::mem::ManuallyDrop::into_inner(owner), self.registration.take()));
        }
    }
}

/// Actual account/input descriptor during the post-settlement registry join.
/// Each transition keeps either the authentic open source, its close owner, or
/// its exact registry membership until the next consuming stage succeeds.
pub(super) struct FinalizerControlSource {
    pub(super) pinned_file: Option<std::mem::ManuallyDrop<PinnedRegularFile>>,
    pub(super) duplicate: Option<std::mem::ManuallyDrop<File>>,
    pub(super) owner: Option<Arc<lillux_sqlite::RetainedOriginalFinalizerFile>>,
    pub(super) registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
impl FinalizerControlSource {
    pub(super) fn pinned_file(file: PinnedRegularFile) -> Self {
        Self { pinned_file: Some(std::mem::ManuallyDrop::new(file)), duplicate: None,
            owner: None, registration: None }
    }
    pub(super) fn duplicate(file: File) -> Self {
        Self { pinned_file: None, duplicate: Some(std::mem::ManuallyDrop::new(file)),
            owner: None, registration: None }
    }

    /// True once the open source moved into its close owner or registry
    /// membership; neither the original pin nor the duplicate remains.
    pub(super) fn source_transferred(&self) -> bool {
        self.pinned_file.is_none() && self.duplicate.is_none()
    }
}
impl Drop for FinalizerControlSource {
    fn drop(&mut self) {
        // Settled exactly when the open source moved on: the close owner and
        // membership carry their own disposal and drop normally. Each slot is
        // taken exactly once, here; otherwise the open descriptor, any close
        // owner and any membership stay retained together until the process
        // exits.
        const KIND: &str = "ryeos-node::native-capacity-finalizer-control";
        let settled = self.source_transferred();
        let taken = (
            self.pinned_file.take().map(std::mem::ManuallyDrop::into_inner),
            self.duplicate.take().map(std::mem::ManuallyDrop::into_inner),
            self.owner.take(),
            self.registration.take(),
        );
        if lillux::exec::in_forked_child() {
            // A copy in another process holds none of these owners.
            lillux::custody::relinquish_in_foreign_process(taken);
            return;
        }
        if settled {
            drop(taken);
        } else {
            lillux::custody::abandon(KIND, taken);
        }
    }
}

/// Original admission descriptors after account settlement. These affine
/// fields are handed to the same preallocated registry as the Store graph;
/// `ManuallyDrop` keeps any source that has not yet registered from being
/// mistaken for an observed descriptor close during refusal handling.
pub(super) struct NativeCapacityInputFinalizationSources {
    pub(super) root: Option<FinalizerDirectorySource>,
    pub(super) runtime: Option<FinalizerDirectorySource>,
    pub(super) completion_parent: Option<FinalizerDirectorySource>,
    pub(super) completion_file: Option<FinalizerControlSource>,
    pub(super) completion_duplicate: Option<FinalizerControlSource>,
    pub(super) policy_parent: Option<FinalizerDirectorySource>,
    pub(super) policy_file: Option<FinalizerControlSource>,
    pub(super) policy_duplicate: Option<FinalizerControlSource>,
    pub(super) exclusion_alias: Option<std::mem::ManuallyDrop<Arc<StateLockLease>>>,
    pub(super) clock_alias: Option<std::mem::ManuallyDrop<Arc<lillux::time::RetainedOccupancyClock>>>,
}
impl NativeCapacityInputFinalizationSources {
    /// True once every source and both aliases moved on: the frame holds
    /// nothing, which is what final consumption requires of it.
    pub(super) fn consumed(&self) -> bool {
        self.root.is_none() && self.runtime.is_none()
            && self.completion_parent.is_none() && self.completion_file.is_none()
            && self.completion_duplicate.is_none() && self.policy_parent.is_none()
            && self.policy_file.is_none() && self.policy_duplicate.is_none()
            && self.exclusion_alias.is_none() && self.clock_alias.is_none()
    }

    /// Move every slot out exactly once, leaving this frame empty.
    fn take_frame(&mut self) -> Self {
        Self {
            root: self.root.take(),
            runtime: self.runtime.take(),
            completion_parent: self.completion_parent.take(),
            completion_file: self.completion_file.take(),
            completion_duplicate: self.completion_duplicate.take(),
            policy_parent: self.policy_parent.take(),
            policy_file: self.policy_file.take(),
            policy_duplicate: self.policy_duplicate.take(),
            exclusion_alias: self.exclusion_alias.take(),
            clock_alias: self.clock_alias.take(),
        }
    }
}
impl Drop for NativeCapacityInputFinalizationSources {
    fn drop(&mut self) {
        // Settled exactly when consumed: nothing remains to drop. Otherwise
        // every retained source and alias is taken exactly once, here, and
        // stays retained together until the process exits.
        const KIND: &str = "ryeos-node::native-capacity-input-sources";
        if lillux::exec::in_forked_child() {
            // A copy in another process holds none of these owners.
            lillux::custody::relinquish_in_foreign_process(self.take_frame());
            return;
        }
        if !self.consumed() {
            lillux::custody::abandon(KIND, self.take_frame());
        }
    }
}

fn quote_native_capacity_input_finalization_sources()
    -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let directory_owner = std::mem::size_of::<PinnedDirectory>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(std::mem::align_of::<PinnedDirectory>() - 1))
        .context("native input directory owner layout overflow")?;
    let directory_bytes = directory_owner.checked_mul(4)
        .context("native input directory source quote overflow")?;
    let bytes = u64::try_from(std::mem::size_of::<NativeCapacityInputFinalizationSources>())?
        .checked_add(u64::try_from(directory_bytes)?)
        .context("native input finalization source quote overflow")?;
    // One bounded outside-work unit for each of four directories, four file
    // descriptors and two retained alias transitions. Registry scans and
    // actual close work are separately charged by their respective owners.
    Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes, steps: 10 })
}

impl RetainedInput {
    fn into_finalization_parts(self) -> (PinnedDirectory, PinnedRegularFile, File) {
        let Self { parent, file, descriptor, observation: _, bytes: _ } = self;
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
        let clock = Arc::new(lillux::time::RetainedOccupancyClock::capture().map_err(anyhow::Error::msg)?);
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
        let (sql_deadline, cleanup_deadline) = clock.original_local_deadlines(
            &occupancy,
            Duration::from_millis(policy.incarnation.cleanup_occupancy_ms),
        ).map_err(anyhow::Error::msg)?;
        let service_edge = clock.service_edge(
            &occupancy,
            Duration::from_millis(policy.incarnation.cleanup_occupancy_ms),
        ).map_err(anyhow::Error::msg)?;
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
            root, runtime, exclusion, completion, policy_file, clock,
            policy: _, snapshot: _, node_fingerprint: _, operator_fingerprint: _,
            capacity_account: _, occupancy: _,
            sql_deadline: _, cleanup_deadline: _, service_edge: _, process: _,
        } = self;
        let (completion_parent, completion_file, completion_duplicate) = completion.into_finalization_parts();
        let (policy_parent, policy_file, policy_duplicate) = policy_file.into_finalization_parts();
        Ok(NativeCapacityInputFinalizationSources {
            root: Some(FinalizerDirectorySource::retained(Arc::new(root))),
            runtime: Some(FinalizerDirectorySource::retained(runtime.into())),
            completion_parent: Some(FinalizerDirectorySource::retained(Arc::new(completion_parent))),
            completion_file: Some(FinalizerControlSource::pinned_file(completion_file)),
            completion_duplicate: Some(FinalizerControlSource::duplicate(completion_duplicate)),
            policy_parent: Some(FinalizerDirectorySource::retained(Arc::new(policy_parent))),
            policy_file: Some(FinalizerControlSource::pinned_file(policy_file)),
            policy_duplicate: Some(FinalizerControlSource::duplicate(policy_duplicate)),
            exclusion_alias: Some(std::mem::ManuallyDrop::new(exclusion)),
            clock_alias: Some(std::mem::ManuallyDrop::new(clock)),
        })
    }

    pub(crate) fn require_original(&self, cleanup: bool) -> Result<()> {
        // Refuse inherited custody before inspecting exclusions, namespaces or
        // admission descriptors. Child cleanup cannot traverse original owners.
        self.require_original_process()?;
        self.exclusion.ensure_protects_app_root(self.root.path())?;
        self.root.ensure_path_binding_fixed().map_err(anyhow::Error::msg)?;
        self.runtime.ensure_path_binding_fixed().map_err(anyhow::Error::msg)?;
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
        self.service_edge.require_service_fixed().map_err(anyhow::Error::msg)
    }
}

/// Serializes the tests that count abandoned custody, which is process-wide
/// and only ever grows.
#[cfg(test)]
pub(super) fn abandoned_custody_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod custody_drop_tests {
    use super::*;

    const DIRECTORY: &str = "ryeos-node::native-capacity-finalizer-directory";
    const CONTROL: &str = "ryeos-node::native-capacity-finalizer-control";
    const FRAME: &str = "ryeos-node::native-capacity-input-sources";

    fn empty_frame() -> NativeCapacityInputFinalizationSources {
        NativeCapacityInputFinalizationSources {
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
        }
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
        assert_eq!(lillux::custody::abandoned_custody().count(DIRECTORY), before + 1);
        assert_eq!(Arc::strong_count(&owner), 2, "the directory alias stays retained");

        let mut transferred = FinalizerDirectorySource::retained(Arc::clone(&owner));
        drop(transferred.owner.take().map(std::mem::ManuallyDrop::into_inner));
        assert!(transferred.owner_transferred());
        drop(transferred);
        assert_eq!(lillux::custody::abandoned_custody().count(DIRECTORY), before + 1);
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
        assert_eq!(lillux::custody::abandoned_custody().count(CONTROL), before + 1);

        let mut transferred = FinalizerControlSource::duplicate(
            File::create(directory.path().join("transferred")).unwrap(),
        );
        drop(transferred.duplicate.take().map(std::mem::ManuallyDrop::into_inner));
        assert!(transferred.source_transferred());
        drop(transferred);
        assert_eq!(lillux::custody::abandoned_custody().count(CONTROL), before + 1);
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
        frame.root = Some(FinalizerDirectorySource::retained(Arc::clone(&owner)));
        assert!(!frame.consumed());
        drop(frame);
        let after = lillux::custody::abandoned_custody();
        assert_eq!(after.count(FRAME), before.count(FRAME) + 1);
        // The directory source went with its frame; its own Drop never ran.
        assert_eq!(after.count(DIRECTORY), before.count(DIRECTORY));
        assert_eq!(Arc::strong_count(&owner), 2, "the root alias stays retained");
    }
}
