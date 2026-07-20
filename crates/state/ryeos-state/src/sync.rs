//! Object-level sync protocol for CAS-as-truth.
//!
//! Provides four core operations:
//! - `export_chain`: collect all reachable objects (and blobs) for a chain
//! - `reconcile_export`: given remote's hash set, return only missing objects
//! - `stage_chain_import`: write foreign objects under durable temporary roots
//! - `finalize_import`: consume that staging token and publish the chain head
//!
//! No transport protocol — sync is object-level. Transport (HTTP, file,
//! etc.) is a separate concern layered on top.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::reachability;
use crate::{CasEntryKind, CasEntryState, NewCasEntryAttribution, StagedCasRootLease, StateDb};

/// A single entry in a sync payload (object or blob).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncEntry {
    /// SHA-256 hash of the content.
    pub hash: String,
    /// Whether this is a blob (true) or a JSON object (false).
    pub is_blob: bool,
    /// Raw content bytes.
    pub data: Vec<u8>,
}

/// Payload for exporting / importing objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPayload {
    /// The chain root ID this export covers.
    pub chain_root_id: String,
    /// The chain head hash (chain_state object) at the time of export.
    /// The importer uses this to sign the local chain head ref.
    pub chain_head_hash: String,
    /// All object + blob entries.
    pub entries: Vec<SyncEntry>,
    /// Total serialized size in bytes.
    pub total_bytes: usize,
}

/// Result of importing objects from a remote sync payload.
#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    /// Objects/blobs newly written.
    pub imported: usize,
    /// Objects/blobs already present locally.
    pub already_present: usize,
    /// Hash verification failures (corrupt entries).
    pub hash_mismatches: usize,
    /// Total bytes written.
    pub bytes_written: usize,
}

/// A verified chain payload whose imported objects remain durable GC roots
/// until [`finalize_import`] publishes and projects the authoritative head.
/// Dropping an unfinished value abandons the import but deliberately leaves
/// its durable record for the exclusive GC recovery pass; callers cannot
/// accidentally finalize a hash unrelated to the staged payload because the
/// identity is carried privately by this token.
#[must_use = "a staged chain import must be finalized"]
pub struct StagedChainImport {
    pub result: ImportResult,
    chain_root_id: String,
    chain_head_hash: String,
    roots: StagedCasRootLease,
}

/// Attribution attached to an imported CAS payload.
///
/// This is not trust admission. It records where entries came from so later
/// admission, mirroring, GC, and operator inspection can make deterministic
/// decisions from the same local index.
#[derive(Debug, Clone, Default)]
pub struct ImportAttribution {
    pub source_principal: Option<String>,
    pub source_peer: Option<String>,
    pub job_id: Option<String>,
}

/// Standalone export rooted in one runtime-state authority.
pub fn export_chain(
    runtime_state_dir: &Path,
    chain_root_id: &str,
    trust_store: &crate::refs::TrustStore,
) -> Result<ExportPayload> {
    let (_cas_mutation_guard, cas, refs_directory) =
        pin_standalone_export_authority(runtime_state_dir)?;
    export_chain_pinned(&cas, &refs_directory, chain_root_id, trust_store)
}

/// Export one chain from descriptor-bound refs and CAS authority.
///
/// The signed head is trust-verified exactly once from `refs_directory`. Its
/// target is then used both as the closure root and as the returned advertised
/// head, while every closure and payload read uses the same pinned `cas`.
/// Missing or malformed closure entries fail the export. The caller must
/// retain the shared or exclusive CAS-mutation guard protecting this authority
/// for the complete call.
pub fn export_chain_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    chain_root_id: &str,
    trust_store: &crate::refs::TrustStore,
) -> Result<ExportPayload> {
    let (chain_head_hash, reachable) =
        verified_chain_reachability_pinned(cas, refs_directory, chain_root_id, trust_store)?;

    let mut entries =
        Vec::with_capacity(reachable.object_hashes.len() + reachable.blob_hashes.len());
    let mut total_bytes = 0usize;

    // Collect JSON objects
    let mut object_hashes = reachable.object_hashes.into_iter().collect::<Vec<_>>();
    object_hashes.sort();
    for hash in object_hashes {
        let value = cas
            .get_object(&hash)
            .with_context(|| format!("read exported chain object {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("exported chain object {hash} disappeared"))?;
        let data = lillux::canonical_json(&value)
            .with_context(|| format!("canonicalize exported chain object {hash}"))?
            .into_bytes();
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash,
            is_blob: false,
            data,
        });
    }

    // Collect blobs
    let mut blob_hashes = reachable.blob_hashes.into_iter().collect::<Vec<_>>();
    blob_hashes.sort();
    for hash in blob_hashes {
        let data = cas
            .get_blob(&hash)
            .with_context(|| format!("read exported chain blob {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("exported chain blob {hash} disappeared"))?;
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash,
            is_blob: true,
            data,
        });
    }

    Ok(ExportPayload {
        chain_root_id: chain_root_id.to_string(),
        chain_head_hash,
        entries,
        total_bytes,
    })
}

