//! Durable, provider-neutral identity of one external runtime snapshot effect.
//!
//! The intent owns one provider-sequence attempt. A provider locator is
//! only an attempt result; restored bytes and runtime behavior require an
//! independent qualification rooted in the retained source testimony.

use anyhow::{Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::canonical_json;

pub const RUNTIME_SNAPSHOT_INTENT_SCHEMA: u32 = 4;
pub const RUNTIME_SNAPSHOT_RESULT_SCHEMA: u32 = 2;
pub const RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL: &str = "ryeos.runtime-snapshot-adapter.v5";
pub const RUNTIME_SNAPSHOT_READINESS_PROTOCOL: &str = "ryeos.runtime-snapshot-readiness.v1";
pub const RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA: u32 = 1;
pub const RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL: &str =
    "ryeos.runtime-snapshot-qualification-adapter.v1";
pub const RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL: &str =
    "ryeos.runtime-snapshot-qualification-termination.v1";
pub const MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES: usize = 24 * 1024;
pub const MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES: u64 = 64 * 1024 * 1024 + 16 * 1024;
pub const RUNTIME_SNAPSHOT_STAGE_SCHEMA: u32 = 1;
pub const RUNTIME_SNAPSHOT_UPLOAD_RECEIPT_SCHEMA: u32 = 1;
pub const RUNTIME_SNAPSHOT_UPLOAD_ADAPTER_PROTOCOL: &str =
    "ryeos.runtime-snapshot-upload-adapter.v1";
pub const RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL: &str =
    "ryeos.runtime-snapshot-create-adapter.v1";
pub const RUNTIME_SNAPSHOT_CREATE_RESULT_SCHEMA: u32 = 1;

/// Provenance of an exact owner-runtime tree. Neither variant alone qualifies
/// the restored provider snapshot or grants a worker execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotSource {
    CapturedProduct {
        product_witness_hash: String,
    },
    BundleMaterialization {
        materialization_attestation_hash: String,
        source_coordinate_digest: String,
        materialization_binding_digest: String,
    },
}

impl RuntimeSnapshotSource {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CapturedProduct {
                product_witness_hash,
            } => {
                require_hash(product_witness_hash, "product witness")?;
            }
            Self::BundleMaterialization {
                materialization_attestation_hash,
                source_coordinate_digest,
                materialization_binding_digest,
            } => {
                for (label, value) in [
                    (
                        "materialization attestation",
                        materialization_attestation_hash,
                    ),
                    ("materialization coordinate", source_coordinate_digest),
                    ("materialization binding", materialization_binding_digest),
                ] {
                    require_hash(value, label)?;
                }
            }
        }
        Ok(())
    }
}

/// One sealed invocation of the exact admitted snapshot producer. The upload
/// descriptor is a process-local transport coordinate, not durable identity;
/// its bytes and digest are owned by the retained intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotIntent,
    pub provider_spec_digest: String,
    pub upload_descriptor: u32,
    pub upload_bytes: u64,
    pub upload_sha256: String,
}

impl RuntimeSnapshotAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.intent.validate()?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL
                && self.upload_descriptor > 2
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && self.upload_bytes == self.intent.upload_bytes
                && self.upload_sha256 == self.intent.upload_sha256,
            "runtime snapshot adapter handoff differs from the retained intent"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "runtime snapshot adapter request exceeds its bound"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotAdapterResponse {
    Bound {
        locator: RuntimeSnapshotLocator,
    },
    Uncertain {
        operation_id: String,
        intent_digest: String,
    },
}

