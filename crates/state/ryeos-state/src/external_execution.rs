//! Exact, occurrence-scoped external execution channel records.
//!
//! Authentication is not admission, native isolation qualification, candidate
//! validation or publication. In particular a signed supervisor observation is
//! not a Lillux proof. The owning runtime retains those independent checks.
//! These keys never enter the node's enrolled-principal namespace.

use anyhow::{Context as _, Result, bail, ensure};
use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::{Signature, Signer as _, SigningKey, VerifyingKey};
use lillux::process_control::CapabilityEvidence;
pub use ryeos_external_execution_contract::{
    ExternalCommandOutputCommitment, ExternalCommandOutputStream, ExternalCommandTermination,
    ExternalCommandTerminationReason, ExternalExecutionMode, ExternalTargetExit,
};
use serde::{Deserialize, Serialize};

pub mod admission;
pub mod connector;
pub mod export;
pub mod guest_journal;
pub mod journal;
pub mod native_journal;
pub mod qualification_controller_canary;
pub mod qualification_session;
pub mod runtime_content;
pub mod supervisor_journal;
pub mod transcript;
pub mod transport;

pub const MAX_FRAME_BYTES: usize = 384 * 1024;
pub const MAX_CHUNK_BYTES: usize = 256 * 1024;
/// Immutable wire identity for an occurrence-scoped execution channel.
///
/// Keep this independent from the enclosing persistent-session capsule,
/// external-candidate requirement and supervisor-bootstrap schemas. Those
/// documents evolve under different authorities and are not compatibility
/// aliases for this binding.
pub const EXECUTION_CHANNEL_BINDING_SCHEMA: u32 = 5;
/// First-generation logical base/candidate ceiling shared by native capture
/// and receiver verification; transport budgets may independently be smaller.
pub const MAX_CANDIDATE_CONTENT_BYTES: u64 = 1024 * 1024 * 1024;
/// One terminal control frame per direction has capacity independent of data.
/// Acknowledgements cannot consume this reserve. Lifecycle validation also
/// refuses repeated Cancel/Stopped, so it is not an unbounded emergency lane.
pub const TERMINAL_CONTROL_BYTES: u64 = 4096;
/// The entire finite Q finish exchange is carved out of signed journal ceilings.
/// Includes EOF, completion, native settlement and exact lifecycle receipts.
/// The journal alone classifies correlated receipts; idle polls stay ordinary.
/// Supervisor: EOF + settlement + completion Applied receipt + three EOF
/// receipt replies + two settlement receipt replies = eight. Owner: completion
/// + three EOF receipts + two settlement receipts = six. Scope claim/finish is
/// atomic; its externally visible receipts are Retained and Applied only.
pub const Q_FINISH_FRAMES: u32 = 8;
pub const Q_FINISH_BYTES: u64 = 128 * 1024;
const SIGNATURE_DOMAIN: &[u8] = b"ryeos.external-execution.frame.v1\0";
/// Bounds on what a supervisor may report about its own boot and host.
pub const MAX_READY_VERIFIED_INPUTS: usize = 8;
pub const MAX_CAPABILITY_REPORT_ENTRIES: usize = 64;
const MAX_CAPABILITY_NAME_BYTES: usize = 128;
const MAX_CAPABILITY_TEXT_BYTES: usize = 512;

/// Shared by precontact admission, bootstrap validation and channel validation.
pub fn validate_channel_journal_limits(
    mode: ExternalExecutionMode,
    max_frames: u32,
    max_bytes: u64,
) -> Result<()> {
    ensure!(
        (1..=65_536).contains(&max_frames) && (1..=64 * 1024 * 1024).contains(&max_bytes),
        "external channel journal exceeds bounds"
    );
    if matches!(
        mode,
        ExternalExecutionMode::QualificationStructuredSession {}
    ) {
        ensure!(
            max_frames > Q_FINISH_FRAMES && max_bytes > Q_FINISH_BYTES,
            "Q channel journal cannot reserve native settlement and ordinary traffic"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionChannelBinding {
    pub schema: u32,
    pub execution_mode: ExternalExecutionMode,
    pub placement_thread_id: String,
    pub allocation_request_digest: String,
    pub occurrence_id: String,
    pub admitted_capsule_hash: String,
    pub base_snapshot_hash: String,
    pub execution_binding_hash: String,
    pub supervisor_runtime_hash: String,
    /// Exact admitted command/mount recipe and qualified runtime identities.
    /// This prevents a lifecycle adapter from substituting a different program
    /// beneath the same runtime tree hash.
    pub candidate_program_digest: String,
    /// Fresh random 256-bit identity; never reused after replacement.
    pub channel_nonce: String,
    /// Canonical base64 Ed25519 keys, not profile or cloud-account credentials.
    pub owner_public_key: String,
    pub supervisor_public_key: String,
    pub issued_at_ms: i64,
    pub execution_deadline_ms: i64,
    pub expires_at_ms: i64,
    /// Maximum raw candidate bytes that may cross the export protocol. This is
    /// distinct from `max_bytes`, which bounds the larger authenticated wire
    /// transcript after JSON, base64, signatures, acknowledgements and control
    /// frames are included. Qualification sessions and direct commands require
    /// zero because neither mode has candidate export authority.
    pub candidate_export_max_bytes: u64,
    /// Per-direction journal bounds including acknowledged frames. Q reserves
    /// Q_FINISH_FRAMES and Q_FINISH_BYTES within these ceilings per direction.
    /// One bounded Cancel/Stopped frame is reserved independently.
    pub max_frames: u32,
    pub max_bytes: u64,
}

impl ExecutionChannelBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == EXECUTION_CHANNEL_BINDING_SCHEMA,
            "unsupported external channel schema"
        );
        self.execution_mode.validate()?;
        text(&self.placement_thread_id, 256)?;
        text(&self.occurrence_id, 512)?;
        for digest in [
            &self.allocation_request_digest,
            &self.admitted_capsule_hash,
            &self.base_snapshot_hash,
            &self.execution_binding_hash,
            &self.supervisor_runtime_hash,
            &self.candidate_program_digest,
            &self.channel_nonce,
        ] {
            hash(digest)?;
        }
        let owner = public_key(&self.owner_public_key)?;
        let supervisor = public_key(&self.supervisor_public_key)?;
        ensure!(owner != supervisor, "channel roles require distinct keys");
        ensure!(
            self.issued_at_ms > 0
                && self.execution_deadline_ms > self.issued_at_ms
                && self.execution_deadline_ms - self.issued_at_ms <= 3_600_000
                && self.expires_at_ms > self.execution_deadline_ms
                && self.expires_at_ms - self.issued_at_ms <= 86_400_000,
            "external channel lifetime is outside its bounds"
        );
        validate_channel_journal_limits(self.execution_mode, self.max_frames, self.max_bytes)?;
        ensure!(
            match self.execution_mode {
                ExternalExecutionMode::StructuredSession {} =>
                    (1..=self.max_bytes).contains(&self.candidate_export_max_bytes),
                ExternalExecutionMode::QualificationStructuredSession {}
                | ExternalExecutionMode::DirectCommand { .. } => {
                    self.candidate_export_max_bytes == 0
                }
            },
            "external channel export authority does not match execution mode"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelDirection {
    OwnerToSupervisor,
    SupervisorToOwner,
}

impl ChannelDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerToSupervisor => "owner_to_supervisor",
            Self::SupervisorToOwner => "supervisor_to_owner",
        }
    }
    pub fn opposite(self) -> Self {
        match self {
            Self::OwnerToSupervisor => Self::SupervisorToOwner,
            Self::SupervisorToOwner => Self::OwnerToSupervisor,
        }
    }
}

