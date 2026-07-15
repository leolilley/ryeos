//! Shared reachability traversal for CAS-as-truth.
//!
//! Walks all signed heads (chains + projects) and collects every reachable
//! CAS object hash via BFS. Used by rebuild, verify, sync, and GC.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::object_closure::collect_object_closure;
use crate::refs::{
    list_verified_deployed_project_refs, list_verified_generic_head_refs,
    list_verified_project_head_refs, TrustStore,
};

/// Complete set of reachable hashes from all signed heads.
#[derive(Debug, Clone, Default)]
pub struct ReachableSet {
    /// All reachable JSON object hashes.
    pub object_hashes: HashSet<String>,
    /// All reachable blob hashes (raw binary content).
    pub blob_hashes: HashSet<String>,
    /// Number of distinct signed head records walked. This deliberately
    /// counts namespace-neutral heads and separately-owned heads even when
    /// multiple records target the same CAS object or project identity.
    pub authoritative_root_count: usize,
    /// Chain root IDs that were walked.
    pub chain_root_ids: Vec<String>,
    /// Project hashes that were walked.
    pub project_hashes: Vec<String>,
}

/// Walk all signed heads (chains + projects) and collect every reachable
/// CAS hash via BFS.
///
/// Traversal rules:
///   signed chain head → chain_state → threads map (snapshot_hash, last_event_hash)
///     → thread_snapshot (base_project_snapshot_hash, result_project_snapshot_hash, last_event_hash)
///     → thread_event (prev_chain_event_hash, prev_thread_event_hash)
///   chain_state.prev_chain_state_hash → walk history
///
///   signed project head → project_snapshot (project_manifest_hash, user_manifest_hash, parent_hashes)
///     → source_manifest (item_source_hashes values)
///       → item_source (content_blob_hash) → blob
///   project_snapshot.parent_hashes → walk history
pub fn collect_reachable(
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
) -> Result<ReachableSet> {
    collect_reachable_with_extra_roots(cas_root, refs_root, trust_store, &[])
}

/// Walk verified signed heads plus daemon-authoritative transient roots.
///
/// Extra roots are supplied only by the daemon from active runtime launch
/// metadata; they are not part of the public maintenance request schema.
pub fn collect_reachable_with_extra_roots(
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
    extra_roots: &[String],
) -> Result<ReachableSet> {
    crate::refs::validate_authoritative_ref_namespaces(refs_root)?;
    let mut set = ReachableSet::default();
    let mut roots: BTreeSet<String> = extra_roots.iter().cloned().collect();

    // Every namespace-neutral signed head is an authoritative CAS root. This
    // deliberately does not enumerate a fixed list of kinds: admissions,
    // bundle events, chains, and future generic protocols all retain their
    // complete closure through the same verified ref contract. The generic
    // ref enumerator also proves the exact on-disk/ref_path binding and fails
    // closed on a relocated or malformed head.
    for head in list_verified_generic_head_refs(refs_root, "", trust_store)? {
        set.authoritative_root_count += 1;
        if head.namespace == "chains" {
            set.chain_root_ids.push(head.name.clone());
        }
        roots.insert(head.target_hash);
    }

    for head in list_verified_project_head_refs(refs_root, trust_store)? {
        set.authoritative_root_count += 1;
        roots.insert(head.signed_ref.target_hash);
        set.project_hashes.push(head.project_hash);
    }

    for head in list_verified_deployed_project_refs(refs_root, trust_store)? {
        set.authoritative_root_count += 1;
        roots.insert(head.signed_ref.target_hash);
        set.project_hashes.push(head.project_hash);
    }

    set.chain_root_ids.sort();
    set.chain_root_ids.dedup();
    set.project_hashes.sort();
    set.project_hashes.dedup();

    merge_object_closure(cas_root, roots.into_iter().collect(), &mut set)?;

    Ok(set)
}

