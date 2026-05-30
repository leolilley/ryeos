//! Generic CAS object-graph closure collection.
//!
//! This module owns schema-defined traversal from CAS object roots to the
//! transitive set of reachable object and blob hashes. It intentionally
//! does not discover roots from refs; callers provide root object hashes.

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;

use anyhow::Context;
use serde_json::Value;

const DEFAULT_MAX_OBJECTS: usize = 10_000;
const DEFAULT_MAX_BLOBS: usize = 10_000;
const DEFAULT_MAX_OBJECT_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BLOB_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_LINKS_PER_OBJECT: usize = 10_000;

/// Transitive closure for one or more CAS object roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectClosureReport {
    /// Root object hashes requested by the caller.
    pub roots: Vec<String>,
    /// All reachable JSON object hashes, including roots that are present
    /// or referenced even if their object body is missing/malformed.
    pub object_hashes: BTreeSet<String>,
    /// All reachable blob hashes.
    pub blob_hashes: BTreeSet<String>,
    /// Blob hashes referenced by reachable objects but absent from CAS.
    pub missing_blobs: Vec<MissingDependency>,
    /// Object hashes that were referenced but not present in CAS.
    pub missing_objects: Vec<MissingDependency>,
    /// Object hashes whose JSON body or schema-defined edges were malformed.
    pub malformed_objects: Vec<MalformedObject>,
    /// Objects with a kind this collector does not know how to traverse.
    pub unsupported_objects: Vec<UnsupportedObjectKind>,
}

