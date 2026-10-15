//! Generic protected supervisor process composition.
//!
//! Provider adapters arrange exact inherited descriptors and lifecycle. This
//! module owns no cloud API, credential, project pathname, or deployment
//! convention. A fresh state root can launch once; after durable launch intent,
//! reopen is recovery-only and can never manufacture a replacement candidate.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
use lillux_sqlite::{
    OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority, OriginalNativeFactory,
};
use ryeos_external_execution::launcher::{
    ExternalCandidateLauncherSpec, prepare_launcher_subprocess_request,
};
use ryeos_external_execution::launcher_protocol::{
    LiveInheritedExternalCandidateSupervisor, launch_prepared_external_candidate_supervisor,
};
use ryeos_external_execution::transport::{
    ExternalCandidateTransportDriver, ExternalExecutionChannelTransport, ExternalTransportExchange,
    ExternalTransportProgress, ExternalTransportStepFailure,
};
use ryeos_external_execution_contract::guest_supervisor_descriptors::rebind_fixed_guest_descriptors;
use ryeos_external_execution_contract::{ExternalGuestInputProjection, GuestMountContentAuthority};
use ryeos_state::external_execution::{
    ExecutionChannelBinding, ExecutionChannelPayload, SignedExecutionFrame,
};
use ryeos_state::external_execution::guest_journal::{
    GuestJournalStoreIdentity, LiveGuestJournal, PreparedGuestJournal, ReservedGuestJournal,
};
use ryeos_state::external_execution::native_journal::settle_journal_refusal;
use ryeos_state::external_execution::supervisor_journal::{
    AttachedExternalSupervisorJournal, ExternalSupervisorJournalRecovery,
    ExternalSupervisorJournalStoreIdentity, ExternalSupervisorTransportCapture,
    LaunchIntentExternalSupervisorJournal, PreparedExternalSupervisorJournal,
    RecoveredExternalSupervisorJournal,
};
use ryeos_state::external_execution::transport::{
    ExternalCapturedNetworkInputs, ExternalNetworkInputPolicy, ExternalNetworkInputSelection,
    ExternalSupervisorBootstrap, ExternalSupervisorNativeEnvelope,
};
use ryeos_state::operational::{CarriedOwners, RetainTerminal};
use serde::{Deserialize, Serialize};

use crate::native_host::{SupervisorJournalEnvelope, SupervisorNativeHost};
use crate::relay_transport::OriginalRelayMailbox;
use crate::{
    AttachedExternalExecutionTransport, attach_external_execution_channel_exact,
    select_exchange_transport,
};

const OUTER_DIRECTORY: &str = "outer";
const GUEST_DIRECTORY: &str = "guest";
const STATE_ANCHOR: &str = "supervisor-state.json";
const MAX_STATE_ANCHOR_BYTES: u64 = 64 * 1024;
const IDLE_POLL: lillux::time::Duration = lillux::time::Duration::from_millis(10);

/// Exact authorities supplied by an independently admitted lifecycle adapter.
/// Paths retained by these Lillux values are diagnostic only; all mutations,
/// reads, mounts and child execution remain descriptor-relative.
pub struct ExternalCandidateSupervisorInputs {
    /// Original sealed input. Never rewrite its descriptor coordinates before
    /// committing it to the protected journal.
    pub bootstrap: ExternalSupervisorBootstrap,
    /// Same semantic input, rebound only to the actual adopted descriptors.
    pub execution_guest_inputs: ExternalGuestInputProjection,
    pub state_root: lillux::PinnedDirectory,
    pub candidate_runtime: lillux::PinnedDirectory,
    pub candidate_private_parent: lillux::PinnedDirectory,
    pub relay_mailbox: Option<lillux::PinnedDirectory>,
    pub staged_source: Option<lillux::PinnedDirectory>,
    pub launcher: lillux::InheritedDescriptorAuthority,
    pub workspace_outputs: Option<lillux::InheritedDescriptorAuthority>,
    pub runtime_mounts: Vec<lillux::InheritedDescriptorAuthority>,
    pub content_records: Vec<lillux::InheritedDescriptorAuthority>,
    /// Verified activation inputs a leased guest's Ready names, in order.
    /// Present exactly when the bootstrap asks for a capability report.
    pub ready_boot_inputs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExternalCandidateSupervisorOutcome {
    ExportApplied,
    CommandTerminatedApplied,
    RevokedApplied,
    RevokedClaimedUnknown,
    ExecutionDeadline,
    ChannelExpired,
    RecoveryOnly {
        binding_digest: String,
        launch_intent_digest: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalSupervisorStateAnchor {
    schema: u32,
    state_root_identity: lillux::PinnedDirectoryIdentity,
    outer_store_identity: ExternalSupervisorJournalStoreIdentity,
    /// The native envelope sealed into the original bootstrap. Recovery
    /// compares the incoming bootstrap's envelope with it before issuing the
    /// native host, because the retained bootstrap itself can only be read
    /// through a journal open that host must already fund.
    native_journal: ExternalSupervisorNativeEnvelope,
}

enum RetainedSupervisorJournal {
    Prepared(PreparedExternalSupervisorJournal),
    Attached(AttachedExternalSupervisorJournal),
    /// Launch intent recorded by this process, before its launcher runs.
    Launched(LaunchIntentExternalSupervisorJournal),
    /// Launch intent reopened by recovery; evidence only.
    LaunchIntent(RecoveredExternalSupervisorJournal),
}

impl RetainedSupervisorJournal {
    fn transport_capture(&self) -> &ExternalSupervisorTransportCapture {
        match self {
            Self::Prepared(journal) => journal.transport_capture(),
            Self::Attached(journal) => journal.transport_capture(),
            Self::Launched(journal) => journal.transport_capture(),
            Self::LaunchIntent(journal) => journal.transport_capture(),
        }
    }

    fn bootstrap(&self) -> &ExternalSupervisorBootstrap {
        match self {
            Self::Prepared(journal) => journal.bootstrap(),
            Self::Attached(journal) => journal.bootstrap(),
            Self::Launched(journal) => journal.bootstrap(),
            Self::LaunchIntent(journal) => journal.bootstrap(),
        }
    }

    /// Close whichever stage this is; a refusal keeps the journal.
    fn close(self) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        match self {
            Self::Prepared(journal) => journal
                .close()
                .map_err(|(journal, error)| (Self::Prepared(journal), error)),
            Self::Attached(journal) => journal
                .close()
                .map_err(|(journal, error)| (Self::Attached(journal), error)),
            Self::Launched(journal) => journal
                .close()
                .map_err(|(journal, error)| (Self::Launched(journal), error)),
            Self::LaunchIntent(journal) => journal
                .close()
                .map_err(|(journal, error)| (Self::LaunchIntent(journal), error)),
        }
    }
}

/// The native surface every journal open, close and refusal of this process
/// runs on: the supervisor host's factory and destination, and its census.
struct JournalNative<'h> {
    factory: &'h Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
    retain: RetainTerminal<'h>,
}

impl JournalNative<'_> {
    fn destination(&self) -> Arc<dyn OriginalNativeDestinationAuthority> {
        Arc::clone(&self.destination)
    }

    /// Hand every journal terminal `error` carries to the census. Whatever
    /// cannot be settled stays carried by the returned error.
    fn settle(&self, error: anyhow::Error) -> anyhow::Error {
        settle_journal_refusal(error, self.retain)
    }

    /// Record one journal close in the census. A refused close keeps the
    /// journal, and a refused retention keeps the terminal, inside the error.
    fn close<J: Send + 'static>(
        &self,
        label: &str,
        closed: std::result::Result<OriginalNativeConnectionTerminal, (J, anyhow::Error)>,
    ) -> Result<()> {
        match closed {
            Ok(terminal) => match (self.retain)(terminal) {
                Ok(()) => Ok(()),
                Err((terminal, error)) => Err(CarriedOwners::carry(
                    error.context(format!(
                        "{label} terminal was not retained; the terminal is kept"
                    )),
                    terminal,
                )),
            },
            Err((journal, error)) => Err(CarriedOwners::carry(
                error.context(format!("{label} close refused; the journal is kept")),
                journal,
            )),
        }
    }

    /// Close a journal that `error` abandons and return `error`, which also
    /// carries the journal or its terminal when either was refused.
    fn refuse<J: Send + 'static>(
        &self,
        label: &str,
        error: anyhow::Error,
        closed: std::result::Result<OriginalNativeConnectionTerminal, (J, anyhow::Error)>,
    ) -> anyhow::Error {
        match self.close(label, closed) {
            Ok(()) => error,
            Err(close) => CarriedOwners::carry(error.context(format!("{close:#}")), close),
        }
    }
}

/// The protected state root under its exclusive lock with both journal
/// directories pinned, before any journal opens.
struct LockedSupervisorRoot {
    state_root: lillux::PinnedDirectory,
    state_lock: lillux::PinnedDirectoryLock,
    outer_directory: lillux::PinnedDirectory,
    guest_directory: lillux::PinnedDirectory,
    origin: SupervisorRootOrigin,
}

enum SupervisorRootOrigin {
    /// A fresh root. HTTP sampled its network inputs before any entry was
    /// created; relay captures its mailbox when the journal is created.
    Fresh {
        network_inputs: Option<ExternalCapturedNetworkInputs>,
    },
    /// A retained root whose anchor already matched its exact identity.
    Anchored(ExternalSupervisorStateAnchor),
}

struct ProtectedSupervisorState {
    state_root: lillux::PinnedDirectory,
    _state_lock: lillux::PinnedDirectoryLock,
    guest_directory: lillux::PinnedDirectory,
    journal: Option<RetainedSupervisorJournal>,
    /// Reserved guest inode awaiting launch intent. It takes no native ticket
    /// but keeps the host's factory alive, so it ends before the host does.
    guest_reservation: Option<ReservedGuestJournal>,
}

type LiveControlDriver = ExternalCandidateTransportDriver<
    LiveInheritedExternalCandidateSupervisor,
    AttachedExternalExecutionTransport,
>;

struct LiveExternalCandidateSupervisor {
    driver: Option<LiveControlDriver>,
    /// The live guest journal, handed back once the launcher was reaped.
    guest_journal: Option<LiveGuestJournal>,
    execution_deadline: lillux::time::MonotonicDeadline,
    expiry_deadline: lillux::time::MonotonicDeadline,
}

/// Run one exact external candidate until its export is durably applied, its
/// cancellation is locally settled, or a signed lifecycle deadline expires.
/// Network ambiguity retries only the identical retained signed frame.
///
/// Both journals open on one native host issued from the envelope sealed into
/// the bootstrap. Every exit path closes each journal into the host's census
/// before the host finishes.
pub fn run_external_candidate_supervisor(
    inputs: ExternalCandidateSupervisorInputs,
) -> Result<ExternalCandidateSupervisorOutcome> {
    validate_authorities(&inputs)?;
    let ExternalCandidateSupervisorInputs {
        bootstrap,
        execution_guest_inputs,
        state_root,
        candidate_runtime,
        candidate_private_parent,
        relay_mailbox,
        staged_source,
        launcher,
        workspace_outputs,
        runtime_mounts,
        content_records: _,
        ready_boot_inputs,
    } = inputs;
    let root = LockedSupervisorRoot::lock(state_root, &bootstrap, relay_mailbox.is_some())?;
    // Issued once, before any journal opens, under the state root's exclusive
    // lock. The host is not Send: it and every journal it funds stay on this
    // thread.
    let host = SupervisorNativeHost::issue(
        SupervisorJournalEnvelope::from(bootstrap.native_journal),
        &root.state_root,
        &root.state_lock,
        &[&root.outer_directory, &root.guest_directory],
    )?;
    let (outcome, closed) = {
        let retain = |terminal: OriginalNativeConnectionTerminal| host.retain_terminal(terminal);
        match (host.factory(), host.destination()) {
            (Ok(factory), Ok(destination)) => {
                let native = JournalNative {
                    factory,
                    destination,
                    retain: &retain,
                };
                let mut state: Option<ProtectedSupervisorState> = None;
                let mut live: Option<LiveExternalCandidateSupervisor> = None;
                let outcome = (|| -> Result<ExternalCandidateSupervisorOutcome> {
                    let opened = state.insert(ProtectedSupervisorState::open(
                        root,
                        bootstrap,
                        relay_mailbox.as_ref(),
                        &native,
                    )?);
                    if let Some(outcome) = opened.recovery_only_outcome()? {
                        return Ok(outcome);
                    }
                    let started = live.insert(LiveExternalCandidateSupervisor::start(
                        opened,
                        &native,
                        execution_guest_inputs,
                        candidate_runtime,
                        candidate_private_parent,
                        relay_mailbox,
                        staged_source,
                        launcher,
                        workspace_outputs,
                        runtime_mounts,
                        ready_boot_inputs,
                    )?);
                    started.run()
                })();
                let outcome = outcome.map_err(|error| native.settle(error));
                let closed = close_supervisor_journals(&native, state, live);
                (outcome, closed)
            }
            (Err(error), _) | (_, Err(error)) => (
                Err(error.context("supervisor native host has no journal factory")),
                Ok(()),
            ),
        }
    };
    let (retained_host, finish_refusal) = match host.finish() {
        Ok(_facts) => (None, None),
        Err((retained, error)) => (retained, Some(error)),
    };
    let result = match (outcome, closed, finish_refusal) {
        (Ok(outcome), Ok(()), None) => Ok(outcome),
        (outcome, closed, finish_refusal) => {
            let mut error = match outcome {
                Ok(outcome) => anyhow::anyhow!(
                    "external supervisor reached {outcome:?} but its native shutdown was refused"
                ),
                Err(error) => error,
            };
            if let Err(closed) = closed {
                error = CarriedOwners::carry(error.context(format!("{closed:#}")), closed);
            }
            if let Some(finish) = finish_refusal {
                error = error.context(format!(
                    "supervisor native host finish refused; the host is kept until this run ends: {finish:#}"
                ));
            }
            Err(error)
        }
    };
    // A refused host is not Send, so it cannot ride the error; it ends here,
    // after the refusal naming it was built.
    drop(retained_host);
    result
}

