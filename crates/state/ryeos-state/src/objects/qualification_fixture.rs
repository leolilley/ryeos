//! Finite Q fixture capture testimony and immutable content identity.
//!
//! These types grant no mount, write, export, or qualification authority. A
//! native owner must capture the whole allowed directory after once-reaped
//! namespace settlement under its original descriptor custody. The receiver
//! must independently authenticate that owner and the expected predecessor.
//!
//! The native observation embedded here MUST remain capture-free. The eventual
//! signed settlement wrapper may reference this metadata; this metadata cannot
//! reference that wrapper. Its CAS closure contains exactly this object and its
//! blob. The derived ProjectFile commitment is not a third stored-object link.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ProjectFile, thread_snapshot::validate_canonical_hash};
use crate::external_execution::{
    ExecutionChannelBinding, NativeNamespaceExit, NativeQualificationScopeSettlementObservation,
    NativeWriterExclusionMechanism,
};
pub use ryeos_external_execution_contract::qualification_fixture::{
    MAX_Q_FIXTURE_BYTES, Q_FIXTURE_CONTRACT_SCHEMA, Q_FIXTURE_MOUNT_DESTINATION, QFixtureContract,
};

pub const Q_FIXTURE_CAPTURE_KIND: &str = "qualification_fixture_capture";
pub const Q_FIXTURE_CAPTURE_SCHEMA: u32 = 1;

/// Use the kernel CAS owner's canonical JSON bytes, never a typed serializer's
/// member order, for signed contract commitments and metadata object identities.
pub fn q_fixture_contract_digest(contract: &QFixtureContract) -> Result<String> {
    contract.validate()?;
    canonical_digest(&serde_json::to_value(contract)?)
}

