//! Generic protected supervisor process composition.
//!
//! Provider adapters arrange exact inherited descriptors and lifecycle. This
//! module owns no cloud API, credential, project pathname, or deployment
//! convention. A fresh state root can launch once; after durable launch intent,
//! reopen is recovery-only and can never manufacture a replacement candidate.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_executor::execution::external_candidate_launcher::{
    ExternalCandidateLauncherSpec, prepare_launcher_subprocess_request,
};
use ryeos_executor::execution::external_candidate_launcher_protocol::{
    LiveInheritedExternalCandidateSupervisor, launch_prepared_external_candidate_supervisor,
};
use ryeos_executor::execution::external_candidate_transport::{
    ExternalCandidateTransportDriver, ExternalTransportProgress, ExternalTransportStepFailure,
};
use ryeos_state::external_execution::guest_journal::PreparedGuestJournal;
use ryeos_state::external_execution::supervisor_journal::{
    AttachedExternalSupervisorJournal, ExternalSupervisorJournalRecovery,
    ExternalSupervisorJournalStoreIdentity, LaunchIntentExternalSupervisorJournal,
    PreparedExternalSupervisorJournal, RecoveredExternalSupervisorJournal,
};
use ryeos_state::external_execution::transport::ExternalSupervisorBootstrap;
use serde::{Deserialize, Serialize};

use crate::{
    AttachedExternalExecutionChannel, attach_external_execution_channel_exact,
    reconnect_external_execution_channel,
};

const OUTER_DIRECTORY: &str = "outer";
const GUEST_DIRECTORY: &str = "guest";
const STATE_ANCHOR: &str = "supervisor-state.json";
const MAX_STATE_ANCHOR_BYTES: u64 = 64 * 1024;
const IDLE_POLL: lillux::time::Duration = lillux::time::Duration::from_millis(10);

pub const SUPERVISOR_BOOTSTRAP_FD: u32 = 50;
pub const SUPERVISOR_STATE_ROOT_FD: u32 = 51;
pub const SUPERVISOR_CANDIDATE_RUNTIME_FD: u32 = 52;
pub const SUPERVISOR_PRIVATE_PARENT_FD: u32 = 53;
pub const SUPERVISOR_LAUNCHER_FD: u32 = 54;
pub const SUPERVISOR_RUNTIME_MOUNT_FD_BASE: u32 = 64;

