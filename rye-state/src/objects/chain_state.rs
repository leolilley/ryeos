//! ChainState — authoritative root per execution chain.
//!
//! Each chain has one current ChainState. New versions are created on
//! every state mutation. The chain_state is hash-linked to its previous
//! version for tamper detection.
//!
//! Uses `BTreeMap` for the threads map to ensure deterministic serialization,
//! which is required for CAS content-addressing.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::{validate_object_kind, SCHEMA_VERSION};
use crate::objects::thread_snapshot::ThreadStatus;

/// Entry for a single thread within a ChainState.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainThreadEntry {
    /// CAS hash of the thread's latest snapshot.
    pub snapshot_hash: String,
    /// CAS hash of the thread's last event.
    pub last_event_hash: Option<String>,
    /// Last thread sequence number.
    pub last_thread_seq: u64,
    /// Current thread status.
    pub status: ThreadStatus,
}

impl ChainThreadEntry {
    /// Validate this entry's invariants.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.snapshot_hash.is_empty() {
            anyhow::bail!("snapshot_hash must not be empty");
        }
        if !lillux::valid_hash(&self.snapshot_hash) {
            anyhow::bail!("invalid snapshot_hash: {}", self.snapshot_hash);
        }
        if let Some(hash) = &self.last_event_hash {
            if !lillux::valid_hash(hash) {
                anyhow::bail!("invalid last_event_hash: {hash}");
            }
        }
        Ok(())
    }
}

/// Authoritative state root for an entire execution chain.
///
/// Matches the JSON schema from ARCHITECTURE.md §3:
/// ```json
/// {
///   "schema": 1,
///   "kind": "chain_state",
///   "chain_root_id": "T-root",
///   "prev_chain_state_hash": "<hash-or-null>",
///   "last_event_hash": "<hash>",
///   "last_chain_seq": 17,
///   "updated_at": "...",
///   "threads": {
///     "T-root": {
///       "snapshot_hash": "<hash>",
///       "last_event_hash": "<hash>",
///       "last_thread_seq": 9,
///       "status": "continued"
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainState {
    pub schema: u32,
    pub kind: String,
    pub chain_root_id: String,
    /// Hash of the previous ChainState version (null for the first).
    pub prev_chain_state_hash: Option<String>,
    /// Hash of the last event in this chain.
    pub last_event_hash: Option<String>,
    /// Last chain sequence number.
    pub last_chain_seq: u64,
    pub updated_at: String,
    /// Map of thread_id → thread entry. BTreeMap for deterministic serialization.
    #[serde(with = "btreemap_serde")]
    pub threads: BTreeMap<String, ChainThreadEntry>,
}

mod btreemap_serde {
    use std::collections::BTreeMap;

    use serde::{Deserializer, Serializer};

    use super::ChainThreadEntry;

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<String, ChainThreadEntry>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut seq = s.serialize_map(Some(map.len()))?;
        for (k, v) in map {
            seq.serialize_entry(k, v)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, ChainThreadEntry>, D::Error> {
        let map: std::collections::HashMap<String, ChainThreadEntry> =
            serde::Deserialize::deserialize(d)?;
        Ok(map.into_iter().collect())
    }
}

/// Size thresholds for chain state (ARCHITECTURE.md §3).
pub struct ChainStateThresholds;

impl ChainStateThresholds {
    /// Green: < 200 threads, < 256 KB — normal operation.
    pub const MAX_GREEN_THREADS: usize = 200;
    pub const MAX_GREEN_BYTES: usize = 256 * 1024;

    /// Yellow: 200–500 threads, 256 KB–1 MB — emit warning metrics.
    pub const MAX_YELLOW_THREADS: usize = 500;
    pub const MAX_YELLOW_BYTES: usize = 1024 * 1024;