fn canonical_digest(value: &Value) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(value)?.as_bytes(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QFixtureCaptureMetadata {
    pub kind: String,
    pub schema: u32,
    pub fixture_contract_digest: String,
    /// Original capture-free native predecessor, joined by the guest journal.
    /// It must never acquire a reference to this capture or its final wrapper.
    pub native_settlement: NativeQualificationScopeSettlementObservation,
    pub blob_hash: String,
    pub size_bytes: u64,
    pub normalized_mode: u32,
}

impl QFixtureCaptureMetadata {
    /// Structural validity only. This does not authenticate native testimony,
    /// whole-directory shape, link count, descriptor custody, or write ordering.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == Q_FIXTURE_CAPTURE_KIND && self.schema == Q_FIXTURE_CAPTURE_SCHEMA,
            "unsupported Q fixture capture object"
        );
        for (label, digest) in [
            ("fixture contract", &self.fixture_contract_digest),
            ("fixture blob", &self.blob_hash),
            (
                "fixture native binding",
                &self.native_settlement.binding_digest,
            ),
            (
                "fixture native launcher",
                &self.native_settlement.launcher_occurrence_digest,
            ),
            (
                "fixture native runtime",
                &self.native_settlement.applied_runtime_receipt_digest,
            ),
            (
                "fixture completion frame",
                &self.native_settlement.completion_frame_digest,
            ),
            (
                "fixture completion request",
                &self.native_settlement.completion_request_digest,
            ),
        ] {
            validate_canonical_hash(label, digest)?;
        }
        ensure!(
            self.size_bytes <= MAX_Q_FIXTURE_BYTES
                && self.normalized_mode == ProjectFile::REGULAR_MODE,
            "Q fixture capture changed its finite regular-file bound"
        );
        ensure!(
            self.native_settlement.schema == 1
                && self.native_settlement.mechanism
                    == NativeWriterExclusionMechanism::NamespaceInitReaped
                && match self.native_settlement.exit {
                    NativeNamespaceExit::Code(n) => (0..=255).contains(&n),
                    NativeNamespaceExit::Signal(n) => (1..=64).contains(&n),
                },
            "Q fixture capture has invalid native predecessor testimony"
        );
        Ok(())
    }

    pub fn validate_for(
        &self,
        contract: &QFixtureContract,
        binding: &ExecutionChannelBinding,
        expected_native: &NativeQualificationScopeSettlementObservation,
    ) -> Result<()> {
        self.validate()?;
        binding.validate()?;
        expected_native.validate(binding)?;
        ensure!(
            self.native_settlement == *expected_native,
            "Q fixture capture changed its original native completion"
        );
        ensure!(
            self.fixture_contract_digest == q_fixture_contract_digest(contract)?
                && self.size_bytes <= contract.max_bytes,
            "Q fixture capture changed its signed fixture contract or limit"
        );
        Ok(())
    }

    pub fn to_value(&self) -> Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_value(value: &Value) -> Result<Self> {
        let metadata: Self = serde_json::from_value(value.clone())?;
        metadata.validate()?;
        Ok(metadata)
    }

    pub fn digest(&self) -> Result<String> {
        canonical_digest(&self.to_value()?)
    }

    /// Derived regular-file identity; callers do not need to store this as an
    /// extra CAS object. This method alone proves no whole-directory capture.
    pub fn project_file(&self) -> Result<ProjectFile> {
        self.validate()?;
        Ok(ProjectFile {
            blob_hash: self.blob_hash.clone(),
            size: self.size_bytes,
            normalized_mode: self.normalized_mode,
        })
    }

    pub fn project_file_digest(&self) -> Result<String> {
        canonical_digest(&self.project_file()?.to_value())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QFixtureCaptureReference {
    pub metadata_hash: String,
    pub blob_hash: String,
    pub size_bytes: u64,
}

impl QFixtureCaptureReference {
    pub fn validate(&self) -> Result<()> {
        validate_canonical_hash("Q fixture metadata", &self.metadata_hash)?;
        validate_canonical_hash("Q fixture blob", &self.blob_hash)?;
        ensure!(
            self.size_bytes <= MAX_Q_FIXTURE_BYTES,
            "Q fixture reference exceeds its ceiling"
        );
        Ok(())
    }

    /// Verify already bounded/read bytes against exact retained authority. The
    /// caller must cap its filesystem/transport read BEFORE allocating `bytes`.
    /// Positive qualification requires native/provider/enclosing settlement and
    /// independent product semantics beyond this immutable-content comparison.
    pub fn verify_for(
        &self,
        metadata: &QFixtureCaptureMetadata,
        contract: &QFixtureContract,
        binding: &ExecutionChannelBinding,
        expected_native: &NativeQualificationScopeSettlementObservation,
        bytes: &[u8],
    ) -> Result<()> {
        self.validate()?;
        ensure!(
            bytes.len() as u64 == self.size_bytes,
            "Q fixture bytes changed their bounded size"
        );
        metadata.validate_for(contract, binding, expected_native)?;
        ensure!(
            self.metadata_hash == metadata.digest()?
                && self.blob_hash == metadata.blob_hash
                && self.size_bytes == metadata.size_bytes
                && self.blob_hash == lillux::sha256_hex(bytes),
            "Q fixture reference changed its metadata or exact captured bytes"
        );
        Ok(())
    }
}

/// One finite transfer of already captured Q evidence. This is not an export
/// command, a CAS publication capability, or native settlement authority.
///
/// The caller must bound the complete incoming frame before deserializing this
/// type. At the 64 KiB raw ceiling the padded body is 87,384 bytes; the current
/// fixed metadata adds less than 4 KiB. This fits the 384 KiB frame ceiling, but
/// the selected frame/relay/aggregate budgets still require independent admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QFixtureCaptureContent {
    /// The metadata's current kind/schema versions this closed transfer shape.
    pub metadata: QFixtureCaptureMetadata,
    pub bytes_base64: String,
}

impl QFixtureCaptureContent {
    /// Package bounded captured bytes. Native provenance and signed fixture
    /// authority are deliberately not inferred by this encoding operation.
    pub fn from_bytes(metadata: QFixtureCaptureMetadata, bytes: &[u8]) -> Result<Self> {
        metadata.validate()?;
        ensure!(
            bytes.len() as u64 == metadata.size_bytes
                && bytes.len() as u64 <= MAX_Q_FIXTURE_BYTES
                && lillux::sha256_hex(bytes) == metadata.blob_hash,
            "Q fixture transfer changed captured byte size or hash"
        );
        Ok(Self {
            metadata,
            bytes_base64: STANDARD.encode(bytes),
        })
    }