impl RuntimeSnapshotAdapterResponse {
    pub fn validate_for(&self, request: &RuntimeSnapshotAdapterRequest) -> Result<()> {
        request.validate()?;
        match self {
            Self::Bound { locator } => locator.validate_for(&request.intent)?,
            Self::Uncertain {
                operation_id,
                intent_digest,
            } => {
                ensure!(
                    operation_id == &request.intent.operation_id
                        && intent_digest == &request.intent.digest()?,
                    "uncertain snapshot result changed its durable attempt"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotIntent {
    pub schema: u32,
    pub operation_id: String,
    pub owner_principal: String,
    pub provider_id: String,
    pub source_occurrence_id: String,
    /// Present only for a materialized source. This binds the provider
    /// occurrence to the one-shot bootstrap journal across later replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_bootstrap_operation_id: Option<String>,
    /// Exact create-time identity checked again by the adapter's final GET.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_timeout_seconds: Option<u32>,
    pub provider_group_id: String,
    /// Signed producer authority, distinct from the later placement binding
    /// that will name the resulting snapshot ID.
    pub production_profile_digest: String,
    pub adapter_artifact_hash: String,
    pub provider_spec_digest: String,
    pub settings_digest: String,
    pub source: RuntimeSnapshotSource,
    pub guest_runtime_manifest_hash: String,
    pub owner_executable_sha256: String,
    pub controller_public_root: String,
    pub upload_sha256: String,
    pub upload_bytes: u64,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            "unsupported runtime snapshot intent"
        );
        for (label, value) in [
            ("operation", &self.operation_id),
            ("production profile", &self.production_profile_digest),
            ("adapter", &self.adapter_artifact_hash),
            ("provider spec", &self.provider_spec_digest),
            ("settings", &self.settings_digest),
            ("guest runtime manifest", &self.guest_runtime_manifest_hash),
            ("owner executable", &self.owner_executable_sha256),
            ("upload", &self.upload_sha256),
        ] {
            require_hash(value, label)?;
        }
        self.source.validate()?;
        match &self.source {
            RuntimeSnapshotSource::CapturedProduct { .. } => ensure!(
                self.source_bootstrap_operation_id.is_none()
                    && self.source_created_at.is_none()
                    && self.source_timeout_seconds.is_none(),
                "captured product cannot borrow bootstrap authority"
            ),
            RuntimeSnapshotSource::BundleMaterialization { .. } => ensure!(
                self.source_bootstrap_operation_id
                    .as_deref()
                    .is_some_and(|value| require_hash(value, "bootstrap operation").is_ok())
                    && self.source_created_at.as_deref().is_some_and(|value| {
                        !value.is_empty()
                            && value.len() <= 128
                            && value.bytes().all(|byte| byte.is_ascii_graphic())
                    })
                    && self
                        .source_timeout_seconds
                        .is_some_and(|value| (1..=86_400).contains(&value)),
                "materialized snapshot lacks exact bootstrap creation witness"
            ),
        }
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .ok_or_else(|| anyhow::anyhow!("runtime snapshot owner principal is invalid"))?;
        require_hash(owner, "owner")?;
        require_bounded_text(&self.provider_id, 128, "provider")?;
        require_bounded_text(&self.source_occurrence_id, 256, "source occurrence")?;
        require_bounded_text(&self.provider_group_id, 256, "provider group")?;
        let encoded = self
            .controller_public_root
            .strip_prefix("ed25519:")
            .ok_or_else(|| {
                anyhow::anyhow!("runtime snapshot controller root has no Ed25519 envelope")
            })?;
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        ensure!(
            decoded.len() == 32
                && base64::engine::general_purpose::STANDARD.encode(&decoded) == encoded,
            "runtime snapshot controller root is not canonical 32-byte base64"
        );
        ensure!(
            (1..=MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES).contains(&self.upload_bytes)
                && self.attempt_deadline_ms > 0,
            "runtime snapshot upload or deadline exceeds its bound"
        );
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "runtime snapshot operation does not match exact transfer coordinates"
        );
        Ok(())
    }

    /// Stable across a retry with a later deadline, but unique to the exact
    /// admitted source and delivery bytes. A caller cannot mint a second
    /// attempt opportunity by choosing another operation ID.
    pub fn derived_operation_id(&self) -> Result<String> {
        let coordinates = (
            "ryeos.runtime-snapshot-operation.v4",
            &self.owner_principal,
            &self.provider_id,
            &self.source_occurrence_id,
            (
                &self.source_bootstrap_operation_id,
                &self.source_created_at,
                &self.source_timeout_seconds,
            ),
            &self.provider_group_id,
            &self.production_profile_digest,
            &self.adapter_artifact_hash,
            &self.provider_spec_digest,
            &self.settings_digest,
            &self.source,
            &self.guest_runtime_manifest_hash,
            &self.owner_executable_sha256,
            &self.controller_public_root,
            &self.upload_sha256,
            self.upload_bytes,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

/// One durable mutation stage beneath an immutable snapshot intent. A stage
/// operation ID cannot be changed by extending its deadline or by changing a
/// provider response. The journal must
/// reserve and claim this exact intent before invoking its adapter; this type
/// alone is not permission to make provider contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSnapshotStage {
    Upload,
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotStageIntent {
    pub schema: u32,
    pub operation_id: String,
    pub parent_operation_id: String,
    pub parent_intent_digest: String,
    pub stage: RuntimeSnapshotStage,
    /// Required only for create. It names a fully accepted, immutable upload
    /// receipt, not a minted token or a completed local socket write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_upload_receipt_digest: Option<String>,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotStageIntent {
    pub fn validate_for(
        &self,
        parent: &RuntimeSnapshotIntent,
        accepted_upload: Option<(&RuntimeSnapshotStageIntent, &RuntimeSnapshotUploadReceipt)>,
    ) -> Result<()> {
        parent.validate()?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_STAGE_SCHEMA
                && self.parent_operation_id == parent.operation_id
                && self.parent_intent_digest == parent.digest()?
                && self.attempt_deadline_ms > 0
                && self.attempt_deadline_ms <= parent.attempt_deadline_ms,
            "snapshot stage differs from its retained parent"
        );
        match self.stage {
            RuntimeSnapshotStage::Upload => ensure!(
                self.accepted_upload_receipt_digest.is_none() && accepted_upload.is_none(),
                "snapshot upload cannot borrow an earlier receipt"
            ),
            RuntimeSnapshotStage::Create => {
                let (upload_stage, receipt) = accepted_upload.ok_or_else(|| {
                    anyhow::anyhow!("snapshot create lacks an accepted upload receipt")
                })?;
                receipt.validate_for_stage(parent, upload_stage)?;
                ensure!(
                    self.accepted_upload_receipt_digest.as_deref()
                        == Some(receipt.digest()?.as_str()),
                    "snapshot create changed its accepted upload receipt"
                );
            }
        }
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "snapshot stage operation changed its mutation identity"
        );
        Ok(())
    }

    pub fn derived_operation_id(&self) -> Result<String> {
        require_hash(&self.parent_operation_id, "stage parent operation")?;
        require_hash(&self.parent_intent_digest, "stage parent intent")?;
        if let Some(digest) = &self.accepted_upload_receipt_digest {
            require_hash(digest, "stage upload receipt")?;
        }
        let coordinates = (
            "ryeos.runtime-snapshot-stage-operation.v1",
            &self.parent_operation_id,
            self.stage,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
    }

    pub fn digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

/// The adapter's bounded observation of an accepted proxy upload. This is
/// protocol evidence for a later create claim, not independent proof of the
/// bytes restored from a provider snapshot or remote writer settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotUploadReceipt {
    pub schema: u32,
    pub upload_operation_id: String,
    pub upload_intent_digest: String,
    pub parent_operation_id: String,
    pub parent_intent_digest: String,
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    pub upload_path: String,
    pub content_type: String,
    pub upload_sha256: String,
    pub upload_bytes: u64,
    pub source_response_sha256: String,
    pub provider_response_sha256: String,
    pub provider_status: u16,
    pub completed_at_ms: i64,
}

impl RuntimeSnapshotUploadReceipt {
    pub fn validate_for(&self, parent: &RuntimeSnapshotIntent) -> Result<()> {
        parent.validate()?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_UPLOAD_RECEIPT_SCHEMA
                && self.parent_operation_id == parent.operation_id
                && self.parent_intent_digest == parent.digest()?
                && self.source_occurrence_id == parent.source_occurrence_id
                && self.provider_group_id == parent.provider_group_id
                && self.upload_sha256 == parent.upload_sha256
                && self.upload_bytes == parent.upload_bytes
                && (200..300).contains(&self.provider_status)
                && self.completed_at_ms > 0,
            "snapshot upload receipt does not acknowledge its exact parent"
        );
        require_hash(&self.source_response_sha256, "source response")?;
        require_hash(&self.provider_response_sha256, "upload response")?;
        ensure!(
            self.upload_path.starts_with('/')
                && self.upload_path.len() <= 512
                && !self.upload_path.contains("..")
                && !self.upload_path.contains("//")
                && self.upload_path.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')
                }),
            "snapshot upload receipt path is invalid"
        );
        ensure!(
            !self.content_type.is_empty()
                && self.content_type.len() <= 128
                && self.content_type.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'+')
                }),
            "snapshot upload receipt content type is invalid"
        );
        let upload_stage = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: self.upload_operation_id.clone(),
            parent_operation_id: self.parent_operation_id.clone(),
            parent_intent_digest: self.parent_intent_digest.clone(),
            stage: RuntimeSnapshotStage::Upload,
            accepted_upload_receipt_digest: None,
            attempt_deadline_ms: 1,
        };
        ensure!(
            self.upload_operation_id == upload_stage.derived_operation_id()?,
            "snapshot upload receipt changed its stage operation"
        );
        require_hash(&self.upload_intent_digest, "upload stage intent")?;
        Ok(())
    }

    pub fn validate_for_stage(
        &self,
        parent: &RuntimeSnapshotIntent,
        stage: &RuntimeSnapshotStageIntent,
    ) -> Result<()> {
        self.validate_observation_for_stage(parent, stage)?;
        ensure!(
            self.completed_at_ms <= stage.attempt_deadline_ms,
            "snapshot upload receipt completed after its stage deadline"
        );
        Ok(())
    }

    /// Exact attempt attribution remains valid for a late response. Only the
    /// timely variant may authorize the subsequent create mutation.
    pub fn validate_observation_for_stage(
        &self,
        parent: &RuntimeSnapshotIntent,
        stage: &RuntimeSnapshotStageIntent,
    ) -> Result<()> {
        self.validate_for(parent)?;
        stage.validate_for(parent, None)?;
        ensure!(
            stage.stage == RuntimeSnapshotStage::Upload
                && self.upload_operation_id == stage.operation_id
                && self.upload_intent_digest == stage.digest()?,
            "snapshot upload receipt belongs to another attempt"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(canonical_json(self)?)))
    }
}