/// Standalone reconciled export rooted in one runtime-state authority.
pub fn reconcile_export(
    runtime_state_dir: &Path,
    chain_root_id: &str,
    remote_has: &HashSet<String>,
    trust_store: &crate::refs::TrustStore,
) -> Result<ExportPayload> {
    let (_cas_mutation_guard, cas, refs_directory) =
        pin_standalone_export_authority(runtime_state_dir)?;
    reconcile_export_pinned(
        &cas,
        &refs_directory,
        chain_root_id,
        remote_has,
        trust_store,
    )
}

/// Return only remote-missing entries from one descriptor-bound chain export.
///
/// The head is verified once, the exact verified target is returned, and both
/// closure traversal and payload reads stay on `cas`. The caller must retain
/// the CAS-mutation guard protecting the pinned authority for the complete
/// call.
pub fn reconcile_export_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    chain_root_id: &str,
    remote_has: &HashSet<String>,
    trust_store: &crate::refs::TrustStore,
) -> Result<ExportPayload> {
    let (chain_head_hash, reachable) =
        verified_chain_reachability_pinned(cas, refs_directory, chain_root_id, trust_store)?;

    // Filter to only objects the remote doesn't have
    let mut missing_objects: Vec<_> = reachable
        .object_hashes
        .into_iter()
        .filter(|hash| !remote_has.contains(hash))
        .collect();
    missing_objects.sort();

    let mut missing_blobs: Vec<_> = reachable
        .blob_hashes
        .into_iter()
        .filter(|hash| !remote_has.contains(hash))
        .collect();
    missing_blobs.sort();

    let mut entries = Vec::with_capacity(missing_objects.len() + missing_blobs.len());
    let mut total_bytes = 0usize;

    for hash in missing_objects {
        let value = cas
            .get_object(&hash)
            .with_context(|| format!("read reconciled chain object {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("reconciled chain object {hash} disappeared"))?;
        let data = lillux::canonical_json(&value)
            .with_context(|| format!("canonicalize reconciled chain object {hash}"))?
            .into_bytes();
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash,
            is_blob: false,
            data,
        });
    }

    for hash in missing_blobs {
        let data = cas
            .get_blob(&hash)
            .with_context(|| format!("read reconciled chain blob {hash}"))?
            .ok_or_else(|| anyhow::anyhow!("reconciled chain blob {hash} disappeared"))?;
        total_bytes += data.len();
        entries.push(SyncEntry {
            hash,
            is_blob: true,
            data,
        });
    }

    Ok(ExportPayload {
        chain_root_id: chain_root_id.to_string(),
        chain_head_hash,
        entries,
        total_bytes,
    })
}

/// Stage a remote chain payload in local CAS under durable temporary GC roots.
///
/// For each entry:
/// - Verify the hash matches the content (SHA-256 integrity check)
/// - Skip if already present locally
/// - Write to the correct sharded path
/// - Verify and lease the exact head's complete local closure
///
/// The returned token must be consumed by [`finalize_import`] before its roots
/// can be released safely.
pub fn stage_chain_import(
    runtime_state_dir: &Path,
    payload: &ExportPayload,
) -> Result<StagedChainImport> {
    let runtime_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
        .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
    let cas_mutation_guard =
        crate::recovery::CasMutationGuard::acquire_existing_shared_in_pinned_runtime(
            &runtime_directory,
        )?;
    let cas_directory = runtime_directory
        .open_child_directory(std::ffi::OsStr::new("objects"))?
        .ok_or_else(|| anyhow::anyhow!("CAS root is absent"))?;
    let recovery_parent =
        runtime_directory.open_or_create_child(std::ffi::OsStr::new("recovery"), 0o700)?;
    let recovery_directory =
        recovery_parent.open_or_create_child(std::ffi::OsStr::new("thread-projection"), 0o700)?;
    let recovery = crate::RecoveryStore::from_pinned_directories(
        runtime_directory.try_clone()?,
        recovery_directory,
    )?;
    let cas = lillux::CasStore::from_pinned_root(cas_directory);
    stage_chain_import_with_pinned_authority(&cas, &recovery, payload, &cas_mutation_guard)
}

/// Stage a remote chain payload using one captured runtime authority.
///
/// CAS writes, durable temporary roots, and closure verification are all
/// descriptor-relative to `authority`. The caller must retain `guard` until
/// this function returns; the returned durable lease then protects the exact
/// verified closure until [`finalize_import`] consumes it.
pub fn stage_chain_import_pinned(
    authority: &crate::PinnedStateAuthority,
    payload: &ExportPayload,
    guard: &crate::CasMutationGuard,
) -> Result<StagedChainImport> {
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    stage_chain_import_with_pinned_authority(&cas, authority.require_recovery()?, payload, guard)
}

