//! Dedicated native namespace owner beneath the existing consumer verifier.
//!
//! The observer, threaded scripted peer and evidence exporter remain outside
//! this single-threaded process. Namespace ownership is Lillux's; Codex protocol
//! interpretation remains with the observer. No Worker/session/candidate or
//! contact journal is created here, and this module issues no qualification.

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use lillux::PinnedDirectory;
use lillux::inherited_pipes::{InheritedPipeInput, InheritedPipeOutput, PipeInterrupt};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge;
use std::ffi::OsStr;

use crate::consumer_record::{
    ConsumerInputRecord, ConsumerOuterCompletionRequest, ImportedConsumerInputs,
    MAX_OUTER_COMPLETION_BYTES,
};

const OUTER_START: &str = "outer-started";
const OUTER_FINISH: &str = "outer-finish";
const OUTER_OBSERVATION: &str = "outer-observation.json";
const MAX_OUTER_OBSERVATION_BYTES: usize = 64 * 1024;
pub const OUTER_CHALLENGE_ENV: &str = "RYEOS_CONSUMER_OUTER_CHALLENGE_B64";
pub const OUTER_HOME_ENV: &str = "RYEOS_CONSUMER_OUTER_HOME";
pub const OUTER_CONTROL_ENV: &str = "RYEOS_CONSUMER_OUTER_CONTROL";

/// Exclusive synchronous startup for the retained namespace owner. Environment
/// values select already-private inputs; they grant no admission or identity.
/// The enclosing verifier must authenticate this executable and its launch.
pub fn run_startup(encoded_challenge: &str) -> Result<()> {
    use crate::consumer_record::{
        CONSUMER_INPUT_RECORD_NAME, CONSUMER_INPUT_ROOT_ENV, MAX_CONSUMER_INPUT_RECORD_BYTES,
    };
    ensure!(
        !encoded_challenge.is_empty() && encoded_challenge.len() <= 8192,
        "outer challenge encoding exceeds bound"
    );
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded_challenge)?;
    let challenge = ConsumerRuntimeChallenge::parse(&bytes)?;
    let (active, cleanup) = crate::native_guest::production_deadlines(
        challenge.intent.attempt_deadline_ms,
        lillux::time::timestamp_millis(),
    )?;
    let runtime_root = PinnedDirectory::open(std::path::Path::new("/ryeos/guest-runtime"))?
        .context("outer installed owner runtime absent")?;
    let runtime = require_selected_owner_runtime(&runtime_root, &challenge)?;
    // The enclosing verifier already relinquishes administrator authority.
    // Never drop only this helper while leaving a privileged observer behind.
    runtime.profile().account.require_current_process()?;
    // SAFETY: this mode is selected at exclusive executable startup, before
    // async runtimes, stdio wrappers or threads. The enclosing launch owns the
    // opposite pipe endpoints. Namespace preparation remains single-threaded.
    let pair = unsafe {
        lillux::inherited_pipes::InheritedPipePair::take_inherited_pipes(0, 1, 16 * 1024, active)
    }?;
    let (input, mut output, interrupt) = pair.split();
    let owner = PinnedDirectory::open(std::path::Path::new("."))?
        .context("outer protected owner cwd absent")?;
    let imported_root = open_environment_directory(CONSUMER_INPUT_ROOT_ENV)?;
    let home = open_environment_directory(OUTER_HOME_ENV)?;
    let control = open_environment_directory(OUTER_CONTROL_ENV)?;
    for mutable in [&owner, &home, &control, &imported_root] {
        runtime.require_disjoint_directory_tree(mutable)?;
    }
    let file = imported_root
        .open_pinned_regular(OsStr::new(CONSUMER_INPUT_RECORD_NAME), false)?
        .context("outer consumer input record absent")?;
    let record_bytes =
        file.read_stable_bounded(&file.observation()?, MAX_CONSUMER_INPUT_RECORD_BYTES as u64)?;
    let record = ConsumerInputRecord::parse(&record_bytes)?;
    let imported = record.open_imported_products(&imported_root, &challenge)?;
    runtime.recheck()?;
    runtime.profile().account.require_current_process()?;
    run_outer_protocol(
        &imported,
        &record,
        &challenge,
        &home,
        &control,
        &owner,
        input,
        &mut output,
        interrupt,
        active,
        cleanup,
    )
}