/// The upload invocation carries a sealed byte descriptor and can make only
/// the token and proxy-upload mutations. It cannot request snapshot creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotUploadAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotIntent,
    pub stage: RuntimeSnapshotStageIntent,
    pub provider_spec_digest: String,
    pub upload_descriptor: u32,
    pub upload_bytes: u64,
    pub upload_sha256: String,
}

impl RuntimeSnapshotUploadAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.stage.validate_for(&self.intent, None)?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_UPLOAD_ADAPTER_PROTOCOL
                && self.stage.stage == RuntimeSnapshotStage::Upload
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && self.upload_descriptor > 2
                && self.upload_bytes == self.intent.upload_bytes
                && self.upload_sha256 == self.intent.upload_sha256,
            "snapshot upload adapter handoff differs from its retained stage"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "snapshot upload adapter request exceeds bound"
        );
        Ok(())
    }
}

/// The create invocation has no upload descriptor and is authorized only by
/// the exact receipt already accepted by the durable upload journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotCreateAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotIntent,
    pub upload_stage: RuntimeSnapshotStageIntent,
    pub accepted_upload: RuntimeSnapshotUploadReceipt,
    pub create_stage: RuntimeSnapshotStageIntent,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotCreateAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.accepted_upload
            .validate_for_stage(&self.intent, &self.upload_stage)?;
        self.create_stage.validate_for(
            &self.intent,
            Some((&self.upload_stage, &self.accepted_upload)),
        )?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL
                && self.create_stage.stage == RuntimeSnapshotStage::Create
                && self.provider_spec_digest == self.intent.provider_spec_digest,
            "snapshot create adapter handoff differs from its retained stages"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "snapshot create adapter request exceeds bound"
        );
        Ok(())
    }
}

/// An adapter response binds a provider locator to the one admitted create
/// stage and its accepted upload. It remains only a creation observation;
/// independent restored-content qualification is still required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotCreateResult {
    pub schema: u32,
    pub create_operation_id: String,
    pub create_intent_digest: String,
    pub accepted_upload_receipt_digest: String,
    pub locator: RuntimeSnapshotLocator,
}

impl RuntimeSnapshotCreateResult {
    pub fn validate_for(&self, request: &RuntimeSnapshotCreateAdapterRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_CREATE_RESULT_SCHEMA
                && self.create_operation_id == request.create_stage.operation_id
                && self.create_intent_digest == request.create_stage.digest()?
                && self.accepted_upload_receipt_digest == request.accepted_upload.digest()?,
            "snapshot create result changed its durable stage or accepted upload"
        );
        self.locator.validate_for(&request.intent)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotLocator {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    pub snapshot_id: String,
    pub provider_response_sha256: String,
    /// Bounded, adapter-validated creation projection retained for later
    /// readiness interpretation. It is not a restored-content claim.
    pub provider_creation_observation: serde_json::Value,
    pub adapter_observation_sha256: String,
}

/// One separately owned restored-Sandbox qualification attempt. This is not
/// a Worker allocation: it cannot inherit a Worker runtime qualification that
/// this very attempt is intended to establish. The exact selected snapshot
/// and provider profile are retained before non-idempotent provider contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationIntent {
    pub schema: u32,
    pub operation_id: String,
    pub owner_principal: String,
    pub snapshot_operation_id: String,
    pub snapshot_intent_digest: String,
    pub snapshot_id: String,
    pub provider_id: String,
    pub provider_group_id: String,
    pub qualification_profile_digest: String,
    pub adapter_artifact_hash: String,
    pub provider_spec_digest: String,
    pub settings_digest: String,
    pub verifier_artifact_hash: String,
    pub maximum_lifetime_seconds: u32,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotQualificationIntent {
    pub fn validate_for(
        &self,
        source: &RuntimeSnapshotIntent,
        locator: &RuntimeSnapshotLocator,
    ) -> Result<()> {
        locator.validate_for(source)?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA
                && self.owner_principal == source.owner_principal
                && self.snapshot_operation_id == source.operation_id
                && self.snapshot_intent_digest == source.digest()?
                && self.snapshot_id == locator.snapshot_id
                && self.provider_id == source.provider_id
                && self.provider_group_id == source.provider_group_id
                && self.attempt_deadline_ms > 0
                && (1..=3600).contains(&self.maximum_lifetime_seconds),
            "snapshot qualification intent differs from retained snapshot authority"
        );
        for (label, hash) in [
            ("qualification profile", &self.qualification_profile_digest),
            ("qualification adapter", &self.adapter_artifact_hash),
            ("provider spec", &self.provider_spec_digest),
            ("settings", &self.settings_digest),
            ("verifier artifact", &self.verifier_artifact_hash),
        ] {
            require_hash(hash, label)?;
        }
        ensure!(
            self.operation_id == self.derived_operation_id()?,
            "snapshot qualification operation differs from exact selected product"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "snapshot qualification intent exceeds its bound"
        );
        Ok(())
    }