    /// Return exact inert bytes only after their full retained identity agrees.
    /// Every decoded allocation is capped by the signed raw ceiling, including
    /// padded encodings that could otherwise decode to one or two extra bytes.
    pub fn verify_for(
        &self,
        reference: &QFixtureCaptureReference,
        contract: &QFixtureContract,
        binding: &ExecutionChannelBinding,
        expected_native: &NativeQualificationScopeSettlementObservation,
    ) -> Result<Vec<u8>> {
        // Check the signed fixture before the transfer decoder can allocate.
        self.metadata
            .validate_for(contract, binding, expected_native)?;
        ensure!(
            self.bytes_base64.len() as u64 <= contract.max_bytes.div_ceil(3) * 4,
            "Q fixture transfer encoding exceeds its signed ceiling"
        );
        self.decode_reference(reference)
    }

    /// Global structural/content identity only. A frame validator can use this
    /// without fabricating a signed fixture contract. Guest/receiver admission
    /// must still call verify_for with the actual originally selected contract.
    pub fn verify_reference(
        &self,
        reference: &QFixtureCaptureReference,
        binding: &ExecutionChannelBinding,
        expected_native: &NativeQualificationScopeSettlementObservation,
    ) -> Result<Vec<u8>> {
        binding.validate()?;
        expected_native.validate(binding)?;
        self.metadata.validate()?;
        ensure!(
            self.metadata.native_settlement == *expected_native,
            "Q fixture transfer changed its original native completion"
        );
        self.decode_reference(reference)
    }