    /// Red: > 500 threads, > 1 MB — trigger Merkleized thread map.
    pub const RED_THREAD_LIMIT: usize = 500;
    pub const RED_BYTE_LIMIT: usize = 1024 * 1024;
}

impl ChainState {
    /// Validate this chain state's invariants.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_object_kind(&self.kind, "chain_state")?;
        if self.schema != SCHEMA_VERSION {
            anyhow::bail!("unexpected schema version: {}", self.schema);
        }
        if self.chain_root_id.is_empty() {
            anyhow::bail!("chain_root_id must not be empty");
        }
        if let Some(hash) = &self.prev_chain_state_hash {
            if !lillux::valid_hash(hash) {
                anyhow::bail!("invalid prev_chain_state_hash: {hash}");
            }
        }
        if let Some(hash) = &self.last_event_hash {
            if !lillux::valid_hash(hash) {
                anyhow::bail!("invalid last_event_hash: {hash}");
            }
        }
        if self.updated_at.is_empty() {
            anyhow::bail!("updated_at must not be empty");
        }

        // Validate all thread entries
        for (thread_id, entry) in &self.threads {
            if thread_id.is_empty() {
                anyhow::bail!("thread_id in threads map must not be empty");
            }
            entry
                .validate()
                .with_context(|| format!("invalid thread entry for '{thread_id}'"))?;
        }

        // chain_root_id must exist in threads map
        if !self.threads.contains_key(&self.chain_root_id) {
            anyhow::bail!(
                "chain_root_id '{}' must exist in threads map",
                self.chain_root_id
            );
        }

        Ok(())
    }

    /// Estimate the serialized size in bytes for threshold checks.
    ///
    /// Uses canonical JSON to get a deterministic size estimate.
    pub fn estimated_size_bytes(&self) -> usize {
        let value = self.to_value();
        let canonical = lillux::canonical_json(&value);
        canonical.len()
    }

    /// Check if this chain state is in the green zone (normal operation).
    pub fn is_green(&self) -> bool {
        self.threads.len() < ChainStateThresholds::MAX_GREEN_THREADS
            && self.estimated_size_bytes() < ChainStateThresholds::MAX_GREEN_BYTES
    }

    /// Check if this chain state is in the yellow zone (warning metrics).
    pub fn is_yellow(&self) -> bool {
        self.threads.len() >= ChainStateThresholds::MAX_GREEN_THREADS
            && self.threads.len() < ChainStateThresholds::MAX_YELLOW_THREADS
            && self.estimated_size_bytes() < ChainStateThresholds::MAX_YELLOW_BYTES
    }

    /// Check if this chain state is in the red zone (needs Merkleization).
    pub fn is_red(&self) -> bool {
        self.threads.len() >= ChainStateThresholds::RED_THREAD_LIMIT
            || self.estimated_size_bytes() >= ChainStateThresholds::RED_BYTE_LIMIT
    }

    /// Convert to a `serde_json::Value` for CAS storage.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ChainState serialization cannot fail")
    }
}

/// Compute the CAS content hash of a [`ChainState`] using canonical JSON.
pub fn hash_chain_state(state: &ChainState) -> String {
    let value = state.to_value();
    let canonical = lillux::canonical_json(&value);
    lillux::sha256_hex(canonical.as_bytes())
}

/// Builder for constructing [`ChainState`] instances.
pub struct ChainStateBuilder {
    chain_root_id: String,
    prev_chain_state_hash: Option<String>,
    last_event_hash: Option<String>,
    last_chain_seq: u64,
    updated_at: Option<String>,
    threads: BTreeMap<String, ChainThreadEntry>,
}

impl ChainStateBuilder {
    /// Start building a new chain state.
    pub fn new(chain_root_id: impl Into<String>) -> Self {
        Self {
            chain_root_id: chain_root_id.into(),
            prev_chain_state_hash: None,
            last_event_hash: None,
            last_chain_seq: 0,
            updated_at: None,
            threads: BTreeMap::new(),
        }
    }

    pub fn prev_chain_state_hash(mut self, hash: Option<String>) -> Self {
        self.prev_chain_state_hash = hash;
        self
    }

    pub fn last_event_hash(mut self, hash: Option<String>) -> Self {
        self.last_event_hash = hash;
        self
    }

    pub fn last_chain_seq(mut self, seq: u64) -> Self {
        self.last_chain_seq = seq;
        self
    }

    pub fn updated_at(mut self, ts: String) -> Self {
        self.updated_at = Some(ts);
        self
    }

