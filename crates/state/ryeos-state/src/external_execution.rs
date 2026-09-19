//! Exact, occurrence-scoped external execution channel records.
//!
//! Authentication is not admission, native isolation qualification, candidate
//! validation or publication. In particular a signed supervisor observation is
//! not a Lillux proof. The owning runtime retains those independent checks.
//! These keys never enter the node's enrolled-principal namespace.

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

pub mod export;

pub const MAX_FRAME_BYTES: usize = 384 * 1024;
pub const MAX_CHUNK_BYTES: usize = 256 * 1024;
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
    pub placement_thread_id: String,
    pub allocation_request_digest: String,
    pub occurrence_id: String,
    pub admitted_capsule_hash: String,
    pub base_snapshot_hash: String,
    pub execution_binding_hash: String,
    pub supervisor_runtime_hash: String,
    /// Fresh random 256-bit identity; never reused after replacement.
    pub channel_nonce: String,
    /// Canonical base64 Ed25519 keys, not profile or cloud-account credentials.
    pub owner_public_key: String,
    pub supervisor_public_key: String,
    pub issued_at_ms: i64,
    pub execution_deadline_ms: i64,
    pub expires_at_ms: i64,
    /// Per-direction ordinary journal bounds including acknowledged frames.
    /// One bounded Cancel/Stopped frame is reserved independently.
    pub max_frames: u32,
    pub max_bytes: u64,
}

impl ExecutionChannelBinding {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported external channel schema");
        text(&self.placement_thread_id, 256)?;
        text(&self.occurrence_id, 512)?;
        for digest in [
            &self.allocation_request_digest,
            &self.admitted_capsule_hash,
            &self.base_snapshot_hash,
            &self.execution_binding_hash,
            &self.supervisor_runtime_hash,
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
    Release,
    ProtocolBytes {
        bytes_base64: String,
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
        completion_request_digest: String,
        writer_exclusion_evidence_hash: String,
    },
    Cancel,
    Stopped {
        reason: ExternalStopReason,
    },
    Acknowledge,
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
            Ready { .. } | ExportObjectChunk { .. } | ExportSealed { .. } | Stopped { .. }
        );
        ensure!(
            !owner_only || direction == ChannelDirection::OwnerToSupervisor,
            "supervisor cannot author an owner command"
        );
        ensure!(
            !supervisor_only || direction == ChannelDirection::SupervisorToOwner,
            "owner cannot author supervisor observations"
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
            ProtocolBytes { bytes_base64 } => {
                chunk(bytes_base64, false)?;
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
                completion_request_digest,
                writer_exclusion_evidence_hash,
            } => {
                hash(candidate_snapshot_hash)?;
                hash(completion_request_digest)?;
                hash(writer_exclusion_evidence_hash)?;
            }
            Release | Cancel | Stopped { .. } | Acknowledge => {}
        }
        Ok(())
    }
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
        budget
            .retain(&binding, &ExecutionChannelPayload::Acknowledge, 500)
            .unwrap();
        assert!(
            budget
                .retain(&binding, &ExecutionChannelPayload::Acknowledge, 1)
                .is_err()
        );
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
        peer.retain(&binding, &ExecutionChannelPayload::Acknowledge, 500)
            .unwrap();
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
                schema: 1,
                placement_thread_id: "T-external".into(),
                occurrence_id: "occurrence".into(),
                allocation_request_digest: "a".repeat(64),
                admitted_capsule_hash: "b".repeat(64),
                base_snapshot_hash: "c".repeat(64),
                execution_binding_hash: "d".repeat(64),
                supervisor_runtime_hash: "e".repeat(64),
                channel_nonce: "f".repeat(64),
                owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
                supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
                issued_at_ms: 1,
                execution_deadline_ms: 100,
                expires_at_ms: 200,
                max_frames: 100,
                max_bytes: 1024 * 1024,
            },
            owner,
            supervisor,
        )
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
        let (binding, owner, _) = binding();
        let mut value = frame(&binding);
        value.payload = ExecutionChannelPayload::Stopped {
            reason: ExternalStopReason::Fault,
        };
        assert!(SignedExecutionFrame::sign(value, &binding, &owner).is_err());
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