/// What a supervisor binds into its Ready about the boot it runs on.
/// Workload: an owner that activates a supervisor on a remote guest it
/// cannot observe, and must notice a reboot or a second activation. The
/// owner compares every field against what it retained before contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorReadyBoot {
    /// Kernel boot id of the host, as the lowercase hyphenated UUID Linux
    /// reports in `/proc/sys/kernel/random/boot_id`.
    pub boot_id: String,
    /// Digests of the activation inputs the supervisor verified on this boot
    /// before reporting ready, in the order the owner delivered them.
    pub verified_input_digests: Vec<String>,
    /// Wall-clock time the supervisor became ready.
    pub ready_at_ms: i64,
}

impl SupervisorReadyBoot {
    fn validate(&self, binding: &ExecutionChannelBinding) -> Result<()> {
        boot_id(&self.boot_id)?;
        ensure!(
            !self.verified_input_digests.is_empty()
                && self.verified_input_digests.len() <= MAX_READY_VERIFIED_INPUTS,
            "supervisor Ready verified-input list is empty or exceeds its bound"
        );
        for digest in &self.verified_input_digests {
            hash(digest)?;
        }
        ensure!(
            self.ready_at_ms >= binding.issued_at_ms && self.ready_at_ms < binding.expires_at_ms,
            "supervisor Ready time is outside the channel lifetime"
        );
        Ok(())
    }
}

