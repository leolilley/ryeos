//! Portable bytes for one finite Q Worker setup and turn with a session stream.
//!
//! The manifest is structural testimony only. Its chunks contain the exact
//! canonical ObservationBatch bodies, the optional retained Delta body, and
//! the retained setup and turn Final bodies, and the optional original native
//! fixture capture. Capture identity is checked here; the publisher authenticates
//! its native predecessor and retained receiver custody. Batches span the session beginning
//! at setup contact; they are not evidence of turn request causality. The command
//! journal also retains turn State digests, turn coordinates, and daemon-owned
//! successful send return timestamps. State bodies outside those retained frames
//! are not present here. The timestamps must be joined to the exact retained
//! command row by the publisher; a manifest cannot prove those returns itself.
//! Neither a valid manifest nor a transcript establishes semantic success,
//! native death, provider settlement, or a qualification.

use super::qualification_fixture::{QFixtureCaptureContent, QFixtureCaptureReference};
use anyhow::{Context as _, ensure};
use base64::Engine as _;
use ryeos_external_execution_contract::ProviderConfigurationLaunchInput;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const QUALIFICATION_WORKER_TRANSCRIPT_KIND: &str = "qualification_worker_transcript";
pub const QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA: u32 = 5;
pub const QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA: &str =
    "ryeos.qualification_worker_transcript_stream.v3";
pub const MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES: u64 = 1024 * 1024;
pub const MAX_QUALIFICATION_WORKER_TRANSCRIPT_BYTES: u64 = 112 * 1024 * 1024;
pub const MAX_QUALIFICATION_WORKER_BATCH_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_QUALIFICATION_WORKER_FINAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_OBSERVATIONS_PER_EVENT: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerTranscriptChunk {
    pub blob_hash: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerTranscriptManifest {
    pub kind: String,
    pub schema: u32,
    pub owner_principal: String,
    pub accepted_root_id: String,
    pub accepted_capsule_hash: String,
    pub placement_thread_id: String,
    pub qualification_operation_id: String,
    pub qualification_occurrence_id: String,
    pub channel_binding_digest: String,
    pub process_incarnation_digest: String,
    /// Bounded public projection retained from the exact original process launch.
    /// It describes one provider configuration, not a complete tool inventory.
    pub provider_configuration: ProviderConfigurationLaunchInput,
    /// Original controller observation, authenticated by the publisher's
    /// death-settled process and local custody source, not by this shape.
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub controller_canary: Option<
        crate::external_execution::qualification_controller_canary::QControllerCanaryEvidence,
    >,
    /// Original post-native-settlement capture, never ordinary project export.
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub fixture_capture: Option<QFixtureCaptureReference>,
    pub setup_request_digest: String,
    pub setup_final_response_digest: String,
    /// Exact durable daemon-owned successful setup send return.
    pub setup_owner_send_settled_at_ms: i64,
    pub request_digest: String,
    pub wire_max_bytes: u32,
    pub turn_id: String,
    pub turn_start_digest: String,
    pub turn_completion_digest: String,
    /// Exact durable daemon-owned successful send return for this command.
    pub owner_send_settled_at_ms: i64,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub delta_digest: Option<String>,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub delta_response_bytes: Option<u64>,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub delta_wire_max_bytes: Option<u32>,
    pub final_response_digest: String,
    pub final_response_bytes: u64,
    pub batch_count: u64,
    pub through_sequence: u64,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub last_batch_digest: Option<String>,
    pub batch_canonical_bytes: u64,
    pub stream_sha256: String,
    pub stream_bytes: u64,
    pub chunks: Vec<QualificationWorkerTranscriptChunk>,
}

/// Exact manifest identity for bounded historical transcript readback. Root,
/// occurrence and purpose authority must be joined by the admitting reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerTranscriptReference {
    pub manifest_hash: String,
    pub manifest: QualificationWorkerTranscriptManifest,
}

