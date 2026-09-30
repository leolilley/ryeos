//! Exact, occurrence-scoped external execution channel records.
//!
//! Authentication is not admission, native isolation qualification, candidate
//! validation or publication. In particular a signed supervisor observation is
//! not a Lillux proof. The owning runtime retains those independent checks.
//! These keys never enter the node's enrolled-principal namespace.

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::{Signature, Signer as _, SigningKey, VerifyingKey};
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
pub const EXECUTION_CHANNEL_BINDING_SCHEMA: u32 = 4;
/// First-generation logical base/candidate ceiling shared by native capture
/// and receiver verification; transport budgets may independently be smaller.
pub const MAX_CANDIDATE_CONTENT_BYTES: u64 = 1024 * 1024 * 1024;
/// One terminal control frame per direction has capacity independent of data.
/// Acknowledgements cannot consume this reserve. Lifecycle validation also
/// refuses repeated Cancel/Stopped, so it is not an unbounded emergency lane.
pub const TERMINAL_CONTROL_BYTES: u64 = 4096;
const SIGNATURE_DOMAIN: &[u8] = b"ryeos.external-execution.frame.v1\0";

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
    /// frames are included. Direct commands require zero: their immutable input
    /// view has no candidate export authority.
    pub candidate_export_max_bytes: u64,
    /// Per-direction ordinary journal bounds including acknowledged frames.
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
        ensure!(
            (1..=65_536).contains(&self.max_frames)
                && (1..=64 * 1024 * 1024).contains(&self.max_bytes),
            "external channel journal exceeds bounds"
        );
        ensure!(
            match self.execution_mode {
                ExternalExecutionMode::StructuredSession {} =>
                    (1..=self.max_bytes).contains(&self.candidate_export_max_bytes),
                ExternalExecutionMode::DirectCommand { .. } => self.candidate_export_max_bytes == 0,
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

/// Tool protocol bytes cannot manufacture a control/export observation. The
/// protected supervisor, not the candidate endpoint, signs control frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionChannelPayload {
    Ready {
        supervisor_runtime_hash: String,
        base_snapshot_hash: String,
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
        let owner_only = matches!(self, Release | Quiesce { .. } | Cancel);
        let supervisor_only = matches!(
            self,
            Ready { .. }
                | RuntimeApplied { .. }
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
            !matches!(
                self,
                ProtocolBytes { .. }
                    | ProtocolEof
                    | Quiesce { .. }
                    | ExportObjectChunk { .. }
                    | ExportSealed { .. }
            ) || matches!(
                binding.execution_mode,
                ExternalExecutionMode::StructuredSession {}
            ),
            "direct command channel cannot carry session protocol or candidate export"
        );
        match self {
            Ready {
                supervisor_runtime_hash,
                base_snapshot_hash,
            } => {
                ensure!(
                    supervisor_runtime_hash == &binding.supervisor_runtime_hash
                        && base_snapshot_hash == &binding.base_snapshot_hash,
                    "supervisor readiness changed admitted inputs"
                );
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
/// Counts are separate from sequence, which spans both ordinary and terminal
/// traffic and therefore preserves one authenticated chain per direction.
#[derive(Default)]
pub struct ExecutionChannelBudget {
    pub ordinary_frames: u64,
    pub ordinary_bytes: u64,
    pub terminal_frames: u64,
    pub terminal_bytes: u64,
}

impl ExecutionChannelBudget {
    pub fn retain(
        &mut self,
        binding: &ExecutionChannelBinding,
        payload: &ExecutionChannelPayload,
        bytes: u64,
    ) -> Result<()> {
        let (frames, used, frame_limit, byte_limit) = if payload.uses_terminal_reserve() {
            (
                &mut self.terminal_frames,
                &mut self.terminal_bytes,
                1,
                TERMINAL_CONTROL_BYTES,
            )
        } else {
            (
                &mut self.ordinary_frames,
                &mut self.ordinary_bytes,
                u64::from(binding.max_frames),
                binding.max_bytes,
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
    use super::*;

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
    fn channel_schema_is_independent_from_enclosing_external_documents() {
        let (binding, _, _) = binding();
        binding.validate().unwrap();

        let mut changed = binding;
        changed.schema = 5;
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