impl ObjectClosureReport {
    pub fn is_complete(&self) -> bool {
        self.missing_objects.is_empty()
            && self.missing_blobs.is_empty()
            && self.malformed_objects.is_empty()
            && self.unsupported_objects.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDependency {
    pub hash: String,
    pub referenced_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedObject {
    pub hash: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedObjectKind {
    pub hash: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectLinks {
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
    pub unsupported_kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectClosureLimits {
    pub max_objects: usize,
    pub max_blobs: usize,
    pub max_object_bytes: u64,
    pub max_blob_bytes: u64,
    pub max_total_blob_bytes: u64,
    pub max_links_per_object: usize,
}

impl Default for ObjectClosureLimits {
    fn default() -> Self {
        Self {
            max_objects: DEFAULT_MAX_OBJECTS,
            max_blobs: DEFAULT_MAX_BLOBS,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            max_total_blob_bytes: DEFAULT_MAX_TOTAL_BLOB_BYTES,
            max_links_per_object: DEFAULT_MAX_LINKS_PER_OBJECT,
        }
    }
}

impl ObjectClosureLimits {
    pub fn unbounded_for_local_maintenance() -> Self {
        Self {
            max_objects: usize::MAX,
            max_blobs: usize::MAX,
            max_object_bytes: u64::MAX,
            max_blob_bytes: u64::MAX,
            max_total_blob_bytes: u64::MAX,
            max_links_per_object: usize::MAX,
        }
    }
}

/// Collect the schema-defined object/blob closure reachable from `roots`.
pub fn collect_object_closure(
    cas_root: &Path,
    roots: impl IntoIterator<Item = String>,
) -> anyhow::Result<ObjectClosureReport> {
    collect_object_closure_with_limits(
        cas_root,
        roots,
        ObjectClosureLimits::unbounded_for_local_maintenance(),
    )
}

/// Collect the schema-defined object/blob closure reachable from `roots`,
/// aborting once more than `max_objects` object hashes would be visited.
pub fn collect_object_closure_with_limit(
    cas_root: &Path,
    roots: impl IntoIterator<Item = String>,
    max_objects: Option<usize>,
) -> anyhow::Result<ObjectClosureReport> {
    let mut limits = ObjectClosureLimits::unbounded_for_local_maintenance();
    if let Some(max_objects) = max_objects {
        limits.max_objects = max_objects;
    }
    collect_object_closure_with_limits(cas_root, roots, limits)
}

/// Collect the schema-defined object/blob closure reachable from `roots`,
/// enforcing object-count, per-object-byte, and per-object-link limits.
pub fn collect_object_closure_with_limits(
    cas_root: &Path,
    roots: impl IntoIterator<Item = String>,
    limits: ObjectClosureLimits,
) -> anyhow::Result<ObjectClosureReport> {
    let mut report = ObjectClosureReport::default();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
    let mut total_blob_bytes = 0_u64;

    for root in roots {
        report.roots.push(root.clone());
        queue.push_back((root, None));
    }

    while let Some((hash, referenced_by)) = queue.pop_front() {
        if !lillux::valid_hash(&hash) {
            report.malformed_objects.push(MalformedObject {
                hash,
                reason: "invalid object hash".to_string(),
            });
            continue;
        }

        if !report.object_hashes.insert(hash.clone()) {
            continue;
        }

        if report.object_hashes.len() > limits.max_objects {
            anyhow::bail!(
                "object closure exceeds max_objects: {} > {}",
                report.object_hashes.len(),
                limits.max_objects
            );
        }

        let object_path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        match std::fs::metadata(&object_path) {
            Ok(metadata) if metadata.len() > limits.max_object_bytes => {
                anyhow::bail!(
                    "object {hash} exceeds max_object_bytes: {} > {}",
                    metadata.len(),
                    limits.max_object_bytes
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("stat object {hash}")),
        }

        let content = match std::fs::read_to_string(&object_path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                report.missing_objects.push(MissingDependency {
                    hash,
                    referenced_by,
                });
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("read object {hash}")),
        };
        let value: Value = match serde_json::from_str(&content) {
            Ok(value) => value,
            Err(err) => {
                report.malformed_objects.push(MalformedObject {
                    hash,
                    reason: format!("invalid JSON: {err}"),
                });
                continue;
            }
        };

        let links = match object_links(&value) {
            Ok(links) => links,
            Err(reason) => {
                report
                    .malformed_objects
                    .push(MalformedObject { hash, reason });
                continue;
            }
        };

        let link_count = links
            .object_hashes
            .len()
            .saturating_add(links.blob_hashes.len());
        if link_count > limits.max_links_per_object {
            anyhow::bail!(
                "object {hash} exceeds max_links_per_object: {} > {}",
                link_count,
                limits.max_links_per_object
            );
        }

        if let Some(kind) = links.unsupported_kind {
            report
                .unsupported_objects
                .push(UnsupportedObjectKind { hash, kind });
            continue;
        }

        for child in links.object_hashes {
            queue.push_back((child, Some(hash.clone())));
        }
        for blob in links.blob_hashes {
            if lillux::valid_hash(&blob) {
                let blob_path = lillux::shard_path(cas_root, "blobs", &blob, "");
                match std::fs::metadata(&blob_path) {
                    Ok(metadata) => {
                        if metadata.len() > limits.max_blob_bytes {
                            anyhow::bail!(
                                "blob {blob} exceeds max_blob_bytes: {} > {}",
                                metadata.len(),
                                limits.max_blob_bytes
                            );
                        }
                        if !report.blob_hashes.contains(&blob) {
                            if report.blob_hashes.len() + 1 > limits.max_blobs {
                                anyhow::bail!(
                                    "object closure exceeds max_blobs: {} > {}",
                                    report.blob_hashes.len() + 1,
                                    limits.max_blobs
                                );
                            }
                            total_blob_bytes = total_blob_bytes.saturating_add(metadata.len());
                            if total_blob_bytes > limits.max_total_blob_bytes {
                                anyhow::bail!(
                                    "object closure exceeds max_total_blob_bytes: {} > {}",
                                    total_blob_bytes,
                                    limits.max_total_blob_bytes
                                );
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        report.missing_blobs.push(MissingDependency {
                            hash: blob,
                            referenced_by: Some(hash.clone()),
                        });
                        continue;
                    }
                    Err(err) => return Err(err).with_context(|| format!("stat blob {blob}")),
                }
                report.blob_hashes.insert(blob);
            } else {
                report.malformed_objects.push(MalformedObject {
                    hash: hash.clone(),
                    reason: format!("invalid blob hash: {blob}"),
                });
            }
        }
    }

    report.missing_objects.sort_by(|a, b| a.hash.cmp(&b.hash));
    report.missing_blobs.sort_by(|a, b| a.hash.cmp(&b.hash));
    report.malformed_objects.sort_by(|a, b| a.hash.cmp(&b.hash));
    report
        .unsupported_objects
        .sort_by(|a, b| a.hash.cmp(&b.hash));
    Ok(report)
}

/// Extract schema-defined links from one CAS object value.
pub fn object_links(value: &Value) -> Result<ObjectLinks, String> {
    let kind = value
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing object kind".to_string())?;

    let mut links = ObjectLinks::default();

    match kind {
        "attestation" => push_required_hash(value, "subject_hash", &mut links.object_hashes)?,
        "chain_state" => {
            push_optional_hash(value, "prev_chain_state_hash", &mut links.object_hashes)?;
            push_optional_hash(value, "last_event_hash", &mut links.object_hashes)?;
            let Some(threads) = value.get("threads").and_then(|v| v.as_object()) else {
                return Err("chain_state missing threads object".to_string());
            };
            for entry in threads.values() {
                push_required_hash(entry, "snapshot_hash", &mut links.object_hashes)?;
                push_optional_hash(entry, "last_event_hash", &mut links.object_hashes)?;
            }
        }
        "thread_snapshot" => {
            push_optional_hash(
                value,
                "base_project_snapshot_hash",
                &mut links.object_hashes,
            )?;
            push_optional_hash(
                value,
                "result_project_snapshot_hash",
                &mut links.object_hashes,
            )?;
            push_optional_hash(value, "last_event_hash", &mut links.object_hashes)?;
        }
        "thread_event" => {
            push_optional_hash(value, "prev_chain_event_hash", &mut links.object_hashes)?;
            push_optional_hash(value, "prev_thread_event_hash", &mut links.object_hashes)?;
        }
        "project_snapshot" => {
            push_required_hash(value, "project_manifest_hash", &mut links.object_hashes)?;
            push_optional_hash(value, "user_manifest_hash", &mut links.object_hashes)?;
            let parents = value
                .get("parent_hashes")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "project_snapshot missing parent_hashes array".to_string())?;
            for parent in parents {
                let Some(hash) = parent.as_str() else {
                    return Err("project_snapshot parent_hashes contains non-string".to_string());
                };
                push_hash(hash, &mut links.object_hashes)?;
            }
        }
        "source_manifest" => {
            let hashes = value
                .get("item_source_hashes")
                .and_then(|v| v.as_object())
                .ok_or_else(|| "source_manifest missing item_source_hashes object".to_string())?;
            for hash_value in hashes.values() {
                let Some(hash) = hash_value.as_str() else {
                    return Err(
                        "source_manifest item_source_hashes contains non-string".to_string()
                    );
                };
                push_hash(hash, &mut links.object_hashes)?;
            }
        }
        "item_source" => push_required_hash(value, "content_blob_hash", &mut links.blob_hashes)?,
        other => links.unsupported_kind = Some(other.to_string()),
    }

    links.object_hashes.sort();
    links.object_hashes.dedup();
    links.blob_hashes.sort();
    links.blob_hashes.dedup();
    Ok(links)
}

fn push_required_hash(value: &Value, field: &str, out: &mut Vec<String>) -> Result<(), String> {
    let hash = value
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required hash field {field}"))?;
    push_hash(hash, out)
}

fn push_optional_hash(value: &Value, field: &str, out: &mut Vec<String>) -> Result<(), String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(hash)) => push_hash(hash, out),
        Some(_) => Err(format!(
            "optional hash field {field} is not a string or null"
        )),
    }
}

fn push_hash(hash: &str, out: &mut Vec<String>) -> Result<(), String> {
    if !lillux::valid_hash(hash) {
        return Err(format!("invalid hash: {hash}"));
    }
    out.push(hash.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn h(byte: &str) -> String {
        byte.repeat(32)
    }

    fn write_object(cas_root: &Path, value: &Value) -> String {
        let canonical = lillux::canonical_json(value);
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    fn write_blob(cas_root: &Path, data: &[u8]) -> String {
        let hash = lillux::sha256_hex(data);
        let path = lillux::shard_path(cas_root, "blobs", &hash, "");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, data).unwrap();
        hash
    }

    #[test]
    fn project_snapshot_reaches_manifest_item_and_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let blob_hash = write_blob(&cas_root, b"hello closure");
        let item = write_object(
            &cas_root,
            &json!({
                "kind": "item_source",
                "item_ref": "directive:test/example",
                "content_blob_hash": blob_hash,
                "integrity": "none"
            }),
        );
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": "source_manifest",
                "item_source_hashes": { "directive:test/example": item }
            }),
        );
        let snapshot = write_object(
            &cas_root,
            &json!({
                "kind": "project_snapshot",
                "schema": 3,
                "project_manifest_hash": manifest,
                "project_sync_scope": "full_project",
                "parent_hashes": [],
                "created_at": "2026-05-29T00:00:00Z",
                "source": "test"
            }),
        );

        let report = collect_object_closure(&cas_root, [snapshot]).unwrap();
        assert!(report.is_complete());
        assert_eq!(report.object_hashes.len(), 3);
        assert!(report.blob_hashes.contains(&blob_hash));
    }

    #[test]
    fn missing_blob_makes_closure_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let blob_hash = h("cd");
        let item = write_object(
            &cas_root,
            &json!({
                "kind": "item_source",
                "item_ref": "directive:test/example",
                "content_blob_hash": blob_hash,
                "integrity": "none"
            }),
        );

        let report = collect_object_closure(&cas_root, [item]).unwrap();
        assert!(!report.is_complete());
        assert_eq!(report.missing_blobs.len(), 1);
        assert_eq!(report.missing_blobs[0].hash, blob_hash);
    }

