//! One-way, exact guest base-installation intent beneath an external Worker.
//!
//! The durable owner record is written before private-source import and the
//! mutable base copy. Recovery checks exact retained intent but cannot
//! recreate the owner's process-local source, repeat installation, or grant
//! supervisor launch. The eventual guest owner must separately journal
//! launch, enforce input writer exclusion and settle the enclosing scope.

use std::ffi::OsStr;
use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::guest_supervisor_descriptors::{
    GuestSupervisorDescriptorPlan, SUPERVISOR_BOOTSTRAP_FD, SUPERVISOR_CANDIDATE_RUNTIME_FD,
    SUPERVISOR_EXECUTABLE_FD, SUPERVISOR_LAUNCHER_FD, SUPERVISOR_MOUNTED_CONTROL_DESCRIPTORS,
    SUPERVISOR_PRIVATE_PARENT_FD, SUPERVISOR_STAGE_MOUNT_DESTINATION, SUPERVISOR_STATE_ROOT_FD,
    fixed_guest_supervisor_descriptor_plan,
};
use ryeos_external_execution_contract::staging_package::{
    GuestImportContext, GuestImportTicket, GuestStagingEntry,
};
use ryeos_external_execution_contract::{ExternalGuestInputProjection, GuestMountContentAuthority};
use serde::{Deserialize, Serialize};

use crate::guest_import_authorization::VerifiedGuestImportAuthorization;
use crate::guest_staging::{
    GuestStageIdentity, TicketedGuestImport, stage_ticketed_uploaded_guest_package,
};

const OWNER_DIRECTORY: &str = "guest-import-owner";
const OWNER_RECORD_NAME: &str = "occurrence-owner.json";
const CANDIDATE_RUNTIME_DIRECTORY: &str = "candidate-runtime";
const CANDIDATE_PRIVATE_DIRECTORY: &str = "candidate-private";
const SUPERVISOR_STATE_DIRECTORY: &str = "supervisor-state";
const RECORD_NAME: &str = "guest-base-install-intent.json";
const SUPERVISOR_LAUNCH_RECORD_NAME: &str = "guest-supervisor-launch-intent.json";
const STAGE_MARKER_NAME: &str = "guest-base-install-owner.json";
const MAX_RECORD_BYTES: u64 = 8 * 1024;
pub const MAX_GUEST_SUPERVISOR_LAUNCH_RECORD_BYTES: usize = 8 * 1024;

/// Recovery-only outer owner→supervisor intent. The only future production
/// writer must hold continuous writer exclusion and commit this before spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestSupervisorLaunchIntent {
    schema: u32,
    ticket_sha256: String,
    occurrence_id: String,
    occurrence_directory: lillux::PinnedDirectoryIdentity,
    owner_directory: lillux::PinnedDirectoryIdentity,
    install_record_file: lillux::PinnedRegularFileIdentity,
    install_record_sha256: String,
    stage: GuestStageIdentity,
    candidate_runtime: lillux::PinnedDirectoryIdentity,
    private_parent: lillux::PinnedDirectoryIdentity,
    scratch: Vec<GuestScratchIdentity>,
    supervisor_state_root: lillux::PinnedDirectoryIdentity,
    bootstrap_sha256: String,
    supervisor_sha256: String,
    launcher_sha256: String,
    guest_input_identity: String,
    /// Exact IEEE-754 bits of the outer launch deadline budget.
    launch_timeout_bits: u64,
    stage_mount_destination: String,
    control_descriptors: Vec<u32>,
}

/// Supervisor-side interpretation of the sealed post-import record. This is
/// an exact content/handle join, not evidence that the outer mount was applied
/// read-only or that the held target may be released.
pub struct MountedGuestSupervisorHandoff {
    stage: GuestStageIdentity,
    launcher_sha256: String,
    record_sha256: String,
}

impl MountedGuestSupervisorHandoff {
    pub fn stage_directory_identity(&self) -> lillux::PinnedDirectoryIdentity {
        self.stage.directory_identity()
    }

    pub fn launcher_sha256(&self) -> &str {
        &self.launcher_sha256
    }

    pub fn record_sha256(&self) -> &str {
        &self.record_sha256
    }
}