fn stage_chain_import_with_pinned_authority(
    cas: &lillux::CasStore,
    recovery: &crate::RecoveryStore,
    payload: &ExportPayload,
    guard: &crate::CasMutationGuard,
) -> Result<StagedChainImport> {
    recovery.ensure_guard(guard)?;
    let mut roots = recovery.begin_staged_cas_roots_admitted(guard, "chain-import")?;
    let mut result = ImportResult::default();

    for entry in &payload.entries {
        // Verify hash integrity
        let computed = lillux::sha256_hex(&entry.data);
        if computed != entry.hash {
            result.hash_mismatches += 1;
            continue;
        }

        // Protect the expected hash durably before the first CAS write. The
        // lease spans the gap between this function and final head publication.
        if entry.is_blob {
            roots.protect_blob_hash_admitted(guard, &entry.hash)?;
        } else {
            roots.protect_object_hash_admitted(guard, &entry.hash)?;
        }

        let outcome = put_sync_entry(cas, entry)?;
        if outcome.hash != entry.hash {
            anyhow::bail!(
                "sync entry canonical hash mismatch: declared {}, canonical {}",
                entry.hash,
                outcome.hash
            );
        }
        if outcome.created {
            result.imported += 1;
            result.bytes_written += entry.data.len();
        } else {
            result.already_present += 1;
        }
    }

    // Verify the exact staged head and its complete closure while the shared
    // CAS mutation guard still excludes sweep. Incremental payloads may omit
    // descendants already present locally, so lease the verified closure—not
    // merely the entries transported in this payload—before releasing it.
    let closure = crate::rebuild::verified_repair_closure_with_cas(
        cas,
        &payload.chain_root_id,
        &payload.chain_head_hash,
        true,
    )?;
    roots.protect_cas_closure_admitted(
        guard,
        closure.object_hashes.iter().map(String::as_str),
        closure.blob_hashes.iter().map(String::as_str),
    )?;

    Ok(StagedChainImport {
        result,
        chain_root_id: payload.chain_root_id.clone(),
        chain_head_hash: payload.chain_head_hash.clone(),
        roots,
    })
}

/// Import objects and record every verified entry in the CAS attribution index.
///
/// Newly imported and already-present entries are both recorded as `staged` by
/// default. A later admission pass can promote them to `accepted`, `mirrored`,
/// or `rejected` without re-reading the transfer payload.
pub fn import_objects_staged(
    db: &StateDb,
    payload: &ExportPayload,
    attribution: &ImportAttribution,
    cas_mutation_guard: &crate::recovery::CasMutationGuard,
) -> Result<ImportResult> {
    db.pinned_authority()?.ensure_guard(cas_mutation_guard)?;
    let cas = db.pinned_cas()?;
    let mut result = ImportResult::default();

    for entry in &payload.entries {
        let computed = lillux::sha256_hex(&entry.data);
        if computed != entry.hash {
            result.hash_mismatches += 1;
            continue;
        }

        let entry_kind = if entry.is_blob {
            CasEntryKind::Blob
        } else {
            CasEntryKind::Object
        };

        let outcome = put_sync_entry(&cas, entry)?;
        if outcome.hash != entry.hash {
            anyhow::bail!(
                "sync entry canonical hash mismatch: declared {}, canonical {}",
                entry.hash,
                outcome.hash
            );
        }
        if outcome.created {
            result.imported += 1;
            result.bytes_written += entry.data.len();
        } else {
            result.already_present += 1;
        }

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: entry.hash.clone(),
            entry_kind,
            bytes: entry.data.len() as u64,
            source_principal: attribution.source_principal.clone(),
            source_peer: attribution.source_peer.clone(),
            job_id: attribution.job_id.clone(),
            state: CasEntryState::Staged,
        })
        .with_context(|| format!("failed to record CAS attribution for {}", entry.hash))?;
    }

    Ok(result)
}

fn put_sync_entry(cas: &lillux::CasStore, entry: &SyncEntry) -> Result<lillux::CasPutOutcome> {
    if entry.is_blob {
        return cas
            .put_blob(&entry.data)
            .with_context(|| format!("publish sync blob {}", entry.hash));
    }
    let value: serde_json::Value = serde_json::from_slice(&entry.data)
        .with_context(|| format!("decode sync object {}", entry.hash))?;
    let canonical = lillux::canonical_json(&value)
        .with_context(|| format!("canonicalize sync object {}", entry.hash))?;
    if canonical.as_bytes() != entry.data {
        anyhow::bail!("sync object {} is not canonically encoded", entry.hash);
    }
    cas.put_object(&value)
        .with_context(|| format!("publish sync object {}", entry.hash))
}