    fn decode_reference(&self, reference: &QFixtureCaptureReference) -> Result<Vec<u8>> {
        reference.validate()?;
        self.metadata.validate()?;
        ensure!(
            reference.metadata_hash == self.metadata.digest()?
                && reference.blob_hash == self.metadata.blob_hash
                && reference.size_bytes == self.metadata.size_bytes,
            "Q fixture transfer changed its retained metadata reference"
        );
        let encoded_size = reference.size_bytes.div_ceil(3) * 4;
        ensure!(
            self.bytes_base64.len() as u64 == encoded_size
                && encoded_size <= MAX_Q_FIXTURE_BYTES.div_ceil(3) * 4,
            "Q fixture transfer encoding changed its bounded declared size"
        );
        let mut bytes = vec![0; usize::try_from(reference.size_bytes)?];
        let decoded = STANDARD
            .decode_slice(&self.bytes_base64, &mut bytes)
            .context("decode bounded Q fixture transfer")?;
        ensure!(
            decoded == bytes.len() && STANDARD.encode(&bytes) == self.bytes_base64,
            "Q fixture transfer bytes are not exact canonical base64"
        );
        ensure!(
            lillux::sha256_hex(&bytes) == reference.blob_hash,
            "Q fixture transfer changed its exact captured bytes"
        );
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_execution::{EXECUTION_CHANNEL_BINDING_SCHEMA, ExternalExecutionMode};
    use base64::Engine as _;

    fn fixture() -> (
        QFixtureContract,
        ExecutionChannelBinding,
        NativeQualificationScopeSettlementObservation,
        QFixtureCaptureMetadata,
        QFixtureCaptureReference,
    ) {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let binding = ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: ExternalExecutionMode::QualificationStructuredSession {},
            placement_thread_id: "T-q".into(),
            occurrence_id: "q".into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program_digest: "f".repeat(64),
            channel_nonce: "0".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: 1,
            execution_deadline_ms: 100,
            expires_at_ms: 200,
            candidate_export_max_bytes: 0,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        let contract = QFixtureContract {
            schema: Q_FIXTURE_CONTRACT_SCHEMA,
            mount_destination: Q_FIXTURE_MOUNT_DESTINATION.into(),
            leaf: "candidate.txt".into(),
            initial_content_b64: STANDARD.encode(b"before"),
            initial_blob_hash: lillux::sha256_hex(b"before"),
            max_bytes: 128,
        };
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
        let metadata = QFixtureCaptureMetadata {
            kind: Q_FIXTURE_CAPTURE_KIND.into(),
            schema: Q_FIXTURE_CAPTURE_SCHEMA,
            fixture_contract_digest: q_fixture_contract_digest(&contract).unwrap(),
            native_settlement: native.clone(),
            blob_hash: lillux::sha256_hex(b"after"),
            size_bytes: 5,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let reference = QFixtureCaptureReference {
            metadata_hash: metadata.digest().unwrap(),
            blob_hash: metadata.blob_hash.clone(),
            size_bytes: 5,
        };
        (contract, binding, native, metadata, reference)
    }

    fn transfer_fixture(
        bytes: &[u8],
    ) -> (
        QFixtureContract,
        ExecutionChannelBinding,
        NativeQualificationScopeSettlementObservation,
        QFixtureCaptureReference,
        QFixtureCaptureContent,
    ) {
        let (mut contract, binding, native, mut metadata, _) = fixture();
        contract.max_bytes = MAX_Q_FIXTURE_BYTES;
        metadata.fixture_contract_digest = q_fixture_contract_digest(&contract).unwrap();
        metadata.blob_hash = lillux::sha256_hex(bytes);
        metadata.size_bytes = bytes.len() as u64;
        let reference = QFixtureCaptureReference {
            metadata_hash: metadata.digest().unwrap(),
            blob_hash: metadata.blob_hash.clone(),
            size_bytes: metadata.size_bytes,
        };
        let content = QFixtureCaptureContent::from_bytes(metadata, bytes).unwrap();
        (contract, binding, native, reference, content)
    }

    #[test]
    fn finite_transfer_roundtrips_empty_and_maximum_bytes() {
        for bytes in [vec![], vec![0xff; MAX_Q_FIXTURE_BYTES as usize]] {
            let (contract, binding, native, reference, content) = transfer_fixture(&bytes);
            assert_eq!(
                content
                    .verify_for(&reference, &contract, &binding, &native)
                    .unwrap(),
                bytes
            );
            let wire = lillux::canonical_json(&serde_json::to_value(&content).unwrap()).unwrap();
            assert!(wire.len() < crate::external_execution::MAX_FRAME_BYTES - 16 * 1024);
            assert_eq!(
                serde_json::from_str::<QFixtureCaptureContent>(&wire).unwrap(),
                content
            );
        }
        let (_, _, _, metadata, _) = fixture();
        assert!(
            QFixtureCaptureContent::from_bytes(
                metadata.clone(),
                &vec![0; MAX_Q_FIXTURE_BYTES as usize + 1]
            )
            .is_err()
        );
        assert!(QFixtureCaptureContent::from_bytes(metadata, b"other").is_err());
    }

    #[test]
    fn structural_transfer_check_never_substitutes_for_signed_fixture_validation() {
        let (mut contract, binding, native, reference, content) = transfer_fixture(b"after");
        assert_eq!(
            content
                .verify_reference(&reference, &binding, &native)
                .unwrap(),
            b"after"
        );
        contract.leaf = "different.txt".into();
        assert!(
            content
                .verify_for(&reference, &contract, &binding, &native)
                .is_err()
        );
        let mut other_native = native;
        other_native.completion_request_digest = "9".repeat(64);
        assert!(
            content
                .verify_reference(&reference, &binding, &other_native)
                .is_err()
        );
    }

    #[test]
    fn finite_transfer_refuses_padding_overrun_and_same_size_byte_substitution() {
        let (contract, binding, native, reference, content) = transfer_fixture(b"a");
        // "YWJj" has the same padded encoded length as "YQ==", but would
        // decode three bytes into a declared one-byte result. decode_slice
        // must refuse without allocating those extra decoded bytes.
        for bad in [
            "YQ".to_owned(),
            "YR==".into(),
            "YQ==\n".into(),
            "YWJj".into(),
            "Yg==".into(),
            "A".repeat(90_000),
        ] {
            let mut changed = content.clone();
            changed.bytes_base64 = bad;
            assert!(
                changed
                    .verify_for(&reference, &contract, &binding, &native)
                    .is_err()
            );
        }
        let mut changed_reference = reference;
        changed_reference.size_bytes = MAX_Q_FIXTURE_BYTES + 1;
        assert!(
            content
                .verify_for(&changed_reference, &contract, &binding, &native)
                .is_err()
        );
    }

    #[test]
    fn finite_transfer_refuses_rehashed_native_substitution_and_open_or_stale_wire() {
        let (contract, binding, native, reference, content) = transfer_fixture(b"after");
        let mut changed = content.clone();
        changed.metadata.native_settlement.completion_frame_digest = "9".repeat(64);
        let mut rehashed = reference.clone();
        rehashed.metadata_hash = changed.metadata.digest().unwrap();
        assert!(
            changed
                .verify_for(&rehashed, &contract, &binding, &native)
                .is_err()
        );
        let value = serde_json::to_value(&content).unwrap();
        for field in ["metadata", "bytes_base64"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<QFixtureCaptureContent>(missing).is_err());
        }
        let mut extra = value.clone();
        extra["path"] = serde_json::json!("/workspace");
        assert!(serde_json::from_value::<QFixtureCaptureContent>(extra).is_err());
        let mut stale = value;
        stale["metadata"]["schema"] = serde_json::json!(0);
        let decoded: QFixtureCaptureContent = serde_json::from_value(stale).unwrap();
        assert!(
            decoded
                .verify_for(&reference, &contract, &binding, &native)
                .is_err()
        );
        changed = content;
        changed.metadata.normalized_mode = ProjectFile::EXECUTABLE_MODE;
        assert!(
            changed
                .verify_for(&reference, &contract, &binding, &native)
                .is_err()
        );
    }

    #[test]
    fn exact_capture_uses_kernel_cas_canonical_identity() {
        let (contract, binding, native, metadata, reference) = fixture();
        reference
            .verify_for(&metadata, &contract, &binding, &native, b"after")
            .unwrap();
        // CAS writes Values in kernel canonical order, not derived struct order.
        let value = metadata.to_value().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let cas = lillux::cas::CasStore::new(directory.path().to_path_buf());
        assert_eq!(
            reference.metadata_hash,
            cas.put_object(&value).unwrap().hash
        );
        assert_eq!(
            metadata.project_file_digest().unwrap(),
            cas.put_object(&metadata.project_file().unwrap().to_value())
                .unwrap()
                .hash
        );
        assert_eq!(
            q_fixture_contract_digest(&contract).unwrap(),
            cas.put_object(&serde_json::to_value(&contract).unwrap())
                .unwrap()
                .hash
        );
        assert_eq!(
            QFixtureCaptureMetadata::from_value(&value).unwrap(),
            metadata
        );
        assert!(value.get("settlement_hash").is_none());
        assert!(value.get("project_file_hash").is_none());
    }

    #[test]
    fn refuses_changed_completion_and_fixture_even_with_rehashed_metadata() {
        let (contract, binding, native, metadata, reference) = fixture();
        for field in [
            "binding_digest",
            "launcher_occurrence_digest",
            "applied_runtime_receipt_digest",
            "completion_frame_digest",
            "completion_request_digest",
        ] {
            let mut value = metadata.to_value().unwrap();
            value["native_settlement"][field] = "9".repeat(64).into();
            let changed = QFixtureCaptureMetadata::from_value(&value).unwrap();
            let mut rehashed = reference.clone();
            rehashed.metadata_hash = changed.digest().unwrap();
            assert!(
                rehashed
                    .verify_for(&changed, &contract, &binding, &native, b"after")
                    .is_err(),
                "{field}"
            );
        }
        let mut other_contract = contract.clone();
        other_contract.leaf = "other.txt".into();
        assert!(
            reference
                .verify_for(&metadata, &other_contract, &binding, &native, b"after")
                .is_err()
        );
        let mut other_binding = binding.clone();
        other_binding.occurrence_id = "other".into();
        assert!(
            reference
                .verify_for(&metadata, &contract, &other_binding, &native, b"after")
                .is_err()
        );
        assert!(
            reference
                .verify_for(&metadata, &contract, &binding, &native, b"other")
                .is_err()
        );
    }

    #[test]
    fn refuses_ordinary_lane_executable_oversize_and_open_wire() {
        let (contract, mut binding, native, metadata, reference) = fixture();
        binding.execution_mode = ExternalExecutionMode::StructuredSession {};
        binding.candidate_export_max_bytes = 1;
        assert!(
            reference
                .verify_for(&metadata, &contract, &binding, &native, b"after")
                .is_err()
        );
        let mut changed = metadata.clone();
        changed.normalized_mode = ProjectFile::EXECUTABLE_MODE;
        assert!(changed.validate().is_err());
        changed = metadata.clone();
        changed.size_bytes = MAX_Q_FIXTURE_BYTES + 1;
        assert!(changed.validate().is_err());
        let value = metadata.to_value().unwrap();
        for key in value.as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(QFixtureCaptureMetadata::from_value(&missing).is_err());
        }
        let mut extra = value;
        extra["settlement_hash"] = "a".repeat(64).into();
        assert!(QFixtureCaptureMetadata::from_value(&extra).is_err());
    }
}