/// Walk every authoritative head from the exact refs and CAS directory
/// inodes selected by the owning [`crate::StateDb`].
///
/// This is the production maintenance form. Keeping the descriptors for the
/// complete traversal prevents a runtime-directory rename or replacement from
/// redirecting only part of a mark pass.
pub(crate) fn collect_reachable_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    trust_store: &TrustStore,
) -> Result<ReachableSet> {
    crate::refs::validate_authoritative_ref_namespaces_in_directory(refs_directory)?;
    let mut set = ReachableSet::default();
    let mut roots = BTreeSet::new();

    for head in
        crate::refs::list_verified_generic_head_refs_in_directory(refs_directory, "", trust_store)?
    {
        set.authoritative_root_count += 1;
        if head.namespace == "chains" {
            set.chain_root_ids.push(head.name.clone());
        }
        roots.insert(head.target_hash);
    }

    for head in
        crate::refs::list_verified_project_head_refs_in_directory(refs_directory, trust_store)?
    {
        set.authoritative_root_count += 1;
        roots.insert(head.signed_ref.target_hash);
        set.project_hashes.push(head.project_hash);
    }

    for head in
        crate::refs::list_verified_deployed_project_refs_in_directory(refs_directory, trust_store)?
    {
        set.authoritative_root_count += 1;
        roots.insert(head.signed_ref.target_hash);
        set.project_hashes.push(head.project_hash);
    }

    set.chain_root_ids.sort();
    set.chain_root_ids.dedup();
    set.project_hashes.sort();
    set.project_hashes.dedup();

    merge_object_closure_with_cas(cas, roots, &mut set)?;
    Ok(set)
}

/// Collect reachable objects for a single chain only.
///
/// Useful for sync export of a specific chain.
pub fn collect_chain_reachable(
    cas_root: &Path,
    refs_root: &Path,
    chain_root_id: &str,
    trust_store: &TrustStore,
) -> Result<ReachableSet> {
    let signed_ref = crate::refs::read_verified_generic_head_ref(
        refs_root,
        "chains",
        chain_root_id,
        trust_store,
    )?;
    match signed_ref {
        Some(signed_ref) => {
            collect_chain_reachable_from_head(cas_root, chain_root_id, &signed_ref.target_hash)
        }
        None => Ok(ReachableSet::default()),
    }
}

/// Walk the exact already-verified chain target. Sync callers use this after a
/// single signed-head read so the payload and advertised head cannot diverge
/// under a concurrent later publication.
pub fn collect_chain_reachable_from_head(
    cas_root: &Path,
    chain_root_id: &str,
    chain_head_hash: &str,
) -> Result<ReachableSet> {
    let mut set = ReachableSet::default();
    set.authoritative_root_count = 1;
    set.chain_root_ids.push(chain_root_id.to_string());
    merge_object_closure(cas_root, vec![chain_head_hash.to_string()], &mut set)?;
    Ok(set)
}

pub(crate) fn collect_chain_reachable_from_head_with_cas(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    chain_head_hash: &str,
) -> Result<ReachableSet> {
    let mut set = ReachableSet::default();
    set.authoritative_root_count = 1;
    set.chain_root_ids.push(chain_root_id.to_string());
    merge_object_closure_with_cas(cas, [chain_head_hash.to_string()], &mut set)?;
    Ok(set)
}

fn merge_object_closure(cas_root: &Path, roots: Vec<String>, set: &mut ReachableSet) -> Result<()> {
    let closure = collect_object_closure(cas_root, roots)?;
    if !closure.is_complete() {
        anyhow::bail!(
            "reachable closure incomplete: missing={}, missing_blobs={}, malformed={}, unsupported={}",
            closure.missing_objects.len(),
            closure.missing_blobs.len(),
            closure.malformed_objects.len(),
            closure.unsupported_objects.len()
        );
    }
    set.object_hashes.extend(closure.object_hashes);
    set.blob_hashes.extend(closure.blob_hashes);
    Ok(())
}

