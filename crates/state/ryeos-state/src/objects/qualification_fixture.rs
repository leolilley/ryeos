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

use anyhow::{Result, ensure};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_execution::{EXECUTION_CHANNEL_BINDING_SCHEMA, ExternalExecutionMode};
    use base64::{Engine as _, engine::general_purpose::STANDARD};

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