    /// Deadline changes cannot mint a fresh non-idempotent create opportunity.
    pub fn derived_operation_id(&self) -> Result<String> {
        let coordinates = (
            "ryeos.runtime-snapshot-qualification.v1",
            &self.owner_principal,
            &self.snapshot_operation_id,
            &self.snapshot_intent_digest,
            &self.snapshot_id,
            &self.provider_id,
            &self.provider_group_id,
            &self.qualification_profile_digest,
            &self.adapter_artifact_hash,
            &self.provider_spec_digest,
            &self.settings_digest,
            &self.verifier_artifact_hash,
            self.maximum_lifetime_seconds,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
    }
}

/// A complete, bounded provider creation observation. The occurrence is only
/// a locator for the verifier run; it does not prove restored content or make
/// the runtime eligible for Worker allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationOccurrence {
    pub schema: u32,
    pub operation_id: String,
    pub occurrence_id: String,
    pub provider_response_sha256: String,
    /// Daemon-authored host observation. An adapter response must supply
    /// false; the controller replaces it with its own observed deadline bit
    /// before retaining the occurrence for settlement and cleanup.
    pub contact_deadline_exceeded: bool,
}

impl RuntimeSnapshotQualificationOccurrence {
    pub fn validate_for(&self, intent: &RuntimeSnapshotQualificationIntent) -> Result<()> {
        ensure!(
            self.schema == 1 && self.operation_id == intent.operation_id,
            "snapshot qualification occurrence changed attempt identity"
        );
        require_bounded_text(&self.occurrence_id, 256, "qualification occurrence")?;
        require_hash(&self.provider_response_sha256, "qualification response")?;
        ensure!(
            canonical_json(self)?.len() <= 1024,
            "snapshot qualification occurrence exceeds its bound"
        );
        Ok(())
    }
}

/// One termination authority for a restored qualification occurrence. It is
/// distinct from Worker termination and cannot be reminted by changing a
/// deadline. A provider terminal observation does not itself prove guest
/// writer exclusion or qualify the runtime product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationTerminationIntent {
    pub schema: u32,
    pub operation_id: String,
    pub qualification_operation_id: String,
    pub occurrence_id: String,
    pub owner_principal: String,
    pub provider_id: String,
    pub provider_spec_digest: String,
    pub attempt_deadline_ms: i64,
}

impl RuntimeSnapshotQualificationTerminationIntent {
    pub fn validate_for(
        &self,
        qualification: &RuntimeSnapshotQualificationIntent,
        occurrence: &RuntimeSnapshotQualificationOccurrence,
    ) -> Result<()> {
        occurrence.validate_for(qualification)?;
        ensure!(
            self.schema == 1
                && self.qualification_operation_id == qualification.operation_id
                && self.occurrence_id == occurrence.occurrence_id
                && self.owner_principal == qualification.owner_principal
                && self.provider_id == qualification.provider_id
                && self.provider_spec_digest == qualification.provider_spec_digest
                && self.attempt_deadline_ms > 0
                && self.operation_id == self.derived_operation_id()?,
            "qualification termination differs from the retained restored occurrence"
        );
        ensure!(
            canonical_json(self)?.len() <= 2048,
            "qualification termination intent exceeds its bound"
        );
        Ok(())
    }

    pub fn derived_operation_id(&self) -> Result<String> {
        let coordinates = (
            RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL,
            &self.qualification_operation_id,
            &self.occurrence_id,
        );
        Ok(hex::encode(Sha256::digest(canonical_json(&coordinates)?)))
    }
}

/// Provider-only terminal observation. The daemon must bind it to a claimed
/// one-shot attempt, and a separate guest-writer witness remains required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationTerminalObservation {
    pub schema: u32,
    pub operation_id: String,
    pub occurrence_id: String,
    pub provider_response_sha256: String,
    pub terminated_at: String,
    pub contact_deadline_exceeded: bool,
}

