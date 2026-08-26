//! ThreadEvent — immutable journal fact stored in CAS.
//!
//! Events are hash-linked per-chain and per-thread for tamper detection.
//! Once written, an event is immutable.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::thread_snapshot::{parse_canonical_timestamp, validate_canonical_hash};
use super::{SCHEMA_VERSION, validate_object_kind};

/// Maximum canonical JSON size of one thread event stored or emitted by the
/// daemon. This limit is enforced on the complete event object, not only its
/// payload, so every writer shares the same resource ceiling.
pub const MAX_THREAD_EVENT_SERIALIZED_BYTES: usize = 512 * 1024;
/// Exact serialized JSON ceiling for a worker observation batch before the
/// daemon wraps it in a root-chain event.  The reserved 128 KiB covers every
/// bounded root-event identity/hash field and keeps the resulting event below
/// [`MAX_THREAD_EVENT_SERIALIZED_BYTES`].
pub const MAX_STRUCTURED_OBSERVATION_BATCH_BYTES: usize = 384 * 1024;
/// Hard cumulative ceiling for worker events accepted by one hosted session
/// across all of its worker epochs. This bounds root-chain and projection
/// growth independently of the seven-day lifetime ceiling.
pub const MAX_HOSTED_SESSION_OBSERVATION_EVENTS: u64 = 1_048_576;
pub const REMOTE_CONTINUATION_AUTHORITY_SCHEMA: u32 = 2;

/// Typed authority retained on a cross-site `thread_continued` edge.
///
/// These are explicit object edges, not hashes scraped from opaque
/// attestation evidence. They therefore drive transfer completeness,
/// verification, reachability, and GC through the ordinary event braid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteContinuationAuthority {
    pub schema: u32,
    pub operation_id: String,
    pub preflight_id: String,
    pub preflight_attestation_hash: String,
    pub source_chain_head_hash: String,
    pub source_last_event_hash: String,
    pub checkpoint_manifest_hash: String,
    pub target_placement_attestation_hash: String,
    pub chain_writer_grant_hash: String,
    pub target_launch_capsule_hash: String,
    pub target_runtime_seed_hash: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub target_node_signer_fingerprint: String,
    pub successor_thread_id: String,
}

impl RemoteContinuationAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != REMOTE_CONTINUATION_AUTHORITY_SCHEMA {
            anyhow::bail!("remote continuation authority is not the current schema");
        }
        for (label, value) in [
            ("remote continuation operation", self.operation_id.as_str()),
            ("remote continuation preflight", self.preflight_id.as_str()),
            (
                "remote continuation preflight attestation",
                self.preflight_attestation_hash.as_str(),
            ),
            (
                "remote continuation source head",
                self.source_chain_head_hash.as_str(),
            ),
            (
                "remote continuation source event",
                self.source_last_event_hash.as_str(),
            ),
            (
                "remote continuation checkpoint",
                self.checkpoint_manifest_hash.as_str(),
            ),
            (
                "remote continuation placement attestation",
                self.target_placement_attestation_hash.as_str(),
            ),
            (
                "remote continuation writer grant",
                self.chain_writer_grant_hash.as_str(),
            ),
            (
                "remote continuation launch capsule",
                self.target_launch_capsule_hash.as_str(),
            ),
            (
                "remote continuation runtime seed",
                self.target_runtime_seed_hash.as_str(),
            ),
        ] {
            validate_canonical_hash(label, value)?;
        }
        for (label, value) in [
            (
                "remote continuation source site",
                self.source_site_id.as_str(),
            ),
            (
                "remote continuation target site",
                self.target_site_id.as_str(),
            ),
            (
                "remote continuation successor",
                self.successor_thread_id.as_str(),
            ),
        ] {
            if value.is_empty()
                || value.len() > 4096
                || value.trim() != value
                || value.bytes().any(|byte| byte.is_ascii_control())
            {
                anyhow::bail!("{label} is not a bounded canonical label");
            }
        }
        if self.source_site_id == self.target_site_id {
            anyhow::bail!("remote continuation must change current site");
        }
        if self.target_node_signer_fingerprint.len() != 64
            || !self
                .target_node_signer_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            anyhow::bail!("remote continuation target signer is not canonical");
        }
        Ok(())
    }
}