/// Tool protocol bytes cannot manufacture a control/export observation. The
/// protected supervisor, not the candidate endpoint, signs control frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionChannelPayload {
    Ready {
        supervisor_runtime_hash: String,
        base_snapshot_hash: String,
        /// The boot the supervisor reports ready from. Absent when the owner
        /// does not ask for it; an owner that needs it refuses a Ready
        /// without it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        boot: Option<SupervisorReadyBoot>,
    },
    /// The supervisor's own capability probe of the host it runs on, taken
    /// on the boot named in its Ready and before any workload code runs
    /// there. Workload: a disposable remote guest whose owner admits it only
    /// if this report covers the workload's guest requirements. Each entry
    /// is evidence the owner grades with its own requirement data; a missing
    /// name carries the supervisor's plain reason. The owner that asked
    /// consumes it directly; a channel journal retains it, linked after Ready
    /// so the next frame chains to it, but never applies it.
    CapabilityReport {
        boot_id: String,
        provided: BTreeMap<String, CapabilityEvidence>,
        missing: BTreeMap<String, String>,
    },
    /// Exact child-owned pre-exec observation projected from the guest's
    /// immutable occurrence row. This proves neither exec success nor a
    /// qualified Worker; both remain separate authority joins.
    RuntimeApplied {
        launcher_occurrence_digest: String,
        candidate_program_digest: String,
        receipt: lillux::LinuxSandboxAppliedLaunchReceipt,
    },
    Release,
    /// Controller request to reap only a qualification session's native
    /// namespace. This request makes no assertion about verifier success.
    QualificationComplete {
        completion_request_digest: String,
    },
    /// Guest projection of an immutable native reap row, never a provider or
    /// semantic qualification result.
    QualificationScopeSettled {
        observation: NativeQualificationScopeSettlement,
    },
    ProtocolBytes {
        bytes_base64: String,
    },
    /// Candidate protocol stdout reached EOF. This is an endpoint transport
    /// observation, not candidate success, writer exclusion, or cleanup.
    ProtocolEof,
    /// A bounded, ordered prefix of the actual executable's stdout or stderr.
    /// Per-stream continuity and terminal commitments are journal-owned.
    CommandOutput {
        stream: ExternalCommandOutputStream,
        offset: u64,
        bytes_base64: String,
    },
    /// Actual target exit plus exact retained output commitments. This is not
    /// a surrogate cleanup init's exit, a writer-exclusion proof, or cleanup evidence.
    CommandTerminated {
        observation: ExternalCommandTermination,
    },
    Quiesce {
        completion_request_digest: String,
    },
    ExportObjectChunk {
        content_kind: ExportContentKind,
        object_hash: String,
        offset: u64,
        bytes_base64: String,
        final_chunk: bool,
    },
    ExportSealed {
        candidate_snapshot_hash: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        candidate_output_capture_hash: Option<String>,
        completion_request_digest: String,
        writer_exclusion_evidence_hash: String,
    },
    Cancel,
    Stopped {
        reason: ExternalStopReason,
    },
    Acknowledge {
        /// Exact peer frame whose application state is reported. This is
        /// deliberately distinct from the frame header's cumulative transport
        /// receive frontier: an older claimed operation may finish after later
        /// peer frames have already been retained.
        peer_frame_sequence: u64,
        peer_frame_digest: String,
        /// Durable destination-side state. Transport receipt alone may report
        /// only `retained`; `claimed` and `applied` require the destination's
        /// journal transition, while `revoked` proves it was never applied.
        application: ExecutionFrameApplication,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionFrameApplication {
    Retained,
    Claimed,
    Applied,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalStopReason {
    Cancelled,
    Deadline,
    EndpointExited,
    Fault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportContentKind {
    Object,
    Blob,
}

/// Protected-supervisor testimony, transported as a bounded canonical CAS
/// blob. The receiver validates its binding; only the native supervisor may
/// generate it from its non-serializable Lillux terminal proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeWriterExclusionObservation {
    pub schema: u32,
    pub binding_digest: String,
    pub base_snapshot_hash: String,
    pub completion_request_digest: String,
    pub mechanism: NativeWriterExclusionMechanism,
    pub exit: NativeNamespaceExit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeQualificationScopeSettlementObservation {
    pub schema: u32,
    pub binding_digest: String,
    pub launcher_occurrence_digest: String,
    pub applied_runtime_receipt_digest: String,
    pub completion_frame_digest: String,
    pub completion_request_digest: String,
    pub mechanism: NativeWriterExclusionMechanism,
    pub exit: NativeNamespaceExit,
}

/// Current signed settlement wrapper. The nested native predecessor remains
/// capture-free so immutable capture metadata cannot form a hash cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeQualificationScopeSettlement {
    pub schema: u32,
    pub observation: NativeQualificationScopeSettlementObservation,
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub fixture_capture: Option<crate::objects::qualification_fixture::QFixtureCaptureReference>,
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub fixture_content: Option<crate::objects::qualification_fixture::QFixtureCaptureContent>,
}

impl NativeQualificationScopeSettlement {
    pub fn validate(&self, binding: &ExecutionChannelBinding) -> Result<()> {
        ensure!(self.schema == 3, "unsupported Q scope settlement wrapper");
        self.observation.validate(binding)?;
        match (&self.fixture_capture, &self.fixture_content) {
            (None, None) => {}
            (Some(reference), Some(content)) => {
                content.verify_reference(reference, binding, &self.observation)?;
            }
            _ => bail!("Q scope settlement fixture reference/content presence differs"),
        }
        Ok(())
    }
}

impl NativeQualificationScopeSettlementObservation {
    pub fn validate(&self, binding: &ExecutionChannelBinding) -> Result<()> {
        ensure!(
            binding.execution_mode == ExternalExecutionMode::QualificationStructuredSession {}
                && self.schema == 1
                && self.binding_digest == binding.digest()?
                && self.mechanism == NativeWriterExclusionMechanism::NamespaceInitReaped,
            "qualification scope settlement changed its native authority"
        );
        for digest in [
            &self.launcher_occurrence_digest,
            &self.applied_runtime_receipt_digest,
            &self.completion_frame_digest,
            &self.completion_request_digest,
        ] {
            hash(digest)?;
        }
        ensure!(
            match self.exit {
                NativeNamespaceExit::Code(n) => (0..=255).contains(&n),
                NativeNamespaceExit::Signal(n) => (1..=64).contains(&n),
            },
            "qualification scope settlement has an invalid native exit"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeWriterExclusionMechanism {
    NamespaceInitReaped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum NativeNamespaceExit {
    Code(i32),
    Signal(i32),
}

impl NativeWriterExclusionObservation {
    pub fn validate(&self, binding: &ExecutionChannelBinding, completion: &str) -> Result<()> {
        hash(completion)?;
        ensure!(
            self.schema == 1
                && self.binding_digest == binding.digest()?
                && self.base_snapshot_hash == binding.base_snapshot_hash
                && self.completion_request_digest == completion,
            "writer-exclusion observation changed its execution authority"
        );
        ensure!(
            match self.exit {
                NativeNamespaceExit::Code(n) => (0..=255).contains(&n),
                NativeNamespaceExit::Signal(n) => (1..=64).contains(&n),
            },
            "writer-exclusion observation has an invalid terminal status"
        );
        Ok(())
    }
}

impl ExecutionChannelPayload {
    pub fn uses_terminal_reserve(&self) -> bool {
        matches!(self, Self::Cancel | Self::Stopped { .. })
    }

    fn validate(
        &self,
        binding: &ExecutionChannelBinding,
        direction: ChannelDirection,
    ) -> Result<()> {
        use ExecutionChannelPayload::*;
        let owner_only = matches!(
            self,
            Release | Quiesce { .. } | QualificationComplete { .. } | Cancel
        );
        let supervisor_only = matches!(
            self,
            Ready { .. }
                | CapabilityReport { .. }
                | RuntimeApplied { .. }
                | QualificationScopeSettled { .. }
                | ProtocolEof
                | CommandOutput { .. }
                | CommandTerminated { .. }
                | ExportObjectChunk { .. }
                | ExportSealed { .. }
                | Stopped { .. }
        );
        ensure!(
            !owner_only || direction == ChannelDirection::OwnerToSupervisor,
            "supervisor cannot author an owner command"
        );
        ensure!(
            !supervisor_only || direction == ChannelDirection::SupervisorToOwner,
            "owner cannot author supervisor observations"
        );
        ensure!(
            !matches!(self, ProtocolBytes { .. } | ProtocolEof)
                || matches!(
                    binding.execution_mode,
                    ExternalExecutionMode::StructuredSession {}
                        | ExternalExecutionMode::QualificationStructuredSession {}
                ),
            "direct command channel cannot carry session protocol"
        );
        ensure!(
            !matches!(
                self,
                Quiesce { .. } | ExportObjectChunk { .. } | ExportSealed { .. }
            ) || matches!(
                binding.execution_mode,
                ExternalExecutionMode::StructuredSession {}
            ),
            "execution mode cannot carry candidate export"
        );
        ensure!(
            !matches!(
                self,
                QualificationComplete { .. } | QualificationScopeSettled { .. }
            ) || binding.execution_mode == ExternalExecutionMode::QualificationStructuredSession {},
            "qualification settlement requires a qualification session"
        );
        match self {
            Ready {
                supervisor_runtime_hash,
                base_snapshot_hash,
                boot,
            } => {
                ensure!(
                    supervisor_runtime_hash == &binding.supervisor_runtime_hash
                        && base_snapshot_hash == &binding.base_snapshot_hash,
                    "supervisor readiness changed admitted inputs"
                );
                if let Some(boot) = boot {
                    boot.validate(binding)?;
                }
            }
            CapabilityReport {
                boot_id: reported_boot,
                provided,
                missing,
            } => {
                boot_id(reported_boot)?;
                ensure!(
                    provided.len() <= MAX_CAPABILITY_REPORT_ENTRIES
                        && missing.len() <= MAX_CAPABILITY_REPORT_ENTRIES,
                    "capability report exceeds its entry bound"
                );
                for (name, evidence) in provided {
                    capability_text(name, MAX_CAPABILITY_NAME_BYTES)?;
                    capability_text(evidence.source.as_str(), MAX_CAPABILITY_NAME_BYTES)?;
                    capability_text(&evidence.provider, MAX_CAPABILITY_NAME_BYTES)?;
                    capability_text(&evidence.basis, MAX_CAPABILITY_TEXT_BYTES)?;
                }
                for (name, reason) in missing {
                    capability_text(name, MAX_CAPABILITY_NAME_BYTES)?;
                    capability_text(reason, MAX_CAPABILITY_TEXT_BYTES)?;
                    ensure!(
                        !provided.contains_key(name),
                        "capability report names {name} both provided and missing"
                    );
                }
            }
            RuntimeApplied {
                launcher_occurrence_digest,
                candidate_program_digest,
                receipt,
            } => {
                hash(launcher_occurrence_digest)?;
                ensure!(
                    candidate_program_digest == &binding.candidate_program_digest
                        && receipt.owned_child_pid > 0
                        && receipt.namespace_pid == 1
                        && receipt.effective_uid == 1
                        && receipt.effective_gid == 1
                        && receipt.no_new_privs
                        && receipt.seccomp_mode == 2
                        && receipt.post_release_mount_view.schema == 1
                        && receipt.post_release_mount_view.mount_count > 0,
                    "external runtime-applied frame changed bound native controls"
                );
            }
            QualificationComplete {
                completion_request_digest,
            } => hash(completion_request_digest)?,
            QualificationScopeSettled { observation } => observation.validate(binding)?,
            ProtocolBytes { bytes_base64 } => {
                chunk(bytes_base64, false)?;
            }
            CommandOutput {
                stream,
                offset,
                bytes_base64,
            } => {
                let limit = binding.execution_mode.output_limit(*stream)?;
                let bytes = chunk(bytes_base64, false)?;
                ensure!(
                    offset
                        .checked_add(bytes.len() as u64)
                        .is_some_and(|n| n <= limit),
                    "external command output exceeds admitted stream bounds"
                );
            }
            CommandTerminated { observation } => {
                observation.validate(binding.execution_mode)?;
            }
            Quiesce {
                completion_request_digest,
            } => hash(completion_request_digest)?,
            ExportObjectChunk {
                object_hash,
                offset,
                bytes_base64,
                final_chunk,
                ..
            } => {
                hash(object_hash)?;
                let bytes = chunk(bytes_base64, *final_chunk)?;
                ensure!(
                    offset
                        .checked_add(bytes.len() as u64)
                        .is_some_and(|n| n <= binding.max_bytes),
                    "external object exceeds admitted transfer bounds"
                );
            }
            ExportSealed {
                candidate_snapshot_hash,
                candidate_output_capture_hash,
                completion_request_digest,
                writer_exclusion_evidence_hash,
            } => {
                hash(candidate_snapshot_hash)?;
                if let Some(hash_value) = candidate_output_capture_hash {
                    hash(hash_value)?;
                }
                hash(completion_request_digest)?;
                hash(writer_exclusion_evidence_hash)?;
            }
            Acknowledge {
                peer_frame_sequence,
                peer_frame_digest,
                ..
            } => {
                ensure!(
                    *peer_frame_sequence > 0,
                    "external acknowledgement target sequence is zero"
                );
                hash(peer_frame_digest)?;
            }
            Release | ProtocolEof | Cancel | Stopped { .. } => {}
        }
        Ok(())
    }
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionFrame {
    pub schema: u32,
    pub binding_digest: String,
    pub direction: ChannelDirection,
    pub sequence: u64,
    /// Sequence zero has no digest. Every later message names its predecessor.
    pub previous_frame_digest: Option<String>,
    pub acknowledged_peer_sequence: u64,
    pub payload: ExecutionChannelPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedExecutionFrame {
    pub frame: ExecutionFrame,
    pub signature: String,
}

/// Construction is possible only after exact binding/signature verification.
/// Durable ordering and application claims are the journal owner's concern.
pub struct AuthenticatedExecutionFrame {
    signed: SignedExecutionFrame,
    digest: String,
    canonical: String,
}

impl AuthenticatedExecutionFrame {
    pub fn frame(&self) -> &ExecutionFrame {
        &self.signed.frame
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn canonical(&self) -> &str {
        &self.canonical
    }
    pub fn protocol_bytes(&self) -> Result<Vec<u8>> {
        match &self.frame().payload {
            ExecutionChannelPayload::ProtocolBytes { bytes_base64 } => chunk(bytes_base64, false),
            _ => bail!("external frame does not carry protocol bytes"),
        }
    }
}

impl SignedExecutionFrame {
    pub fn sign(
        frame: ExecutionFrame,
        binding: &ExecutionChannelBinding,
        key: &SigningKey,
    ) -> Result<Self> {
        validate_frame(&frame, binding)?;
        let expected = key_for(binding, frame.direction)?;
        ensure!(
            key.verifying_key() == expected,
            "wrong external channel signing role"
        );
        let signature = STANDARD.encode(key.sign(&signing_bytes(&frame)?).to_bytes());
        Ok(Self { frame, signature })
    }

    pub fn decode_and_verify(
        bytes: &[u8],
        binding: &ExecutionChannelBinding,
        now_ms: i64,
    ) -> Result<AuthenticatedExecutionFrame> {
        ensure!(
            bytes.len() <= MAX_FRAME_BYTES,
            "external frame exceeds wire bound"
        );
        let signed: Self =
            serde_json::from_slice(bytes).context("decode external execution frame")?;
        validate_frame(&signed.frame, binding)?;
        ensure!(
            now_ms >= binding.issued_at_ms && now_ms < binding.expires_at_ms,
            "external channel is outside its admitted lifetime"
        );
        let canonical = lillux::canonical_json(&serde_json::to_value(&signed)?)?;
        ensure!(
            canonical.as_bytes() == bytes,
            "external frame is not canonical"
        );
        let signature_bytes = STANDARD.decode(&signed.signature)?;
        ensure!(
            STANDARD.encode(&signature_bytes) == signed.signature,
            "noncanonical signature"
        );
        let signature = Signature::from_slice(&signature_bytes)?;
        key_for(binding, signed.frame.direction)?
            .verify_strict(&signing_bytes(&signed.frame)?, &signature)?;
        Ok(AuthenticatedExecutionFrame {
            digest: lillux::sha256_hex(bytes),
            signed,
            canonical,
        })
    }
}

fn validate_frame(frame: &ExecutionFrame, binding: &ExecutionChannelBinding) -> Result<()> {
    ensure!(
        frame.schema == 1 && frame.binding_digest == binding.digest()?,
        "external frame binding mismatch"
    );
    ensure!(
        (1..=u64::from(binding.max_frames) + 1).contains(&frame.sequence)
            && frame.acknowledged_peer_sequence <= u64::from(binding.max_frames) + 1,
        "external frame sequence exceeds its bounds"
    );
    match (frame.sequence, &frame.previous_frame_digest) {
        (1, None) => {}
        (2.., Some(digest)) => hash(digest)?,
        _ => bail!("external frame predecessor is missing or unexpected"),
    }
    if matches!(frame.payload, ExecutionChannelPayload::Ready { .. }) {
        ensure!(
            frame.direction == ChannelDirection::SupervisorToOwner && frame.sequence == 1,
            "external readiness must be the first supervisor observation"
        );
    }
    if matches!(
        frame.payload,
        ExecutionChannelPayload::CapabilityReport { .. }
    ) {
        ensure!(
            frame.sequence > 1,
            "a capability report cannot precede the supervisor's readiness"
        );
    }
    if let ExecutionChannelPayload::Acknowledge {
        peer_frame_sequence,
        peer_frame_digest: _,
        application: _,
    } = &frame.payload
    {
        ensure!(
            *peer_frame_sequence > 0,
            "external application acknowledgement has no peer frame"
        );
    }
    frame.payload.validate(binding, frame.direction)?;
    let size = lillux::canonical_json(&serde_json::to_value(frame)?)?.len();
    ensure!(
        size + 256 <= MAX_FRAME_BYTES,
        "external frame exceeds wire bound"
    );
    Ok(())
}

/// The same accounting is used for new frames and historical replay validation.
/// Counts are separate from sequence, which spans ordinary, Q settlement and terminal
/// traffic and therefore preserves one authenticated chain per direction.
#[derive(Default)]
pub struct ExecutionChannelBudget {
    pub ordinary_frames: u64,
    pub ordinary_bytes: u64,
    pub q_finish_frames: u64,
    pub q_finish_bytes: u64,
    pub terminal_frames: u64,
    pub terminal_bytes: u64,
}

impl ExecutionChannelBudget {
    /// Only the journal may use this after joining an exact, first lifecycle
    /// receipt to its retained peer. Ordinary Acknowledge never opts itself in.
    pub(super) fn retain_checked_q_finish_ack(
        &mut self,
        binding: &ExecutionChannelBinding,
        bytes: u64,
    ) -> Result<()> {
        validate_channel_journal_limits(
            binding.execution_mode,
            binding.max_frames,
            binding.max_bytes,
        )?;
        ensure!(
            matches!(
                binding.execution_mode,
                ExternalExecutionMode::QualificationStructuredSession {}
            ),
            "Q finish receipt requires Q mode"
        );
        let frames = self
            .q_finish_frames
            .checked_add(1)
            .context("Q finish frame count overflow")?;
        let total = self
            .q_finish_bytes
            .checked_add(bytes)
            .context("Q finish byte count overflow")?;
        ensure!(
            frames <= u64::from(Q_FINISH_FRAMES) && total <= Q_FINISH_BYTES,
            "external transcript budget exhausted"
        );
        self.q_finish_frames = frames;
        self.q_finish_bytes = total;
        Ok(())
    }

    pub fn retain(
        &mut self,
        binding: &ExecutionChannelBinding,
        payload: &ExecutionChannelPayload,
        bytes: u64,
    ) -> Result<()> {
        validate_channel_journal_limits(
            binding.execution_mode,
            binding.max_frames,
            binding.max_bytes,
        )?;
        let q_mode = matches!(
            binding.execution_mode,
            ExternalExecutionMode::QualificationStructuredSession {}
        );
        let (frames, used, frame_limit, byte_limit) = if payload.uses_terminal_reserve() {
            (
                &mut self.terminal_frames,
                &mut self.terminal_bytes,
                1,
                TERMINAL_CONTROL_BYTES,
            )
        } else if matches!(
            payload,
            ExecutionChannelPayload::QualificationScopeSettled { .. }
        ) || (q_mode
            && matches!(
                payload,
                ExecutionChannelPayload::QualificationComplete { .. }
                    | ExecutionChannelPayload::ProtocolEof
            ))
        {
            ensure!(q_mode, "native Q settlement requires Q channel accounting");
            (
                &mut self.q_finish_frames,
                &mut self.q_finish_bytes,
                u64::from(Q_FINISH_FRAMES),
                Q_FINISH_BYTES,
            )
        } else {
            (
                &mut self.ordinary_frames,
                &mut self.ordinary_bytes,
                u64::from(binding.max_frames - if q_mode { Q_FINISH_FRAMES } else { 0 }),
                binding.max_bytes - if q_mode { Q_FINISH_BYTES } else { 0 },
            )
        };
        let next_frames = frames
            .checked_add(1)
            .context("external frame count overflow")?;
        let next_bytes = used
            .checked_add(bytes)
            .context("external byte count overflow")?;
        ensure!(
            next_frames <= frame_limit && next_bytes <= byte_limit,
            "external transcript budget exhausted"
        );
        *frames = next_frames;
        *used = next_bytes;
        Ok(())
    }
}

fn signing_bytes(frame: &ExecutionFrame) -> Result<Vec<u8>> {
    let mut bytes = SIGNATURE_DOMAIN.to_vec();
    bytes.extend_from_slice(lillux::canonical_json(&serde_json::to_value(frame)?)?.as_bytes());
    Ok(bytes)
}
fn key_for(binding: &ExecutionChannelBinding, direction: ChannelDirection) -> Result<VerifyingKey> {
    public_key(match direction {
        ChannelDirection::OwnerToSupervisor => &binding.owner_public_key,
        ChannelDirection::SupervisorToOwner => &binding.supervisor_public_key,
    })
}

/// Validate the canonical Ed25519 public-key spelling shared by bootstrap
/// reservations and finalized channel bindings without exposing a constructor
/// for authenticated frames.
pub fn validate_channel_public_key(value: &str) -> Result<()> {
    public_key(value).map(|_| ())
}

/// Canonical spelling for an already-generated channel role key.
pub fn encode_channel_public_key(value: &VerifyingKey) -> Result<String> {
    ensure!(!value.is_weak(), "weak external channel key");
    Ok(STANDARD.encode(value.to_bytes()))
}

fn public_key(value: &str) -> Result<VerifyingKey> {
    ensure!(value.len() == 44, "external channel key has wrong length");
    let bytes: [u8; 32] = STANDARD
        .decode(value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("external channel key has wrong size"))?;
    ensure!(
        STANDARD.encode(bytes) == value,
        "noncanonical external channel key"
    );
    let key = VerifyingKey::from_bytes(&bytes)?;
    ensure!(!key.is_weak(), "weak external channel key");
    Ok(key)
}
fn chunk(value: &str, allow_empty: bool) -> Result<Vec<u8>> {
    ensure!(
        value.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4,
        "external chunk exceeds bound"
    );
    let bytes = STANDARD.decode(value)?;
    ensure!(
        bytes.len() <= MAX_CHUNK_BYTES
            && (allow_empty || !bytes.is_empty())
            && STANDARD.encode(&bytes) == value,
        "empty, oversized or noncanonical external chunk"
    );
    Ok(bytes)
}
fn hash(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "noncanonical external execution digest"
    );
    Ok(())
}
/// Lowercase hyphenated UUID, the form Linux reports for a kernel boot id.
fn boot_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 36
            && value.bytes().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
            }),
        "supervisor boot id is malformed"
    );
    Ok(())
}
/// Printable ASCII, non-empty and bounded: capability names, grades,
/// provider ids and reasons are quoted into refusals.
fn capability_text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= maximum
            && value.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
        "capability report entry is empty, oversized or not printable"
    );
    Ok(())
}
fn text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= maximum
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "invalid external execution coordinate"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use lillux::process_control::ProofSource;

    use super::*;

    #[test]
    fn q_settlement_and_cleanup_survive_full_ordinary_budget() {
        let (mut binding, _, _) = binding();
        binding.execution_mode = ExternalExecutionMode::QualificationStructuredSession {};
        binding.candidate_export_max_bytes = 0;
        binding.max_frames = Q_FINISH_FRAMES + 1;
        binding.max_bytes = Q_FINISH_BYTES + 1000;
        let acknowledgement = ExecutionChannelPayload::Acknowledge {
            peer_frame_sequence: 1,
            peer_frame_digest: "a".repeat(64),
            application: ExecutionFrameApplication::Retained,
        };
        let settlement = ExecutionChannelPayload::QualificationScopeSettled {
            observation: NativeQualificationScopeSettlement {
                schema: 3,
                observation: NativeQualificationScopeSettlementObservation {
                    schema: 1,
                    binding_digest: binding.digest().unwrap(),
                    launcher_occurrence_digest: "1".repeat(64),
                    applied_runtime_receipt_digest: "2".repeat(64),
                    completion_frame_digest: "3".repeat(64),
                    completion_request_digest: "4".repeat(64),
                    mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
                    exit: NativeNamespaceExit::Code(0),
                },
                fixture_capture: None,
                fixture_content: None,
            },
        };
        let mut budget = ExecutionChannelBudget::default();
        budget.retain(&binding, &acknowledgement, 1000).unwrap();
        assert!(budget.retain(&binding, &acknowledgement, 1).is_err());
        assert!(
            budget
                .retain(&binding, &settlement, Q_FINISH_BYTES + 1)
                .is_err()
        );
        assert_eq!(budget.q_finish_frames, 0);
        budget
            .retain(&binding, &settlement, Q_FINISH_BYTES)
            .unwrap();
        assert!(budget.retain(&binding, &settlement, 1).is_err());
        assert_eq!(
            budget.ordinary_bytes + budget.q_finish_bytes,
            binding.max_bytes
        );
        assert_eq!(budget.ordinary_frames + budget.q_finish_frames, 2);
        assert!(
            budget
                .retain(
                    &binding,
                    &ExecutionChannelPayload::Cancel,
                    TERMINAL_CONTROL_BYTES + 1
                )
                .is_err()
        );
        budget
            .retain(
                &binding,
                &ExecutionChannelPayload::Cancel,
                TERMINAL_CONTROL_BYTES,
            )
            .unwrap();
        assert!(
            budget
                .retain(&binding, &ExecutionChannelPayload::Cancel, 1)
                .is_err()
        );
        binding.execution_mode = ExternalExecutionMode::StructuredSession {};
        assert!(
            ExecutionChannelBudget::default()
                .retain(&binding, &settlement, 1000)
                .is_err()
        );
    }

    #[test]
    fn q_reserve_requires_capacity_inside_current_signed_limits() {
        let (mut binding, _, _) = binding();
        binding.execution_mode = ExternalExecutionMode::QualificationStructuredSession {};
        binding.candidate_export_max_bytes = 0;
        binding.max_bytes = Q_FINISH_BYTES;
        assert!(binding.validate().is_err());
        binding.max_bytes += 1;
        binding.max_frames = Q_FINISH_FRAMES;
        assert!(binding.validate().is_err());
        binding.max_frames = Q_FINISH_FRAMES + 1;
        binding.validate().unwrap();
        binding.schema = 4;
        assert!(binding.validate().is_err());
    }

    #[test]
    fn terminal_control_survives_both_ordinary_budget_ceilings() {
        let (mut binding, _, _) = binding();
        binding.max_frames = 1;
        binding.max_bytes = 500;
        let mut budget = ExecutionChannelBudget::default();
        let acknowledgement = ExecutionChannelPayload::Acknowledge {
            peer_frame_sequence: 1,
            peer_frame_digest: "a".repeat(64),
            application: ExecutionFrameApplication::Retained,
        };
        budget.retain(&binding, &acknowledgement, 500).unwrap();
        assert!(budget.retain(&binding, &acknowledgement, 1).is_err());
        budget
            .retain(&binding, &ExecutionChannelPayload::Cancel, 600)
            .unwrap();
        assert!(
            budget
                .retain(&binding, &ExecutionChannelPayload::Cancel, 600)
                .is_err()
        );
        assert_eq!(budget.ordinary_bytes, 500);
        assert_eq!(budget.terminal_frames, 1);
        let mut peer = ExecutionChannelBudget::default();
        peer.retain(&binding, &acknowledgement, 500).unwrap();
        peer.retain(
            &binding,
            &ExecutionChannelPayload::Stopped {
                reason: ExternalStopReason::Deadline,
            },
            650,
        )
        .unwrap();
    }

    pub(super) fn binding() -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        (
            ExecutionChannelBinding {
                schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
                execution_mode: ExternalExecutionMode::StructuredSession {},
                placement_thread_id: "T-external".into(),
                occurrence_id: "occurrence".into(),
                allocation_request_digest: "a".repeat(64),
                admitted_capsule_hash: "b".repeat(64),
                base_snapshot_hash: "c".repeat(64),
                execution_binding_hash: "d".repeat(64),
                supervisor_runtime_hash: "e".repeat(64),
                candidate_program_digest: "0".repeat(64),
                channel_nonce: "f".repeat(64),
                owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
                supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
                issued_at_ms: 1,
                execution_deadline_ms: 100,
                expires_at_ms: 200,
                candidate_export_max_bytes: 512 * 1024,
                max_frames: 100,
                max_bytes: 1024 * 1024,
            },
            owner,
            supervisor,
        )
    }
    #[test]
    fn q_settlement_wrapper_requires_current_schema_and_explicit_capture_presence() {
        let (mut binding, _, _) = binding();
        binding.execution_mode = ExternalExecutionMode::QualificationStructuredSession {};
        binding.candidate_export_max_bytes = 0;
        let native = NativeQualificationScopeSettlementObservation {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            launcher_occurrence_digest: "1".repeat(64),
            applied_runtime_receipt_digest: "2".repeat(64),
            completion_frame_digest: "3".repeat(64),
            completion_request_digest: "4".repeat(64),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Signal(9),
        };
        let settlement = NativeQualificationScopeSettlement {
            schema: 3,
            observation: native.clone(),
            fixture_capture: None,
            fixture_content: None,
        };
        settlement.validate(&binding).unwrap();
        let wire = serde_json::to_value(&settlement).unwrap();
        assert!(wire["fixture_capture"].is_null());
        assert!(wire["fixture_content"].is_null());
        for field in ["fixture_capture", "fixture_content"] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<NativeQualificationScopeSettlement>(missing).is_err());
        }
        assert!(
            serde_json::from_value::<NativeQualificationScopeSettlement>(
                serde_json::to_value(&native).unwrap()
            )
            .is_err()
        );
        let mut old = settlement.clone();
        old.schema = 2;
        assert!(old.validate(&binding).is_err());
        let mut changed = settlement;
        changed.observation.binding_digest = "f".repeat(64);
        assert!(changed.validate(&binding).is_err());
        let mut cyclic = wire;
        cyclic["observation"]["fixture_capture"] = serde_json::json!(null);
        assert!(serde_json::from_value::<NativeQualificationScopeSettlement>(cyclic).is_err());
    }

    #[test]
    fn q_settlement_content_is_exact_and_maximum_fits_reserved_frame() {
        use crate::objects::qualification_fixture::{
            MAX_Q_FIXTURE_BYTES, Q_FIXTURE_CAPTURE_KIND, Q_FIXTURE_CAPTURE_SCHEMA,
            QFixtureCaptureContent, QFixtureCaptureMetadata, QFixtureCaptureReference,
        };
        let (mut binding, _, supervisor) = binding();
        binding.execution_mode = ExternalExecutionMode::QualificationStructuredSession {};
        binding.candidate_export_max_bytes = 0;
        let native = NativeQualificationScopeSettlementObservation {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            launcher_occurrence_digest: "1".repeat(64),
            applied_runtime_receipt_digest: "2".repeat(64),
            completion_frame_digest: "3".repeat(64),
            completion_request_digest: "4".repeat(64),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Signal(9),
        };
        let bytes = vec![0xab; MAX_Q_FIXTURE_BYTES as usize];
        let metadata = QFixtureCaptureMetadata {
            kind: Q_FIXTURE_CAPTURE_KIND.into(),
            schema: Q_FIXTURE_CAPTURE_SCHEMA,
            fixture_contract_digest: "5".repeat(64),
            native_settlement: native.clone(),
            blob_hash: lillux::sha256_hex(&bytes),
            size_bytes: bytes.len() as u64,
            normalized_mode: crate::objects::ProjectFile::REGULAR_MODE,
        };
        let reference = QFixtureCaptureReference {
            metadata_hash: metadata.digest().unwrap(),
            blob_hash: metadata.blob_hash.clone(),
            size_bytes: metadata.size_bytes,
        };
        let content = QFixtureCaptureContent::from_bytes(metadata, &bytes).unwrap();
        let settlement = NativeQualificationScopeSettlement {
            schema: 3,
            observation: native,
            fixture_capture: Some(reference),
            fixture_content: Some(content),
        };
        settlement.validate(&binding).unwrap();
        let mut partial = settlement.clone();
        partial.fixture_content = None;
        assert!(partial.validate(&binding).is_err());
        partial = settlement.clone();
        partial.fixture_capture = None;
        assert!(partial.validate(&binding).is_err());
        let mut changed = settlement.clone();
        changed
            .fixture_content
            .as_mut()
            .unwrap()
            .metadata
            .native_settlement
            .completion_frame_digest = "9".repeat(64);
        changed.fixture_capture.as_mut().unwrap().metadata_hash = changed
            .fixture_content
            .as_ref()
            .unwrap()
            .metadata
            .digest()
            .unwrap();
        assert!(changed.validate(&binding).is_err());
        changed = settlement.clone();
        changed.fixture_content.as_mut().unwrap().bytes_base64 =
            STANDARD.encode(vec![0xbb; bytes.len()]);
        assert!(changed.validate(&binding).is_err());
        let frame = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction: ChannelDirection::SupervisorToOwner,
                sequence: 2,
                previous_frame_digest: Some("6".repeat(64)),
                acknowledged_peer_sequence: 1,
                payload: ExecutionChannelPayload::QualificationScopeSettled {
                    observation: settlement,
                },
            },
            &binding,
            &supervisor,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(frame).unwrap()).unwrap();
        assert!(
            wire.len() as u64 <= Q_FINISH_BYTES,
            "maximum Q content exceeds finite settlement reservation"
        );
    }

    #[test]
    fn channel_schema_is_independent_from_enclosing_external_documents() {
        let (binding, _, _) = binding();
        binding.validate().unwrap();

        let mut changed = binding;
        changed.schema = 4;
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unsupported external channel schema")
        );
    }
    fn frame(binding: &ExecutionChannelBinding) -> ExecutionFrame {
        ExecutionFrame {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            direction: ChannelDirection::OwnerToSupervisor,
            sequence: 1,
            previous_frame_digest: None,
            acknowledged_peer_sequence: 0,
            payload: ExecutionChannelPayload::Release,
        }
    }

    fn direct_binding() -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        let (mut binding, owner, supervisor) = binding();
        binding.execution_mode = ExternalExecutionMode::DirectCommand {
            stdout_max_bytes: 4,
            stderr_max_bytes: 2,
        };
        binding.candidate_export_max_bytes = 0;
        (binding, owner, supervisor)
    }

    fn terminated() -> ExecutionChannelPayload {
        let empty = ExternalCommandOutputCommitment {
            bytes: 0,
            sha256: lillux::sha256_hex(b""),
            truncated: false,
        };
        ExecutionChannelPayload::CommandTerminated {
            observation: ExternalCommandTermination {
                target_exit: ExternalTargetExit::Code(0),
                reason: ExternalCommandTerminationReason::TargetExited,
                stdout: empty.clone(),
                stderr: empty,
            },
        }
    }

    #[test]
    fn channel_execution_mode_is_required_and_committed() {
        let (binding, _, _) = binding();
        let mut changed = binding.clone();
        changed.execution_mode = direct_binding().0.execution_mode;
        changed.candidate_export_max_bytes = 0;
        assert_ne!(binding.digest().unwrap(), changed.digest().unwrap());
        let mut value = serde_json::to_value(&binding).unwrap();
        value.as_object_mut().unwrap().remove("execution_mode");
        assert!(serde_json::from_value::<ExecutionChannelBinding>(value).is_err());
        changed.schema = 3;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn channel_export_authority_matches_only_session_mode() {
        let (mut session, _, _) = binding();
        session.candidate_export_max_bytes = 0;
        assert!(session.validate().is_err());
        let (mut direct, _, _) = direct_binding();
        direct.validate().unwrap();
        direct.candidate_export_max_bytes = 1;
        assert!(direct.validate().is_err());
    }

    #[test]
    fn qualification_session_carries_protocol_but_no_export() {
        let (mut binding, owner, supervisor) = binding();
        let ordinary_digest = binding.digest().unwrap();
        binding.execution_mode = ExternalExecutionMode::QualificationStructuredSession {};
        binding.candidate_export_max_bytes = 0;
        binding.validate().unwrap();
        let mut wrong_export = binding.clone();
        wrong_export.candidate_export_max_bytes = 1;
        assert!(wrong_export.validate().is_err());
        assert_ne!(binding.digest().unwrap(), ordinary_digest);

        for (direction, payload) in [
            (
                ChannelDirection::OwnerToSupervisor,
                ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(b"input"),
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(b"output"),
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ProtocolEof,
            ),
        ] {
            let mut value = frame(&binding);
            value.direction = direction;
            value.payload = payload;
            let signer = match direction {
                ChannelDirection::OwnerToSupervisor => &owner,
                ChannelDirection::SupervisorToOwner => &supervisor,
            };
            SignedExecutionFrame::sign(value, &binding, signer).unwrap();
        }

        for (direction, payload) in [
            (
                ChannelDirection::OwnerToSupervisor,
                ExecutionChannelPayload::Quiesce {
                    completion_request_digest: "a".repeat(64),
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ExportObjectChunk {
                    content_kind: ExportContentKind::Blob,
                    object_hash: "a".repeat(64),
                    offset: 0,
                    bytes_base64: STANDARD.encode(b"x"),
                    final_chunk: true,
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ExportSealed {
                    candidate_snapshot_hash: "a".repeat(64),
                    candidate_output_capture_hash: None,
                    completion_request_digest: "b".repeat(64),
                    writer_exclusion_evidence_hash: "c".repeat(64),
                },
            ),
            (ChannelDirection::SupervisorToOwner, terminated()),
        ] {
            let mut value = frame(&binding);
            value.direction = direction;
            value.payload = payload;
            let signer = match direction {
                ChannelDirection::OwnerToSupervisor => &owner,
                ChannelDirection::SupervisorToOwner => &supervisor,
            };
            assert!(SignedExecutionFrame::sign(value, &binding, signer).is_err());
        }
    }

    #[test]
    fn direct_command_observations_require_supervisor_signature_and_exact_mode() {
        let (binding, owner, supervisor) = direct_binding();
        let mut value = frame(&binding);
        value.payload = terminated();
        assert!(SignedExecutionFrame::sign(value.clone(), &binding, &owner).is_err());
        value.direction = ChannelDirection::SupervisorToOwner;
        assert!(SignedExecutionFrame::sign(value.clone(), &binding, &owner).is_err());
        let signed = SignedExecutionFrame::sign(value, &binding, &supervisor).unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(signed).unwrap()).unwrap();
        SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &binding, 50).unwrap();
        let mut changed = binding.clone();
        changed.execution_mode = ExternalExecutionMode::StructuredSession {};
        changed.candidate_export_max_bytes = 1;
        assert!(SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &changed, 50).is_err());
        let mut value = frame(&changed);
        value.direction = ChannelDirection::SupervisorToOwner;
        value.payload = terminated();
        assert!(SignedExecutionFrame::sign(value, &changed, &supervisor).is_err());
        assert!(!terminated().uses_terminal_reserve());
    }

    #[test]
    fn direct_command_channel_refuses_session_and_export_payloads() {
        let (binding, owner, supervisor) = direct_binding();
        for (direction, payload) in [
            (
                ChannelDirection::OwnerToSupervisor,
                ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(b"input"),
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(b"output"),
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ProtocolEof,
            ),
            (
                ChannelDirection::OwnerToSupervisor,
                ExecutionChannelPayload::Quiesce {
                    completion_request_digest: "a".repeat(64),
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ExportObjectChunk {
                    content_kind: ExportContentKind::Blob,
                    object_hash: "a".repeat(64),
                    offset: 0,
                    bytes_base64: STANDARD.encode(b"x"),
                    final_chunk: true,
                },
            ),
            (
                ChannelDirection::SupervisorToOwner,
                ExecutionChannelPayload::ExportSealed {
                    candidate_snapshot_hash: "a".repeat(64),
                    candidate_output_capture_hash: None,
                    completion_request_digest: "b".repeat(64),
                    writer_exclusion_evidence_hash: "c".repeat(64),
                },
            ),
        ] {
            let mut value = frame(&binding);
            value.direction = direction;
            value.payload = payload;
            let signer = match direction {
                ChannelDirection::OwnerToSupervisor => &owner,
                ChannelDirection::SupervisorToOwner => &supervisor,
            };
            assert!(SignedExecutionFrame::sign(value, &binding, signer).is_err());
        }
    }

    #[test]
    fn direct_command_chunks_validate_direction_stream_bounds_and_overflow() {
        let (binding, owner, supervisor) = direct_binding();
        for (stream, offset, bytes, accepted) in [
            (
                ExternalCommandOutputStream::Stdout,
                0,
                b"four".as_slice(),
                true,
            ),
            (
                ExternalCommandOutputStream::Stderr,
                0,
                b"four".as_slice(),
                false,
            ),
            (
                ExternalCommandOutputStream::Stdout,
                4,
                b"x".as_slice(),
                false,
            ),
            (
                ExternalCommandOutputStream::Stdout,
                u64::MAX,
                b"x".as_slice(),
                false,
            ),
            (
                ExternalCommandOutputStream::Stdout,
                0,
                b"".as_slice(),
                false,
            ),
        ] {
            let mut value = frame(&binding);
            value.payload = ExecutionChannelPayload::CommandOutput {
                stream,
                offset,
                bytes_base64: STANDARD.encode(bytes),
            };
            assert!(SignedExecutionFrame::sign(value.clone(), &binding, &owner).is_err());
            value.direction = ChannelDirection::SupervisorToOwner;
            assert_eq!(
                SignedExecutionFrame::sign(value, &binding, &supervisor).is_ok(),
                accepted
            );
        }
    }
    #[test]
    fn external_frames_bind_exact_roles_occurrence_and_lifetime() {
        let (binding, owner, supervisor) = binding();
        let frame = frame(&binding);
        assert!(SignedExecutionFrame::sign(frame.clone(), &binding, &supervisor).is_err());
        let signed = SignedExecutionFrame::sign(frame, &binding, &owner).unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(&signed).unwrap()).unwrap();
        let verified =
            SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &binding, 50).unwrap();
        assert_eq!(verified.frame().sequence, 1);
        assert_eq!(verified.digest(), lillux::sha256_hex(wire.as_bytes()));
        assert!(SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &binding, 200).is_err());
        let mut changed = binding.clone();
        changed.occurrence_id = "replacement".into();
        assert!(SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &changed, 50).is_err());
        let mut changed = signed;
        changed.frame.payload = ExecutionChannelPayload::Cancel;
        let changed = lillux::canonical_json(&serde_json::to_value(changed).unwrap()).unwrap();
        assert!(SignedExecutionFrame::decode_and_verify(changed.as_bytes(), &binding, 50).is_err());
    }
    fn supervisor_frame(
        binding: &ExecutionChannelBinding,
        sequence: u64,
        payload: ExecutionChannelPayload,
    ) -> ExecutionFrame {
        ExecutionFrame {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            direction: ChannelDirection::SupervisorToOwner,
            sequence,
            previous_frame_digest: (sequence > 1).then(|| "9".repeat(64)),
            acknowledged_peer_sequence: 0,
            payload,
        }
    }

    fn ready_with(
        binding: &ExecutionChannelBinding,
        boot: Option<SupervisorReadyBoot>,
    ) -> ExecutionChannelPayload {
        ExecutionChannelPayload::Ready {
            supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
            base_snapshot_hash: binding.base_snapshot_hash.clone(),
            boot,
        }
    }

    fn ready_boot() -> SupervisorReadyBoot {
        SupervisorReadyBoot {
            boot_id: "997aad14-f3e6-4990-9d40-d1068e5a38d5".into(),
            verified_input_digests: vec!["1".repeat(64), "2".repeat(64)],
            ready_at_ms: 10,
        }
    }

    #[test]
    fn ready_boot_is_optional_on_the_wire_and_validated_when_present() {
        let (binding, _, supervisor) = binding();
        let without = SignedExecutionFrame::sign(
            supervisor_frame(&binding, 1, ready_with(&binding, None)),
            &binding,
            &supervisor,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(&without).unwrap()).unwrap();
        assert!(
            !wire.contains("\"boot\""),
            "an absent boot leaves the wire unchanged"
        );
        SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &binding, 50).unwrap();
        let mut explicit_null = serde_json::to_value(&without).unwrap();
        explicit_null["frame"]["payload"]["boot"] = serde_json::Value::Null;
        assert!(
            SignedExecutionFrame::decode_and_verify(
                lillux::canonical_json(&explicit_null).unwrap().as_bytes(),
                &binding,
                50
            )
            .is_err(),
            "an explicit null boot is noncanonical"
        );

        let with = SignedExecutionFrame::sign(
            supervisor_frame(&binding, 1, ready_with(&binding, Some(ready_boot()))),
            &binding,
            &supervisor,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(&with).unwrap()).unwrap();
        let verified =
            SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &binding, 50).unwrap();
        assert!(matches!(
            &verified.frame().payload,
            ExecutionChannelPayload::Ready { boot: Some(boot), .. } if boot == &ready_boot()
        ));

        let mut malformed = ready_boot();
        malformed.boot_id = "997AAD14-F3E6-4990-9D40-D1068E5A38D5".into();
        let mut empty_inputs = ready_boot();
        empty_inputs.verified_input_digests.clear();
        let mut too_many_inputs = ready_boot();
        too_many_inputs.verified_input_digests =
            vec!["1".repeat(64); MAX_READY_VERIFIED_INPUTS + 1];
        let mut bad_digest = ready_boot();
        bad_digest.verified_input_digests = vec!["X".repeat(64)];
        let mut before_issue = ready_boot();
        before_issue.ready_at_ms = 0;
        let mut after_expiry = ready_boot();
        after_expiry.ready_at_ms = binding.expires_at_ms;
        for boot in [
            malformed,
            empty_inputs,
            too_many_inputs,
            bad_digest,
            before_issue,
            after_expiry,
        ] {
            assert!(
                SignedExecutionFrame::sign(
                    supervisor_frame(&binding, 1, ready_with(&binding, Some(boot))),
                    &binding,
                    &supervisor,
                )
                .is_err()
            );
        }
    }

    fn capability_report(
        provided: &[(&str, ProofSource)],
        missing: &[(&str, &str)],
    ) -> ExecutionChannelPayload {
        ExecutionChannelPayload::CapabilityReport {
            boot_id: "997aad14-f3e6-4990-9d40-d1068e5a38d5".into(),
            provided: provided
                .iter()
                .map(|(name, source)| {
                    (
                        (*name).to_owned(),
                        CapabilityEvidence {
                            source: source.clone(),
                            provider: "host".into(),
                            basis: "probe".into(),
                        },
                    )
                })
                .collect(),
            missing: missing
                .iter()
                .map(|(name, reason)| ((*name).to_owned(), (*reason).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn capability_report_is_a_bounded_supervisor_observation_after_ready() {
        let (binding, owner, supervisor) = binding();
        let report = capability_report(
            &[
                ("isolation:namespaces", ProofSource::Measured),
                ("network:egress_denied", ProofSource::ProviderAttested),
            ],
            &[("persistence:durable_state", "no persistent disk")],
        );
        let signed = SignedExecutionFrame::sign(
            supervisor_frame(&binding, 2, report.clone()),
            &binding,
            &supervisor,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(&signed).unwrap()).unwrap();
        let verified =
            SignedExecutionFrame::decode_and_verify(wire.as_bytes(), &binding, 50).unwrap();
        assert_eq!(verified.frame().payload, report);

        assert!(
            SignedExecutionFrame::sign(
                supervisor_frame(&binding, 1, report.clone()),
                &binding,
                &supervisor
            )
            .is_err(),
            "a report cannot take readiness's place"
        );
        let mut from_owner = supervisor_frame(&binding, 2, report);
        from_owner.direction = ChannelDirection::OwnerToSupervisor;
        assert!(SignedExecutionFrame::sign(from_owner, &binding, &owner).is_err());

        let contradictory = capability_report(
            &[("isolation:namespaces", ProofSource::Measured)],
            &[("isolation:namespaces", "no")],
        );
        let control_character = capability_report(&[], &[("isolation:namespaces", "bad\u{7}")]);
        let names: Vec<String> = (0..=MAX_CAPABILITY_REPORT_ENTRIES)
            .map(|index| format!("name:{index}"))
            .collect();
        let oversized = capability_report(
            &[],
            &names
                .iter()
                .map(|name| (name.as_str(), "absent"))
                .collect::<Vec<_>>(),
        );
        let mut bad_boot = capability_report(&[], &[]);
        if let ExecutionChannelPayload::CapabilityReport { boot_id, .. } = &mut bad_boot {
            *boot_id = "not-a-boot".into();
        }
        for payload in [contradictory, control_character, oversized, bad_boot] {
            assert!(
                SignedExecutionFrame::sign(
                    supervisor_frame(&binding, 2, payload),
                    &binding,
                    &supervisor
                )
                .is_err()
            );
        }
    }

    #[test]
    fn external_frames_reject_unknown_fields_wrong_directions_and_bounds() {
        let (binding, owner, supervisor) = binding();
        let mut value = frame(&binding);
        value.payload = ExecutionChannelPayload::Stopped {
            reason: ExternalStopReason::Fault,
        };
        assert!(SignedExecutionFrame::sign(value, &binding, &owner).is_err());
        let mut value = frame(&binding);
        value.payload = ExecutionChannelPayload::ProtocolEof;
        assert!(SignedExecutionFrame::sign(value, &binding, &owner).is_err());
        let mut value = frame(&binding);
        value.direction = ChannelDirection::SupervisorToOwner;
        value.payload = ExecutionChannelPayload::ProtocolEof;
        assert!(SignedExecutionFrame::sign(value, &binding, &supervisor).is_ok());
        let signed = SignedExecutionFrame::sign(frame(&binding), &binding, &owner).unwrap();
        let mut wire = serde_json::to_value(&signed).unwrap();
        wire["extra"] = true.into();
        assert!(
            SignedExecutionFrame::decode_and_verify(
                lillux::canonical_json(&wire).unwrap().as_bytes(),
                &binding,
                50
            )
            .is_err()
        );
        let mut bad = frame(&binding);
        bad.sequence = 2;
        assert!(SignedExecutionFrame::sign(bad, &binding, &owner).is_err());
        let mut bad = binding;
        bad.max_bytes = u64::MAX;
        assert!(bad.validate().is_err());
    }
}
