//! ThreadSnapshot — current durable state of one thread.
//!
//! A new snapshot is created on every state transition. Previous snapshots
//! remain in CAS (immutable). The chain_state points to the latest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{validate_object_kind, SCHEMA_VERSION};

/// Thread status enum — must match the CHECK constraint in db.rs exactly:
/// created, running, completed, failed, cancelled, killed, timed_out, continued
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Created,
    Running,
    Completed,
    Failed,
    Cancelled,
    Killed,
    TimedOut,
    Continued,
}

impl ThreadStatus {
    /// Convert to the string representation used in the database CHECK constraint.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
            Self::Continued => "continued",
        }
    }

    /// Parse from the database string representation.
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "killed" => Some(Self::Killed),
            "timed_out" => Some(Self::TimedOut),
            "continued" => Some(Self::Continued),
            _ => None,
        }
    }

    /// Whether this is a terminal status (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Killed | Self::TimedOut | Self::Continued
        )
    }
}

impl std::fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Current durable state of one thread.
///
/// Matches the JSON schema from ARCHITECTURE.md §3:
/// ```json
/// {
///   "schema": 1,
///   "kind": "thread_snapshot",
///   "thread_id": "T-child",
///   "chain_root_id": "T-root",
///   "status": "running",
///   "kind_name": "agent",
///   "item_ref": "directive:foo/bar",
///   "executor_ref": "native:directive-runtime",
///   "launch_mode": "inline",
///   "current_site_id": "site:host",
///   "origin_site_id": "site:host",
///   "upstream_thread_id": "T-root",
///   "requested_by": "user:alice",
///   "created_at": "...",
///   "updated_at": "...",
///   "started_at": "...",
///   "finished_at": null,
///   "result": null,
///   "error": null,
///   "budget": { ... },
///   "artifacts": [ ... ],
///   "facets": { ... },
///   "last_event_hash": "<hash>",
///   "last_chain_seq": 7,
///   "last_thread_seq": 2
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub schema: u32,
    pub kind: String,
    pub thread_id: String,
    pub chain_root_id: String,
    pub status: ThreadStatus,
    /// The thread kind (e.g. "agent", "tool", "graph").
    pub kind_name: String,
    /// The item reference being executed (e.g. "directive:foo/bar").
    pub item_ref: String,
    /// The executor reference (e.g. "native:directive-runtime").
    pub executor_ref: String,
    /// Launch mode: "inline" or "detached".
    pub launch_mode: String,
    /// Current execution site (e.g. "site:host").
    pub current_site_id: String,
    /// Origin site where the thread was created.
    pub origin_site_id: String,
    /// The thread that spawned or continued this thread (if any).
    pub upstream_thread_id: Option<String>,
    /// Who requested the execution (e.g. "user:alice").
    pub requested_by: Option<String>,
    /// The project snapshot hash at the start of execution.
    /// Set when execution begins against a specific project state.
    /// Immutable for this thread. Null for non-CS executions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_project_snapshot_hash: Option<String>,
    /// The project snapshot hash after fold-back.
    /// Set on finalization if the working directory changed.
    /// Null if no changes or not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_project_snapshot_hash: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Final result payload (set on terminal snapshots).
    pub result: Option<serde_json::Value>,
    /// Error payload (set on failed snapshots).
    pub error: Option<serde_json::Value>,
    /// Budget information.
    pub budget: Option<serde_json::Value>,
    /// Published artifacts.
    pub artifacts: Vec<serde_json::Value>,
    /// Key-value facets (e.g. cost annotations). Uses BTreeMap for deterministic serialization.
    #[serde(
        serialize_with = "serialize_btreemap",
        deserialize_with = "deserialize_btreemap"
    )]
    pub facets: BTreeMap<String, String>,
    /// Hash of the last event in this thread.
    pub last_event_hash: Option<String>,
    /// Last chain sequence number at the time of this snapshot.
    pub last_chain_seq: u64,
    /// Last thread sequence number at the time of this snapshot.
    pub last_thread_seq: u64,
}

fn serialize_btreemap<S: serde::Serializer>(
    map: &BTreeMap<String, String>,
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut seq = s.serialize_map(Some(map.len()))?;
    for (k, v) in map {
        seq.serialize_entry(k, v)?;
    }
    seq.end()
}

fn deserialize_btreemap<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let map: std::collections::HashMap<String, String> =
        serde::Deserialize::deserialize(d)?;
    Ok(map.into_iter().collect())
}