/// Close every journal this process still holds into the native census: the
/// live guest journal the reaped launcher handed back, any guest reservation,
/// then the outer journal. Each refused close keeps its journal inside the
/// returned error.
fn close_supervisor_journals(
    native: &JournalNative<'_>,
    state: Option<ProtectedSupervisorState>,
    live: Option<LiveExternalCandidateSupervisor>,
) -> Result<()> {
    let mut failures = Vec::new();
    if let Some(live) = live {
        failures.extend(live.close_journals(native));
    }
    if let Some(state) = state
        && let Err(error) = state.close_journals(native)
    {
        failures.push(error);
    }
    if failures.is_empty() {
        return Ok(());
    }
    let summary = failures
        .iter()
        .map(|failure| format!("{failure:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut error = anyhow::anyhow!("external supervisor journal shutdown refused: {summary}");
    for failure in failures {
        error = CarriedOwners::carry(error, failure);
    }
    Err(error)
}

fn validate_authorities(inputs: &ExternalCandidateSupervisorInputs) -> Result<()> {
    inputs.bootstrap.validate()?;
    inputs.execution_guest_inputs.validate()?;
    ensure!(
        inputs.relay_mailbox.is_some() == inputs.bootstrap.controller.relay().is_ok(),
        "relay mailbox presence differs from sealed transport selection"
    );
    ensure!(
        inputs.ready_boot_inputs.is_some() == inputs.bootstrap.report_capabilities,
        "leased guest ready inputs differ from the bootstrap's capability-report selection"
    );
    ensure!(
        !(inputs.bootstrap.report_capabilities && inputs.bootstrap.controller.relay().is_ok()),
        "a leased guest reports capabilities only over the HTTP or stream carrier"
    );
    if let Some(mailbox) = inputs.relay_mailbox.as_ref() {
        let staged = inputs
            .staged_source
            .as_ref()
            .context("relay supervisor has no original staged source")?;
        inputs
            .bootstrap
            .candidate_program
            .validate_guest_inputs(&inputs.execution_guest_inputs)?;
        for protected in [
            &inputs.state_root,
            &inputs.candidate_runtime,
            &inputs.candidate_private_parent,
            staged,
        ] {
            mailbox.require_disjoint_directory_tree(protected)?;
        }
        mailbox.require_owner_private_directory()?;
        ensure!(
            mailbox.path() == std::path::Path::new(&inputs.bootstrap.relay_mailbox_root()?),
            "relay mailbox path differs from sealed occurrence"
        );
    }
    ensure!(
        inputs.execution_guest_inputs.identity_digest()? == inputs.bootstrap.guest_input_identity,
        "external execution projection changed sealed guest-input identity"
    );
    inputs.state_root.require_owner_private_directory()?;
    inputs
        .candidate_private_parent
        .require_owner_private_directory()?;
    inputs
        .state_root
        .require_disjoint_directory_tree(&inputs.candidate_runtime)?;
    inputs
        .state_root
        .require_disjoint_directory_tree(&inputs.candidate_private_parent)?;
    inputs
        .candidate_runtime
        .require_disjoint_directory_tree(&inputs.candidate_private_parent)?;
    inputs.launcher.require_owned_executable()?;
    let launcher_observation = inputs.launcher.regular_file_observation()?;
    ensure!(
        inputs
            .launcher
            .digest_regular_file_stable_exact(&launcher_observation)?
            == inputs.bootstrap.launcher_artifact_hash,
        "external candidate launcher changed its admitted artifact identity"
    );
    match (
        &inputs.workspace_outputs,
        &inputs.execution_guest_inputs.workspace_outputs,
    ) {
        (None, None) => {}
        (Some(authority), Some(outputs)) => {
            ensure!(
                authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    == outputs.descriptor,
                "external workspace-output descriptor changed"
            );
            let observation = authority.regular_file_observation()?;
            ensure!(
                observation.size() == outputs.bytes
                    && authority.digest_regular_file_stable_exact(&observation)?
                        == outputs.authority_hash,
                "external workspace-output authority changed"
            );
        }
        _ => bail!("external workspace-output authority presence changed"),
    }
    ensure!(
        inputs.runtime_mounts.len() == inputs.execution_guest_inputs.inputs.len(),
        "external candidate guest input authority count changed"
    );
    ensure!(
        inputs.content_records.len() == inputs.execution_guest_inputs.record_descriptors().count(),
        "external candidate content record authority count changed"
    );
    let mut directory_inputs: Vec<lillux::PinnedDirectory> = Vec::new();
    let mut content_records = inputs.content_records.iter();
    for (mount, projection) in inputs
        .runtime_mounts
        .iter()
        .zip(&inputs.execution_guest_inputs.inputs)
    {
        ensure!(
            mount.inherited_descriptor().map_err(anyhow::Error::msg)? == projection.descriptor,
            "external guest runtime descriptor changed"
        );
        match projection.kind {
            ryeos_external_execution_contract::GuestMountKind::Directory => {
                let pinned = mount.try_clone_pinned_directory(std::path::PathBuf::from(
                    "<external-guest-input>",
                ))?;
                inputs.state_root.require_disjoint_directory_tree(&pinned)?;
                inputs
                    .candidate_runtime
                    .require_disjoint_directory_tree(&pinned)?;
                inputs
                    .candidate_private_parent
                    .require_disjoint_directory_tree(&pinned)?;
                if let Some(mailbox) = inputs.relay_mailbox.as_ref() {
                    mailbox.require_disjoint_directory_tree(&pinned)?;
                }
                for existing in &directory_inputs {
                    existing.require_disjoint_directory_tree(&pinned)?;
                }
                if projection.access
                    == ryeos_external_execution_contract::GuestMountAccess::PrivateWritable
                {
                    pinned.require_owner_private_directory()?;
                }
                directory_inputs.push(pinned);
            }
            ryeos_external_execution_contract::GuestMountKind::RegularFile => {
                let observation = mount.regular_file_observation()?;
                ensure!(
                    observation.size() == projection.bytes,
                    "external guest file input changed its admitted size"
                );
            }
        }
        match &projection.content_authority {
            ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                manifest_kind,
                manifest_hash,
                manifest_descriptor,
                manifest_bytes,
            } => {
                let manifest = content_records
                    .next()
                    .context("external guest product manifest is absent")?;
                ensure!(
                    manifest
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?
                        == *manifest_descriptor,
                    "external guest product manifest descriptor changed"
                );
                ryeos_state::external_content::realization_verification::verify_staged_external_realization(
                    mount, manifest,
                    match manifest_kind {
                        ryeos_external_execution_contract::GuestProductManifestKind::Content =>
                            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                        ryeos_external_execution_contract::GuestProductManifestKind::LargeContent =>
                            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                    },
                    manifest_hash, *manifest_bytes,
                    match projection.kind {
                        ryeos_external_execution_contract::GuestMountKind::Directory =>
                            ryeos_state::objects::ExternalContentKind::Tree,
                        ryeos_external_execution_contract::GuestMountKind::RegularFile =>
                            ryeos_state::objects::ExternalContentKind::File,
                    },
                    projection.bytes,
                )?;
            }
            ryeos_external_execution_contract::GuestMountContentAuthority::RawFile { sha256 } => {
                let observation = mount.regular_file_observation()?;
                ensure!(
                    mount.digest_regular_file_stable_exact(&observation)? == *sha256
                        && Some(observation.portable_mode()?) == projection.normalized_mode,
                    "external guest raw file changed its admitted bytes or mode"
                );
            }
            GuestMountContentAuthority::SourceClosure {
                binding_hash,
                binding_descriptor,
                binding_bytes,
                manifest_hash,
                manifest_descriptor,
                manifest_bytes,
            } => {
                let binding = content_records
                    .next()
                    .context("external guest source binding record is absent")?;
                let manifest = content_records
                    .next()
                    .context("external guest source manifest record is absent")?;
                verify_source_input(
                    mount,
                    projection,
                    binding,
                    manifest,
                    binding_hash,
                    *binding_descriptor,
                    *binding_bytes,
                    manifest_hash,
                    *manifest_descriptor,
                    *manifest_bytes,
                )?;
            }
            ryeos_external_execution_contract::GuestMountContentAuthority::PrivateScratch {
                ..
            } => {}
        }
    }
    ensure!(
        content_records.next().is_none(),
        "external guest content record authority is extra"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_source_input(
    mount: &lillux::InheritedDescriptorAuthority,
    projection: &ryeos_external_execution_contract::GuestMountInput,
    binding: &lillux::InheritedDescriptorAuthority,
    manifest: &lillux::InheritedDescriptorAuthority,
    binding_hash: &str,
    binding_descriptor: u32,
    binding_bytes: u64,
    manifest_hash: &str,
    manifest_descriptor: u32,
    manifest_bytes: u64,
) -> Result<()> {
    let read_record = |authority: &lillux::InheritedDescriptorAuthority,
                       descriptor: u32,
                       bytes: u64,
                       maximum: u64|
     -> Result<Vec<u8>> {
        ensure!(
            authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?
                == descriptor,
            "external guest source record descriptor changed"
        );
        ensure!(
            bytes > 0 && bytes <= maximum,
            "external guest source record byte bound is invalid"
        );
        let (content, observation) = authority.read_regular_file_stable_bounded(bytes)?;
        ensure!(
            observation.size() == bytes && content.len() as u64 == bytes,
            "external guest source record changed its admitted size"
        );
        Ok(content)
    };
    let binding_content = read_record(
        binding,
        binding_descriptor,
        binding_bytes,
        ryeos_state::objects::MAX_SOURCE_BINDING_BYTES as u64,
    )?;
    let manifest_content = read_record(
        manifest,
        manifest_descriptor,
        manifest_bytes,
        ryeos_state::objects::MAX_SOURCE_MANIFEST_BYTES as u64,
    )?;
    let records =
        ryeos_state::source_verification::VerifiedAdmittedSourceRecords::from_canonical_bytes(
            binding_hash,
            manifest_hash,
            &binding_content,
            &manifest_content,
        )?;
    ensure!(
        projection.bytes == records.manifest().totals.total_bytes,
        "external guest source byte total differs from its manifest"
    );
    let source =
        mount.try_clone_pinned_directory(std::path::PathBuf::from("<external-admitted-source>"))?;
    records.verify_tree(&source)
}

impl LockedSupervisorRoot {
    /// Take the state root's exclusive lock and pin both journal directories:
    /// created on a fresh root, reopened under the exact anchor otherwise.
    fn lock(
        state_root: lillux::PinnedDirectory,
        bootstrap: &ExternalSupervisorBootstrap,
        relay: bool,
    ) -> Result<Self> {
        state_root.require_owner_private_directory()?;
        let state_lock = state_root
            .try_lock_exclusive()?
            .context("external supervisor state root already has a live owner")?;
        state_lock.ensure_protects(&state_root)?;
        if state_root.entry_names()?.is_empty() {
            bootstrap.validate_at(lillux::time::timestamp_millis())?;
            // Only HTTP samples target-local network files, before this fresh
            // root gains any entry. Relay captures the original pinned mailbox
            // identity when its journal is created.
            let network_inputs = if relay {
                bootstrap.controller.relay()?;
                None
            } else {
                Some(capture_network_inputs(
                    &bootstrap.controller.http()?.network_inputs,
                )?)
            };
            let outer_directory = state_root.create_child(OsStr::new(OUTER_DIRECTORY), 0o700)?;
            let guest_directory = state_root.create_child(OsStr::new(GUEST_DIRECTORY), 0o700)?;
            return Ok(Self {
                state_root,
                state_lock,
                outer_directory,
                guest_directory,
                origin: SupervisorRootOrigin::Fresh { network_inputs },
            });
        }
        require_state_entries(&state_root)?;
        let anchor_file = state_root
            .open_regular(OsStr::new(STATE_ANCHOR), false)?
            .context("external supervisor state anchor is absent")?;
        let anchor_bytes =
            lillux::read_open_regular_file_bounded(anchor_file, MAX_STATE_ANCHOR_BYTES)?;
        let anchor: ExternalSupervisorStateAnchor = serde_json::from_slice(&anchor_bytes)
            .context("decode external supervisor state anchor")?;
        ensure!(
            canonical_anchor(&anchor)? == anchor_bytes,
            "external supervisor state anchor is not canonical"
        );
        ensure!(
            anchor.schema == 1 && anchor.state_root_identity == state_root.identity()?,
            "external supervisor state anchor changed its exact root"
        );
        ensure!(
            anchor.native_journal == bootstrap.native_journal,
            "external supervisor restart changed its sealed native journal envelope"
        );
        let outer_directory = state_root
            .open_child_directory(OsStr::new(OUTER_DIRECTORY))?
            .context("external supervisor outer journal directory is absent")?;
        let guest_directory = state_root
            .open_child_directory(OsStr::new(GUEST_DIRECTORY))?
            .context("external supervisor guest journal directory is absent")?;
        Ok(Self {
            state_root,
            state_lock,
            outer_directory,
            guest_directory,
            origin: SupervisorRootOrigin::Anchored(anchor),
        })
    }
}

impl ProtectedSupervisorState {
    #[cfg(test)]
    fn open_or_create(
        state_root: lillux::PinnedDirectory,
        bootstrap: ExternalSupervisorBootstrap,
        native: &JournalNative<'_>,
    ) -> Result<Self> {
        let root = LockedSupervisorRoot::lock(state_root, &bootstrap, false)?;
        Self::open(root, bootstrap, None, native)
    }

    fn recovery_only_outcome(&self) -> Result<Option<ExternalCandidateSupervisorOutcome>> {
        let Some(RetainedSupervisorJournal::LaunchIntent(recovered)) = &self.journal else {
            return Ok(None);
        };
        Ok(Some(ExternalCandidateSupervisorOutcome::RecoveryOnly {
            binding_digest: recovered.binding().digest()?,
            launch_intent_digest: lillux::sha256_hex(
                lillux::canonical_json(&serde_json::to_value(recovered.launch_intent())?)?
                    .as_bytes(),
            ),
        }))
    }

    /// Open the outer journal on `native`: create it on a fresh root, or
    /// reopen it under the retained anchor. Every refusal after the native
    /// open has closed the journal into the census.
    fn open(
        root: LockedSupervisorRoot,
        bootstrap: ExternalSupervisorBootstrap,
        relay_mailbox: Option<&lillux::PinnedDirectory>,
        native: &JournalNative<'_>,
    ) -> Result<Self> {
        let LockedSupervisorRoot {
            state_root,
            state_lock,
            outer_directory,
            guest_directory,
            origin,
        } = root;
        let native_journal = bootstrap.native_journal;
        let journal = match origin {
            SupervisorRootOrigin::Fresh { network_inputs } => {
                let created = match (network_inputs, relay_mailbox) {
                    (None, Some(mailbox)) => PreparedExternalSupervisorJournal::create_relay(
                        outer_directory,
                        bootstrap,
                        mailbox,
                        lillux::crypto::generate_signing_key(),
                        native.factory,
                        native.destination(),
                        native.retain,
                    ),
                    (Some(network_inputs), None) => PreparedExternalSupervisorJournal::create(
                        outer_directory,
                        bootstrap,
                        network_inputs,
                        lillux::crypto::generate_signing_key(),
                        native.factory,
                        native.destination(),
                        native.retain,
                    ),
                    _ => bail!("external supervisor transport capture differs from its mailbox"),
                };
                let journal = created.map_err(|error| native.settle(error))?;
                let anchored = (|| -> Result<()> {
                    let anchor = ExternalSupervisorStateAnchor {
                        schema: 1,
                        state_root_identity: state_root.identity()?,
                        outer_store_identity: journal.store_identity().clone(),
                        native_journal,
                    };
                    let anchor_bytes = canonical_anchor(&anchor)?;
                    state_root.atomic_write_if_same(
                        OsStr::new(STATE_ANCHOR),
                        None,
                        &anchor_bytes,
                        0o600,
                        crate::SUPERVISOR_TEMP_PURPOSE,
                    )?;
                    require_state_entries(&state_root)
                })();
                if let Err(error) = anchored {
                    return Err(native.refuse(
                        "external supervisor journal",
                        error,
                        journal.close(),
                    ));
                }
                RetainedSupervisorJournal::Prepared(journal)
            }
            SupervisorRootOrigin::Anchored(anchor) => {
                let recovered = ExternalSupervisorJournalRecovery::open(
                    outer_directory,
                    &anchor.outer_store_identity,
                    native.factory,
                    native.destination(),
                    native.retain,
                )
                .map_err(|error| native.settle(error))?;
                let journal = match recovered {
                    ExternalSupervisorJournalRecovery::Prepared(journal) => {
                        RetainedSupervisorJournal::Prepared(journal)
                    }
                    ExternalSupervisorJournalRecovery::Attached(journal) => {
                        RetainedSupervisorJournal::Attached(journal)
                    }
                    ExternalSupervisorJournalRecovery::LaunchIntent(journal) => {
                        RetainedSupervisorJournal::LaunchIntent(journal)
                    }
                };
                if let Err(error) = require_recovered_custody(&journal, &bootstrap, relay_mailbox) {
                    return Err(native.refuse(
                        "external supervisor journal",
                        error,
                        journal.close(),
                    ));
                }
                journal
            }
        };
        Ok(Self {
            state_root,
            _state_lock: state_lock,
            guest_directory,
            journal: Some(journal),
            guest_reservation: None,
        })
    }

    /// Record the authenticated binding: the prepared journal becomes attached.
    fn record_binding(
        &mut self,
        binding: ExecutionChannelBinding,
        relay_response_digest: Option<&str>,
        native: &JournalNative<'_>,
    ) -> Result<()> {
        let prepared = match self.journal.take() {
            Some(RetainedSupervisorJournal::Prepared(prepared)) => prepared,
            other => {
                self.journal = other;
                bail!("external supervisor journal is not prepared for attachment")
            }
        };
        let attached = match relay_response_digest {
            None => prepared.record_binding(binding),
            Some(digest) => prepared.record_relay_binding(binding, digest),
        }
        .map_err(|error| native.settle(error))?;
        self.journal = Some(RetainedSupervisorJournal::Attached(attached));
        Ok(())
    }

    /// Record durable launch intent: the attached journal becomes launched.
    fn begin_launch(
        &mut self,
        guest_store_identity: GuestJournalStoreIdentity,
        launcher_spec_digest: &str,
        launcher_bootstrap_digest: &str,
        launcher_artifact_digest: &str,
        native: &JournalNative<'_>,
    ) -> Result<()> {
        let attached = match self.journal.take() {
            Some(RetainedSupervisorJournal::Attached(attached)) => attached,
            other => {
                self.journal = other;
                bail!("external supervisor journal is not attached before launch")
            }
        };
        let launched = attached
            .begin_launch(
                guest_store_identity,
                launcher_spec_digest,
                launcher_bootstrap_digest,
                launcher_artifact_digest,
            )
            .map_err(|error| native.settle(error))?;
        self.journal = Some(RetainedSupervisorJournal::Launched(launched));
        Ok(())
    }

    /// End the guest reservation, then close the outer journal into the
    /// census. The root's lock and directories end only after that close.
    fn close_journals(self, native: &JournalNative<'_>) -> Result<()> {
        let Self {
            state_root,
            _state_lock: state_lock,
            guest_directory,
            journal,
            guest_reservation,
        } = self;
        drop(guest_reservation);
        let closed = match journal {
            Some(journal) => native.close("external supervisor journal", journal.close()),
            None => Ok(()),
        };
        drop((guest_directory, state_lock, state_root));
        closed
    }
}

/// A reopened outer journal must keep its original transport custody and
/// exact sealed bootstrap, up to fixed guest descriptor numbering.
fn require_recovered_custody(
    journal: &RetainedSupervisorJournal,
    bootstrap: &ExternalSupervisorBootstrap,
    relay_mailbox: Option<&lillux::PinnedDirectory>,
) -> Result<()> {
    match (journal.transport_capture(), relay_mailbox) {
        (ExternalSupervisorTransportCapture::Http(_), None) => {}
        (ExternalSupervisorTransportCapture::Relay(capture), Some(mailbox)) => {
            ensure!(
                mailbox.identity()? == capture.mailbox_identity()
                    && mailbox.path() == std::path::Path::new(capture.mailbox_root()),
                "external supervisor recovery changed original relay mailbox"
            );
            mailbox.require_owner_private_directory()?;
            mailbox.ensure_path_binding()?;
        }
        _ => bail!("external supervisor recovery changed transport custody"),
    }
    let mut retained_comparable: ExternalSupervisorBootstrap =
        serde_json::from_slice(&journal.bootstrap().canonical_bytes()?)?;
    let mut incoming_comparable: ExternalSupervisorBootstrap =
        serde_json::from_slice(&bootstrap.canonical_bytes()?)?;
    rebind_fixed_guest_descriptors(&mut retained_comparable.guest_inputs)?;
    rebind_fixed_guest_descriptors(&mut incoming_comparable.guest_inputs)?;
    ensure!(
        retained_comparable.canonical_bytes()? == incoming_comparable.canonical_bytes()?,
        "external supervisor restart changed its sealed bootstrap"
    );
    Ok(())
}

fn capture_network_inputs(
    policy: &ExternalNetworkInputPolicy,
) -> Result<ExternalCapturedNetworkInputs> {
    policy.validate()?;
    let capture =
        |selection: &ExternalNetworkInputSelection| -> Result<lillux::CapturedRegularFile> {
            // Operator-selected system names may be symlinks. Resolve once, then
            // read and seal the observed exact regular descriptor through Lillux.
            let path = lillux::canonicalize_existing_path(std::path::Path::new(&selection.source))?;
            let file = lillux::open_pinned_regular_file_no_follow(&path)?;
            file.capture_sealed_bounded(&file.observation()?, selection.max_bytes)
        };
    let resolver = capture(&policy.resolver)?;
    let hosts = capture(&policy.hosts)?;
    // Parse before creating the journal or contacting the controller. Sealed
    // captures are supervisor-only and never become candidate descriptors.
    lillux::network::NetworkContext::from_config_bytes(resolver.bytes(), hosts.bytes())?;
    ExternalCapturedNetworkInputs::from_bytes(policy, resolver.bytes(), hosts.bytes())
}

fn adopt_relay_mailbox(
    bootstrap: &ExternalSupervisorBootstrap,
    capture: &ryeos_state::external_execution::supervisor_journal::ExternalRelayMailboxCapture,
    mailbox: Option<lillux::PinnedDirectory>,
    state_root: &lillux::PinnedDirectory,
    candidate_runtime: &lillux::PinnedDirectory,
    candidate_private_parent: &lillux::PinnedDirectory,
    staged_source: Option<&lillux::PinnedDirectory>,
) -> Result<OriginalRelayMailbox> {
    capture.validate_for(bootstrap)?;
    let mailbox = mailbox.context("relay journal lost original mailbox descriptor")?;
    let staged_source = staged_source.context("relay journal lost mounted source descriptor")?;
    let retained_bootstrap = serde_json::from_slice(&bootstrap.canonical_bytes()?)?;
    OriginalRelayMailbox::adopt(
        retained_bootstrap,
        mailbox,
        capture.mailbox_identity(),
        state_root,
        candidate_runtime,
        candidate_private_parent,
        staged_source,
    )
}

impl LiveExternalCandidateSupervisor {
    /// Attach (or reattach), reserve the guest journal, record launch intent
    /// and launch. Each journal stays in `state` until a transition consumes
    /// it, so a refusal at any step leaves every open journal with its owner.
    fn start(
        state: &mut ProtectedSupervisorState,
        native: &JournalNative<'_>,
        execution_guest_inputs: ExternalGuestInputProjection,
        candidate_runtime: lillux::PinnedDirectory,
        candidate_private_parent: lillux::PinnedDirectory,
        mut relay_mailbox: Option<lillux::PinnedDirectory>,
        staged_source: Option<lillux::PinnedDirectory>,
        launcher: lillux::InheritedDescriptorAuthority,
        workspace_outputs: Option<lillux::InheritedDescriptorAuthority>,
        runtime_mounts: Vec<lillux::InheritedDescriptorAuthority>,
        ready_boot_inputs: Option<Vec<String>>,
    ) -> Result<Self> {
        ensure!(
            state.guest_directory.entry_names()?.is_empty(),
            "pre-launch recovery found an abandoned guest reservation"
        );
        let mut transport = match state
            .journal
            .as_ref()
            .context("external supervisor state lost its retained journal")?
        {
            RetainedSupervisorJournal::Prepared(prepared) => match prepared.transport_capture() {
                ExternalSupervisorTransportCapture::Http(_) => {
                    let (binding, transport) = attach_external_execution_channel_exact(
                        prepared.bootstrap(),
                        prepared.supervisor_signing_key(),
                        prepared.attachment_request(),
                        prepared.captured_network_inputs()?,
                    )?;
                    // Selecting the carrier contacts nothing. Its outcome is
                    // held until the attached binding is durable, so a failed
                    // selection still recovers from the attached stage and
                    // never by attaching again.
                    let selected = prepared.captured_network_inputs().and_then(|inputs| {
                        select_exchange_transport(
                            prepared.bootstrap(),
                            prepared.supervisor_signing_key(),
                            &binding,
                            inputs,
                            Some(transport),
                        )
                    });
                    state.record_binding(binding, None, native)?;
                    selected?
                }
                ExternalSupervisorTransportCapture::Relay(capture) => {
                    let mut mailbox = adopt_relay_mailbox(
                        prepared.bootstrap(),
                        capture,
                        relay_mailbox.take(),
                        &state.state_root,
                        &candidate_runtime,
                        &candidate_private_parent,
                        staged_source.as_ref(),
                    )?;
                    let deadline = lillux::time::MonotonicDeadline::after(
                        lillux::time::Duration::from_millis(u64::from(
                            prepared.bootstrap().controller.relay()?.poll_deadline_ms,
                        )),
                    );
                    let authenticated = mailbox.attach_exact(
                        prepared.supervisor_signing_key(),
                        prepared.attachment_request(),
                        deadline,
                    )?;
                    let binding = authenticated.binding().clone();
                    state.record_binding(binding, Some(authenticated.response_digest()), native)?;
                    AttachedExternalExecutionTransport::from(mailbox.into_attached(authenticated)?)
                }
            },
            RetainedSupervisorJournal::Attached(attached) => match attached.transport_capture() {
                ExternalSupervisorTransportCapture::Http(_) => select_exchange_transport(
                    attached.bootstrap(),
                    attached.supervisor_signing_key(),
                    attached.binding(),
                    attached.captured_network_inputs()?,
                    None,
                )?,
                ExternalSupervisorTransportCapture::Relay(capture) => {
                    let mailbox = adopt_relay_mailbox(
                        attached.bootstrap(),
                        capture,
                        relay_mailbox.take(),
                        &state.state_root,
                        &candidate_runtime,
                        &candidate_private_parent,
                        staged_source.as_ref(),
                    )?;
                    let deadline = lillux::time::MonotonicDeadline::after(
                        lillux::time::Duration::from_millis(u64::from(
                            attached.bootstrap().controller.relay()?.poll_deadline_ms,
                        )),
                    );
                    AttachedExternalExecutionTransport::from(mailbox.reconnect_retained(
                        attached.attachment_request(),
                        attached.binding(),
                        &attached.relay_attach_response_digest()?,
                        deadline,
                    )?)
                }
            },
            RetainedSupervisorJournal::Launched(_) | RetainedSupervisorJournal::LaunchIntent(_) => {
                bail!("external supervisor launch intent is recovery-only")
            }
        };
        let Some(RetainedSupervisorJournal::Attached(attached)) = state.journal.as_ref() else {
            bail!("external supervisor journal is not attached after attachment")
        };
        let spec = ExternalCandidateLauncherSpec::from_admitted_program(
            attached.binding().clone(),
            &attached.bootstrap().candidate_program,
            &execution_guest_inputs,
        )?;
        let now_ms = lillux::time::timestamp_millis();
        let execution_remaining_ms = attached
            .binding()
            .execution_deadline_ms
            .checked_sub(now_ms)
            .context("external execution deadline already elapsed")?;
        let expiry_remaining_ms = attached
            .binding()
            .expires_at_ms
            .checked_sub(now_ms)
            .context("external channel deadline already elapsed")?;
        ensure!(
            execution_remaining_ms > 0 && expiry_remaining_ms > execution_remaining_ms,
            "external channel has no bounded execution and cleanup window"
        );
        let execution_deadline = lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_millis(u64::try_from(execution_remaining_ms)?),
        );
        let expiry_deadline = lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_millis(u64::try_from(expiry_remaining_ms)?),
        );
        let prepared = prepare_launcher_subprocess_request(
            &launcher,
            &candidate_runtime,
            &candidate_private_parent,
            workspace_outputs.as_ref(),
            &runtime_mounts,
            &spec,
            expiry_deadline.remaining().as_secs_f64(),
        )?;
        ensure!(
            prepared.measured_launcher_digest() == attached.bootstrap().launcher_artifact_hash,
            "captured launcher differs from retained admitted bootstrap artifact"
        );
        let spec_digest = spec.digest()?;
        let signing_key = attached.supervisor_signing_key().clone();
        let lease_ready = ready_boot_inputs
            .map(ryeos_external_execution::supervisor::LeaseReadyPlan::observe)
            .transpose()?;
        let reserved = PreparedGuestJournal::reserve(
            state.guest_directory.try_clone()?,
            &prepared.authority,
            &prepared.bootstrap_digest,
            attached.binding().clone(),
            native.factory,
            native.destination(),
        )
        .map_err(|error| native.settle(error))?;
        let guest_store_identity = reserved.store_identity().clone();
        state.guest_reservation = Some(reserved);
        state.begin_launch(
            guest_store_identity,
            &spec_digest,
            &prepared.bootstrap_digest,
            prepared.measured_launcher_digest(),
            native,
        )?;
        let guest_journal = state
            .guest_reservation
            .take()
            .context("external guest reservation is absent after launch intent")?
            .initialize(native.retain)
            .map_err(|error| native.settle(error))?;
        let runtime = launch_prepared_external_candidate_supervisor(
            guest_journal,
            prepared,
            signing_key,
            lease_ready,
            expiry_deadline,
            native.retain,
        )?;
        // A leased guest's Ready and capability report reach the controller
        // before anything else happens. Its candidate is launched only by the
        // owner's Release, which the controller sends after admitting the
        // report; this handshake dispatches no owner frame.
        if let Some(report) = runtime.capability_report_frame() {
            if let Err(error) = lease_handshake(
                &mut transport,
                runtime.binding(),
                runtime.ready_frame().as_bytes(),
                report.as_bytes(),
                expiry_deadline,
            ) {
                let (journal, cleanup) = runtime.abort_and_reap();
                drop(transport);
                let error = error.context(format!(
                    "leased guest Ready and capability report; launcher cleanup={cleanup:?}"
                ));
                return Err(native.refuse("live guest journal", error, journal.close()));
            }
        }
        let driver = match ExternalCandidateTransportDriver::try_new(runtime, transport) {
            Ok(driver) => driver,
            Err((runtime, transport, error)) => {
                let (journal, cleanup) = runtime.abort_and_reap();
                drop(transport);
                let error = error.context(format!(
                    "start external control loop; launcher cleanup={cleanup:?}"
                ));
                return Err(native.refuse("live guest journal", error, journal.close()));
            }
        };
        Ok(Self {
            driver: Some(driver),
            guest_journal: None,
            execution_deadline,
            expiry_deadline,
        })
    }

    fn run(&mut self) -> Result<ExternalCandidateSupervisorOutcome> {
        let driver = self
            .driver
            .take()
            .context("external supervisor driver is absent")?;
        run_control_loop(
            driver,
            self.execution_deadline,
            self.expiry_deadline,
            &mut self.guest_journal,
        )
    }

    /// Reap a launcher the control loop never settled, then close the live
    /// guest journal into the census. Returns every failure; a refused close
    /// keeps its journal inside its failure.
    fn close_journals(self, native: &JournalNative<'_>) -> Vec<anyhow::Error> {
        let Self {
            driver,
            guest_journal,
            execution_deadline: _,
            expiry_deadline: _,
        } = self;
        let mut failures = Vec::new();
        let mut journals: Vec<LiveGuestJournal> = guest_journal.into_iter().collect();
        if let Some(driver) = driver {
            let (journal, cleanup) = ExternalCandidateControlDriver::abort_and_reap(driver);
            journals.push(journal);
            if let Err(error) = cleanup {
                failures.push(error.context("launcher cleanup at supervisor shutdown"));
            }
        }
        for journal in journals {
            if let Err(error) = native.close("live guest journal", journal.close()) {
                failures.push(error);
            }
        }
        failures
    }
}

/// Present a leased guest's signed Ready and then its capability report, each
/// retried byte for byte after transport ambiguity. The controller holds its
/// answer to Ready, which is idle rather than an error, and answers the report
/// only once it has admitted the guest; a refusal arrives as a failure.
fn lease_handshake<T: ExternalExecutionChannelTransport>(
    transport: &mut T,
    binding: &ExecutionChannelBinding,
    ready: &[u8],
    report: &[u8],
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    let ready_frame = SignedExecutionFrame::decode_and_verify(
        ready,
        binding,
        lillux::time::timestamp_millis(),
    )?;
    ensure!(
        ready_frame.frame().sequence == 1
            && matches!(
                ready_frame.frame().payload,
                ExecutionChannelPayload::Ready { boot: Some(_), .. }
            ),
        "a leased guest starts from a Ready that names its boot"
    );
    let report_frame = SignedExecutionFrame::decode_and_verify(
        report,
        binding,
        lillux::time::timestamp_millis(),
    )?;
    ensure!(
        report_frame.frame().sequence == 2
            && matches!(
                report_frame.frame().payload,
                ExecutionChannelPayload::CapabilityReport { .. }
            ),
        "a leased guest's second frame is its capability report"
    );
    let held = exchange_exact(transport, ready, deadline)?;
    ensure!(
        held.incoming_sequence == 1
            && held.incoming_frame_digest == ready_frame.digest()
            && !held.incoming_new
            && held.outbound_frames.is_empty()
            && held.urgent_revocation_frame.is_none(),
        "the controller answered a leased guest's Ready with more than a hold"
    );
    let admitted = exchange_exact(transport, report, deadline)?;
    ensure!(
        admitted.incoming_sequence == 2 && admitted.incoming_frame_digest == report_frame.digest(),
        "the controller answered a different frame than the capability report"
    );
    Ok(())
}

/// One exchange of exactly `wire`, retried unchanged while the outcome is
/// ambiguous and `deadline` has not elapsed.
fn exchange_exact<T: ExternalExecutionChannelTransport>(
    transport: &mut T,
    wire: &[u8],
    deadline: lillux::time::MonotonicDeadline,
) -> Result<ExternalTransportExchange> {
    loop {
        match transport.exchange_until(wire, deadline) {
            Ok(exchange) => {
                ensure!(
                    exchange.schema == 1,
                    "unsupported external exchange response schema"
                );
                return Ok(exchange);
            }
            Err(ExternalTransportStepFailure::AmbiguousTransport(_))
                if !deadline.has_elapsed() =>
            {
                lillux::time::sleep(IDLE_POLL.min(deadline.remaining()));
            }
            Err(ExternalTransportStepFailure::AmbiguousTransport(error)) => {
                return Err(error.context("leased guest handshake outlived its channel"));
            }
            Err(ExternalTransportStepFailure::Fatal(error)) => return Err(error),
        }
    }
}

trait ExternalCandidateControlDriver: Sized {
    /// What the driver hands back once its launcher was reaped.
    type Retained;
    fn has_durable_capture(&self) -> bool;
    fn is_direct_command(&self) -> bool;
    fn step_until(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportProgress, ExternalTransportStepFailure>;
    /// Reap the launcher on every outcome and hand back what it retained.
    fn abort_and_reap(self) -> (Self::Retained, Result<()>);
}

impl ExternalCandidateControlDriver for LiveControlDriver {
    type Retained = LiveGuestJournal;

    fn has_durable_capture(&self) -> bool {
        self.has_durable_capture()
    }

    fn is_direct_command(&self) -> bool {
        self.is_direct_command()
    }

    fn step_until(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportProgress, ExternalTransportStepFailure> {
        self.step_classified_until(deadline)
    }

    fn abort_and_reap(self) -> (LiveGuestJournal, Result<()>) {
        let (runtime, transport) = self.into_parts();
        let reaped = runtime.abort_and_reap();
        // The transport ends only after the launcher was reaped.
        drop(transport);
        reaped
    }
}

/// Drive `driver` to an outcome. On every path that reaps the launcher, what
/// the driver retained lands in `reaped_owner` for its owner to close.
fn run_control_loop<D: ExternalCandidateControlDriver>(
    driver: D,
    execution_deadline: lillux::time::MonotonicDeadline,
    expiry_deadline: lillux::time::MonotonicDeadline,
    reaped_owner: &mut Option<D::Retained>,
) -> Result<ExternalCandidateSupervisorOutcome> {
    let mut driver = Some(driver);
    let result = loop {
        let current = driver
            .as_ref()
            .context("external supervisor driver is absent")?;
        let retained = current.has_durable_capture();
        let direct = current.is_direct_command();
        if expiry_deadline.has_elapsed() {
            break finish_control_driver(
                &mut driver,
                reaped_owner,
                ExternalCandidateSupervisorOutcome::ChannelExpired,
            );
        }
        if execution_deadline.has_elapsed() && !retained && !direct {
            break finish_control_driver(
                &mut driver,
                reaped_owner,
                ExternalCandidateSupervisorOutcome::ExecutionDeadline,
            );
        }
        // Native execution still enforces its separate absolute deadline.
        // Direct observations may drain until channel expiry; this does not
        // authorize more input, provider contact, or execution after revocation.
        let active_deadline = if retained || direct {
            expiry_deadline
        } else {
            execution_deadline.min(expiry_deadline)
        };
        match driver
            .as_mut()
            .context("external supervisor driver is absent")?
            .step_until(active_deadline)
        {
            Ok(ExternalTransportProgress::CommandTerminatedApplied) => {
                if !direct {
                    break Err(anyhow::anyhow!(
                        "session control loop received a direct terminal receipt"
                    ));
                }
                break finish_control_driver(
                    &mut driver,
                    reaped_owner,
                    ExternalCandidateSupervisorOutcome::CommandTerminatedApplied,
                );
            }
            Ok(ExternalTransportProgress::ExportApplied) => {
                break finish_control_driver(
                    &mut driver,
                    reaped_owner,
                    ExternalCandidateSupervisorOutcome::ExportApplied,
                );
            }
            Ok(ExternalTransportProgress::Revoked) => {
                if direct {
                    lillux::time::sleep(IDLE_POLL.min(active_deadline.remaining()));
                    continue;
                }
                break finish_control_driver(
                    &mut driver,
                    reaped_owner,
                    ExternalCandidateSupervisorOutcome::RevokedApplied,
                );
            }
            Ok(ExternalTransportProgress::RevokedAwaitingCleanup) => {
                if direct {
                    lillux::time::sleep(IDLE_POLL.min(active_deadline.remaining()));
                    continue;
                }
                break finish_control_driver(
                    &mut driver,
                    reaped_owner,
                    ExternalCandidateSupervisorOutcome::RevokedClaimedUnknown,
                );
            }
            Ok(ExternalTransportProgress::Idle) => {
                lillux::time::sleep(IDLE_POLL.min(active_deadline.remaining()));
            }
            Ok(ExternalTransportProgress::Advanced) => {}
            Err(ExternalTransportStepFailure::AmbiguousTransport(_)) => {
                lillux::time::sleep(IDLE_POLL.min(active_deadline.remaining()));
            }
            Err(ExternalTransportStepFailure::Fatal(error)) => break Err(error),
        }
    };
    match result {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            let cleanup = match driver.take() {
                Some(driver) => {
                    let (owner, cleanup) = driver.abort_and_reap();
                    *reaped_owner = Some(owner);
                    cleanup
                }
                None => Err(anyhow::anyhow!(
                    "external supervisor driver already settled"
                )),
            };
            Err(error.context(format!(
                "external supervisor fatal control-loop failure; launcher cleanup={cleanup:?}"
            )))
        }
    }
}

fn finish_control_driver<D: ExternalCandidateControlDriver>(
    driver: &mut Option<D>,
    reaped_owner: &mut Option<D::Retained>,
    outcome: ExternalCandidateSupervisorOutcome,
) -> Result<ExternalCandidateSupervisorOutcome> {
    let (owner, cleanup) = driver
        .take()
        .context("external supervisor driver already settled")?
        .abort_and_reap();
    *reaped_owner = Some(owner);
    cleanup?;
    Ok(outcome)
}

fn canonical_anchor(anchor: &ExternalSupervisorStateAnchor) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(anchor)?)?.into_bytes();
    ensure!(
        u64::try_from(bytes.len())? <= MAX_STATE_ANCHOR_BYTES,
        "external supervisor state anchor exceeds its bound"
    );
    anchor.outer_store_identity.digest()?;
    Ok(bytes)
}