/// Exact authorities supplied by an independently admitted lifecycle adapter.
/// Paths retained by these Lillux values are diagnostic only; all mutations,
/// reads, mounts and child execution remain descriptor-relative.
pub struct ExternalCandidateSupervisorInputs {
    pub bootstrap: ExternalSupervisorBootstrap,
    pub state_root: lillux::PinnedDirectory,
    pub candidate_runtime: lillux::PinnedDirectory,
    pub candidate_private_parent: lillux::PinnedDirectory,
    pub launcher: lillux::InheritedDescriptorAuthority,
    pub runtime_mounts: Vec<lillux::PinnedDirectory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExternalCandidateSupervisorOutcome {
    ExportApplied,
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
}

enum RetainedSupervisorJournal {
    Prepared(PreparedExternalSupervisorJournal),
    Attached(AttachedExternalSupervisorJournal),
    LaunchIntent(RecoveredExternalSupervisorJournal),
}

struct ProtectedSupervisorState {
    _state_root: lillux::PinnedDirectory,
    _state_lock: lillux::PinnedDirectoryLock,
    guest_directory: lillux::PinnedDirectory,
    journal: Option<RetainedSupervisorJournal>,
}

struct LiveExternalCandidateSupervisor {
    _state: ProtectedSupervisorState,
    _outer_journal: LaunchIntentExternalSupervisorJournal,
    driver: Option<
        ExternalCandidateTransportDriver<
            LiveInheritedExternalCandidateSupervisor,
            AttachedExternalExecutionChannel,
        >,
    >,
    execution_deadline: lillux::time::MonotonicDeadline,
    expiry_deadline: lillux::time::MonotonicDeadline,
}

/// Run one exact external candidate until its export is durably applied, its
/// cancellation is locally settled, or a signed lifecycle deadline expires.
/// Network ambiguity retries only the identical retained signed frame.
pub fn run_external_candidate_supervisor(
    inputs: ExternalCandidateSupervisorInputs,
) -> Result<ExternalCandidateSupervisorOutcome> {
    validate_authorities(&inputs)?;
    let ExternalCandidateSupervisorInputs {
        bootstrap,
        state_root,
        candidate_runtime,
        candidate_private_parent,
        launcher,
        runtime_mounts,
    } = inputs;
    let state = ProtectedSupervisorState::open_or_create(state_root, bootstrap)?;
    if let Some(outcome) = state.recovery_only_outcome()? {
        return Ok(outcome);
    }
    let mut live = LiveExternalCandidateSupervisor::start(
        state,
        candidate_runtime,
        candidate_private_parent,
        launcher,
        runtime_mounts,
    )?;
    live.run()
}

fn validate_authorities(inputs: &ExternalCandidateSupervisorInputs) -> Result<()> {
    inputs.bootstrap.validate()?;
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
    ensure!(
        inputs.runtime_mounts.len() == 1,
        "external candidate program requires one exact runtime mount"
    );
    for mount in &inputs.runtime_mounts {
        mount.require_disjoint_directory_tree(&inputs.state_root)?;
        mount.require_disjoint_directory_tree(&inputs.candidate_runtime)?;
        mount.require_disjoint_directory_tree(&inputs.candidate_private_parent)?;
    }
    let observed = ryeos_state::observe_external_content_tree_exact(&inputs.runtime_mounts[0])?;
    ensure!(
        ryeos_state::external_content_manifest_digest(&observed)?
            == inputs.bootstrap.candidate_program.runtime_manifest_hash,
        "external candidate runtime mount changed its admitted manifest identity"
    );
    Ok(())
}

impl ProtectedSupervisorState {
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

    fn open_or_create(
        state_root: lillux::PinnedDirectory,
        bootstrap: ExternalSupervisorBootstrap,
    ) -> Result<Self> {
        state_root.require_owner_private_directory()?;
        let state_lock = state_root
            .try_lock_exclusive()?
            .context("external supervisor state root already has a live owner")?;
        state_lock.ensure_protects(&state_root)?;
        let names = state_root.entry_names()?;
        if names.is_empty() {
            bootstrap.validate_at(lillux::time::timestamp_millis())?;
            return Self::create(state_root, state_lock, bootstrap);
        }
        Self::recover(state_root, state_lock, bootstrap, names)
    }

    fn create(
        state_root: lillux::PinnedDirectory,
        state_lock: lillux::PinnedDirectoryLock,
        bootstrap: ExternalSupervisorBootstrap,
    ) -> Result<Self> {
        let outer_directory = state_root.create_child(OsStr::new(OUTER_DIRECTORY), 0o700)?;
        let guest_directory = state_root.create_child(OsStr::new(GUEST_DIRECTORY), 0o700)?;
        let journal = PreparedExternalSupervisorJournal::create(
            outer_directory,
            bootstrap,
            lillux::crypto::generate_signing_key(),
        )?;
        let anchor = ExternalSupervisorStateAnchor {
            schema: 1,
            state_root_identity: state_root.identity()?,
            outer_store_identity: journal.store_identity().clone(),
        };
        let anchor_bytes = canonical_anchor(&anchor)?;
        state_root.atomic_write_if_same(OsStr::new(STATE_ANCHOR), None, &anchor_bytes, 0o600)?;
        require_state_entries(&state_root)?;
        Ok(Self {
            _state_root: state_root,
            _state_lock: state_lock,
            guest_directory,
            journal: Some(RetainedSupervisorJournal::Prepared(journal)),
        })
    }