/// Observe the exact installed owner profile committed by this retained
/// consumer attempt. Parsing a challenge is data agreement only; the enclosing
/// remote entry still must authenticate that challenge before any account grant.
pub fn require_selected_owner_runtime(
    root: &PinnedDirectory,
    challenge: &ConsumerRuntimeChallenge,
) -> Result<ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime> {
    use ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose;
    challenge.validate()?;
    let RemoteVerificationPurpose::ConsumerRuntime {
        guest_runtime_manifest_hash,
        ..
    } = &challenge.intent.purpose
    else {
        anyhow::bail!("outer owner requires a consumer runtime attempt");
    };
    let runtime =
        ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime::observe(root)?;
    ensure!(
        runtime.manifest_hash() == guest_runtime_manifest_hash,
        "installed owner runtime differs from retained snapshot source"
    );
    Ok(runtime)
}

fn open_environment_directory(name: &str) -> Result<PinnedDirectory> {
    let path = std::env::var_os(name).with_context(|| format!("{name} absent"))?;
    ensure!(
        std::path::Path::new(&path).is_absolute(),
        "outer input directory must be absolute"
    );
    PinnedDirectory::open(std::path::Path::new(&path))?
        .with_context(|| format!("{name} directory absent"))
}

/// Raw, occurrence-bound owner facts. Parsing does not qualify the runtime:
/// callers must join the admitted owner launch, expected request, observer
/// transcript and provider-lifetime settlement independently.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerOuterObservation {
    schema: String,
    operation_id: String,
    challenge_digest: String,
    input_record_sha256: String,
    pub applied_receipt: lillux::LinuxSandboxAppliedLaunchReceipt,
    pub protocol_input_sha256: String,
    pub protocol_input_bytes: usize,
    pub forwarded_protocol_output_sha256: String,
    pub forwarded_protocol_output_bytes: usize,
    pub full_protocol_output_sha256: String,
    pub full_protocol_output_bytes: usize,
    pub diagnostics_sha256: String,
    pub diagnostics_bytes: usize,
    pub namespace_exit: String,
}

impl ConsumerOuterObservation {
    /// Corroborate the observer's complete wire transcript and expected target.
    /// This is still not source authentication, a semantic protocol verdict, or
    /// provider death. The observer must drain all forwarded output, including
    /// trailing notifications, rather than supplying only its parsed prefix.
    pub fn check_launch_and_transcript(
        &self,
        expected_request: &lillux::LinuxSandboxRequest,
        sent: &[u8],
        forwarded_output: &[u8],
    ) -> Result<()> {
        ensure!(
            self.applied_receipt
                .matches_request(expected_request)
                .map_err(anyhow::Error::msg)?,
            "outer applied launch differs from expected retained request"
        );
        ensure!(
            expected_request.overlay.is_none() && expected_request.fixed_parent_views.is_empty(),
            "outer observation checker requires the explicit consumer mount profile"
        );
        let expected_mounts =
            lillux::LinuxSandboxMountPreparationCommitments::from_admitted_mounts(
                &expected_request.mounts,
                &[],
            )
            .map_err(anyhow::Error::msg)?;
        ensure!(
            self.applied_receipt.post_release_mount_view == expected_mounts,
            "outer applied mount view differs from admitted inputs"
        );
        ensure!(
            self.applied_receipt.owned_child_pid > 0
                && self.applied_receipt.namespace_pid == 1
                && self.applied_receipt.effective_uid == 1
                && self.applied_receipt.effective_gid == 1
                && self.applied_receipt.no_new_privs
                && self.applied_receipt.seccomp_mode == 2,
            "outer applied receipt lacks native controls"
        );
        require_exact_wire_bytes(sent, self.protocol_input_bytes, &self.protocol_input_sha256)?;
        require_exact_wire_bytes(
            forwarded_output,
            self.forwarded_protocol_output_bytes,
            &self.forwarded_protocol_output_sha256,
        )?;
        Ok(())
    }