impl ThreadSnapshot {
    /// Validate this snapshot's invariants.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_object_kind(&self.kind, "thread_snapshot")?;
        if self.schema != SCHEMA_VERSION {
            anyhow::bail!("unexpected schema version: {}", self.schema);
        }
        if self.thread_id.is_empty() {
            anyhow::bail!("thread_id must not be empty");
        }
        if self.chain_root_id.is_empty() {
            anyhow::bail!("chain_root_id must not be empty");
        }
        if self.kind_name.is_empty() {
            anyhow::bail!("kind_name must not be empty");
        }
        if self.item_ref.is_empty() {
            anyhow::bail!("item_ref must not be empty");
        }
        if self.executor_ref.is_empty() {
            anyhow::bail!("executor_ref must not be empty");
        }
        if !matches!(self.launch_mode.as_str(), "inline" | "detached") {
            anyhow::bail!(
                "invalid launch_mode: '{}' (expected 'inline' or 'detached')",
                self.launch_mode
            );
        }
        if self.current_site_id.is_empty() {
            anyhow::bail!("current_site_id must not be empty");
        }
        if self.origin_site_id.is_empty() {
            anyhow::bail!("origin_site_id must not be empty");
        }
        if self.created_at.is_empty() {
            anyhow::bail!("created_at must not be empty");
        }
        if self.updated_at.is_empty() {
            anyhow::bail!("updated_at must not be empty");
        }
        if let Some(hash) = &self.last_event_hash {
            if !lillux::valid_hash(hash) {
                anyhow::bail!("invalid last_event_hash: {hash}");
            }
        }
        Ok(())
    }

    /// Convert to a `serde_json::Value` for CAS storage.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ThreadSnapshot serialization cannot fail")
    }
}

/// Compute the CAS content hash of a [`ThreadSnapshot`] using canonical JSON.
pub fn hash_snapshot(snapshot: &ThreadSnapshot) -> String {
    let value = snapshot.to_value();
    let canonical = lillux::canonical_json(&value);
    lillux::sha256_hex(canonical.as_bytes())
}

/// Fluent builder for constructing [`ThreadSnapshot`] instances.
pub struct ThreadSnapshotBuilder {
    thread_id: String,
    chain_root_id: String,
    status: ThreadStatus,
    kind_name: String,
    item_ref: String,
    executor_ref: String,
    launch_mode: String,
    current_site_id: String,
    origin_site_id: String,
    upstream_thread_id: Option<String>,
    requested_by: Option<String>,
    base_project_snapshot_hash: Option<String>,
    result_project_snapshot_hash: Option<String>,
    created_at: String,
    updated_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
    budget: Option<serde_json::Value>,
    artifacts: Vec<serde_json::Value>,
    facets: BTreeMap<String, String>,
    last_event_hash: Option<String>,
    last_chain_seq: u64,
    last_thread_seq: u64,
}