    /// Insert or update a thread entry.
    pub fn thread(mut self, thread_id: impl Into<String>, entry: ChainThreadEntry) -> Self {
        self.threads.insert(thread_id.into(), entry);
        self
    }

    /// Set all threads at once.
    pub fn threads(mut self, threads: BTreeMap<String, ChainThreadEntry>) -> Self {
        self.threads = threads;
        self
    }

    /// Build the [`ChainState`].
    pub fn build(self) -> ChainState {
        ChainState {
            schema: SCHEMA_VERSION,
            kind: "chain_state".to_string(),
            chain_root_id: self.chain_root_id,
            prev_chain_state_hash: self.prev_chain_state_hash,
            last_event_hash: self.last_event_hash,
            last_chain_seq: self.last_chain_seq,
            updated_at: self
                .updated_at
                .unwrap_or_else(|| lillux::time::iso8601_now()),
            threads: self.threads,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_thread_entry(hash_suffix: &str) -> ChainThreadEntry {
        // Produce a valid 64-char hex hash
        let suffix_len = hash_suffix.len().min(64);
        let snapshot_hash = format!("{}{}", "a".repeat(64 - suffix_len), &hash_suffix[..suffix_len]);
        let last_event_hash = format!("{}{}", "b".repeat(64 - suffix_len), &hash_suffix[..suffix_len]);
        ChainThreadEntry {
            snapshot_hash,
            last_event_hash: Some(last_event_hash),
            last_thread_seq: 1,
            status: ThreadStatus::Running,
        }
    }

    #[test]
    fn chain_state_builder_basic() {
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .build();

        assert_eq!(state.schema, SCHEMA_VERSION);
        assert_eq!(state.kind, "chain_state");
        assert_eq!(state.chain_root_id, "T-root");
        assert!(state.prev_chain_state_hash.is_none());
        assert_eq!(state.last_chain_seq, 0);
        assert_eq!(state.threads.len(), 1);
        assert!(state.threads.contains_key("T-root"));
    }

    #[test]
    fn chain_state_builder_with_prev_hash() {
        let prev_hash = "0".repeat(64);
        let state = ChainStateBuilder::new("T-root")
            .prev_chain_state_hash(Some(prev_hash.clone()))
            .last_chain_seq(5)
            .last_event_hash(Some("1".repeat(64)))
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .build();

        assert_eq!(state.prev_chain_state_hash, Some(prev_hash));
        assert_eq!(state.last_chain_seq, 5);
        assert_eq!(state.last_event_hash, Some("1".repeat(64)));
    }

    #[test]
    fn chain_state_validation_passes() {
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .build();

        assert!(state.validate().is_ok());
    }

    #[test]
    fn chain_state_validation_rejects_bad_kind() {
        let mut state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .build();
        state.kind = "wrong".to_string();
        assert!(state.validate().is_err());
    }

    #[test]
    fn chain_state_validation_rejects_root_not_in_threads() {
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-other", make_thread_entry("01"))
            .build();

        assert!(state.validate().is_err());
    }

    #[test]
    fn chain_state_validation_rejects_invalid_prev_hash() {
        let state = ChainStateBuilder::new("T-root")
            .prev_chain_state_hash(Some("not-a-hash".to_string()))
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .build();

        assert!(state.validate().is_err());
    }

    #[test]
    fn chain_state_validation_rejects_invalid_thread_snapshot_hash() {
        let entry = ChainThreadEntry {
            snapshot_hash: "bad-hash".to_string(),
            last_event_hash: None,
            last_thread_seq: 0,
            status: ThreadStatus::Created,
        };
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", entry)
            .build();

        assert!(state.validate().is_err());
    }

    #[test]
    fn chain_state_serialization_roundtrip() {
        let state = ChainStateBuilder::new("T-root")
            .last_chain_seq(10)
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .thread("T-child", make_thread_entry("02"))
            .build();

        let json = serde_json::to_value(&state).unwrap();
        let deserialized: ChainState = serde_json::from_value(json).unwrap();
        assert_eq!(state.chain_root_id, deserialized.chain_root_id);
        assert_eq!(state.last_chain_seq, deserialized.last_chain_seq);
        assert_eq!(state.threads.len(), deserialized.threads.len());
        assert!(deserialized.threads.contains_key("T-root"));
        assert!(deserialized.threads.contains_key("T-child"));
    }

    #[test]
    fn chain_state_canonical_json_determinism() {
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .thread("T-child", make_thread_entry("02"))
            .thread("T-mid", make_thread_entry("03"))
            .build();

        let hash1 = hash_chain_state(&state);
        let hash2 = hash_chain_state(&state);
        assert_eq!(hash1, hash2, "canonical JSON must be deterministic");
        assert!(lillux::valid_hash(&hash1));
    }

    #[test]
    fn chain_state_threads_are_sorted() {
        // Threads added in non-alphabetical order should serialize in sorted order
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-z", make_thread_entry("01"))
            .thread("T-a", make_thread_entry("02"))
            .thread("T-root", make_thread_entry("03"))
            .build();

        let value = state.to_value();
        let canonical = lillux::canonical_json(&value);
        // Check that "T-a" key appears before "T-z" key in canonical JSON
        // Use quoted keys to avoid substring matches
        let pos_a = canonical.find("\"T-a\"").unwrap();
        let pos_z = canonical.find("\"T-z\"").unwrap();
        assert!(
            pos_a < pos_z,
            "T-a should appear before T-z in canonical JSON"
        );
    }

    #[test]
    fn chain_state_estimated_size_bytes() {
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .build();

        let size = state.estimated_size_bytes();
        assert!(size > 0, "estimated size should be positive");
    }

    #[test]
    fn chain_state_thresholds_green() {
        let state = ChainStateBuilder::new("T-root")
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .thread("T-root", make_thread_entry("01"))
            .build();

        assert!(state.is_green());
        assert!(!state.is_yellow());
        assert!(!state.is_red());
    }

    #[test]
    fn chain_thread_entry_validation_passes() {
        let entry = make_thread_entry("01");
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn chain_thread_entry_validation_rejects_empty_hash() {
        let entry = ChainThreadEntry {
            snapshot_hash: String::new(),
            last_event_hash: None,
            last_thread_seq: 0,
            status: ThreadStatus::Created,
        };
        assert!(entry.validate().is_err());
    }

    #[test]
    fn chain_thread_entry_validation_rejects_invalid_hash() {
        let entry = ChainThreadEntry {
            snapshot_hash: "bad".to_string(),
            last_event_hash: None,
            last_thread_seq: 0,
            status: ThreadStatus::Created,
        };
        assert!(entry.validate().is_err());
    }

    #[test]
    fn chain_thread_entry_validation_rejects_invalid_event_hash() {
        let entry = ChainThreadEntry {
            snapshot_hash: "a".repeat(64),
            last_event_hash: Some("bad".to_string()),
            last_thread_seq: 0,
            status: ThreadStatus::Created,
        };
        assert!(entry.validate().is_err());
    }

    #[test]
    fn chain_state_multiple_threads_serialization() {
        let mut threads = BTreeMap::new();
        threads.insert(
            "T-root".to_string(),
            ChainThreadEntry {
                snapshot_hash: "a".repeat(64),
                last_event_hash: Some("b".repeat(64)),
                last_thread_seq: 9,
                status: ThreadStatus::Continued,
            },
        );
        threads.insert(
            "T-child".to_string(),
            ChainThreadEntry {
                snapshot_hash: "c".repeat(64),
                last_event_hash: Some("d".repeat(64)),
                last_thread_seq: 8,
                status: ThreadStatus::Running,
            },
        );

        let state = ChainStateBuilder::new("T-root")
            .threads(threads)
            .last_chain_seq(17)
            .last_event_hash(Some("e".repeat(64)))
            .updated_at("2026-04-21T12:00:00Z".to_string())
            .build();

        assert_eq!(state.threads["T-root"].status, ThreadStatus::Continued);
        assert_eq!(state.threads["T-root"].last_thread_seq, 9);
        assert_eq!(state.threads["T-child"].status, ThreadStatus::Running);
        assert_eq!(state.threads["T-child"].last_thread_seq, 8);
        assert_eq!(state.last_chain_seq, 17);
    }
}