/// Event durability classes.
///
/// | Class      | CAS stored | Projection indexed | Survives crash | Use case                    |
/// |------------|------------|--------------------| ---------------|-----------------------------|
/// | `Durable`  | Yes        | Yes                | Yes            | State changes, artifacts    |
/// | `Journal`  | Yes        | No                 | Yes            | Audit trail, tool calls     |
/// | `Ephemeral`| No         | No                 | No             | Token deltas, streaming logs|
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum EventDurability {
    Durable,
    Journal,
    Ephemeral,
}

impl std::fmt::Display for EventDurability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Durable => write!(f, "durable"),
            Self::Journal => write!(f, "journal"),
            Self::Ephemeral => write!(f, "ephemeral"),
        }
    }
}

impl EventDurability {
    /// Whether this durability class should be stored in CAS.
    pub fn is_cas_stored(&self) -> bool {
        matches!(self, Self::Durable | Self::Journal)
    }

    /// Whether this durability class should be indexed in the projection.
    pub fn is_projection_indexed(&self) -> bool {
        matches!(self, Self::Durable)
    }
}

/// An immutable event in the execution chain.
///
/// Matches the JSON schema from ARCHITECTURE.md §3:
/// ```json
/// {
///   "schema": 1,
///   "kind": "thread_event",
///   "chain_root_id": "T-root",
///   "chain_seq": 7,
///   "thread_id": "T-child",
///   "thread_seq": 2,
///   "event_type": "artifact_published",
///   "durability": "durable",
///   "ts": "2026-04-21T12:34:56Z",
///   "prev_chain_event_hash": "<hash-or-null>",
///   "prev_thread_event_hash": "<hash-or-null>",
///   "payload": { ... }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadEvent {
    pub schema: u32,
    pub kind: String,
    pub chain_root_id: String,
    pub chain_seq: u64,
    pub thread_id: String,
    pub thread_seq: u64,
    pub event_type: String,
    pub durability: EventDurability,
    pub ts: String,
    /// Hash of the previous event in this chain (null for the first event).
    pub prev_chain_event_hash: Option<String>,
    /// Hash of the previous event for this thread (null for the first event in this thread).
    pub prev_thread_event_hash: Option<String>,
    /// Arbitrary event payload as a JSON object.
    pub payload: serde_json::Value,
}

impl ThreadEvent {
    /// Validate this event's invariants.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_object_kind(&self.kind, "thread_event")?;
        if self.schema != SCHEMA_VERSION {
            anyhow::bail!("unexpected schema version: {}", self.schema);
        }
        if self.chain_root_id.is_empty() {
            anyhow::bail!("chain_root_id must not be empty");
        }
        if self.thread_id.is_empty() {
            anyhow::bail!("thread_id must not be empty");
        }
        if self.event_type.is_empty() {
            anyhow::bail!("event_type must not be empty");
        }
        parse_canonical_timestamp(&self.ts)
            .map_err(|error| anyhow::anyhow!("invalid thread event ts: {error}"))?;
        if let Some(hash) = &self.prev_chain_event_hash {
            validate_canonical_hash("prev_chain_event_hash", hash)?;
        }
        if let Some(hash) = &self.prev_thread_event_hash {
            validate_canonical_hash("prev_thread_event_hash", hash)?;
        }
        if self.event_type == "thread_continued"
            && let Some(remote) = self.payload.get("remote_adoption")
        {
            let remote: RemoteContinuationAuthority = serde_json::from_value(remote.clone())
                .context("decode remote continuation authority")?;
            remote.validate()?;
            if self
                .payload
                .get("successor_thread_id")
                .and_then(|value| value.as_str())
                != Some(remote.successor_thread_id.as_str())
            {
                anyhow::bail!("remote continuation successor contradicts its event edge");
            }
            if self.prev_thread_event_hash.as_deref()
                != Some(remote.source_last_event_hash.as_str())
            {
                anyhow::bail!("remote continuation source event is not its thread predecessor");
            }
        }
        let serialized_bytes = lillux::canonical_json(&self.to_value())
            .context("failed to canonicalize thread event")?
            .len();
        if serialized_bytes > MAX_THREAD_EVENT_SERIALIZED_BYTES {
            anyhow::bail!(
                "thread event is {} serialized bytes (max {})",
                serialized_bytes,
                MAX_THREAD_EVENT_SERIALIZED_BYTES
            );
        }
        Ok(())
    }

    /// Convert to a `serde_json::Value` for CAS storage.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ThreadEvent serialization cannot fail")
    }
}