    fn recover(
        state_root: lillux::PinnedDirectory,
        state_lock: lillux::PinnedDirectoryLock,
        bootstrap: ExternalSupervisorBootstrap,
        _observed_names: Vec<OsString>,
    ) -> Result<Self> {
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
        let outer_directory = state_root
            .open_child_directory(OsStr::new(OUTER_DIRECTORY))?
            .context("external supervisor outer journal directory is absent")?;
        let guest_directory = state_root
            .open_child_directory(OsStr::new(GUEST_DIRECTORY))?
            .context("external supervisor guest journal directory is absent")?;
        let recovered =
            ExternalSupervisorJournalRecovery::open(outer_directory, &anchor.outer_store_identity)?;
        let retained_bootstrap = match &recovered {
            ExternalSupervisorJournalRecovery::Prepared(journal) => journal.bootstrap(),
            ExternalSupervisorJournalRecovery::Attached(journal) => journal.bootstrap(),
            ExternalSupervisorJournalRecovery::LaunchIntent(journal) => journal.bootstrap(),
        };
        ensure!(
            retained_bootstrap.canonical_bytes()? == bootstrap.canonical_bytes()?,
            "external supervisor restart changed its sealed bootstrap"
        );
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
        Ok(Self {
            _state_root: state_root,
            _state_lock: state_lock,
            guest_directory,
            journal: Some(journal),
        })
    }
}

impl LiveExternalCandidateSupervisor {
    fn start(
        mut state: ProtectedSupervisorState,
        candidate_runtime: lillux::PinnedDirectory,
        candidate_private_parent: lillux::PinnedDirectory,
        launcher: lillux::InheritedDescriptorAuthority,
        runtime_mounts: Vec<lillux::PinnedDirectory>,
    ) -> Result<Self> {
        ensure!(
            state.guest_directory.entry_names()?.is_empty(),
            "pre-launch recovery found an abandoned guest reservation"
        );
        let (attached, transport) = match state
            .journal
            .take()
            .context("external supervisor state lost its retained journal")?
        {
            RetainedSupervisorJournal::Prepared(prepared) => {
                let (binding, transport) = attach_external_execution_channel_exact(
                    prepared.bootstrap(),
                    prepared.supervisor_signing_key(),
                    prepared.attachment_request(),
                )?;
                (prepared.record_binding(binding)?, transport)
            }
            RetainedSupervisorJournal::Attached(attached) => {
                let transport = reconnect_external_execution_channel(
                    attached.bootstrap(),
                    attached.supervisor_signing_key(),
                    attached.binding(),
                )?;
                (attached, transport)
            }
            RetainedSupervisorJournal::LaunchIntent(_) => {
                bail!("external supervisor launch intent is recovery-only")
            }
        };
        let spec = ExternalCandidateLauncherSpec::from_admitted_program(
            attached.binding().clone(),
            &attached.bootstrap().candidate_program,
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
        let launcher_observation = launcher.regular_file_observation()?;
        let launcher_artifact_digest =
            launcher.digest_regular_file_stable_exact(&launcher_observation)?;
        let runtime_mounts = runtime_mounts
            .into_iter()
            .map(|mount| mount.inherited_descriptor_authority())
            .collect::<Result<Vec<_>>>()?;
        let prepared = prepare_launcher_subprocess_request(
            &launcher,
            &candidate_runtime,
            &candidate_private_parent,
            &runtime_mounts,
            &spec,
            expiry_deadline.remaining().as_secs_f64(),
        )?;
        let reserved = PreparedGuestJournal::reserve(
            state.guest_directory.try_clone()?,
            &prepared.authority,
            &prepared.bootstrap_digest,
            attached.binding().clone(),
        )?;
        let signing_key = attached.supervisor_signing_key().clone();
        let outer_journal = attached.begin_launch(
            reserved.store_identity().clone(),
            &spec.digest()?,
            &prepared.bootstrap_digest,
            &launcher_artifact_digest,
        )?;
        let guest_journal = reserved.initialize()?;
        let runtime = launch_prepared_external_candidate_supervisor(
            guest_journal,
            prepared,
            signing_key,
            &launcher_artifact_digest,
            expiry_deadline,
        )?;
        let driver = ExternalCandidateTransportDriver::new(runtime, transport)?;
        Ok(Self {
            _state: state,
            _outer_journal: outer_journal,
            driver: Some(driver),
            execution_deadline,
            expiry_deadline,
        })
    }

    fn run(&mut self) -> Result<ExternalCandidateSupervisorOutcome> {
        run_control_loop(
            self.driver
                .take()
                .context("external supervisor driver is absent")?,
            self.execution_deadline,
            self.expiry_deadline,
        )
    }
}

trait ExternalCandidateControlDriver: Sized {
    fn has_durable_capture(&self) -> bool;
    fn step_until(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportProgress, ExternalTransportStepFailure>;
    fn abort_and_reap(self) -> Result<()>;
}

impl ExternalCandidateControlDriver
    for ExternalCandidateTransportDriver<
        LiveInheritedExternalCandidateSupervisor,
        AttachedExternalExecutionChannel,
    >
{
    fn has_durable_capture(&self) -> bool {
        self.has_durable_capture()
    }

    fn step_until(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportProgress, ExternalTransportStepFailure> {
        self.step_classified_until(deadline)
    }

    fn abort_and_reap(self) -> Result<()> {
        let (runtime, _transport) = self.into_parts();
        runtime.abort_and_reap()
    }
}

fn run_control_loop<D: ExternalCandidateControlDriver>(
    driver: D,
    execution_deadline: lillux::time::MonotonicDeadline,
    expiry_deadline: lillux::time::MonotonicDeadline,
) -> Result<ExternalCandidateSupervisorOutcome> {
    let mut driver = Some(driver);
    let result = loop {
        let retained = driver
            .as_ref()
            .context("external supervisor driver is absent")?
            .has_durable_capture();
        if expiry_deadline.has_elapsed() {
            break finish_control_driver(
                &mut driver,
                ExternalCandidateSupervisorOutcome::ChannelExpired,
            );
        }
        if execution_deadline.has_elapsed() && !retained {
            break finish_control_driver(
                &mut driver,
                ExternalCandidateSupervisorOutcome::ExecutionDeadline,
            );
        }
        let active_deadline = if retained {
            expiry_deadline
        } else {
            execution_deadline.min(expiry_deadline)
        };
        match driver
            .as_mut()
            .context("external supervisor driver is absent")?
            .step_until(active_deadline)
        {
            Ok(ExternalTransportProgress::ExportApplied) => {
                break finish_control_driver(
                    &mut driver,
                    ExternalCandidateSupervisorOutcome::ExportApplied,
                );
            }
            Ok(ExternalTransportProgress::Revoked) => {
                break finish_control_driver(
                    &mut driver,
                    ExternalCandidateSupervisorOutcome::RevokedApplied,
                );
            }
            Ok(ExternalTransportProgress::RevokedAwaitingCleanup) => {
                break finish_control_driver(
                    &mut driver,
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
            let cleanup = driver
                .take()
                .context("external supervisor driver already settled")
                .and_then(ExternalCandidateControlDriver::abort_and_reap);
            Err(error.context(format!(
                "external supervisor fatal control-loop failure; launcher cleanup={cleanup:?}"
            )))
        }
    }
}

fn finish_control_driver<D: ExternalCandidateControlDriver>(
    driver: &mut Option<D>,
    outcome: ExternalCandidateSupervisorOutcome,
) -> Result<ExternalCandidateSupervisorOutcome> {
    driver
        .take()
        .context("external supervisor driver already settled")?
        .abort_and_reap()?;
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

    use super::*;

    fn bootstrap(occurrence: &str, attachment_deadline_ms: i64) -> ExternalSupervisorBootstrap {
        let roots = vec![STANDARD.encode(b"bounded test root")];
        let recipe = ExternalCandidateRuntimeRecipe {
            schema: 1,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: true,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = recipe.digest().unwrap();
        let program = AdmittedExternalCandidateProgram {
            requirement: ExternalCandidateRequirement {
                schema: 2,
                protocol: PROTOCOL.into(),
                runtime_product_declaration_id: "runtime".into(),
                runtime_recipe: recipe,
            },
            runtime_manifest_hash: "e".repeat(64),
            runtime_witness_hash: "1".repeat(64),
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "3".repeat(64),
            runtime_recipe_digest,
        };
        ExternalSupervisorBootstrap {
            schema: 4,
            controller: ExternalControllerTransportContract {
                schema: 1,
                https_origin: "https://controller.invalid".into(),
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
                connect_timeout_ms: 1_000,
                request_timeout_ms: 2_000,
                maximum_response_bytes: 64 * 1024,
            },
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-placement".into(),
            occurrence_id: occurrence.into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            launcher_artifact_hash: "4".repeat(64),
            candidate_program: program,
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
            schema: 3,
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
        let db = ryeos_state::StateDb::open(root.path(), Arc::new(ryeos_state::TrustStore::new()))
            .unwrap();
        let authority = db.pinned_authority().unwrap();
        drop(db);
        (root, authority)
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
        steps: VecDeque<FakeControlStep>,
        reaps: Arc<AtomicUsize>,
        fail_reap: bool,
    }

    impl ExternalCandidateControlDriver for FakeControlDriver {
        fn has_durable_capture(&self) -> bool {
            self.captured
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

        fn abort_and_reap(self) -> Result<()> {
            self.reaps.fetch_add(1, Ordering::SeqCst);
            if self.fail_reap {
                bail!("fixture launcher cleanup failed")
            }
            Ok(())
        }
    }

    fn fake_driver(
        captured: bool,
        steps: impl IntoIterator<Item = FakeControlStep>,
        reaps: Arc<AtomicUsize>,
    ) -> FakeControlDriver {
        FakeControlDriver {
            captured,
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

    fn runtime_manifest(directory: &lillux::PinnedDirectory) -> String {
        let manifest = ryeos_state::observe_external_content_tree_exact(directory).unwrap();
        ryeos_state::external_content_manifest_digest(&manifest).unwrap()
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
        exact_bootstrap.candidate_program.runtime_manifest_hash = runtime_manifest(&runtime);
        exact_bootstrap.supervisor_runtime_hash = exact_bootstrap
            .candidate_program
            .runtime_manifest_hash
            .clone();
        validate_authorities(&ExternalCandidateSupervisorInputs {
            bootstrap: exact_bootstrap,
            state_root: state,
            candidate_runtime: candidate,
            candidate_private_parent: private,
            launcher,
            runtime_mounts: vec![runtime],
        })
        .unwrap();

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
        nested_bootstrap.candidate_program.runtime_manifest_hash =
            runtime_manifest(&exposed_ancestor);
        nested_bootstrap.supervisor_runtime_hash = nested_bootstrap
            .candidate_program
            .runtime_manifest_hash
            .clone();
        let error = validate_authorities(&ExternalCandidateSupervisorInputs {
            bootstrap: nested_bootstrap,
            state_root: nested_state,
            candidate_runtime: nested_candidate,
            candidate_private_parent: nested_private,
            launcher: nested_launcher,
            runtime_mounts: vec![exposed_ancestor],
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("overlap by ancestry"));
    }

    #[test]
    fn authority_validation_refuses_runtime_or_launcher_substitution() {
        let build = |runtime_bytes: &[u8], admitted_runtime: Option<String>, bad_launcher: bool| {
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
            admitted.candidate_program.runtime_manifest_hash =
                admitted_runtime.unwrap_or_else(|| runtime_manifest(&runtime));
            admitted.supervisor_runtime_hash =
                admitted.candidate_program.runtime_manifest_hash.clone();
            (
                root,
                ExternalCandidateSupervisorInputs {
                    bootstrap: admitted,
                    state_root: state,
                    candidate_runtime: candidate,
                    candidate_private_parent: private,
                    launcher,
                    runtime_mounts: vec![runtime],
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
            )
            .unwrap();
            assert_eq!(outcome, expected);
            assert_eq!(reaps.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn execution_deadline_bounds_a_stalled_exchange_but_not_retained_export_drain() {
        let reaps = Arc::new(AtomicUsize::new(0));
        let outcome = run_control_loop(
            fake_driver(false, [FakeControlStep::StallUntilDeadline], reaps.clone()),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(10)),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
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
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("fixture launcher cleanup failed"));
        assert_eq!(reaps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_prelaunch_state_anchor_recovers_without_new_authority() {
        let root = private_root();
        let deadline = lillux::time::timestamp_millis() + 60_000;
        let state = ProtectedSupervisorState::open_or_create(
            pinned(root.path()),
            bootstrap("occurrence-one", deadline),
        )
        .unwrap();
        assert!(matches!(
            state.journal,
            Some(RetainedSupervisorJournal::Prepared(_))
        ));
        drop(state);

        let recovered = ProtectedSupervisorState::open_or_create(
            pinned(root.path()),
            bootstrap("occurrence-one", deadline),
        )
        .unwrap();
        assert!(matches!(
            recovered.journal,
            Some(RetainedSupervisorJournal::Prepared(_))
        ));
        drop(recovered);

        assert!(
            ProtectedSupervisorState::open_or_create(
                pinned(root.path()),
                bootstrap("occurrence-two", deadline)
            )
            .is_err(),
            "a restart changed the exact sealed occurrence bootstrap"
        );
    }

    #[test]
    fn missing_or_mutated_anchor_never_recreates_a_fresh_supervisor() {
        let partial = private_root();
        std::fs::create_dir(partial.path().join(OUTER_DIRECTORY)).unwrap();
        let deadline = lillux::time::timestamp_millis() + 60_000;
        assert!(
            ProtectedSupervisorState::open_or_create(
                pinned(partial.path()),
                bootstrap("occurrence-partial", deadline)
            )
            .is_err()
        );

        let mutated = private_root();
        drop(
            ProtectedSupervisorState::open_or_create(
                pinned(mutated.path()),
                bootstrap("occurrence-mutated", deadline),
            )
            .unwrap(),
        );
        std::fs::write(mutated.path().join(STATE_ANCHOR), b"{}\n").unwrap();
        assert!(
            ProtectedSupervisorState::open_or_create(
                pinned(mutated.path()),
                bootstrap("occurrence-mutated", deadline)
            )
            .is_err()
        );

        let source = private_root();
        drop(
            ProtectedSupervisorState::open_or_create(
                pinned(source.path()),
                bootstrap("occurrence-source", deadline),
            )
            .unwrap(),
        );
        let substituted = private_root();
        drop(
            ProtectedSupervisorState::open_or_create(
                pinned(substituted.path()),
                bootstrap("occurrence-substituted", deadline),
            )
            .unwrap(),
        );
        std::fs::write(
            substituted.path().join(STATE_ANCHOR),
            std::fs::read(source.path().join(STATE_ANCHOR)).unwrap(),
        )
        .unwrap();
        assert!(
            ProtectedSupervisorState::open_or_create(
                pinned(substituted.path()),
                bootstrap("occurrence-substituted", deadline)
            )
            .is_err(),
            "an anchor from another exact state root was accepted"
        );

        let ambient = private_root();
        drop(
            ProtectedSupervisorState::open_or_create(
                pinned(ambient.path()),
                bootstrap("occurrence-ambient", deadline),
            )
            .unwrap(),
        );
        std::fs::write(ambient.path().join("ambient"), b"not authority").unwrap();
        assert!(
            ProtectedSupervisorState::open_or_create(
                pinned(ambient.path()),
                bootstrap("occurrence-ambient", deadline)
            )
            .is_err(),
            "ambient state-root content was accepted"
        );
    }

    #[test]
    fn retained_launch_intent_recovers_as_evidence_only() {
        let root = private_root();
        let deadline = lillux::time::timestamp_millis() + 60_000;
        let mut state = ProtectedSupervisorState::open_or_create(
            pinned(root.path()),
            bootstrap("occurrence-launched", deadline),
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
        drop(launch);
        drop(reservation);
        drop(state);

        let recovered = ProtectedSupervisorState::open_or_create(
            pinned(root.path()),
            bootstrap("occurrence-launched", deadline),
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
    }
}