    fn parse_for_inputs(
        bytes: &[u8],
        record: &ConsumerInputRecord,
        challenge: &ConsumerRuntimeChallenge,
    ) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_OUTER_OBSERVATION_BYTES,
            "outer observation exceeds bound"
        );
        let value: Self = serde_json::from_slice(bytes)?;
        ensure!(
            lillux::canonical_json(&serde_json::to_value(&value)?)?.as_bytes() == bytes,
            "outer observation is not canonical"
        );
        ensure!(
            value.schema == "ryeos.codex.consumer-outer-observation.v1"
                && value.operation_id == challenge.intent.operation_id
                && value.challenge_digest == challenge.intent.consumer_challenge_digest()?
                && value.input_record_sha256 == lillux::sha256_hex(&record.canonical_bytes()?),
            "outer observation substituted attempt coordinates"
        );
        for digest in [
            &value.protocol_input_sha256,
            &value.forwarded_protocol_output_sha256,
            &value.full_protocol_output_sha256,
            &value.diagnostics_sha256,
        ] {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "outer observation digest is not canonical SHA-256"
            );
        }
        ensure!(
            value.protocol_input_bytes > 0
                && value.protocol_input_bytes <= 1024 * 1024
                && value.full_protocol_output_bytes > 0
                && value.full_protocol_output_bytes <= 1024 * 1024
                && value.forwarded_protocol_output_bytes <= value.full_protocol_output_bytes
                && value.diagnostics_bytes <= 1024 * 1024
                && !value.namespace_exit.is_empty()
                && value.namespace_exit.len() <= 256,
            "outer observation counts or exit exceed bounds"
        );
        if value.forwarded_protocol_output_bytes == value.full_protocol_output_bytes {
            ensure!(
                value.forwarded_protocol_output_sha256 == value.full_protocol_output_sha256,
                "outer complete forwarded transcript has conflicting digests"
            );
        }
        let empty_digest = lillux::sha256_hex(&[]);
        ensure!(
            (value.forwarded_protocol_output_bytes != 0
                || value.forwarded_protocol_output_sha256 == empty_digest)
                && (value.diagnostics_bytes != 0 || value.diagnostics_sha256 == empty_digest),
            "outer empty transcript has nonempty digest"
        );
        Ok(value)
    }
}