impl RuntimeSnapshotQualificationTerminalObservation {
    pub fn validate_for(
        &self,
        intent: &RuntimeSnapshotQualificationTerminationIntent,
    ) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.operation_id == intent.operation_id
                && self.occurrence_id == intent.occurrence_id
                && !self.terminated_at.is_empty()
                && self.terminated_at.len() <= 64,
            "qualification terminal observation changed its exact occurrence"
        );
        require_hash(&self.provider_response_sha256, "terminal response")?;
        ensure!(
            canonical_json(self)?.len() <= 1024,
            "qualification terminal observation exceeds its bound"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationTerminationAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotQualificationTerminationIntent,
    pub qualification_intent: RuntimeSnapshotQualificationIntent,
    pub occurrence: RuntimeSnapshotQualificationOccurrence,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotQualificationTerminationAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.intent
            .validate_for(&self.qualification_intent, &self.occurrence)?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "qualification termination handoff changed signed authority"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotQualificationTerminationAdapterResponse {
    Terminal {
        observation: RuntimeSnapshotQualificationTerminalObservation,
    },
    Pending {
        operation_id: String,
    },
}

impl RuntimeSnapshotQualificationTerminationAdapterResponse {
    pub fn validate_for(
        &self,
        request: &RuntimeSnapshotQualificationTerminationAdapterRequest,
    ) -> Result<()> {
        request.validate()?;
        match self {
            Self::Terminal { observation } => observation.validate_for(&request.intent)?,
            Self::Pending { operation_id } => ensure!(
                operation_id == &request.intent.operation_id,
                "pending qualification termination changed attempt identity"
            ),
        }
        Ok(())
    }
}

/// Sealed create-only handoff. The selected snapshot ID is a retained provider
/// result, not a mutable setting or a value inferred from the adapter's own
/// environment. This grants no verifier run or qualification claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotQualificationAdapterRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotQualificationIntent,
    pub source_intent: RuntimeSnapshotIntent,
    pub locator: RuntimeSnapshotLocator,
    pub readiness: RuntimeSnapshotReadinessObservation,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotQualificationAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.intent
            .validate_for(&self.source_intent, &self.locator)?;
        let readiness_request = RuntimeSnapshotReadinessRequest {
            protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
            intent: self.source_intent.clone(),
            locator: self.locator.clone(),
            provider_spec_digest: self.source_intent.provider_spec_digest.clone(),
        };
        self.readiness.validate_for(&readiness_request)?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL
                && self.provider_spec_digest == self.intent.provider_spec_digest,
            "snapshot qualification adapter handoff changed signed authority"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "snapshot qualification adapter handoff exceeds its bound"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeSnapshotQualificationAdapterResponse {
    OccurrenceBound {
        occurrence: RuntimeSnapshotQualificationOccurrence,
    },
    Uncertain {
        operation_id: String,
    },
}

impl RuntimeSnapshotQualificationAdapterResponse {
    pub fn validate_for(&self, request: &RuntimeSnapshotQualificationAdapterRequest) -> Result<()> {
        request.validate()?;
        match self {
            Self::OccurrenceBound { occurrence } => occurrence.validate_for(&request.intent)?,
            Self::Uncertain { operation_id } => ensure!(
                operation_id == &request.intent.operation_id,
                "uncertain qualification result changed durable attempt"
            ),
        }
        Ok(())
    }
}

/// A read-only observation of a previously bound locator. This grants no
/// create/retry authority and no claim about restored snapshot contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotReadinessRequest {
    pub protocol: String,
    pub intent: RuntimeSnapshotIntent,
    pub locator: RuntimeSnapshotLocator,
    pub provider_spec_digest: String,
}