fn merge_object_closure_with_cas(
    cas: &lillux::CasStore,
    roots: impl IntoIterator<Item = String>,
    set: &mut ReachableSet,
) -> Result<()> {
    let closure = crate::object_closure::collect_object_closure_with_cas(cas, roots)?;
    if !closure.is_complete() {
        anyhow::bail!(
            "reachable closure incomplete: missing={}, missing_blobs={}, malformed={}, unsupported={}",
            closure.missing_objects.len(),
            closure.missing_blobs.len(),
            closure.malformed_objects.len(),
            closure.unsupported_objects.len()
        );
    }
    set.object_hashes.extend(closure.object_hashes);
    set.blob_hashes.extend(closure.blob_hashes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;
    use crate::Signer as _;
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;

    fn setup_cas_refs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("state").join("objects");
        let refs_root = tmp.path().join("state").join("refs");
        (tmp, cas_root, refs_root)
    }

    fn test_trust_store() -> TrustStore {
        let signer = TestSigner::default();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        trust_store
    }

    fn write_trusted_ref(path: &Path, ref_path: &str, target_hash: &str, updated_at: &str) {
        let signer = TestSigner::default();
        let signed_ref = crate::refs::SignedRef::new(
            ref_path.to_string(),
            target_hash.to_string(),
            updated_at.to_string(),
            signer.fingerprint().to_string(),
        );
        crate::refs::write_signed_ref(path, signed_ref, &signer).unwrap();
    }

    fn collect_reachable(cas_root: &Path, refs_root: &Path) -> Result<ReachableSet> {
        super::collect_reachable(cas_root, refs_root, &test_trust_store())
    }

    fn collect_chain_reachable(
        cas_root: &Path,
        refs_root: &Path,
        chain_root_id: &str,
    ) -> Result<ReachableSet> {
        super::collect_chain_reachable(cas_root, refs_root, chain_root_id, &test_trust_store())
    }

    fn write_object_at(cas_root: &Path, hash: &str, value: &Value) {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let canonical = lillux::canonical_json(value).unwrap();
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
    }

    fn write_object(cas_root: &Path, value: &Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        write_object_at(cas_root, &hash, value);
        hash
    }

    fn write_blob(cas_root: &Path, bytes: &[u8]) -> String {
        let hash = lillux::sha256_hex(bytes);
        let path = lillux::shard_path(cas_root, "blobs", &hash, "");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        lillux::atomic_write(&path, bytes).unwrap();
        hash
    }

    fn make_hash(suffix: &str) -> String {
        format!(
            "{:064}",
            suffix
                .as_bytes()
                .iter()
                .fold(0u64, |a, &b| a.wrapping_add(b as u64))
        )
    }

    fn durable_history_policy_json() -> Value {
        serde_json::json!({
            "retention": { "mode": "durable" },
            "canonical_item_ref": "directive:test/item",
            "item_content_hash": "11".repeat(32),
            "item_signer_fingerprint": "22".repeat(32),
            "item_trust_class": "trusted",
            "kind_schema_content_hash": "33".repeat(32),
            "resolved_from": {
                "node_default": { "node_policy": "missing_config" }
            },
        })
    }

    #[test]
    fn collect_reachable_empty_state() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();
        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert!(set.object_hashes.is_empty());
        assert!(set.blob_hashes.is_empty());
        assert!(set.chain_root_ids.is_empty());
        assert!(set.project_hashes.is_empty());
    }

    #[test]
    fn collect_reachable_single_chain_head() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        // Create a minimal thread_snapshot
        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "result": null,
            "outcome_code": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
            "project_root": null,
            "captured_history_policy": durable_history_policy_json(),
        });
        let snap_hash = write_object(&cas_root, &snap);

        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });
        let cs_hash = write_object(&cas_root, &cs);

        // Write signed head ref
        let head_path = refs_root.join("generic/chains/T-root/head");
        write_trusted_ref(
            &head_path,
            "chains/T-root/head",
            &cs_hash,
            "2026-04-22T00:00:00Z",
        );

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert_eq!(set.chain_root_ids.len(), 1);
        assert_eq!(set.chain_root_ids[0], "T-root");
        assert!(set.object_hashes.contains(&cs_hash));
        assert!(set.object_hashes.contains(&snap_hash));
        assert_eq!(set.object_hashes.len(), 2);
    }

    #[test]
    fn collect_reachable_bundle_event_chain() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let first = serde_json::json!({
            "kind": "bundle_event",
            "schema": 1,
            "bundle_id": "ryeos-email",
            "event_kind": "campaign_event",
            "event_type": "campaign_created",
            "schema_version": 1,
            "chain_id": "camp_1",
            "chain_seq": 1,
            "prev_chain_event_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "payload": {}
        });
        let first_hash = write_object(&cas_root, &first);
        let head = serde_json::json!({
            "kind": "bundle_event",
            "schema": 1,
            "bundle_id": "ryeos-email",
            "event_kind": "campaign_event",
            "event_type": "campaign_updated",
            "schema_version": 1,
            "chain_id": "camp_1",
            "chain_seq": 2,
            "prev_chain_event_hash": first_hash,
            "created_at": "2026-04-22T00:00:01Z",
            "payload": {}
        });
        let head_hash = write_object(&cas_root, &head);

        let head_path =
            refs_root.join("generic/bundle_events/ryeos-email/campaign_event/chains/camp_1/head");
        write_trusted_ref(
            &head_path,
            "bundle_events/ryeos-email/campaign_event/chains/camp_1/head",
            &head_hash,
            "2026-04-22T00:00:01Z",
        );

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert!(
            set.object_hashes.contains(&head_hash),
            "bundle event head must be reachable"
        );
        assert!(
            set.object_hashes.contains(&first_hash),
            "full bundle event chain history must be reachable"
        );
    }

    #[test]
    fn collect_reachable_rejects_relocated_generic_refs() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        // A head ref moved into an operator backup directory: its internal
        // ref_path no longer matches its on-disk location. Authoritative
        // reachability must fail closed rather than silently unroot its target.
        let head_path = refs_root.join(
            "generic/bundle_events/ryeos-email.broken-backup-20260610T021602Z/campaign_event/chains/camp_1/head",
        );
        write_trusted_ref(
            &head_path,
            "bundle_events/ryeos-email/campaign_event/chains/camp_1/head",
            &make_hash("gone"),
            "2026-04-22T00:00:00Z",
        );

        let error = collect_reachable(&cas_root, &refs_root)
            .expect_err("relocated generic head must fail exact-path verification");
        assert!(error.to_string().contains("ref_path mismatch"));
    }

    #[test]
    fn collect_reachable_event_chain() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        // event1 has no predecessor; event2 links to its exact CAS hash.
        let event1 = serde_json::json!({
            "kind": "thread_event",
            "schema": 1,
            "chain_root_id": "T-root",
            "chain_seq": 1,
            "thread_id": "T-root",
            "thread_seq": 1,
            "event_type": "thread_created",
            "durability": "durable",
            "ts": "2026-04-22T00:00:01Z",
            "prev_chain_event_hash": null,
            "prev_thread_event_hash": null,
            "payload": {}
        });
        let prev_event_hash = write_object(&cas_root, &event1);
        let event = serde_json::json!({
            "kind": "thread_event",
            "schema": 1,
            "chain_root_id": "T-root",
            "chain_seq": 2,
            "thread_id": "T-root",
            "thread_seq": 2,
            "event_type": "test_event",
            "durability": "durable",
            "ts": "2026-04-22T00:00:02Z",
            "prev_chain_event_hash": prev_event_hash,
            "prev_thread_event_hash": prev_event_hash,
            "payload": {}
        });
        let event_hash = write_object(&cas_root, &event);

        // snapshot points to last event
        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "running",
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": "2026-04-22T00:00:00Z",
            "finished_at": null,
            "result": null,
            "outcome_code": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": event_hash,
            "last_chain_seq": 2,
            "last_thread_seq": 2,
            "upstream_thread_id": null,
            "requested_by": null,
            "project_root": null,
            "captured_history_policy": durable_history_policy_json(),
        });
        let snap_hash = write_object(&cas_root, &snap);

        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": event_hash,
            "last_chain_seq": 2,
            "updated_at": "2026-04-22T00:00:02Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": event_hash,
                    "last_thread_seq": 2,
                    "status": "running"
                }
            }
        });
        let cs_hash = write_object(&cas_root, &cs);

        let head_path = refs_root.join("generic/chains/T-root/head");
        write_trusted_ref(
            &head_path,
            "chains/T-root/head",
            &cs_hash,
            "2026-04-22T00:00:00Z",
        );

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert_eq!(set.object_hashes.len(), 4);
        assert!(set.object_hashes.contains(&cs_hash));
        assert!(set.object_hashes.contains(&snap_hash));
        assert!(set.object_hashes.contains(&event_hash));
        assert!(set.object_hashes.contains(&prev_event_hash));
    }

    #[test]
    fn collect_reachable_project_source() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let blob_hash = write_blob(&cas_root, b"test content");
        let item_source = serde_json::json!({
            "kind": "item_source",
            "item_ref": ".ai/directives/test.md",
            "content_blob_hash": blob_hash,
            "integrity": blob_hash,
            "signature_info": null,
            "mode": null
        });
        let item_source_hash = write_object(&cas_root, &item_source);

        let manifest = serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                ".ai/directives/test.md": item_source_hash
            }
        });
        let manifest_hash = write_object(&cas_root, &manifest);
        let proj_snap = serde_json::json!({
            "kind": "project_snapshot",
            "schema": crate::objects::ProjectSnapshot::SCHEMA,
            "project_manifest_hash": manifest_hash,
            "user_manifest_hash": null,
            "message": null,
            "project_sync_scope": "full_project",
            "parent_hashes": [],
            "created_at": "2026-04-22T00:00:00Z",
            "source": "local"
        });
        let proj_snap_hash = write_object(&cas_root, &proj_snap);

        let head_path = refs_root.join("projects/principal-test/test_project/head");
        write_trusted_ref(
            &head_path,
            "projects/principal-test/test_project/head",
            &proj_snap_hash,
            "2026-04-22T00:00:00Z",
        );

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert_eq!(set.project_hashes.len(), 1);
        assert!(set.object_hashes.contains(&proj_snap_hash));
        assert!(set.object_hashes.contains(&manifest_hash));
        assert!(set.object_hashes.contains(&item_source_hash));
        assert_eq!(set.object_hashes.len(), 3);
        assert!(set.blob_hashes.contains(&blob_hash));
        assert_eq!(set.blob_hashes.len(), 1);
    }

    #[test]
    fn admission_head_roots_its_subjects_complete_project_closure() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();
        let signer = TestSigner::default();
        let blob_hash = write_blob(&cas_root, b"admitted content");
        let item_hash = write_object(
            &cas_root,
            &serde_json::json!({
                "kind": "item_source",
                "item_ref": ".ai/directives/admitted.md",
                "content_blob_hash": blob_hash,
                "integrity": blob_hash,
                "signature_info": null,
                "mode": null,
            }),
        );
        let manifest_hash = write_object(
            &cas_root,
            &serde_json::json!({
                "kind": "source_manifest",
                "item_source_hashes": { ".ai/directives/admitted.md": item_hash },
            }),
        );
        let snapshot_hash = write_object(
            &cas_root,
            &serde_json::json!({
                "kind": "project_snapshot",
                "schema": crate::objects::ProjectSnapshot::SCHEMA,
                "project_manifest_hash": manifest_hash,
                "user_manifest_hash": null,
                "message": null,
                "project_sync_scope": "full_project",
                "parent_hashes": [],
                "created_at": "2026-07-14T00:00:00Z",
                "source": "admission_test",
            }),
        );
        let attestation = crate::objects::Attestation::unsigned(
            snapshot_hash.clone(),
            "accepted".to_string(),
            "policy-a".to_string(),
            "2026-07-14T00:00:01Z".to_string(),
            None,
            serde_json::json!({}),
        )
        .sign(&signer)
        .unwrap();
        let attestation_hash = write_object(&cas_root, &attestation.to_value());
        write_trusted_ref(
            &refs_root.join(format!("generic/admissions/policy-a/{snapshot_hash}/head")),
            &format!("admissions/policy-a/{snapshot_hash}/head"),
            &attestation_hash,
            "2026-07-14T00:00:01Z",
        );

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        for hash in [attestation_hash, snapshot_hash, manifest_hash, item_hash] {
            assert!(set.object_hashes.contains(&hash));
        }
        assert!(set.blob_hashes.contains(&blob_hash));
    }

    #[test]
    fn collect_reachable_ignores_orphan_objects() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        // Write an orphan object not linked from any head
        let orphan_hash = write_object(
            &cas_root,
            &serde_json::json!({
                "kind": "thread_event",
                "schema": 1,
                "chain_root_id": "T-root",
                "chain_seq": 1,
                "thread_id": "T-root",
                "thread_seq": 1,
                "event_type": "orphan",
                "durability": "durable",
                "ts": "2026-04-22T00:00:00Z",
                "prev_chain_event_hash": null,
                "prev_thread_event_hash": null,
                "payload": {}
            }),
        );

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        assert!(set.object_hashes.is_empty());
        assert!(!set.object_hashes.contains(&orphan_hash));
    }

    #[test]
    fn collect_reachable_chain_history() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let snap_v1 = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "result": null,
            "outcome_code": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
            "project_root": null,
            "captured_history_policy": durable_history_policy_json(),
        });

        let snap_v2 = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "running",
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:01Z",
            "started_at": "2026-04-22T00:00:01Z",
            "finished_at": null,
            "result": null,
            "outcome_code": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
            "project_root": null,
            "captured_history_policy": durable_history_policy_json(),
        });
        let snap1 = write_object(&cas_root, &snap_v1);
        let snap2 = write_object(&cas_root, &snap_v2);

        let cs_v1 = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap1,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });
        let cs_v1_hash = write_object(&cas_root, &cs_v1);
        let cs_v2 = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": cs_v1_hash,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:01Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap2,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "running"
                }
            }
        });
        let cs_v2_hash = write_object(&cas_root, &cs_v2);

        let head_path = refs_root.join("generic/chains/T-root/head");
        write_trusted_ref(
            &head_path,
            "chains/T-root/head",
            &cs_v2_hash,
            "2026-04-22T00:00:01Z",
        );

        let set = collect_reachable(&cas_root, &refs_root).unwrap();
        // Both chain states + both snapshots = 4 objects
        assert_eq!(set.object_hashes.len(), 4);
        assert!(set.object_hashes.contains(&cs_v1_hash));
        assert!(set.object_hashes.contains(&cs_v2_hash));
        assert!(set.object_hashes.contains(&snap1));
        assert!(set.object_hashes.contains(&snap2));
    }

    #[test]
    fn collect_chain_reachable_single_chain() {
        let (_tmp, cas_root, refs_root) = setup_cas_refs();

        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 1,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "result": null,
            "outcome_code": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
            "project_root": null,
            "captured_history_policy": durable_history_policy_json(),
        });
        let snap_hash = write_object(&cas_root, &snap);

        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });
        let cs_hash = write_object(&cas_root, &cs);

        // Also create a second chain that should NOT be included
        let cs2 = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-other",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": {}
        });
        let cs2_hash = write_object(&cas_root, &cs2);

        let head_path = refs_root.join("generic/chains/T-root/head");
        write_trusted_ref(
            &head_path,
            "chains/T-root/head",
            &cs_hash,
            "2026-04-22T00:00:00Z",
        );

        // Don't write T-other head ref

        let set = collect_chain_reachable(&cas_root, &refs_root, "T-root").unwrap();
        assert_eq!(set.chain_root_ids.len(), 1);
        assert_eq!(set.chain_root_ids[0], "T-root");
        assert!(set.object_hashes.contains(&cs_hash));
        assert!(set.object_hashes.contains(&snap_hash));
        assert!(!set.object_hashes.contains(&cs2_hash));
    }
}