    #[test]
    fn chain_state_reaches_top_level_last_event_hash() {
        let event_hash = h("ef");
        let links = object_links(&json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": event_hash,
            "last_chain_seq": 1,
            "updated_at": "2026-05-29T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": h("ab"),
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "running"
                }
            }
        }))
        .unwrap();
        assert!(links.object_hashes.contains(&event_hash));
    }

    #[test]
    fn missing_and_unsupported_objects_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let unsupported = write_object(&cas_root, &json!({ "kind": "future_kind" }));
        let missing = h("12");

        let report = collect_object_closure(&cas_root, [unsupported, missing.clone()]).unwrap();
        assert_eq!(report.unsupported_objects.len(), 1);
        assert_eq!(report.missing_objects.len(), 1);
        assert_eq!(report.missing_objects[0].hash, missing);
    }

    #[test]
    fn attestation_reaches_subject_hash() {
        let subject = h("34");
        let links = object_links(&json!({
            "kind": "attestation",
            "schema": 1,
            "subject_hash": subject,
            "claim": "accepted",
            "policy": "test",
            "issuer": "fp:test",
            "issued_at": "2026-05-29T00:00:00Z",
            "expires_at": null,
            "evidence": {},
            "signature": "test"
        }))
        .unwrap();
        assert_eq!(links.object_hashes, vec![subject]);
    }

    #[test]
    fn traversal_stops_when_max_objects_exceeded() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let item = write_object(
            &cas_root,
            &json!({
                "kind": "item_source",
                "item_ref": "directive:test/example",
                "content_blob_hash": h("cd"),
                "integrity": "none"
            }),
        );
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": "source_manifest",
                "item_source_hashes": { "directive:test/example": item }
            }),
        );
        let snapshot = write_object(
            &cas_root,
            &json!({
                "kind": "project_snapshot",
                "schema": 3,
                "project_manifest_hash": manifest,
                "project_sync_scope": "full_project",
                "parent_hashes": [],
                "created_at": "2026-05-29T00:00:00Z",
                "source": "test"
            }),
        );

        let err = collect_object_closure_with_limit(&cas_root, [snapshot], Some(2)).unwrap_err();
        assert!(err.to_string().contains("exceeds max_objects"));
    }

    #[test]
    fn traversal_rejects_oversized_object() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let value = json!({ "kind": "future_kind", "padding": "x".repeat(256) });
        let hash = write_object(&cas_root, &value);

        let err = collect_object_closure_with_limits(
            &cas_root,
            [hash],
            ObjectClosureLimits {
                max_objects: 8,
                max_blobs: 8,
                max_object_bytes: 32,
                max_blob_bytes: 32,
                max_total_blob_bytes: 32,
                max_links_per_object: 8,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("exceeds max_object_bytes"));
    }

    #[test]
    fn traversal_rejects_too_many_links() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": "source_manifest",
                "item_source_hashes": {
                    "directive:a": h("11"),
                    "directive:b": h("22"),
                }
            }),
        );

        let err = collect_object_closure_with_limits(
            &cas_root,
            [manifest],
            ObjectClosureLimits {
                max_objects: 8,
                max_blobs: 8,
                max_object_bytes: 1024,
                max_blob_bytes: 1024,
                max_total_blob_bytes: 1024,
                max_links_per_object: 1,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("exceeds max_links_per_object"));
    }
}