/// Builder for constructing new [`ThreadEvent`] instances.
pub struct NewEvent {
    chain_root_id: String,
    chain_seq: u64,
    thread_id: String,
    thread_seq: u64,
    event_type: String,
    durability: EventDurability,
    prev_chain_event_hash: Option<String>,
    prev_thread_event_hash: Option<String>,
    payload: serde_json::Value,
}

impl NewEvent {
    /// Create a new event builder for a durable event.
    pub fn new(
        chain_root_id: impl Into<String>,
        thread_id: impl Into<String>,
        event_type: impl Into<String>,
    ) -> Self {
        Self {
            chain_root_id: chain_root_id.into(),
            chain_seq: 0,
            thread_id: thread_id.into(),
            thread_seq: 0,
            event_type: event_type.into(),
            durability: EventDurability::Durable,
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: serde_json::Value::Object(Default::default()),
        }
    }

    pub fn chain_seq(mut self, seq: u64) -> Self {
        self.chain_seq = seq;
        self
    }

    pub fn thread_seq(mut self, seq: u64) -> Self {
        self.thread_seq = seq;
        self
    }

    pub fn durability(mut self, durability: EventDurability) -> Self {
        self.durability = durability;
        self
    }

    pub fn prev_chain_event_hash(mut self, hash: Option<String>) -> Self {
        self.prev_chain_event_hash = hash;
        self
    }

    pub fn prev_thread_event_hash(mut self, hash: Option<String>) -> Self {
        self.prev_thread_event_hash = hash;
        self
    }

    pub fn payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// Build the [`ThreadEvent`], setting `ts` to the current UTC time.
    pub fn build(self) -> ThreadEvent {
        ThreadEvent {
            schema: SCHEMA_VERSION,
            kind: "thread_event".to_string(),
            chain_root_id: self.chain_root_id,
            chain_seq: self.chain_seq,
            thread_id: self.thread_id,
            thread_seq: self.thread_seq,
            event_type: self.event_type,
            durability: self.durability,
            ts: lillux::time::iso8601_now(),
            prev_chain_event_hash: self.prev_chain_event_hash,
            prev_thread_event_hash: self.prev_thread_event_hash,
            payload: self.payload,
        }
    }

    /// Build the [`ThreadEvent`] with an explicit timestamp (for testing / replay).
    pub fn build_with_ts(self, ts: String) -> ThreadEvent {
        ThreadEvent {
            schema: SCHEMA_VERSION,
            kind: "thread_event".to_string(),
            chain_root_id: self.chain_root_id,
            chain_seq: self.chain_seq,
            thread_id: self.thread_id,
            thread_seq: self.thread_seq,
            event_type: self.event_type,
            durability: self.durability,
            ts,
            prev_chain_event_hash: self.prev_chain_event_hash,
            prev_thread_event_hash: self.prev_thread_event_hash,
            payload: self.payload,
        }
    }
}