fn load_exact_staged_chain_head(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    chain_head_hash: &str,
) -> Result<crate::ChainState> {
    if !lillux::valid_hash(chain_head_hash)
        || chain_head_hash
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("invalid staged chain head hash: {chain_head_hash}");
    }
    let value = cas
        .get_object(chain_head_hash)
        .with_context(|| format!("read exact staged chain head {chain_head_hash}"))?
        .ok_or_else(|| anyhow::anyhow!("exact staged chain head {chain_head_hash} is absent"))?;
    let chain_state: crate::ChainState = serde_json::from_value(value)
        .with_context(|| format!("decode exact staged chain head {chain_head_hash}"))?;
    chain_state.validate()?;
    if chain_state.chain_root_id != chain_root_id {
        anyhow::bail!(
            "staged chain head root mismatch: expected {chain_root_id}, got {}",
            chain_state.chain_root_id
        );
    }
    Ok(chain_state)
}

/// Finalize an import: verify its complete closure, sign a local chain head,
/// and bring only that chain's projection current through the journaled chain
/// publisher.
///
/// After importing objects, the imported chain needs a signed head ref
/// to be discoverable via signed heads. Without it, `collect_reachable()`
/// won't find the chain and GC will sweep the imported objects.
///
/// Steps:
/// 1. Verify the chain head object exists in CAS
/// 2. prepare/publish a signed local head through the Set journal
/// 3. project and compare-ack the Set
/// 4. release the durable staging roots
pub fn finalize_import(
    state_db: &StateDb,
    mut staged: StagedChainImport,
    signer: &dyn crate::Signer,
    cas_mutation_guard: &crate::CasMutationGuard,
) -> Result<ImportResult> {
    let operation = (|| -> Result<()> {
        let authority = state_db.pinned_authority()?;
        authority.ensure_guard(cas_mutation_guard)?;
        let cas = authority.cas_store()?;
        if staged.result.hash_mismatches != 0 {
            anyhow::bail!(
                "cannot finalize chain import with {} hash mismatch(es)",
                staged.result.hash_mismatches
            );
        }

        // Step 1: decode the exact staged target as the current ChainState
        // before signing anything. Closure traversal alone is not the
        // publication type contract, and a nested ChainState cannot stand in
        // for this exact hash.
        let _chain_state =
            load_exact_staged_chain_head(&cas, &staged.chain_root_id, &staged.chain_head_hash)?;

        // Verify the complete imported closure and every genesis-to-target
        // ChainState transition again at the publication boundary, including
        // hashes of already-local descendants. StateDb repeats this under the
        // chain lock and anchors it to any current signed head before publication.
        crate::rebuild::verified_repair_closure_with_cas(
            &cas,
            &staged.chain_root_id,
            &staged.chain_head_hash,
            true,
        )?;

        // Step 2: publish through StateDb's journaled chain namespace path,
        // which holds the chain critical section through projection and
        // compare-ack.
        state_db.write_chain_head_ref_admitted(
            &staged.chain_root_id,
            &staged.chain_head_hash,
            signer,
            cas_mutation_guard,
        )?;

        tracing::info!(
            chain_root_id = %staged.chain_root_id,
            chain_head_hash = %staged.chain_head_hash,
            "signed local chain head ref after import"
        );
        Ok(())
    })();

    // Always disarm the lease before returning while the caller may still
    // hold the StateStore mutex. finish_admitted never retries through Drop,
    // even if cleanup itself fails.
    let cleanup = staged.roots.finish_admitted(cas_mutation_guard);
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(staged.result),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => {
            Err(cleanup_error).context("release staged CAS roots after import")
        }
        (Err(error), Err(cleanup_error)) => Err(anyhow::anyhow!(
            "{error:#}; releasing staged CAS roots also failed: {cleanup_error:#}"
        )),
    }
}

/// Standalone pathname adapter for collecting one chain's local hashes.
///
/// This pins the complete authority once at entry. Already-open StateDb
/// callers should use [`collect_local_hashes_pinned`].
pub fn collect_local_hashes(
    runtime_state_dir: &Path,
    chain_root_id: &str,
    trust_store: &crate::refs::TrustStore,
) -> Result<HashSet<String>> {
    let (_cas_mutation_guard, cas, refs_directory) =
        pin_standalone_export_authority(runtime_state_dir)?;
    collect_local_hashes_pinned(&cas, &refs_directory, chain_root_id, trust_store)
}