impl ThreadSnapshotBuilder {
    /// Start building a new thread snapshot with required fields.
    pub fn new(
        thread_id: impl Into<String>,
        chain_root_id: impl Into<String>,
        kind_name: impl Into<String>,
        item_ref: impl Into<String>,
        executor_ref: impl Into<String>,
    ) -> Self {
        let now = lillux::time::iso8601_now();
        Self {
            thread_id: thread_id.into(),
            chain_root_id: chain_root_id.into(),
            status: ThreadStatus::Created,
            kind_name: kind_name.into(),
            item_ref: item_ref.into(),
            executor_ref: executor_ref.into(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:host".to_string(),
            origin_site_id: "site:host".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            base_project_snapshot_hash: None,
            result_project_snapshot_hash: None,
            created_at: now.clone(),
            updated_at: now,
            started_at: None,
            finished_at: None,
            result: None,
            error: None,
            budget: None,
            artifacts: Vec::new(),
            facets: BTreeMap::new(),
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        }
    }

    pub fn status(mut self, status: ThreadStatus) -> Self {
        self.status = status;
        self
    }

    pub fn launch_mode(mut self, mode: impl Into<String>) -> Self {
        self.launch_mode = mode.into();
        self
    }

    pub fn current_site_id(mut self, site: impl Into<String>) -> Self {
        self.current_site_id = site.into();
        self
    }

    pub fn origin_site_id(mut self, site: impl Into<String>) -> Self {
        self.origin_site_id = site.into();
        self
    }

    pub fn upstream_thread_id(mut self, id: Option<String>) -> Self {
        self.upstream_thread_id = id;
        self
    }

    pub fn requested_by(mut self, who: Option<String>) -> Self {
        self.requested_by = who;
        self
    }

    pub fn base_project_snapshot_hash(mut self, hash: impl Into<String>) -> Self {
        self.base_project_snapshot_hash = Some(hash.into());
        self
    }

    pub fn result_project_snapshot_hash(mut self, hash: impl Into<String>) -> Self {
        self.result_project_snapshot_hash = Some(hash.into());
        self
    }

    pub fn started_at(mut self, ts: Option<String>) -> Self {
        self.started_at = ts;
        self
    }

    pub fn finished_at(mut self, ts: Option<String>) -> Self {
        self.finished_at = ts;
        self
    }

    pub fn result(mut self, result: Option<serde_json::Value>) -> Self {
        self.result = result;
        self
    }

    pub fn error(mut self, error: Option<serde_json::Value>) -> Self {
        self.error = error;
        self
    }

    pub fn budget(mut self, budget: Option<serde_json::Value>) -> Self {
        self.budget = budget;
        self
    }

    pub fn artifacts(mut self, artifacts: Vec<serde_json::Value>) -> Self {
        self.artifacts = artifacts;
        self
    }

    pub fn facets(mut self, facets: BTreeMap<String, String>) -> Self {
        self.facets = facets;
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

    pub fn last_thread_seq(mut self, seq: u64) -> Self {
        self.last_thread_seq = seq;
        self
    }

    pub fn created_at(mut self, ts: String) -> Self {
        self.created_at = ts;
        self
    }

    pub fn updated_at(mut self, ts: String) -> Self {
        self.updated_at = ts;
        self
    }

    /// Build the [`ThreadSnapshot`].
    pub fn build(self) -> ThreadSnapshot {
        ThreadSnapshot {
            schema: SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: self.thread_id,
            chain_root_id: self.chain_root_id,
            status: self.status,
            kind_name: self.kind_name,
            item_ref: self.item_ref,
            executor_ref: self.executor_ref,
            launch_mode: self.launch_mode,
            current_site_id: self.current_site_id,
            origin_site_id: self.origin_site_id,
            upstream_thread_id: self.upstream_thread_id,
            requested_by: self.requested_by,
            base_project_snapshot_hash: self.base_project_snapshot_hash,
            result_project_snapshot_hash: self.result_project_snapshot_hash,
            created_at: self.created_at,
            updated_at: self.updated_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            result: self.result,
            error: self.error,
            budget: self.budget,
            artifacts: self.artifacts,
            facets: self.facets,
            last_event_hash: self.last_event_hash,
            last_chain_seq: self.last_chain_seq,
            last_thread_seq: self.last_thread_seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_builder_defaults() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();

        assert_eq!(snap.schema, SCHEMA_VERSION);
        assert_eq!(snap.kind, "thread_snapshot");
        assert_eq!(snap.thread_id, "T-1");
        assert_eq!(snap.chain_root_id, "T-root");
        assert_eq!(snap.status, ThreadStatus::Created);
        assert_eq!(snap.launch_mode, "inline");
        assert_eq!(snap.current_site_id, "site:host");
        assert!(snap.upstream_thread_id.is_none());
        assert!(snap.requested_by.is_none());
        assert!(snap.started_at.is_none());
        assert!(snap.finished_at.is_none());
        assert!(snap.result.is_none());
        assert!(snap.error.is_none());
        assert!(snap.budget.is_none());
        assert!(snap.artifacts.is_empty());
        assert!(snap.facets.is_empty());
        assert!(snap.last_event_hash.is_none());
        assert_eq!(snap.last_chain_seq, 0);
        assert_eq!(snap.last_thread_seq, 0);
    }

    #[test]
    fn snapshot_builder_fluent() {
        let mut facets = BTreeMap::new();
        facets.insert("cost.spend".to_string(), "0.12".to_string());
        facets.insert("cost.tokens".to_string(), "1500".to_string());

        let snap = ThreadSnapshotBuilder::new(
            "T-child", "T-root", "agent", "directive:foo/bar", "native:directive-runtime",
        )
        .status(ThreadStatus::Running)
        .launch_mode("detached")
        .upstream_thread_id(Some("T-root".to_string()))
        .requested_by(Some("user:alice".to_string()))
        .started_at(Some("2026-04-21T12:00:01Z".to_string()))
        .finished_at(Some("2026-04-21T12:05:00Z".to_string()))
        .result(Some(serde_json::json!({"output": "done"})))
        .facets(facets)
        .last_chain_seq(7)
        .last_thread_seq(3)
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:05:00Z".to_string())
        .build();

        assert_eq!(snap.status, ThreadStatus::Running);
        assert_eq!(snap.launch_mode, "detached");
        assert_eq!(snap.upstream_thread_id.as_deref(), Some("T-root"));
        assert_eq!(snap.requested_by.as_deref(), Some("user:alice"));
        assert_eq!(snap.facets.get("cost.spend").unwrap(), "0.12");
        assert_eq!(snap.facets.get("cost.tokens").unwrap(), "1500");
        assert_eq!(snap.last_chain_seq, 7);
        assert_eq!(snap.last_thread_seq, 3);
    }

    #[test]
    fn snapshot_validation_passes() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_ok());
    }

    #[test]
    fn snapshot_validation_rejects_bad_kind() {
        let mut snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        snap.kind = "wrong".to_string();
        assert!(snap.validate().is_err());
    }

    #[test]
    fn snapshot_validation_rejects_invalid_launch_mode() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .launch_mode("invalid_mode")
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_err());
    }

    #[test]
    fn snapshot_validation_rejects_invalid_event_hash() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .last_event_hash(Some("bad-hash".to_string()))
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_err());
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .status(ThreadStatus::Failed)
        .error(Some(serde_json::json!({"message": "oom"})))
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:05:00Z".to_string())
        .build();

        let json = serde_json::to_value(&snap).unwrap();
        let deserialized: ThreadSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.thread_id, deserialized.thread_id);
        assert_eq!(snap.status, deserialized.status);
        assert_eq!(snap.error, deserialized.error);
    }

    #[test]
    fn snapshot_canonical_json_determinism() {
        let mut facets = BTreeMap::new();
        facets.insert("z_key".to_string(), "3".to_string());
        facets.insert("a_key".to_string(), "1".to_string());

        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .facets(facets)
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();

        let hash1 = hash_snapshot(&snap);
        let hash2 = hash_snapshot(&snap);
        assert_eq!(hash1, hash2, "canonical JSON must be deterministic");
        assert!(lillux::valid_hash(&hash1));
    }

    #[test]
    fn thread_status_matches_db_values() {
        // These must exactly match the CHECK constraint in db.rs
        assert_eq!(ThreadStatus::Created.as_str(), "created");
        assert_eq!(ThreadStatus::Running.as_str(), "running");
        assert_eq!(ThreadStatus::Completed.as_str(), "completed");
        assert_eq!(ThreadStatus::Failed.as_str(), "failed");
        assert_eq!(ThreadStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(ThreadStatus::Killed.as_str(), "killed");
        assert_eq!(ThreadStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(ThreadStatus::Continued.as_str(), "continued");
    }

    #[test]
    fn thread_status_from_str_lossy_roundtrip() {
        for status in [
            ThreadStatus::Created,
            ThreadStatus::Running,
            ThreadStatus::Completed,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            let s = status.as_str();
            let back = ThreadStatus::from_str_lossy(s).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn thread_status_from_str_lossy_unknown() {
        assert!(ThreadStatus::from_str_lossy("unknown_status").is_none());
    }

    #[test]
    fn thread_status_is_terminal() {
        assert!(!ThreadStatus::Created.is_terminal());
        assert!(!ThreadStatus::Running.is_terminal());
        assert!(ThreadStatus::Completed.is_terminal());
        assert!(ThreadStatus::Failed.is_terminal());
        assert!(ThreadStatus::Cancelled.is_terminal());
        assert!(ThreadStatus::Killed.is_terminal());
        assert!(ThreadStatus::TimedOut.is_terminal());
        assert!(ThreadStatus::Continued.is_terminal());
    }

    #[test]
    fn thread_status_serde_roundtrip() {
        for status in [
            ThreadStatus::Created,
            ThreadStatus::Running,
            ThreadStatus::Completed,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            let json = serde_json::to_value(&status).unwrap();
            let back: ThreadStatus = serde_json::from_value(json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn snapshot_detached_launch_mode_validates() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .launch_mode("detached")
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_ok());
    }

    #[test]
    fn snapshot_with_result_and_error_serializes() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1", "T-root", "agent", "directive:test", "native:directive-runtime",
        )
        .status(ThreadStatus::Completed)
        .result(Some(serde_json::json!({"answer": 42})))
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:05:00Z".to_string())
        .build();

        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: ThreadSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.result.unwrap()["answer"], 42);
    }
}