impl RuntimeSnapshotReadinessRequest {
    pub fn validate(&self) -> Result<()> {
        self.locator.validate_for(&self.intent)?;
        ensure!(
            self.protocol == RUNTIME_SNAPSHOT_READINESS_PROTOCOL
                && self.provider_spec_digest == self.intent.provider_spec_digest
                && canonical_json(self)?.len() <= MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES,
            "runtime snapshot readiness request differs from its retained locator"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotReadinessObservation {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub snapshot_id: String,
    pub source_occurrence_id: String,
    pub provider_group_id: String,
    pub creation_response_sha256: String,
    pub readiness_response_sha256: String,
    pub captured_at: String,
    pub size_bytes: i64,
}

impl RuntimeSnapshotReadinessObservation {
    pub fn validate_for(&self, request: &RuntimeSnapshotReadinessRequest) -> Result<()> {
        request.validate()?;
        ensure!(
            self.schema == 1
                && self.operation_id == request.intent.operation_id
                && self.intent_digest == request.intent.digest()?
                && self.snapshot_id == request.locator.snapshot_id
                && self.source_occurrence_id == request.intent.source_occurrence_id
                && self.provider_group_id == request.intent.provider_group_id
                && self.creation_response_sha256 == request.locator.provider_response_sha256
                && self.size_bytes > 0,
            "runtime snapshot readiness changed its retained locator"
        );
        require_hash(&self.readiness_response_sha256, "readiness response")?;
        ensure!(
            self.captured_at.len() <= 64
                && !self.captured_at.is_empty()
                && self.captured_at.is_ascii(),
            "runtime snapshot readiness has invalid capture time"
        );
        Ok(())
    }
}

impl RuntimeSnapshotLocator {
    pub fn validate_for(&self, intent: &RuntimeSnapshotIntent) -> Result<()> {
        intent.validate()?;
        ensure!(
            self.schema == RUNTIME_SNAPSHOT_RESULT_SCHEMA
                && self.operation_id == intent.operation_id
                && self.intent_digest == intent.digest()?,
            "runtime snapshot locator contradicts its durable intent"
        );
        for (label, value) in [
            ("provider response", &self.provider_response_sha256),
            ("adapter observation", &self.adapter_observation_sha256),
        ] {
            require_hash(value, label)?;
        }
        let observation = canonical_json(&self.provider_creation_observation)?;
        ensure!(
            self.provider_creation_observation.is_object()
                && !observation.is_empty()
                && observation.len() <= 4096
                && hex::encode(Sha256::digest(&observation)) == self.adapter_observation_sha256,
            "runtime snapshot creation observation changed its adapter identity"
        );
        for (label, value) in [
            ("source occurrence", &self.source_occurrence_id),
            ("provider group", &self.provider_group_id),
            ("snapshot", &self.snapshot_id),
        ] {
            require_bounded_text(value, 256, label)?;
        }
        ensure!(
            self.source_occurrence_id == intent.source_occurrence_id
                && self.provider_group_id == intent.provider_group_id,
            "runtime snapshot locator changed its source occurrence"
        );
        Ok(())
    }
}

fn require_hash(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "runtime snapshot {label} digest is invalid"
    );
    Ok(())
}

fn require_bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= maximum
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b':')),
        "runtime snapshot {label} is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> RuntimeSnapshotIntent {
        let mut intent = RuntimeSnapshotIntent {
            schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbox-fixture-1".into(),
            source_bootstrap_operation_id: None,
            source_created_at: None,
            source_timeout_seconds: None,
            provider_group_id: "sbg-fixture-1".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "d".repeat(64),
            settings_digest: "5".repeat(64),
            source: RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: "6".repeat(64),
            },
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode([3u8; 32])
            ),
            upload_sha256: "9".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: 1_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    #[test]
    fn source_and_delivery_changes_move_intent_identity() {
        let baseline = intent();
        let digest = baseline.digest().unwrap();
        let mut materialized = baseline.clone();
        materialized.source = RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash: "a".repeat(64),
            source_coordinate_digest: "b".repeat(64),
            materialization_binding_digest: "c".repeat(64),
        };
        materialized.source_bootstrap_operation_id = Some("d".repeat(64));
        assert!(materialized.validate().is_err());
        materialized.source_created_at = Some("2026-09-29T00:00:00Z".into());
        materialized.source_timeout_seconds = Some(900);
        materialized.operation_id = materialized.derived_operation_id().unwrap();
        materialized.validate().unwrap();
        assert_ne!(materialized.operation_id, baseline.operation_id);
        assert_ne!(materialized.digest().unwrap(), digest);
        let mut changed_creation = materialized.clone();
        changed_creation.source_created_at = Some("2026-09-30T00:00:00Z".into());
        assert!(changed_creation.validate().is_err());
        changed_creation.operation_id = changed_creation.derived_operation_id().unwrap();
        assert_ne!(changed_creation.operation_id, materialized.operation_id);
        let mut changed_timeout = materialized.clone();
        changed_timeout.source_timeout_seconds = Some(901);
        assert!(changed_timeout.validate().is_err());
        changed_timeout.operation_id = changed_timeout.derived_operation_id().unwrap();
        assert_ne!(changed_timeout.operation_id, materialized.operation_id);
        for field in [
            "guest_runtime_manifest_hash",
            "controller_public_root",
            "upload_sha256",
            "adapter_artifact_hash",
            "provider_spec_digest",
            "settings_digest",
            "production_profile_digest",
            "source_occurrence_id",
            "provider_group_id",
        ] {
            let mut changed = serde_json::to_value(&baseline).unwrap();
            changed[field] = serde_json::json!(if field == "controller_public_root" {
                format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode([4u8; 32])
                )
            } else if field == "source_occurrence_id" {
                "sbox-fixture-2".to_owned()
            } else if field == "provider_group_id" {
                "sbg-fixture-2".to_owned()
            } else {
                "a".repeat(64)
            });
            let mut changed: RuntimeSnapshotIntent = serde_json::from_value(changed).unwrap();
            assert!(changed.validate().is_err(), "{field} reused operation ID");
            changed.operation_id = changed.derived_operation_id().unwrap();
            assert_ne!(changed.digest().unwrap(), digest, "{field}");
        }
        let mut unknown = serde_json::to_value(&baseline).unwrap();
        unknown["credential"] = serde_json::json!("ambient");
        assert!(serde_json::from_value::<RuntimeSnapshotIntent>(unknown).is_err());
    }

    #[test]
    fn deadline_change_cannot_mint_another_attempt_opportunity() {
        let baseline = intent();
        let mut retried = baseline.clone();
        retried.attempt_deadline_ms += 100;
        assert_eq!(
            retried.derived_operation_id().unwrap(),
            baseline.operation_id
        );
        assert_ne!(retried.digest().unwrap(), baseline.digest().unwrap());
        retried.operation_id = "f".repeat(64);
        assert!(retried.validate().is_err());
    }

    fn upload_stage(parent: &RuntimeSnapshotIntent) -> RuntimeSnapshotStageIntent {
        let mut stage = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: String::new(),
            parent_operation_id: parent.operation_id.clone(),
            parent_intent_digest: parent.digest().unwrap(),
            stage: RuntimeSnapshotStage::Upload,
            accepted_upload_receipt_digest: None,
            attempt_deadline_ms: 100,
        };
        stage.operation_id = stage.derived_operation_id().unwrap();
        stage
    }

    fn upload_receipt(
        parent: &RuntimeSnapshotIntent,
        stage: &RuntimeSnapshotStageIntent,
    ) -> RuntimeSnapshotUploadReceipt {
        RuntimeSnapshotUploadReceipt {
            schema: RUNTIME_SNAPSHOT_UPLOAD_RECEIPT_SCHEMA,
            upload_operation_id: stage.operation_id.clone(),
            upload_intent_digest: stage.digest().unwrap(),
            parent_operation_id: parent.operation_id.clone(),
            parent_intent_digest: parent.digest().unwrap(),
            source_occurrence_id: parent.source_occurrence_id.clone(),
            provider_group_id: parent.provider_group_id.clone(),
            upload_path: "/runtime/owner.tar".into(),
            content_type: "application/octet-stream".into(),
            upload_sha256: parent.upload_sha256.clone(),
            upload_bytes: parent.upload_bytes,
            source_response_sha256: "a".repeat(64),
            provider_response_sha256: "b".repeat(64),
            provider_status: 204,
            completed_at_ms: 99,
        }
    }

    #[test]
    fn create_stage_requires_exact_accepted_upload() {
        let parent = intent();
        let upload = upload_stage(&parent);
        upload.validate_for(&parent, None).unwrap();
        let receipt = upload_receipt(&parent, &upload);
        receipt.validate_for_stage(&parent, &upload).unwrap();
        let mut create = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: String::new(),
            parent_operation_id: parent.operation_id.clone(),
            parent_intent_digest: parent.digest().unwrap(),
            stage: RuntimeSnapshotStage::Create,
            accepted_upload_receipt_digest: Some(receipt.digest().unwrap()),
            attempt_deadline_ms: 200,
        };
        create.operation_id = create.derived_operation_id().unwrap();
        create
            .validate_for(&parent, Some((&upload, &receipt)))
            .unwrap();
        assert!(create.validate_for(&parent, None).is_err());
        let mut changed = receipt.clone();
        changed.provider_response_sha256 = "c".repeat(64);
        assert!(
            create
                .validate_for(&parent, Some((&upload, &changed)))
                .is_err()
        );
        changed = receipt.clone();
        changed.provider_status = 500;
        assert!(
            create
                .validate_for(&parent, Some((&upload, &changed)))
                .is_err()
        );
        changed = receipt.clone();
        changed.upload_sha256 = "c".repeat(64);
        assert!(
            create
                .validate_for(&parent, Some((&upload, &changed)))
                .is_err()
        );
        let mut changed = create.clone();
        changed.accepted_upload_receipt_digest = None;
        assert!(
            changed
                .validate_for(&parent, Some((&upload, &receipt)))
                .is_err()
        );
        let mut changed = create.clone();
        changed.attempt_deadline_ms += 100;
        assert_eq!(changed.derived_operation_id().unwrap(), create.operation_id);
        assert_ne!(changed.digest().unwrap(), create.digest().unwrap());
        let mut changed_receipt = receipt.clone();
        changed_receipt.provider_response_sha256 = "c".repeat(64);
        let mut another_create = create.clone();
        another_create.accepted_upload_receipt_digest = Some(changed_receipt.digest().unwrap());
        assert_eq!(
            another_create.derived_operation_id().unwrap(),
            create.operation_id
        );
        let mut extended_parent = parent.clone();
        extended_parent.attempt_deadline_ms += 100;
        let mut extended_upload = upload.clone();
        extended_upload.parent_intent_digest = extended_parent.digest().unwrap();
        assert_eq!(
            extended_upload.derived_operation_id().unwrap(),
            upload.operation_id
        );
        extended_upload
            .validate_for(&extended_parent, None)
            .unwrap();
    }

    #[test]
    fn upload_acknowledgment_is_bounded_by_exact_stage_and_deadline() {
        let parent = intent();
        let upload = upload_stage(&parent);
        let receipt = upload_receipt(&parent, &upload);
        receipt.validate_for_stage(&parent, &upload).unwrap();
        let mut changed = receipt.clone();
        changed.completed_at_ms = 101;
        changed
            .validate_observation_for_stage(&parent, &upload)
            .unwrap();
        assert!(changed.validate_for_stage(&parent, &upload).is_err());
        let mut changed = receipt.clone();
        changed.upload_intent_digest = "c".repeat(64);
        assert!(
            changed
                .validate_observation_for_stage(&parent, &upload)
                .is_err()
        );
        assert!(changed.validate_for_stage(&parent, &upload).is_err());
        let mut changed = receipt.clone();
        changed.upload_path = "/runtime/../other".into();
        assert!(changed.validate_for_stage(&parent, &upload).is_err());
        let mut changed = receipt;
        changed.source_occurrence_id = "sbox-other".into();
        assert!(changed.validate_for_stage(&parent, &upload).is_err());
    }

    #[test]
    fn staged_adapter_handoffs_bind_create_to_accepted_upload() {
        let parent = intent();
        let upload = upload_stage(&parent);
        let receipt = upload_receipt(&parent, &upload);
        let upload_request = RuntimeSnapshotUploadAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_UPLOAD_ADAPTER_PROTOCOL.into(),
            intent: parent.clone(),
            stage: upload.clone(),
            provider_spec_digest: parent.provider_spec_digest.clone(),
            upload_descriptor: 11,
            upload_bytes: parent.upload_bytes,
            upload_sha256: parent.upload_sha256.clone(),
        };
        upload_request.validate().unwrap();
        let mut over_parent = upload_request.clone();
        over_parent.stage.attempt_deadline_ms = parent.attempt_deadline_ms + 1;
        assert!(over_parent.validate().is_err());
        let mut create_stage = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: String::new(),
            parent_operation_id: parent.operation_id.clone(),
            parent_intent_digest: parent.digest().unwrap(),
            stage: RuntimeSnapshotStage::Create,
            accepted_upload_receipt_digest: Some(receipt.digest().unwrap()),
            attempt_deadline_ms: 200,
        };
        create_stage.operation_id = create_stage.derived_operation_id().unwrap();
        let create_request = RuntimeSnapshotCreateAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL.into(),
            intent: parent.clone(),
            upload_stage: upload,
            accepted_upload: receipt.clone(),
            create_stage: create_stage.clone(),
            provider_spec_digest: parent.provider_spec_digest.clone(),
        };
        create_request.validate().unwrap();
        let result = RuntimeSnapshotCreateResult {
            schema: RUNTIME_SNAPSHOT_CREATE_RESULT_SCHEMA,
            create_operation_id: create_stage.operation_id.clone(),
            create_intent_digest: create_stage.digest().unwrap(),
            accepted_upload_receipt_digest: receipt.digest().unwrap(),
            locator: RuntimeSnapshotLocator {
                schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
                operation_id: parent.operation_id.clone(),
                intent_digest: parent.digest().unwrap(),
                source_occurrence_id: parent.source_occurrence_id.clone(),
                provider_group_id: parent.provider_group_id.clone(),
                snapshot_id: "snp-staged".into(),
                provider_response_sha256: "a".repeat(64),
                provider_creation_observation: serde_json::json!({"schema": 1}),
                adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
            },
        };
        result.validate_for(&create_request).unwrap();
        let mut substituted = result.clone();
        substituted.accepted_upload_receipt_digest = "f".repeat(64);
        assert!(substituted.validate_for(&create_request).is_err());
        let mut substituted = result;
        substituted.create_intent_digest = "f".repeat(64);
        assert!(substituted.validate_for(&create_request).is_err());
    }

    #[test]
    fn locator_cannot_switch_source_after_attempt() {
        let intent = intent();
        let mut locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-fixture-1".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
        };
        locator.validate_for(&intent).unwrap();
        let mut changed_observation = locator.clone();
        changed_observation.provider_creation_observation["schema"] = serde_json::json!(2);
        assert!(changed_observation.validate_for(&intent).is_err());
        locator.source_occurrence_id = "sbox-other".into();
        assert!(locator.validate_for(&intent).is_err());
    }

    #[test]
    fn adapter_handoff_and_result_preserve_the_retained_attempt() {
        let intent = intent();
        let request = RuntimeSnapshotAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL.into(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            upload_descriptor: 11,
            upload_bytes: intent.upload_bytes,
            upload_sha256: intent.upload_sha256.clone(),
            intent,
        };
        request.validate().unwrap();
        let uncertain = RuntimeSnapshotAdapterResponse::Uncertain {
            operation_id: request.intent.operation_id.clone(),
            intent_digest: request.intent.digest().unwrap(),
        };
        uncertain.validate_for(&request).unwrap();
        let mut wrong = request.clone();
        wrong.upload_sha256 = "b".repeat(64);
        assert!(wrong.validate().is_err());
        let mut wrong = uncertain;
        if let RuntimeSnapshotAdapterResponse::Uncertain { intent_digest, .. } = &mut wrong {
            *intent_digest = "f".repeat(64);
        }
        assert!(wrong.validate_for(&request).is_err());
    }

    #[test]
    fn readiness_is_read_only_and_bound_to_the_exact_locator() {
        let intent = intent();
        let locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-fixture-1".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
        };
        let request = RuntimeSnapshotReadinessRequest {
            protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            intent,
            locator,
        };
        request.validate().unwrap();
        let observation = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: request.intent.operation_id.clone(),
            intent_digest: request.intent.digest().unwrap(),
            snapshot_id: request.locator.snapshot_id.clone(),
            source_occurrence_id: request.locator.source_occurrence_id.clone(),
            provider_group_id: request.locator.provider_group_id.clone(),
            creation_response_sha256: request.locator.provider_response_sha256.clone(),
            readiness_response_sha256: "b".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        observation.validate_for(&request).unwrap();
        let mut substituted = observation.clone();
        substituted.snapshot_id = "snp-other".into();
        assert!(substituted.validate_for(&request).is_err());
        let mut substituted = request;
        substituted.provider_spec_digest = "c".repeat(64);
        assert!(substituted.validate().is_err());
    }

    #[test]
    fn qualification_attempt_is_distinct_and_cannot_switch_snapshot() {
        let source = intent();
        let locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: source.operation_id.clone(),
            intent_digest: source.digest().unwrap(),
            source_occurrence_id: source.source_occurrence_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            snapshot_id: "snp-fixture-1".into(),
            provider_response_sha256: "a".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: hex::encode(Sha256::digest(br#"{"schema":1}"#)),
        };
        let mut qualification = RuntimeSnapshotQualificationIntent {
            schema: RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA,
            operation_id: String::new(),
            owner_principal: source.owner_principal.clone(),
            snapshot_operation_id: source.operation_id.clone(),
            snapshot_intent_digest: source.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            provider_id: source.provider_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            qualification_profile_digest: "1".repeat(64),
            adapter_artifact_hash: "2".repeat(64),
            provider_spec_digest: "3".repeat(64),
            settings_digest: "4".repeat(64),
            verifier_artifact_hash: "5".repeat(64),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: source.attempt_deadline_ms + 1,
        };
        qualification.operation_id = qualification.derived_operation_id().unwrap();
        qualification.validate_for(&source, &locator).unwrap();
        let mut deadline_changed = qualification.clone();
        deadline_changed.attempt_deadline_ms += 1;
        assert_eq!(
            deadline_changed.derived_operation_id().unwrap(),
            qualification.operation_id
        );
        let mut switched = qualification.clone();
        switched.snapshot_id = "snp-other".into();
        assert!(switched.validate_for(&source, &locator).is_err());
        let mut switched = qualification.clone();
        switched.verifier_artifact_hash = "6".repeat(64);
        assert!(switched.validate_for(&source, &locator).is_err());

        let readiness = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: source.operation_id.clone(),
            intent_digest: source.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            source_occurrence_id: source.source_occurrence_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            creation_response_sha256: locator.provider_response_sha256.clone(),
            readiness_response_sha256: "b".repeat(64),
            captured_at: "2026-09-28T00:01:00Z".into(),
            size_bytes: 4096,
        };
        let request = RuntimeSnapshotQualificationAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL.into(),
            provider_spec_digest: qualification.provider_spec_digest.clone(),
            intent: qualification,
            source_intent: source,
            locator,
            readiness,
        };
        request.validate().unwrap();
        let bound = RuntimeSnapshotQualificationAdapterResponse::OccurrenceBound {
            occurrence: RuntimeSnapshotQualificationOccurrence {
                schema: 1,
                operation_id: request.intent.operation_id.clone(),
                occurrence_id: "sbx-restored".into(),
                provider_response_sha256: "c".repeat(64),
                contact_deadline_exceeded: false,
            },
        };
        bound.validate_for(&request).unwrap();
        let occurrence = RuntimeSnapshotQualificationOccurrence {
            schema: 1,
            operation_id: request.intent.operation_id.clone(),
            occurrence_id: "sbx-restored".into(),
            provider_response_sha256: "c".repeat(64),
            contact_deadline_exceeded: false,
        };
        let mut termination = RuntimeSnapshotQualificationTerminationIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: request.intent.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            owner_principal: request.intent.owner_principal.clone(),
            provider_id: request.intent.provider_id.clone(),
            provider_spec_digest: request.intent.provider_spec_digest.clone(),
            attempt_deadline_ms: request.intent.attempt_deadline_ms + 2,
        };
        termination.operation_id = termination.derived_operation_id().unwrap();
        termination
            .validate_for(&request.intent, &occurrence)
            .unwrap();
        let mut changed_deadline = termination.clone();
        changed_deadline.attempt_deadline_ms += 1;
        assert_eq!(
            changed_deadline.derived_operation_id().unwrap(),
            termination.operation_id
        );
        let mut switched_occurrence = termination.clone();
        switched_occurrence.occurrence_id = "sbx-other".into();
        assert!(
            switched_occurrence
                .validate_for(&request.intent, &occurrence)
                .is_err()
        );
        let terminal = RuntimeSnapshotQualificationTerminalObservation {
            schema: 1,
            operation_id: termination.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            provider_response_sha256: "d".repeat(64),
            terminated_at: "2026-09-28T00:02:00Z".into(),
            contact_deadline_exceeded: false,
        };
        terminal.validate_for(&termination).unwrap();
        let termination_request = RuntimeSnapshotQualificationTerminationAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL.into(),
            intent: termination.clone(),
            qualification_intent: request.intent.clone(),
            occurrence: occurrence.clone(),
            provider_spec_digest: termination.provider_spec_digest.clone(),
        };
        termination_request.validate().unwrap();
        RuntimeSnapshotQualificationTerminationAdapterResponse::Terminal {
            observation: terminal.clone(),
        }
        .validate_for(&termination_request)
        .unwrap();
        let mut wrong_request = termination_request.clone();
        wrong_request.occurrence.occurrence_id = "sbx-other".into();
        assert!(wrong_request.validate().is_err());
        let mut switched_terminal = terminal;
        switched_terminal.occurrence_id = "sbx-other".into();
        assert!(switched_terminal.validate_for(&termination).is_err());
        let mut wrong = request.clone();
        wrong.locator.snapshot_id = "snp-other".into();
        assert!(wrong.validate().is_err());
        wrong = request;
        wrong.readiness.snapshot_id = "snp-other".into();
        assert!(wrong.validate().is_err());
    }
}