/// Collect one chain's object and blob hashes from descriptor-bound authority.
///
/// The caller must retain the CAS-mutation guard protecting `cas` for the
/// complete call.
pub fn collect_local_hashes_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    chain_root_id: &str,
    trust_store: &crate::refs::TrustStore,
) -> Result<HashSet<String>> {
    let (_, reachable) =
        verified_chain_reachability_pinned(cas, refs_directory, chain_root_id, trust_store)?;
    let mut hashes =
        HashSet::with_capacity(reachable.object_hashes.len() + reachable.blob_hashes.len());
    hashes.extend(reachable.object_hashes);
    hashes.extend(reachable.blob_hashes);
    Ok(hashes)
}

fn verified_chain_reachability_pinned(
    cas: &lillux::CasStore,
    refs_directory: &lillux::PinnedDirectory,
    chain_root_id: &str,
    trust_store: &crate::refs::TrustStore,
) -> Result<(String, reachability::ReachableSet)> {
    let signed_ref = crate::refs::read_verified_generic_head_ref_in_directory(
        refs_directory,
        "chains",
        chain_root_id,
        trust_store,
    )?
    .ok_or_else(|| anyhow::anyhow!("missing chain head ref for {chain_root_id}"))?;
    let chain_head_hash = signed_ref.target_hash;
    let reachable = reachability::collect_chain_reachable_from_head_with_cas(
        cas,
        chain_root_id,
        &chain_head_hash,
    )?;
    Ok((chain_head_hash, reachable))
}