impl QualificationWorkerTranscriptReference {
    pub fn validate(&self) -> anyhow::Result<()> {
        hash("Q transcript manifest", &self.manifest_hash)?;
        ensure!(
            super::canonical_value_digest(&self.manifest.to_value()?)? == self.manifest_hash,
            "Q transcript reference changed its exact manifest"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerTranscriptChunkSelection {
    pub manifest_hash: String,
    pub chunk_index: u32,
}

impl QualificationWorkerTranscriptChunkSelection {
    pub fn validate(&self) -> anyhow::Result<()> {
        hash("Q transcript selected manifest", &self.manifest_hash)?;
        ensure!(
            u64::from(self.chunk_index)
                < MAX_QUALIFICATION_WORKER_TRANSCRIPT_BYTES
                    .div_ceil(MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES),
            "Q transcript selected chunk exceeds the transcript ceiling"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerTranscriptChunkResponse {
    pub manifest_hash: String,
    pub chunk_index: u32,
    pub blob_hash: String,
    pub size_bytes: u64,
    pub bytes_b64: String,
}

impl QualificationWorkerTranscriptChunkResponse {
    /// Decode at most one declared chunk. A valid response proves exact bytes
    /// only; complete stream verification and product interpretation follow.
    pub fn verify_for(
        &self,
        reference: &QualificationWorkerTranscriptReference,
        selection: &QualificationWorkerTranscriptChunkSelection,
    ) -> anyhow::Result<Vec<u8>> {
        reference.validate()?;
        selection.validate()?;
        ensure!(
            selection.manifest_hash == reference.manifest_hash
                && self.manifest_hash == reference.manifest_hash
                && self.chunk_index == selection.chunk_index,
            "Q transcript chunk response changed its selected manifest or index"
        );
        let chunk = reference
            .manifest
            .chunks
            .get(usize::try_from(selection.chunk_index)?)
            .context("Q transcript selected chunk is absent from its manifest")?;
        ensure!(
            self.blob_hash == chunk.blob_hash && self.size_bytes == chunk.size_bytes,
            "Q transcript chunk response changed its declared blob or size"
        );
        // Manifest validation bounds this size to one MiB. Check the exact
        // padded encoding length before the decoder can allocate its output.
        let encoded_bytes = chunk.size_bytes.div_ceil(3) * 4;
        ensure!(
            self.bytes_b64.len() == usize::try_from(encoded_bytes)?,
            "Q transcript chunk response exceeds or differs from its encoded size"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.bytes_b64)
            .context("decode Q transcript chunk")?;
        ensure!(
            bytes.len() == usize::try_from(chunk.size_bytes)?
                && lillux::sha256_hex(&bytes) == chunk.blob_hash
                && base64::engine::general_purpose::STANDARD.encode(&bytes) == self.bytes_b64,
            "Q transcript chunk response changed its exact canonical bytes"
        );
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerTranscriptStream {
    pub schema: String,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub fixture_content: Option<QFixtureCaptureContent>,
    pub batches: Vec<Value>,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub delta_response: Option<Value>,
    #[serde(deserialize_with = "Value::deserialize")]
    pub setup_final_response: Value,
    pub final_response: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerObservationBatch {
    first_sequence: u64,
    count: u64,
    previous_digest: Option<String>,
    batch_digest: String,
    events: Vec<Value>,
    session_observations: Vec<Value>,
}

impl QualificationWorkerTranscriptManifest {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::validate_object_kind(&self.kind, QUALIFICATION_WORKER_TRANSCRIPT_KIND)?;
        ensure!(
            self.schema == QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA,
            "Q Worker transcript is not the current v5 schema"
        );
        self.provider_configuration.validate()?;
        if let Some(canary) = &self.controller_canary {
            canary.validate()?;
            ensure!(
                canary.binding.resolved_request_digest == self.request_digest
                    && canary.binding.provider_declaration_id
                        == self.provider_configuration.provider_declaration_id
                    && canary.binding.configuration_adapter_artifact_hash
                        == self
                            .provider_configuration
                            .configuration_adapter_artifact_hash
                    && canary.binding.configuration_adapter_artifact_bytes
                        == self
                            .provider_configuration
                            .configuration_adapter_artifact_bytes,
                "Q canary resolved request differs from its transcript"
            );
        }
        if let Some(capture) = &self.fixture_capture {
            capture.validate()?;
        }
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .context("Q transcript owner is not a fingerprint principal")?;
        hash("Q transcript owner", owner)?;
        for (label, value) in [
            ("accepted root", &self.accepted_root_id),
            (
                "qualification occurrence",
                &self.qualification_occurrence_id,
            ),
            ("turn id", &self.turn_id),
        ] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 256
                    && value.trim() == value
                    && !value.chars().any(char::is_control),
                "Q transcript {label} is not bounded and canonical"
            );
        }
        let placement_hash = self
            .placement_thread_id
            .strip_prefix("Q-")
            .context("Q transcript placement is not a Q placement")?;
        hash("Q transcript placement", placement_hash)?;
        for (label, value) in [
            ("accepted capsule", &self.accepted_capsule_hash),
            ("qualification operation", &self.qualification_operation_id),
            ("channel binding", &self.channel_binding_digest),
            ("process incarnation", &self.process_incarnation_digest),
            ("setup request", &self.setup_request_digest),
            ("setup Final response", &self.setup_final_response_digest),
            ("request", &self.request_digest),
            ("turn start", &self.turn_start_digest),
            ("turn completion", &self.turn_completion_digest),
            ("Final response", &self.final_response_digest),
            ("stream", &self.stream_sha256),
        ] {
            hash(label, value)?;
        }
        ensure!(
            self.setup_request_digest != self.request_digest,
            "Q transcript setup and turn request digests must differ"
        );
        ensure!(
            self.setup_owner_send_settled_at_ms > 0
                && self.owner_send_settled_at_ms >= self.setup_owner_send_settled_at_ms,
            "Q transcript lacks ordered positive daemon-owned send return markers"
        );
        ensure!(
            (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES as u32).contains(&self.wire_max_bytes)
                && (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES).contains(&self.final_response_bytes)
                && self.final_response_bytes <= u64::from(self.wire_max_bytes),
            "Q transcript Final or selected wire bound is invalid"
        );
        ensure!(
            self.delta_digest.is_some() == self.delta_response_bytes.is_some()
                && self.delta_digest.is_some() == self.delta_wire_max_bytes.is_some(),
            "Q transcript Delta fields differ in presence"
        );
        if let (Some(digest), Some(bytes), Some(wire)) = (
            &self.delta_digest,
            self.delta_response_bytes,
            self.delta_wire_max_bytes,
        ) {
            hash("Q transcript Delta", digest)?;
            ensure!(
                (1..=MAX_QUALIFICATION_WORKER_FINAL_BYTES).contains(&bytes)
                    && bytes <= u64::from(wire)
                    && wire == self.wire_max_bytes,
                "Q transcript Delta differs from selected signed wire bound"
            );
        }
        ensure!(
            self.batch_count <= super::MAX_HOSTED_SESSION_OBSERVATION_EVENTS
                && self.batch_canonical_bytes <= MAX_QUALIFICATION_WORKER_BATCH_TOTAL_BYTES
                && match self.batch_count {
                    0 => {
                        self.through_sequence == 0
                            && self.last_batch_digest.is_none()
                            && self.batch_canonical_bytes == 0
                    }
                    _ => {
                        (self.batch_count..=super::MAX_HOSTED_SESSION_OBSERVATION_EVENTS)
                            .contains(&self.through_sequence)
                            && self.last_batch_digest.is_some()
                            && self.batch_canonical_bytes > 0
                    }
                },
            "Q transcript batch summary is invalid"
        );
        if let Some(last) = &self.last_batch_digest {
            hash("Q transcript last batch", last)?;
        }
        ensure!(
            self.stream_bytes
                > self.batch_canonical_bytes
                    + self.final_response_bytes
                    + self.delta_response_bytes.unwrap_or(0)
                && self.stream_bytes <= MAX_QUALIFICATION_WORKER_TRANSCRIPT_BYTES,
            "Q transcript stream byte count is invalid"
        );
        let expected_chunks = self
            .stream_bytes
            .div_ceil(MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES);
        ensure!(
            !self.chunks.is_empty() && u64::try_from(self.chunks.len())? == expected_chunks,
            "Q transcript has an invalid chunk count"
        );
        let mut total = 0u64;
        for (index, chunk) in self.chunks.iter().enumerate() {
            hash("Q transcript chunk", &chunk.blob_hash)?;
            ensure!(
                (1..=MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES).contains(&chunk.size_bytes)
                    && (index + 1 == self.chunks.len()
                        || chunk.size_bytes == MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES),
                "Q transcript chunk has an invalid ordered size"
            );
            total = total
                .checked_add(chunk.size_bytes)
                .context("Q transcript chunk byte count overflow")?;
        }
        ensure!(
            total == self.stream_bytes,
            "Q transcript chunks changed total bytes"
        );
        ensure!(
            lillux::canonical_json(&serde_json::to_value(self)?)?.len() <= MAX_MANIFEST_BYTES,
            "Q transcript manifest exceeds its object bound"
        );
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        ensure!(
            serde_json::to_vec(value)?.len() <= MAX_MANIFEST_BYTES,
            "Q transcript manifest exceeds its object bound"
        );
        let manifest: Self = serde_json::from_value(value.clone())?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Verify the complete bytes named by the manifest. This proves framing,
    /// ordering and digests only; it does not interpret Worker semantics.
    pub fn verify_chunks(
        &self,
        chunks: &[Vec<u8>],
    ) -> anyhow::Result<QualificationWorkerTranscriptStream> {
        self.validate()?;
        ensure!(
            chunks.len() == self.chunks.len(),
            "Q transcript chunk count differs from manifest"
        );
        let mut bytes = Vec::with_capacity(usize::try_from(self.stream_bytes)?);
        for (declared, actual) in self.chunks.iter().zip(chunks) {
            ensure!(
                actual.len() == usize::try_from(declared.size_bytes)?
                    && lillux::sha256_hex(actual) == declared.blob_hash,
                "Q transcript chunk differs from its exact blob"
            );
            bytes.extend_from_slice(actual);
        }
        ensure!(
            bytes.len() == usize::try_from(self.stream_bytes)?
                && lillux::sha256_hex(&bytes) == self.stream_sha256,
            "Q transcript stream differs from its manifest"
        );
        let value: Value = serde_json::from_slice(&bytes).context("decode Q transcript stream")?;
        ensure!(
            lillux::canonical_json(&value)?.as_bytes() == bytes.as_slice(),
            "Q transcript stream is not canonical JSON"
        );
        let stream: QualificationWorkerTranscriptStream =
            serde_json::from_value(value).context("decode Q transcript fields")?;
        ensure!(
            stream.schema == QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA,
            "Q transcript stream schema is not current"
        );
        ensure!(
            self.fixture_capture.is_some() == stream.fixture_content.is_some(),
            "Q transcript fixture reference and content differ in presence"
        );
        if let (Some(reference), Some(content)) = (&self.fixture_capture, &stream.fixture_content) {
            ensure!(
                content.metadata.native_settlement.binding_digest == self.channel_binding_digest,
                "Q transcript fixture changed its original channel binding"
            );
            content.verify_content_identity(reference)?;
        }
        let setup_final_bytes = lillux::canonical_json(&stream.setup_final_response)?;
        ensure!(
            setup_final_bytes.len() <= self.wire_max_bytes as usize
                && lillux::sha256_hex(setup_final_bytes.as_bytes())
                    == self.setup_final_response_digest,
            "Q transcript setup Final exceeds its wire bound or differs from retained digest"
        );
        let final_bytes = lillux::canonical_json(&stream.final_response)?;
        ensure!(
            u64::try_from(final_bytes.len())? == self.final_response_bytes
                && lillux::sha256_hex(final_bytes.as_bytes()) == self.final_response_digest,
            "Q transcript Final differs from retained command digest"
        );
        ensure!(
            stream.delta_response.is_some() == self.delta_digest.is_some(),
            "Q transcript Delta presence differs from retained command"
        );
        if let Some(delta) = &stream.delta_response {
            let canonical = lillux::canonical_json(delta)?;
            ensure!(
                Some(u64::try_from(canonical.len())?) == self.delta_response_bytes
                    && Some(lillux::sha256_hex(canonical.as_bytes())) == self.delta_digest,
                "Q transcript Delta differs from retained command digest"
            );
        }
        ensure!(
            u64::try_from(stream.batches.len())? == self.batch_count,
            "Q transcript omitted or added a batch"
        );
        let mut next = 1u64;
        let mut previous = None::<String>;
        let mut batch_bytes = 0u64;
        for raw in &stream.batches {
            let canonical = lillux::canonical_json(raw)?;
            ensure!(
                canonical.len() <= super::MAX_STRUCTURED_OBSERVATION_BATCH_BYTES
                    && canonical.len() <= self.wire_max_bytes as usize,
                "Q transcript batch exceeds its wire bound"
            );
            batch_bytes = batch_bytes
                .checked_add(u64::try_from(canonical.len())?)
                .context("Q transcript batch bytes overflow")?;
            ensure!(
                batch_bytes <= MAX_QUALIFICATION_WORKER_BATCH_TOTAL_BYTES,
                "Q transcript batch bytes exceed journal bound"
            );
            let batch: WorkerObservationBatch = serde_json::from_value(raw.clone())?;
            ensure!(
                batch.first_sequence == next
                    && batch.previous_digest == previous
                    && (1..=128).contains(&batch.count)
                    && batch.events.len() == usize::try_from(batch.count)?
                    && batch.session_observations.len()
                        <= batch
                            .events
                            .len()
                            .saturating_mul(MAX_OBSERVATIONS_PER_EVENT),
                "Q transcript batch chain or shape differs"
            );
            hash("Q transcript batch digest", &batch.batch_digest)?;
            let mut digest_input = raw.clone();
            digest_input
                .as_object_mut()
                .context("Q transcript batch is not an object")?
                .remove("batch_digest");
            ensure!(
                super::canonical_value_digest(&digest_input)? == batch.batch_digest,
                "Q transcript batch digest differs from canonical content"
            );
            next = next
                .checked_add(batch.count)
                .context("Q transcript batch sequence overflow")?;
            ensure!(
                next - 1 <= super::MAX_HOSTED_SESSION_OBSERVATION_EVENTS,
                "Q transcript event count exceeds its bound"
            );
            previous = Some(batch.batch_digest);
        }
        ensure!(
            batch_bytes == self.batch_canonical_bytes
                && next - 1 == self.through_sequence
                && previous == self.last_batch_digest,
            "Q transcript batch tail differs from manifest"
        );
        Ok(stream)
    }
}

fn hash(label: &str, value: &str) -> anyhow::Result<()> {
    super::thread_snapshot::validate_canonical_hash(label, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_and_chunk() -> (QualificationWorkerTranscriptManifest, Vec<u8>) {
        let mut batch = json!({
            "first_sequence":1,
            "count":1,
            "previous_digest":null,
            "events":[{"event_type":"observed","payload":{"ok":true}}],
            "session_observations":[],
        });
        let digest = super::super::canonical_value_digest(&batch).unwrap();
        batch["batch_digest"] = Value::String(digest.clone());
        let setup_final_response = json!({"result":"session observed"});
        let setup_final_json = lillux::canonical_json(&setup_final_response).unwrap();
        let final_response = json!({"result":"observed"});
        let final_json = lillux::canonical_json(&final_response).unwrap();
        let batch_json = lillux::canonical_json(&batch).unwrap();
        let stream = json!({
            "schema":QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA,
            "fixture_content":null,
            "batches":[batch],
            "delta_response":null,
            "setup_final_response":setup_final_response,
            "final_response":final_response,
        });
        let bytes = lillux::canonical_json(&stream).unwrap().into_bytes();
        let manifest = QualificationWorkerTranscriptManifest {
            kind: QUALIFICATION_WORKER_TRANSCRIPT_KIND.into(),
            schema: QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA,
            owner_principal: format!("fp:{}", "a".repeat(64)),
            accepted_root_id: "T-root".into(),
            accepted_capsule_hash: "b".repeat(64),
            placement_thread_id: format!("Q-{}", "c".repeat(64)),
            qualification_operation_id: "d".repeat(64),
            qualification_occurrence_id: "occurrence".into(),
            channel_binding_digest: "e".repeat(64),
            process_incarnation_digest: "f".repeat(64),
            fixture_capture: None,
            provider_configuration: ProviderConfigurationLaunchInput {
                schema: 1,
                configuration_sha256: "5".repeat(64),
                configuration_bytes: 100,
                command_environment: ryeos_external_execution_contract::ProviderCommandEnvironmentProjection {
                    environment_id: "ryeos-external-candidate".into(),
                    default_environment_id: "ryeos-external-candidate".into(),
                    include_local: false,
                    transport: ryeos_external_execution_contract::ProviderCommandEnvironmentTransport::CommandConnector,
                },
                provider_declaration_id: "codex-hosted".into(),
                configuration_destination: "environments.toml".into(),
                configuration_adapter_artifact_hash: "6".repeat(64),
                configuration_adapter_artifact_bytes: 100,
            },
            controller_canary: None,
            setup_request_digest: "4".repeat(64),
            setup_final_response_digest: lillux::sha256_hex(setup_final_json.as_bytes()),
            setup_owner_send_settled_at_ms: 1,
            request_digest: "1".repeat(64),
            wire_max_bytes: 1024 * 1024,
            turn_id: "turn".into(),
            turn_start_digest: "2".repeat(64),
            turn_completion_digest: "3".repeat(64),
            owner_send_settled_at_ms: 1,
            delta_digest: None,
            delta_response_bytes: None,
            delta_wire_max_bytes: None,
            final_response_digest: lillux::sha256_hex(final_json.as_bytes()),
            final_response_bytes: final_json.len() as u64,
            batch_count: 1,
            through_sequence: 1,
            last_batch_digest: Some(digest),
            batch_canonical_bytes: batch_json.len() as u64,
            stream_sha256: lillux::sha256_hex(&bytes),
            stream_bytes: bytes.len() as u64,
            chunks: vec![QualificationWorkerTranscriptChunk {
                blob_hash: lillux::sha256_hex(&bytes),
                size_bytes: bytes.len() as u64,
            }],
        };
        (manifest, bytes)
    }

    fn rebind_stream(
        manifest: &mut QualificationWorkerTranscriptManifest,
        stream: &Value,
    ) -> Vec<u8> {
        let bytes = lillux::canonical_json(stream).unwrap().into_bytes();
        assert!(bytes.len() <= MAX_QUALIFICATION_WORKER_TRANSCRIPT_CHUNK_BYTES as usize);
        manifest.stream_sha256 = lillux::sha256_hex(&bytes);
        manifest.stream_bytes = bytes.len() as u64;
        manifest.chunks = vec![QualificationWorkerTranscriptChunk {
            blob_hash: manifest.stream_sha256.clone(),
            size_bytes: manifest.stream_bytes,
        }];
        bytes
    }

    fn captured_stream(bytes: &[u8]) -> (QualificationWorkerTranscriptManifest, Value) {
        use super::super::qualification_fixture::{
            Q_FIXTURE_CAPTURE_KIND, Q_FIXTURE_CAPTURE_SCHEMA, QFixtureCaptureMetadata,
        };
        use crate::external_execution::{
            NativeNamespaceExit, NativeQualificationScopeSettlementObservation,
            NativeWriterExclusionMechanism,
        };
        let (mut manifest, original) = manifest_and_chunk();
        let metadata = QFixtureCaptureMetadata {
            kind: Q_FIXTURE_CAPTURE_KIND.into(),
            schema: Q_FIXTURE_CAPTURE_SCHEMA,
            fixture_contract_digest: "a".repeat(64),
            native_settlement: NativeQualificationScopeSettlementObservation {
                schema: 1,
                binding_digest: manifest.channel_binding_digest.clone(),
                launcher_occurrence_digest: "b".repeat(64),
                applied_runtime_receipt_digest: "c".repeat(64),
                completion_frame_digest: "d".repeat(64),
                completion_request_digest: "e".repeat(64),
                mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
                exit: NativeNamespaceExit::Code(0),
            },
            blob_hash: lillux::sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
            normalized_mode: super::super::ProjectFile::REGULAR_MODE,
        };
        manifest.fixture_capture = Some(QFixtureCaptureReference {
            metadata_hash: metadata.digest().unwrap(),
            blob_hash: metadata.blob_hash.clone(),
            size_bytes: metadata.size_bytes,
        });
        let content = QFixtureCaptureContent::from_bytes(metadata, bytes).unwrap();
        let mut stream: Value = serde_json::from_slice(&original).unwrap();
        stream["fixture_content"] = serde_json::to_value(content).unwrap();
        (manifest, stream)
    }

    #[test]
    fn fixture_content_is_required_nullable_and_current() {
        let (manifest, bytes) = manifest_and_chunk();
        manifest.verify_chunks(&[bytes.clone()]).unwrap();
        let mut missing = manifest.to_value().unwrap();
        missing.as_object_mut().unwrap().remove("fixture_capture");
        assert!(QualificationWorkerTranscriptManifest::from_current_value(&missing).is_err());
        let mut predecessor = manifest.clone();
        predecessor.schema = 3;
        assert!(predecessor.validate().is_err());
        for missing_field in [true, false] {
            let mut changed = manifest.clone();
            let mut stream: Value = serde_json::from_slice(&bytes).unwrap();
            if missing_field {
                stream.as_object_mut().unwrap().remove("fixture_content");
            } else {
                stream["schema"] = json!("ryeos.qualification_worker_transcript_stream.v2");
            }
            let rebound = rebind_stream(&mut changed, &stream);
            assert!(changed.verify_chunks(&[rebound]).is_err());
        }
    }

    #[test]
    fn fixture_stream_refuses_substitution_even_when_outer_chunks_are_rehashed() {
        let (manifest, stream) = captured_stream(b"changed");
        for mutation in [
            "missing_reference",
            "missing_content",
            "native_binding",
            "reference",
            "bytes",
            "oversized",
            "extra",
        ] {
            let mut changed = manifest.clone();
            let mut body = stream.clone();
            match mutation {
                "missing_reference" => changed.fixture_capture = None,
                "missing_content" => body["fixture_content"] = Value::Null,
                "native_binding" => {
                    body["fixture_content"]["metadata"]["native_settlement"]["binding_digest"] =
                        json!("0".repeat(64));
                    // Rehash the metadata too: the separate original-channel
                    // join must reject a structurally valid foreign capture.
                    changed.fixture_capture.as_mut().unwrap().metadata_hash =
                        super::super::canonical_value_digest(&body["fixture_content"]["metadata"])
                            .unwrap();
                }
                "reference" => {
                    changed.fixture_capture.as_mut().unwrap().metadata_hash = "0".repeat(64)
                }
                "bytes" => {
                    body["fixture_content"]["bytes_base64"] =
                        json!(base64::engine::general_purpose::STANDARD.encode(b"swapped"))
                }
                "oversized" => body["fixture_content"]["bytes_base64"] = json!("A".repeat(87388)),
                "extra" => body["fixture_content"]["authority"] = json!(true),
                _ => unreachable!(),
            }
            let bytes = rebind_stream(&mut changed, &body);
            assert!(
                changed.verify_chunks(&[bytes]).is_err(),
                "accepted {mutation}"
            );
        }
    }

    #[test]
    fn finite_capture_roundtrip_roots_metadata_and_its_blob_only() {
        for bytes in [
            Vec::new(),
            vec![b'x'; super::super::qualification_fixture::MAX_Q_FIXTURE_BYTES as usize],
        ] {
            let (mut manifest, body) = captured_stream(&bytes);
            let chunk = rebind_stream(&mut manifest, &body);
            let stream = manifest.verify_chunks(&[chunk]).unwrap();
            let reference = manifest.fixture_capture.as_ref().unwrap();
            let content = stream.fixture_content.unwrap();
            assert_eq!(content.verify_content_identity(reference).unwrap(), bytes);
            let links = crate::object_closure::object_links(&manifest.to_value().unwrap()).unwrap();
            assert_eq!(links.object_hashes, vec![reference.metadata_hash.clone()]);
            assert_eq!(
                links.blob_hashes,
                vec![manifest.chunks[0].blob_hash.clone()]
            );
            assert!(links.large_object_hashes.is_empty() && links.unsupported_kind.is_none());
            let capture_links =
                crate::object_closure::object_links(&content.metadata.to_value().unwrap()).unwrap();
            assert_eq!(capture_links.blob_hashes, vec![reference.blob_hash.clone()]);
            assert!(
                capture_links.object_hashes.is_empty()
                    && capture_links.large_object_hashes.is_empty()
            );
        }
    }

    #[test]
    fn provider_launch_input_is_required_current_and_manifest_bound() {
        let (manifest, _) = manifest_and_chunk();
        manifest.validate().unwrap();
        let value = manifest.to_value().unwrap();
        let mut missing = value.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("provider_configuration");
        assert!(serde_json::from_value::<QualificationWorkerTranscriptManifest>(missing).is_err());
        let mut null = value;
        null["provider_configuration"] = Value::Null;
        assert!(serde_json::from_value::<QualificationWorkerTranscriptManifest>(null).is_err());
        let mut previous = manifest.clone();
        previous.schema = 2;
        assert!(previous.validate().is_err());
        let mut local = manifest.clone();
        local
            .provider_configuration
            .command_environment
            .include_local = true;
        assert!(local.validate().is_err());
        let reference = QualificationWorkerTranscriptReference {
            manifest_hash: super::super::canonical_value_digest(&manifest.to_value().unwrap())
                .unwrap(),
            manifest,
        };
        for change_adapter in [false, true] {
            let mut changed = reference.clone();
            if change_adapter {
                changed
                    .manifest
                    .provider_configuration
                    .configuration_adapter_artifact_hash = "7".repeat(64);
            } else {
                changed.manifest.provider_configuration.configuration_sha256 = "7".repeat(64);
            }
            changed.manifest.validate().unwrap();
            assert!(changed.validate().is_err());
        }
    }

    #[test]
    fn bounded_readback_preserves_exact_manifest_and_stream() {
        let (manifest, bytes) = manifest_and_chunk();
        let reference = QualificationWorkerTranscriptReference {
            manifest_hash: super::super::canonical_value_digest(&manifest.to_value().unwrap())
                .unwrap(),
            manifest,
        };
        let selection = QualificationWorkerTranscriptChunkSelection {
            manifest_hash: reference.manifest_hash.clone(),
            chunk_index: 0,
        };
        let response = QualificationWorkerTranscriptChunkResponse {
            manifest_hash: reference.manifest_hash.clone(),
            chunk_index: 0,
            blob_hash: reference.manifest.chunks[0].blob_hash.clone(),
            size_bytes: bytes.len() as u64,
            bytes_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        };
        let decoded = response.verify_for(&reference, &selection).unwrap();
        assert_eq!(decoded, bytes);
        reference.manifest.verify_chunks(&[decoded]).unwrap();

        let mut wrong_reference = reference.clone();
        wrong_reference.manifest.accepted_root_id = "T-substituted".into();
        assert!(wrong_reference.validate().is_err());
        wrong_reference.manifest_hash =
            super::super::canonical_value_digest(&wrong_reference.manifest.to_value().unwrap())
                .unwrap();
        wrong_reference.validate().unwrap();
        assert!(response.verify_for(&wrong_reference, &selection).is_err());

        for changed in [
            QualificationWorkerTranscriptChunkResponse {
                chunk_index: 1,
                ..response.clone()
            },
            QualificationWorkerTranscriptChunkResponse {
                blob_hash: "0".repeat(64),
                ..response.clone()
            },
            QualificationWorkerTranscriptChunkResponse {
                size_bytes: response.size_bytes + 1,
                ..response.clone()
            },
            QualificationWorkerTranscriptChunkResponse {
                bytes_b64: base64::engine::general_purpose::STANDARD
                    .encode(vec![b'x'; bytes.len()]),
                ..response.clone()
            },
            QualificationWorkerTranscriptChunkResponse {
                bytes_b64: "!".repeat(response.bytes_b64.len()),
                ..response.clone()
            },
            QualificationWorkerTranscriptChunkResponse {
                bytes_b64: format!("{}AAAA", response.bytes_b64),
                ..response.clone()
            },
        ] {
            assert!(changed.verify_for(&reference, &selection).is_err());
        }
        let absent = QualificationWorkerTranscriptChunkSelection {
            chunk_index: 1,
            ..selection.clone()
        };
        let absent_response = QualificationWorkerTranscriptChunkResponse {
            chunk_index: 1,
            ..response.clone()
        };
        assert!(absent_response.verify_for(&reference, &absent).is_err());
        assert!(
            QualificationWorkerTranscriptChunkSelection {
                chunk_index: 112,
                ..selection.clone()
            }
            .validate()
            .is_err()
        );

        let mut value = serde_json::to_value(&reference).unwrap();
        value["extra"] = json!(true);
        assert!(serde_json::from_value::<QualificationWorkerTranscriptReference>(value).is_err());
        let mut value = serde_json::to_value(&selection).unwrap();
        value["extra"] = json!(true);
        assert!(
            serde_json::from_value::<QualificationWorkerTranscriptChunkSelection>(value).is_err()
        );
        let mut value = serde_json::to_value(&response).unwrap();
        value["extra"] = json!(true);
        assert!(
            serde_json::from_value::<QualificationWorkerTranscriptChunkResponse>(value).is_err()
        );
    }

    #[test]
    fn exact_manifest_roundtrips_and_verifies_stream() {
        let (manifest, bytes) = manifest_and_chunk();
        assert_eq!(
            QualificationWorkerTranscriptManifest::from_current_value(
                &manifest.to_value().unwrap()
            )
            .unwrap(),
            manifest
        );
        manifest.verify_chunks(&[bytes]).unwrap();
        let links = crate::object_closure::object_links(&manifest.to_value().unwrap()).unwrap();
        assert_eq!(
            links.blob_hashes,
            vec![manifest.chunks[0].blob_hash.clone()]
        );
        assert!(links.unsupported_kind.is_none());
    }

    #[test]
    fn stream_or_owner_substitution_is_refused() {
        let (manifest, bytes) = manifest_and_chunk();
        let mut changed = manifest.clone();
        changed.placement_thread_id = "ordinary".into();
        assert!(changed.validate().is_err());
        let mut changed = manifest.clone();
        changed.chunks[0].size_bytes -= 1;
        assert!(changed.validate().is_err());
        let mut changed = manifest.clone();
        changed.chunks[0].blob_hash = "0".repeat(64);
        assert!(changed.verify_chunks(&[bytes.clone()]).is_err());
        let mut changed = bytes;
        changed[0] = b' ';
        assert!(manifest.verify_chunks(&[changed]).is_err());
        let mut missing_null = manifest.to_value().unwrap();
        missing_null.as_object_mut().unwrap().remove("delta_digest");
        assert!(QualificationWorkerTranscriptManifest::from_current_value(&missing_null).is_err());
        let mut missing_settlement = manifest.to_value().unwrap();
        missing_settlement
            .as_object_mut()
            .unwrap()
            .remove("owner_send_settled_at_ms");
        assert!(
            QualificationWorkerTranscriptManifest::from_current_value(&missing_settlement).is_err()
        );
        let mut unsettled = manifest;
        unsettled.owner_send_settled_at_ms = 0;
        assert!(unsettled.validate().is_err());
    }

    #[test]
    fn setup_evidence_is_required_exact_and_bounded() {
        let (manifest, bytes) = manifest_and_chunk();
        for field in [
            "setup_request_digest",
            "setup_final_response_digest",
            "setup_owner_send_settled_at_ms",
        ] {
            let mut value = manifest.to_value().unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(QualificationWorkerTranscriptManifest::from_current_value(&value).is_err());
        }
        let mut predecessor = manifest.clone();
        predecessor.schema = 1;
        assert!(predecessor.validate().is_err());
        let mut unsettled = manifest.clone();
        unsettled.setup_owner_send_settled_at_ms = 0;
        assert!(unsettled.validate().is_err());
        let mut reversed = manifest.clone();
        reversed.setup_owner_send_settled_at_ms = reversed.owner_send_settled_at_ms + 1;
        assert!(reversed.validate().is_err());
        let mut same_request = manifest.clone();
        same_request.setup_request_digest = same_request.request_digest.clone();
        assert!(same_request.validate().is_err());
        let mut substituted_digest = manifest.clone();
        substituted_digest.setup_final_response_digest = "0".repeat(64);
        assert!(substituted_digest.verify_chunks(&[bytes.clone()]).is_err());

        // Update the outer hashes too: the retained setup digest and bound,
        // required field and current schema must each reject independently.
        for mutation in [
            "missing",
            "substituted",
            "oversized",
            "predecessor",
            "extra",
        ] {
            let mut changed = manifest.clone();
            let mut stream: Value = serde_json::from_slice(&bytes).unwrap();
            match mutation {
                "missing" => {
                    stream
                        .as_object_mut()
                        .unwrap()
                        .remove("setup_final_response");
                }
                "substituted" => {
                    stream["setup_final_response"] = json!({"result":"other"});
                }
                "oversized" => {
                    stream["setup_final_response"] = json!({"result":"x".repeat(2048)});
                    changed.setup_final_response_digest =
                        super::super::canonical_value_digest(&stream["setup_final_response"])
                            .unwrap();
                    changed.wire_max_bytes = 1024;
                }
                "predecessor" => {
                    stream["schema"] = json!("ryeos.qualification_worker_transcript_stream.v1");
                }
                "extra" => {
                    stream["extra"] = json!(true);
                }
                _ => unreachable!(),
            }
            let updated = lillux::canonical_json(&stream).unwrap().into_bytes();
            changed.stream_sha256 = lillux::sha256_hex(&updated);
            changed.stream_bytes = updated.len() as u64;
            changed.chunks[0].blob_hash = changed.stream_sha256.clone();
            changed.chunks[0].size_bytes = changed.stream_bytes;
            changed.validate().unwrap();
            assert!(
                changed.verify_chunks(&[updated]).is_err(),
                "accepted {mutation}"
            );
        }
    }

    #[test]
    fn retained_delta_is_optional_but_exact_when_present() {
        let (mut manifest, bytes) = manifest_and_chunk();
        let mut stream: Value = serde_json::from_slice(&bytes).unwrap();
        let delta = json!({
            "events":[],
            "session_observations":[{
                "kind":"state",
                "expected":"idle",
                "next":"turn_running",
                "turn_id":"turn"
            }]
        });
        let delta_json = lillux::canonical_json(&delta).unwrap();
        stream["delta_response"] = delta;
        let bytes = lillux::canonical_json(&stream).unwrap().into_bytes();
        manifest.delta_digest = Some(lillux::sha256_hex(delta_json.as_bytes()));
        manifest.delta_response_bytes = Some(delta_json.len() as u64);
        manifest.delta_wire_max_bytes = Some(manifest.wire_max_bytes);
        manifest.stream_sha256 = lillux::sha256_hex(&bytes);
        manifest.stream_bytes = bytes.len() as u64;
        manifest.chunks[0].blob_hash = lillux::sha256_hex(&bytes);
        manifest.chunks[0].size_bytes = bytes.len() as u64;
        manifest.verify_chunks(&[bytes]).unwrap();
        manifest.delta_digest = Some("0".repeat(64));
        assert!(manifest.validate().is_ok());
        assert!(
            manifest
                .verify_chunks(&[lillux::canonical_json(&stream).unwrap().into_bytes()])
                .is_err()
        );
    }
    #[test]
    fn original_controller_canary_is_required_nullable_and_manifest_current_only() {
        let (manifest, _) = manifest_and_chunk();
        let mut wire = manifest.to_value().unwrap();
        assert!(wire["controller_canary"].is_null());
        wire.as_object_mut().unwrap().remove("controller_canary");
        assert!(serde_json::from_value::<QualificationWorkerTranscriptManifest>(wire).is_err());
        let mut previous = manifest;
        previous.schema = 4;
        assert!(previous.validate().is_err());
    }
    #[test]
    fn canary_manifest_must_use_its_actual_process_configuration_adapter() {
        use ryeos_external_execution_contract::qualification_request_binding::{
            QualificationRequestBindingInput, QualificationRequestBindingOutput,
        };
        let (mut manifest, _) = manifest_and_chunk();
        let input = QualificationRequestBindingInput {
            schema: 1,
            template: json!({"provider_defined":"template"}),
            controller_canary_path: "/private/controller-canary/canary".into(),
        };
        let output = QualificationRequestBindingOutput {
            schema: 1,
            input_digest: input.digest().unwrap(),
            resolved_body: json!({"provider_defined":"bound"}),
        };
        let initial = json!({"directory_identity":{"containing_device":1,"inode":2},
            "file_identity":{"containing_device":1,"inode":3},"content_sha256":"a".repeat(64),"size_bytes":65,"permission_mode":384});
        let binding = json!({
            "schema":1,"controller_path":input.controller_canary_path,"initial_file":initial,
            "provider_declaration_id":manifest.provider_configuration.provider_declaration_id,
            "configuration_adapter_artifact_hash":manifest.provider_configuration.configuration_adapter_artifact_hash,
            "configuration_adapter_artifact_bytes":manifest.provider_configuration.configuration_adapter_artifact_bytes,
            "binding_input_digest":input.digest().unwrap(),"binding_output_digest":output.digest().unwrap(),
            "template_request_digest":super::super::canonical_value_digest(&input.template).unwrap(),
            "resolved_request_digest":super::super::canonical_value_digest(&output.resolved_body).unwrap(),"resolved_body":output.resolved_body,
        });
        manifest.request_digest = binding["resolved_request_digest"].as_str().unwrap().into();
        manifest.controller_canary = Some(
            serde_json::from_value(json!({"schema":1,"binding":binding,"terminal_file":initial}))
                .unwrap(),
        );
        manifest.validate().unwrap();
        for changed in [
            "provider_declaration_id",
            "configuration_adapter_artifact_hash",
            "configuration_adapter_artifact_bytes",
        ] {
            let mut value = serde_json::to_value(&manifest).unwrap();
            value["controller_canary"]["binding"][changed] = match changed {
                "provider_declaration_id" => json!("other-provider"),
                "configuration_adapter_artifact_hash" => json!("f".repeat(64)),
                _ => json!(999),
            };
            let changed: QualificationWorkerTranscriptManifest =
                serde_json::from_value(value).unwrap();
            changed
                .controller_canary
                .as_ref()
                .unwrap()
                .validate()
                .unwrap();
            assert!(changed.validate().is_err());
        }
    }
}