pub fn decode_mounted_supervisor_handoff(
    sealed_record: &[u8],
    bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
    state_root: &lillux::PinnedDirectory,
    candidate_runtime: &lillux::PinnedDirectory,
    private_parent: &lillux::PinnedDirectory,
) -> Result<MountedGuestSupervisorHandoff> {
    ensure!(
        sealed_record.len() <= MAX_GUEST_SUPERVISOR_LAUNCH_RECORD_BYTES,
        "sealed guest supervisor launch record exceeds bound"
    );
    bootstrap.validate()?;
    let record: GuestSupervisorLaunchIntent = serde_json::from_slice(sealed_record)?;
    ensure!(
        record.schema == 2
            && canonical_launch_record(&record)? == sealed_record
            && record.stage_mount_destination == SUPERVISOR_STAGE_MOUNT_DESTINATION
            && record.control_descriptors == SUPERVISOR_MOUNTED_CONTROL_DESCRIPTORS
            && record.occurrence_id == bootstrap.occurrence_id
            && record.bootstrap_sha256 == lillux::sha256_hex(&bootstrap.canonical_bytes()?)
            && record.launcher_sha256 == bootstrap.launcher_artifact_hash
            && record.guest_input_identity == bootstrap.guest_input_identity
            && record.supervisor_state_root == state_root.identity()?
            && record.candidate_runtime == candidate_runtime.identity()?
            && record.private_parent == private_parent.identity()?
            && f64::from_bits(record.launch_timeout_bits).is_finite()
            && f64::from_bits(record.launch_timeout_bits) > 0.0,
        "sealed guest supervisor launch record differs from bootstrap or private handles"
    );
    record.stage.validate()?;
    recheck_scratch_bindings(private_parent, &record.scratch, &bootstrap.guest_inputs)?;
    Ok(MountedGuestSupervisorHandoff {
        stage: record.stage,
        launcher_sha256: record.launcher_sha256,
        record_sha256: lillux::sha256_hex(sealed_record),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestBaseInstallIntentIdentity {
    schema: u32,
    journal_directory: lillux::PinnedDirectoryIdentity,
    record_file: lillux::PinnedRegularFileIdentity,
    record_sha256: String,
    stage_marker_file: lillux::PinnedRegularFileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestBaseInstallMarker {
    schema: u32,
    journal_directory: lillux::PinnedDirectoryIdentity,
    record_file: lillux::PinnedRegularFileIdentity,
    record_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestOccurrenceOwnerRecord {
    schema: u32,
    authorization_sha256: String,
    ticket_sha256: String,
    occurrence_id: String,
    occurrence_directory: lillux::PinnedDirectoryIdentity,
    owner_directory: lillux::PinnedDirectoryIdentity,
}

/// Fixed, create-only owner below the independently selected occurrence root.
/// It is created before an uploaded byte can be imported. A failed stage or
/// process crash leaves this occurrence fenced against a second import.
pub struct GuestOccurrenceOwner {
    occurrence: lillux::PinnedDirectory,
    root: lillux::PinnedDirectory,
    _lock: lillux::PinnedDirectoryLock,
    ticket: GuestImportTicket,
    record: GuestOccurrenceOwnerRecord,
}

pub struct StagedGuestOccurrence {
    owner: GuestOccurrenceOwner,
    imported: TicketedGuestImport,
    source: GuestSourceCustody,
}

/// The production variant retains the dedicated private filesystem owner
/// through staging, installation and eventual held-supervisor preparation.
/// An ordinary pinned directory exists only for structural unit fixtures.
enum GuestSourceCustody {
    Live(lillux::sandbox::LinuxPrivateSourceFilesystem),
    Sealed(lillux::sandbox::LinuxSealedPrivateSourceFilesystem),
    #[cfg(test)]
    StructuralFixture(lillux::PinnedDirectory),
}

impl GuestSourceCustody {
    fn root(&self) -> &lillux::PinnedDirectory {
        match self {
            Self::Live(source) => source.root(),
            Self::Sealed(source) => source.root(),
            #[cfg(test)]
            Self::StructuralFixture(root) => root,
        }
    }

    fn seal_after_install(self) -> Result<Self> {
        match self {
            Self::Live(source) => Ok(Self::Sealed(
                source.seal_read_only().map_err(anyhow::Error::msg)?,
            )),
            Self::Sealed(_) => anyhow::bail!("guest private source was already sealed"),
            #[cfg(test)]
            Self::StructuralFixture(root) => Ok(Self::StructuralFixture(root)),
        }
    }
}

/// Retains both the exact owner and stage after base installation. This is
/// not supervisor adoption, writer exclusion, or a Ready claim.
pub struct InstalledGuestBase {
    owner: GuestOccurrenceOwner,
    imported: TicketedGuestImport,
    runtime: lillux::PinnedDirectory,
    intent_identity: GuestBaseInstallIntentIdentity,
    children: InstalledRuntimeChildren,
    _source: Option<GuestSourceCustody>,
}

/// One-shot, still-owned staged authorities prepared after the exact installed
/// base was rechecked. This is descriptor custody, not writer exclusion or a
/// launch permission; the outer owner must still create private scratch,
/// commit launch intent, and use Lillux's exact-inheritance spawn.
pub struct PreparedGuestContent {
    pub(crate) _installed: InstalledGuestBase,
    pub(crate) observation: InstalledGuestBaseObservation,
    pub(crate) handles: crate::guest_content::VerifiedGuestContentHandles,
}

/// Exact fresh private scratch created after verified staged-content custody.
/// This point observation is not writer exclusion or supervisor launch proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestPrivateInputObservation {
    pub schema: u32,
    pub private_parent: lillux::PinnedDirectoryIdentity,
    pub scratch: Vec<GuestScratchIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestScratchIdentity {
    pub input_index: usize,
    pub binding_hash: String,
    pub directory: lillux::PinnedDirectoryIdentity,
}

/// Retains the exact private parent and every completed runtime-mount slot.
/// The next owner must still bind the fixed descriptors, exclude writers and
/// commit an outer one-way launch intent before any native spawn.
pub struct PreparedGuestPrivateInputs {
    pub(crate) content: PreparedGuestContent,
    pub(crate) private_parent: lillux::PinnedDirectory,
    pub(crate) observation: GuestPrivateInputObservation,
}

/// Exact opened package artifacts and fresh state retained alongside the
/// installed base. The bootstrap source is a regular package file, not the
/// sealed memfd required at supervisor FD50. This remains neither writer
/// exclusion nor launch authority.
pub struct PreparedGuestLaunchArtifacts {
    pub(crate) private: PreparedGuestPrivateInputs,
    pub(crate) state_root: lillux::PinnedDirectory,
    pub(crate) bootstrap_source: lillux::InheritedDescriptorAuthority,
    pub(crate) supervisor: lillux::InheritedDescriptorAuthority,
    pub(crate) launcher: lillux::InheritedDescriptorAuthority,
    pub(crate) observation: GuestLaunchArtifactObservation,
}

/// Full exact descriptor proposal. This cannot spawn: the owner must commit
/// its durable outer intent, then join continuous writer exclusion and the
/// exact process-scope launch before any guest execution is permitted.
pub struct PreparedGuestSupervisorRequest {
    _artifacts: PreparedGuestLaunchArtifacts,
    request: lillux::SubprocessRequest,
    plan: GuestSupervisorDescriptorPlan,
    bootstrap_sha256: String,
}

/// One exact staged generation selected from the still-owned sealed source.
/// This is a mount input, not a target channel or a supervisor launch token.
/// The held Lillux transition must recheck it as the only read-only private
/// source mount and attest the applied destination before target adoption.
pub(crate) struct GuestSourceMountSelection {
    stage: GuestStageIdentity,
    authority: lillux::InheritedDescriptorAuthority,
}

impl GuestSourceMountSelection {
    pub(crate) fn stage(&self) -> &GuestStageIdentity {
        &self.stage
    }

    pub(crate) fn mount_authority(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.authority
    }
}

/// One-way outer launch intent retained with the exact prepared descriptors.
/// This deliberately has no spawn accessor: writer exclusion, process-scope
/// ownership and installed Ready still require a separate joined transition.
pub struct CommittedGuestSupervisorLaunchIntent {
    _prepared: PreparedGuestSupervisorRequest,
    _source_mount: GuestSourceMountSelection,
    record_file: lillux::PinnedRegularFileIdentity,
    record_sha256: String,
}

/// One exact held-sandbox proposal with all control FDs and the sealed source
/// owner still alive. No public raw-request or spawn accessor exists: the
/// next transition must join the Lillux held/applied receipts and release.
pub struct PreparedGuestMountedSandbox {
    _committed: CommittedGuestSupervisorLaunchIntent,
    _source_copy: lillux::InheritedDescriptorAuthority,
    _controls: [lillux::InheritedDescriptorAuthority; 5],
    _network_inputs: [lillux::InheritedDescriptorAuthority; 2],
    request: lillux::LinuxSandboxRequest,
}

/// A held native supervisor in its dedicated private-source owner. The target
/// has not been released or adopted as a Ready guest. Dropping this value
/// terminates the held target through Lillux.
pub struct HeldGuestMountedSandbox {
    // Fields drop in declaration order: settle the held namespace before
    // releasing the committed occurrence lock and descriptor custody.
    held: lillux::sandbox::HeldPrivateSourceSandboxProcess,
    _committed: CommittedGuestSupervisorLaunchIntent,
    _source_copy: lillux::InheritedDescriptorAuthority,
    _controls: [lillux::InheritedDescriptorAuthority; 5],
    _network_inputs: [lillux::InheritedDescriptorAuthority; 2],
    request: lillux::LinuxSandboxRequest,
}

/// The sole released native supervisor attempt. Applied launch is still only
/// pre-exec evidence; this owner cannot claim guest Ready or writer exclusion.
pub struct ReleasedGuestMountedSandbox {
    // Keep the namespace owner ahead of all source and control custody so
    // dropping an unadopted attempt settles the child first.
    held: lillux::sandbox::HeldPrivateSourceSandboxProcess,
    _committed: CommittedGuestSupervisorLaunchIntent,
    _source_copy: lillux::InheritedDescriptorAuthority,
    _controls: [lillux::InheritedDescriptorAuthority; 5],
    _network_inputs: [lillux::InheritedDescriptorAuthority; 2],
    expected_launch: lillux::LinuxSandboxAppliedLaunchCommitments,
    expected_mounts: lillux::LinuxSandboxMountPreparationCommitments,
}

impl HeldGuestMountedSandbox {
    /// Child-origin final-root observation before target release. It is not
    /// applied-exec, writer-exclusion, or guest Ready evidence.
    pub fn mount_preparation_receipt(
        &mut self,
    ) -> Result<lillux::LinuxSandboxMountPreparationReceipt> {
        let receipt = self
            .held
            .held()
            .mount_preparation_receipt()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            receipt
                .matches_request(&self.request)
                .map_err(anyhow::Error::msg)?,
            "held supervisor mount preparation differs from committed request"
        );
        Ok(receipt)
    }

    /// Consume the exact held launch once. The committed outer intent already
    /// forbids recovery from preparing a replacement. An ambiguous release
    /// error drops and settles this owner; it is never retried.
    pub fn release_once(mut self) -> Result<ReleasedGuestMountedSandbox> {
        // The exact launch record was sealed into a retained control FD
        // before Lillux entered the source-owner mount namespace. Reopening
        // its former host pathname here is neither possible nor authority.
        let expected_launch = lillux::LinuxSandboxAppliedLaunchCommitments::from_target(
            lillux::LinuxSandboxAppliedLaunchTarget {
                executable: &self.request.executable,
                argv0: &self.request.argv0,
                arguments: &self.request.arguments,
                cwd: &self.request.cwd,
                environment: &self.request.environment,
            },
        )
        .map_err(anyhow::Error::msg)?;
        let expected_mounts =
            lillux::LinuxSandboxMountPreparationCommitments::from_admitted_mounts(
                &self.request.mounts,
                &[],
            )
            .map_err(anyhow::Error::msg)?;
        let held_mounts = self.mount_preparation_receipt()?;
        ensure!(
            held_mounts.matches_commitments(&expected_mounts)
                && held_mounts.owned_child_pid == self.held.held().child_pid(),
            "held supervisor mount preparation changed before release"
        );
        self.held
            .held()
            .release_once()
            .map_err(anyhow::Error::msg)?;
        Ok(ReleasedGuestMountedSandbox {
            held: self.held,
            _committed: self._committed,
            _source_copy: self._source_copy,
            _controls: self._controls,
            _network_inputs: self._network_inputs,
            expected_launch,
            expected_mounts,
        })
    }
}

impl ReleasedGuestMountedSandbox {
    /// Cancel the sole released supervisor attempt under the caller's fixed
    /// cleanup deadline. Lillux terminates the exact namespace-init lifetime
    /// and proves descendant death; a timeout retains this owner for further
    /// observation, never a replacement launch. The returned proof is writer
    /// exclusion only, not successful execution, authenticated Ready, a
    /// frozen candidate or cloud-occurrence termination.
    pub fn terminate_namespace_for_export_until(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<lillux::LinuxSandboxTermination> {
        self.held
            .held()
            .terminate_namespace_for_export_until(deadline)
            .map_err(anyhow::Error::msg)
    }

    /// Observe the exact child immediately before its exec attempt. This
    /// checks the committed target and final-root mounts, but does not infer
    /// exec success, supervisor attachment, Ready or whole-scope settlement.
    pub fn try_observe_applied_launch(
        &mut self,
    ) -> Result<Option<lillux::LinuxSandboxAppliedLaunchReceipt>> {
        let Some(receipt) = self
            .held
            .held()
            .try_observe_applied_launch()
            .map_err(anyhow::Error::msg)?
        else {
            return Ok(None);
        };
        check_applied_supervisor_receipt(
            &receipt,
            &self.expected_launch,
            &self.expected_mounts,
            self.held.held().child_pid(),
        )?;
        Ok(Some(receipt))
    }

    /// Refuse an already-dead supervisor before attachment. A pending exit
    /// observation is only a point-in-time fact: it cannot establish that
    /// exec succeeded, that the target stays live, or that Ready is authentic.
    pub fn refuse_if_target_exited(&mut self) -> Result<()> {
        if let Some(exit) = self
            .held
            .held()
            .try_observe_target_exit()
            .map_err(anyhow::Error::msg)?
        {
            anyhow::bail!(
                "released supervisor exited before authenticated attachment: {:?}; launch_failure={}",
                exit.exit(),
                exit.launch_failure().is_some()
            );
        }
        Ok(())
    }

    /// Observe natural namespace death after the separate authenticated
    /// channel has finished. This returns Lillux's descendant-writer
    /// exclusion, not a Ready, successful protocol, or cloud-termination
    /// claim. An applied-launch receipt from the exact target and mounts is
    /// required before a code-zero exit can be accepted. The exit observation
    /// consumes the sole live process handle; this is not a replayable point
    /// read after terminal settlement or refusal.
    pub fn try_observe_natural_settlement(
        &mut self,
    ) -> Result<Option<lillux::LinuxSandboxTermination>> {
        let Some(_applied) = self.try_observe_applied_launch()? else {
            return Ok(None);
        };
        let Some(exit) = self
            .held
            .held()
            .try_observe_target_exit()
            .map_err(anyhow::Error::msg)?
        else {
            return Ok(None);
        };
        ensure!(
            exit.launch_failure().is_none() && exit.exit() == lillux::LinuxSandboxExit::Code(0),
            "released supervisor did not settle successfully after applied launch"
        );
        Ok(Some(exit.into_termination()))
    }
}

fn check_applied_supervisor_receipt(
    receipt: &lillux::LinuxSandboxAppliedLaunchReceipt,
    expected_launch: &lillux::LinuxSandboxAppliedLaunchCommitments,
    expected_mounts: &lillux::LinuxSandboxMountPreparationCommitments,
    child_pid: u32,
) -> Result<()> {
    ensure!(
        receipt.matches_commitments(expected_launch)
            && receipt.matches_post_release_mounts(expected_mounts)
            && receipt.owned_child_pid == child_pid,
        "released supervisor applied target or mounts differ from committed request"
    );
    Ok(())
}

/// Capture only the two guest-local files selected by the sealed bootstrap.
/// Lillux resolves symlinks once, pins exact regular files and seals bounded
/// bytes; the mounted supervisor sees those bytes at the original policy
/// paths, not ambient files from a controller or a later namespace.
fn capture_supervisor_network_mounts(
    policy: &ryeos_state::external_execution::transport::ExternalNetworkInputPolicy,
) -> Result<([lillux::InheritedDescriptorAuthority; 2], [PathBuf; 2])> {
    policy.validate()?;
    let destinations = [
        std::path::Path::new(&policy.resolver.source),
        std::path::Path::new(&policy.hosts.source),
        std::path::Path::new(SUPERVISOR_STAGE_MOUNT_DESTINATION),
    ];
    ensure!(
        destinations.iter().enumerate().all(|(index, path)| {
            destinations[index + 1..]
                .iter()
                .all(|other| !path.starts_with(other) && !other.starts_with(path))
        }),
        "supervisor network input destinations overlap each other or the stage"
    );
    let capture =
        |selection: &ryeos_state::external_execution::transport::ExternalNetworkInputSelection,
         name: &std::ffi::CStr|
         -> Result<(lillux::InheritedDescriptorAuthority, PathBuf, Vec<u8>)> {
            let source =
                lillux::canonicalize_existing_path(std::path::Path::new(&selection.source))?;
            let file = lillux::open_pinned_regular_file_no_follow(&source)?;
            let captured =
                file.capture_sealed_bounded(&file.observation()?, selection.max_bytes)?;
            let bytes = captured.bytes().to_vec();
            let sealed = lillux::sealed_memfd(name, &bytes)
                .map_err(anyhow::Error::msg)?
                .duplicate_at_or_above(59)
                .map_err(anyhow::Error::msg)?;
            Ok((sealed, PathBuf::from(&selection.source), bytes))
        };
    let (resolver, resolver_path, resolver_bytes) =
        capture(&policy.resolver, c"ryeos-guest-resolver")?;
    let (hosts, hosts_path, hosts_bytes) = capture(&policy.hosts, c"ryeos-guest-hosts")?;
    lillux::network::NetworkContext::from_config_bytes(&resolver_bytes, &hosts_bytes)?;
    Ok(([resolver, hosts], [resolver_path, hosts_path]))
}

impl CommittedGuestSupervisorLaunchIntent {
    pub fn record_file(&self) -> &lillux::PinnedRegularFileIdentity {
        &self.record_file
    }

    pub fn record_sha256(&self) -> &str {
        &self.record_sha256
    }

    /// Seal the already committed one-way record for supervisor adoption.
    /// This passes the retained stage identity across the exec boundary; it
    /// does not attest the applied mount, release a held target, or grant a
    /// second launch after uncertainty.
    pub fn seal_record_for_supervisor(&self) -> Result<lillux::InheritedDescriptorAuthority> {
        let owner = &self._prepared._artifacts.private.content._installed.owner;
        owner.root.require_owner_private_directory()?;
        let file = owner
            .root
            .open_pinned_regular(OsStr::new(SUPERVISOR_LAUNCH_RECORD_NAME), false)?
            .context("committed supervisor launch record is absent")?;
        ensure!(
            lillux::pinned_regular_file_identity(&file.try_clone_descriptor()?)?
                == self.record_file,
            "committed supervisor launch record inode changed"
        );
        let observation = file.observation()?;
        ensure!(
            observation.size() <= MAX_RECORD_BYTES,
            "committed supervisor launch record exceeds bound"
        );
        let bytes = file.read_stable_bounded(&observation, MAX_RECORD_BYTES)?;
        ensure!(
            lillux::sha256_hex(&bytes) == self.record_sha256,
            "committed supervisor launch record bytes changed"
        );
        let parsed: GuestSupervisorLaunchIntent = serde_json::from_slice(&bytes)?;
        ensure!(
            canonical_launch_record(&parsed)? == bytes
                && parsed.schema == 2
                && parsed.stage_mount_destination == SUPERVISOR_STAGE_MOUNT_DESTINATION
                && parsed.control_descriptors == SUPERVISOR_MOUNTED_CONTROL_DESCRIPTORS
                && parsed.stage == *self._source_mount.stage()
                && parsed.install_record_file
                    == self
                        ._prepared
                        ._artifacts
                        .private
                        .content
                        ._installed
                        .intent_identity
                        .record_file,
            "committed supervisor launch record changed retained source authority"
        );
        lillux::sealed_memfd(c"ryeos-guest-supervisor-launch-intent", &bytes)
            .map_err(anyhow::Error::msg)
    }

    /// Form the sole mounted-source supervisor request from committed owner
    /// handles. The controller transport belongs to this trusted supervisor;
    /// untrusted candidate commands receive a separate nested isolated view.
    /// This retains every FD, but deliberately cannot spawn or claim Ready.
    pub fn prepare_mounted_sandbox_request(
        self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<PreparedGuestMountedSandbox> {
        let artifacts = &self._prepared._artifacts;
        let installed = &artifacts.private.content._installed;
        installed.owner.recheck(context, inputs)?;
        installed.recheck_for_adoption(context, inputs)?;
        ensure!(
            self._source_mount.stage() == &installed.imported.stage_identity()?,
            "mounted supervisor source differs from committed stage"
        );
        let bootstrap = artifacts.seal_supervisor_bootstrap(context, inputs)?;
        let bootstrap_bytes = lillux::read_sealed_inherited_descriptor(
            bootstrap
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            ryeos_state::external_execution::transport::MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
        )
        .map_err(anyhow::Error::msg)?;
        let parsed_bootstrap: ryeos_state::external_execution::transport::ExternalSupervisorBootstrap =
            serde_json::from_slice(&bootstrap_bytes)?;
        ensure!(
            parsed_bootstrap.canonical_bytes()? == bootstrap_bytes,
            "sealed supervisor bootstrap changed before network-input capture"
        );
        let (network_inputs, network_destinations) =
            capture_supervisor_network_mounts(&parsed_bootstrap.controller.network_inputs)?;
        let state = artifacts.state_root.inherited_descriptor_authority()?;
        let runtime = installed.runtime.inherited_descriptor_authority()?;
        let private = artifacts
            .private
            .private_parent
            .inherited_descriptor_authority()?;
        let launch_record = self.seal_record_for_supervisor()?;
        let controls: [lillux::InheritedDescriptorAuthority; 5] =
            [bootstrap, state, runtime, private, launch_record]
                .into_iter()
                .map(|authority| {
                    authority
                        .duplicate_at_or_above(59)
                        .map_err(anyhow::Error::msg)
                })
                .collect::<Result<Vec<_>>>()?
                .try_into()
                .map_err(|_| anyhow::anyhow!("supervisor control inventory changed length"))?;
        let channels = controls
            .iter()
            .zip(SUPERVISOR_MOUNTED_CONTROL_DESCRIPTORS)
            .map(|(authority, target)| {
                Ok::<_, anyhow::Error>((
                    authority
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    target,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let source_copy = self
            ._source_mount
            .mount_authority()
            .duplicate_at_or_above(59)
            .map_err(anyhow::Error::msg)?;
        let source_fd = source_copy
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            channels.iter().all(|(fd, _)| *fd != source_fd),
            "sealed source descriptor entered supervisor control channels"
        );
        let mut mounts = vec![lillux::LinuxSandboxMount {
            source_fd,
            destination: PathBuf::from(SUPERVISOR_STAGE_MOUNT_DESTINATION),
            access: lillux::LinuxSandboxMountAccess::ReadOnly,
            layer: 0,
        }];
        for (source, destination) in network_inputs.iter().zip(network_destinations) {
            mounts.push(lillux::LinuxSandboxMount {
                source_fd: source.inherited_descriptor().map_err(anyhow::Error::msg)?,
                destination,
                access: lillux::LinuxSandboxMountAccess::ReadOnly,
                layer: 0,
            });
        }
        let request = lillux::LinuxSandboxRequest {
            executable: PathBuf::from(SUPERVISOR_STAGE_MOUNT_DESTINATION).join("supervisor"),
            argv0: "ryeos-external-candidate-supervisor".into(),
            arguments: Vec::new(),
            cwd: PathBuf::from("/"),
            environment: std::collections::BTreeMap::new(),
            mounts,
            fixed_parent_views: Vec::new(),
            overlay: None,
            network: lillux::LinuxSandboxNetwork::Host,
            private_tmp: true,
            proc_filesystem: lillux::LinuxSandboxProcFilesystem::PidNamespaceNested,
            minimal_devices: true,
            character_devices: Vec::new(),
            target_channels: channels,
            lifecycle: lillux::LinuxSandboxLifecycle::Run,
            contain_process_group: false,
            nested_sandbox: true,
            aggregate_limits: None,
        };
        Ok(PreparedGuestMountedSandbox {
            _committed: self,
            _source_copy: source_copy,
            _controls: controls,
            _network_inputs: network_inputs,
            request,
        })
    }
}

impl PreparedGuestMountedSandbox {
    /// Enter the one-way native preparation cut in a dedicated, unprivileged
    /// owner process. Lillux exits that process on a source mismatch or failed
    /// namespace transition; this must never run on a shared daemon worker.
    pub fn prepare_held_in_dedicated_owner(mut self) -> Result<HeldGuestMountedSandbox> {
        let installed = &mut self
            ._committed
            ._prepared
            ._artifacts
            .private
            .content
            ._installed;
        let stage_name = self._committed._source_mount.stage().name().to_owned();
        let source = match installed._source.take() {
            Some(GuestSourceCustody::Sealed(source)) => source,
            Some(_) => anyhow::bail!("guest source was not sealed for native preparation"),
            None => anyhow::bail!("guest source custody was already consumed"),
        };
        let mut held = lillux::sandbox::prepare_linux_sandbox_from_sealed_source(
            source,
            OsStr::new(&stage_name),
            std::path::Path::new(SUPERVISOR_STAGE_MOUNT_DESTINATION),
            self.request.clone(),
        );
        let receipt = held
            .held()
            .mount_preparation_receipt()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            receipt
                .matches_request(&self.request)
                .map_err(anyhow::Error::msg)?,
            "held supervisor mount preparation differs from committed request"
        );
        Ok(HeldGuestMountedSandbox {
            held,
            _committed: self._committed,
            _source_copy: self._source_copy,
            _controls: self._controls,
            _network_inputs: self._network_inputs,
            request: self.request,
        })
    }
}

#[cfg(test)]
impl PreparedGuestMountedSandbox {
    pub(crate) fn inspect_for_test(&self) -> &lillux::LinuxSandboxRequest {
        &self.request
    }
}

impl PreparedGuestSupervisorRequest {
    /// Resolve the retained stage under its original private source owner.
    /// The source remains owned by `self`; extracting this exact mount input
    /// does not permit a raw spawn or forwarding its FD to `target_channels`.
    pub(crate) fn prepare_source_mount_selection(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<GuestSourceMountSelection> {
        let installed = &self._artifacts.private.content._installed;
        installed.recheck_for_adoption(context, inputs)?;
        let source_root = match installed._source.as_ref() {
            Some(GuestSourceCustody::Sealed(source)) => source.root(),
            Some(GuestSourceCustody::Live(_)) => {
                anyhow::bail!("guest source was not sealed before mount selection")
            }
            #[cfg(test)]
            Some(GuestSourceCustody::StructuralFixture(root)) => root,
            None => anyhow::bail!("guest source custody was already consumed"),
        };
        let stage = installed.imported.stage_identity()?;
        let selected = stage.resolve_under(source_root)?;
        ensure!(
            selected.identity()? == stage.directory_identity(),
            "guest source mount selection changed stage identity"
        );
        Ok(GuestSourceMountSelection {
            stage,
            authority: selected.inherited_descriptor_authority()?,
        })
    }

    fn exact_request_timeout_bits(&self) -> Result<u64> {
        let request = &self.request;
        ensure!(
            request.cmd == format!("/proc/self/fd/{SUPERVISOR_EXECUTABLE_FD}")
                && request.argv0.as_deref() == Some("ryeos-external-candidate-supervisor")
                && request.args.is_empty()
                && request.cwd.is_none()
                && request.envs.is_empty()
                && request.stdin_data.is_none()
                && request.timeout.is_finite()
                && request.timeout > 0.0
                && request.limits.is_none()
                && request.inherited_fds.is_empty()
                && request.supervised_status.is_none(),
            "guest supervisor request differs from fixed execution profile"
        );
        let mut actual = request
            .inherited_fd_mappings
            .iter()
            .map(lillux::InheritedDescriptorMapping::target_descriptor)
            .collect::<Vec<_>>();
        actual.sort_unstable();
        let mut expected = self.plan.inherited_descriptors.clone();
        expected.sort_unstable();
        ensure!(
            actual == expected,
            "guest supervisor request differs from fixed descriptor inventory"
        );
        Ok(request.timeout.to_bits())
    }

    fn planned_outer_launch_intent_bytes(
        &self,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<Vec<u8>> {
        let installed = &self._artifacts.private.content._installed;
        let ticket = installed.imported.ticket();
        let owner = &installed.owner;
        ensure!(
            self.plan == fixed_guest_supervisor_descriptor_plan(inputs)?,
            "guest supervisor content plan changed before outer intent"
        );
        canonical_launch_record(&GuestSupervisorLaunchIntent {
            schema: 2,
            ticket_sha256: digest_ticket(ticket)?,
            occurrence_id: owner.record.occurrence_id.clone(),
            occurrence_directory: owner.occurrence.identity()?,
            owner_directory: owner.root.identity()?,
            install_record_file: installed.intent_identity.record_file.clone(),
            install_record_sha256: installed.intent_identity.record_sha256.clone(),
            stage: installed.imported.stage_identity()?,
            candidate_runtime: installed.runtime.identity()?,
            private_parent: self._artifacts.private.observation.private_parent.clone(),
            scratch: self._artifacts.private.observation.scratch.clone(),
            supervisor_state_root: self._artifacts.observation.state_root.clone(),
            bootstrap_sha256: self.bootstrap_sha256.clone(),
            supervisor_sha256: ticket.supervisor_sha256.clone(),
            launcher_sha256: ticket.launcher_sha256.clone(),
            guest_input_identity: inputs.identity_digest()?,
            launch_timeout_bits: self.exact_request_timeout_bits()?,
            stage_mount_destination: SUPERVISOR_STAGE_MOUNT_DESTINATION.into(),
            control_descriptors: SUPERVISOR_MOUNTED_CONTROL_DESCRIPTORS.to_vec(),
        })
    }

    /// Commit exactly one durable pre-spawn intent. A failure after this cut
    /// is `LaunchUncertain` on recovery, never permission to re-import, copy,
    /// remint descriptors, or launch another supervisor.
    pub fn commit_outer_launch_intent(
        self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<CommittedGuestSupervisorLaunchIntent> {
        let installed = &self._artifacts.private.content._installed;
        installed.owner.recheck(context, inputs)?;
        installed.recheck_for_adoption(context, inputs)?;
        let source_mount = self.prepare_source_mount_selection(context, inputs)?;
        ensure!(
            source_mount.mount_authority().directory_identity()?
                == source_mount.stage().directory_identity(),
            "guest launch source mount differs from retained stage"
        );
        ensure!(
            self._artifacts.private.private_parent.identity()?
                == self._artifacts.private.observation.private_parent
                && self._artifacts.state_root.identity()? == self._artifacts.observation.state_root
                && self
                    ._artifacts
                    .state_root
                    .entries_no_follow_bounded(0)?
                    .is_empty(),
            "guest launch private or state root changed before outer intent"
        );
        self._artifacts
            .private
            .private_parent
            .require_owner_private_directory()?;
        self._artifacts
            .state_root
            .require_owner_private_directory()?;
        ensure_child_binding(
            &installed.owner.occurrence,
            CANDIDATE_PRIVATE_DIRECTORY,
            &self._artifacts.private.private_parent,
        )?;
        ensure_child_binding(
            &installed.owner.occurrence,
            SUPERVISOR_STATE_DIRECTORY,
            &self._artifacts.state_root,
        )?;
        recheck_scratch_bindings(
            &self._artifacts.private.private_parent,
            &self._artifacts.private.observation.scratch,
            inputs,
        )?;
        let bytes = self.planned_outer_launch_intent_bytes(inputs)?;
        let (record_file, record_sha256) =
            create_launch_intent_record(&installed.owner.root, &bytes)?;
        Ok(CommittedGuestSupervisorLaunchIntent {
            record_file,
            record_sha256,
            _prepared: self,
            _source_mount: source_mount,
        })
    }
}

#[cfg(test)]
impl PreparedGuestSupervisorRequest {
    /// Structural inspection only. Production code has no way to extract the
    /// raw request until an outer writer-fenced launch transition exists.
    pub(crate) fn inspect_for_test(
        &self,
    ) -> (
        &lillux::SubprocessRequest,
        &GuestSupervisorDescriptorPlan,
        &str,
        &lillux::InheritedDescriptorAuthority,
    ) {
        (
            &self.request,
            &self.plan,
            &self.bootstrap_sha256,
            &self._artifacts.supervisor,
        )
    }

    /// Synthetic fixture bytes only. This does not commit a production launch
    /// intent or authorize a spawn without continuous writer exclusion.
    pub(crate) fn planned_outer_launch_intent_bytes_for_test(
        &self,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<Vec<u8>> {
        self.planned_outer_launch_intent_bytes(inputs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestLaunchArtifactObservation {
    pub schema: u32,
    pub state_root: lillux::PinnedDirectoryIdentity,
    pub bootstrap: lillux::PinnedRegularFileIdentity,
    pub supervisor: lillux::PinnedRegularFileIdentity,
    pub launcher: lillux::PinnedRegularFileIdentity,
}

impl PreparedGuestLaunchArtifacts {
    /// Complete every fixed supervisor descriptor from retained handles.
    /// The resulting request is intentionally held behind a non-launching
    /// type until the outer one-way journal and writer fence are joined.
    pub fn prepare_supervisor_request(
        self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        timeout_seconds: f64,
    ) -> Result<PreparedGuestSupervisorRequest> {
        ensure!(
            timeout_seconds.is_finite() && timeout_seconds > 0.0,
            "supervisor launch timeout is invalid"
        );
        let sealed_bootstrap = self.seal_supervisor_bootstrap(context, inputs)?;
        ensure!(
            self.state_root.identity()? == self.observation.state_root
                && self.state_root.entries_no_follow_bounded(0)?.is_empty(),
            "supervisor state root changed before request preparation"
        );
        let request = lillux::SubprocessRequest {
            cmd: String::new(),
            argv0: Some("ryeos-external-candidate-supervisor".into()),
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: timeout_seconds,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        let (mut request, plan) = self
            .private
            .bind_content_to_supervisor_request(context, inputs, request)?;
        self.supervisor
            .bind_as_subprocess_executable(&mut request, SUPERVISOR_EXECUTABLE_FD)
            .map_err(anyhow::Error::msg)?;
        sealed_bootstrap
            .bind_to_subprocess_request(&mut request, SUPERVISOR_BOOTSTRAP_FD)
            .map_err(anyhow::Error::msg)?;
        self.state_root
            .inherited_descriptor_authority()?
            .bind_to_subprocess_request(&mut request, SUPERVISOR_STATE_ROOT_FD)
            .map_err(anyhow::Error::msg)?;
        self.launcher
            .bind_to_subprocess_request(&mut request, SUPERVISOR_LAUNCHER_FD)
            .map_err(anyhow::Error::msg)?;
        let mut actual = request
            .inherited_fd_mappings
            .iter()
            .map(lillux::InheritedDescriptorMapping::target_descriptor)
            .collect::<Vec<_>>();
        actual.sort_unstable();
        let mut expected = plan.inherited_descriptors.clone();
        expected.sort_unstable();
        ensure!(
            request.inherited_fds.is_empty() && actual == expected,
            "supervisor request differs from exact fixed descriptor plan"
        );
        let bootstrap_sha256 = self
            .private
            .content
            ._installed
            .imported
            .ticket()
            .bootstrap_sha256
            .clone();
        Ok(PreparedGuestSupervisorRequest {
            _artifacts: self,
            request,
            plan,
            bootstrap_sha256,
        })
    }

    /// Re-read the exact opened package source, validate its canonical secret
    /// bootstrap against the independently retained occurrence and projection,
    /// then mint the sealed FD50 input required by the supervisor. No regular
    /// package file may be bound directly at that coordinate.
    pub fn seal_supervisor_bootstrap(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<lillux::InheritedDescriptorAuthority> {
        let source = &self.bootstrap_source;
        let ticket = self.private.content._installed.imported.ticket();
        ticket.validate_for_context(context)?;
        ensure!(
            ticket.guest_input_identity == inputs.identity_digest()?,
            "supervisor bootstrap ticket differs from retained guest input"
        );
        let (bytes, observed) = source.read_regular_file_stable_bounded(
            ryeos_state::external_execution::transport::MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES
                as u64,
        )?;
        ensure!(
            // The ticket's bootstrap digest is supplied from the controller's
            // exact retained activation, not derived from package bytes. It
            // binds every field, including controller and owner authority.
            lillux::sha256_hex(&bytes) == ticket.bootstrap_sha256
                && observed.permission_mode()? == 0o600,
            "opened supervisor bootstrap source changed before sealing"
        );
        let bootstrap: ryeos_state::external_execution::transport::ExternalSupervisorBootstrap =
            serde_json::from_slice(&bytes).context("decode imported supervisor bootstrap")?;
        ensure!(
            bootstrap.canonical_bytes()? == bytes
                && bootstrap.occurrence_id == context.occurrence_id
                && bootstrap.allocation_request_digest == context.allocation_request_digest
                && bootstrap.base_snapshot_hash == inputs.base_snapshot.snapshot_hash
                && bootstrap.guest_input_identity == inputs.identity_digest()?
                && bootstrap.launcher_artifact_hash == ticket.launcher_sha256,
            "imported supervisor bootstrap differs from retained occurrence authority"
        );
        lillux::sealed_memfd(c"ryeos-external-supervisor-bootstrap", &bytes)
            .map_err(anyhow::Error::msg)
    }
}

impl PreparedGuestPrivateInputs {
    /// Open and verify the three exact package artifacts, then create the
    /// state root once under the retained occurrence. A failed or crashed
    /// preparation leaves this occurrence fenced against a second owner.
    pub fn prepare_launch_artifacts_once(
        self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<PreparedGuestLaunchArtifacts> {
        self.content
            ._installed
            .recheck_for_adoption(context, inputs)?;
        let imported = &self.content._installed.imported;
        let (bootstrap_source, bootstrap_identity) = open_verified_guest_artifact(
            imported,
            "bootstrap",
            &imported.ticket().bootstrap_sha256,
            false,
        )?;
        let (supervisor, supervisor_identity) = open_verified_guest_artifact(
            imported,
            "supervisor",
            &imported.ticket().supervisor_sha256,
            true,
        )?;
        let (launcher, launcher_identity) = open_verified_guest_artifact(
            imported,
            "launcher",
            &imported.ticket().launcher_sha256,
            true,
        )?;
        let state_root = self
            .content
            ._installed
            .owner
            .occurrence
            .create_child(OsStr::new(SUPERVISOR_STATE_DIRECTORY), 0o700)?;
        state_root.require_owner_private_directory()?;
        ensure!(
            state_root.entries_no_follow_bounded(0)?.is_empty(),
            "new supervisor state root contains ambient content"
        );
        state_root.require_disjoint_directory_tree(&self.content._installed.runtime)?;
        state_root.require_disjoint_directory_tree(&self.private_parent)?;
        state_root.require_disjoint_directory_tree(imported.root())?;
        let observation = GuestLaunchArtifactObservation {
            schema: 1,
            state_root: state_root.identity()?,
            bootstrap: bootstrap_identity,
            supervisor: supervisor_identity,
            launcher: launcher_identity,
        };
        Ok(PreparedGuestLaunchArtifacts {
            private: self,
            state_root,
            bootstrap_source,
            supervisor,
            launcher,
            observation,
        })
    }
}

fn open_verified_guest_artifact(
    imported: &TicketedGuestImport,
    name: &str,
    expected_hash: &str,
    executable: bool,
) -> Result<(
    lillux::InheritedDescriptorAuthority,
    lillux::PinnedRegularFileIdentity,
)> {
    let (expected_bytes, expected_mode) = imported
        .manifest()
        .entries
        .iter()
        .find_map(|entry| match entry {
            GuestStagingEntry::RegularFile {
                path, bytes, mode, ..
            } if path == name => Some((*bytes, *mode)),
            _ => None,
        })
        .with_context(|| format!("guest {name} is absent from its manifest"))?;
    let file = imported
        .root()
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("guest {name} disappeared before handle custody"))?;
    let observation = file.observation()?;
    ensure!(
        observation.size() == expected_bytes
            && file.permission_mode()? == expected_mode
            && file.digest_stable_exact(&observation)? == expected_hash,
        "opened guest {name} differs from retained package identity"
    );
    let authority = file.inherited_descriptor_authority()?;
    if executable {
        authority.require_owned_executable()?;
    } else {
        authority.require_owned_regular()?;
    }
    let identity = lillux::pinned_regular_file_identity(&file.try_clone_descriptor()?)?;
    Ok((authority, identity))
}

impl PreparedGuestPrivateInputs {
    /// Bind retained input handles to the supervisor's one fixed descriptor
    /// plan. The request is consumed so a partial binding failure cannot be
    /// reused as a launch request. This is a repeatable prelaunch observation,
    /// not one-shot launch authority. Bootstrap, state root, launcher, durable
    /// launch intent, and writer exclusion remain separate requirements.
    pub fn bind_content_to_supervisor_request(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        mut request: lillux::SubprocessRequest,
    ) -> Result<(lillux::SubprocessRequest, GuestSupervisorDescriptorPlan)> {
        ensure!(
            request.inherited_fds.is_empty() && request.inherited_fd_mappings.is_empty(),
            "guest content binding requires an unbound subprocess request"
        );
        ensure!(
            self.content
                ._installed
                .recheck_for_adoption(context, inputs)?
                == self.content.observation,
            "installed guest base changed after content custody"
        );
        ensure!(
            self.private_parent.identity()? == self.observation.private_parent,
            "private guest parent changed after creation"
        );
        self.private_parent.require_owner_private_directory()?;
        for scratch in &self.observation.scratch {
            let name = format!("guest-scratch-{:02}", scratch.input_index);
            let directory = self
                .private_parent
                .open_child_directory(OsStr::new(&name))?
                .context("private guest scratch disappeared before descriptor binding")?;
            directory.require_owner_private_directory()?;
            ensure!(
                directory.identity()? == scratch.directory
                    && directory.entries_no_follow_bounded(0)?.is_empty(),
                "private guest scratch changed before descriptor binding"
            );
        }
        let plan = fixed_guest_supervisor_descriptor_plan(inputs)?;
        ensure!(
            plan.runtime_mount_descriptors.len() == self.content.handles.runtime_mounts.len()
                && plan.content_record_descriptors.len()
                    == self.content.handles.content_records.len(),
            "prepared guest content differs from fixed supervisor descriptor plan"
        );
        self.content
            ._installed
            .runtime
            .inherited_descriptor_authority()?
            .bind_to_subprocess_request(&mut request, SUPERVISOR_CANDIDATE_RUNTIME_FD)
            .map_err(anyhow::Error::msg)?;
        self.private_parent
            .inherited_descriptor_authority()?
            .bind_to_subprocess_request(&mut request, SUPERVISOR_PRIVATE_PARENT_FD)
            .map_err(anyhow::Error::msg)?;
        match (
            self.content.handles.workspace_outputs.as_ref(),
            inputs.workspace_outputs.as_ref(),
        ) {
            (Some(handle), Some(_)) => handle
                .bind_to_subprocess_request(
                    &mut request,
                    ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_WORKSPACE_OUTPUT_FD,
                )
                .map_err(anyhow::Error::msg)?,
            (None, None) => {}
            _ => anyhow::bail!("prepared guest workspace-output authority changed"),
        }
        for (handle, target) in self
            .content
            .handles
            .runtime_mounts
            .iter()
            .zip(&plan.runtime_mount_descriptors)
        {
            handle
                .as_ref()
                .context("private guest input slot is absent")?
                .bind_to_subprocess_request(&mut request, *target)
                .map_err(anyhow::Error::msg)?;
        }
        for (handle, target) in self
            .content
            .handles
            .content_records
            .iter()
            .zip(&plan.content_record_descriptors)
        {
            handle
                .bind_to_subprocess_request(&mut request, *target)
                .map_err(anyhow::Error::msg)?;
        }
        let mut expected_targets = vec![
            SUPERVISOR_CANDIDATE_RUNTIME_FD,
            SUPERVISOR_PRIVATE_PARENT_FD,
        ];
        if inputs.workspace_outputs.is_some() {
            expected_targets.push(
                ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_WORKSPACE_OUTPUT_FD,
            );
        }
        expected_targets.extend(&plan.runtime_mount_descriptors);
        expected_targets.extend(&plan.content_record_descriptors);
        ensure!(
            request
                .inherited_fd_mappings
                .iter()
                .map(lillux::InheritedDescriptorMapping::target_descriptor)
                .collect::<Vec<_>>()
                == expected_targets,
            "guest content binding differs from fixed supervisor descriptor subset"
        );
        Ok((request, plan))
    }
}

impl PreparedGuestContent {
    /// Create fresh scratch under the same one-shot occurrence owner. No
    /// package byte or provider pathname can select these child names.
    pub fn create_private_scratch_once(
        mut self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<PreparedGuestPrivateInputs> {
        self._installed.recheck_for_adoption(context, inputs)?;
        ensure!(
            self.handles.runtime_mounts.len() == inputs.inputs.len(),
            "prepared guest content slot count changed"
        );
        let private_parent = self
            ._installed
            .owner
            .occurrence
            .create_child(OsStr::new(CANDIDATE_PRIVATE_DIRECTORY), 0o700)?;
        private_parent.require_owner_private_directory()?;
        private_parent.require_disjoint_directory_tree(&self._installed.runtime)?;
        private_parent.require_disjoint_directory_tree(self._installed.imported.root())?;
        let mut scratch = Vec::new();
        for (index, input) in inputs.inputs.iter().enumerate() {
            match &input.content_authority {
                GuestMountContentAuthority::PrivateScratch { binding_hash } => {
                    ensure!(
                        self.handles.runtime_mounts[index].is_none(),
                        "private scratch slot was populated by staged content"
                    );
                    let name = format!("guest-scratch-{index:02}");
                    let directory = private_parent.create_child(OsStr::new(&name), 0o700)?;
                    directory.require_owner_private_directory()?;
                    ensure!(
                        directory.entries_no_follow_bounded(0)?.is_empty(),
                        "new private scratch contains ambient content"
                    );
                    let identity = directory.identity()?;
                    self.handles.runtime_mounts[index] =
                        Some(directory.inherited_descriptor_authority()?);
                    scratch.push(GuestScratchIdentity {
                        input_index: index,
                        binding_hash: binding_hash.clone(),
                        directory: identity,
                    });
                }
                _ => ensure!(
                    self.handles.runtime_mounts[index].is_some(),
                    "verified immutable guest input slot is absent"
                ),
            }
        }
        ensure!(
            self.handles.runtime_mounts.iter().all(Option::is_some),
            "guest runtime mount inventory is incomplete"
        );
        let observation = GuestPrivateInputObservation {
            schema: 1,
            private_parent: private_parent.identity()?,
            scratch,
        };
        Ok(PreparedGuestPrivateInputs {
            content: self,
            private_parent,
            observation,
        })
    }
}

/// Exact installed child inodes observed before handoff. This is a point
/// coordinate, not a lock or evidence that untrusted writers were excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledRuntimeChildren {
    pub objects: lillux::PinnedDirectoryIdentity,
    pub refs: lillux::PinnedDirectoryIdentity,
    pub recovery: lillux::PinnedDirectoryIdentity,
    pub thread_projection: lillux::PinnedDirectoryIdentity,
    pub mutation_lock: lillux::PinnedRegularFileIdentity,
}

/// A point observation of the still-owned, exact installed base. It does not
/// grant supervisor launch or attest exclusion of other writers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledGuestBaseObservation {
    pub schema: u32,
    pub occurrence_id: String,
    pub ticket_sha256: String,
    pub stage: GuestStageIdentity,
    pub candidate_runtime: lillux::PinnedDirectoryIdentity,
    pub children: InstalledRuntimeChildren,
    pub base_snapshot_hash: String,
    pub base_closure_digest: String,
}

impl InstalledGuestBase {
    /// Consume the live installed owner while retaining the exact opened
    /// staged descriptors that a later outer launch owner will bind.
    pub fn prepare_content_for_adoption(
        self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<PreparedGuestContent> {
        self.recheck_for_adoption(context, inputs)?;
        let handles = crate::guest_content::open_verified_staged_guest_content(
            self.imported.staged(),
            inputs,
        )?;
        let observation = self.recheck_for_adoption(context, inputs)?;
        Ok(PreparedGuestContent {
            _installed: self,
            observation,
            handles,
        })
    }

    /// Recheck the original stage, installation journal, and copied base
    /// immediately before a future descriptor-bound supervisor adoption.
    /// The caller must separately exclude writers across that handoff.
    pub fn recheck_for_adoption(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<InstalledGuestBaseObservation> {
        self.owner.recheck(context, inputs)?;
        self.runtime.require_owner_private_directory()?;
        ensure_child_binding(
            &self.owner.occurrence,
            CANDIDATE_RUNTIME_DIRECTORY,
            &self.runtime,
        )?;
        self.owner
            .root
            .require_disjoint_directory_tree(&self.runtime)?;
        verify_committed_intent(
            &self.imported,
            context,
            inputs,
            &self.owner.root,
            &self.runtime,
            &self.intent_identity,
        )?;
        ensure!(
            inspect_installed_runtime_children(&self.runtime)? == self.children,
            "installed guest runtime child inodes changed before adoption"
        );
        let objects = self
            .runtime
            .open_child_directory(OsStr::new("objects"))?
            .context("installed guest base CAS disappeared")?;
        let observed = ryeos_project_capture::inspect_project_snapshot_transfer(
            &objects,
            &inputs.base_snapshot.snapshot_hash,
        )?;
        ensure!(
            &observed == self.imported.base(),
            "installed guest base changed before adoption"
        );
        Ok(InstalledGuestBaseObservation {
            schema: 1,
            occurrence_id: context.occurrence_id.to_owned(),
            ticket_sha256: digest_ticket(&self.owner.ticket)?,
            stage: self.imported.stage_identity()?,
            candidate_runtime: self.runtime.identity()?,
            children: self.children.clone(),
            base_snapshot_hash: observed.snapshot_hash,
            base_closure_digest: observed.closure_digest,
        })
    }
}

fn inspect_installed_runtime_children(
    runtime: &lillux::PinnedDirectory,
) -> Result<InstalledRuntimeChildren> {
    runtime.require_owner_private_directory()?;
    ensure!(
        runtime.entry_names()?
            == ["objects", "recovery", "refs"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
        "installed guest runtime has unexpected entries before adoption"
    );
    let objects = runtime
        .open_child_directory(OsStr::new("objects"))?
        .context("installed guest base CAS disappeared")?;
    objects.require_owner_private_directory()?;
    let refs = runtime
        .open_child_directory(OsStr::new("refs"))?
        .context("installed guest refs root disappeared")?;
    refs.require_owner_private_directory()?;
    ensure!(
        refs.entries_no_follow_bounded(0)?.is_empty(),
        "installed guest refs changed before supervisor adoption"
    );
    let recovery = runtime
        .open_child_directory(OsStr::new("recovery"))?
        .context("installed guest CAS recovery root disappeared")?;
    recovery.require_owner_private_directory()?;
    ensure!(
        recovery.entry_names()?
            == ["cas-mutation.lock", "thread-projection"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
        "installed guest CAS recovery root has unexpected entries"
    );
    let lock = recovery
        .open_pinned_regular(OsStr::new("cas-mutation.lock"), false)?
        .context("installed guest CAS mutation lock disappeared")?;
    let lock_observation = lock.observation()?;
    ensure!(
        lock.permission_mode()? == 0o600 && lock_observation.size() == 0,
        "installed guest CAS mutation lock changed"
    );
    let thread_projection = recovery
        .open_child_directory(OsStr::new("thread-projection"))?
        .context("installed guest thread projection root disappeared")?;
    thread_projection.require_owner_private_directory()?;
    ensure!(
        thread_projection.entries_no_follow_bounded(0)?.is_empty(),
        "installed guest thread projection changed before adoption"
    );
    Ok(InstalledRuntimeChildren {
        objects: objects.identity()?,
        refs: refs.identity()?,
        recovery: recovery.identity()?,
        thread_projection: thread_projection.identity()?,
        mutation_lock: lillux::pinned_regular_file_identity(&lock.try_clone_descriptor()?)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestOccurrenceRecoveryPhase {
    /// An owner was committed before import, but no install intent exists.
    /// An upload or stage may have happened; neither can be replayed here.
    ImportUncertain,
    /// Installation intent is committed. The base copy may be absent,
    /// partial, or complete; this observation does not grant another copy.
    InstallationUncertain,
    /// An outer launch intent is present. The supervisor may have started;
    /// recovery must not repeat launch or demand pristine mutable inputs.
    LaunchUncertain,
}

pub struct RecoveredGuestOccurrence {
    _occurrence: lillux::PinnedDirectory,
    _root: lillux::PinnedDirectory,
    _lock: lillux::PinnedDirectoryLock,
    _runtime: Option<lillux::PinnedDirectory>,
    phase: GuestOccurrenceRecoveryPhase,
}

impl RecoveredGuestOccurrence {
    pub fn phase(&self) -> &GuestOccurrenceRecoveryPhase {
        &self.phase
    }
}

/// Point-read the fixed owner child beneath the retained occurrence. Neither
/// phase exposes import, installation, supervisor launch, or Ready authority.
/// In particular, a crash before the owner record or stage intent is complete
/// remains quarantined rather than guessed from directory contents.
pub fn recover_guest_occurrence_authorized(
    occurrence: &lillux::PinnedDirectory,
    authorization: &VerifiedGuestImportAuthorization,
) -> Result<RecoveredGuestOccurrence> {
    let admitted = authorization.authorization();
    recover_guest_occurrence_inner(
        occurrence,
        &admitted.ticket,
        &admitted.context(),
        &admitted.guest_inputs,
        authorization.identity_digest(),
    )
}

#[cfg(test)]
pub(crate) fn recover_guest_occurrence(
    occurrence: &lillux::PinnedDirectory,
    ticket: &GuestImportTicket,
    context: &GuestImportContext<'_>,
    inputs: &ExternalGuestInputProjection,
) -> Result<RecoveredGuestOccurrence> {
    recover_guest_occurrence_inner(
        occurrence,
        ticket,
        context,
        inputs,
        &structural_fixture_authorization_digest(ticket)?,
    )
}

fn recover_guest_occurrence_inner(
    occurrence: &lillux::PinnedDirectory,
    ticket: &GuestImportTicket,
    context: &GuestImportContext<'_>,
    inputs: &ExternalGuestInputProjection,
    authorization_sha256: &str,
) -> Result<RecoveredGuestOccurrence> {
    occurrence.require_owner_private_directory()?;
    ticket.staging_expected(context, inputs)?;
    ensure!(
        lillux::valid_hash(authorization_sha256),
        "guest import authorization digest is invalid"
    );
    let root = occurrence
        .open_child_directory(OsStr::new(OWNER_DIRECTORY))?
        .context("guest occurrence owner is absent")?;
    root.require_owner_private_directory()?;
    let lock = root
        .try_lock_exclusive()?
        .context("guest occurrence owner remains live")?;
    lock.ensure_protects(&root)?;
    let owner_file = root
        .open_pinned_regular(OsStr::new(OWNER_RECORD_NAME), false)?
        .context("guest occurrence owner record is absent")?;
    ensure!(
        owner_file.permission_mode()? == 0o600,
        "guest occurrence owner record mode changed"
    );
    let owner_observation = owner_file.observation()?;
    ensure!(
        owner_observation.size() <= MAX_RECORD_BYTES,
        "guest occurrence owner record exceeds bound"
    );
    let owner_bytes = owner_file.read_stable_bounded(&owner_observation, MAX_RECORD_BYTES)?;
    let owner: GuestOccurrenceOwnerRecord = serde_json::from_slice(&owner_bytes)?;
    ensure!(
        canonical_owner_record(&owner)? == owner_bytes
            && owner.schema == 2
            && owner.authorization_sha256 == authorization_sha256
            && owner.ticket_sha256 == digest_ticket(ticket)?
            && owner.occurrence_id == context.occurrence_id
            && owner.occurrence_directory == occurrence.identity()?
            && owner.owner_directory == root.identity()?,
        "guest occurrence owner differs from retained placement"
    );
    let Some(record) = root.open_pinned_regular(OsStr::new(RECORD_NAME), false)? else {
        ensure!(
            root.open_pinned_regular(OsStr::new(SUPERVISOR_LAUNCH_RECORD_NAME), false)?
                .is_none(),
            "guest supervisor launch intent exists without base installation intent"
        );
        ensure_child_binding(occurrence, OWNER_DIRECTORY, &root)?;
        return Ok(RecoveredGuestOccurrence {
            _occurrence: occurrence.try_clone()?,
            _root: root,
            _lock: lock,
            _runtime: None,
            phase: GuestOccurrenceRecoveryPhase::ImportUncertain,
        });
    };
    ensure!(
        record.permission_mode()? == 0o600,
        "guest installation intent mode changed"
    );
    let observed = record.observation()?;
    ensure!(
        observed.size() <= MAX_RECORD_BYTES,
        "guest installation intent exceeds bound"
    );
    let bytes = record.read_stable_bounded(&observed, MAX_RECORD_BYTES)?;
    let intent: GuestBaseInstallIntent = serde_json::from_slice(&bytes)?;
    ensure!(
        canonical_record(&intent)? == bytes
            && intent.schema == 1
            && intent.ticket_sha256 == owner.ticket_sha256
            && intent.journal_directory == root.identity()?,
        "guest installation intent differs from occurrence owner"
    );
    intent.stage.validate_for_ticket(&ticket.manifest_sha256)?;
    let runtime = occurrence
        .open_child_directory(OsStr::new(CANDIDATE_RUNTIME_DIRECTORY))?
        .context("committed guest installation has no retained runtime")?;
    runtime.require_owner_private_directory()?;
    root.require_disjoint_directory_tree(&runtime)?;
    if let Some(launch_file) =
        root.open_pinned_regular(OsStr::new(SUPERVISOR_LAUNCH_RECORD_NAME), false)?
    {
        let private_parent = occurrence
            .open_child_directory(OsStr::new(CANDIDATE_PRIVATE_DIRECTORY))?
            .context("committed guest launch has no retained private parent")?;
        let state_root = occurrence
            .open_child_directory(OsStr::new(SUPERVISOR_STATE_DIRECTORY))?
            .context("committed guest launch has no retained supervisor state root")?;
        private_parent.require_owner_private_directory()?;
        state_root.require_owner_private_directory()?;
        root.require_disjoint_directory_tree(&private_parent)?;
        root.require_disjoint_directory_tree(&state_root)?;
        runtime.require_disjoint_directory_tree(&private_parent)?;
        runtime.require_disjoint_directory_tree(&state_root)?;
        private_parent.require_disjoint_directory_tree(&state_root)?;
        ensure!(
            launch_file.permission_mode()? == 0o600,
            "guest supervisor launch intent mode changed"
        );
        let launch_observation = launch_file.observation()?;
        ensure!(
            launch_observation.size() <= MAX_RECORD_BYTES,
            "guest supervisor launch intent exceeds bound"
        );
        let launch_bytes =
            launch_file.read_stable_bounded(&launch_observation, MAX_RECORD_BYTES)?;
        let launch: GuestSupervisorLaunchIntent = serde_json::from_slice(&launch_bytes)?;
        ensure!(
            canonical_launch_record(&launch)? == launch_bytes
                && launch.schema == 2
                && launch.ticket_sha256 == owner.ticket_sha256
                && launch.occurrence_id == owner.occurrence_id
                && launch.occurrence_directory == owner.occurrence_directory
                && launch.owner_directory == owner.owner_directory
                && launch.install_record_file
                    == lillux::pinned_regular_file_identity(&record.try_clone_descriptor()?)?
                && launch.install_record_sha256 == lillux::sha256_hex(&bytes)
                && launch.stage == intent.stage
                && launch.candidate_runtime == intent.candidate_runtime
                && launch.candidate_runtime == runtime.identity()?
                && launch.private_parent == private_parent.identity()?
                && launch.supervisor_state_root == state_root.identity()?
                && launch.bootstrap_sha256 == ticket.bootstrap_sha256
                && launch.supervisor_sha256 == ticket.supervisor_sha256
                && launch.launcher_sha256 == ticket.launcher_sha256
                && launch.guest_input_identity == inputs.identity_digest()?
                && f64::from_bits(launch.launch_timeout_bits).is_finite()
                && f64::from_bits(launch.launch_timeout_bits) > 0.0
                && launch.stage_mount_destination == SUPERVISOR_STAGE_MOUNT_DESTINATION
                && launch.control_descriptors == SUPERVISOR_MOUNTED_CONTROL_DESCRIPTORS,
            "guest supervisor launch intent differs from retained authority"
        );
        ensure_child_binding(occurrence, CANDIDATE_PRIVATE_DIRECTORY, &private_parent)?;
        ensure_child_binding(occurrence, SUPERVISOR_STATE_DIRECTORY, &state_root)?;
        recheck_scratch_bindings(&private_parent, &launch.scratch, inputs)?;
        ensure_child_binding(occurrence, OWNER_DIRECTORY, &root)?;
        ensure_child_binding(occurrence, CANDIDATE_RUNTIME_DIRECTORY, &runtime)?;
        return Ok(RecoveredGuestOccurrence {
            _occurrence: occurrence.try_clone()?,
            _root: root,
            _lock: lock,
            _runtime: Some(runtime),
            phase: GuestOccurrenceRecoveryPhase::LaunchUncertain,
        });
    }
    ensure!(
        intent.candidate_runtime == runtime.identity()?,
        "committed guest runtime changed inode"
    );
    // The staged source belongs to the lost owner's process-private mount.
    // A child or retained FD may keep that mount alive, but recovery cannot
    // reconstruct its authority from a durable path. This phase can only be
    // quarantined, never adopted or copied.
    ensure_child_binding(occurrence, OWNER_DIRECTORY, &root)?;
    ensure_child_binding(occurrence, CANDIDATE_RUNTIME_DIRECTORY, &runtime)?;
    Ok(RecoveredGuestOccurrence {
        _occurrence: occurrence.try_clone()?,
        _root: root,
        _lock: lock,
        _runtime: Some(runtime),
        phase: GuestOccurrenceRecoveryPhase::InstallationUncertain,
    })
}

fn ensure_child_binding(
    parent: &lillux::PinnedDirectory,
    name: &str,
    child: &lillux::PinnedDirectory,
) -> Result<()> {
    ensure!(
        parent
            .open_child_directory(OsStr::new(name))?
            .context("retained guest occurrence child disappeared")?
            .identity()?
            == child.identity()?,
        "retained guest occurrence child changed inode"
    );
    Ok(())
}

fn recheck_scratch_bindings(
    private_parent: &lillux::PinnedDirectory,
    scratch: &[GuestScratchIdentity],
    inputs: &ExternalGuestInputProjection,
) -> Result<()> {
    let expected = inputs
        .inputs
        .iter()
        .enumerate()
        .filter_map(|(index, input)| match &input.content_authority {
            GuestMountContentAuthority::PrivateScratch { binding_hash } => {
                Some((index, binding_hash.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure!(
        scratch.len() == expected.len(),
        "guest private scratch inventory differs from admitted inputs"
    );
    for (observed, (index, binding_hash)) in scratch.iter().zip(expected) {
        ensure!(
            observed.input_index == index && observed.binding_hash == binding_hash,
            "guest private scratch coordinate differs from admitted input"
        );
        let name = format!("guest-scratch-{index:02}");
        let opened = private_parent
            .open_child_directory(OsStr::new(&name))?
            .context("guest private scratch binding disappeared")?;
        opened.require_owner_private_directory()?;
        ensure!(
            opened.identity()? == observed.directory,
            "guest private scratch binding changed inode"
        );
    }
    Ok(())
}

impl GuestOccurrenceOwner {
    /// Reserve the exact occurrence before importing an uploaded package.
    /// An incumbent child, even an incomplete one, is never adopted here.
    pub fn begin_authorized(
        occurrence: &lillux::PinnedDirectory,
        authorization: VerifiedGuestImportAuthorization,
    ) -> Result<Self> {
        authorization.require_fresh_admission()?;
        Self::begin_with_verified(occurrence, authorization)
    }

    #[cfg(test)]
    pub(crate) fn begin_authorized_at(
        occurrence: &lillux::PinnedDirectory,
        authorization: VerifiedGuestImportAuthorization,
        now_ms: i64,
    ) -> Result<Self> {
        authorization.require_fresh_admission_at(now_ms)?;
        Self::begin_with_verified(occurrence, authorization)
    }

    fn begin_with_verified(
        occurrence: &lillux::PinnedDirectory,
        authorization: VerifiedGuestImportAuthorization,
    ) -> Result<Self> {
        let admitted = authorization.authorization();
        Self::begin_inner(
            occurrence,
            &admitted.ticket,
            &admitted.context(),
            &admitted.guest_inputs,
            authorization.identity_digest(),
        )
    }

    /// Structural tests exercise owner and recovery mechanics without
    /// producing a controller signature. No production importer can call it.
    #[cfg(test)]
    pub(crate) fn begin(
        occurrence: &lillux::PinnedDirectory,
        ticket: &GuestImportTicket,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<Self> {
        Self::begin_inner(
            occurrence,
            ticket,
            context,
            inputs,
            &structural_fixture_authorization_digest(ticket)?,
        )
    }

    fn begin_inner(
        occurrence: &lillux::PinnedDirectory,
        ticket: &GuestImportTicket,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        authorization_sha256: &str,
    ) -> Result<Self> {
        occurrence.require_owner_private_directory()?;
        ticket.staging_expected(context, inputs)?;
        ensure!(
            lillux::valid_hash(authorization_sha256),
            "guest import authorization digest is invalid"
        );
        let root = occurrence.create_child(OsStr::new(OWNER_DIRECTORY), 0o700)?;
        let lock = root
            .try_lock_exclusive()?
            .context("new guest occurrence owner is already locked")?;
        lock.ensure_protects(&root)?;
        let record = GuestOccurrenceOwnerRecord {
            schema: 2,
            authorization_sha256: authorization_sha256.to_owned(),
            ticket_sha256: digest_ticket(ticket)?,
            occurrence_id: context.occurrence_id.to_owned(),
            occurrence_directory: occurrence.identity()?,
            owner_directory: root.identity()?,
        };
        let bytes = canonical_owner_record(&record)?;
        root.atomic_create_regular(OsStr::new(OWNER_RECORD_NAME), &bytes, 0o600)?
            .context("guest occurrence owner record already exists")?;
        Ok(Self {
            occurrence: occurrence.try_clone()?,
            root,
            _lock: lock,
            ticket: ticket.clone(),
            record,
        })
    }

    fn recheck(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<()> {
        self._lock.ensure_protects(&self.root)?;
        self.ticket.staging_expected(context, inputs)?;
        ensure!(
            self.record.schema == 2
                && lillux::valid_hash(&self.record.authorization_sha256)
                && self.record.ticket_sha256 == digest_ticket(&self.ticket)?
                && self.record.occurrence_id == context.occurrence_id
                && self.record.occurrence_directory == self.occurrence.identity()?
                && self.record.owner_directory == self.root.identity()?,
            "guest occurrence owner differs from retained authority"
        );
        ensure!(
            self.occurrence
                .open_child_directory(OsStr::new(OWNER_DIRECTORY))?
                .context("guest occurrence owner disappeared")?
                .identity()?
                == self.root.identity()?,
            "guest occurrence owner namespace changed inode"
        );
        let record = self
            .root
            .open_pinned_regular(OsStr::new(OWNER_RECORD_NAME), false)?
            .context("guest occurrence owner record is absent")?;
        ensure!(
            record.permission_mode()? == 0o600,
            "guest occurrence owner record mode changed"
        );
        let observed = record.observation()?;
        ensure!(
            observed.size() <= MAX_RECORD_BYTES,
            "guest occurrence owner record exceeds bound"
        );
        ensure!(
            record.read_stable_bounded(&observed, MAX_RECORD_BYTES)?
                == canonical_owner_record(&self.record)?,
            "guest occurrence owner record changed bytes"
        );
        Ok(())
    }

    /// Consumes the pre-upload owner and stages only into the dedicated
    /// process-private source filesystem, disjoint from durable occurrence
    /// journal and writable runtime. Failure leaves the create-only owner
    /// record in place and exposes no retry method.
    pub fn stage_uploaded_once(
        self,
        upload: &lillux::PinnedRegularFile,
        source: lillux::sandbox::LinuxPrivateSourceFilesystem,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<StagedGuestOccurrence> {
        self.stage_uploaded_into_source_root(
            upload,
            GuestSourceCustody::Live(source),
            context,
            inputs,
            deadline,
        )
    }

    /// Structural fixture only. Production import must supply the typed
    /// Lillux process-private source owner above, not an arbitrary directory.
    #[cfg(test)]
    pub(crate) fn stage_uploaded_with_source_root_for_test(
        self,
        upload: &lillux::PinnedRegularFile,
        source_root: &lillux::PinnedDirectory,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<StagedGuestOccurrence> {
        self.stage_uploaded_into_source_root(
            upload,
            GuestSourceCustody::StructuralFixture(source_root.try_clone()?),
            context,
            inputs,
            deadline,
        )
    }

    fn stage_uploaded_into_source_root(
        self,
        upload: &lillux::PinnedRegularFile,
        source: GuestSourceCustody,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<StagedGuestOccurrence> {
        let source_root = source.root();
        self.recheck(context, inputs)?;
        ensure!(
            self.root.entry_names()? == vec![std::ffi::OsString::from(OWNER_RECORD_NAME)],
            "guest occurrence already has staged or ambient content"
        );
        source_root.require_owner_private_directory()?;
        self.occurrence
            .require_disjoint_directory_tree(source_root)?;
        let imported = stage_ticketed_uploaded_guest_package(
            upload,
            source_root,
            &self.ticket,
            context,
            inputs,
            deadline,
        )?;
        Ok(StagedGuestOccurrence {
            owner: self,
            imported,
            source,
        })
    }
}

impl StagedGuestOccurrence {
    /// The owner journal already fences this occurrence against re-staging.
    /// Commit exact stage/runtime intent, then copy the verified base once while
    /// the original process-private source owner remains alive.
    pub fn install_base_once(
        self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<InstalledGuestBase> {
        self.owner.recheck(context, inputs)?;
        let runtime = self
            .owner
            .occurrence
            .create_child(OsStr::new(CANDIDATE_RUNTIME_DIRECTORY), 0o700)?;
        runtime.require_owner_private_directory()?;
        self.owner.root.require_disjoint_directory_tree(&runtime)?;
        let prepared = prepare_guest_base_install_locked(
            &self.imported,
            context,
            inputs,
            self.owner.root.try_clone()?,
            &runtime,
            self.owner._lock.clone(),
        )?;
        let intent_identity = prepared.identity.clone();
        prepared.install_once()?;
        let children = inspect_installed_runtime_children(&runtime)?;
        // The imported package is no longer writable before any launch
        // preparation can consume these exact staged descriptors. A failed
        // seal leaves the create-only installation intent uncertain; it does
        // not authorize a second import or another base copy.
        let source = self.source.seal_after_install()?;
        Ok(InstalledGuestBase {
            owner: self.owner,
            imported: self.imported,
            runtime,
            intent_identity,
            children,
            _source: Some(source),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestBaseInstallIntent {
    schema: u32,
    ticket_sha256: String,
    stage: GuestStageIdentity,
    journal_directory: lillux::PinnedDirectoryIdentity,
    candidate_runtime: lillux::PinnedDirectoryIdentity,
}

/// The only local path that may perform the copy after create-only intent.
/// Dropping this value does not erase the intent or grant a retry.
struct PreparedGuestBaseInstall<'a> {
    imported: &'a TicketedGuestImport,
    context: &'a GuestImportContext<'a>,
    inputs: &'a ExternalGuestInputProjection,
    runtime: &'a lillux::PinnedDirectory,
    journal: lillux::PinnedDirectory,
    _lock: lillux::PinnedDirectoryLock,
    identity: GuestBaseInstallIntentIdentity,
}

impl PreparedGuestBaseInstall<'_> {
    /// Consuming the prepared value is process-local one-shot. The durable
    /// create-only intent prevents another prepare/recovery from rerunning it.
    pub fn install_once(self) -> Result<()> {
        self._lock.ensure_protects(&self.journal)?;
        verify_committed_intent(
            self.imported,
            self.context,
            self.inputs,
            &self.journal,
            self.runtime,
            &self.identity,
        )?;
        self.imported
            .install_base_into(self.context, self.inputs, self.runtime)
    }
}

fn prepare_guest_base_install_locked<'a>(
    imported: &'a TicketedGuestImport,
    context: &'a GuestImportContext<'a>,
    inputs: &'a ExternalGuestInputProjection,
    journal: lillux::PinnedDirectory,
    runtime: &'a lillux::PinnedDirectory,
    lock: lillux::PinnedDirectoryLock,
) -> Result<PreparedGuestBaseInstall<'a>> {
    lock.ensure_protects(&journal)?;
    ensure!(
        journal
            .open_pinned_regular(OsStr::new(RECORD_NAME), false)?
            .is_none(),
        "guest installation intent already exists"
    );
    imported.recheck_for_adoption(context, inputs)?;
    let intent = GuestBaseInstallIntent {
        schema: 1,
        ticket_sha256: ticket_digest(imported)?,
        stage: imported.stage_identity()?,
        journal_directory: journal.identity()?,
        candidate_runtime: runtime.identity()?,
    };
    let bytes = canonical_record(&intent)?;
    let record = journal
        .atomic_create_regular(OsStr::new(RECORD_NAME), &bytes, 0o600)?
        .context("guest installation intent already exists")?;
    let marker = GuestBaseInstallMarker {
        schema: 1,
        journal_directory: intent.journal_directory,
        record_file: lillux::pinned_regular_file_identity(&record)?,
        record_sha256: lillux::sha256_hex(&bytes),
    };
    let marker_bytes = canonical_marker(&marker)?;
    let marker_file = imported
        .root()
        .atomic_create_regular(OsStr::new(STAGE_MARKER_NAME), &marker_bytes, 0o600)?
        .context("ticketed guest stage already has an installation owner")?;
    let identity = GuestBaseInstallIntentIdentity {
        schema: 1,
        journal_directory: marker.journal_directory,
        record_file: marker.record_file,
        record_sha256: marker.record_sha256,
        stage_marker_file: lillux::pinned_regular_file_identity(&marker_file)?,
    };
    Ok(PreparedGuestBaseInstall {
        imported,
        context,
        inputs,
        runtime,
        journal,
        _lock: lock,
        identity,
    })
}

fn verify_committed_intent(
    imported: &TicketedGuestImport,
    context: &GuestImportContext<'_>,
    inputs: &ExternalGuestInputProjection,
    journal: &lillux::PinnedDirectory,
    runtime: &lillux::PinnedDirectory,
    identity: &GuestBaseInstallIntentIdentity,
) -> Result<GuestBaseInstallIntent> {
    let marker = imported
        .root()
        .open_pinned_regular(OsStr::new(STAGE_MARKER_NAME), false)?
        .context("guest stage installation owner marker is absent")?;
    ensure!(
        lillux::pinned_regular_file_identity(&marker.try_clone_descriptor()?)?
            == identity.stage_marker_file
            && marker.permission_mode()? == 0o600,
        "guest stage installation owner marker inode or mode changed"
    );
    let marker_observation = marker.observation()?;
    ensure!(
        marker_observation.size() <= MAX_RECORD_BYTES,
        "guest stage installation owner marker exceeds bound"
    );
    let marker_bytes = marker.read_stable_bounded(&marker_observation, MAX_RECORD_BYTES)?;
    let expected_marker = GuestBaseInstallMarker {
        schema: 1,
        journal_directory: identity.journal_directory,
        record_file: identity.record_file,
        record_sha256: identity.record_sha256.clone(),
    };
    ensure!(
        marker_bytes == canonical_marker(&expected_marker)?,
        "guest stage installation owner marker differs from retained intent"
    );
    let record = journal
        .open_pinned_regular(OsStr::new(RECORD_NAME), false)?
        .context("guest installation intent is absent")?;
    ensure!(
        lillux::pinned_regular_file_identity(&record.try_clone_descriptor()?)?
            == identity.record_file
            && record.permission_mode()? == 0o600,
        "guest installation intent inode or mode changed"
    );
    let observed = record.observation()?;
    ensure!(
        observed.size() <= MAX_RECORD_BYTES,
        "guest installation intent exceeds bound"
    );
    let bytes = record.read_stable_bounded(&observed, MAX_RECORD_BYTES)?;
    ensure!(
        lillux::sha256_hex(&bytes) == identity.record_sha256,
        "guest installation intent changed bytes"
    );
    let intent: GuestBaseInstallIntent = serde_json::from_slice(&bytes)?;
    ensure!(
        canonical_record(&intent)? == bytes
            && intent.schema == 1
            && intent.journal_directory == identity.journal_directory
            && intent.stage == imported.stage_identity()?
            && intent.ticket_sha256 == ticket_digest(imported)?
            && intent.candidate_runtime == runtime.identity()?,
        "guest installation intent differs from retained authority"
    );
    imported.recheck_for_adoption(context, inputs)?;
    Ok(intent)
}

fn ticket_digest(imported: &TicketedGuestImport) -> Result<String> {
    digest_ticket(imported.ticket())
}

fn digest_ticket(ticket: &GuestImportTicket) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&serde_json::to_value(ticket)?)?.as_bytes(),
    ))
}

#[cfg(test)]
fn structural_fixture_authorization_digest(ticket: &GuestImportTicket) -> Result<String> {
    Ok(lillux::sha256_hex(
        format!("unqualified-guest-owner-fixture:{}", digest_ticket(ticket)?).as_bytes(),
    ))
}

fn canonical_owner_record(record: &GuestOccurrenceOwnerRecord) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(record)?)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "guest occurrence owner record exceeds bound"
    );
    Ok(bytes)
}

fn canonical_launch_record(record: &GuestSupervisorLaunchIntent) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(record)?)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "guest supervisor launch intent exceeds bound"
    );
    Ok(bytes)
}

fn create_launch_intent_record(
    owner_root: &lillux::PinnedDirectory,
    bytes: &[u8],
) -> Result<(lillux::PinnedRegularFileIdentity, String)> {
    owner_root.require_owner_private_directory()?;
    let parsed: GuestSupervisorLaunchIntent = serde_json::from_slice(bytes)?;
    ensure!(
        canonical_launch_record(&parsed)? == bytes,
        "guest supervisor launch intent is not canonical"
    );
    let record = owner_root
        .atomic_create_regular(OsStr::new(SUPERVISOR_LAUNCH_RECORD_NAME), bytes, 0o600)?
        .context("guest supervisor launch intent already exists")?;
    Ok((
        lillux::pinned_regular_file_identity(&record)?,
        lillux::sha256_hex(bytes),
    ))
}

#[cfg(test)]
pub(crate) fn create_launch_intent_record_for_test(
    owner_root: &lillux::PinnedDirectory,
    bytes: &[u8],
) -> Result<(lillux::PinnedRegularFileIdentity, String)> {
    create_launch_intent_record(owner_root, bytes)
}

#[cfg(test)]
mod recovery_binding_tests {
    use super::*;
    use ryeos_state::external_execution::transport::{
        ExternalNetworkInputPolicy, ExternalNetworkInputSelection,
    };

    fn network_policy(
        resolver: &std::path::Path,
        hosts: &std::path::Path,
    ) -> ExternalNetworkInputPolicy {
        ExternalNetworkInputPolicy {
            resolver: ExternalNetworkInputSelection {
                source: resolver.to_str().unwrap().into(),
                max_bytes: 64 * 1024,
            },
            hosts: ExternalNetworkInputSelection {
                source: hosts.to_str().unwrap().into(),
                max_bytes: 64 * 1024,
            },
        }
    }

    #[test]
    fn supervisor_network_mounts_keep_exact_bytes_after_source_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let resolver = temporary.path().join("resolver");
        let resolver_link = temporary.path().join("resolver-link");
        let hosts = temporary.path().join("hosts");
        let resolver_bytes = b"nameserver 127.0.0.1\n";
        let hosts_bytes = b"127.0.0.1 localhost\n";
        std::fs::write(&resolver, resolver_bytes).unwrap();
        std::os::unix::fs::symlink(&resolver, &resolver_link).unwrap();
        std::fs::write(&hosts, hosts_bytes).unwrap();
        let (mounted, destinations) =
            capture_supervisor_network_mounts(&network_policy(&resolver_link, &hosts)).unwrap();
        assert_eq!(destinations, [resolver_link, hosts.clone()]);
        std::fs::write(&resolver, b"nameserver 192.0.2.1\n").unwrap();
        std::fs::write(&hosts, b"192.0.2.1 changed\n").unwrap();
        assert_eq!(
            lillux::read_sealed_inherited_descriptor(
                mounted[0].inherited_descriptor().unwrap(),
                64 * 1024
            )
            .unwrap(),
            resolver_bytes
        );
        assert_eq!(
            lillux::read_sealed_inherited_descriptor(
                mounted[1].inherited_descriptor().unwrap(),
                64 * 1024
            )
            .unwrap(),
            hosts_bytes
        );
    }

    #[test]
    fn supervisor_network_mounts_refuse_overlap_and_invalid_sources() {
        let temporary = tempfile::tempdir().unwrap();
        let resolver = temporary.path().join("resolver");
        let hosts = temporary.path().join("hosts");
        std::fs::write(&resolver, b"nameserver 127.0.0.1\n").unwrap();
        std::fs::write(&hosts, b"127.0.0.1 localhost\n").unwrap();
        let mut policy = network_policy(&resolver, &hosts);
        for bad_path in [
            resolver.clone(),
            PathBuf::from(SUPERVISOR_STAGE_MOUNT_DESTINATION),
            PathBuf::from(SUPERVISOR_STAGE_MOUNT_DESTINATION).join("network"),
        ] {
            policy.hosts.source = bad_path.to_str().unwrap().into();
            assert!(capture_supervisor_network_mounts(&policy).is_err());
        }
        policy = network_policy(&resolver, &hosts);
        policy.resolver.max_bytes = 1;
        assert!(capture_supervisor_network_mounts(&policy).is_err());
        policy = network_policy(temporary.path(), &hosts);
        assert!(capture_supervisor_network_mounts(&policy).is_err());
        let dangling = temporary.path().join("dangling");
        std::os::unix::fs::symlink("missing", &dangling).unwrap();
        policy = network_policy(&dangling, &hosts);
        assert!(capture_supervisor_network_mounts(&policy).is_err());
        std::fs::write(&resolver, b"not a resolver\n").unwrap();
        policy = network_policy(&resolver, &hosts);
        assert!(capture_supervisor_network_mounts(&policy).is_err());
    }

    #[test]
    fn released_supervisor_receipt_rejects_wrong_process_target_or_mounts() {
        let target = lillux::LinuxSandboxAppliedLaunchCommitments {
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: [3; 32],
            cwd_sha256: [4; 32],
        };
        let mounts = lillux::LinuxSandboxMountPreparationCommitments {
            schema: 1,
            mount_count: 1,
            destination_access_sha256: [5; 32],
        };
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 42,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: target.executable_sha256,
            argv_sha256: target.argv_sha256,
            environment_sha256: target.environment_sha256,
            cwd_sha256: target.cwd_sha256,
            post_release_mount_view: mounts.clone(),
        };
        check_applied_supervisor_receipt(&receipt, &target, &mounts, 42).unwrap();
        assert!(check_applied_supervisor_receipt(&receipt, &target, &mounts, 43).is_err());
        for changed in [
            lillux::LinuxSandboxAppliedLaunchReceipt {
                executable_sha256: [9; 32],
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                argv_sha256: [9; 32],
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                environment_sha256: [9; 32],
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                cwd_sha256: [9; 32],
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                    destination_access_sha256: [9; 32],
                    ..mounts.clone()
                },
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                    mount_count: 2,
                    ..mounts.clone()
                },
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                namespace_pid: 2,
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                effective_uid: 2,
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                no_new_privs: false,
                ..receipt.clone()
            },
            lillux::LinuxSandboxAppliedLaunchReceipt {
                seccomp_mode: 0,
                ..receipt.clone()
            },
        ] {
            assert!(check_applied_supervisor_receipt(&changed, &target, &mounts, 42).is_err());
        }
    }

    #[test]
    fn retained_child_binding_refuses_detached_or_replaced_inode() {
        let temporary = tempfile::tempdir().unwrap();
        let occurrence = lillux::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let child = occurrence
            .create_child(OsStr::new("retained"), 0o700)
            .unwrap();
        ensure_child_binding(&occurrence, "retained", &child).unwrap();
        std::fs::rename(
            temporary.path().join("retained"),
            temporary.path().join("detached"),
        )
        .unwrap();
        assert!(ensure_child_binding(&occurrence, "retained", &child).is_err());
        occurrence
            .create_child(OsStr::new("retained"), 0o700)
            .unwrap();
        assert!(ensure_child_binding(&occurrence, "retained", &child).is_err());
    }
}

fn canonical_record(intent: &GuestBaseInstallIntent) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(intent)?)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "guest installation intent exceeds bound"
    );
    Ok(bytes)
}

fn canonical_marker(marker: &GuestBaseInstallMarker) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(marker)?)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "guest installation owner marker exceeds bound"
    );
    Ok(bytes)
}