fn pin_standalone_export_authority(
    runtime_state_dir: &Path,
) -> Result<(
    crate::CasMutationGuard,
    lillux::CasStore,
    lillux::PinnedDirectory,
)> {
    let runtime_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
        .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
    let cas_mutation_guard =
        crate::recovery::CasMutationGuard::acquire_existing_shared_in_pinned_runtime(
            &runtime_directory,
        )?;
    let cas_directory = runtime_directory
        .open_child_directory(std::ffi::OsStr::new("objects"))?
        .ok_or_else(|| anyhow::anyhow!("CAS objects directory is absent"))?;
    let refs_directory = runtime_directory
        .open_child_directory(std::ffi::OsStr::new("refs"))?
        .ok_or_else(|| anyhow::anyhow!("signed refs directory is absent"))?;
    let cas = lillux::CasStore::from_pinned_root(cas_directory);
    Ok((cas_mutation_guard, cas, refs_directory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, ChainThreadEntry, ThreadHistoryRetention,
        ThreadSnapshotBuilder, ThreadStatus,
    };
    use crate::signer::TestSigner;
    use crate::Signer as _;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;

    fn test_trust_store() -> Arc<crate::refs::TrustStore> {
        let signer = TestSigner::default();
        let mut trust_store = crate::refs::TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        Arc::new(trust_store)
    }

    fn write_object(cas_root: &Path, hash: &str, value: &serde_json::Value) {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let canonical = lillux::canonical_json(value).unwrap();
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
    }

    fn object_hash(value: &serde_json::Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        lillux::sha256_hex(canonical.as_bytes())
    }

    fn initialize_standalone_runtime(runtime_state_dir: &Path) {
        fs::create_dir_all(runtime_state_dir.join("objects")).unwrap();
        fs::create_dir_all(runtime_state_dir.join("refs")).unwrap();
        crate::CasMutationGuard::ensure_anchor(runtime_state_dir).unwrap();
    }

    fn minimal_chain_payload(chain_root_id: &str) -> ExportPayload {
        let policy_hash = "a".repeat(64);
        let snapshot = ThreadSnapshotBuilder::new(
            chain_root_id,
            chain_root_id,
            "directive",
            "directive:test",
            "directive-runtime",
        )
        .captured_history_policy(Some(CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::Durable,
            canonical_item_ref: "directive:test".to_string(),
            item_content_hash: policy_hash.clone(),
            item_signer_fingerprint: Some(policy_hash.clone()),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: policy_hash,
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        }))
        .build();
        let snapshot_data = lillux::canonical_json(&snapshot.to_value())
            .unwrap()
            .into_bytes();
        let snapshot_hash = lillux::sha256_hex(&snapshot_data);
        let chain_state = crate::ChainState {
            schema: crate::objects::SCHEMA_VERSION,
            kind: "chain_state".to_string(),
            chain_root_id: chain_root_id.to_string(),
            prev_chain_state_hash: None,
            last_event_hash: None,
            last_chain_seq: 0,
            updated_at: snapshot.updated_at.clone(),
            threads: BTreeMap::from([(
                chain_root_id.to_string(),
                ChainThreadEntry {
                    snapshot_hash: snapshot_hash.clone(),
                    last_event_hash: None,
                    last_thread_seq: 0,
                    status: ThreadStatus::Created,
                },
            )]),
        };
        let chain_state_data = lillux::canonical_json(&chain_state.to_value())
            .unwrap()
            .into_bytes();
        let chain_head_hash = lillux::sha256_hex(&chain_state_data);
        let total_bytes = snapshot_data.len() + chain_state_data.len();
        ExportPayload {
            chain_root_id: chain_root_id.to_string(),
            chain_head_hash: chain_head_hash.clone(),
            entries: vec![
                SyncEntry {
                    hash: snapshot_hash,
                    is_blob: false,
                    data: snapshot_data,
                },
                SyncEntry {
                    hash: chain_head_hash,
                    is_blob: false,
                    data: chain_state_data,
                },
            ],
            total_bytes,
        }
    }

    fn write_signed_head(refs_root: &Path, chain_root_id: &str, target_hash: &str) {
        let signer = crate::signer::TestSigner::default();
        let ref_path = format!("chains/{}/head", chain_root_id);
        let signed_ref = crate::refs::SignedRef::new(
            ref_path,
            target_hash.to_string(),
            "2026-04-23T00:00:00Z".to_string(),
            signer.fingerprint().to_string(),
        );
        let head_path = refs_root
            .join("generic/chains")
            .join(chain_root_id)
            .join("head");
        crate::refs::write_signed_ref(&head_path, signed_ref, &signer).unwrap();
    }

    #[test]
    fn export_chain_collects_all_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        initialize_standalone_runtime(tmp.path());

        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": crate::objects::THREAD_SNAPSHOT_SCHEMA_VERSION,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "wait",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-23T00:00:00Z",
            "updated_at": "2026-04-23T00:00:00Z",
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
            "project_authority": { "kind": "projectless" },
            "captured_history_policy": {
                "retention": { "mode": "durable" },
                "canonical_item_ref": "directive:test/item",
                "item_content_hash": "11".repeat(32),
                "item_signer_fingerprint": "22".repeat(32),
                "item_trust_class": "trusted",
                "kind_schema_content_hash": "33".repeat(32),
                "resolved_from": {
                    "node_default": { "node_policy": "missing_config" }
                }
            }
        });
        let snap_hash = object_hash(&snap);
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-23T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });
        let cs_hash = object_hash(&cs);

        write_object(&cas_root, &cs_hash, &cs);
        write_object(&cas_root, &snap_hash, &snap);
        write_signed_head(&refs_root, "T-root", &cs_hash);

        let trust_store = test_trust_store();
        let payload = export_chain(tmp.path(), "T-root", trust_store.as_ref()).unwrap();
        assert_eq!(payload.chain_root_id, "T-root");
        assert_eq!(payload.chain_head_hash, cs_hash);
        assert_eq!(payload.entries.len(), 2);

        let hashes: Vec<&str> = payload.entries.iter().map(|e| e.hash.as_str()).collect();
        assert!(hashes.contains(&cs_hash.as_str()));
        assert!(hashes.contains(&snap_hash.as_str()));
        assert!(payload.entries.iter().all(|e| !e.is_blob));

        let _guard = crate::CasMutationGuard::shared_from_cas_root(&cas_root).unwrap();
        let cas_directory = lillux::PinnedDirectory::open(&cas_root).unwrap().unwrap();
        let refs_directory = lillux::PinnedDirectory::open(&refs_root).unwrap().unwrap();
        let cas = lillux::CasStore::from_pinned_root(cas_directory);
        let pinned =
            export_chain_pinned(&cas, &refs_directory, "T-root", trust_store.as_ref()).unwrap();
        assert_eq!(pinned.chain_head_hash, cs_hash);
        assert_eq!(pinned.entries.len(), 2);
        let local =
            collect_local_hashes_pinned(&cas, &refs_directory, "T-root", trust_store.as_ref())
                .unwrap();
        assert_eq!(local, HashSet::from([cs_hash, snap_hash]));
    }

    #[test]
    fn staged_chain_import_writes_and_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        initialize_standalone_runtime(tmp.path());

        let payload = minimal_chain_payload("T-test");
        let head_hash = payload.chain_head_hash.clone();

        let staged = stage_chain_import(tmp.path(), &payload).unwrap();
        assert_eq!(staged.result.imported, 2);
        assert_eq!(staged.result.already_present, 0);
        assert_eq!(staged.result.hash_mismatches, 0);
        drop(staged);

        // Verify file exists
        let path = lillux::shard_path(&cas_root, "objects", &head_hash, ".json");
        assert!(path.exists());

        // Re-import should be a no-op (already present)
        let staged2 = stage_chain_import(tmp.path(), &payload).unwrap();
        assert_eq!(staged2.result.imported, 0);
        assert_eq!(staged2.result.already_present, 2);
    }

    #[test]
    fn import_objects_staged_records_attribution() {
        let tmp = tempfile::tempdir().unwrap();
        let db = StateDb::open(tmp.path(), test_trust_store()).unwrap();

        let data = b"{\"kind\":\"test\",\"value\":42}";
        let hash = lillux::sha256_hex(data);

        let payload = ExportPayload {
            chain_root_id: "T-test".to_string(),
            chain_head_hash: hash.clone(),
            entries: vec![SyncEntry {
                hash: hash.clone(),
                is_blob: false,
                data: data.to_vec(),
            }],
            total_bytes: data.len(),
        };

        let attribution = ImportAttribution {
            source_principal: Some("fp:remote".to_string()),
            source_peer: Some("remote-a".to_string()),
            job_id: Some("job-a".to_string()),
        };

        let cas_guard = crate::CasMutationGuard::shared_from_cas_root(db.cas_root()).unwrap();
        let result = import_objects_staged(&db, &payload, &attribution, &cas_guard).unwrap();
        assert_eq!(result.imported, 1);

        let entry = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(entry.hash, hash);
        assert_eq!(entry.entry_kind, CasEntryKind::Object);
        assert_eq!(entry.bytes, data.len() as u64);
        assert_eq!(entry.state, CasEntryState::Staged);
        assert_eq!(entry.source_principal.as_deref(), Some("fp:remote"));
        assert_eq!(entry.source_peer.as_deref(), Some("remote-a"));
        assert_eq!(entry.job_id.as_deref(), Some("job-a"));

        let result2 = import_objects_staged(&db, &payload, &attribution, &cas_guard).unwrap();
        assert_eq!(result2.imported, 0);
        assert_eq!(result2.already_present, 1);
    }

    #[test]
    fn staged_chain_import_rejects_corrupt_hash() {
        let tmp = tempfile::tempdir().unwrap();
        initialize_standalone_runtime(tmp.path());

        let payload = ExportPayload {
            chain_root_id: "T-test".to_string(),
            chain_head_hash: "00".repeat(32),
            entries: vec![SyncEntry {
                hash: "00".repeat(32),
                is_blob: false,
                data: b"corrupt data".to_vec(),
            }],
            total_bytes: 12,
        };

        let error = stage_chain_import(tmp.path(), &payload)
            .err()
            .expect("corrupt staged head must fail closure verification");
        assert!(format!("{error:#}").contains("incomplete CAS closure"));
    }

    #[test]
    fn staged_chain_import_handles_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        initialize_standalone_runtime(tmp.path());

        let data = b"binary blob content here";
        let hash = lillux::sha256_hex(data);
        let mut payload = minimal_chain_payload("T-test");
        payload.entries.push(SyncEntry {
            hash: hash.clone(),
            is_blob: true,
            data: data.to_vec(),
        });
        payload.total_bytes += data.len();

        let staged = stage_chain_import(tmp.path(), &payload).unwrap();
        assert_eq!(staged.result.imported, 3);

        let path = lillux::shard_path(&cas_root, "blobs", &hash, "");
        assert!(path.exists());
        assert_eq!(fs::read(&path).unwrap(), data);
    }

    #[test]
    fn staged_chain_import_leases_omitted_local_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        initialize_standalone_runtime(tmp.path());

        let mut payload = minimal_chain_payload("T-test");
        let snapshot = payload.entries.remove(0);
        let snapshot_path = lillux::shard_path(&cas_root, "objects", &snapshot.hash, ".json");
        fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        lillux::atomic_write(&snapshot_path, &snapshot.data).unwrap();
        payload.total_bytes -= snapshot.data.len();

        let staged = stage_chain_import(tmp.path(), &payload).unwrap();
        let active = crate::RecoveryStore::from_runtime_state_dir(tmp.path())
            .unwrap()
            .active_staged_cas_root_hashes()
            .unwrap();
        assert!(active.object_hashes.contains(&payload.chain_head_hash));
        assert!(active.object_hashes.contains(&snapshot.hash));
        drop(staged);
    }

    #[test]
    fn reconcile_export_skips_known_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        initialize_standalone_runtime(tmp.path());

        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": crate::objects::THREAD_SNAPSHOT_SCHEMA_VERSION,
            "thread_id": "T-root",
            "chain_root_id": "T-root",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "wait",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-23T00:00:00Z",
            "updated_at": "2026-04-23T00:00:00Z",
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
            "project_authority": { "kind": "projectless" },
            "captured_history_policy": {
                "retention": { "mode": "durable" },
                "canonical_item_ref": "directive:test/item",
                "item_content_hash": "11".repeat(32),
                "item_signer_fingerprint": "22".repeat(32),
                "item_trust_class": "trusted",
                "kind_schema_content_hash": "33".repeat(32),
                "resolved_from": {
                    "node_default": { "node_policy": "missing_config" }
                }
            }
        });
        let snap_hash = object_hash(&snap);
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-23T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });
        let cs_hash = object_hash(&cs);

        write_object(&cas_root, &cs_hash, &cs);
        write_object(&cas_root, &snap_hash, &snap);
        write_signed_head(&refs_root, "T-root", &cs_hash);

        // Remote already has the chain_state
        let mut remote_has = HashSet::new();
        remote_has.insert(cs_hash.clone());

        let trust_store = test_trust_store();
        let payload =
            reconcile_export(tmp.path(), "T-root", &remote_has, trust_store.as_ref()).unwrap();
        assert_eq!(payload.entries.len(), 1);
        assert_eq!(payload.entries[0].hash, snap_hash);

        let _guard = crate::CasMutationGuard::shared_from_cas_root(&cas_root).unwrap();
        let cas_directory = lillux::PinnedDirectory::open(&cas_root).unwrap().unwrap();
        let refs_directory = lillux::PinnedDirectory::open(&refs_root).unwrap().unwrap();
        let cas = lillux::CasStore::from_pinned_root(cas_directory);
        let pinned = reconcile_export_pinned(
            &cas,
            &refs_directory,
            "T-root",
            &remote_has,
            trust_store.as_ref(),
        )
        .unwrap();
        assert_eq!(pinned.chain_head_hash, cs_hash);
        assert_eq!(pinned.entries.len(), 1);
        assert_eq!(pinned.entries[0].hash, snap_hash);
    }

    /// H6: finalize_import signs a local chain head ref and catches up projection.
    #[test]
    fn finalize_import_signs_local_ref() {
        use crate::StateDb;

        let tmp = tempfile::tempdir().unwrap();
        let runtime_state_dir = tmp.path();
        let db = StateDb::open(runtime_state_dir, test_trust_store()).unwrap();
        let signer = TestSigner::default();

        let cas_root = db.cas_root();
        let refs_root = db.refs_root();

        // Build an imported closure with canonical hashes.
        let snap = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": crate::objects::THREAD_SNAPSHOT_SCHEMA_VERSION,
            "thread_id": "T-imported",
            "chain_root_id": "T-imported",
            "status": "created",
            "kind_name": "directive",
            "item_ref": "directive:test/imported",
            "executor_ref": "test/executor",
            "launch_mode": "wait",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-23T00:00:00Z",
            "updated_at": "2026-04-23T00:00:00Z",
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
            "project_authority": { "kind": "projectless" },
            "captured_history_policy": {
                "retention": { "mode": "durable" },
                "canonical_item_ref": "directive:test/imported",
                "item_content_hash": "11".repeat(32),
                "item_signer_fingerprint": "22".repeat(32),
                "item_trust_class": "trusted",
                "kind_schema_content_hash": "33".repeat(32),
                "resolved_from": {
                    "node_default": { "node_policy": "missing_config" }
                }
            }
        });
        let snap_bytes = lillux::canonical_json(&snap).unwrap().into_bytes();
        let snap_hash = lillux::sha256_hex(&snap_bytes);
        let cs = serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-imported",
            "prev_chain_state_hash": null,
            "last_event_hash": null,
            "last_chain_seq": 0,
            "updated_at": "2026-04-23T00:00:00Z",
            "threads": {
                "T-imported": {
                    "snapshot_hash": snap_hash,
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "created"
                }
            }
        });
        let cs_bytes = lillux::canonical_json(&cs).unwrap().into_bytes();
        let cs_hash = lillux::sha256_hex(&cs_bytes);
        let payload = ExportPayload {
            chain_root_id: "T-imported".to_string(),
            chain_head_hash: cs_hash.clone(),
            total_bytes: snap_bytes.len() + cs_bytes.len(),
            entries: vec![
                SyncEntry {
                    hash: snap_hash,
                    is_blob: false,
                    data: snap_bytes,
                },
                SyncEntry {
                    hash: cs_hash.clone(),
                    is_blob: false,
                    data: cs_bytes,
                },
            ],
        };

        let staged = stage_chain_import(cas_root.parent().unwrap(), &payload).unwrap();
        let cas_guard = crate::CasMutationGuard::shared_from_cas_root(db.cas_root()).unwrap();
        finalize_import(&db, staged, &signer, &cas_guard).unwrap();

        // Assert: signed chain head ref exists
        let head_ref_path = refs_root.join("generic/chains/T-imported/head");
        assert!(head_ref_path.exists(), "signed chain head ref should exist");

        // Assert: ref target matches the imported chain head hash
        let signed_ref = crate::refs::read_signed_ref_envelope_structural(&head_ref_path).unwrap();
        assert_eq!(signed_ref.target_hash, cs_hash);

        // Assert: projection caught up — thread is queryable
        let thread = crate::queries::get_thread(db.projection(), "T-imported").unwrap();
        assert!(
            thread.is_some(),
            "imported thread should appear in projection"
        );
        let thread = thread.unwrap();
        assert_eq!(thread.status, "created");
        assert_eq!(thread.item_ref, "directive:test/imported");
    }
}