/// Compute the CAS content hash of a [`ThreadEvent`] using canonical JSON.
pub fn hash_event(event: &ThreadEvent) -> Result<String, lillux::CanonicalJsonError> {
    let value = event.to_value();
    let canonical = lillux::canonical_json(&value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

/// Compute the CAS content hash of a [`serde_json::Value`] as a deterministic map.
///
/// `serde_json::Value::Object` uses `BTreeMap` under the hood since serde_json 1.0.85+,
/// but this helper guarantees deterministic ordering by sorting keys explicitly.
pub fn sorted_object_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sorted_object_value(v)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sorted_object_value).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod remote_continuation_tests {
    use super::*;

    fn authority() -> RemoteContinuationAuthority {
        RemoteContinuationAuthority {
            schema: REMOTE_CONTINUATION_AUTHORITY_SCHEMA,
            operation_id: "1".repeat(64),
            preflight_id: "a".repeat(64),
            preflight_attestation_hash: "b".repeat(64),
            source_chain_head_hash: "2".repeat(64),
            source_last_event_hash: "3".repeat(64),
            checkpoint_manifest_hash: "4".repeat(64),
            target_placement_attestation_hash: "5".repeat(64),
            chain_writer_grant_hash: "6".repeat(64),
            target_launch_capsule_hash: "7".repeat(64),
            target_runtime_seed_hash: "9".repeat(64),
            source_site_id: "site:a".into(),
            target_site_id: "site:b".into(),
            target_node_signer_fingerprint: "8".repeat(64),
            successor_thread_id: "T-target".into(),
        }
    }

    #[test]
    fn remote_edge_is_bound_to_its_actual_thread_predecessor_and_successor() {
        let remote = authority();
        let mut event = NewEvent::new("T-root", "T-source", "thread_continued")
            .prev_thread_event_hash(Some(remote.source_last_event_hash.clone()))
            .payload(serde_json::json!({
                "successor_thread_id":"T-target",
                "reason":"remote_adoption",
                "remote_adoption":remote,
            }))
            .build();
        event.validate().unwrap();
        event.payload["successor_thread_id"] = serde_json::json!("T-other");
        assert!(event.validate().is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_builder_defaults() {
        let event = NewEvent::new("chain-1", "thread-1", "test_event").build();
        assert_eq!(event.schema, SCHEMA_VERSION);
        assert_eq!(event.kind, "thread_event");
        assert_eq!(event.chain_root_id, "chain-1");
        assert_eq!(event.thread_id, "thread-1");
        assert_eq!(event.event_type, "test_event");
        assert_eq!(event.durability, EventDurability::Durable);
        assert_eq!(event.chain_seq, 0);
        assert_eq!(event.thread_seq, 0);
        assert!(event.prev_chain_event_hash.is_none());
        assert!(event.prev_thread_event_hash.is_none());
        assert!(!event.ts.is_empty());
    }

    #[test]
    fn event_builder_fluent() {
        let event = NewEvent::new("chain-1", "thread-1", "tool_call")
            .chain_seq(5)
            .thread_seq(3)
            .durability(EventDurability::Journal)
            .prev_chain_event_hash(Some("a".repeat(64)))
            .prev_thread_event_hash(Some("b".repeat(64)))
            .payload(serde_json::json!({"tool": "bash", "cmd": "ls"}))
            .build_with_ts("2026-04-21T12:34:56Z".to_string());

        assert_eq!(event.chain_seq, 5);
        assert_eq!(event.thread_seq, 3);
        assert_eq!(event.durability, EventDurability::Journal);
        assert_eq!(event.prev_chain_event_hash, Some("a".repeat(64)));
        assert_eq!(event.prev_thread_event_hash, Some("b".repeat(64)));
        assert_eq!(event.payload["tool"], "bash");
        assert_eq!(event.ts, "2026-04-21T12:34:56Z");
    }

    #[test]
    fn event_validation_passes() {
        let event = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        assert!(event.validate().is_ok());
    }

    #[test]
    fn event_validation_rejects_bad_kind() {
        let mut event = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        event.kind = "wrong_kind".to_string();
        assert!(event.validate().is_err());
    }

    #[test]
    fn event_validation_rejects_bad_schema() {
        let mut event = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        event.schema = 99;
        assert!(event.validate().is_err());
    }

    #[test]
    fn event_validation_rejects_empty_chain_root_id() {
        let mut event = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        event.chain_root_id = String::new();
        assert!(event.validate().is_err());
    }

    #[test]
    fn event_validation_rejects_empty_thread_id() {
        let mut event = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        event.thread_id = String::new();
        assert!(event.validate().is_err());
    }

    #[test]
    fn event_validation_rejects_empty_event_type() {
        let mut event = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        event.event_type = String::new();
        assert!(event.validate().is_err());
    }

    #[test]
    fn event_validation_rejects_invalid_prev_hash() {
        let event = NewEvent::new("chain-1", "thread-1", "test")
            .prev_chain_event_hash(Some("not-a-valid-hash".to_string()))
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        assert!(event.validate().is_err());
    }

    #[test]
    fn event_validation_rejects_noncanonical_timestamp_and_uppercase_link() {
        let fractional = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56.000Z".to_string());
        assert!(fractional.validate().is_err());

        let uppercase = NewEvent::new("chain-1", "thread-1", "test")
            .prev_thread_event_hash(Some("AA".repeat(32)))
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        assert!(uppercase.validate().is_err());
    }

    #[test]
    fn event_serialization_roundtrip() {
        let event = NewEvent::new("chain-1", "thread-1", "tool_call")
            .chain_seq(7)
            .thread_seq(2)
            .durability(EventDurability::Durable)
            .payload(serde_json::json!({"key": "value"}))
            .build_with_ts("2026-04-21T12:34:56Z".to_string());

        let json = serde_json::to_value(&event).unwrap();
        let deserialized: ThreadEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event.chain_root_id, deserialized.chain_root_id);
        assert_eq!(event.chain_seq, deserialized.chain_seq);
        assert_eq!(event.thread_id, deserialized.thread_id);
        assert_eq!(event.thread_seq, deserialized.thread_seq);
        assert_eq!(event.event_type, deserialized.event_type);
        assert_eq!(event.durability, deserialized.durability);
        assert_eq!(event.ts, deserialized.ts);
        assert_eq!(event.payload, deserialized.payload);
    }

    #[test]
    fn event_canonical_json_determinism() {
        let event = NewEvent::new("chain-1", "thread-1", "test")
            .chain_seq(1)
            .thread_seq(1)
            .payload(serde_json::json!({"z": 1, "a": 2, "m": 3}))
            .build_with_ts("2026-04-21T12:34:56Z".to_string());

        let hash1 = hash_event(&event).unwrap();
        let hash2 = hash_event(&event).unwrap();
        assert_eq!(hash1, hash2, "canonical JSON must be deterministic");
        assert!(lillux::valid_hash(&hash1), "hash must be 64 hex chars");
    }

    #[test]
    fn event_different_payloads_produce_different_hashes() {
        let event_a = NewEvent::new("chain-1", "thread-1", "test")
            .chain_seq(1)
            .thread_seq(1)
            .payload(serde_json::json!({"key": "value_a"}))
            .build_with_ts("2026-04-21T12:34:56Z".to_string());

        let event_b = NewEvent::new("chain-1", "thread-1", "test")
            .chain_seq(1)
            .thread_seq(1)
            .payload(serde_json::json!({"key": "value_b"}))
            .build_with_ts("2026-04-21T12:34:56Z".to_string());

        assert_ne!(hash_event(&event_a).unwrap(), hash_event(&event_b).unwrap());
    }

    #[test]
    fn durability_cas_stored() {
        assert!(EventDurability::Durable.is_cas_stored());
        assert!(EventDurability::Journal.is_cas_stored());
        assert!(!EventDurability::Ephemeral.is_cas_stored());
    }

    #[test]
    fn durability_projection_indexed() {
        assert!(EventDurability::Durable.is_projection_indexed());
        assert!(!EventDurability::Journal.is_projection_indexed());
        assert!(!EventDurability::Ephemeral.is_projection_indexed());
    }

    #[test]
    fn durability_serde_roundtrip() {
        for d in [
            EventDurability::Durable,
            EventDurability::Journal,
            EventDurability::Ephemeral,
        ] {
            let json = serde_json::to_value(d).unwrap();
            let back: EventDurability = serde_json::from_value(json).unwrap();
            assert_eq!(d, back);
        }
    }

    #[test]
    fn to_value_returns_valid_json() {
        let event = NewEvent::new("chain-1", "thread-1", "test")
            .build_with_ts("2026-04-21T12:34:56Z".to_string());
        let value = event.to_value();
        assert!(value.is_object());
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["kind"], "thread_event");
    }
}