fn require_state_entries(state_root: &lillux::PinnedDirectory) -> Result<()> {
    let observed = state_root
        .entry_names()?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let expected = [OUTER_DIRECTORY, GUEST_DIRECTORY, STATE_ANCHOR]
        .into_iter()
        .map(OsString::from)
        .collect::<BTreeSet<_>>();
    ensure!(
        observed == expected,
        "external supervisor state root contains ambient or missing entries"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_state::external_execution::ExecutionChannelBinding;
    use ryeos_state::external_execution::admission::{
        AdmittedExternalCandidateProgram, ExternalCandidateProcFilesystem,
        ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe, PROTOCOL,
    };
    use ryeos_state::external_execution::encode_channel_public_key;
    use ryeos_state::external_execution::transport::{
        EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
        external_tls_root_bundle_digest,
    };

    use ryeos_state::external_execution::native_journal::testing::retire;

    use super::*;

    fn bootstrap(occurrence: &str, attachment_deadline_ms: i64) -> ExternalSupervisorBootstrap {
        // Recovery constructs the real TLS client from retained inputs. Use
        // the shared test CA, not merely a bounded non-certificate placeholder.
        let roots = vec![crate::test_support::TEST_CA_DER_BASE64.to_owned()];
        let recipe = ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = recipe.digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 7,
            required_lifecycle_capabilities: Default::default(),
            protocol: PROTOCOL.into(),
            connector_protocol:
                ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
            execution_route: ryeos_state::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "runtime".into(),
            runtime_authority: ryeos_state::external_execution::admission::ExternalCandidateRuntimeAuthority::CapturedProduct,
            runtime_recipe: recipe,
        };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        let program = AdmittedExternalCandidateProgram {
            requirement,
            qualification_use,
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_manifest_hash: "e".repeat(64),
            runtime_source: ryeos_state::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { witness_hash: "1".repeat(64) },
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "3".repeat(64),
            runtime_recipe_digest,
        };
        let guest_inputs = ryeos_external_execution_contract::ExternalGuestInputProjection {
            workspace_view: ryeos_external_execution_contract::GuestWorkspaceView::FullSnapshot,
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: ryeos_external_execution_contract::GuestBaseSnapshotInput {
                descriptor: 55,
                snapshot_hash: "c".repeat(64),
                closure_digest: "5".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![ryeos_external_execution_contract::GuestMountInput {
                role: ryeos_external_execution_contract::GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 64,
                destination: "/runtime".into(),
                kind: ryeos_external_execution_contract::GuestMountKind::Directory,
                access: ryeos_external_execution_contract::GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority:
                    ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                        manifest_kind:
                            ryeos_external_execution_contract::GuestProductManifestKind::Content,
                        manifest_hash: "e".repeat(64),
                        manifest_descriptor: 65,
                        manifest_bytes: 256,
                    },
                bytes: 1,
            }],
            executable_search: vec!["/runtime/bin".into()],
            environment: BTreeMap::new(),
        };
        let guest_input_identity = guest_inputs.identity_digest().unwrap();
        ExternalSupervisorBootstrap {
            schema: ryeos_state::external_execution::transport::EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
            controller: ExternalControllerTransportContract {
                schema: 2,
                network_inputs: ExternalNetworkInputPolicy {
                    resolver: ExternalNetworkInputSelection {
                        source: "/etc/resolv.conf".into(),
                        max_bytes: 64 * 1024,
                    },
                    hosts: ExternalNetworkInputSelection {
                        source: "/etc/hosts".into(),
                        max_bytes: 64 * 1024,
                    },
                },
                https_origin: "https://controller.invalid".into(),
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
                connect_timeout_ms: 1_000,
                request_timeout_ms: 2_000,
                maximum_response_bytes: 64 * 1024,
            }
            .into(),
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-placement".into(),
            occurrence_id: occurrence.into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            launcher_artifact_hash: "4".repeat(64),
            candidate_program: program.into(),
            guest_input_identity,
            guest_inputs,
            owner_public_key: encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[51; 32]).verifying_key(),
            )
            .unwrap(),
            bootstrap_capability: STANDARD.encode([7_u8; 32]),
            attachment_deadline_ms,
            execution_timeout_seconds: 30,
            post_execution_timeout_seconds: 30,
            candidate_export_max_bytes: 1024 * 1024,
            channel_max_bytes: 2 * 1024 * 1024,
            native_journal: ryeos_state::external_execution::transport::ExternalSupervisorNativeEnvelope {
                storage_bytes: 67_108_864,
                allocator_steps: 2_000_000_000,
                connections: 16,
                operation_millis: 4_860_000,
                cleanup_millis: 120_000,
                cleanup_steps: 3_000_000,
            },
            report_capabilities: false,
        }
    }

    fn pinned(path: &std::path::Path) -> lillux::PinnedDirectory {
        lillux::PinnedDirectory::open(path).unwrap().unwrap()
    }

    fn binding(
        bootstrap: &ExternalSupervisorBootstrap,
        supervisor: &lillux::crypto::SigningKey,
    ) -> ExecutionChannelBinding {
        let issued_at_ms = lillux::time::timestamp_millis();
        ExecutionChannelBinding {
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
            placement_thread_id: bootstrap.placement_thread_id.clone(),
            allocation_request_digest: bootstrap.allocation_request_digest.clone(),
            occurrence_id: bootstrap.occurrence_id.clone(),
            admitted_capsule_hash: bootstrap.admitted_capsule_hash.clone(),
            base_snapshot_hash: bootstrap.base_snapshot_hash.clone(),
            execution_binding_hash: bootstrap.execution_binding_hash.clone(),
            supervisor_runtime_hash: bootstrap.supervisor_runtime_hash.clone(),
            candidate_program_digest: bootstrap.candidate_program.digest().unwrap(),
            channel_nonce: "f".repeat(64),
            owner_public_key: bootstrap.owner_public_key.clone(),
            supervisor_public_key: encode_channel_public_key(&supervisor.verifying_key()).unwrap(),
            issued_at_ms,
            execution_deadline_ms: issued_at_ms + 30_000,
            expires_at_ms: issued_at_ms + 60_000,
            candidate_export_max_bytes: bootstrap.candidate_export_max_bytes,
            max_frames: bootstrap.binding_max_frames().unwrap(),
            max_bytes: bootstrap.channel_max_bytes,
        }
    }

    fn state_authority() -> (tempfile::TempDir, ryeos_state::PinnedStateAuthority) {
        let root = private_root();
        let db = ryeos_state::test_support::TestState::open(
            root.path(),
            Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        let authority = db.pinned_authority().unwrap();
        drop(db);
        (root, authority)
    }

    /// Run `work` with journal custody on this thread's shared native test
    /// host, then hand the host back for its census release.
    fn with_test_native<T>(
        work: impl FnOnce(&lillux_sqlite::test_host::TestNativeHost, &JournalNative<'_>) -> T,
    ) -> T {
        let host = ryeos_state::test_support::shared_test_host();
        let result = {
            let retain = |terminal: OriginalNativeConnectionTerminal| -> std::result::Result<
                (),
                (OriginalNativeConnectionTerminal, anyhow::Error),
            > {
                host.retain_terminal(terminal);
                Ok(())
            };
            let native = JournalNative {
                factory: host.factory(),
                destination: host.destination(),
                retain: &retain,
            };
            work(&*host, &native)
        };
        ryeos_state::test_support::release_shared_test_host(host);
        result
    }

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    enum FakeControlStep {
        Progress(ExternalTransportProgress),
        Ambiguous,
        Fatal,
        StallUntilDeadline,
    }

    struct FakeControlDriver {
        captured: bool,
        direct: bool,
        steps: VecDeque<FakeControlStep>,
        reaps: Arc<AtomicUsize>,
        fail_reap: bool,
    }

    impl ExternalCandidateControlDriver for FakeControlDriver {
        type Retained = ();

        fn has_durable_capture(&self) -> bool {
            self.captured
        }

        fn is_direct_command(&self) -> bool {
            self.direct
        }

        fn step_until(
            &mut self,
            deadline: lillux::time::MonotonicDeadline,
        ) -> std::result::Result<ExternalTransportProgress, ExternalTransportStepFailure> {
            match self.steps.pop_front().expect("fake control step") {
                FakeControlStep::Progress(progress) => Ok(progress),
                FakeControlStep::Ambiguous => {
                    Err(ExternalTransportStepFailure::AmbiguousTransport(
                        anyhow::anyhow!("ambiguous fixture exchange"),
                    ))
                }
                FakeControlStep::Fatal => Err(ExternalTransportStepFailure::Fatal(
                    anyhow::anyhow!("fatal fixture dispatch"),
                )),
                FakeControlStep::StallUntilDeadline => {
                    lillux::time::sleep(
                        deadline.remaining() + lillux::time::Duration::from_millis(1),
                    );
                    Err(ExternalTransportStepFailure::AmbiguousTransport(
                        anyhow::anyhow!("fixture request timed out"),
                    ))
                }
            }
        }

        fn abort_and_reap(self) -> ((), Result<()>) {
            self.reaps.fetch_add(1, Ordering::SeqCst);
            if self.fail_reap {
                return ((), Err(anyhow::anyhow!("fixture launcher cleanup failed")));
            }
            ((), Ok(()))
        }
    }

    fn fake_driver(
        captured: bool,
        steps: impl IntoIterator<Item = FakeControlStep>,
        reaps: Arc<AtomicUsize>,
    ) -> FakeControlDriver {
        FakeControlDriver {
            captured,
            direct: false,
            steps: steps.into_iter().collect(),
            reaps,
            fail_reap: false,
        }
    }

    fn make_private_directory(path: &std::path::Path) -> lillux::PinnedDirectory {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        pinned(path)
    }

    fn make_launcher(
        directory: &std::path::Path,
    ) -> (lillux::InheritedDescriptorAuthority, String) {
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join("launcher");
        std::fs::write(&path, b"fixture launcher").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = pinned(directory);
        let launcher = parent
            .open_inherited_regular(OsStr::new("launcher"), false)
            .unwrap()
            .unwrap();
        let observation = launcher.regular_file_observation().unwrap();
        let digest = launcher
            .digest_regular_file_stable_exact(&observation)
            .unwrap();
        (launcher, digest)
    }

    fn runtime_manifest(directory: &lillux::PinnedDirectory) -> (String, Vec<u8>) {
        let manifest = ryeos_state::observe_external_content_tree_exact(directory).unwrap();
        let bytes = lillux::canonical_json(&serde_json::to_value(manifest).unwrap())
            .unwrap()
            .into_bytes();
        (lillux::sha256_hex(&bytes), bytes)
    }

    fn bind_runtime_manifest(
        bootstrap: &mut ExternalSupervisorBootstrap,
        manifest_hash: String,
        manifest: &lillux::InheritedDescriptorAuthority,
        manifest_bytes: usize,
    ) {
        bootstrap
            .candidate_program
            .worker_mut()
            .unwrap()
            .runtime_manifest_hash = manifest_hash.clone();
        bootstrap.supervisor_runtime_hash = manifest_hash.clone();
        bootstrap.guest_inputs.inputs[0].content_authority =
            ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                manifest_kind: ryeos_external_execution_contract::GuestProductManifestKind::Content,
                manifest_hash,
                manifest_descriptor: manifest.inherited_descriptor().unwrap(),
                manifest_bytes: manifest_bytes as u64,
            };
        let (manifest_json, _) = manifest
            .read_regular_file_stable_bounded(manifest_bytes as u64)
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&manifest_json).unwrap();
        bootstrap.guest_inputs.inputs[0].bytes =
            ryeos_state::objects::ExternalContentManifestObject::from_value(&value)
                .unwrap()
                .total_bytes;
        bootstrap.guest_input_identity = bootstrap.guest_inputs.identity_digest().unwrap();
    }

    fn source_fixture(
        directory: &std::path::Path,
    ) -> (
        ryeos_external_execution_contract::GuestMountInput,
        lillux::InheritedDescriptorAuthority,
        lillux::InheritedDescriptorAuthority,
        lillux::InheritedDescriptorAuthority,
    ) {
        use ryeos_external_execution_contract::{
            GuestMountAccess, GuestMountInput, GuestMountKind, GuestMountRole,
        };
        use ryeos_state::objects::*;
        let source = make_private_directory(directory);
        std::fs::write(directory.join("run.py"), b"print('B evaluator')\n").unwrap();
        std::fs::set_permissions(
            directory.join("run.py"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let manifest = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".into(),
            }],
            vec![SourceClosureFile {
                root: "source".into(),
                path: "run.py".into(),
                blob_hash: lillux::sha256_hex(b"print('B evaluator')\n"),
                size: b"print('B evaluator')\n".len() as u64,
                mode: SourceFileMode::ReadOnly,
            }],
        )
        .unwrap();
        let schema_body = "kind: tool\nlocation:\n  directory: tools\n".to_owned();
        let binding = EffectiveSourceBinding {
            schema: EFFECTIVE_SOURCE_BINDING_SCHEMA,
            kind: EFFECTIVE_SOURCE_BINDING_KIND.into(),
            owner: SourceOwnerIdentity {
                canonical_ref: "tool:test/run".into(),
                item_kind: "tool".into(),
                source_space: SourceSpaceIdentity::Project,
                source_root: SourceRootIdentity::Project,
                root_source_content_digest: "a".repeat(64),
                root_raw_content_digest: "b".repeat(64),
                signer_fingerprint: "c".repeat(64),
                logical_item_key: "test/run".into(),
            },
            kind_ceiling: SignedKindSourceCeiling {
                schema_ref: "kind:tool".into(),
                source_content_digest: "d".repeat(64),
                raw_content_digest: lillux::signature::content_hash(&schema_body),
                signer_fingerprint: "f".repeat(64),
                signature_header: "signed".into(),
                schema_body,
                schema_document: serde_json::json!({"kind": "tool", "location": {"directory": "tools"}}),
                normalized_declaration: serde_json::json!({
                    "derived": SOURCE_CLOSURE_DERIVED_KEY, "location": {"type": "item_namespace"},
                    "testimony": "owner_signed_files", "max_files": 8, "max_total_bytes": 1024,
                    "max_file_bytes": 512, "max_depth": 8,
                }),
                root_kind_format: serde_json::json!({"extensions": ["yaml"]}),
                root_signature_envelope: serde_json::json!({"style": "header"}),
            },
            content_manifest_hash: manifest.digest().unwrap(),
            testimony: SourceTestimonyProof::OwnerSignedFiles {
                signer_fingerprint: "c".repeat(64),
                file_count: 1,
                entries_digest: "2".repeat(64),
            },
            execution_policy: SourceExecutionPolicyIdentity::Executor {
                declarer_ref: "tool:ryeos/core/runtimes/python/function".into(),
                signer_fingerprint: "3".repeat(64),
                source_content_digest: "4".repeat(64),
                raw_content_digest: "5".repeat(64),
                policy_digest: "6".repeat(64),
                chain_digest: "7".repeat(64),
            },
            logical_binding: SourceLogicalBinding::ToolDirectory {
                loader_roots: vec![SourceLoaderRoot::ItemDirectory],
                root: "test".into(),
                root_entry: "run.py".into(),
            },
        };
        binding.validate_content_manifest(&manifest).unwrap();
        let binding_bytes = lillux::canonical_json(&binding.to_value().unwrap())
            .unwrap()
            .into_bytes();
        let manifest_bytes = lillux::canonical_json(&manifest.to_value().unwrap())
            .unwrap()
            .into_bytes();
        let binding_record = lillux::sealed_memfd(c"test-source-binding", &binding_bytes).unwrap();
        let manifest_record =
            lillux::sealed_memfd(c"test-source-manifest", &manifest_bytes).unwrap();
        let mount = source.inherited_descriptor_authority().unwrap();
        let projection = GuestMountInput {
            role: GuestMountRole::Source,
            authority_id: binding.digest().unwrap(),
            descriptor: mount.inherited_descriptor().unwrap(),
            destination: format!(
                "/ryeos/realizations/source-closures/{}",
                binding.digest().unwrap()
            ),
            kind: GuestMountKind::Directory,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: None,
            content_authority: GuestMountContentAuthority::SourceClosure {
                binding_hash: binding.digest().unwrap(),
                binding_descriptor: binding_record.inherited_descriptor().unwrap(),
                binding_bytes: binding_bytes.len() as u64,
                manifest_hash: manifest.digest().unwrap(),
                manifest_descriptor: manifest_record.inherited_descriptor().unwrap(),
                manifest_bytes: manifest_bytes.len() as u64,
            },
            bytes: manifest.totals.total_bytes,
        };
        (projection, mount, binding_record, manifest_record)
    }

    #[test]
    fn source_delivery_checks_exact_records_descriptors_and_tree() {
        let root = private_root();
        let source_path = root.path().join("source");
        let (mut projection, mount, binding, manifest) = source_fixture(&source_path);
        let verify = |projection: &ryeos_external_execution_contract::GuestMountInput,
                      binding: &lillux::InheritedDescriptorAuthority,
                      manifest: &lillux::InheritedDescriptorAuthority| {
            let GuestMountContentAuthority::SourceClosure {
                binding_hash,
                binding_descriptor,
                binding_bytes,
                manifest_hash,
                manifest_descriptor,
                manifest_bytes,
            } = &projection.content_authority
            else {
                panic!("source fixture");
            };
            verify_source_input(
                &mount,
                projection,
                binding,
                manifest,
                binding_hash,
                *binding_descriptor,
                *binding_bytes,
                manifest_hash,
                *manifest_descriptor,
                *manifest_bytes,
            )
        };
        verify(&projection, &binding, &manifest).unwrap();
        assert!(
            verify(&projection, &manifest, &binding)
                .unwrap_err()
                .to_string()
                .contains("descriptor changed")
        );
        let original = projection.clone();
        if let GuestMountContentAuthority::SourceClosure { binding_hash, .. } =
            &mut projection.content_authority
        {
            *binding_hash = "9".repeat(64);
        }
        assert!(verify(&projection, &binding, &manifest).is_err());
        projection = original.clone();
        if let GuestMountContentAuthority::SourceClosure { manifest_bytes, .. } =
            &mut projection.content_authority
        {
            *manifest_bytes += 1;
        }
        assert!(verify(&projection, &binding, &manifest).is_err());
        projection = original;
        std::fs::write(source_path.join("unexpected.pyc"), b"ambient").unwrap();
        assert!(verify(&projection, &binding, &manifest).is_err());
        std::fs::remove_file(source_path.join("unexpected.pyc")).unwrap();
        std::fs::write(source_path.join("run.py"), b"print('C evaluator')\n").unwrap();
        assert!(verify(&projection, &binding, &manifest).is_err());
        std::fs::remove_file(source_path.join("run.py")).unwrap();
        assert!(verify(&projection, &binding, &manifest).is_err());
    }

    #[test]
    fn authority_validation_joins_exact_artifacts_and_refuses_tree_overlap() {
        let root = private_root();
        let state = make_private_directory(&root.path().join("state"));
        let candidate = make_private_directory(&root.path().join("candidate"));
        let private = make_private_directory(&root.path().join("private"));
        let runtime_path = root.path().join("runtime");
        std::fs::create_dir_all(runtime_path.join("bin")).unwrap();
        std::fs::write(runtime_path.join("bin/codex"), b"runtime").unwrap();
        let runtime = pinned(&runtime_path);
        let (launcher, launcher_digest) = make_launcher(&root.path().join("launcher-bin"));
        let mut exact_bootstrap = bootstrap(
            "occurrence-authority",
            lillux::time::timestamp_millis() + 60_000,
        );
        exact_bootstrap.launcher_artifact_hash = launcher_digest;
        let (manifest_hash, manifest_bytes) = runtime_manifest(&runtime);
        let manifest = lillux::sealed_memfd(c"test-runtime-manifest", &manifest_bytes).unwrap();
        bind_runtime_manifest(
            &mut exact_bootstrap,
            manifest_hash,
            &manifest,
            manifest_bytes.len(),
        );
        let runtime = runtime.inherited_descriptor_authority().unwrap();
        exact_bootstrap.guest_inputs.inputs[0].descriptor = runtime.inherited_descriptor().unwrap();
        let mut valid_inputs = ExternalCandidateSupervisorInputs {
            execution_guest_inputs: exact_bootstrap.guest_inputs.clone(),
            bootstrap: exact_bootstrap,
            state_root: state,
            candidate_runtime: candidate,
            candidate_private_parent: private,
            relay_mailbox: None,
            staged_source: None,
            launcher,
            workspace_outputs: None,
            runtime_mounts: vec![runtime],
            content_records: vec![manifest],
            ready_boot_inputs: None,
        };
        validate_authorities(&valid_inputs).unwrap();
        let (source_projection, source_mount, source_binding, source_manifest) =
            source_fixture(&root.path().join("admitted-source"));
        valid_inputs
            .bootstrap
            .guest_inputs
            .inputs
            .push(source_projection);
        valid_inputs.bootstrap.guest_input_identity = valid_inputs
            .bootstrap
            .guest_inputs
            .identity_digest()
            .unwrap();
        valid_inputs.execution_guest_inputs = valid_inputs.bootstrap.guest_inputs.clone();
        valid_inputs.runtime_mounts.push(source_mount);
        valid_inputs
            .content_records
            .extend([source_binding, source_manifest]);
        validate_authorities(&valid_inputs).unwrap();
        let source_manifest = valid_inputs.content_records.pop().unwrap();
        assert!(
            validate_authorities(&valid_inputs)
                .unwrap_err()
                .to_string()
                .contains("content record authority count changed")
        );
        valid_inputs.content_records.push(source_manifest);
        std::fs::write(
            root.path().join("admitted-source/run.py"),
            b"candidate substituted evaluator",
        )
        .unwrap();
        assert!(validate_authorities(&valid_inputs).is_err());
        std::fs::write(
            root.path().join("admitted-source/run.py"),
            b"print('B evaluator')\n",
        )
        .unwrap();
        validate_authorities(&valid_inputs).unwrap();
        valid_inputs.execution_guest_inputs.inputs[0].descriptor = 999;
        assert!(
            validate_authorities(&valid_inputs)
                .unwrap_err()
                .to_string()
                .contains("runtime descriptor changed")
        );

        let nested_root = private_root();
        let nested_state = make_private_directory(&nested_root.path().join("payload/state"));
        let nested_candidate =
            make_private_directory(&nested_root.path().join("candidate-runtime"));
        let nested_private = make_private_directory(&nested_root.path().join("private"));
        let exposed_ancestor = pinned(&nested_root.path().join("payload"));
        let (nested_launcher, nested_launcher_digest) =
            make_launcher(&nested_root.path().join("launcher-bin"));
        let mut nested_bootstrap = bootstrap(
            "occurrence-overlap",
            lillux::time::timestamp_millis() + 60_000,
        );
        nested_bootstrap.launcher_artifact_hash = nested_launcher_digest;
        let (manifest_hash, manifest_bytes) = runtime_manifest(&exposed_ancestor);
        let manifest = lillux::sealed_memfd(c"test-runtime-manifest", &manifest_bytes).unwrap();
        bind_runtime_manifest(
            &mut nested_bootstrap,
            manifest_hash,
            &manifest,
            manifest_bytes.len(),
        );
        let exposed_ancestor = exposed_ancestor.inherited_descriptor_authority().unwrap();
        nested_bootstrap.guest_inputs.inputs[0].descriptor =
            exposed_ancestor.inherited_descriptor().unwrap();
        let error = validate_authorities(&ExternalCandidateSupervisorInputs {
            execution_guest_inputs: nested_bootstrap.guest_inputs.clone(),
            bootstrap: nested_bootstrap,
            state_root: nested_state,
            candidate_runtime: nested_candidate,
            candidate_private_parent: nested_private,
            relay_mailbox: None,
            staged_source: None,
            launcher: nested_launcher,
            workspace_outputs: None,
            runtime_mounts: vec![exposed_ancestor],
            content_records: vec![manifest],
            ready_boot_inputs: None,
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("overlap by ancestry"));
    }

    #[test]
    fn authority_validation_refuses_runtime_or_launcher_substitution() {
        let build = |runtime_bytes: &[u8],
                     admitted_runtime: Option<(String, Vec<u8>)>,
                     bad_launcher: bool| {
            let root = private_root();
            let state = make_private_directory(&root.path().join("state"));
            let candidate = make_private_directory(&root.path().join("candidate"));
            let private = make_private_directory(&root.path().join("private"));
            let runtime_path = root.path().join("runtime");
            std::fs::create_dir_all(runtime_path.join("bin")).unwrap();
            std::fs::write(runtime_path.join("bin/codex"), runtime_bytes).unwrap();
            let runtime = pinned(&runtime_path);
            let (launcher, launcher_digest) = make_launcher(&root.path().join("launcher-bin"));
            let mut admitted = bootstrap(
                "occurrence-substitution",
                lillux::time::timestamp_millis() + 60_000,
            );
            admitted.launcher_artifact_hash = if bad_launcher {
                "9".repeat(64)
            } else {
                launcher_digest
            };
            let (manifest_hash, manifest_bytes) =
                admitted_runtime.unwrap_or_else(|| runtime_manifest(&runtime));
            let manifest = lillux::sealed_memfd(c"test-runtime-manifest", &manifest_bytes).unwrap();
            bind_runtime_manifest(
                &mut admitted,
                manifest_hash,
                &manifest,
                manifest_bytes.len(),
            );
            let runtime = runtime.inherited_descriptor_authority().unwrap();
            admitted.guest_inputs.inputs[0].descriptor = runtime.inherited_descriptor().unwrap();
            (
                root,
                ExternalCandidateSupervisorInputs {
                    execution_guest_inputs: admitted.guest_inputs.clone(),
                    bootstrap: admitted,
                    state_root: state,
                    candidate_runtime: candidate,
                    candidate_private_parent: private,
                    relay_mailbox: None,
                    staged_source: None,
                    launcher,
                    workspace_outputs: None,
                    runtime_mounts: vec![runtime],
                    content_records: vec![manifest],
                    ready_boot_inputs: None,
                },
            )
        };

        let first = private_root();
        let first_runtime = make_private_directory(&first.path().join("runtime"));
        std::fs::write(first.path().join("runtime/content"), b"first").unwrap();
        let expected = runtime_manifest(&first_runtime);
        let (_root, substituted_runtime) = build(b"second", Some(expected), false);
        assert!(validate_authorities(&substituted_runtime).is_err());

        let (_root, substituted_launcher) = build(b"runtime", None, true);
        assert!(validate_authorities(&substituted_launcher).is_err());
    }

    #[test]
    fn control_loop_retries_ambiguity_and_reaps_once_after_success() {
        let reaps = Arc::new(AtomicUsize::new(0));
        let outcome = run_control_loop(
            fake_driver(
                false,
                [
                    FakeControlStep::Ambiguous,
                    FakeControlStep::Progress(ExternalTransportProgress::Advanced),
                    FakeControlStep::Progress(ExternalTransportProgress::ExportApplied),
                ],
                reaps.clone(),
            ),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
            &mut None,
        )
        .unwrap();
        assert_eq!(outcome, ExternalCandidateSupervisorOutcome::ExportApplied);
        assert_eq!(reaps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn control_loop_fatal_failure_reaps_once_and_never_reports_success() {
        let reaps = Arc::new(AtomicUsize::new(0));
        let error = run_control_loop(
            fake_driver(false, [FakeControlStep::Fatal], reaps.clone()),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
            &mut None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("fatal fixture dispatch"));
        assert_eq!(reaps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn control_loop_keeps_revocation_outcomes_distinct() {
        for (progress, expected) in [
            (
                ExternalTransportProgress::Revoked,
                ExternalCandidateSupervisorOutcome::RevokedApplied,
            ),
            (
                ExternalTransportProgress::RevokedAwaitingCleanup,
                ExternalCandidateSupervisorOutcome::RevokedClaimedUnknown,
            ),
        ] {
            let reaps = Arc::new(AtomicUsize::new(0));
            let outcome = run_control_loop(
                fake_driver(false, [FakeControlStep::Progress(progress)], reaps.clone()),
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
                &mut None,
            )
            .unwrap();
            assert_eq!(outcome, expected);
            assert_eq!(reaps.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn direct_control_loop_drains_after_revocation_and_keeps_terminal_receipt_distinct() {
        for revoked in [
            ExternalTransportProgress::Revoked,
            ExternalTransportProgress::RevokedAwaitingCleanup,
        ] {
            let reaps = Arc::new(AtomicUsize::new(0));
            let mut driver = fake_driver(
                false,
                [
                    FakeControlStep::Progress(revoked),
                    FakeControlStep::Ambiguous,
                    FakeControlStep::Progress(ExternalTransportProgress::Advanced),
                    FakeControlStep::Progress(ExternalTransportProgress::CommandTerminatedApplied),
                ],
                reaps.clone(),
            );
            driver.direct = true;
            let outcome = run_control_loop(
                driver,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
                &mut None,
            )
            .unwrap();
            assert_eq!(
                outcome,
                ExternalCandidateSupervisorOutcome::CommandTerminatedApplied
            );
            assert_eq!(reaps.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn direct_control_loop_without_target_status_expires_without_inventing_completion() {
        let reaps = Arc::new(AtomicUsize::new(0));
        let mut driver = fake_driver(false, [FakeControlStep::StallUntilDeadline], reaps.clone());
        driver.direct = true;
        let outcome = run_control_loop(
            driver,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(10)),
            &mut None,
        )
        .unwrap();
        assert_eq!(outcome, ExternalCandidateSupervisorOutcome::ChannelExpired);
        assert_eq!(reaps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn command_terminal_receipt_does_not_bypass_mode_or_cleanup_checks() {
        for direct in [false, true] {
            let reaps = Arc::new(AtomicUsize::new(0));
            let mut driver = fake_driver(
                false,
                [FakeControlStep::Progress(
                    ExternalTransportProgress::CommandTerminatedApplied,
                )],
                reaps.clone(),
            );
            driver.direct = direct;
            driver.fail_reap = direct;
            let error = run_control_loop(
                driver,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
                &mut None,
            )
            .unwrap_err();
            assert_eq!(reaps.load(Ordering::SeqCst), 1);
            assert!(format!("{error:#}").contains(if direct {
                "cleanup failed"
            } else {
                "direct terminal receipt"
            }));
        }
    }

    #[test]
    fn execution_deadline_bounds_a_stalled_exchange_but_not_retained_export_drain() {
        let reaps = Arc::new(AtomicUsize::new(0));
        let outcome = run_control_loop(
            fake_driver(false, [FakeControlStep::StallUntilDeadline], reaps.clone()),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(10)),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
            &mut None,
        )
        .unwrap();
        assert_eq!(
            outcome,
            ExternalCandidateSupervisorOutcome::ExecutionDeadline
        );
        assert_eq!(reaps.load(Ordering::SeqCst), 1);

        let retained_reaps = Arc::new(AtomicUsize::new(0));
        let retained = run_control_loop(
            fake_driver(
                true,
                [FakeControlStep::Progress(
                    ExternalTransportProgress::ExportApplied,
                )],
                retained_reaps.clone(),
            ),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
            &mut None,
        )
        .unwrap();
        assert_eq!(retained, ExternalCandidateSupervisorOutcome::ExportApplied);
        assert_eq!(retained_reaps.load(Ordering::SeqCst), 1);

        let expired_reaps = Arc::new(AtomicUsize::new(0));
        let expired = run_control_loop(
            fake_driver(
                true,
                [FakeControlStep::StallUntilDeadline],
                expired_reaps.clone(),
            ),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(10)),
            &mut None,
        )
        .unwrap();
        assert_eq!(expired, ExternalCandidateSupervisorOutcome::ChannelExpired);
        assert_eq!(expired_reaps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_transport_cannot_hide_failed_launcher_cleanup() {
        let reaps = Arc::new(AtomicUsize::new(0));
        let error = run_control_loop(
            FakeControlDriver {
                captured: true,
                direct: false,
                steps: [FakeControlStep::Progress(
                    ExternalTransportProgress::ExportApplied,
                )]
                .into_iter()
                .collect(),
                reaps: reaps.clone(),
                fail_reap: true,
            },
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
            &mut None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("fixture launcher cleanup failed"));
        assert_eq!(reaps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retained_network_capture_survives_source_removal_in_prepared_and_attached_recovery() {
        for attached in [false, true] {
            let root = private_root();
            let sources = private_root();
            let resolver_path = sources.path().join("resolver");
            let hosts_path = sources.path().join("hosts");
            std::fs::write(&resolver_path, b"nameserver 127.0.0.1\n").unwrap();
            std::fs::write(&hosts_path, b"127.0.0.1 retained.example\n").unwrap();
            let mut original = bootstrap(
                "captured-network",
                lillux::time::timestamp_millis() + 60_000,
            );
            original
                .controller
                .http_mut()
                .unwrap()
                .network_inputs
                .resolver
                .source = resolver_path.to_str().unwrap().into();
            original
                .controller
                .http_mut()
                .unwrap()
                .network_inputs
                .hosts
                .source = hosts_path.to_str().unwrap().into();
            let original_bytes = original.canonical_bytes().unwrap();
            with_test_native(|host, native| {
                let mut state =
                    ProtectedSupervisorState::open_or_create(pinned(root.path()), original, native)
                        .unwrap();
                let prepared = match state.journal.take().unwrap() {
                    RetainedSupervisorJournal::Prepared(journal) => journal,
                    _ => panic!("fresh state must be prepared"),
                };
                let capture_digest = prepared
                    .captured_network_inputs()
                    .unwrap()
                    .digest()
                    .unwrap();
                let original_store_identity = prepared.store_identity().clone();
                assert_eq!(
                    prepared.bootstrap().canonical_bytes().unwrap(),
                    original_bytes
                );
                if attached {
                    let exact_binding =
                        binding(prepared.bootstrap(), prepared.supervisor_signing_key());
                    retire(
                        host,
                        prepared.record_binding(exact_binding).unwrap().close(),
                    );
                } else {
                    retire(host, prepared.close());
                }
                state.close_journals(native).unwrap();
                std::fs::write(&resolver_path, b"nameserver 192.0.2.99\n").unwrap();
                std::fs::remove_file(&hosts_path).unwrap();
                let incoming = serde_json::from_slice(&original_bytes).unwrap();
                let recovered =
                    ProtectedSupervisorState::open_or_create(pinned(root.path()), incoming, native)
                        .unwrap();
                let (retained, capture, identity) = match recovered.journal.as_ref().unwrap() {
                    RetainedSupervisorJournal::Prepared(journal) => (
                        journal.bootstrap(),
                        journal.captured_network_inputs().unwrap(),
                        journal.store_identity(),
                    ),
                    RetainedSupervisorJournal::Attached(journal) => (
                        journal.bootstrap(),
                        journal.captured_network_inputs().unwrap(),
                        journal.store_identity(),
                    ),
                    _ => panic!("recovery changed the prelaunch stage"),
                };
                assert_eq!(capture.digest().unwrap(), capture_digest);
                assert_eq!(identity, &original_store_identity);
                assert_eq!(retained.canonical_bytes().unwrap(), original_bytes);
                assert_eq!(
                    capture.hosts_bytes().unwrap(),
                    b"127.0.0.1 retained.example\n"
                );
                assert_eq!(capture.resolver_bytes().unwrap(), b"nameserver 127.0.0.1\n");
                // Building a client parses only retained bytes, even with a missing
                // selected file. It neither reconnects nor sends an attachment.
                crate::build_client(retained, capture).unwrap();
                recovered.close_journals(native).unwrap();
                let mut changed: ExternalSupervisorBootstrap =
                    serde_json::from_slice(&original_bytes).unwrap();
                changed
                    .controller
                    .http_mut()
                    .unwrap()
                    .network_inputs
                    .resolver
                    .source = sources.path().join("missing").to_str().unwrap().into();
                let error =
                    ProtectedSupervisorState::open_or_create(pinned(root.path()), changed, native)
                        .err()
                        .unwrap();
                assert!(error.to_string().contains("changed its sealed bootstrap"));
            });
        }
    }

    #[test]
    fn missing_or_oversized_network_sources_refuse_before_journal_creation() {
        for oversized in [false, true] {
            let root = private_root();
            let source = private_root();
            let path = source.path().join("resolver");
            if oversized {
                std::fs::write(&path, b"nameserver 127.0.0.1\n").unwrap();
            }
            let mut admitted =
                bootstrap("invalid-network", lillux::time::timestamp_millis() + 60_000);
            admitted
                .controller
                .http_mut()
                .unwrap()
                .network_inputs
                .resolver
                .source = path.to_str().unwrap().into();
            admitted
                .controller
                .http_mut()
                .unwrap()
                .network_inputs
                .resolver
                .max_bytes = 1;
            assert!(with_test_native(|_host, native| {
                ProtectedSupervisorState::open_or_create(pinned(root.path()), admitted, native)
                    .is_err()
            }));
            assert!(pinned(root.path()).entry_names().unwrap().is_empty());
        }
    }

    #[test]
    fn exact_prelaunch_state_anchor_recovers_without_new_authority() {
        let root = private_root();
        let deadline = lillux::time::timestamp_millis() + 60_000;
        let original = bootstrap("occurrence-one", deadline);
        let original_bytes = original.canonical_bytes().unwrap();
        with_test_native(|_host, native| {
            let state =
                ProtectedSupervisorState::open_or_create(pinned(root.path()), original, native)
                    .unwrap();
            assert!(matches!(
                state.journal,
                Some(RetainedSupervisorJournal::Prepared(_))
            ));
            state.close_journals(native).unwrap();

            let recovered = ProtectedSupervisorState::open_or_create(
                pinned(root.path()),
                bootstrap("occurrence-one", deadline),
                native,
            )
            .unwrap();
            assert!(matches!(
                recovered.journal,
                Some(RetainedSupervisorJournal::Prepared(_))
            ));
            recovered.close_journals(native).unwrap();

            let mut relocated_transport = bootstrap("occurrence-one", deadline);
            relocated_transport.guest_inputs.base_snapshot.descriptor = 128;
            relocated_transport.guest_inputs.inputs[0].descriptor = 129;
            if let ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                manifest_descriptor,
                ..
            } = &mut relocated_transport.guest_inputs.inputs[0].content_authority
            {
                *manifest_descriptor = 130;
            }
            relocated_transport.validate().unwrap();
            let relocated = ProtectedSupervisorState::open_or_create(
                pinned(root.path()),
                relocated_transport,
                native,
            )
            .unwrap();
            if let Some(RetainedSupervisorJournal::Prepared(journal)) = relocated.journal.as_ref() {
                assert_eq!(
                    journal.bootstrap().canonical_bytes().unwrap(),
                    original_bytes,
                    "FD equivalence must not rewrite the retained original bootstrap"
                );
            }
            assert!(matches!(
                relocated.journal,
                Some(RetainedSupervisorJournal::Prepared(_))
            ));
            relocated.close_journals(native).unwrap();

            let mut changed_content = bootstrap("occurrence-one", deadline);
            changed_content.guest_inputs.inputs[0].bytes += 1;
            changed_content.guest_input_identity =
                changed_content.guest_inputs.identity_digest().unwrap();
            let changed_content_error = ProtectedSupervisorState::open_or_create(
                pinned(root.path()),
                changed_content,
                native,
            )
            .err()
            .expect("changed guest content must refuse recovery");
            assert!(
                format!("{changed_content_error:#}")
                    .contains("external supervisor restart changed its sealed bootstrap"),
                "descriptor normalization must not admit changed guest content: {changed_content_error:#}"
            );

            // A changed envelope is refused by the root lock itself, before a
            // native host could be funded from it.
            let mut enlarged_envelope = bootstrap("occurrence-one", deadline);
            enlarged_envelope.native_journal.storage_bytes += 4096;
            let enlarged_envelope_error = match LockedSupervisorRoot::lock(
                pinned(root.path()),
                &enlarged_envelope,
                false,
            ) {
                Ok(_) => panic!("a changed native journal envelope reached the native host"),
                Err(error) => error,
            };
            assert!(
                format!("{enlarged_envelope_error:#}")
                    .contains("external supervisor restart changed its sealed native journal envelope"),
                "{enlarged_envelope_error:#}"
            );

            assert!(
                ProtectedSupervisorState::open_or_create(
                    pinned(root.path()),
                    bootstrap("occurrence-two", deadline),
                    native,
                )
                .is_err(),
                "a restart changed the exact sealed occurrence bootstrap"
            );
        });
    }

    #[test]
    fn missing_or_mutated_anchor_never_recreates_a_fresh_supervisor() {
        let partial = private_root();
        std::fs::create_dir(partial.path().join(OUTER_DIRECTORY)).unwrap();
        let deadline = lillux::time::timestamp_millis() + 60_000;
        with_test_native(|_host, native| {
            assert!(
                ProtectedSupervisorState::open_or_create(
                    pinned(partial.path()),
                    bootstrap("occurrence-partial", deadline),
                    native,
                )
                .is_err()
            );

            let mutated = private_root();
            ProtectedSupervisorState::open_or_create(
                pinned(mutated.path()),
                bootstrap("occurrence-mutated", deadline),
                native,
            )
            .unwrap()
            .close_journals(native)
            .unwrap();
            std::fs::write(mutated.path().join(STATE_ANCHOR), b"{}\n").unwrap();
            assert!(
                ProtectedSupervisorState::open_or_create(
                    pinned(mutated.path()),
                    bootstrap("occurrence-mutated", deadline),
                    native,
                )
                .is_err()
            );

            let source = private_root();
            ProtectedSupervisorState::open_or_create(
                pinned(source.path()),
                bootstrap("occurrence-source", deadline),
                native,
            )
            .unwrap()
            .close_journals(native)
            .unwrap();
            let substituted = private_root();
            ProtectedSupervisorState::open_or_create(
                pinned(substituted.path()),
                bootstrap("occurrence-substituted", deadline),
                native,
            )
            .unwrap()
            .close_journals(native)
            .unwrap();
            std::fs::write(
                substituted.path().join(STATE_ANCHOR),
                std::fs::read(source.path().join(STATE_ANCHOR)).unwrap(),
            )
            .unwrap();
            assert!(
                ProtectedSupervisorState::open_or_create(
                    pinned(substituted.path()),
                    bootstrap("occurrence-substituted", deadline),
                    native,
                )
                .is_err(),
                "an anchor from another exact state root was accepted"
            );

            let ambient = private_root();
            ProtectedSupervisorState::open_or_create(
                pinned(ambient.path()),
                bootstrap("occurrence-ambient", deadline),
                native,
            )
            .unwrap()
            .close_journals(native)
            .unwrap();
            std::fs::write(ambient.path().join("ambient"), b"not authority").unwrap();
            assert!(
                ProtectedSupervisorState::open_or_create(
                    pinned(ambient.path()),
                    bootstrap("occurrence-ambient", deadline),
                    native,
                )
                .is_err(),
                "ambient state-root content was accepted"
            );
        });
    }

    #[test]
    fn retained_launch_intent_recovers_as_evidence_only() {
        let root = private_root();
        let deadline = lillux::time::timestamp_millis() + 60_000;
        with_test_native(|host, native| {
            let mut state = ProtectedSupervisorState::open_or_create(
                pinned(root.path()),
                bootstrap("occurrence-launched", deadline),
                native,
            )
            .unwrap();
            let prepared = match state.journal.take().unwrap() {
                RetainedSupervisorJournal::Prepared(prepared) => prepared,
                _ => panic!("fresh supervisor did not retain its prepared journal"),
            };
            let expected_binding = binding(prepared.bootstrap(), prepared.supervisor_signing_key());
            let attached = prepared.record_binding(expected_binding.clone()).unwrap();
            let (_authority_root, authority) = state_authority();
            let launcher_digest = "7".repeat(64);
            let reservation = PreparedGuestJournal::reserve(
                state.guest_directory.try_clone().unwrap(),
                &authority,
                &launcher_digest,
                expected_binding,
                native.factory,
                native.destination(),
            )
            .unwrap();
            let launch = attached
                .begin_launch(
                    reservation.store_identity().clone(),
                    &launcher_digest,
                    &launcher_digest,
                    &"8".repeat(64),
                )
                .unwrap();
            let expected_binding_digest = launch.binding().digest().unwrap();
            retire(host, launch.close());
            drop(reservation);
            state.close_journals(native).unwrap();

            let recovered = ProtectedSupervisorState::open_or_create(
                pinned(root.path()),
                bootstrap("occurrence-launched", deadline),
                native,
            )
            .unwrap();
            let Some(ExternalCandidateSupervisorOutcome::RecoveryOnly {
                binding_digest,
                launch_intent_digest,
            }) = recovered.recovery_only_outcome().unwrap()
            else {
                panic!("retained launch intent recovered with executable authority")
            };
            assert_eq!(binding_digest, expected_binding_digest);
            assert!(lillux::valid_hash(&launch_intent_digest));
            recovered.close_journals(native).unwrap();
        });
    }
    enum HandshakeReply {
        Answer(ExternalTransportExchange),
        Ambiguous,
        Fatal,
    }

    struct ScriptedTransport {
        sent: Vec<Vec<u8>>,
        replies: VecDeque<HandshakeReply>,
    }

    impl ExternalExecutionChannelTransport for ScriptedTransport {
        fn exchange(&mut self, _frame: &[u8]) -> Result<ExternalTransportExchange> {
            bail!("the scripted transport answers only until a deadline")
        }

        fn exchange_until(
            &mut self,
            frame: &[u8],
            _deadline: lillux::time::MonotonicDeadline,
        ) -> std::result::Result<ExternalTransportExchange, ExternalTransportStepFailure> {
            self.sent.push(frame.to_vec());
            match self
                .replies
                .pop_front()
                .expect("the handshake sent a frame the script did not expect")
            {
                HandshakeReply::Answer(exchange) => Ok(exchange),
                HandshakeReply::Ambiguous => Err(ExternalTransportStepFailure::AmbiguousTransport(
                    anyhow::anyhow!("scripted transport loss"),
                )),
                HandshakeReply::Fatal => Err(ExternalTransportStepFailure::Fatal(
                    anyhow::anyhow!("scripted controller refusal"),
                )),
            }
        }
    }

    struct HandshakeFrames {
        binding: ExecutionChannelBinding,
        ready: Vec<u8>,
        ready_digest: String,
        report: Vec<u8>,
        report_digest: String,
    }

    fn handshake_frames() -> HandshakeFrames {
        use ryeos_state::external_execution::{ChannelDirection, ExecutionFrame};

        let supervisor = lillux::crypto::SigningKey::from_bytes(&[61; 32]);
        let bootstrap = bootstrap("lease-handshake", lillux::time::timestamp_millis() + 60_000);
        let binding = binding(&bootstrap, &supervisor);
        let boot_id = "01234567-89ab-cdef-0123-456789abcdef".to_owned();
        let author = |sequence: u64, previous: Option<String>, payload: ExecutionChannelPayload| {
            let signed = SignedExecutionFrame::sign(
                ExecutionFrame {
                    schema: 1,
                    binding_digest: binding.digest().unwrap(),
                    direction: ChannelDirection::SupervisorToOwner,
                    sequence,
                    previous_frame_digest: previous,
                    acknowledged_peer_sequence: 0,
                    payload,
                },
                &binding,
                &supervisor,
            )
            .unwrap();
            let wire = lillux::canonical_json(&serde_json::to_value(signed).unwrap())
                .unwrap()
                .into_bytes();
            let digest = SignedExecutionFrame::decode_and_verify(
                &wire,
                &binding,
                lillux::time::timestamp_millis(),
            )
            .unwrap()
            .digest()
            .to_owned();
            (wire, digest)
        };
        let (ready, ready_digest) = author(
            1,
            None,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
                boot: Some(ryeos_state::external_execution::SupervisorReadyBoot {
                    boot_id: boot_id.clone(),
                    verified_input_digests: vec!["1".repeat(64), "2".repeat(64), "3".repeat(64)],
                    ready_at_ms: lillux::time::timestamp_millis(),
                }),
            },
        );
        let (report, report_digest) = author(
            2,
            Some(ready_digest.clone()),
            ExecutionChannelPayload::CapabilityReport {
                boot_id,
                provided: BTreeMap::new(),
                missing: BTreeMap::new(),
            },
        );
        HandshakeFrames {
            binding,
            ready,
            ready_digest,
            report,
            report_digest,
        }
    }

    fn handshake_answer(
        sequence: u64,
        digest: &str,
        incoming_new: bool,
    ) -> HandshakeReply {
        HandshakeReply::Answer(ExternalTransportExchange {
            schema: 1,
            incoming_new,
            incoming_sequence: sequence,
            incoming_frame_digest: digest.to_owned(),
            acknowledgement_frame_digest: None,
            outbound_frames: Vec::new(),
            urgent_revocation_frame: None,
        })
    }

    fn handshake_deadline() -> lillux::time::MonotonicDeadline {
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30))
    }

    #[test]
    fn lease_handshake_sends_ready_then_the_report_and_retries_exact_bytes() {
        let frames = handshake_frames();
        let mut transport = ScriptedTransport {
            sent: Vec::new(),
            replies: VecDeque::from([
                HandshakeReply::Ambiguous,
                handshake_answer(1, &frames.ready_digest, false),
                handshake_answer(2, &frames.report_digest, true),
            ]),
        };
        lease_handshake(
            &mut transport,
            &frames.binding,
            &frames.ready,
            &frames.report,
            handshake_deadline(),
        )
        .unwrap();
        assert_eq!(
            transport.sent,
            vec![frames.ready.clone(), frames.ready.clone(), frames.report.clone()]
        );
    }

    #[test]
    fn lease_handshake_refuses_a_ready_answer_that_is_not_a_hold_and_sends_no_report() {
        let frames = handshake_frames();
        let mut transport = ScriptedTransport {
            sent: Vec::new(),
            replies: VecDeque::from([handshake_answer(1, &frames.ready_digest, true)]),
        };
        let error = lease_handshake(
            &mut transport,
            &frames.binding,
            &frames.ready,
            &frames.report,
            handshake_deadline(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("more than a hold"));
        assert_eq!(transport.sent, vec![frames.ready.clone()]);
    }

    #[test]
    fn lease_handshake_surfaces_a_controller_refusal_of_the_report() {
        let frames = handshake_frames();
        let mut transport = ScriptedTransport {
            sent: Vec::new(),
            replies: VecDeque::from([
                handshake_answer(1, &frames.ready_digest, false),
                HandshakeReply::Fatal,
            ]),
        };
        let error = lease_handshake(
            &mut transport,
            &frames.binding,
            &frames.ready,
            &frames.report,
            handshake_deadline(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("scripted controller refusal"));
        assert_eq!(transport.sent.len(), 2);
    }
}