fn require_exact_wire_bytes(bytes: &[u8], count: usize, digest: &str) -> Result<()> {
    ensure!(
        bytes.len() == count && count <= 1024 * 1024 && lillux::sha256_hex(bytes) == digest,
        "outer owner and observer wire transcript differ"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_join_rejects_prefix_tail_and_same_length_mutation() {
        let bytes = b"reply\nnotification\n";
        let digest = lillux::sha256_hex(bytes);
        require_exact_wire_bytes(bytes, bytes.len(), &digest).unwrap();
        assert!(require_exact_wire_bytes(b"reply\n", bytes.len(), &digest).is_err());
        assert!(
            require_exact_wire_bytes(b"reply\nnotification\nextra", bytes.len(), &digest).is_err()
        );
        assert!(require_exact_wire_bytes(b"Reply\nnotification\n", bytes.len(), &digest).is_err());
        assert!(require_exact_wire_bytes(bytes, bytes.len() - 1, &digest).is_err());
    }
}

/// Missing evidence is pending, never success or permission to relaunch.
#[allow(clippy::too_many_arguments)]
pub fn read_outer_observation(
    imported: &ImportedConsumerInputs,
    record: &ConsumerInputRecord,
    challenge: &ConsumerRuntimeChallenge,
    fresh_home: &PinnedDirectory,
    native_control: &PinnedDirectory,
    protected_owner: &PinnedDirectory,
    deadline: MonotonicDeadline,
) -> Result<Option<ConsumerOuterObservation>> {
    imported.require_record_challenge(record, challenge)?;
    imported.require_control_root(protected_owner)?;
    protected_owner.require_disjoint_directory_tree(fresh_home)?;
    protected_owner.require_disjoint_directory_tree(native_control)?;
    ensure!(!deadline.has_elapsed(), "outer observation lookup expired");
    let result = match protected_owner.open_pinned_regular(OsStr::new(OUTER_OBSERVATION), false)? {
        None => None,
        Some(file) => {
            ensure!(
                file.permission_mode()? == 0o400,
                "outer observation mode changed"
            );
            let bytes =
                file.read_stable_bounded(&file.observation()?, MAX_OUTER_OBSERVATION_BYTES as u64)?;
            Some(ConsumerOuterObservation::parse_for_inputs(
                &bytes, record, challenge,
            )?)
        }
    };
    protected_owner.ensure_path_binding()?;
    imported.require_record_challenge(record, challenge)?;
    ensure!(
        !deadline.has_elapsed(),
        "outer observation read exceeded deadline"
    );
    Ok(result)
}

/// Deliver an exact one-way settlement request to the protected owner, never
/// through a target-writable mount. An existing identical request is observable
/// delivery, not permission to restart or proof that settlement has completed.
#[allow(clippy::too_many_arguments)]
pub fn request_outer_settlement(
    imported: &ImportedConsumerInputs,
    record: &ConsumerInputRecord,
    challenge: &ConsumerRuntimeChallenge,
    fresh_home: &PinnedDirectory,
    native_control: &PinnedDirectory,
    protected_owner: &PinnedDirectory,
    deadline: MonotonicDeadline,
) -> Result<()> {
    imported.require_record_challenge(record, challenge)?;
    imported.require_control_root(protected_owner)?;
    protected_owner.require_disjoint_directory_tree(fresh_home)?;
    protected_owner.require_disjoint_directory_tree(native_control)?;
    ensure!(!deadline.has_elapsed(), "outer settlement delivery expired");
    let expected =
        ConsumerOuterCompletionRequest::for_inputs(record, challenge)?.canonical_bytes()?;
    let started = protected_owner
        .open_pinned_regular(OsStr::new(OUTER_START), false)?
        .context("outer owner has not started")?;
    ensure!(
        started.permission_mode()? == 0o400,
        "outer start mode changed"
    );
    let started_bytes =
        started.read_stable_bounded(&started.observation()?, MAX_OUTER_COMPLETION_BYTES as u64)?;
    ConsumerOuterCompletionRequest::parse_for_inputs(&started_bytes, record, challenge)?;
    if protected_owner
        .atomic_create_pinned_regular(OsStr::new(OUTER_FINISH), &expected, 0o400)?
        .is_none()
    {
        ensure!(
            completion_requested(protected_owner, record, challenge)?,
            "outer settlement request disappeared"
        );
    }
    protected_owner.ensure_path_binding()?;
    imported.require_record_challenge(record, challenge)?;
    ensure!(
        !deadline.has_elapsed(),
        "outer settlement delivery exceeded deadline"
    );
    Ok(())
}

fn completion_requested(
    owner: &PinnedDirectory,
    record: &ConsumerInputRecord,
    challenge: &ConsumerRuntimeChallenge,
) -> Result<bool> {
    owner.ensure_path_binding()?;
    let Some(file) = owner.open_pinned_regular(OsStr::new(OUTER_FINISH), false)? else {
        return Ok(false);
    };
    ensure!(
        file.permission_mode()? == 0o400,
        "outer completion request mode changed"
    );
    let bytes =
        file.read_stable_bounded(&file.observation()?, MAX_OUTER_COMPLETION_BYTES as u64)?;
    ConsumerOuterCompletionRequest::parse_for_inputs(&bytes, record, challenge)?;
    owner.ensure_path_binding()?;
    Ok(true)
}

/// Relay existing app-server bytes to the exact retained Codex target, then
/// settle its entire native PID namespace before retaining protected facts.
///
/// The caller must be a dedicated, unprivileged, single-threaded process; its
/// admitted launch/source and private generation are authenticated elsewhere.
/// This function does not authenticate archive delivery or provider authority.
/// No thread or peer is started after the namespace transition. EOF, malformed
/// completion, overflow and deadline failure all take the shared cleanup path
/// and never produce a successful observation or permission to restart.
#[allow(clippy::too_many_arguments)]
pub fn run_outer_protocol(
    imported: &ImportedConsumerInputs,
    record: &ConsumerInputRecord,
    challenge: &ConsumerRuntimeChallenge,
    fresh_home: &PinnedDirectory,
    native_control: &PinnedDirectory,
    protected_owner: &PinnedDirectory,
    input: InheritedPipeInput,
    output: &mut InheritedPipeOutput,
    interrupt: PipeInterrupt,
    active: MonotonicDeadline,
    cleanup: MonotonicDeadline,
) -> Result<()> {
    imported.require_record_challenge(record, challenge)?;
    ensure!(
        !active.has_elapsed(),
        "outer owner deadline expired before preparation"
    );
    protected_owner.require_owner_private_directory()?;
    protected_owner.ensure_path_binding()?;
    for name in [OUTER_START, OUTER_FINISH, OUTER_OBSERVATION] {
        ensure!(
            protected_owner
                .open_pinned_regular(OsStr::new(name), false)?
                .is_none(),
            "outer occurrence was already used; live pipes cannot be recreated"
        );
    }
    let request = imported.prepare_codex_outer_request(
        record,
        challenge,
        fresh_home,
        native_control,
        protected_owner,
    )?;
    let start_bytes =
        ConsumerOuterCompletionRequest::for_inputs(record, challenge)?.canonical_bytes()?;
    protected_owner
        .atomic_create_pinned_regular(OsStr::new(OUTER_START), &start_bytes, 0o400)?
        .context("outer owner execution was already attempted")?;
    let observed = crate::native_guest::run_prepared_protocol(
        || completion_requested(protected_owner, record, challenge),
        request.request().clone(),
        input,
        output,
        interrupt,
        active,
        cleanup,
    )?;
    // Keep request and its source/mount/protected-owner custody alive until the
    // namespace is settled and all raw buffers are drained by the shared loop.
    // The target had no mount or inherited descriptor for this owner directory.
    protected_owner.ensure_path_binding()?;
    ensure!(
        !cleanup.has_elapsed(),
        "outer owner evidence deadline expired"
    );
    let forwarded = observed
        .received
        .get(..observed.forwarded_output_bytes)
        .context("outer owner forwarded prefix exceeds observed output")?;
    let evidence = ConsumerOuterObservation {
        schema: "ryeos.codex.consumer-outer-observation.v1".into(),
        operation_id: challenge.intent.operation_id.clone(),
        challenge_digest: challenge.intent.consumer_challenge_digest()?,
        input_record_sha256: lillux::sha256_hex(&record.canonical_bytes()?),
        applied_receipt: observed.applied_receipt,
        protocol_input_sha256: lillux::sha256_hex(&observed.sent),
        protocol_input_bytes: observed.sent.len(),
        forwarded_protocol_output_sha256: lillux::sha256_hex(forwarded),
        forwarded_protocol_output_bytes: observed.forwarded_output_bytes,
        full_protocol_output_sha256: lillux::sha256_hex(&observed.received),
        full_protocol_output_bytes: observed.received.len(),
        diagnostics_sha256: lillux::sha256_hex(&observed.diagnostics),
        diagnostics_bytes: observed.diagnostics.len(),
        namespace_exit: observed.namespace_exit,
    };
    let bytes = lillux::canonical_json(&serde_json::to_value(&evidence)?)?.into_bytes();
    ConsumerOuterObservation::parse_for_inputs(&bytes, record, challenge)?;
    ensure!(
        bytes.len() <= MAX_OUTER_OBSERVATION_BYTES,
        "outer owner evidence exceeds bound"
    );
    protected_owner
        .atomic_create_pinned_regular(OsStr::new(OUTER_OBSERVATION), &bytes, 0o400)?
        .context("outer owner observation already exists")?;
    protected_owner.ensure_path_binding()?;
    ensure!(
        !cleanup.has_elapsed(),
        "outer owner evidence publication exceeded deadline"
    );
    Ok(())
}
