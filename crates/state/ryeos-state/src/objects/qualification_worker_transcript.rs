//! Portable bytes for one locally observed Q Worker command and pushed stream.
//!
//! The manifest is structural testimony only. Its chunks contain the exact
//! canonical ObservationBatch bodies, the optional retained Delta body, and
//! the retained Final body. The command journal also retains turn State
//! digests and turn coordinates; State bodies outside those retained frames
//! are not present here. Neither a valid manifest nor a transcript establishes
//! semantic success, native death, provider settlement, or a qualification.

use anyhow::{Context as _, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const QUALIFICATION_WORKER_TRANSCRIPT_KIND: &str = "qualification_worker_transcript";
pub const QUALIFICATION_WORKER_TRANSCRIPT_SCHEMA: u32 = 1;
pub const QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA: &str =
    "ryeos.qualification_worker_transcript_stream.v1";
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
    pub request_digest: String,
    pub wire_max_bytes: u32,
    pub turn_id: String,
    pub turn_start_digest: String,
    pub turn_completion_digest: String,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerTranscriptStream {
    pub schema: String,
    pub batches: Vec<Value>,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub delta_response: Option<Value>,
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
            "Q Worker transcript is not the current v1 schema"
        );
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
            ("request", &self.request_digest),
            ("turn start", &self.turn_start_digest),
            ("turn completion", &self.turn_completion_digest),
            ("Final response", &self.final_response_digest),
            ("stream", &self.stream_sha256),
        ] {
            hash(label, value)?;
        }
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
        let final_response = json!({"result":"observed"});
        let final_json = lillux::canonical_json(&final_response).unwrap();
        let batch_json = lillux::canonical_json(&batch).unwrap();
        let stream = json!({
            "schema":QUALIFICATION_WORKER_TRANSCRIPT_STREAM_SCHEMA,
            "batches":[batch],
            "delta_response":null,
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
            request_digest: "1".repeat(64),
            wire_max_bytes: 1024 * 1024,
            turn_id: "turn".into(),
            turn_start_digest: "2".repeat(64),
            turn_completion_digest: "3".repeat(64),
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
}
